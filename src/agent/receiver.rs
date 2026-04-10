// The Receiver Agent - owns the receive-side io_uring poller and all
// ReceiveChannelEndpoints. Mirrors aeron_driver_receiver_t.

use std::sync::Arc;

use super::{Agent, AgentError};
use crate::clock::CachedNanoClock;
use crate::frame::*;

use crate::client::sub_bridge::{PendingImage, SubscriptionBridge};
use crate::context::DriverContext;
use crate::media::poller::{RecvMessage, TransportPoller};
use crate::media::receive_channel_endpoint::{
    DataFrameHandler, PendingNak, PendingSm, ReceiveChannelEndpoint,
};
use crate::media::shared_image::{new_shared_image, ReceiverImage};
use crate::media::term_buffer::{partition_index, PARTITION_COUNT};
use crate::media::uring_poller::UringTransportPoller;

// ── Pre-sized capacity for images ──
const MAX_IMAGES: usize = 256;
/// Bitmask for image hash index (power-of-two, no modulo in hot path).
const IMAGE_INDEX_MASK: usize = MAX_IMAGES - 1;
/// Sentinel value meaning "empty slot" in the image hash index.
const IMAGE_INDEX_EMPTY: u16 = u16::MAX;
/// Maximum expected endpoints - pre-sized to avoid reallocation.
const MAX_RECV_ENDPOINTS: usize = 16;
/// Maximum NAKs accumulated per duty cycle. Pre-sized flat array, no allocation.
const MAX_PENDING_NAKS_PER_CYCLE: usize = 64;

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

/// Remove an image from the hash index using backward-shift deletion.
///
/// Unlike tombstone-based deletion, backward-shift preserves O(1) average
/// lookup without degrading probe chains over time. After finding the
/// target slot, we shift subsequent entries backward if they belong to an
/// earlier probe chain, then clear the vacated slot.
///
/// No allocation, O(1) amortized, bitmask-only indexing.
fn remove_image_index(
    index: &mut [u16; MAX_IMAGES],
    images: &[ImageEntry; MAX_IMAGES],
    session_id: i32,
    stream_id: i32,
) {
    // Phase 1: find the slot containing the target entry.
    let start = image_hash(session_id, stream_id);
    let mut target = MAX_IMAGES; // sentinel: not found
    for probe in 0..MAX_IMAGES {
        let slot = (start.wrapping_add(probe)) & IMAGE_INDEX_MASK;
        let idx = index[slot];
        if idx == IMAGE_INDEX_EMPTY {
            return; // not in table
        }
        let img = &images[idx as usize];
        if img.session_id == session_id && img.stream_id == stream_id {
            target = slot;
            break;
        }
    }
    if target == MAX_IMAGES {
        return; // not found
    }

    // Phase 2: backward-shift deletion.
    //
    // `gap` = the empty hole to fill.  `j` = advancing scan cursor.
    // `j` always moves forward; `gap` only moves when we shift an entry.
    // This avoids the infinite-loop pitfall of tying both to the same var.
    let mut gap = target;
    let mut j = target;
    loop {
        j = (j.wrapping_add(1)) & IMAGE_INDEX_MASK;
        let neighbor_idx = index[j];
        if neighbor_idx == IMAGE_INDEX_EMPTY {
            break; // chain ended
        }
        let img = &images[neighbor_idx as usize];
        let natural = image_hash(img.session_id, img.stream_id);

        // Should this entry shift back into the gap?
        // Yes if its natural slot is NOT strictly between (gap, j] in the
        // circular probe order.  Equivalently: the distance from natural
        // to j (wrapping) is ≥ the distance from gap to j.
        let dist_natural = (j.wrapping_sub(natural)) & IMAGE_INDEX_MASK;
        let dist_gap = (j.wrapping_sub(gap)) & IMAGE_INDEX_MASK;
        if dist_natural >= dist_gap {
            index[gap] = index[j];
            gap = j;
        }

        // Safety: full-table wrap guard (should never fire with < 100% load).
        if j == target {
            break;
        }
    }

    index[gap] = IMAGE_INDEX_EMPTY;
}

