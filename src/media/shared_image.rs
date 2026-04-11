// Shared image buffer - cross-thread data delivery from receiver to subscriber.
//
// Splits the image into two handles:
//   - ReceiverImage (receiver agent thread, writer)
//   - SubscriberImage (client application thread, reader)
//
// Both share an Arc<ImageInner> containing:
//   - SharedLogBuffer (UnsafeCell<Vec<u8>>, allocated once)
//   - receiver_position: AtomicI64 (written by receiver, read by subscriber)
//   - subscriber_position: AtomicI64 (written by subscriber, read by receiver)
//   - Immutable config (session_id, stream_id, term_length, etc.)
//
// Commit protocol (mirrors ConcurrentPublication but reversed):
//   Receiver writes header+payload, then stores frame_length with Release.
//   Subscriber loads frame_length with Acquire - if > 0, the frame is visible.
//
// No Mutex. No SeqCst. Zero allocation in steady state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crate::frame::{DATA_HEADER_LENGTH, FRAME_TYPE_PAD};
use crate::media::term_buffer::{
    AppendError, FRAME_ALIGNMENT, PARTITION_COUNT, SharedLogBuffer, align_frame_length,
    atomic_frame_length_load, atomic_frame_length_store, partition_index,
};

// ---- Shared inner state ----

/// Shared state between receiver and subscriber, wrapped in Arc.
/// Allocated once at construction (cold path). Never resized.
struct ImageInner {
    log: SharedLogBuffer,
    /// Highest written byte position. Written by receiver (Release),
    /// read by subscriber (Acquire) to know how far data extends.
    receiver_position: AtomicI64,
    /// Highest read byte position. Written by subscriber (Release),
    /// read by receiver (Acquire) for back-pressure.
    subscriber_position: AtomicI64,
    /// Set by receiver when this image is closed/removed.
    /// Subscriber checks during poll and removes the image locally.
    closed: AtomicBool,

    // Immutable after construction - no atomics needed.
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    term_length: u32,
    term_length_mask: u32,
    position_bits_to_shift: u32,
}

// ---- ReceiverImage (writer handle) ----

/// Writer handle owned by the ReceiverAgent.
///
/// Takes `&mut self` for all write operations - single writer enforced
/// at compile time. Not Clone. Send.
pub struct ReceiverImage {
    inner: Arc<ImageInner>,
    /// Local cached position. Single writer - avoids atomic load per write.
    receiver_position_local: i64,
}

// ReceiverImage is Send (moved to receiver thread) but not Sync.
// SAFETY: single writer, no concurrent access to mutable state.
unsafe impl Send for ReceiverImage {}

// ---- SubscriberImage (reader handle) ----

/// Reader handle owned by the Subscription on the client thread.
///
/// Takes `&mut self` for `poll_fragments` - single reader per image.
/// Not Clone. Send.
pub struct SubscriberImage {
    inner: Arc<ImageInner>,
    /// Local cached position. Single reader - avoids atomic load per poll.
    subscriber_position_local: i64,
}

// SubscriberImage is Send (moved to client thread) but not Sync.
unsafe impl Send for SubscriberImage {}

// ---- Construction ----

/// Create a shared image pair (receiver writer + subscriber reader).
///
/// Cold path - allocation happens here, once (SharedLogBuffer + Arc).
///
/// Returns `None` if:
/// - `term_length` is not a power-of-two or < 32 (FRAME_ALIGNMENT)
pub fn new_shared_image(
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    active_term_id: i32,
    term_length: u32,
    initial_term_offset: i32,
) -> Option<(ReceiverImage, SubscriberImage)> {
    let log = SharedLogBuffer::new(term_length)?;

    let term_length_mask = term_length - 1;
    let position_bits_to_shift = term_length.trailing_zeros();

    // Compute initial position from active_term_id + initial_term_offset.
    let term_count = active_term_id.wrapping_sub(initial_term_id) as i64;
    let initial_position = term_count * term_length as i64 + initial_term_offset as i64;

    let inner = Arc::new(ImageInner {
        log,
        receiver_position: AtomicI64::new(initial_position),
        subscriber_position: AtomicI64::new(initial_position),
        closed: AtomicBool::new(false),
        session_id,
        stream_id,
        initial_term_id,
        term_length,
        term_length_mask,
        position_bits_to_shift,
    });

    let receiver = ReceiverImage {
        inner: Arc::clone(&inner),
        receiver_position_local: initial_position,
    };

    let subscriber = SubscriberImage {
        inner,
        subscriber_position_local: initial_position,
    };

    Some((receiver, subscriber))
}

