// Network publication - manages a single publication's term buffer, position
// tracking, and offer/scan API.
//
// Owns a RawLog (4-partition term buffer) and tracks:
//   - active_term_id / term_offset (write cursor)
//   - pub_position (highest published byte)
//   - sender_position (highest scanned byte)
//
// All hot-path methods (offer, sender_scan) are zero-allocation.
// All index arithmetic uses bitmask (& mask) instead of modulo (%).
// All term_id arithmetic uses wrapping_add / wrapping_sub.

use std::fmt;

use crate::frame::{
    DataHeader, FrameHeader, CURRENT_VERSION, DATA_FLAG_BEGIN, DATA_FLAG_END,
    DATA_HEADER_LENGTH, FRAME_TYPE_DATA,
};
use crate::media::term_buffer::{AppendError, RawLog, PARTITION_COUNT};

// ---- Error type ----

/// Stack-only error for offer operations.
/// Matches the `PollError` / `AppendError` pattern - never heap-allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferError {
    /// Sender has not caught up - publisher is too far ahead.
    /// Caller should retry later after sender drains.
    BackPressured,
    /// Term rotation occurred - caller should retry immediately.
    /// This lets the SenderAgent poll control messages between retries.
    AdminAction,
    /// Payload exceeds mtu - DATA_HEADER_LENGTH. Caller bug.
    PayloadTooLarge,
    /// Internal invariant violation (should never happen).
    InvalidState,
}

impl fmt::Display for OfferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OfferError::BackPressured => f.write_str("back pressured"),
            OfferError::AdminAction => f.write_str("admin action"),
            OfferError::PayloadTooLarge => f.write_str("payload too large"),
            OfferError::InvalidState => f.write_str("invalid state"),
        }
    }
}

// ---- NetworkPublication ----

/// A single network publication backed by a 4-partition term buffer.
///
/// Provides `offer(payload)` to write data frames and `sender_scan(limit, emit)`
/// to walk committed frames for transmission. Single-threaded - owned by the
/// SenderAgent.
///
/// Memory layout (owned by RawLog):
/// ```text
/// [partition 0][partition 1][partition 2][partition 3]
///  term_length   term_length  term_length  term_length
/// ```
///
/// Allocated once at construction. Never resized.
pub struct NetworkPublication {
    raw_log: RawLog,
    initial_term_id: i32,
    active_term_id: i32,
    term_offset: u32,
    term_length: u32,
    /// term_length - 1. For bitmask offset extraction from position.
    term_length_mask: u32,
    /// term_length.trailing_zeros(). For position >> shift to get term count.
    position_bits_to_shift: u32,
    /// Highest published byte position (advanced by offer).
    pub_position: i64,
    /// Highest scanned byte position (advanced by sender_scan).
    sender_position: i64,
    session_id: i32,
    stream_id: i32,
    mtu: u32,
    /// mtu - DATA_HEADER_LENGTH. Cached at construction.
    max_payload_length: u32,
}