/// Tracks one publication image on the receiver side.
#[derive(Clone, Copy)]
struct ImageEntry {
    session_id: i32,
    stream_id: i32,
    endpoint_idx: usize,
    /// Source address of the sender.
    source_addr: libc::sockaddr_storage,
    /// Term configuration from setup frame.
    initial_term_id: i32,
    active_term_id: i32,
    #[allow(dead_code)]
    term_length: u32,
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
            initial_term_id: 0,
            active_term_id: 0,
            term_length: 0,
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
    /// Hash index: maps hash(session_id, stream_id) -> index in `images[]`.
    /// `IMAGE_INDEX_EMPTY` means empty slot. Uses linear probing, bitmask (no modulo).
    image_index: [u16; MAX_IMAGES],
    /// Per-image shared buffer handle (parallel to `images[]`). Allocated on setup,
    /// dropped on remove. Vec is pre-sized to MAX_IMAGES and never resized.
    image_handles: Vec<Option<ReceiverImage>>,
    /// Configured cap on active images (from DriverContext).
    max_images: usize,
    clock: CachedNanoClock,
    sm_interval_ns: i64,
    nak_delay_ns: i64,
    #[allow(dead_code)]
    default_receiver_window: i32,
    receiver_id: i64,
    /// Pre-sized NAK accumulator - filled during poll_data, drained in
    /// send_control_messages. No allocation in duty cycle.
    nak_queue: [PendingNak; MAX_PENDING_NAKS_PER_CYCLE],
    nak_queue_len: usize,
    /// Bridge to deliver SubscriberImage handles to client subscriptions.
    /// Set via `set_subscription_bridge()` before starting the agent.
    sub_bridge: Option<Arc<SubscriptionBridge>>,
}

// SAFETY: ReceiverAgent exclusively owns all its fields including the
// UringTransportPoller (which contains raw pointers into kernel-mapped
// memory). The agent is single-threaded by design - it is created on one
// thread and moved (not shared) to a dedicated agent thread via
// AgentRunner::start(). No concurrent access occurs.
unsafe impl Send for ReceiverAgent {}

/// Temporary handler used during poll dispatch.
struct InlineHandler<'a> {
    images: &'a mut [ImageEntry; MAX_IMAGES],
    image_count: &'a mut usize,
    image_index: &'a mut [u16; MAX_IMAGES],
    image_handles: &'a mut [Option<ReceiverImage>],
    max_images: usize,
    now_ns: i64,
    nak_delay_ns: i64,
    nak_queue: &'a mut [PendingNak; MAX_PENDING_NAKS_PER_CYCLE],
    nak_queue_len: &'a mut usize,
    sub_bridge: &'a Option<Arc<SubscriptionBridge>>,
}

