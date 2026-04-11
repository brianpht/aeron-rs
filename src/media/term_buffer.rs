// Term buffer - manages the raw log buffer for publications.
//
// A RawLog is a single contiguous Vec<u8> divided into 4 equal partitions
// (see ADR-001). Each partition holds one term's worth of data frames.
// The Vec is allocated once at construction and never resized.
//
// All index arithmetic uses bitmask (& mask) instead of modulo (%).
// All term_id arithmetic uses wrapping_sub to handle i32 wrap.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::frame::{CURRENT_VERSION, DATA_HEADER_LENGTH, FRAME_TYPE_PAD};

// ---- Constants ----

/// Number of term partitions. Power-of-two enables `& (PARTITION_COUNT - 1)`.
/// See ADR-001: 4 partitions instead of Aeron's 3.
pub const PARTITION_COUNT: usize = 4;

/// Bitmask for partition index: `(term_id - initial_term_id) & PARTITION_INDEX_MASK`.
const PARTITION_INDEX_MASK: u32 = (PARTITION_COUNT as u32) - 1;

/// Frame alignment in bytes. Matches DATA_HEADER_LENGTH (32).
/// Every frame starts at a 32-byte aligned offset within its partition.
pub const FRAME_ALIGNMENT: usize = DATA_HEADER_LENGTH;

/// Bitmask for aligning up to FRAME_ALIGNMENT.
const ALIGNMENT_MASK: usize = !(FRAME_ALIGNMENT - 1);

/// Minimum supported term_length (must be >= FRAME_ALIGNMENT and power-of-two).
const MIN_TERM_LENGTH: u32 = FRAME_ALIGNMENT as u32;

// ---- Error type ----

/// Stack-only error for term buffer operations.
/// Matches the `PollError` pattern - never heap-allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendError {
    /// The current term partition is full. Caller should rotate to next term.
    TermFull,
    /// Partition index is out of range [0, PARTITION_COUNT).
    InvalidPartition,
}

impl fmt::Display for AppendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppendError::TermFull => f.write_str("term partition full"),
            AppendError::InvalidPartition => f.write_str("partition index out of range"),
        }
    }
}

// ---- RawLog ----

/// Raw log buffer divided into 4 equal partitions.
///
/// Memory layout:
/// ```text
/// [partition 0][partition 1][partition 2][partition 3]
///  term_length   term_length  term_length  term_length
/// ```
///
/// The backing `Vec<u8>` is allocated once in `new()` and never resized.
/// Each partition stores data frames written by `append_frame` and read by
/// `scan_frames`. Frames are 32-byte aligned within each partition.
pub struct RawLog {
    /// Backing buffer: length = PARTITION_COUNT * term_length. Allocated once.
    buffer: Vec<u8>,
    /// Term length in bytes. Guaranteed power-of-two >= FRAME_ALIGNMENT.
    term_length: u32,
}

impl RawLog {
    /// Create a new RawLog with the given term length.
    ///
    /// `term_length` must be a power-of-two and >= 32 (FRAME_ALIGNMENT).
    /// Returns `None` if the constraint is violated.
    ///
    /// This is a cold-path operation (allocation happens here, once).
    pub fn new(term_length: u32) -> Option<Self> {
        if term_length < MIN_TERM_LENGTH || !term_length.is_power_of_two() {
            return None;
        }
        let total = (term_length as usize).checked_mul(PARTITION_COUNT)?;
        let buffer = vec![0u8; total];
        Some(Self {
            buffer,
            term_length,
        })
    }

