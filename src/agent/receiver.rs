// The Receiver Agent — owns the receive-side io_uring poller and all
// ReceiveChannelEndpoints. Mirrors aeron_driver_receiver_t.

use super::{Agent, AgentError};
use crate::clock::CachedNanoClock;
use crate::frame::*;

use crate::context::DriverContext;
use crate::media::poller::{RecvMessage, TransportPoller};
use crate::media::receive_channel_endpoint::{
    DataFrameHandler, PendingSm, ReceiveChannelEndpoint,
};
use crate::media::uring_poller::UringTransportPoller;

// ── Pre-sized capacity for images ──
const MAX_IMAGES: usize = 256;
/// Bitmask for image hash index (power-of-two, no modulo in hot path).
const IMAGE_INDEX_MASK: usize = MAX_IMAGES - 1;
/// Sentinel value meaning "empty slot" in the image hash index.
const IMAGE_INDEX_EMPTY: u16 = u16::MAX;
/// Maximum expected endpoints — pre-sized to avoid reallocation.
const MAX_RECV_ENDPOINTS: usize = 16;

// ── O(1) image lookup helpers (no allocation, no modulo) ──

/// Hash (session_id, stream_id) into a table index using multiply-xor.
/// Uses bitmask (not modulo) as required by coding rules.
#[inline]
fn image_hash(session_id: i32, stream_id: i32) -> usize {
    let h = (session_id as u32).wrapping_mul(0x9E37_79B9)
        ^ (stream_id as u32).wrapping_mul(0x517C_C1B7);
    (h as usize) & IMAGE_INDEX_MASK
}

/// Find the image index for (session_id, stream_id) via linear probing.
/// Returns the index into `images[]`, or `None` if not found.
#[inline]
fn find_image_in_index(
    images: &[ImageEntry; MAX_IMAGES],
    index: &[u16; MAX_IMAGES],
    session_id: i32,
    stream_id: i32,
) -> Option<usize> {
    let start = image_hash(session_id, stream_id);
    for probe in 0..MAX_IMAGES {
        let slot = (start.wrapping_add(probe)) & IMAGE_INDEX_MASK;
        let idx = index[slot];
        if idx == IMAGE_INDEX_EMPTY {
            return None;
        }
        let img = &images[idx as usize];
        if img.active && img.session_id == session_id && img.stream_id == stream_id {
            return Some(idx as usize);
        }
    }
    None
}

/// Insert an image into the hash index. Caller must ensure table is not full.
#[inline]
fn insert_image_index(
    index: &mut [u16; MAX_IMAGES],
    images_idx: usize,
    session_id: i32,
    stream_id: i32,
) {
    let start = image_hash(session_id, stream_id);
    for probe in 0..MAX_IMAGES {
        let slot = (start.wrapping_add(probe)) & IMAGE_INDEX_MASK;
        if index[slot] == IMAGE_INDEX_EMPTY {
            index[slot] = images_idx as u16;
            return;
        }
    }
}

/// Tracks one publication image on the receiver side.
struct ImageEntry {
    session_id: i32,
    stream_id: i32,
    endpoint_idx: usize,
    /// Source address of the sender.
    source_addr: libc::sockaddr_storage,
    /// Consumption tracking.
    consumption_term_id: i32,
    consumption_term_offset: i32,
    receiver_window: i32,
    #[allow(dead_code)]
    receiver_id: i64,
    /// Loss detection.
    expected_term_offset: i32,
    /// Timing.
    time_of_last_sm_ns: i64,
    #[allow(dead_code)]
    time_of_last_nak_ns: i64,
    /// Whether this slot is active.
    active: bool,
}

impl Default for ImageEntry {
    fn default() -> Self {
        Self {
            session_id: 0,
            stream_id: 0,
            endpoint_idx: 0,
            source_addr: unsafe { std::mem::zeroed() },
            consumption_term_id: 0,
            consumption_term_offset: 0,
            receiver_window: 0,
            receiver_id: 0,
            expected_term_offset: 0,
            time_of_last_sm_ns: 0,
            time_of_last_nak_ns: 0,
            active: false,
        }
    }
}