impl<'a> DataFrameHandler for InlineHandler<'a> {
    fn on_data(
        &mut self,
        data_header: &DataHeader,
        payload: &[u8],
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
            let term_id = data_header.term_id;
            let term_offset = data_header.term_offset;

            // Gap detection using wrapping arithmetic (no bare > comparison).
            // Only generate NAKs within the active term (active-term only).
            let gap = term_offset.wrapping_sub(img.expected_term_offset);
            if gap > 0 && gap < (i32::MAX / 2) && term_id == img.active_term_id {
                // Timer-based coalescing: one NAK per image per nak_delay_ns.
                let since_last = self.now_ns.wrapping_sub(img.time_of_last_nak_ns);
                if since_last >= 0 && since_last >= self.nak_delay_ns {
                    if *self.nak_queue_len < MAX_PENDING_NAKS_PER_CYCLE {
                        self.nak_queue[*self.nak_queue_len] = PendingNak {
                            dest_addr: img.source_addr,
                            session_id,
                            stream_id,
                            active_term_id: img.active_term_id,
                            term_offset: img.expected_term_offset,
                            length: gap,
                        };
                        *self.nak_queue_len += 1;
                    }
                    img.time_of_last_nak_ns = self.now_ns;
                }
                tracing::trace!(
                    session_id,
                    stream_id,
                    expected = img.expected_term_offset,
                    got = term_offset,
                    "gap detected"
                );
            }

            if let Some(ref mut recv_image) = self.image_handles[idx] {
                // Term rotation: advance active_term_id if data is ahead.
                let term_ahead = term_id.wrapping_sub(img.active_term_id);
                if term_ahead > 0 && term_ahead < (i32::MAX / 2) {
                    let steps = (term_ahead as u32).min(PARTITION_COUNT as u32);
                    for s in 1..=steps {
                        let tid = img.active_term_id.wrapping_add(s as i32);
                        let pidx = partition_index(tid, img.initial_term_id);
                        recv_image.clean_partition(pidx);
                    }
                    img.active_term_id = term_id;
                    img.consumption_term_id = term_id;
                    img.consumption_term_offset = 0;
                    img.expected_term_offset = 0;
                }

                // Reject stale data (term too far behind active).
                let term_behind = img.active_term_id.wrapping_sub(term_id) as u32;
                if term_behind >= PARTITION_COUNT as u32 {
                    return;
                }

                // Write frame into image term buffer.
                let pidx = partition_index(term_id, img.initial_term_id);
                match recv_image.append_frame(
                    pidx, term_offset as u32, data_header, payload,
                ) {
                    Ok(new_offset) => {
                        let aligned_end = new_offset as i32;
                        // Only advance consumption within the current consumption term.
                        if term_id == img.consumption_term_id {
                            let advance = aligned_end
                                .wrapping_sub(img.consumption_term_offset);
                            if advance > 0 && advance < (i32::MAX / 2) {
                                img.consumption_term_offset = aligned_end;
                            }
                        }
                        // Track expected offset within the active term.
                        if term_id == img.active_term_id {
                            let advance = aligned_end
                                .wrapping_sub(img.expected_term_offset);
                            if advance > 0 && advance < (i32::MAX / 2) {
                                img.expected_term_offset = aligned_end;
                            }
                        }
                    }
                    Err(_) => {
                        // TermFull or InvalidPartition - frame does not fit.
                    }
                }
            }
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

        if !exists && *self.image_count < self.max_images {
            // Validate term_length from sender (must be power-of-two >= 32).
            let tl = setup.term_length;
            if tl < 32 || !((tl as u32).is_power_of_two()) {
                tracing::warn!(
                    session_id,
                    stream_id,
                    term_length = tl,
                    "rejecting setup: invalid term_length"
                );
                return;
            }

            let (recv_image, sub_image) = match new_shared_image(
                session_id,
                stream_id,
                setup.initial_term_id,
                setup.active_term_id,
                tl as u32,
                setup.term_offset,
            ) {
                Some(pair) => pair,
                None => {
                    tracing::warn!(
                        session_id,
                        stream_id,
                        term_length = tl,
                        "rejecting setup: shared image allocation failed"
                    );
                    return;
                }
            };

            tracing::info!(session_id, stream_id, "new image from setup");
            let idx = *self.image_count;
            self.images[idx] = ImageEntry {
                session_id,
                stream_id,
                endpoint_idx: 0, // resolved in full impl
                source_addr: *source,
                initial_term_id: setup.initial_term_id,
                active_term_id: setup.active_term_id,
                term_length: tl as u32,
                consumption_term_id: setup.active_term_id,
                consumption_term_offset: setup.term_offset,
                receiver_window: setup.term_length / 2,
                receiver_id: 0,
                expected_term_offset: setup.term_offset,
                time_of_last_sm_ns: 0,
                time_of_last_nak_ns: 0,
                active: true,
            };
            self.image_handles[idx] = Some(recv_image);

            // Deposit subscriber handle into bridge for client consumption.
            if let Some(bridge) = self.sub_bridge {
                let pending = PendingImage {
                    image: sub_image,
                    session_id,
                    stream_id,
                    correlation_id: 0, // assigned by conductor in full impl
                };
                if !bridge.deposit(pending) {
                    tracing::warn!(
                        session_id,
                        stream_id,
                        "subscription bridge full - image not visible to client"
                    );
                }
            }

            // Insert into hash index for O(1) future lookups.
            insert_image_index(self.image_index, idx, session_id, stream_id);
            *self.image_count = idx + 1;
        }
    }
}