    /// Term length in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.term_length
    }

    /// Total buffer capacity (all partitions).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Compute the partition index for a given term_id relative to initial_term_id.
    ///
    /// Uses wrapping subtraction (handles i32 wrap) and bitmask (no modulo).
    #[inline]
    pub fn partition_index(term_id: i32, initial_term_id: i32) -> usize {
        (term_id.wrapping_sub(initial_term_id) as u32 & PARTITION_INDEX_MASK) as usize
    }

    /// Immutable slice of a single partition.
    ///
    /// Returns `None` if `partition_idx >= PARTITION_COUNT`.
    #[inline]
    pub fn partition(&self, partition_idx: usize) -> Option<&[u8]> {
        if partition_idx >= PARTITION_COUNT {
            return None;
        }
        let offset = partition_idx * self.term_length as usize;
        Some(&self.buffer[offset..offset + self.term_length as usize])
    }

    /// Mutable slice of a single partition.
    ///
    /// Returns `None` if `partition_idx >= PARTITION_COUNT`.
    #[inline]
    pub fn partition_mut(&mut self, partition_idx: usize) -> Option<&mut [u8]> {
        if partition_idx >= PARTITION_COUNT {
            return None;
        }
        let tl = self.term_length as usize;
        let offset = partition_idx * tl;
        Some(&mut self.buffer[offset..offset + tl])
    }

    /// Append a data frame (header + payload) into a partition at the given offset.
    ///
    /// The `header` must already have `frame_length` set correctly by the caller.
    /// Payload is copied immediately after the 32-byte DataHeader.
    /// The frame is padded to FRAME_ALIGNMENT (32 bytes).
    ///
    /// Returns the new term_offset (current + aligned frame length) on success.
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

        let raw_len = DATA_HEADER_LENGTH + payload.len();
        let aligned_len = (raw_len + FRAME_ALIGNMENT - 1) & ALIGNMENT_MASK;
        let new_offset = term_offset + aligned_len as u32;

        if new_offset > self.term_length {
            return Err(AppendError::TermFull);
        }

        let tl = self.term_length as usize;
        let base = partition_idx * tl + term_offset as usize;

        // Copy DataHeader (32 bytes) into the partition.
        // SAFETY: copy_nonoverlapping is safe - source is a valid DataHeader,
        // destination is within our owned buffer, and we verified bounds above.
        unsafe {
            std::ptr::copy_nonoverlapping(
                header as *const crate::frame::DataHeader as *const u8,
                self.buffer[base..].as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }

        // Copy payload after the header.
        if !payload.is_empty() {
            self.buffer[base + DATA_HEADER_LENGTH..base + DATA_HEADER_LENGTH + payload.len()]
                .copy_from_slice(payload);
        }

        Ok(new_offset)
    }

    /// Scan committed frames in a partition starting from `offset`.
    ///
    /// Walks forward through frames, reading `frame_length` from each frame's
    /// first 4 bytes (little-endian i32). Stops when:
    /// - `frame_length <= 0` (uncommitted / zero region)
    /// - `frame_length < DATA_HEADER_LENGTH` (corrupt / padding-only)
    /// - Accumulated bytes scanned reaches `limit`
    /// - End of partition reached
    ///
    /// Calls `emit(term_offset, frame_slice)` for each valid frame.
    /// Returns total bytes scanned.
    ///
    /// Zero-allocation, O(n) in number of frames. Hot path.
    pub fn scan_frames<F>(&self, partition_idx: usize, offset: u32, limit: u32, mut emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let part = match self.partition(partition_idx) {
            Some(p) => p,
            None => return 0,
        };

        let tl = self.term_length;
        let mut pos = offset;
        let mut scanned: u32 = 0;

        while pos < tl && scanned < limit {
            let remaining = (tl - pos) as usize;
            if remaining < 4 {
                break;
            }

            // Read frame_length field-by-field (little-endian, no pointer cast).
            let p = pos as usize;
            let frame_length = i32::from_le_bytes([part[p], part[p + 1], part[p + 2], part[p + 3]]);

            // Zero or negative frame_length means we hit uncommitted space.
            if frame_length <= 0 {
                break;
            }

            let frame_len_u = frame_length as u32;

            // Frame too small to contain a DataHeader - stop.
            if frame_len_u < DATA_HEADER_LENGTH as u32 {
                break;
            }

            // Align frame length up to FRAME_ALIGNMENT.
            let aligned_len = (frame_len_u + FRAME_ALIGNMENT as u32 - 1) & ALIGNMENT_MASK as u32;

            // Would exceed partition bounds - stop.
            if pos + aligned_len > tl {
                break;
            }

            emit(pos, &part[p..p + frame_len_u as usize]);

            pos += aligned_len;
            scanned += aligned_len;
        }

        scanned
    }

    /// Zero out an entire partition. Called during term rotation (cold path).
    pub fn clean_partition(&mut self, partition_idx: usize) {
        if let Some(part) = self.partition_mut(partition_idx) {
            part.fill(0);
        }
    }

    /// Write a padding frame covering the unused tail of a term partition.
    ///
    /// Called before `rotate_term()` when `append_frame` returns `TermFull`
    /// and `term_offset < term_length`. The pad frame has `frame_type =
    /// FRAME_TYPE_PAD` and `frame_length = term_length - term_offset`,
    /// allowing `scan_frames` to advance `sender_position` past the term
    /// boundary instead of getting stuck on zeros.
    ///
    /// Only the 32-byte DataHeader is written - the remaining bytes in the
    /// pad region are already zeroed from construction or `clean_partition`.
    ///
    /// No-op if `term_offset >= term_length` or `remaining < DATA_HEADER_LENGTH`.
    pub fn write_pad_frame(&mut self, partition_idx: usize, term_offset: u32) {
        if partition_idx >= PARTITION_COUNT {
            return;
        }
        if term_offset >= self.term_length {
            return;
        }
        let remaining = self.term_length - term_offset;
        if (remaining as usize) < DATA_HEADER_LENGTH {
            return;
        }
        let tl = self.term_length as usize;
        let base = partition_idx * tl + term_offset as usize;

        // Build a minimal DataHeader on the stack with FRAME_TYPE_PAD.
        let hdr = crate::frame::DataHeader {
            frame_header: crate::frame::FrameHeader {
                frame_length: remaining as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_PAD,
            },
            term_offset: term_offset as i32,
            session_id: 0,
            stream_id: 0,
            term_id: 0,
            reserved_value: 0,
        };

        // SAFETY: bounds verified above - base + DATA_HEADER_LENGTH <= total buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const crate::frame::DataHeader as *const u8,
                self.buffer[base..].as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
    }
}