// ---- ReceiverImage impl ----

impl ReceiverImage {
    /// Append a data frame (header + payload) into the image's shared log buffer.
    ///
    /// Writes the DataHeader and payload, then commits with an atomic Release
    /// store of frame_length so the subscriber can see it.
    ///
    /// Returns the new term_offset on success.
    ///
    /// Zero-allocation, O(1). Hot path.
    pub fn append_frame(
        &mut self,
        partition_idx: usize,
        term_offset: u32,
        header: &crate::frame::DataHeader,
        payload: &[u8],
    ) -> Result<u32, AppendError> {
        if partition_idx >= PARTITION_COUNT {
            return Err(AppendError::InvalidPartition);
        }

        let inner = &*self.inner;
        let raw_len = DATA_HEADER_LENGTH + payload.len();
        let aligned_len = align_frame_length(raw_len) as u32;
        let new_offset = term_offset + aligned_len;

        if new_offset > inner.term_length {
            return Err(AppendError::TermFull);
        }

        let tl = inner.term_length as usize;
        let base = partition_idx * tl + term_offset as usize;
        let frame_length = raw_len as i32;

        // SAFETY: Single writer (ReceiverImage takes &mut self).
        // Back-pressure ensures subscriber finished reading this region.
        // The buffer is allocated once and never resized.
        unsafe {
            let buf_ptr = inner.log.as_mut_ptr();

            // Step 1: Write header bytes [0..32] with frame_length = 0.
            // Zero frame_length means "not yet committed" to the subscriber.
            let mut hdr_copy = std::ptr::read(header);
            hdr_copy.frame_header.frame_length = 0;
            std::ptr::copy_nonoverlapping(
                &hdr_copy as *const crate::frame::DataHeader as *const u8,
                buf_ptr.add(base),
                DATA_HEADER_LENGTH,
            );

            // Step 2: Write payload bytes [32..32+len].
            if !payload.is_empty() {
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    buf_ptr.add(base + DATA_HEADER_LENGTH),
                    payload.len(),
                );
            }

            // Step 3: COMMIT - atomic Release store of the real frame_length.
            // All preceding writes become visible to any thread that reads
            // this word with Acquire.
            atomic_frame_length_store(buf_ptr, base, frame_length);
        }

        Ok(new_offset)
    }

    /// Update the receiver position (Release store).
    ///
    /// Called after writing frames so the subscriber knows data is available.
    #[inline]
    pub fn advance_receiver_position(&mut self, new_pos: i64) {
        self.receiver_position_local = new_pos;
        self.inner
            .receiver_position
            .store(new_pos, Ordering::Release);
    }

    /// Check if the subscriber is too far behind (back-pressure).
    ///
    /// Returns `true` if the gap between receiver and subscriber exceeds
    /// (PARTITION_COUNT - 1) terms - same limit as ConcurrentPublication.
    /// When true, the receiver should skip writing to this image to avoid
    /// overwriting unread data.
    #[inline]
    pub fn check_back_pressure(&self) -> bool {
        let max_ahead = self.inner.term_length as i64 * (PARTITION_COUNT as i64 - 1);
        let sub_pos = self.inner.subscriber_position.load(Ordering::Acquire);
        self.receiver_position_local.wrapping_sub(sub_pos) >= max_ahead
    }

    /// Zero out a partition during term rotation.
    ///
    /// # Safety
    ///
    /// Caller must ensure the subscriber has finished reading this partition
    /// (guaranteed by back-pressure: receiver is at most 3 terms ahead).
    pub fn clean_partition(&self, partition_idx: usize) {
        // SAFETY: Back-pressure guarantees subscriber finished reading.
        unsafe {
            self.inner.log.clean_partition(partition_idx);
        }
    }

    /// Signal that this image is closed. The subscriber will detect this
    /// on the next poll and remove the image.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }

    /// Session ID for this image.
    #[inline]
    pub fn session_id(&self) -> i32 {
        self.inner.session_id
    }

    /// Stream ID for this image.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.inner.stream_id
    }

    /// Term length in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.inner.term_length
    }

    /// Current receiver position (local cached copy).
    #[inline]
    pub fn receiver_position(&self) -> i64 {
        self.receiver_position_local
    }
}