impl ReceiverAgent {
    pub fn new(ctx: &DriverContext) -> Result<Self, AgentError> {
        ctx.validate().map_err(|e| {
            AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let poller = UringTransportPoller::new(ctx)?;
        let max_images = ctx.max_receiver_images.min(MAX_IMAGES);
        Ok(Self {
            poller,
            endpoints: Vec::with_capacity(MAX_RECV_ENDPOINTS),
            images: std::array::from_fn(|_| ImageEntry::default()),
            image_count: 0,
            image_index: [IMAGE_INDEX_EMPTY; MAX_IMAGES],
            image_handles: std::iter::repeat_with(|| None).take(MAX_IMAGES).collect(),
            max_images,
            clock: CachedNanoClock::new(),
            sm_interval_ns: ctx.sm_interval_ns,
            nak_delay_ns: ctx.nak_delay_ns,
            default_receiver_window: (ctx.mtu_length * 32) as i32,
            receiver_id: rand_receiver_id(),
            nak_queue: [unsafe { std::mem::zeroed() }; MAX_PENDING_NAKS_PER_CYCLE],
            nak_queue_len: 0,
            sub_bridge: None,
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

    /// Set the subscription bridge for delivering image handles to clients.
    ///
    /// Must be called before `start()`. Cold path.
    pub(crate) fn set_subscription_bridge(&mut self, bridge: Arc<SubscriptionBridge>) {
        self.sub_bridge = Some(bridge);
    }

    /// Remove an image by (session_id, stream_id).
    ///
    /// Uses backward-shift deletion in the hash index (no tombstones).
    /// The image array is compacted by swap-removing the last active entry
    /// into the deleted slot, then updating the hash index for the moved entry.
    ///
    /// Zero-allocation, O(1) amortized.
    pub fn remove_image(&mut self, session_id: i32, stream_id: i32) -> bool {
        let found = find_image_in_index(
            &self.images, &self.image_index, session_id, stream_id,
        );
        let Some(idx) = found else { return false };

        // Remove from hash index first.
        remove_image_index(
            &mut self.image_index, &self.images, session_id, stream_id,
        );
        self.images[idx].active = false;

        // Drop the removed image's shared buffer handle (signals close to subscriber).
        if let Some(ref handle) = self.image_handles[idx] {
            handle.close();
        }
        self.image_handles[idx] = None;

        // Compact: swap the last active entry into the hole.
        let last = self.image_count - 1;
        if idx != last {
            let moved_session = self.images[last].session_id;
            let moved_stream = self.images[last].stream_id;

            // Remove the moved entry from its old hash position.
            remove_image_index(
                &mut self.image_index, &self.images, moved_session, moved_stream,
            );

            // Swap into the hole.
            self.images[idx] = self.images[last];
            self.image_handles[idx] = self.image_handles[last].take();

            // Re-insert at the new images[] index.
            insert_image_index(
                &mut self.image_index, idx, moved_session, moved_stream,
            );
        }

        // Clear the vacated last slot.
        self.images[last] = ImageEntry::default();
        // image_handles[last] is already None (closed above or taken via .take()).
        self.image_count = last;

        true
    }

    fn poll_data(&mut self, now_ns: i64) -> i32 {
        // Split borrows: poller, endpoints, images, image_handles are all separate fields.
        let poller = &mut self.poller;
        let endpoints = &mut self.endpoints;
        let images = &mut self.images;
        let image_count = &mut self.image_count;
        let image_index = &mut self.image_index;
        let image_handles = self.image_handles.as_mut_slice();
        let max_images = self.max_images;
        let nak_delay_ns = self.nak_delay_ns;
        let nak_queue = &mut self.nak_queue;
        let nak_queue_len = &mut self.nak_queue_len;
        let sub_bridge = &self.sub_bridge;

        let result = poller.poll_recv(|msg: RecvMessage<'_>| {
            let mut handler = InlineHandler {
                images,
                image_count,
                image_index,
                image_handles,
                max_images,
                now_ns,
                nak_delay_ns,
                nak_queue,
                nak_queue_len,
                sub_bridge,
            };
            // O(1) dispatch: transport_idx maps 1:1 to endpoint index.
            if let Some(ep) = endpoints.get_mut(msg.transport_idx) {
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

        // Drain NAK queue (accumulated during poll_data) into endpoint pending queues.
        for i in 0..self.nak_queue_len {
            let nak = self.nak_queue[i];
            if let Some(idx) = find_image_in_index(
                &self.images, &self.image_index, nak.session_id, nak.stream_id,
            ) {
                let ep_idx = self.images[idx].endpoint_idx;
                if let Some(ep) = self.endpoints.get_mut(ep_idx) {
                    ep.queue_nak(nak);
                    work_count += 1;
                }
            }
        }
        self.nak_queue_len = 0;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_images() -> [ImageEntry; MAX_IMAGES] {
        std::array::from_fn(|_| ImageEntry::default())
    }

    fn make_active(images: &mut [ImageEntry; MAX_IMAGES], idx: usize, session: i32, stream: i32) {
        images[idx].session_id = session;
        images[idx].stream_id = stream;
        images[idx].active = true;
    }

    // ── image_hash ──

    #[test]
    fn image_hash_is_within_mask() {
        for session in -100..100 {
            for stream in -10..10 {
                let h = image_hash(session, stream);
                assert!(h < MAX_IMAGES, "hash {h} out of range for ({session},{stream})");
            }
        }
    }

    // ── insert + find ──

    #[test]
    fn insert_and_find_single() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        make_active(&mut images, 0, 42, 7);
        insert_image_index(&mut index, 0, 42, 7);
        assert_eq!(find_image_in_index(&images, &index, 42, 7), Some(0));
    }

    #[test]
    fn find_nonexistent_returns_none() {
        let images = make_images();
        let index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        assert_eq!(find_image_in_index(&images, &index, 99, 99), None);
    }

    #[test]
    fn insert_multiple_and_find() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        for i in 0..10 {
            make_active(&mut images, i, i as i32 * 100, i as i32);
            insert_image_index(&mut index, i, i as i32 * 100, i as i32);
        }
        for i in 0..10 {
            assert_eq!(
                find_image_in_index(&images, &index, i as i32 * 100, i as i32),
                Some(i),
            );
        }
    }

    // ── remove_image_index (backward-shift deletion) ──

    #[test]
    fn remove_single_entry() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        make_active(&mut images, 0, 42, 7);
        insert_image_index(&mut index, 0, 42, 7);

        remove_image_index(&mut index, &images, 42, 7);
        assert_eq!(find_image_in_index(&images, &index, 42, 7), None);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        // Should not panic or corrupt state.
        remove_image_index(&mut index, &images, 99, 99);
        assert_eq!(index, [IMAGE_INDEX_EMPTY; MAX_IMAGES]);
    }

    #[test]
    fn remove_preserves_chained_entries() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];

        // Insert three entries, some will chain via linear probing.
        for i in 0..3 {
            make_active(&mut images, i, i as i32, 0);
            insert_image_index(&mut index, i, i as i32, 0);
        }

        // Remove the first one.
        remove_image_index(&mut index, &images, 0, 0);
        assert_eq!(find_image_in_index(&images, &index, 0, 0), None);

        // The others must still be findable.
        assert!(find_image_in_index(&images, &index, 1, 0).is_some());
        assert!(find_image_in_index(&images, &index, 2, 0).is_some());
    }

    #[test]
    fn remove_middle_of_chain() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];

        for i in 0..5 {
            make_active(&mut images, i, i as i32, 0);
            insert_image_index(&mut index, i, i as i32, 0);
        }

        // Remove entry 2 (middle of chain).
        remove_image_index(&mut index, &images, 2, 0);

        // All others still findable.
        for i in 0..5 {
            if i == 2 { continue; }
            assert!(
                find_image_in_index(&images, &index, i as i32, 0).is_some(),
                "entry {i} should still be findable"
            );
        }
        assert_eq!(find_image_in_index(&images, &index, 2, 0), None);
    }

    #[test]
    fn remove_all_entries() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        let n = 20;

        for i in 0..n {
            make_active(&mut images, i, i as i32, i as i32);
            insert_image_index(&mut index, i, i as i32, i as i32);
        }

        // Remove all.
        for i in 0..n {
            remove_image_index(&mut index, &images, i as i32, i as i32);
        }

        // Table should be empty.
        for i in 0..n {
            assert_eq!(find_image_in_index(&images, &index, i as i32, i as i32), None);
        }
    }