// ---- Compile-time assertions ----

const _: () = assert!(FRAME_ALIGNMENT == DATA_HEADER_LENGTH);
const _: () = assert!(PARTITION_COUNT.is_power_of_two());
const _: () = assert!(FRAME_ALIGNMENT.is_power_of_two());

// ---- Free function: partition index ----

/// Compute partition index from term_id and initial_term_id.
///
/// Uses wrapping subtraction (handles i32 wrap) and bitmask (no modulo).
/// Standalone function so both `RawLog` and `SharedLogBuffer` can use it.
#[inline]
pub fn partition_index(term_id: i32, initial_term_id: i32) -> usize {
    (term_id.wrapping_sub(initial_term_id) as u32 & PARTITION_INDEX_MASK) as usize
}

// ---- SharedLogBuffer ----

/// Shared log buffer for cross-thread publication.
///
/// Wraps a single contiguous `Vec<u8>` (allocated once, never resized)
/// inside `UnsafeCell` so that a publisher thread and a sender thread
/// can both access the backing memory without `&mut`.
///
/// Memory layout is identical to `RawLog`:
/// ```text
/// [partition 0][partition 1][partition 2][partition 3]
///  term_length   term_length  term_length  term_length
/// ```
///
/// # Safety contract
///
/// - Single writer per partition at a time (enforced by `ConcurrentPublication`
///   taking `&mut self`).
/// - Readers only read committed frames (guarded by Acquire load of
///   `frame_length` at offset 0 of each frame).
/// - Back-pressure guarantees the publisher never writes into a partition
///   that the sender is still reading.
/// - The `Vec<u8>` is never resized after construction.
pub struct SharedLogBuffer {
    buffer: UnsafeCell<Vec<u8>>,
    term_length: u32,
}

// SAFETY: See safety contract above. Single writer per partition,
// readers use Acquire/Release protocol on frame_length commit word.
// Buffer is never resized.
unsafe impl Send for SharedLogBuffer {}
unsafe impl Sync for SharedLogBuffer {}

impl SharedLogBuffer {
    /// Create a new SharedLogBuffer. Cold path - allocation happens here.
    ///
    /// Returns `None` if `term_length` is not power-of-two or < FRAME_ALIGNMENT.
    pub fn new(term_length: u32) -> Option<Self> {
        if term_length < MIN_TERM_LENGTH || !term_length.is_power_of_two() {
            return None;
        }
        let total = (term_length as usize).checked_mul(PARTITION_COUNT)?;
        let buffer = vec![0u8; total];
        Some(Self {
            buffer: UnsafeCell::new(buffer),
            term_length,
        })
    }

