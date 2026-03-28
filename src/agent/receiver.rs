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
        ctx.validate().map_err(|e| {
            AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
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

            // Re-insert at the new images[] index.
            insert_image_index(
                &mut self.image_index, idx, moved_session, moved_stream,
            );
        }

        // Clear the vacated last slot.
        self.images[last] = ImageEntry::default();
        self.image_count = last;

        true
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
}
