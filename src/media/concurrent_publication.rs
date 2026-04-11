// Concurrent publication - cross-thread offer/scan via Release/Acquire atomics.
//
// Splits the publication into two handles:
//   - ConcurrentPublication (publisher handle, application thread)
//   - SenderPublication (sender-side view, SenderAgent thread)
//
// Both share an Arc<PublicationInner> containing:
//   - SharedLogBuffer (UnsafeCell<Vec<u8>>, allocated once)
//   - pub_position: AtomicI64 (written by publisher, read by sender)
//   - sender_position: AtomicI64 (written by sender, read by publisher)
//   - Immutable config (session_id, stream_id, term_length, etc.)
//
// Commit protocol: publisher writes header+payload, then stores
// frame_length with Release ordering. Scanner loads frame_length with
// Acquire - if > 0, the frame data is visible.
//
// No Mutex. No SeqCst. Zero allocation in steady state.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::frame::{
    CURRENT_VERSION, DATA_FLAG_BEGIN, DATA_FLAG_END, DATA_HEADER_LENGTH, DataHeader,
    FRAME_TYPE_DATA, FRAME_TYPE_PAD, FrameHeader,
};
use crate::media::network_publication::OfferError;
use crate::media::term_buffer::{
    PARTITION_COUNT, SharedLogBuffer, align_frame_length, atomic_frame_length_load,
    atomic_frame_length_store, partition_index,
};

// ---- Shared inner state ----

/// Shared state between publisher and sender, wrapped in Arc.
/// Allocated once at construction (cold path). Never resized.
pub(crate) struct PublicationInner {
    log: SharedLogBuffer,
    /// Highest published byte position. Written by publisher (Release),
    /// read by external observers.
    pub_position: AtomicI64,
    /// Highest scanned byte position. Written by sender (Release),
    /// read by publisher (Acquire) for back-pressure.
    sender_position: AtomicI64,

    // Immutable after construction - no atomics needed.
    initial_term_id: i32,
    term_length: u32,
    term_length_mask: u32,
    position_bits_to_shift: u32,
    session_id: i32,
    stream_id: i32,
    mtu: u32,
    max_payload_length: u32,
}

// ---- ConcurrentPublication (publisher handle) ----

/// Publisher handle for cross-thread offer.
///
/// Moved to the application thread. Takes `&mut self` for `offer()` -
/// enforces single-publisher at compile time (like Aeron ExclusivePublication).
///
/// Implements `Send` (movable to another thread) but NOT `Clone`.
pub struct ConcurrentPublication {
    inner: Arc<PublicationInner>,
    // Publisher-local mutable state (not shared):
    active_term_id: i32,
    term_offset: u32,
    /// Cached copy of pub_position. Single writer - local copy is always
    /// accurate, avoids atomic load on every offer.
    pub_position_local: i64,
}

// ConcurrentPublication is Send (can move to another thread) but not Sync
// (only one thread should call offer at a time, enforced by &mut self).

// ---- SenderPublication (sender-side view) ----

/// Sender-side view for cross-thread scan.
///
/// Owned by the SenderAgent. Takes `&mut self` for `sender_scan()`.
/// Implements `Send`.
pub struct SenderPublication {
    inner: Arc<PublicationInner>,
    /// Cached copy of sender_position. Single writer - local copy is
    /// always accurate, avoids atomic load on every scan.
    sender_position_local: i64,
}

// ---- Construction ----