// ---- SubscriberImage impl ----

impl SubscriberImage {
    /// Poll for committed fragments in this image.
    ///
    /// Walks forward through committed frames using Acquire loads on
    /// frame_length (same scan pattern as SenderPublication::sender_scan).
    /// Calls `handler(payload, session_id, stream_id)` for each data fragment.
    ///
    /// Gap handling (Aeron-style loss recovery):
    ///   When frame_length <= 0, checks receiver_position (Acquire) to
    ///   distinguish "no more data" from "gap in the middle of written data".
    ///   If a gap is detected (receiver ahead), scans forward to find the
    ///   next committed frame and skips the gap. Lost bytes are not delivered.
    ///
    /// Pad frame handling:
    ///   Frames with frame_type == FRAME_TYPE_PAD are skipped (position
    ///   advances) without invoking the handler. Pad frames appear at
    ///   term boundaries (written by the publisher) and in gap-fill regions
    ///   (written by the receiver after retransmit timeout).
    ///
    /// Returns the number of data fragments delivered (not bytes, not pads).
    ///
    /// After polling, stores subscriber_position with Release so the
    /// receiver can see freed space for back-pressure.
    ///
    /// Zero-allocation, O(n) in fragments polled (normal path).
    /// Gap scan is O(gap_size / FRAME_ALIGNMENT), bounded by term_length.
    pub fn poll_fragments<F>(&mut self, mut handler: F, limit: i32) -> i32
    where
        F: FnMut(&[u8], i32, i32),
    {
        if limit <= 0 {
            return 0;
        }

        let inner = &*self.inner;

        let term_id = inner
            .initial_term_id
            .wrapping_add((self.subscriber_position_local >> inner.position_bits_to_shift) as i32);
        let term_offset = (self.subscriber_position_local & inner.term_length_mask as i64) as u32;

        let part_idx = partition_index(term_id, inner.initial_term_id);

        let tl = inner.term_length as usize;
        let part_base = part_idx * tl;
        let buf_ptr = inner.log.as_ptr();

        let mut pos = term_offset;
        let mut fragments = 0i32;
        let mut bytes_scanned: u32 = 0;

        while pos < inner.term_length && fragments < limit {
            let remaining = (inner.term_length - pos) as usize;
            if remaining < 4 {
                break;
            }

            let byte_offset = part_base + pos as usize;

            // Acquire load of frame_length - pairs with receiver's Release store.
            let frame_length = unsafe { atomic_frame_length_load(buf_ptr, byte_offset) };

            if frame_length <= 0 {
                // Check if this is a gap (receiver has written past this point)
                // or genuine end of available data.
                let recv_pos = inner.receiver_position.load(Ordering::Acquire);
                let current_pos = self.subscriber_position_local + bytes_scanned as i64;
                if recv_pos <= current_pos {
                    // No data past this point - normal end of available data.
                    break;
                }

                // Gap detected - receiver is ahead. Scan forward for the next
                // committed frame within this term. The scan is bounded by
                // term_length / FRAME_ALIGNMENT (at most 2048 for 64 KiB term).
                let mut skip = FRAME_ALIGNMENT as u32;
                while pos + skip < inner.term_length {
                    let fl = unsafe {
                        atomic_frame_length_load(buf_ptr, part_base + (pos + skip) as usize)
                    };
                    if fl > 0 {
                        break;
                    }
                    skip += FRAME_ALIGNMENT as u32;
                }

                // Clamp skip to term boundary.
                if pos + skip > inner.term_length {
                    skip = inner.term_length - pos;
                }

                pos += skip;
                bytes_scanned += skip;
                continue;
            }

            let frame_len_u = frame_length as u32;

            // Frame too small to contain a DataHeader.
            if frame_len_u < DATA_HEADER_LENGTH as u32 {
                break;
            }

            let aligned_len = align_frame_length(frame_len_u as usize) as u32;

            // Would exceed partition bounds.
            if pos + aligned_len > inner.term_length {
                break;
            }

            // Check frame_type for FRAME_TYPE_PAD. Read from offset 6 within
            // the frame (frame_type field is at bytes [6..8] of FrameHeader).
            // Field-by-field little-endian decode per coding rules.
            let frame_type = unsafe {
                let ft_ptr = buf_ptr.add(byte_offset + 6);
                u16::from_le_bytes([*ft_ptr, *ft_ptr.add(1)])
            };

            if frame_type == FRAME_TYPE_PAD {
                // Pad frame - advance position without delivering to handler.
                pos += aligned_len;
                bytes_scanned += aligned_len;
                continue;
            }

            // Extract payload (everything after the 32-byte DataHeader).
            let payload_start = byte_offset + DATA_HEADER_LENGTH;
            let payload_len = frame_len_u as usize - DATA_HEADER_LENGTH;

            // SAFETY: Acquire load of frame_length guarantees all header +
            // payload bytes written by the receiver (before their Release
            // store) are visible.
            if payload_len > 0 {
                let payload =
                    unsafe { std::slice::from_raw_parts(buf_ptr.add(payload_start), payload_len) };
                handler(payload, inner.session_id, inner.stream_id);
            } else {
                handler(&[], inner.session_id, inner.stream_id);
            }

            pos += aligned_len;
            bytes_scanned += aligned_len;
            fragments += 1;
        }

        // Release store of subscriber_position so receiver sees freed space.
        if bytes_scanned > 0 {
            self.subscriber_position_local += bytes_scanned as i64;
            inner
                .subscriber_position
                .store(self.subscriber_position_local, Ordering::Release);
        }

        fragments
    }