pub struct ReceiverAgent {
    poller: UringTransportPoller,
    endpoints: Vec<ReceiveChannelEndpoint>,
    images: [ImageEntry; MAX_IMAGES],
    image_count: usize,
    /// Hash index: maps hash(session_id, stream_id) → index in `images[]`.
    /// `IMAGE_INDEX_EMPTY` means empty slot. Uses linear probing, bitmask (no modulo).
    image_index: [u16; MAX_IMAGES],
    clock: CachedNanoClock,
    sm_interval_ns: i64,
    #[allow(dead_code)]
    nak_delay_ns: i64,
    #[allow(dead_code)]
    default_receiver_window: i32,
    receiver_id: i64,
}

/// Temporary handler used during poll dispatch.
struct InlineHandler<'a> {
    images: &'a mut [ImageEntry; MAX_IMAGES],
    image_count: &'a mut usize,
    image_index: &'a mut [u16; MAX_IMAGES],
    #[allow(dead_code)]
    now_ns: i64,
}

impl<'a> DataFrameHandler for InlineHandler<'a> {
    fn on_data(
        &mut self,
        data_header: &DataHeader,
        _payload: &[u8],
        _source: &libc::sockaddr_storage,
    ) {
        let session_id = data_header.session_id;
        let stream_id = data_header.stream_id;

        // O(1) image lookup via hash index (replaces linear scan).
        let image_idx = find_image_in_index(
            self.images, self.image_index, session_id, stream_id,
        );

        if let Some(idx) = image_idx {
            let img = &mut self.images[idx];
            let term_offset = data_header.term_offset;
            let frame_len = data_header.frame_header.frame_length;

            // Gap detection using wrapping arithmetic (no bare > comparison).
            let gap = term_offset.wrapping_sub(img.expected_term_offset);
            if gap > 0 && gap < (i32::MAX / 2) {
                tracing::trace!(
                    session_id,
                    stream_id,
                    expected = img.expected_term_offset,
                    got = term_offset,
                    "gap detected"
                );
            }

            // Advance consumption position using wrapping comparison.
            let new_offset = term_offset.wrapping_add(frame_len);
            let advance = new_offset.wrapping_sub(img.consumption_term_offset);
            if advance > 0 && advance < (i32::MAX / 2) {
                img.consumption_term_offset = new_offset;
            }
            img.expected_term_offset = new_offset;

            // TODO: write data into image term buffer (phase 3).
        }
    }

    fn on_setup(
        &mut self,
        setup: &SetupHeader,
        source: &libc::sockaddr_storage,
    ) {
        let session_id = setup.session_id;
        let stream_id = setup.stream_id;

        // O(1) existence check via hash index.
        let exists = find_image_in_index(
            self.images, self.image_index, session_id, stream_id,
        ).is_some();

        if !exists && *self.image_count < MAX_IMAGES {
            tracing::info!(session_id, stream_id, "new image from setup");
            let idx = *self.image_count;
            self.images[idx] = ImageEntry {
                session_id,
                stream_id,
                endpoint_idx: 0, // resolved in full impl
                source_addr: *source,
                consumption_term_id: setup.active_term_id,
                consumption_term_offset: setup.term_offset,
                receiver_window: setup.term_length / 2,
                receiver_id: 0,
                expected_term_offset: setup.term_offset,
                time_of_last_sm_ns: 0,
                time_of_last_nak_ns: 0,
                active: true,
            };
            // Insert into hash index for O(1) future lookups.
            insert_image_index(self.image_index, idx, session_id, stream_id);
            *self.image_count = idx + 1;
        }
    }
}