    /// Term length in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.term_length
    }

    /// Total buffer capacity (all partitions).
    #[inline]
    pub fn capacity(&self) -> usize {
        // SAFETY: reading Vec::len() is safe - no mutation of the Vec itself.
        unsafe { (*self.buffer.get()).len() }
    }

    /// Raw pointer to the buffer start.
    ///
    /// # Safety
    ///
    /// Caller must uphold the single-writer-per-partition invariant and
    /// use atomic frame_length for cross-thread commit/read protocol.
    #[inline]
    pub unsafe fn as_mut_ptr(&self) -> *mut u8 {
        unsafe { (*self.buffer.get()).as_mut_ptr() }
    }

    /// Read-only pointer to the buffer start.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        // SAFETY: reading the pointer is safe. Actual reads must follow
        // the Acquire protocol on frame_length.
        unsafe { (*self.buffer.get()).as_ptr() }
    }

    /// Immutable slice of a partition. For use by the sender (scanner) thread.
    ///
    /// # Safety
    ///
    /// Caller must ensure no concurrent writes to this partition, or must
    /// use atomic frame_length loads to only read committed regions.
    #[inline]
    pub unsafe fn partition_slice(&self, partition_idx: usize) -> Option<&[u8]> {
        if partition_idx >= PARTITION_COUNT {
            return None;
        }
        let tl = self.term_length as usize;
        let offset = partition_idx * tl;
        unsafe {
            let ptr = self.as_ptr().add(offset);
            Some(std::slice::from_raw_parts(ptr, tl))
        }
    }

    /// Zero out an entire partition. Called during term rotation.
    ///
    /// # Safety
    ///
    /// Caller must ensure the sender has finished scanning this partition
    /// (guaranteed by back-pressure: publisher is at most 3 terms ahead).
    pub unsafe fn clean_partition(&self, partition_idx: usize) {
        if partition_idx >= PARTITION_COUNT {
            return;
        }
        let tl = self.term_length as usize;
        let offset = partition_idx * tl;
        unsafe {
            let ptr = self.as_mut_ptr().add(offset);
            std::ptr::write_bytes(ptr, 0, tl);
        }
    }
}

// ---- Atomic frame_length helpers ----

/// Release-ordered store of frame_length (i32) at `offset` within the buffer.
///
/// Used by the publisher to commit a frame. All preceding header/payload
/// writes become visible to any thread that reads this word with Acquire.
///
/// # Safety
///
/// - `ptr` must point to a valid buffer region of sufficient length.
/// - `offset` must be 4-byte aligned (all frame offsets are 32-byte aligned).
/// - Caller must have exclusive write access to this frame slot.
#[inline]
pub unsafe fn atomic_frame_length_store(ptr: *mut u8, offset: usize, value: i32) {
    unsafe {
        let target = &*ptr.add(offset).cast::<AtomicI32>();
        target.store(value, Ordering::Release);
    }
}

/// Acquire-ordered load of frame_length (i32) at `offset` within the buffer.
///
/// Used by the sender/scanner to check if a frame has been committed.
/// If the returned value is > 0, all header/payload bytes preceding the
/// publisher's Release store are guaranteed visible.
///
/// # Safety
///
/// - `ptr` must point to a valid buffer region of sufficient length.
/// - `offset` must be 4-byte aligned.
#[inline]
pub unsafe fn atomic_frame_length_load(ptr: *const u8, offset: usize) -> i32 {
    unsafe {
        let target = &*ptr.add(offset).cast::<AtomicI32>();
        target.load(Ordering::Acquire)
    }
}