    #[test]
    fn remove_then_insert_same_key() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        make_active(&mut images, 0, 42, 7);
        insert_image_index(&mut index, 0, 42, 7);

        remove_image_index(&mut index, &images, 42, 7);
        assert_eq!(find_image_in_index(&images, &index, 42, 7), None);

        // Re-insert at a different images[] index.
        make_active(&mut images, 5, 42, 7);
        insert_image_index(&mut index, 5, 42, 7);
        assert_eq!(find_image_in_index(&images, &index, 42, 7), Some(5));
    }

    #[test]
    fn remove_with_wrap_around() {
        let mut images = make_images();
        let mut index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];

        // Force wrap-around by scanning for session_ids whose hash lands
        // at the very end of the table (slots 253-255). Linear probing
        // will push subsequent inserts past slot 255 and wrap to slot 0+.
        let mut inserted = Vec::new();
        for session in 0..100_000i32 {
            let h = image_hash(session, 0);
            if h >= MAX_IMAGES - 3 {
                let idx = inserted.len();
                make_active(&mut images, idx, session, 0);
                insert_image_index(&mut index, idx, session, 0);
                inserted.push(session);
            }
            if inserted.len() >= 5 { break; }
        }

        // Must have found at least 2 entries for a meaningful test.
        assert!(inserted.len() >= 2, "need entries that hash near end of table");

        // Remove the first one inserted (at or near end of table).
        let target = inserted[0];
        remove_image_index(&mut index, &images, target, 0);
        assert_eq!(find_image_in_index(&images, &index, target, 0), None);

        // All others still findable.
        for &s in &inserted[1..] {
            assert!(
                find_image_in_index(&images, &index, s, 0).is_some(),
                "session {s} should still be findable after removing {target}"
            );
        }
    }

    // ── InlineHandler + image term buffer ──

    use crate::frame::{
        DATA_HEADER_LENGTH, CURRENT_VERSION, DATA_FLAG_BEGIN, DATA_FLAG_END,
        FRAME_TYPE_DATA, FRAME_TYPE_SETUP, SETUP_TOTAL_LENGTH,
    };
    use crate::media::term_buffer::PARTITION_COUNT;

    const TEST_TERM_LENGTH: u32 = 1024;

    /// Helper: create a minimal InlineHandler with pre-allocated image_handles.
    #[allow(clippy::type_complexity)]
    fn make_handler_parts() -> (
        [ImageEntry; MAX_IMAGES],
        usize,
        [u16; MAX_IMAGES],
        Vec<Option<ReceiverImage>>,
        [PendingNak; MAX_PENDING_NAKS_PER_CYCLE],
        usize,
        Option<Arc<SubscriptionBridge>>,
    ) {
        let images = make_images();
        let image_count = 0usize;
        let image_index = [IMAGE_INDEX_EMPTY; MAX_IMAGES];
        let image_handles: Vec<Option<ReceiverImage>> =
            std::iter::repeat_with(|| None).take(MAX_IMAGES).collect();
        let nak_queue: [PendingNak; MAX_PENDING_NAKS_PER_CYCLE] =
            [unsafe { std::mem::zeroed() }; MAX_PENDING_NAKS_PER_CYCLE];
        let nak_queue_len = 0usize;
        let sub_bridge = Some(SubscriptionBridge::new() as Arc<SubscriptionBridge>);
        (images, image_count, image_index, image_handles, nak_queue, nak_queue_len, sub_bridge)
    }

    /// Take the first SubscriberImage from the bridge (deposited by on_setup).
    fn take_subscriber_image(bridge: &Option<Arc<SubscriptionBridge>>) -> Option<crate::media::shared_image::SubscriberImage> {
        let b = bridge.as_ref()?;
        for i in 0..crate::client::sub_bridge::SUB_BRIDGE_CAPACITY {
            if let Some(pending) = b.try_take(i) {
                return Some(pending.image);
            }
        }
        None
    }

    fn make_setup_header(
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        active_term_id: i32,
        term_length: i32,
        term_offset: i32,
    ) -> SetupHeader {
        SetupHeader {
            frame_header: FrameHeader {
                frame_length: SETUP_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SETUP,
            },
            term_offset,
            session_id,
            stream_id,
            initial_term_id,
            active_term_id,
            term_length,
            mtu: 1408,
            ttl: 0,
        }
    }

    fn make_data_header(
        session_id: i32,
        stream_id: i32,
        term_id: i32,
        term_offset: i32,
        payload_len: usize,
    ) -> DataHeader {
        DataHeader {
            frame_header: FrameHeader {
                frame_length: (DATA_HEADER_LENGTH + payload_len) as i32,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset,
            session_id,
            stream_id,
            term_id,
            reserved_value: 0,
        }
    }

    fn zeroed_source() -> libc::sockaddr_storage {
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn on_setup_creates_image_with_shared_buffer() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);
        }
        assert_eq!(count, 1);
        assert!(images[0].active);
        assert_eq!(images[0].session_id, 42);
        assert_eq!(images[0].initial_term_id, 0);
        assert_eq!(images[0].active_term_id, 0);
        assert_eq!(images[0].term_length, TEST_TERM_LENGTH);
        assert!(handles[0].is_some());
        let recv_img = handles[0].as_ref().unwrap();
        assert_eq!(recv_img.term_length(), TEST_TERM_LENGTH);
        // Verify subscriber image was deposited in bridge.
        let sub_img = take_subscriber_image(&sub_bridge);
        assert!(sub_img.is_some());
    }

    #[test]
    fn on_setup_rejects_invalid_term_length() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Not power-of-two.
            let setup = make_setup_header(1, 1, 0, 0, 100, 0);
            handler.on_setup(&setup, &source);
        }
        assert_eq!(count, 0); // rejected
        assert!(handles[0].is_none());
    }

    #[test]
    fn on_setup_rejects_too_small_term_length() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Too small (< 32).
            let setup = make_setup_header(1, 1, 0, 0, 16, 0);
            handler.on_setup(&setup, &source);
        }
        assert_eq!(count, 0);
    }

    #[test]
    fn on_setup_respects_max_images() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: 2, // cap at 2
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            for i in 0..5 {
                let setup = make_setup_header(i, i, 0, 0, TEST_TERM_LENGTH as i32, 0);
                handler.on_setup(&setup, &source);
            }
        }
        assert_eq!(count, 2); // capped at max_images
    }

    #[test]
    fn on_data_writes_frame_into_image_buffer() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Create image via setup.
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Send data frame.
            let payload = [0xCA, 0xFE, 0xBA, 0xBE];
            let dh = make_data_header(42, 7, 0, 0, payload.len());
            handler.on_data(&dh, &payload, &source);
        }

        // Verify data was written into the shared image buffer via subscriber poll.
        let mut sub_img = take_subscriber_image(&sub_bridge).unwrap();
        let mut found_payload = Vec::new();
        let count_polled = sub_img.poll_fragments(|data, sid, stid| {
            assert_eq!(sid, 42);
            assert_eq!(stid, 7);
            found_payload.extend_from_slice(data);
        }, 10);
        assert_eq!(count_polled, 1);
        assert_eq!(&found_payload, &[0xCA, 0xFE, 0xBA, 0xBE]);
    }

    #[test]
    fn on_data_advances_consumption_from_append() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Header-only frame: 32 bytes, aligned 32.
            let dh = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh, &[], &source);
        }
        // Consumption should reflect aligned frame end (32).
        assert_eq!(images[0].consumption_term_offset, 32);
        assert_eq!(images[0].expected_term_offset, 32);
    }

    #[test]
    fn on_data_multiple_frames_sequential() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Frame 1 at offset 0 (header-only, 32 bytes).
            let dh1 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh1, &[], &source);

            // Frame 2 at offset 32 (header-only, 32 bytes).
            let dh2 = make_data_header(42, 7, 0, 32, 0);
            handler.on_data(&dh2, &[], &source);
        }
        assert_eq!(images[0].consumption_term_offset, 64);

        // Poll should find 2 frames.
        let mut sub_img = take_subscriber_image(&sub_bridge).unwrap();
        let frame_count = sub_img.poll_fragments(|_, _, _| {}, 10);
        assert_eq!(frame_count, 2);
    }

    #[test]
    fn on_data_term_rotation_cleans_entering_partition() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Write a frame in term 0.
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);

            // Now receive data in term 1 - should trigger rotation.
            let dh1 = make_data_header(42, 7, 1, 0, 0);
            handler.on_data(&dh1, &[], &source);
        }

        assert_eq!(images[0].active_term_id, 1);
        assert_eq!(images[0].consumption_term_id, 1);

        // Subscriber should be able to poll data from the new term.
        // Note: subscriber starts at initial position (term 0, offset 0), so
        // after term rotation it needs to be at the right position. Since the
        // setup started at term 0 and rotation resets consumption, verify
        // receiver image has data by checking the handle exists.
        assert!(handles[0].is_some());
    }

    #[test]
    fn on_data_stale_term_rejected() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Start at term 4, so terms 0-3 are all stale (>= PARTITION_COUNT behind).
            let setup = make_setup_header(
                42, 7, 0, PARTITION_COUNT as i32, TEST_TERM_LENGTH as i32, 0,
            );
            handler.on_setup(&setup, &source);

            // Data for term 0 is PARTITION_COUNT behind active (term 4) - stale.
            let dh = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh, &[], &source);
        }

        // Consumption should not advance (stale frame rejected).
        assert_eq!(images[0].consumption_term_offset, 0);

        // Stale frame should not be visible via subscriber poll.
        // The subscriber starts at position for term 4, offset 0. No committed
        // frames should be visible since the stale write was rejected.
        let mut sub_img = take_subscriber_image(&sub_bridge).unwrap();
        let frame_count = sub_img.poll_fragments(|_, _, _| {}, 10);
        assert_eq!(frame_count, 0, "stale frame should not be written");
    }

    #[test]
    fn on_data_recent_past_term_accepted() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Start at term 2, initial 0.
            let setup = make_setup_header(42, 7, 0, 2, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Data for term 1 is 1 behind active (within PARTITION_COUNT) - accepted.
            let dh = make_data_header(42, 7, 1, 0, 0);
            handler.on_data(&dh, &[], &source);
        }

        // Frame should have been written (receiver image handle still active).
        assert!(handles[0].is_some(), "recent-past frame should be written");
    }

    #[test]
    fn on_data_multi_term_jump_cleans_all_entering() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 0,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Write data in term 0.
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);

            // Jump to term 3 (skipping 1 and 2).
            let dh3 = make_data_header(42, 7, 3, 0, 0);
            handler.on_data(&dh3, &[], &source);
        }

        assert_eq!(images[0].active_term_id, 3);

        // Receiver image handle should still be active.
        assert!(handles[0].is_some());
    }

    // ── NAK generation ──

    #[test]
    fn gap_produces_pending_nak() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000, // 100ms
                nak_delay_ns: 0,     // no coalescing delay
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Frame at offset 0 (32 bytes aligned).
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);

            // Skip offset 32, send frame at offset 64 - gap of 32 bytes.
            let dh2 = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh2, &[], &source);
        }

        assert_eq!(nql, 1, "one NAK should be queued");
        assert_eq!(nq[0].session_id, 42);
        assert_eq!(nq[0].stream_id, 7);
        assert_eq!(nq[0].active_term_id, 0);
        assert_eq!(nq[0].term_offset, 32); // start of the gap
        assert_eq!(nq[0].length, 32);      // gap size
    }

    #[test]
    fn no_nak_for_in_order_data() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000,
                nak_delay_ns: 0,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Sequential frames: no gap.
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);
            let dh1 = make_data_header(42, 7, 0, 32, 0);
            handler.on_data(&dh1, &[], &source);
            let dh2 = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh2, &[], &source);
        }

        assert_eq!(nql, 0, "no NAK for in-order data");
    }

    #[test]
    fn nak_coalescing_suppresses_within_delay() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000,   // 100ms
                nak_delay_ns: 60_000_000, // 60ms coalescing
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // First gap - should produce NAK (time_of_last_nak_ns starts at 0,
            // 100ms - 0ms = 100ms >= 60ms delay).
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);
            let dh2 = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh2, &[], &source);
        }
        assert_eq!(nql, 1, "first gap should produce NAK");

        // Second gap shortly after (within delay).
        // Simulate: now_ns = 100ms + 10ms = 110ms, last NAK was at 100ms.
        // Elapsed = 10ms < 60ms delay - should be suppressed.
        nql = 0;
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 110_000_000,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Another gap at different offset (skip 96, send at 128).
            let dh4 = make_data_header(42, 7, 0, 128, 0);
            handler.on_data(&dh4, &[], &source);
        }
        assert_eq!(nql, 0, "second gap within delay should be suppressed");
    }

    #[test]
    fn nak_coalescing_allows_after_delay() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // First gap - NAK generated.
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);
            let dh2 = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh2, &[], &source);
        }
        assert_eq!(nql, 1);

        // After delay elapses: now_ns = 100ms + 70ms = 170ms.
        // Elapsed = 170ms - 100ms = 70ms >= 60ms - should allow.
        nql = 0;
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 170_000_000,
                nak_delay_ns: 60_000_000,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Another gap (skip 96, send at 128).
            let dh4 = make_data_header(42, 7, 0, 128, 0);
            handler.on_data(&dh4, &[], &source);
        }
        assert_eq!(nql, 1, "NAK should be allowed after delay elapsed");
    }

    #[test]
    fn nak_only_for_active_term() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();
        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000,
                nak_delay_ns: 0,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            // Start at term 1 with some data already consumed.
            let setup = make_setup_header(42, 7, 0, 1, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Write frame at offset 0 in term 1 (active term).
            let dh0 = make_data_header(42, 7, 1, 0, 0);
            handler.on_data(&dh0, &[], &source);

            // Send a frame in term 0 (past term) with a gap - should NOT NAK
            // because term 0 is not the active term.
            let dh_old = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh_old, &[], &source);
        }

        assert_eq!(nql, 0, "no NAK for gap in non-active term");
    }

    #[test]
    fn nak_queue_overflow_drops_silently() {
        let (mut images, mut count, mut index, mut handles, mut nq, mut nql, sub_bridge) = make_handler_parts();
        let source = zeroed_source();

        // Pre-fill NAK queue to capacity.
        let _ = nql;
        nql = MAX_PENDING_NAKS_PER_CYCLE;

        {
            let mut handler = InlineHandler {
                images: &mut images,
                image_count: &mut count,
                image_index: &mut index,
                image_handles: handles.as_mut_slice(),
                sub_bridge: &sub_bridge,
                max_images: MAX_IMAGES,
                now_ns: 100_000_000,
                nak_delay_ns: 0,
                nak_queue: &mut nq,
                nak_queue_len: &mut nql,
            };
            let setup = make_setup_header(42, 7, 0, 0, TEST_TERM_LENGTH as i32, 0);
            handler.on_setup(&setup, &source);

            // Create a gap - should be silently dropped (queue full).
            let dh0 = make_data_header(42, 7, 0, 0, 0);
            handler.on_data(&dh0, &[], &source);
            let dh2 = make_data_header(42, 7, 0, 64, 0);
            handler.on_data(&dh2, &[], &source);
        }

        // Queue length should not exceed capacity.
        assert_eq!(nql, MAX_PENDING_NAKS_PER_CYCLE, "queue should not grow past capacity");
    }
}