/// Create a concurrent publication pair (publisher handle + sender view).
///
/// Cold path - allocation happens here, once (SharedLogBuffer + Arc).
///
/// Returns `None` if:
/// - `term_length` is not a power-of-two or < 32 (FRAME_ALIGNMENT)
/// - `mtu` < DATA_HEADER_LENGTH
pub fn new_concurrent(
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    term_length: u32,
    mtu: u32,
) -> Option<(ConcurrentPublication, SenderPublication)> {
    if (mtu as usize) < DATA_HEADER_LENGTH {
        return None;
    }

    let log = SharedLogBuffer::new(term_length)?;

    let term_length_mask = term_length - 1;
    let position_bits_to_shift = term_length.trailing_zeros();
    let max_payload_length = mtu - DATA_HEADER_LENGTH as u32;

    let inner = Arc::new(PublicationInner {
        log,
        pub_position: AtomicI64::new(0),
        sender_position: AtomicI64::new(0),
        initial_term_id,
        term_length,
        term_length_mask,
        position_bits_to_shift,
        session_id,
        stream_id,
        mtu,
        max_payload_length,
    });

    let publisher = ConcurrentPublication {
        inner: Arc::clone(&inner),
        active_term_id: initial_term_id,
        term_offset: 0,
        pub_position_local: 0,
    };

    let sender = SenderPublication {
        inner,
        sender_position_local: 0,
    };

    Some((publisher, sender))
}

// ---- ConcurrentPublication impl ----