/// Align a byte length up to FRAME_ALIGNMENT (32 bytes).
#[inline]
pub const fn align_frame_length(len: usize) -> usize {
    (len + FRAME_ALIGNMENT - 1) & ALIGNMENT_MASK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::*;

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

    // ---- Constructor ----

    #[test]
    fn new_rejects_non_power_of_two() {
        assert!(RawLog::new(100).is_none());
        assert!(RawLog::new(1000).is_none());
        assert!(RawLog::new(48).is_none());
    }

    #[test]
    fn new_rejects_too_small() {
        assert!(RawLog::new(0).is_none());
        assert!(RawLog::new(1).is_none());
        assert!(RawLog::new(16).is_none()); // < FRAME_ALIGNMENT (32)
    }

    #[test]
    fn new_accepts_valid_sizes() {
        assert!(RawLog::new(32).is_some());
        assert!(RawLog::new(64).is_some());
        assert!(RawLog::new(1024).is_some());
        assert!(RawLog::new(65536).is_some());
    }

    #[test]
    fn new_allocates_correct_capacity() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        assert_eq!(log.capacity(), TEST_TERM_LENGTH as usize * PARTITION_COUNT);
        assert_eq!(log.term_length(), TEST_TERM_LENGTH);
    }

    #[test]
    fn buffer_starts_zeroed() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        for idx in 0..PARTITION_COUNT {
            let part = log.partition(idx).unwrap();
            assert!(part.iter().all(|&b| b == 0));
        }
    }

    // ---- partition_index ----

    #[test]
    fn partition_index_basic() {
        assert_eq!(RawLog::partition_index(0, 0), 0);
        assert_eq!(RawLog::partition_index(1, 0), 1);
        assert_eq!(RawLog::partition_index(2, 0), 2);
        assert_eq!(RawLog::partition_index(3, 0), 3);
        assert_eq!(RawLog::partition_index(4, 0), 0); // wraps
        assert_eq!(RawLog::partition_index(5, 0), 1);
    }

    #[test]
    fn partition_index_with_initial_term_id() {
        assert_eq!(RawLog::partition_index(10, 10), 0);
        assert_eq!(RawLog::partition_index(11, 10), 1);
        assert_eq!(RawLog::partition_index(12, 10), 2);
        assert_eq!(RawLog::partition_index(13, 10), 3);
        assert_eq!(RawLog::partition_index(14, 10), 0);
    }

    #[test]
    fn partition_index_wraps_at_i32_boundary() {
        // term_id near i32::MAX, initial_term_id well before it.
        // (i32::MAX - 0) as u32 & 3 = 0x7FFF_FFFF & 3 = 3
        assert_eq!(RawLog::partition_index(i32::MAX, 0), 3);
        // Wrap from MAX to MIN: i32::MIN.wrapping_sub(i32::MAX) = 1
        assert_eq!(RawLog::partition_index(i32::MIN, i32::MAX), 1 & 3);
        // Negative initial: (-1).wrapping_sub(-5) = 4, 4 & 3 = 0
        assert_eq!(RawLog::partition_index(-1, -5), 0);
    }

    // ---- partition accessors ----

    #[test]
    fn partition_out_of_range_returns_none() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        assert!(log.partition(PARTITION_COUNT).is_none());
        assert!(log.partition(100).is_none());
    }

    #[test]
    fn partition_returns_correct_length() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        for idx in 0..PARTITION_COUNT {
            assert_eq!(log.partition(idx).unwrap().len(), TEST_TERM_LENGTH as usize);
        }
    }

    #[test]
    fn partition_mut_out_of_range_returns_none() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        assert!(log.partition_mut(PARTITION_COUNT).is_none());
    }

    #[test]
    fn partitions_are_independent() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        // Write to partition 0.
        log.partition_mut(0).unwrap()[0] = 0xAA;
        // Partition 1 should still be zero.
        assert_eq!(log.partition(1).unwrap()[0], 0);
        // Partition 0 should have our write.
        assert_eq!(log.partition(0).unwrap()[0], 0xAA);
    }

    // ---- append_frame ----

    #[test]
    fn append_frame_invalid_partition() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32, 0, 1, 1, 0);
        assert_eq!(
            log.append_frame(PARTITION_COUNT, 0, &hdr, &[]),
            Err(AppendError::InvalidPartition)
        );
    }

    #[test]
    fn append_frame_header_only() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);

        let new_offset = log.append_frame(0, 0, &hdr, &[]).unwrap();
        assert_eq!(new_offset, FRAME_ALIGNMENT as u32);

        // Verify the header was written by parsing it back.
        let part = log.partition(0).unwrap();
        let parsed = DataHeader::parse(part).unwrap();
        assert_eq!(
            { parsed.frame_header.frame_length },
            DATA_HEADER_LENGTH as i32
        );
        assert_eq!({ parsed.session_id }, 42);
        assert_eq!({ parsed.stream_id }, 7);
    }

    #[test]
    fn append_frame_with_payload_roundtrip() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let payload = [0xCA, 0xFE, 0xBA, 0xBE];
        let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let hdr = make_data_header(frame_len, 0, 42, 7, 0);

        let new_offset = log.append_frame(0, 0, &hdr, &payload).unwrap();
        // 32 (header) + 4 (payload) = 36, aligned to 32 = 64
        assert_eq!(new_offset, 64);

        let part = log.partition(0).unwrap();
        let parsed = DataHeader::parse(part).unwrap();
        let extracted = parsed.payload(part);
        assert_eq!(extracted, &payload);
    }

    #[test]
    fn append_frame_alignment_padding() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        // Payload of 1 byte: total = 33, aligned = 64
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32 + 1, 0, 1, 1, 0);
        let new_offset = log.append_frame(0, 0, &hdr, &[0xFF]).unwrap();
        assert_eq!(new_offset, 64);

        // Payload of 0 bytes: total = 32, aligned = 32
        let hdr2 = make_data_header(DATA_HEADER_LENGTH as i32, 64, 1, 1, 0);
        let new_offset2 = log.append_frame(0, 64, &hdr2, &[]).unwrap();
        assert_eq!(new_offset2, 96);
    }

    #[test]
    fn append_frame_term_full() {
        let mut log = RawLog::new(64).unwrap(); // tiny term: 64 bytes
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32, 0, 1, 1, 0);

        // First frame at offset 0 should succeed (32 bytes).
        let off = log.append_frame(0, 0, &hdr, &[]).unwrap();
        assert_eq!(off, 32);

        // Second frame at offset 32 should succeed (32 bytes, fills to 64).
        let hdr2 = make_data_header(DATA_HEADER_LENGTH as i32, 32, 1, 1, 0);
        let off2 = log.append_frame(0, 32, &hdr2, &[]).unwrap();
        assert_eq!(off2, 64);

        // Third frame should fail - term is full.
        let hdr3 = make_data_header(DATA_HEADER_LENGTH as i32, 64, 1, 1, 0);
        assert_eq!(
            log.append_frame(0, 64, &hdr3, &[]),
            Err(AppendError::TermFull)
        );
    }

    #[test]
    fn append_frame_payload_exceeds_remaining() {
        let mut log = RawLog::new(64).unwrap();
        // Try to write 33 bytes payload at offset 0 - needs 96 bytes aligned, only 64 avail.
        let payload = [0u8; 33];
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32 + 33, 0, 1, 1, 0);
        assert_eq!(
            log.append_frame(0, 0, &hdr, &payload),
            Err(AppendError::TermFull)
        );
    }

    // ---- scan_frames ----

    #[test]
    fn scan_frames_empty_partition() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let mut count = 0u32;
        let scanned = log.scan_frames(0, 0, TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(scanned, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn scan_frames_single_frame() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let payload = [0xDE, 0xAD];
        let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let hdr = make_data_header(frame_len, 0, 42, 7, 0);
        log.append_frame(0, 0, &hdr, &payload).unwrap();

        let mut frames: Vec<(u32, Vec<u8>)> = Vec::new();
        let scanned = log.scan_frames(0, 0, TEST_TERM_LENGTH, |off, data| {
            frames.push((off, data.to_vec()));
        });

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, 0);
        assert_eq!(frames[0].1.len(), frame_len as usize);
        // 34 bytes frame, aligned to 64
        assert_eq!(scanned, 64);
    }

    #[test]
    fn scan_frames_multiple_frames() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();

        // Frame 1: header only (32 bytes, aligned 32)
        let hdr1 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 1, 1, 0);
        let off1 = log.append_frame(0, 0, &hdr1, &[]).unwrap();
        assert_eq!(off1, 32);

        // Frame 2: header + 4 bytes payload (36 bytes, aligned 64)
        let payload2 = [1, 2, 3, 4];
        let hdr2 = make_data_header(DATA_HEADER_LENGTH as i32 + 4, off1 as i32, 2, 1, 0);
        let off2 = log.append_frame(0, off1, &hdr2, &payload2).unwrap();
        assert_eq!(off2, 96); // 32 + 64

        let mut offsets = Vec::new();
        let scanned = log.scan_frames(0, 0, TEST_TERM_LENGTH, |off, _| {
            offsets.push(off);
        });

        assert_eq!(offsets, vec![0, 32]);
        assert_eq!(scanned, 96); // 32 + 64
    }

    #[test]
    fn scan_frames_stops_at_zero_frame_length() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32, 0, 1, 1, 0);
        log.append_frame(0, 0, &hdr, &[]).unwrap();

        // Scan - should find exactly 1 frame, then stop at zeroed region.
        let mut count = 0u32;
        log.scan_frames(0, 0, TEST_TERM_LENGTH, |_, _| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn scan_frames_respects_limit() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();

        // Write 4 header-only frames (32 bytes each = 128 total).
        for i in 0..4u32 {
            let off = i * FRAME_ALIGNMENT as u32;
            let hdr = make_data_header(DATA_HEADER_LENGTH as i32, off as i32, 1, 1, 0);
            log.append_frame(0, off, &hdr, &[]).unwrap();
        }

        // Limit to 64 bytes - should only emit 2 frames.
        let mut count = 0u32;
        let scanned = log.scan_frames(0, 0, 64, |_, _| count += 1);
        assert_eq!(count, 2);
        assert_eq!(scanned, 64);
    }

    #[test]
    fn scan_frames_from_nonzero_offset() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();

        // Write 3 header-only frames.
        for i in 0..3u32 {
            let off = i * FRAME_ALIGNMENT as u32;
            let hdr = make_data_header(DATA_HEADER_LENGTH as i32, off as i32, i as i32, 1, 0);
            log.append_frame(0, off, &hdr, &[]).unwrap();
        }

        // Scan from offset 32 - should skip frame 0, find frames 1 and 2.
        let mut session_ids = Vec::new();
        log.scan_frames(0, 32, TEST_TERM_LENGTH, |_, data| {
            if let Some(parsed) = DataHeader::parse(data) {
                session_ids.push(parsed.session_id);
            }
        });
        assert_eq!(session_ids, vec![1, 2]);
    }

    #[test]
    fn scan_frames_invalid_partition_returns_zero() {
        let log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let scanned = log.scan_frames(PARTITION_COUNT, 0, TEST_TERM_LENGTH, |_, _| {});
        assert_eq!(scanned, 0);
    }

    // ---- clean_partition ----

    #[test]
    fn clean_partition_zeros_all_bytes() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let hdr = make_data_header(DATA_HEADER_LENGTH as i32, 0, 42, 7, 0);
        log.append_frame(0, 0, &hdr, &[]).unwrap();

        // Verify data was written.
        assert_ne!(log.partition(0).unwrap()[0], 0);

        // Clean and verify.
        log.clean_partition(0);
        assert!(log.partition(0).unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn clean_partition_does_not_affect_others() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 10, 1, 0);
        let hdr1 = make_data_header(DATA_HEADER_LENGTH as i32, 0, 20, 1, 1);
        log.append_frame(0, 0, &hdr0, &[]).unwrap();
        log.append_frame(1, 0, &hdr1, &[]).unwrap();

        log.clean_partition(0);

        // Partition 0 is clean.
        assert!(log.partition(0).unwrap().iter().all(|&b| b == 0));
        // Partition 1 still has data.
        let p1 = log.partition(1).unwrap();
        let parsed = DataHeader::parse(p1).unwrap();
        assert_eq!({ parsed.session_id }, 20);
    }

    #[test]
    fn clean_partition_out_of_range_is_noop() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        // Should not panic.
        log.clean_partition(PARTITION_COUNT);
        log.clean_partition(100);
    }

    // ---- append then scan roundtrip ----

    #[test]
    fn append_scan_roundtrip_preserves_payload() {
        let mut log = RawLog::new(TEST_TERM_LENGTH).unwrap();
        let payload = b"hello world - test payload data!";
        let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
        let hdr = make_data_header(frame_len, 0, 99, 5, 0);
        log.append_frame(0, 0, &hdr, payload).unwrap();

        let mut found_payload = Vec::new();
        log.scan_frames(0, 0, TEST_TERM_LENGTH, |_, data| {
            if let Some(parsed) = DataHeader::parse(data) {
                found_payload.extend_from_slice(parsed.payload(data));
            }
        });
        assert_eq!(found_payload.as_slice(), payload.as_slice());
    }

    #[test]
    fn fill_partition_then_scan_all() {
        let tl: u32 = 256;
        let mut log = RawLog::new(tl).unwrap();

        // Fill with header-only frames: 256 / 32 = 8 frames.
        let mut offset = 0u32;
        let mut written = 0u32;
        while offset < tl {
            let hdr = make_data_header(
                DATA_HEADER_LENGTH as i32,
                offset as i32,
                written as i32,
                1,
                0,
            );
            match log.append_frame(0, offset, &hdr, &[]) {
                Ok(new_off) => {
                    offset = new_off;
                    written += 1;
                }
                Err(AppendError::TermFull) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(written, 8); // 256 / 32

        // Scan should find all 8 frames.
        let mut count = 0u32;
        log.scan_frames(0, 0, tl, |_, _| count += 1);
        assert_eq!(count, 8);
    }

    // ---- AppendError Display ----

    #[test]
    fn append_error_display() {
        let e = AppendError::TermFull;
        assert_eq!(e.to_string(), "term partition full");
        let e2 = AppendError::InvalidPartition;
        assert_eq!(e2.to_string(), "partition index out of range");
    }

    // ---- SharedLogBuffer ----

    #[test]
    fn shared_log_new_valid() {
        let log = SharedLogBuffer::new(TEST_TERM_LENGTH);
        assert!(log.is_some());
        let log = log.unwrap();
        assert_eq!(log.term_length(), TEST_TERM_LENGTH);
        assert_eq!(log.capacity(), TEST_TERM_LENGTH as usize * PARTITION_COUNT);
    }

    #[test]
    fn shared_log_new_rejects_invalid() {
        assert!(SharedLogBuffer::new(0).is_none());
        assert!(SharedLogBuffer::new(16).is_none());
        assert!(SharedLogBuffer::new(100).is_none());
    }

    #[test]
    fn shared_log_starts_zeroed() {
        let log = SharedLogBuffer::new(TEST_TERM_LENGTH).unwrap();
        for idx in 0..PARTITION_COUNT {
            let part = unsafe { log.partition_slice(idx).unwrap() };
            assert!(part.iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn shared_log_partition_out_of_range() {
        let log = SharedLogBuffer::new(TEST_TERM_LENGTH).unwrap();
        assert!(unsafe { log.partition_slice(PARTITION_COUNT) }.is_none());
        assert!(unsafe { log.partition_slice(100) }.is_none());
    }

    #[test]
    fn shared_log_clean_partition() {
        let log = SharedLogBuffer::new(TEST_TERM_LENGTH).unwrap();
        // Write some bytes.
        unsafe {
            let ptr = log.as_mut_ptr();
            *ptr = 0xAA;
            *(ptr.add(1)) = 0xBB;
        }
        // Verify written.
        let part = unsafe { log.partition_slice(0).unwrap() };
        assert_eq!(part[0], 0xAA);
        assert_eq!(part[1], 0xBB);

        // Clean.
        unsafe { log.clean_partition(0) };
        let part = unsafe { log.partition_slice(0).unwrap() };
        assert!(part.iter().all(|&b| b == 0));
    }

    #[test]
    fn shared_log_clean_does_not_affect_other_partitions() {
        let log = SharedLogBuffer::new(64).unwrap();
        unsafe {
            // Write to partition 1.
            let ptr = log.as_mut_ptr().add(64);
            *ptr = 0xFF;
        }
        // Clean partition 0.
        unsafe { log.clean_partition(0) };
        // Partition 1 still has data.
        let part1 = unsafe { log.partition_slice(1).unwrap() };
        assert_eq!(part1[0], 0xFF);
    }

    // ---- Atomic frame_length helpers ----

    #[test]
    fn atomic_frame_length_roundtrip() {
        // Allocate a 32-byte aligned buffer.
        let log = SharedLogBuffer::new(64).unwrap();
        let ptr = unsafe { log.as_mut_ptr() };

        // Store then load.
        unsafe {
            atomic_frame_length_store(ptr, 0, 42);
            let val = atomic_frame_length_load(ptr as *const u8, 0);
            assert_eq!(val, 42);
        }
    }

    #[test]
    fn atomic_frame_length_zero_means_uncommitted() {
        let log = SharedLogBuffer::new(64).unwrap();
        let ptr = log.as_ptr();
        // Buffer starts zeroed - frame_length should read 0 (uncommitted).
        let val = unsafe { atomic_frame_length_load(ptr, 0) };
        assert_eq!(val, 0);
    }

    #[test]
    fn atomic_frame_length_at_nonzero_offset() {
        let log = SharedLogBuffer::new(128).unwrap();
        let ptr = unsafe { log.as_mut_ptr() };

        // Write at offset 32 (second frame slot).
        unsafe {
            atomic_frame_length_store(ptr, 32, 99);
            // First slot still zero.
            assert_eq!(atomic_frame_length_load(ptr as *const u8, 0), 0);
            // Second slot has our value.
            assert_eq!(atomic_frame_length_load(ptr as *const u8, 32), 99);
        }
    }

    // ---- align_frame_length ----

    #[test]
    fn align_frame_length_exact_multiple() {
        assert_eq!(align_frame_length(32), 32);
        assert_eq!(align_frame_length(64), 64);
    }

    #[test]
    fn align_frame_length_rounds_up() {
        assert_eq!(align_frame_length(33), 64);
        assert_eq!(align_frame_length(36), 64);
        assert_eq!(align_frame_length(63), 64);
    }

    #[test]
    fn align_frame_length_zero() {
        assert_eq!(align_frame_length(0), 0);
    }

    // ---- partition_index free function ----

    #[test]
    fn free_partition_index_matches_raw_log() {
        assert_eq!(partition_index(0, 0), RawLog::partition_index(0, 0));
        assert_eq!(partition_index(5, 0), RawLog::partition_index(5, 0));
        assert_eq!(
            partition_index(i32::MAX, 0),
            RawLog::partition_index(i32::MAX, 0)
        );
        assert_eq!(
            partition_index(i32::MIN, i32::MAX),
            RawLog::partition_index(i32::MIN, i32::MAX)
        );
    }
}