impl NetworkPublication {
    /// Create a new NetworkPublication.
    ///
    /// Cold path - allocation happens here, once.
    ///
    /// Returns `None` if:
    /// - `term_length` is not a power-of-two or < 32 (FRAME_ALIGNMENT)
    /// - `mtu` < DATA_HEADER_LENGTH
    pub fn new(
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> Option<Self> {
        if (mtu as usize) < DATA_HEADER_LENGTH {
            return None;
        }

        let raw_log = RawLog::new(term_length)?;

        let term_length_mask = term_length - 1;
        let position_bits_to_shift = term_length.trailing_zeros();
        let max_payload_length = mtu - DATA_HEADER_LENGTH as u32;

        Some(Self {
            raw_log,
            initial_term_id,
            active_term_id: initial_term_id,
            term_offset: 0,
            term_length,
            term_length_mask,
            position_bits_to_shift,
            pub_position: 0,
            sender_position: 0,
            session_id,
            stream_id,
            mtu,
            max_payload_length,
        })
    }

    /// Offer a payload to be published as a data frame.
    ///
    /// Builds a DataHeader on the stack, appends header + payload to the
    /// active term partition. Returns the new pub_position on success.
    ///
    /// Zero-allocation, O(1). Hot path.
    ///
    /// Errors:
    /// - `PayloadTooLarge` - payload exceeds mtu - DATA_HEADER_LENGTH
    /// - `BackPressured` - sender too far behind (would overwrite unscanned data)
    /// - `AdminAction` - term rotated, caller should retry immediately
    /// - `InvalidState` - internal bug (should never happen)
    pub fn offer(&mut self, payload: &[u8]) -> Result<i64, OfferError> {
        // Guard: payload size.
        if payload.len() > self.max_payload_length as usize {
            return Err(OfferError::PayloadTooLarge);
        }

        // Guard: back-pressure. Publisher must not lap the sender by more than
        // (PARTITION_COUNT - 1) terms - that would overwrite unscanned data.
        let max_ahead = self.term_length as i64 * (PARTITION_COUNT as i64 - 1);
        if self.pub_position.wrapping_sub(self.sender_position) >= max_ahead {
            return Err(OfferError::BackPressured);
        }

        let partition_idx =
            RawLog::partition_index(self.active_term_id, self.initial_term_id);

        let frame_length = DATA_HEADER_LENGTH as i32 + payload.len() as i32;

        let hdr = DataHeader {
            frame_header: FrameHeader {
                frame_length,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: self.term_offset as i32,
            session_id: self.session_id,
            stream_id: self.stream_id,
            term_id: self.active_term_id,
            reserved_value: 0,
        };

        match self.raw_log.append_frame(partition_idx, self.term_offset, &hdr, payload) {
            Ok(new_offset) => {
                self.term_offset = new_offset;
                self.pub_position =
                    self.compute_position(self.active_term_id, new_offset);
                Ok(self.pub_position)
            }
            Err(AppendError::TermFull) => {
                // Write a pad frame at the unused tail so sender_scan can
                // advance past it instead of getting stuck on zeros.
                self.raw_log.write_pad_frame(partition_idx, self.term_offset);
                self.rotate_term();
                Err(OfferError::AdminAction)
            }
            Err(AppendError::InvalidPartition) => Err(OfferError::InvalidState),
        }
    }

    /// Scan committed frames starting from sender_position.
    ///
    /// Walks forward through frames in the current sender term, calling
    /// `emit(term_offset, frame_slice)` for each valid frame. Stops at the
    /// scan limit, end of committed data, or end of the current term partition
    /// (does not cross term boundaries - caller invokes repeatedly).
    ///
    /// Advances sender_position by the number of bytes scanned.
    /// Returns total bytes scanned.
    ///
    /// Zero-allocation, O(n) in frames scanned. Hot path.
    pub fn sender_scan<F>(&mut self, limit: u32, emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let term_id = self.initial_term_id.wrapping_add(
            (self.sender_position >> self.position_bits_to_shift) as i32,
        );
        let term_offset =
            (self.sender_position & self.term_length_mask as i64) as u32;

        let partition_idx = RawLog::partition_index(term_id, self.initial_term_id);

        // Clamp limit to remaining bytes in the current term.
        let remaining_in_term = self.term_length - term_offset;
        let effective_limit = if limit < remaining_in_term {
            limit
        } else {
            remaining_in_term
        };

        let scanned =
            self.raw_log
                .scan_frames(partition_idx, term_offset, effective_limit, emit);

        self.sender_position += scanned as i64;

        scanned
    }

    // ---- Term rotation (called on TermFull) ----

    /// Rotate to the next term partition.
    ///
    /// Increments active_term_id (wrapping), resets term_offset, and cleans
    /// the partition we are rotating into.
    ///
    /// Cleaning the entering partition is always safe:
    /// - First cycle: partition is already zero from construction (no-op).
    /// - Subsequent cycles: back-pressure (pub - sender < 3 * term_length)
    ///   guarantees the sender has fully scanned this partition's previous data
    ///   before the publisher wraps around to reuse it.
    fn rotate_term(&mut self) {
        self.active_term_id = self.active_term_id.wrapping_add(1);
        self.term_offset = 0;

        // Clean the partition we are entering. With 4 partitions this
        // partition was last used 4 terms ago. Back-pressure ensures the
        // sender is at most 3 terms behind, so it has finished scanning
        // the previous contents.
        let new_partition_idx =
            RawLog::partition_index(self.active_term_id, self.initial_term_id);
        self.raw_log.clean_partition(new_partition_idx);
    }

    // ---- Position helpers ----

    /// Compute an absolute i64 position from a term_id and term_offset.
    ///
    /// Uses wrapping subtraction for term_id difference (handles i32 wrap).
    /// term_length is power-of-two so multiplication by term_count is exact.
    #[inline]
    pub fn compute_position(&self, term_id: i32, term_offset: u32) -> i64 {
        let term_count = term_id.wrapping_sub(self.initial_term_id) as i64;
        term_count * self.term_length as i64 + term_offset as i64
    }

    // ---- Accessors ----

    /// Highest published byte position.
    #[inline]
    pub fn pub_position(&self) -> i64 {
        self.pub_position
    }

    /// Highest scanned byte position (advanced by sender_scan).
    #[inline]
    pub fn sender_position(&self) -> i64 {
        self.sender_position
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
        self.session_id
    }

    /// Stream ID for this publication.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.stream_id
    }

    /// Initial term ID (set at construction, never changes).
    #[inline]
    pub fn initial_term_id(&self) -> i32 {
        self.initial_term_id
    }

    /// Term length in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.term_length
    }