impl ReceiverAgent {
    pub fn new(ctx: &DriverContext) -> Result<Self, AgentError> {
        let poller = UringTransportPoller::new(ctx)?;
        Ok(Self {
            poller,
            endpoints: Vec::with_capacity(MAX_RECV_ENDPOINTS),
            images: std::array::from_fn(|_| ImageEntry::default()),
            image_count: 0,
            image_index: [IMAGE_INDEX_EMPTY; MAX_IMAGES],
            clock: CachedNanoClock::new(),
            sm_interval_ns: ctx.sm_interval_ns,
            nak_delay_ns: ctx.nak_delay_ns,
            default_receiver_window: (ctx.mtu_length * 32) as i32,
            receiver_id: rand_receiver_id(),
        })
    }

    pub fn add_endpoint(
        &mut self,
        mut endpoint: ReceiveChannelEndpoint,
    ) -> std::io::Result<usize> {
        endpoint.register(&mut self.poller)?;
        let idx = self.endpoints.len();
        self.endpoints.push(endpoint);
        Ok(idx)
    }

    fn poll_data(&mut self, now_ns: i64) -> i32 {
        // Split borrows: poller, endpoints, images are all separate fields.
        let poller = &mut self.poller;
        let endpoints = &self.endpoints;
        let images = &mut self.images;
        let image_count = &mut self.image_count;
        let image_index = &mut self.image_index;

        let result = poller.poll_recv(|msg: RecvMessage<'_>| {
            let mut handler = InlineHandler {
                images,
                image_count,
                image_index,
                now_ns,
            };
            // O(1) dispatch: transport_idx maps 1:1 to endpoint index.
            if let Some(ep) = endpoints.get(msg.transport_idx) {
                ep.on_message(msg.data, msg.source_addr, &mut handler);
            }
        });

        match result {
            Ok(r) => r.messages_received as i32,
            Err(e) => {
                tracing::warn!("receiver poll error: {e}");
                0
            }
        }
    }

    fn send_control_messages(&mut self, now_ns: i64) -> i32 {
        let mut work_count = 0i32;

        // Collect SM requests from images into the endpoint pending queues.
        for i in 0..self.image_count {
            let img = &mut self.images[i];
            if !img.active {
                continue;
            }

            // Send SM if interval has elapsed.
            if now_ns.wrapping_sub(img.time_of_last_sm_ns) > self.sm_interval_ns {
                let ep_idx = img.endpoint_idx;
                let sm = PendingSm {
                    dest_addr: img.source_addr,
                    session_id: img.session_id,
                    stream_id: img.stream_id,
                    consumption_term_id: img.consumption_term_id,
                    consumption_term_offset: img.consumption_term_offset,
                    receiver_window: img.receiver_window,
                    receiver_id: self.receiver_id,
                };
                img.time_of_last_sm_ns = now_ns;

                if let Some(ep) = self.endpoints.get_mut(ep_idx) {
                    ep.queue_sm(sm);
                    work_count += 1;
                }
            }
        }

        // Flush all pending control sends.
        for ep in &mut self.endpoints {
            match ep.send_pending(&mut self.poller) {
                Ok(n) => work_count += n as i32,
                Err(e) => tracing::warn!("send pending error: {e}"),
            }
        }

        let _ = self.poller.flush();
        work_count
    }
}

impl Agent for ReceiverAgent {
    fn name(&self) -> &str {
        "aeron-receiver"
    }

    fn do_work(&mut self) -> Result<i32, AgentError> {
        let now_ns = self.clock.update();
        let mut work_count = 0i32;

        // Step 1: Poll incoming data from io_uring.
        work_count += self.poll_data(now_ns);

        // Step 2: Send SM / NAK / RTTM back to senders.
        work_count += self.send_control_messages(now_ns);

        Ok(work_count)
    }
}

fn rand_receiver_id() -> i64 {
    let mut buf = [0u8; 8];
    // SAFETY: getrandom is always available on Linux 3.17+.
    if unsafe { libc::getrandom(buf.as_mut_ptr() as *mut _, 8, 0) } == 8 {
        i64::from_ne_bytes(buf)
    } else {
        std::process::id() as i64
    }
}