impl ConcurrentPublication {
    /// Offer a payload to be published as a data frame.
    ///
    /// Builds a DataHeader, writes header bytes [4..32] and payload into the
    /// term buffer, then commits with an atomic Release store of frame_length
    /// at offset [0..4]. Returns the new pub_position on success.
    ///
    /// Zero-allocation, O(1). Hot path.
    ///
    /// Errors:
    /// - `PayloadTooLarge` - payload exceeds mtu - DATA_HEADER_LENGTH
    /// - `BackPressured` - sender too far behind (would overwrite unscanned data)
    /// - `AdminAction` - term rotated, caller should retry immediately
    pub fn offer(&mut self, payload: &[u8]) -> Result<i64, OfferError> {
        let inner = &*self.inner;

        // Guard: payload size.
        if payload.len() > inner.max_payload_length as usize {
            return Err(OfferError::PayloadTooLarge);
        }

        // Guard: back-pressure. Publisher must not lap the sender by more than
        // (PARTITION_COUNT - 1) terms.
        let max_ahead = inner.term_length as i64 * (PARTITION_COUNT as i64 - 1);
        let sender_pos = inner.sender_position.load(Ordering::Acquire);
        if self.pub_position_local.wrapping_sub(sender_pos) >= max_ahead {
            return Err(OfferError::BackPressured);
        }

        let part_idx = partition_index(self.active_term_id, inner.initial_term_id);

        let frame_length = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let raw_len = DATA_HEADER_LENGTH + payload.len();
        let aligned_len = align_frame_length(raw_len);
        let new_offset = self.term_offset + aligned_len as u32;

        // Check term capacity.
        if new_offset > inner.term_length {
            // Write a pad frame at the unused tail so sender_scan can
            // advance past it instead of getting stuck on zeros.
            let remaining = inner.term_length - self.term_offset;
            if remaining as usize >= DATA_HEADER_LENGTH {
                let tl = inner.term_length as usize;
                let pad_base = part_idx * tl + self.term_offset as usize;
                let pad_hdr = DataHeader {
                    frame_header: FrameHeader {
                        frame_length: remaining as i32,
                        version: CURRENT_VERSION,
                        flags: 0,
                        frame_type: FRAME_TYPE_PAD,
                    },
                    term_offset: self.term_offset as i32,
                    session_id: 0,
                    stream_id: 0,
                    term_id: 0,
                    reserved_value: 0,
                };
                unsafe {
                    let buf_ptr = inner.log.as_mut_ptr();
                    // Write pad header bytes [4..32] first.
                    std::ptr::copy_nonoverlapping(
                        (&pad_hdr as *const DataHeader as *const u8).add(4),
                        buf_ptr.add(pad_base + 4),
                        DATA_HEADER_LENGTH - 4,
                    );
                    // Commit: atomic Release store of frame_length at [0..4].
                    atomic_frame_length_store(buf_ptr, pad_base, remaining as i32);
                }
            }
            self.rotate_term();
            return Err(OfferError::AdminAction);
        }

        let tl = inner.term_length as usize;
        let base = part_idx * tl + self.term_offset as usize;

        // Build the DataHeader on the stack.
        let hdr = DataHeader {
            frame_header: FrameHeader {
                frame_length: 0, // placeholder - committed atomically below
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: self.term_offset as i32,
            session_id: inner.session_id,
            stream_id: inner.stream_id,
            term_id: self.active_term_id,
            reserved_value: 0,
        };

        // SAFETY: We have exclusive write access to this frame slot.
        // Back-pressure ensures no reader is in this partition region.
        // The buffer is allocated and never resized.
        unsafe {
            let buf_ptr = inner.log.as_mut_ptr();

            // Step 1: Write header bytes [0..32] with frame_length = 0.
            // The zero frame_length means "not yet committed" to any scanner.
            std::ptr::copy_nonoverlapping(
                &hdr as *const DataHeader as *const u8,
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
            // All preceding writes (header + payload) become visible to any
            // thread that reads this word with Acquire.
            atomic_frame_length_store(buf_ptr, base, frame_length);
        }

        // Step 4: Advance local cursor.
        self.term_offset = new_offset;
        self.pub_position_local = self.compute_position(self.active_term_id, new_offset);
        self.inner
            .pub_position
            .store(self.pub_position_local, Ordering::Release);

        Ok(self.pub_position_local)
    }

    /// Rotate to the next term partition.
    fn rotate_term(&mut self) {
        self.active_term_id = self.active_term_id.wrapping_add(1);
        self.term_offset = 0;

        let new_part_idx = partition_index(self.active_term_id, self.inner.initial_term_id);
        // SAFETY: Back-pressure guarantees the sender has finished scanning
        // this partition (4 partitions, publisher max 3 terms ahead).
        unsafe {
            self.inner.log.clean_partition(new_part_idx);
        }
    }

    /// Compute an absolute i64 position from a term_id and term_offset.
    #[inline]
    fn compute_position(&self, term_id: i32, term_offset: u32) -> i64 {
        let term_count = term_id.wrapping_sub(self.inner.initial_term_id) as i64;
        term_count * self.inner.term_length as i64 + term_offset as i64
    }

    // ---- Accessors ----

    /// Highest published byte position (local cached copy).
    #[inline]
    pub fn pub_position(&self) -> i64 {
        self.pub_position_local
    }

    /// Current active term ID (write cursor).
    #[inline]
    pub fn active_term_id(&self) -> i32 {
        self.active_term_id
    }

    /// Current write offset within the active term.
    #[inline]
    pub fn term_offset(&self) -> u32 {
        self.term_offset
    }

    /// Session ID for this publication.
    #[inline]
    pub fn session_id(&self) -> i32 {
        self.inner.session_id
    }

    /// Stream ID for this publication.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.inner.stream_id
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

    /// Maximum transmission unit.
    #[inline]
    pub fn mtu(&self) -> u32 {
        self.inner.mtu
    }
}

// ---- SenderPublication impl ----

impl SenderPublication {
    /// Scan committed frames starting from sender_position.
    ///
    /// Walks forward through frames in the current sender term, reading
    /// frame_length with Acquire ordering to detect committed frames.
    /// Calls `emit(term_offset, frame_slice)` for each valid frame.
    ///
    /// Advances sender_position (Release store) so the publisher can see
    /// the freed space for back-pressure.
    ///
    /// Returns total bytes scanned.
    ///
    /// Zero-allocation, O(n) in frames scanned. Hot path.
    pub fn sender_scan<F>(&mut self, limit: u32, mut emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let inner = &*self.inner;

        let term_id = inner
            .initial_term_id
            .wrapping_add((self.sender_position_local >> inner.position_bits_to_shift) as i32);
        let term_offset = (self.sender_position_local & inner.term_length_mask as i64) as u32;

        let part_idx = partition_index(term_id, inner.initial_term_id);

        // Clamp limit to remaining bytes in the current term.
        let remaining_in_term = inner.term_length - term_offset;
        let effective_limit = if limit < remaining_in_term {
            limit
        } else {
            remaining_in_term
        };

        let tl = inner.term_length as usize;
        let part_base = part_idx * tl;
        let buf_ptr = inner.log.as_ptr();

        let mut pos = term_offset;
        let mut scanned: u32 = 0;

        while pos < inner.term_length && scanned < effective_limit {
            let remaining = (inner.term_length - pos) as usize;
            if remaining < 4 {
                break;
            }

            let byte_offset = part_base + pos as usize;

            // Acquire load of frame_length - pairs with publisher's Release store.
            let frame_length = unsafe { atomic_frame_length_load(buf_ptr, byte_offset) };

            // Zero or negative means uncommitted / not yet written.
            if frame_length <= 0 {
                break;
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

            // SAFETY: Acquire load of frame_length guarantees all header + payload
            // bytes written by the publisher (before their Release store) are visible.
            let frame_slice = unsafe {
                std::slice::from_raw_parts(buf_ptr.add(byte_offset), frame_len_u as usize)
            };

            emit(pos, frame_slice);

            pos += aligned_len;
            scanned += aligned_len;
        }

        // Release store of sender_position so publisher sees freed space.
        if scanned > 0 {
            self.sender_position_local += scanned as i64;
            inner
                .sender_position
                .store(self.sender_position_local, Ordering::Release);
        }

        scanned
    }

    // ---- Accessors ----

    /// Highest scanned byte position (local cached copy).
    #[inline]
    pub fn sender_position(&self) -> i64 {
        self.sender_position_local
    }

    /// Session ID for this publication.
    #[inline]
    pub fn session_id(&self) -> i32 {
        self.inner.session_id
    }

    /// Stream ID for this publication.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.inner.stream_id
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

    /// Maximum transmission unit.
    #[inline]
    pub fn mtu(&self) -> u32 {
        self.inner.mtu
    }

    /// Current active term ID (read from atomic pub_position to derive).
    /// This is approximate - the publisher may have rotated since last check.
    #[inline]
    pub fn active_term_id_approx(&self) -> i32 {
        let pub_pos = self.inner.pub_position.load(Ordering::Relaxed);
        self.inner
            .initial_term_id
            .wrapping_add((pub_pos >> self.inner.position_bits_to_shift) as i32)
    }

    /// Current term offset (read from atomic pub_position to derive).
    /// This is approximate.
    #[inline]
    pub fn term_offset_approx(&self) -> u32 {
        let pub_pos = self.inner.pub_position.load(Ordering::Relaxed);
        (pub_pos & self.inner.term_length_mask as i64) as u32
    }

    /// Read committed frames from a specific term position.
    /// For retransmit use - does not advance sender_position.
    ///
    /// Uses Acquire-ordered atomic loads to detect committed frames
    /// in the SharedLogBuffer (same protocol as sender_scan).
    ///
    /// Returns total bytes of frames emitted.
    pub fn scan_term_at<F>(&self, partition_idx: usize, offset: u32, limit: u32, mut emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let inner = &*self.inner;

        if partition_idx >= PARTITION_COUNT {
            return 0;
        }

        let tl = inner.term_length as usize;
        let part_base = partition_idx * tl;
        let buf_ptr = inner.log.as_ptr();

        let mut pos = offset;
        let mut scanned: u32 = 0;

        while pos < inner.term_length && scanned < limit {
            let remaining = (inner.term_length - pos) as usize;
            if remaining < 4 {
                break;
            }

            let byte_offset = part_base + pos as usize;

            // Acquire load of frame_length - pairs with publisher's Release store.
            let frame_length = unsafe { atomic_frame_length_load(buf_ptr, byte_offset) };

            // Zero or negative means uncommitted / not yet written.
            if frame_length <= 0 {
                break;
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

            // SAFETY: Acquire load of frame_length guarantees all header + payload
            // bytes written by the publisher (before their Release store) are visible.
            let frame_slice = unsafe {
                std::slice::from_raw_parts(buf_ptr.add(byte_offset), frame_len_u as usize)
            };

            emit(pos, frame_slice);

            pos += aligned_len;
            scanned += aligned_len;
        }

        scanned
    }

    /// Compute an absolute i64 position from a term_id and term_offset.
    #[inline]
    pub fn compute_position(&self, term_id: i32, term_offset: u32) -> i64 {
        let term_count = term_id.wrapping_sub(self.inner.initial_term_id) as i64;
        term_count * self.inner.term_length as i64 + term_offset as i64
    }

    /// Publisher's highest published byte position (Acquire load).
    #[inline]
    pub fn pub_position(&self) -> i64 {
        self.inner.pub_position.load(Ordering::Acquire)
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::DataHeader;
    use crate::media::term_buffer::{FRAME_ALIGNMENT, partition_index};

    const TEST_TERM_LENGTH: u32 = 1024;
    const TEST_MTU: u32 = 1408;

    fn make_pair() -> (ConcurrentPublication, SenderPublication) {
        new_concurrent(42, 7, 0, TEST_TERM_LENGTH, TEST_MTU).expect("valid params")
    }

    fn make_pair_with(
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> (ConcurrentPublication, SenderPublication) {
        new_concurrent(session_id, stream_id, initial_term_id, term_length, mtu)
            .expect("valid params")
    }

    // ---- Constructor ----

    #[test]
    fn new_valid_params() {
        let result = new_concurrent(1, 1, 0, TEST_TERM_LENGTH, TEST_MTU);
        assert!(result.is_some());
        let (pub_h, send_h) = result.unwrap();
        assert_eq!(pub_h.session_id(), 1);
        assert_eq!(pub_h.stream_id(), 1);
        assert_eq!(pub_h.initial_term_id(), 0);
        assert_eq!(pub_h.active_term_id(), 0);
        assert_eq!(pub_h.term_offset(), 0);
        assert_eq!(pub_h.term_length(), TEST_TERM_LENGTH);
        assert_eq!(pub_h.mtu(), TEST_MTU);
        assert_eq!(pub_h.pub_position(), 0);
        assert_eq!(send_h.sender_position(), 0);
        assert_eq!(send_h.session_id(), 1);
    }

    #[test]
    fn new_invalid_term_length() {
        assert!(new_concurrent(1, 1, 0, 100, TEST_MTU).is_none());
        assert!(new_concurrent(1, 1, 0, 0, TEST_MTU).is_none());
        assert!(new_concurrent(1, 1, 0, 16, TEST_MTU).is_none());
    }

    #[test]
    fn new_invalid_mtu() {
        assert!(new_concurrent(1, 1, 0, TEST_TERM_LENGTH, 0).is_none());
        assert!(new_concurrent(1, 1, 0, TEST_TERM_LENGTH, 31).is_none());
        // Exactly DATA_HEADER_LENGTH is valid (zero payload).
        assert!(new_concurrent(1, 1, 0, TEST_TERM_LENGTH, DATA_HEADER_LENGTH as u32).is_some());
    }

    // ---- offer: happy path ----

    #[test]
    fn offer_single_frame_empty_payload() {
        let (mut pub_h, _send_h) = make_pair();
        let pos = pub_h.offer(&[]).unwrap();
        assert_eq!(pos, FRAME_ALIGNMENT as i64);
        assert_eq!(pub_h.pub_position(), FRAME_ALIGNMENT as i64);
        assert_eq!(pub_h.term_offset(), FRAME_ALIGNMENT as u32);
    }

    #[test]
    fn offer_single_frame_with_payload() {
        let (mut pub_h, _send_h) = make_pair();
        let payload = [0xCA, 0xFE, 0xBA, 0xBE];
        let pos = pub_h.offer(&payload).unwrap();
        // 32 (header) + 4 (payload) = 36, aligned to 64.
        assert_eq!(pos, 64);
        assert_eq!(pub_h.term_offset(), 64);
    }

    #[test]
    fn offer_advances_position_monotonically() {
        let (mut pub_h, _send_h) = make_pair();
        let mut prev = 0i64;
        for _ in 0..5 {
            let pos = pub_h.offer(&[]).unwrap();
            assert!(pos > prev, "position must increase: {pos} > {prev}");
            prev = pos;
        }
    }

    // ---- offer: errors ----

    #[test]
    fn offer_payload_too_large() {
        let (mut pub_h, _send_h) = make_pair_with(1, 1, 0, TEST_TERM_LENGTH, 64);
        let big_payload = [0u8; 33];
        assert_eq!(pub_h.offer(&big_payload), Err(OfferError::PayloadTooLarge));
        // Exactly at limit should succeed.
        let ok_payload = [0u8; 32];
        assert!(pub_h.offer(&ok_payload).is_ok());
    }

    #[test]
    fn offer_back_pressured() {
        // Small term: 64 bytes. Max ahead = 3 * 64 = 192.
        let (mut pub_h, mut send_h) = make_pair_with(1, 1, 0, 64, TEST_MTU);

        // Fill 3 terms without scanning.
        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));

        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));

        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());

        // Next offer should be back-pressured (192 - 0 >= 192).
        assert_eq!(pub_h.offer(&[]), Err(OfferError::BackPressured));

        // After scanning, back-pressure clears.
        send_h.sender_scan(u32::MAX, |_, _| {});
        // Term rotation, then write into next term.
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
        assert!(pub_h.offer(&[]).is_ok());
    }

    // ---- offer: term rotation ----

    #[test]
    fn offer_term_rotation_returns_admin_action() {
        let (mut pub_h, _send_h) = make_pair_with(1, 1, 0, 64, TEST_MTU);
        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
    }

    #[test]
    fn offer_after_admin_action_succeeds() {
        let (mut pub_h, _send_h) = make_pair_with(1, 1, 0, 64, TEST_MTU);
        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
        assert!(pub_h.offer(&[]).is_ok());
    }

    #[test]
    fn offer_term_id_increments_on_rotation() {
        let (mut pub_h, _send_h) = make_pair_with(1, 1, 10, 64, TEST_MTU);
        assert_eq!(pub_h.active_term_id(), 10);

        assert!(pub_h.offer(&[]).is_ok());
        assert!(pub_h.offer(&[]).is_ok());
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));