    /// Maximum transmission unit.
    #[inline]
    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    /// Read-only access to the underlying RawLog (for retransmit reads).
    #[inline]
    pub fn raw_log(&self) -> &RawLog {
        &self.raw_log
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::DataHeader;
    use crate::media::term_buffer::FRAME_ALIGNMENT;

    const TEST_TERM_LENGTH: u32 = 1024;
    const TEST_MTU: u32 = 1408;

    fn make_pub() -> NetworkPublication {
        NetworkPublication::new(42, 7, 0, TEST_TERM_LENGTH, TEST_MTU)
            .expect("valid params")
    }

    fn make_pub_with(
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> NetworkPublication {
        NetworkPublication::new(session_id, stream_id, initial_term_id, term_length, mtu)
            .expect("valid params")
    }

    // ---- Constructor ----

    #[test]
    fn new_valid_params() {
        let p = NetworkPublication::new(1, 1, 0, TEST_TERM_LENGTH, TEST_MTU);
        assert!(p.is_some());
        let p = p.unwrap();
        assert_eq!(p.session_id(), 1);
        assert_eq!(p.stream_id(), 1);
        assert_eq!(p.initial_term_id(), 0);
        assert_eq!(p.active_term_id(), 0);
        assert_eq!(p.term_offset(), 0);
        assert_eq!(p.term_length(), TEST_TERM_LENGTH);
        assert_eq!(p.mtu(), TEST_MTU);
        assert_eq!(p.pub_position(), 0);
        assert_eq!(p.sender_position(), 0);
    }

    #[test]
    fn new_invalid_term_length_not_power_of_two() {
        assert!(NetworkPublication::new(1, 1, 0, 100, TEST_MTU).is_none());
        assert!(NetworkPublication::new(1, 1, 0, 48, TEST_MTU).is_none());
        assert!(NetworkPublication::new(1, 1, 0, 1000, TEST_MTU).is_none());
    }

    #[test]
    fn new_invalid_term_length_zero() {
        assert!(NetworkPublication::new(1, 1, 0, 0, TEST_MTU).is_none());
    }

    #[test]
    fn new_invalid_term_length_too_small() {
        // 16 < FRAME_ALIGNMENT (32)
        assert!(NetworkPublication::new(1, 1, 0, 16, TEST_MTU).is_none());
        assert!(NetworkPublication::new(1, 1, 0, 1, TEST_MTU).is_none());
    }

    #[test]
    fn new_invalid_mtu_too_small() {
        assert!(NetworkPublication::new(1, 1, 0, TEST_TERM_LENGTH, 0).is_none());
        assert!(NetworkPublication::new(1, 1, 0, TEST_TERM_LENGTH, 31).is_none());
        // Exactly DATA_HEADER_LENGTH is valid (zero payload allowed).
        assert!(
            NetworkPublication::new(1, 1, 0, TEST_TERM_LENGTH, DATA_HEADER_LENGTH as u32)
                .is_some()
        );
    }

    // ---- offer: happy path ----

    #[test]
    fn offer_single_frame_empty_payload() {
        let mut p = make_pub();
        let pos = p.offer(&[]).unwrap();
        // Header-only frame: 32 bytes, aligned to 32.
        assert_eq!(pos, FRAME_ALIGNMENT as i64);
        assert_eq!(p.pub_position(), FRAME_ALIGNMENT as i64);
        assert_eq!(p.term_offset(), FRAME_ALIGNMENT as u32);
    }

    #[test]
    fn offer_single_frame_with_payload() {
        let mut p = make_pub();
        let payload = [0xCA, 0xFE, 0xBA, 0xBE];
        let pos = p.offer(&payload).unwrap();
        // 32 (header) + 4 (payload) = 36, aligned to 64.
        assert_eq!(pos, 64);
        assert_eq!(p.term_offset(), 64);
    }

    #[test]
    fn offer_advances_position_monotonically() {
        let mut p = make_pub();
        let mut prev = 0i64;
        for _ in 0..5 {
            let pos = p.offer(&[]).unwrap();
            assert!(pos > prev, "position must increase: {pos} > {prev}");
            prev = pos;
        }
    }

    // ---- offer: errors ----

    #[test]
    fn offer_payload_too_large() {
        // MTU = 64 -> max_payload = 64 - 32 = 32
        let mut p = make_pub_with(1, 1, 0, TEST_TERM_LENGTH, 64);
        let big_payload = [0u8; 33];
        assert_eq!(p.offer(&big_payload), Err(OfferError::PayloadTooLarge));
        // Exactly at limit should succeed.
        let ok_payload = [0u8; 32];
        assert!(p.offer(&ok_payload).is_ok());
    }

    #[test]
    fn offer_back_pressured() {
        // Small term: 64 bytes. Max ahead = 3 * 64 = 192.
        // Fill 3 terms without scanning -> back-pressure at 192 bytes.
        let mut p = make_pub_with(1, 1, 0, 64, TEST_MTU);

        // Term 0: 2 header-only frames fill 64 bytes, then AdminAction.
        assert!(p.offer(&[]).is_ok());    // pub_pos = 32
        assert!(p.offer(&[]).is_ok());    // pub_pos = 64
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));

        // Term 1: 2 frames.
        assert!(p.offer(&[]).is_ok());    // pub_pos = 96
        assert!(p.offer(&[]).is_ok());    // pub_pos = 128
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));

        // Term 2: 2 frames fill to pub_pos = 192 = max_ahead.
        assert!(p.offer(&[]).is_ok());    // pub_pos = 160
        assert!(p.offer(&[]).is_ok());    // pub_pos = 192

        // Next offer hits back-pressure check (192 - 0 >= 192) before
        // append_frame can even try. No AdminAction, just BackPressured.
        assert_eq!(p.offer(&[]), Err(OfferError::BackPressured));

        // After scanning, back-pressure clears.
        p.sender_scan(u32::MAX, |_, _| {});
        // Now we can offer again (triggers term rotation since term 2 is full).
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
        // And then write into term 3.
        assert!(p.offer(&[]).is_ok());
    }

    // ---- offer: term rotation ----

    #[test]
    fn offer_term_rotation_returns_admin_action() {
        // Term length = 64: 2 header-only frames fill it.
        let mut p = make_pub_with(1, 1, 0, 64, TEST_MTU);
        assert!(p.offer(&[]).is_ok());
        assert!(p.offer(&[]).is_ok());
        // Next offer triggers rotation.
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
    }

    #[test]
    fn offer_after_admin_action_succeeds() {
        let mut p = make_pub_with(1, 1, 0, 64, TEST_MTU);
        assert!(p.offer(&[]).is_ok());
        assert!(p.offer(&[]).is_ok());
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
        // Retry should succeed in the new term.
        assert!(p.offer(&[]).is_ok());
    }

    #[test]
    fn offer_term_id_increments_on_rotation() {
        let mut p = make_pub_with(1, 1, 10, 64, TEST_MTU);
        assert_eq!(p.active_term_id(), 10);

        // Fill term and trigger rotation.
        assert!(p.offer(&[]).is_ok());
        assert!(p.offer(&[]).is_ok());
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));

        assert_eq!(p.active_term_id(), 11);
        assert_eq!(p.term_offset(), 0);
    }

    #[test]
    fn offer_multiple_rotations_through_all_partitions() {
        let mut p = make_pub_with(1, 1, 0, 32, TEST_MTU);
        // term_length=32: each term holds 1 header-only frame.
        // Scan after each rotation to prevent back-pressure.
        for expected_term in 0..8i32 {
            assert_eq!(
                p.active_term_id(),
                expected_term,
                "before fill, term should be {expected_term}"
            );
            // Offer one frame to fill the term.
            assert!(p.offer(&[]).is_ok());
            // Drain sender to prevent back-pressure.
            p.sender_scan(u32::MAX, |_, _| {});
            // Next offer triggers rotation (unless last iteration).
            if expected_term < 7 {
                assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
            }
        }
        // Should have rotated through partitions 0,1,2,3,0,1,2,3.
        assert_eq!(p.active_term_id(), 7);
    }

    #[test]
    fn offer_wrapping_term_id_near_max() {
        let mut p = make_pub_with(1, 1, i32::MAX - 1, 32, TEST_MTU);
        assert_eq!(p.active_term_id(), i32::MAX - 1);

        // Fill and rotate: MAX-1 -> MAX.
        assert!(p.offer(&[]).is_ok());
        p.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
        assert_eq!(p.active_term_id(), i32::MAX);

        // Fill and rotate: MAX -> MIN (wraps).
        assert!(p.offer(&[]).is_ok());
        p.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
        assert_eq!(p.active_term_id(), i32::MIN);

        // Still functional after wrap.
        assert!(p.offer(&[]).is_ok());
    }

    // ---- sender_scan ----

    #[test]
    fn sender_scan_empty_publication() {
        let mut p = make_pub();
        let mut count = 0u32;
        let scanned = p.sender_scan(TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(scanned, 0);
        assert_eq!(count, 0);
        assert_eq!(p.sender_position(), 0);
    }

    #[test]
    fn sender_scan_single_frame() {
        let mut p = make_pub();
        p.offer(&[]).unwrap();

        let mut count = 0u32;
        let scanned = p.sender_scan(TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(count, 1);
        assert_eq!(scanned, FRAME_ALIGNMENT as u32);
        assert_eq!(p.sender_position(), FRAME_ALIGNMENT as i64);
    }

    #[test]
    fn sender_scan_advances_sender_position() {
        let mut p = make_pub();
        p.offer(&[]).unwrap();  // 32 bytes
        p.offer(&[]).unwrap();  // 32 bytes -> total 64

        let scanned = p.sender_scan(TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(scanned, 64);
        assert_eq!(p.sender_position(), 64);
    }

    #[test]
    fn sender_scan_limit_respected() {
        let mut p = make_pub();
        // Offer 4 header-only frames = 128 bytes.
        for _ in 0..4 {
            p.offer(&[]).unwrap();
        }

        // Scan with limit = 64 should only scan 2 frames.
        let mut count = 0u32;
        let scanned = p.sender_scan(64, |_, _| count += 1);
        assert_eq!(count, 2);
        assert_eq!(scanned, 64);
        assert_eq!(p.sender_position(), 64);
    }

    #[test]
    fn sender_scan_catches_up_to_pub_position() {
        let mut p = make_pub();
        for _ in 0..10 {
            p.offer(&[]).unwrap();
        }
        let pub_pos = p.pub_position();

        // Scan until caught up.
        let mut total = 0u32;
        loop {
            let scanned = p.sender_scan(TEST_TERM_LENGTH, |_, _| {});
            if scanned == 0 {
                break;
            }
            total += scanned;
        }
        assert_eq!(p.sender_position(), pub_pos);
        assert_eq!(total as i64, pub_pos);
    }

    #[test]
    fn sender_scan_across_term_boundary() {
        // term_length=64: 2 header-only frames per term.
        let mut p = make_pub_with(1, 1, 0, 64, TEST_MTU);

        // Fill term 0.
        p.offer(&[]).unwrap();
        p.offer(&[]).unwrap();

        // Scan term 0 fully.
        let scanned0 = p.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(scanned0, 64);
        assert_eq!(p.sender_position(), 64);

        // Rotate to term 1.
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));
        p.offer(&[]).unwrap();

        // Scan term 1.
        let mut count = 0u32;
        let scanned1 = p.sender_scan(u32::MAX, |_, _| count += 1);
        assert_eq!(count, 1);
        assert_eq!(scanned1, 32);
        assert_eq!(p.sender_position(), 96); // 64 + 32
    }

    #[test]
    fn sender_scan_emits_correct_frame_data() {
        let mut p = make_pub();
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        p.offer(&payload).unwrap();

        let mut found = false;
        p.sender_scan(TEST_TERM_LENGTH, |off, data| {
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

    // ---- Position arithmetic ----

    #[test]
    fn position_compute_basic() {
        let p = make_pub();
        assert_eq!(p.compute_position(0, 0), 0);
    }

    #[test]
    fn position_compute_mid_term() {
        let p = make_pub();
        assert_eq!(p.compute_position(0, 512), 512);
    }

    #[test]
    fn position_compute_second_term() {
        let p = make_pub();
        assert_eq!(p.compute_position(1, 0), TEST_TERM_LENGTH as i64);
    }

    #[test]
    fn position_compute_with_offset() {
        let p = make_pub();
        assert_eq!(
            p.compute_position(2, 256),
            2 * TEST_TERM_LENGTH as i64 + 256
        );
    }

    #[test]
    fn position_compute_wrapping() {
        // initial_term_id = i32::MAX, term_id = i32::MIN.
        // i32::MIN.wrapping_sub(i32::MAX) = 1 (as i32), so term_count = 1.
        let p = make_pub_with(1, 1, i32::MAX, TEST_TERM_LENGTH, TEST_MTU);
        assert_eq!(p.compute_position(i32::MIN, 0), TEST_TERM_LENGTH as i64);
    }

    // ---- Rotation cleans correct partition ----

    #[test]
    fn rotate_cleans_correct_partition() {
        // term_length=32, 1 frame per term. initial_term_id=0.
        let mut p = make_pub_with(1, 1, 0, 32, TEST_MTU);

        // Offer into partition 0 (term_id=0).
        p.offer(&[]).unwrap();
        // Drain so we can keep offering.
        p.sender_scan(u32::MAX, |_, _| {});

        // Partition 0 should have data.
        let part0 = p.raw_log().partition(0).unwrap();
        assert!(!part0.iter().all(|&b| b == 0), "partition 0 should have data");

        // Rotate to term_id=1 (partition 1).
        assert_eq!(p.offer(&[]), Err(OfferError::AdminAction));

        // New strategy: rotation cleans the partition we're entering (partition 1).
        // Partition 0 still has data (sender already scanned it, but not zeroed).
        let part0_after = p.raw_log().partition(0).unwrap();
        assert!(
            !part0_after.iter().all(|&b| b == 0),
            "partition 0 should still have data (not cleaned by rotation)"
        );

        // Partition 1 was cleaned (it was already zero from construction, so no-op).
        let part1 = p.raw_log().partition(1).unwrap();
        assert!(
            part1.iter().all(|&b| b == 0),
            "partition 1 should be clean (ready for new writes)"
        );
    }

    // ---- OfferError ----

    #[test]
    fn offer_error_display() {
        assert_eq!(OfferError::BackPressured.to_string(), "back pressured");
        assert_eq!(OfferError::AdminAction.to_string(), "admin action");
        assert_eq!(OfferError::PayloadTooLarge.to_string(), "payload too large");
        assert_eq!(OfferError::InvalidState.to_string(), "invalid state");
    }

    #[test]
    fn offer_error_is_copy() {
        let e = OfferError::BackPressured;
        let e2 = e; // Copy
        let e3 = e; // Still valid - Copy
        assert_eq!(e2, e3);
    }

    // ---- Accessor coverage ----

    #[test]
    fn accessors_return_construction_values() {
        let p = make_pub_with(42, 7, 100, 1024, 1408);
        assert_eq!(p.session_id(), 42);
        assert_eq!(p.stream_id(), 7);
        assert_eq!(p.initial_term_id(), 100);
        assert_eq!(p.active_term_id(), 100);
        assert_eq!(p.term_length(), 1024);
        assert_eq!(p.mtu(), 1408);
        assert_eq!(p.term_offset(), 0);
        assert_eq!(p.pub_position(), 0);
        assert_eq!(p.sender_position(), 0);
    }

    #[test]
    fn raw_log_accessor() {
        let p = make_pub();
        let log = p.raw_log();
        assert_eq!(log.term_length(), TEST_TERM_LENGTH);
        assert_eq!(log.capacity(), TEST_TERM_LENGTH as usize * PARTITION_COUNT);
    }
}