    /// Check if the receiver has closed this image.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Session ID for this image.
    #[inline]
    pub fn session_id(&self) -> i32 {
        self.inner.session_id
    }

    /// Stream ID for this image.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.inner.stream_id
    }

    /// Current subscriber position (local cached copy).
    #[inline]
    pub fn position(&self) -> i64 {
        self.subscriber_position_local
    }

    /// Initial term ID.
    #[inline]
    pub fn initial_term_id(&self) -> i32 {
        self.inner.initial_term_id
    }

    /// Term length in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.inner.term_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::*;
    use crate::media::term_buffer::FRAME_ALIGNMENT;

    const TEST_TERM_LENGTH: u32 = 1024;

    fn make_data_header(
        frame_length: i32,
        term_offset: i32,
        session_id: i32,
        stream_id: i32,
        term_id: i32,
    ) -> DataHeader {
        DataHeader {
            frame_header: FrameHeader {
                frame_length,
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

    #[test]
    fn new_shared_image_valid_params() {
        let result = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0);
        assert!(result.is_some());
        let (recv, sub) = result.unwrap();
        assert_eq!(recv.session_id(), 42);
        assert_eq!(sub.session_id(), 42);
        assert_eq!(recv.stream_id(), 7);
        assert_eq!(sub.stream_id(), 7);
        assert_eq!(recv.term_length(), TEST_TERM_LENGTH);
        assert_eq!(sub.term_length(), TEST_TERM_LENGTH);
    }

    #[test]
    fn new_shared_image_rejects_invalid() {
        // Not power of two.
        assert!(new_shared_image(1, 1, 0, 0, 100, 0).is_none());
        // Too small.
        assert!(new_shared_image(1, 1, 0, 0, 16, 0).is_none());
        // Zero.
        assert!(new_shared_image(1, 1, 0, 0, 0, 0).is_none());
    }

    #[test]
    fn poll_returns_zero_when_empty() {
        let (_, mut sub) = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0).unwrap();
        let count = sub.poll_fragments(|_, _, _| panic!("should not be called"), 10);
        assert_eq!(count, 0);
    }

    #[test]
    fn append_and_poll_single_frame() {
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0).unwrap();

        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let hdr = make_data_header(frame_len, 0, 42, 7, 0);

        let new_offset = recv.append_frame(0, 0, &hdr, &payload).unwrap();
        assert!(new_offset > 0);

        // Advance receiver position so subscriber can see it.
        recv.advance_receiver_position(new_offset as i64);

        // Poll should find the frame.
        let mut found_payload = Vec::new();
        let count = sub.poll_fragments(
            |data, sid, stid| {
                assert_eq!(sid, 42);
                assert_eq!(stid, 7);
                found_payload.extend_from_slice(data);
            },
            10,
        );

        assert_eq!(count, 1);
        assert_eq!(&found_payload, &payload);
    }

    #[test]
    fn poll_advances_subscriber_position() {
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0).unwrap();

        let payload = [0xAB; 8];
        let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let hdr = make_data_header(frame_len, 0, 42, 7, 0);

        recv.append_frame(0, 0, &hdr, &payload).unwrap();
        recv.advance_receiver_position(align_frame_length(frame_len as usize) as i64);

        assert_eq!(sub.position(), 0);
        sub.poll_fragments(|_, _, _| {}, 10);
        assert!(sub.position() > 0, "position should advance after poll");

        // Second poll should find nothing.
        let count = sub.poll_fragments(|_, _, _| panic!("no more"), 10);
        assert_eq!(count, 0);
    }

    #[test]
    fn multiple_frames_polled_in_order() {
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0).unwrap();

        let mut offset: u32 = 0;
        for i in 0..5u8 {
            let payload = [i; 4];
            let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
            let hdr = make_data_header(frame_len, offset as i32, 42, 7, 0);
            let new_off = recv.append_frame(0, offset, &hdr, &payload).unwrap();
            offset = new_off;
        }
        recv.advance_receiver_position(offset as i64);

        let mut seen = Vec::new();
        let count = sub.poll_fragments(
            |data, _, _| {
                seen.push(data[0]);
            },
            10,
        );

        assert_eq!(count, 5);
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn poll_respects_limit() {
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, TEST_TERM_LENGTH, 0).unwrap();

        let mut offset: u32 = 0;
        for i in 0..5u8 {
            let payload = [i; 4];
            let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
            let hdr = make_data_header(frame_len, offset as i32, 42, 7, 0);
            offset = recv.append_frame(0, offset, &hdr, &payload).unwrap();
        }
        recv.advance_receiver_position(offset as i64);

        // Poll only 2.
        let count = sub.poll_fragments(|_, _, _| {}, 2);
        assert_eq!(count, 2);

        // Poll remaining 3.
        let count = sub.poll_fragments(|_, _, _| {}, 10);
        assert_eq!(count, 3);
    }

    #[test]
    fn back_pressure_detected() {
        // Use smallest valid term so back-pressure triggers quickly.
        let tl: u32 = 64;
        let (mut recv, _sub) = new_shared_image(1, 1, 0, 0, tl, 0).unwrap();

        // Advance receiver position far ahead without subscriber advancing.
        let max_ahead = tl as i64 * (PARTITION_COUNT as i64 - 1);
        recv.advance_receiver_position(max_ahead);

        assert!(recv.check_back_pressure(), "should detect back-pressure");
    }

    #[test]
    fn close_signal_visible() {
        let (recv, sub) = new_shared_image(1, 1, 0, 0, TEST_TERM_LENGTH, 0).unwrap();
        assert!(!sub.is_closed());
        recv.close();
        assert!(sub.is_closed());
    }

    #[test]
    fn term_rotation_and_poll() {
        // Use a small term (256 bytes) so we can fill partition 0 quickly.
        // 256 / 32 = 8 header-only frames per partition.
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).expect("valid params");

        // Fill partition 0 (term 0) completely with 8 header-only frames.
        let frames_per_term = (tl / FRAME_ALIGNMENT as u32) as usize;
        let mut offset: u32 = 0;
        for i in 0..frames_per_term {
            let payload = [i as u8; 0]; // header-only
            let frame_len = DATA_HEADER_LENGTH as i32;
            let hdr = make_data_header(frame_len, offset as i32, 42, 7, 0);
            offset = recv
                .append_frame(0, offset, &hdr, &payload)
                .expect("append in term 0");
        }
        assert_eq!(offset, tl, "partition 0 should be exactly full");

        // Clean partition 1 before writing (simulates receiver term rotation).
        recv.clean_partition(1);

        // Write 3 frames into partition 1 (term 1) at offset 0.
        let term1_pidx = partition_index(1, 0);
        assert_eq!(term1_pidx, 1);
        let mut t1_offset: u32 = 0;
        for i in 0..3u8 {
            let payload = [0xA0 | i; 4];
            let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
            let hdr = make_data_header(frame_len, t1_offset as i32, 42, 7, 1);
            t1_offset = recv
                .append_frame(term1_pidx, t1_offset, &hdr, &payload)
                .expect("append in term 1");
        }

        // Advance receiver position to cover both terms.
        let total_position = tl as i64 + t1_offset as i64;
        recv.advance_receiver_position(total_position);

        // Poll term 0 - should get all 8 frames.
        let mut term0_count = 0i32;
        term0_count += sub.poll_fragments(
            |_, sid, stid| {
                assert_eq!(sid, 42);
                assert_eq!(stid, 7);
            },
            100,
        );
        assert_eq!(
            term0_count, frames_per_term as i32,
            "should poll all term 0 frames"
        );
        assert_eq!(
            sub.position(),
            tl as i64,
            "position should be at term boundary"
        );

        // Poll term 1 - subscriber should automatically rotate to partition 1.
        let mut term1_payloads = Vec::new();
        let term1_count = sub.poll_fragments(
            |data, _, _| {
                term1_payloads.push(data[0]);
            },
            100,
        );
        assert_eq!(term1_count, 3, "should poll 3 frames from term 1");
        assert_eq!(term1_payloads, vec![0xA0, 0xA1, 0xA2]);
        assert_eq!(
            sub.position(),
            total_position,
            "final position should match receiver position"
        );

        // One more poll should find nothing.
        let empty = sub.poll_fragments(|_, _, _| panic!("no more"), 10);
        assert_eq!(empty, 0);
    }

    #[test]
    fn term_rotation_multi_term_jump() {
        // Verify subscriber can jump across multiple terms (0 -> 1 -> 2).
        let tl: u32 = 128; // 128 / 32 = 4 frames per partition
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).expect("valid params");

        let frames_per_term = (tl / FRAME_ALIGNMENT as u32) as usize;

        // Fill partition 0 (term 0).
        let mut offset: u32 = 0;
        for _ in 0..frames_per_term {
            let hdr = make_data_header(DATA_HEADER_LENGTH as i32, offset as i32, 42, 7, 0);
            offset = recv.append_frame(0, offset, &hdr, &[]).expect("p0");
        }

        // Fill partition 1 (term 1).
        recv.clean_partition(1);
        let mut t1_off: u32 = 0;
        for _ in 0..frames_per_term {
            let hdr = make_data_header(DATA_HEADER_LENGTH as i32, t1_off as i32, 42, 7, 1);
            t1_off = recv.append_frame(1, t1_off, &hdr, &[]).expect("p1");
        }

        // Write 2 frames into partition 2 (term 2).
        recv.clean_partition(2);
        let mut t2_off: u32 = 0;
        for _ in 0..2 {
            let hdr = make_data_header(DATA_HEADER_LENGTH as i32, t2_off as i32, 42, 7, 2);
            t2_off = recv.append_frame(2, t2_off, &hdr, &[]).expect("p2");
        }

        let total_pos = (tl as i64) * 2 + t2_off as i64;
        recv.advance_receiver_position(total_pos);

        // Poll through all three terms.
        let mut grand_total = 0i32;

        // Term 0.
        grand_total += sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(grand_total, frames_per_term as i32);

        // Term 1.
        grand_total += sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(grand_total, (frames_per_term * 2) as i32);

        // Term 2.
        grand_total += sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(grand_total, (frames_per_term * 2 + 2) as i32);

        assert_eq!(sub.position(), total_pos);
    }

    #[test]
    fn term_rotation_with_nonzero_initial_term_id() {
        // Verify partition_index math works with initial_term_id != 0.
        let tl: u32 = 128;
        let initial_term_id = 100;
        let (mut recv, mut sub) =
            new_shared_image(42, 7, initial_term_id, initial_term_id, tl, 0).expect("valid params");

        let frames_per_term = (tl / FRAME_ALIGNMENT as u32) as usize;

        // Partition for term 100 is partition_index(100, 100) = 0.
        let p0 = partition_index(initial_term_id, initial_term_id);
        assert_eq!(p0, 0);

        // Fill partition 0 (term 100).
        let mut offset: u32 = 0;
        for _ in 0..frames_per_term {
            let hdr = make_data_header(
                DATA_HEADER_LENGTH as i32,
                offset as i32,
                42,
                7,
                initial_term_id,
            );
            offset = recv.append_frame(p0, offset, &hdr, &[]).expect("p0");
        }

        // Write 1 frame into partition 1 (term 101).
        let p1 = partition_index(initial_term_id + 1, initial_term_id);
        assert_eq!(p1, 1);
        recv.clean_partition(p1);
        let payload = [0xFF; 4];
        let frame_len = DATA_HEADER_LENGTH as i32 + 4;
        let hdr = make_data_header(frame_len, 0, 42, 7, initial_term_id + 1);
        let t1_end = recv.append_frame(p1, 0, &hdr, &payload).expect("p1");

        let total_pos = tl as i64 + t1_end as i64;
        recv.advance_receiver_position(total_pos);

        // Poll term 100.
        let c0 = sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(c0, frames_per_term as i32);

        // Poll term 101 - should get 1 frame with correct payload.
        let mut got_payload = Vec::new();
        let c1 = sub.poll_fragments(
            |data, _, _| {
                got_payload.extend_from_slice(data);
            },
            100,
        );
        assert_eq!(c1, 1);
        assert_eq!(&got_payload, &[0xFF; 4]);
    }

    #[test]
    fn cross_thread_write_poll() {
        // 20 frames * 64 bytes each = 1280 bytes; need term_length >= 1280.
        // Use 2048 (next power-of-two) so all frames fit in one partition.
        let cross_term_length: u32 = 2048;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, cross_term_length, 0).unwrap();

        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_clone = done.clone();

        let writer = std::thread::spawn(move || {
            let mut offset: u32 = 0;
            for i in 0..20u8 {
                let payload = [i; 4];
                let frame_len = DATA_HEADER_LENGTH as i32 + 4;
                let hdr = make_data_header(frame_len, offset as i32, 42, 7, 0);
                let new_off = recv.append_frame(0, offset, &hdr, &payload).unwrap();
                offset = new_off;
                recv.advance_receiver_position(offset as i64);
            }
            done_clone.store(true, std::sync::atomic::Ordering::Release);
        });

        let mut total = 0i32;
        loop {
            let n = sub.poll_fragments(|_, _, _| {}, 10);
            total += n;
            if total >= 20 {
                break;
            }
            if n == 0 && done.load(std::sync::atomic::Ordering::Acquire) {
                // Final drain.
                total += sub.poll_fragments(|_, _, _| {}, 100);
                break;
            }
            std::thread::yield_now();
        }

        writer.join().unwrap();
        assert_eq!(total, 20);
    }

    // ---- Gap-skip tests ----

    #[test]
    fn gap_skip_single_frame_gap() {
        // Write frames at offsets 0 and 64, leaving a gap at 32.
        // Subscriber should skip the gap and deliver both frames.
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        // Frame at offset 0 (header-only, 32 bytes).
        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();

        // Skip offset 32 (gap), write at offset 64.
        let hdr2 = make_data_header(DATA_HEADER_LENGTH as i32, 64, 42, 7, 0);
        recv.append_frame(0, 64, &hdr2, &[]).unwrap();

        // Receiver position past both frames (offset 96).
        recv.advance_receiver_position(96);

        let mut count = 0i32;
        count += sub.poll_fragments(|_, _, _| {}, 100);

        // Should deliver both data frames (skipping the gap at 32).
        assert_eq!(count, 2, "should deliver 2 frames, skipping the gap");
        assert_eq!(sub.position(), 96);
    }

    #[test]
    fn gap_skip_advances_to_term_boundary() {
        // Write one frame at offset 0, then leave the rest of term empty
        // but advance receiver_position past the term boundary.
        let tl: u32 = 128;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();

        // Receiver is in term 1 (past the entire term 0).
        recv.advance_receiver_position(tl as i64 + 32);

        let count = sub.poll_fragments(|_, _, _| {}, 100);
        // One data frame delivered, then gap-skip to term boundary.
        assert_eq!(count, 1);
        assert_eq!(sub.position(), tl as i64, "should advance to term boundary");
    }

    #[test]
    fn gap_skip_multiple_gaps() {
        // Frames at 0, 64, 128 with gaps at 32 and 96.
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();
        // gap at 32
        let hdr2 = make_data_header(DATA_HEADER_LENGTH as i32, 64, 42, 7, 0);
        recv.append_frame(0, 64, &hdr2, &[]).unwrap();
        // gap at 96
        let hdr4 = make_data_header(DATA_HEADER_LENGTH as i32, 128, 42, 7, 0);
        recv.append_frame(0, 128, &hdr4, &[]).unwrap();

        recv.advance_receiver_position(160);

        let count = sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(count, 3, "should deliver 3 frames, skipping 2 gaps");
        assert_eq!(sub.position(), 160);
    }

    #[test]
    fn no_gap_skip_when_receiver_not_ahead() {
        // Write one frame, do NOT advance receiver_position beyond it.
        // The zero at offset 32 should be treated as "end of data", not a gap.
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();
        recv.advance_receiver_position(32);

        let count = sub.poll_fragments(|_, _, _| {}, 100);
        assert_eq!(count, 1);
        assert_eq!(sub.position(), 32, "should stop at receiver position");
    }

    // ---- Pad frame tests ----

    #[test]
    fn pad_frame_skipped_without_delivery() {
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        // Write a data frame at offset 0.
        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();

        // Write a pad frame at offset 32 covering the rest of the term.
        // Manually write a pad frame using the commit protocol.
        let pad_length = (tl - 32) as i32;
        let pad_hdr = DataHeader {
            frame_header: FrameHeader {
                frame_length: 0, // will be committed atomically
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_PAD,
            },
            term_offset: 32,
            session_id: 42,
            stream_id: 7,
            term_id: 0,
            reserved_value: 0,
        };
        // Use append_frame to write the header (it stores frame_length from the
        // header parameter but we need to override frame_type). Write manually:
        unsafe {
            let buf_ptr = recv.inner.log.as_mut_ptr();
            let base = 32usize; // partition 0, offset 32
            std::ptr::copy_nonoverlapping(
                &pad_hdr as *const DataHeader as *const u8,
                buf_ptr.add(base),
                DATA_HEADER_LENGTH,
            );
            atomic_frame_length_store(buf_ptr, base, pad_length);
        }

        recv.advance_receiver_position(tl as i64);

        let mut payloads = Vec::new();
        let count = sub.poll_fragments(
            |data, _, _| {
                payloads.push(data.len());
            },
            100,
        );
        // Only the data frame should be delivered, not the pad.
        assert_eq!(count, 1, "pad frame should not be counted as a fragment");
        // Position should advance past both the data frame and the pad.
        assert_eq!(
            sub.position(),
            tl as i64,
            "position should advance past pad"
        );
    }

    #[test]
    fn pad_frame_between_data_frames() {
        // Data at 0, pad at 32 (64 bytes), data at 96.
        let tl: u32 = 256;
        let (mut recv, mut sub) = new_shared_image(42, 7, 0, 0, tl, 0).unwrap();

        // Data frame at offset 0.
        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        recv.append_frame(0, 0, &hdr0, &[]).unwrap();

        // Pad frame at offset 32, covering 64 bytes (to offset 96).
        unsafe {
            let buf_ptr = recv.inner.log.as_mut_ptr();
            let pad_hdr = DataHeader {
                frame_header: FrameHeader {
                    frame_length: 0,
                    version: CURRENT_VERSION,
                    flags: 0,
                    frame_type: FRAME_TYPE_PAD,
                },
                term_offset: 32,
                session_id: 42,
                stream_id: 7,
                term_id: 0,
                reserved_value: 0,
            };
            std::ptr::copy_nonoverlapping(
                &pad_hdr as *const DataHeader as *const u8,
                buf_ptr.add(32),
                DATA_HEADER_LENGTH,
            );
            atomic_frame_length_store(buf_ptr, 32, 64);
        }

        // Data frame at offset 96.
        let payload = [0xBE, 0xEF];
        let hdr3 = make_data_header(
            DATA_HEADER_LENGTH as i32 + payload.len() as i32,
            96,
            42,
            7,
            0,
        );
        recv.append_frame(0, 96, &hdr3, &payload).unwrap();

        recv.advance_receiver_position(160);

        let mut found = Vec::new();
        let count = sub.poll_fragments(
            |data, _, _| {
                found.push(data.to_vec());
            },
            100,
        );
        // Two data frames delivered (pad skipped).
        assert_eq!(count, 2);
        assert!(found[0].is_empty(), "first frame has no payload");
        assert_eq!(&found[1], &[0xBE, 0xEF], "second frame has payload");
        assert_eq!(sub.position(), 160);
    }
}