        assert_eq!(pub_h.active_term_id(), 11);
        assert_eq!(pub_h.term_offset(), 0);
    }

    #[test]
    fn offer_multiple_rotations_through_all_partitions() {
        let (mut pub_h, mut send_h) = make_pair_with(1, 1, 0, 32, TEST_MTU);
        for expected_term in 0..8i32 {
            assert_eq!(pub_h.active_term_id(), expected_term);
            assert!(pub_h.offer(&[]).is_ok());
            send_h.sender_scan(u32::MAX, |_, _| {});
            if expected_term < 7 {
                assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
            }
        }
        assert_eq!(pub_h.active_term_id(), 7);
    }

    #[test]
    fn offer_wrapping_term_id_near_max() {
        let (mut pub_h, mut send_h) = make_pair_with(1, 1, i32::MAX - 1, 32, TEST_MTU);
        assert_eq!(pub_h.active_term_id(), i32::MAX - 1);

        // Fill and rotate: MAX-1 -> MAX.
        assert!(pub_h.offer(&[]).is_ok());
        send_h.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
        assert_eq!(pub_h.active_term_id(), i32::MAX);

        // Fill and rotate: MAX -> MIN (wraps).
        assert!(pub_h.offer(&[]).is_ok());
        send_h.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
        assert_eq!(pub_h.active_term_id(), i32::MIN);

        // Still functional after wrap.
        assert!(pub_h.offer(&[]).is_ok());
    }

    // ---- sender_scan ----

    #[test]
    fn sender_scan_empty_publication() {
        let (_pub_h, mut send_h) = make_pair();
        let mut count = 0u32;
        let scanned = send_h.sender_scan(TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(scanned, 0);
        assert_eq!(count, 0);
        assert_eq!(send_h.sender_position(), 0);
    }

    #[test]
    fn sender_scan_single_frame() {
        let (mut pub_h, mut send_h) = make_pair();
        pub_h.offer(&[]).unwrap();

        let mut count = 0u32;
        let scanned = send_h.sender_scan(TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(count, 1);
        assert_eq!(scanned, FRAME_ALIGNMENT as u32);
        assert_eq!(send_h.sender_position(), FRAME_ALIGNMENT as i64);
    }

    #[test]
    fn sender_scan_advances_sender_position() {
        let (mut pub_h, mut send_h) = make_pair();
        pub_h.offer(&[]).unwrap();
        pub_h.offer(&[]).unwrap();

        let scanned = send_h.sender_scan(TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(scanned, 64);
        assert_eq!(send_h.sender_position(), 64);
    }

    #[test]
    fn sender_scan_limit_respected() {
        let (mut pub_h, mut send_h) = make_pair();
        for _ in 0..4 {
            pub_h.offer(&[]).unwrap();
        }

        let mut count = 0u32;
        let scanned = send_h.sender_scan(64, |_, _| count += 1);
        assert_eq!(count, 2);
        assert_eq!(scanned, 64);
        assert_eq!(send_h.sender_position(), 64);
    }

    #[test]
    fn sender_scan_catches_up_to_pub_position() {
        let (mut pub_h, mut send_h) = make_pair();
        for _ in 0..10 {
            pub_h.offer(&[]).unwrap();
        }
        let pub_pos = pub_h.pub_position();

        let mut total = 0u32;
        loop {
            let scanned = send_h.sender_scan(TEST_TERM_LENGTH, |_, _| {});
            if scanned == 0 {
                break;
            }
            total += scanned;
        }
        assert_eq!(send_h.sender_position(), pub_pos);
        assert_eq!(total as i64, pub_pos);
    }

    #[test]
    fn sender_scan_across_term_boundary() {
        let (mut pub_h, mut send_h) = make_pair_with(1, 1, 0, 64, TEST_MTU);

        // Fill term 0.
        pub_h.offer(&[]).unwrap();
        pub_h.offer(&[]).unwrap();

        // Scan term 0 fully.
        let scanned0 = send_h.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(scanned0, 64);
        assert_eq!(send_h.sender_position(), 64);

        // Rotate to term 1.
        assert_eq!(pub_h.offer(&[]), Err(OfferError::AdminAction));
        pub_h.offer(&[]).unwrap();

        // Scan term 1.
        let mut count = 0u32;
        let scanned1 = send_h.sender_scan(u32::MAX, |_, _| count += 1);
        assert_eq!(count, 1);
        assert_eq!(scanned1, 32);
        assert_eq!(send_h.sender_position(), 96);
    }

    #[test]
    fn sender_scan_emits_correct_frame_data() {
        let (mut pub_h, mut send_h) = make_pair();
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        pub_h.offer(&payload).unwrap();

        let mut found = false;
        send_h.sender_scan(TEST_TERM_LENGTH, |off, data| {
            assert_eq!(off, 0);
            let parsed = DataHeader::parse(data);
            assert!(parsed.is_some());
            let hdr = parsed.unwrap();
            assert_eq!({ hdr.session_id }, 42);
            assert_eq!({ hdr.stream_id }, 7);
            assert_eq!({ hdr.term_id }, 0);
            let pl = hdr.payload(data);
            assert_eq!(pl, &[0xDE, 0xAD, 0xBE, 0xEF]);
            found = true;
        });
        assert!(found);
    }

    // ---- Cross-thread safety (compile-time checks) ----

    #[test]
    fn concurrent_publication_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ConcurrentPublication>();
    }

    #[test]
    fn sender_publication_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SenderPublication>();
    }

    // ---- Back-pressure clears after scan ----

    #[test]
    fn back_pressure_clears_after_sender_scan() {
        let (mut pub_h, mut send_h) = make_pair_with(1, 1, 0, 64, TEST_MTU);

        // Fill until back-pressured.
        loop {
            match pub_h.offer(&[]) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {}
                Err(OfferError::BackPressured) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        // Scan to free space.
        let mut total_scanned = 0u32;
        loop {
            let s = send_h.sender_scan(u32::MAX, |_, _| {});
            if s == 0 {
                break;
            }
            total_scanned += s;
        }
        assert!(total_scanned > 0);

        // Should be able to offer again (may get AdminAction first, then Ok).
        let mut success = false;
        for _ in 0..4 {
            match pub_h.offer(&[]) {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(OfferError::AdminAction) => {}
                Err(e) => panic!("unexpected error after scan: {e}"),
            }
        }
        assert!(
            success,
            "should be able to offer after scanning frees space"
        );
    }

    // ---- Accessor coverage ----

    #[test]
    fn accessors_return_construction_values() {
        let (pub_h, send_h) = make_pair_with(42, 7, 100, 1024, 1408);
        assert_eq!(pub_h.session_id(), 42);
        assert_eq!(pub_h.stream_id(), 7);
        assert_eq!(pub_h.initial_term_id(), 100);
        assert_eq!(pub_h.active_term_id(), 100);
        assert_eq!(pub_h.term_length(), 1024);
        assert_eq!(pub_h.mtu(), 1408);
        assert_eq!(pub_h.term_offset(), 0);
        assert_eq!(pub_h.pub_position(), 0);
        assert_eq!(send_h.sender_position(), 0);
        assert_eq!(send_h.session_id(), 42);
        assert_eq!(send_h.stream_id(), 7);
        assert_eq!(send_h.initial_term_id(), 100);
        assert_eq!(send_h.term_length(), 1024);
        assert_eq!(send_h.mtu(), 1408);
    }

    // ---- scan_term_at ----

    #[test]
    fn scan_term_at_reads_committed() {
        let (mut pub_h, send_h) = make_pair();
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        pub_h.offer(&payload).unwrap();

        let part_idx = partition_index(0, 0);
        let mut found = false;
        let scanned = send_h.scan_term_at(part_idx, 0, TEST_TERM_LENGTH, |off, data| {
            assert_eq!(off, 0);
            let parsed = DataHeader::parse(data);
            assert!(parsed.is_some());
            let hdr = parsed.unwrap();
            assert_eq!({ hdr.session_id }, 42);
            assert_eq!({ hdr.stream_id }, 7);
            let pl = hdr.payload(data);
            assert_eq!(pl, &[0xDE, 0xAD, 0xBE, 0xEF]);
            found = true;
        });

        assert!(found);
        assert!(scanned > 0);
    }

    #[test]
    fn scan_term_at_does_not_advance_position() {
        let (mut pub_h, send_h) = make_pair();
        pub_h.offer(&[]).unwrap();

        let pos_before = send_h.sender_position();
        send_h.scan_term_at(0, 0, TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(send_h.sender_position(), pos_before);
    }

    #[test]
    fn scan_term_at_invalid_partition_returns_zero() {
        let (_pub_h, send_h) = make_pair();
        let scanned = send_h.scan_term_at(99, 0, TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(scanned, 0);
    }

    #[test]
    fn scan_term_at_empty_returns_zero() {
        let (_pub_h, send_h) = make_pair();
        let scanned = send_h.scan_term_at(0, 0, TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(scanned, 0);
    }

    #[test]
    fn sender_pub_compute_position() {
        let (_pub_h, send_h) = make_pair();
        assert_eq!(send_h.compute_position(0, 0), 0);
        assert_eq!(send_h.compute_position(0, 512), 512);
        assert_eq!(send_h.compute_position(1, 0), TEST_TERM_LENGTH as i64);
    }

    #[test]
    fn sender_pub_position_reflects_offers() {
        let (mut pub_h, send_h) = make_pair();
        assert_eq!(send_h.pub_position(), 0);
        pub_h.offer(&[]).unwrap();
        assert_eq!(send_h.pub_position(), FRAME_ALIGNMENT as i64);
    }
}
