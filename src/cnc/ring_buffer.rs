// MPSC ring buffer over a contiguous byte slice (mmap-compatible).
//
// Multiple producers (client processes) write commands via `write()`.
// Single consumer (conductor agent) reads via `read()`.
//
// Layout over `&[u8]` of capacity `C` (must be power-of-two):
//
//   Bytes [0..C)            : message data region
//   Bytes [C..C+128)        : trailer (head, tail, counters - cache-line padded)
//
// Record format (32-byte aligned):
//   [i32 msg_type | i32 record_length | payload... | padding to 32B alignment]
//
// Atomics use Acquire/Release ordering only (no SeqCst per coding rules).
// Bitmask indexing only (no modulo per coding rules).
// Zero-allocation in steady state.

use std::sync::atomic::{AtomicI64, Ordering};

// ---- Constants ----

/// Record header: [i32 msg_type | i32 record_length].
pub const RECORD_HEADER_LENGTH: usize = 8;

/// Record alignment. Bitmask-friendly (power-of-two).
const RECORD_ALIGNMENT: usize = 32;

/// Alignment mask for rounding up to RECORD_ALIGNMENT.
const ALIGNMENT_MASK: usize = !(RECORD_ALIGNMENT - 1);

/// Message type indicating a padding record (skip to end of wrap).
pub const MSG_TYPE_PADDING: i32 = -1;

/// Trailer layout: head_position at offset 0, tail_position at offset 64
/// (separate cache lines to avoid false sharing between producer and consumer).
const TRAILER_LENGTH: usize = 128;
const HEAD_POSITION_OFFSET: usize = 0;
const TAIL_POSITION_OFFSET: usize = 64; // separate cache line

// ---- Error types ----

/// Stack-only error for ring buffer operations. No heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingBufferError {
    /// Capacity is not a power-of-two or is too small.
    InvalidCapacity,
    /// Buffer slice is too small for the requested capacity + trailer.
    BufferTooSmall,
    /// Message is too large for the ring buffer.
    MessageTooLarge,
    /// Ring buffer is full (producers cannot keep up with consumer).
    Full,
}

impl std::fmt::Display for RingBufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCapacity => f.write_str("capacity must be power-of-two >= 32"),
            Self::BufferTooSmall => f.write_str("buffer too small for capacity + trailer"),
            Self::MessageTooLarge => f.write_str("message too large for ring buffer"),
            Self::Full => f.write_str("ring buffer full"),
        }
    }
}

// ---- Helpers ----

/// Align a length up to RECORD_ALIGNMENT.
#[inline]
const fn align_record(len: usize) -> usize {
    (len + RECORD_ALIGNMENT - 1) & ALIGNMENT_MASK
}

/// Total buffer size needed for a given data capacity (capacity + trailer).
pub const fn required_buffer_size(capacity: usize) -> usize {
    capacity + TRAILER_LENGTH
}

/// Read an i32 from a byte slice at the given offset (little-endian).
#[inline]
fn read_i32(buf: &[u8], offset: usize) -> i32 {
    let bytes: [u8; 4] = [
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ];
    i32::from_le_bytes(bytes)
}

/// Write an i32 to a byte slice at the given offset (little-endian).
#[inline]
fn write_i32(buf: &mut [u8], offset: usize, val: i32) {
    let bytes = val.to_le_bytes();
    buf[offset] = bytes[0];
    buf[offset + 1] = bytes[1];
    buf[offset + 2] = bytes[2];
    buf[offset + 3] = bytes[3];
}

// ---- MpscRingBuffer ----

/// MPSC ring buffer backed by a raw byte slice.
///
/// The backing memory can be heap-allocated (Vec<u8>) for testing or
/// mmap'd for cross-process IPC. The ring buffer does not own the memory.
///
/// Producers call `write(msg_type, payload)` - uses CAS on the tail.
/// Consumer calls `read(handler)` - single-threaded, no CAS needed for head.
///
/// Zero-allocation in steady state. Bitmask indexing (no modulo).
pub struct MpscRingBuffer {
    /// Pointer to the start of the backing buffer (data region + trailer).
    buffer: *mut u8,
    /// Data region capacity in bytes (power-of-two).
    capacity: usize,
    /// Bitmask: capacity - 1.
    mask: usize,
    /// Maximum record length (capacity - RECORD_HEADER_LENGTH).
    max_record_length: usize,
}

impl std::fmt::Debug for MpscRingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpscRingBuffer")
            .field("capacity", &self.capacity)
            .finish()
    }
}

// SAFETY: The ring buffer is designed for cross-thread (and cross-process)
// use. Producers and consumer access disjoint regions (producer writes
// at tail, consumer reads at head). Synchronization is via atomic
// head_position and tail_position in the trailer.
unsafe impl Send for MpscRingBuffer {}
unsafe impl Sync for MpscRingBuffer {}

impl MpscRingBuffer {
    /// Create a ring buffer over an existing byte slice.
    ///
    /// `capacity` is the data region size (must be power-of-two >= RECORD_ALIGNMENT).
    /// The buffer must be at least `required_buffer_size(capacity)` bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    /// - `buffer` points to at least `required_buffer_size(capacity)` valid bytes
    /// - The buffer memory remains valid for the lifetime of this ring buffer
    /// - For mmap'd memory: the mapping is MAP_SHARED and properly sized
    pub unsafe fn new(buffer: *mut u8, capacity: usize) -> Result<Self, RingBufferError> {
        if capacity < RECORD_ALIGNMENT || !capacity.is_power_of_two() {
            return Err(RingBufferError::InvalidCapacity);
        }
        Ok(Self {
            buffer,
            capacity,
            mask: capacity - 1,
            max_record_length: capacity - RECORD_HEADER_LENGTH,
        })
    }

    /// Data capacity in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Maximum payload size for a single message.
    #[inline]
    pub fn max_message_length(&self) -> usize {
        self.max_record_length
    }

    // ---- Atomic accessors for head/tail in the trailer ----

    #[inline]
    fn head_position_atomic(&self) -> &AtomicI64 {
        // SAFETY: trailer is within the buffer, 8-byte aligned.
        unsafe {
            &*(self.buffer.add(self.capacity + HEAD_POSITION_OFFSET) as *const AtomicI64)
        }
    }

    #[inline]
    fn tail_position_atomic(&self) -> &AtomicI64 {
        // SAFETY: trailer is within the buffer, 8-byte aligned, separate cache line.
        unsafe {
            &*(self.buffer.add(self.capacity + TAIL_POSITION_OFFSET) as *const AtomicI64)
        }
    }

    // ---- Producer (MPSC write) ----

    /// Write a message into the ring buffer.
    ///
    /// `msg_type` must be >= 0 (negative types are reserved for internal use).
    /// Returns `Ok(())` on success, `Err` if the ring is full or message too large.
    ///
    /// Thread-safe: multiple producers can call this concurrently.
    /// Uses CAS loop on tail_position (Acquire/Release, no SeqCst).
    pub fn write(&self, msg_type: i32, payload: &[u8]) -> Result<(), RingBufferError> {
        let record_length = payload.len() + RECORD_HEADER_LENGTH;
        if record_length > self.capacity {
            return Err(RingBufferError::MessageTooLarge);
        }
        let aligned_length = align_record(record_length);

        let tail_atomic = self.tail_position_atomic();
        let head_atomic = self.head_position_atomic();

        // CAS loop to claim space at the tail.
        loop {
            let tail = tail_atomic.load(Ordering::Acquire);
            let head = head_atomic.load(Ordering::Acquire);

            let available = self.capacity as i64 - (tail.wrapping_sub(head));
            if (aligned_length as i64) > available {
                return Err(RingBufferError::Full);
            }

            let new_tail = tail.wrapping_add(aligned_length as i64);

            // Check if we need to wrap around.
            let tail_index = (tail as usize) & self.mask;
            let contiguous = self.capacity - tail_index;

            if aligned_length > contiguous {
                // Not enough contiguous space at end - need to insert padding
                // and wrap to the beginning.
                let pad_length = contiguous;
                let total_needed = pad_length + aligned_length;
                let available_total = self.capacity as i64 - (tail.wrapping_sub(head));
                if (total_needed as i64) > available_total {
                    return Err(RingBufferError::Full);
                }

                let wrapped_tail = tail.wrapping_add(total_needed as i64);
                if tail_atomic
                    .compare_exchange_weak(tail, wrapped_tail, Ordering::Release, Ordering::Relaxed)
                    .is_err()
                {
                    // Another producer won - retry.
                    std::hint::spin_loop();
                    continue;
                }

                // Write padding record at tail_index.
                // SAFETY: we have exclusive access to [tail_index..tail_index+pad_length).
                unsafe {
                    let pad_ptr = self.buffer.add(tail_index);
                    write_i32(
                        std::slice::from_raw_parts_mut(pad_ptr, RECORD_HEADER_LENGTH),
                        0,
                        MSG_TYPE_PADDING,
                    );
                    write_i32(
                        std::slice::from_raw_parts_mut(pad_ptr, RECORD_HEADER_LENGTH),
                        4,
                        pad_length as i32,
                    );
                    // Commit padding: store msg_type with Release to make it visible.
                    std::sync::atomic::fence(Ordering::Release);
                }

                // Write actual record at offset 0 (wrapped).
                unsafe {
                    let rec_ptr = self.buffer;
                    let rec_slice = std::slice::from_raw_parts_mut(
                        rec_ptr,
                        aligned_length,
                    );
                    // Zero padding region first.
                    for b in rec_slice[RECORD_HEADER_LENGTH + payload.len()..].iter_mut() {
                        *b = 0;
                    }
                    // Write payload.
                    std::ptr::copy_nonoverlapping(
                        payload.as_ptr(),
                        rec_slice[RECORD_HEADER_LENGTH..].as_mut_ptr(),
                        payload.len(),
                    );
                    // Write header last (msg_type + record_length).
                    write_i32(rec_slice, 4, record_length as i32);
                    // Release fence then write msg_type to commit.
                    std::sync::atomic::fence(Ordering::Release);
                    write_i32(rec_slice, 0, msg_type);
                }

                return Ok(());
            }

            // Simple case: fits contiguously.
            if tail_atomic
                .compare_exchange_weak(tail, new_tail, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
                continue;
            }

            // Write the record.
            // SAFETY: we have exclusive access to [tail_index..tail_index+aligned_length).
            unsafe {
                let rec_ptr = self.buffer.add(tail_index);
                let rec_slice = std::slice::from_raw_parts_mut(rec_ptr, aligned_length);
                // Zero padding region.
                for b in rec_slice[RECORD_HEADER_LENGTH + payload.len()..].iter_mut() {
                    *b = 0;
                }
                // Write payload.
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    rec_slice[RECORD_HEADER_LENGTH..].as_mut_ptr(),
                    payload.len(),
                );
                // Write header: record_length first, then msg_type to commit.
                write_i32(rec_slice, 4, record_length as i32);
                std::sync::atomic::fence(Ordering::Release);
                write_i32(rec_slice, 0, msg_type);
            }

            return Ok(());
        }
    }

    // ---- Consumer (single-threaded read) ----

    /// Read all available messages, calling `handler(msg_type, payload)` for each.
    ///
    /// Returns the number of messages read.
    ///
    /// Single-threaded: only the conductor agent should call this.
    /// Zero-allocation.
    pub fn read<F>(&self, mut handler: F) -> i32
    where
        F: FnMut(i32, &[u8]),
    {
        let head_atomic = self.head_position_atomic();
        let tail_atomic = self.tail_position_atomic();

        let head = head_atomic.load(Ordering::Acquire);
        let tail = tail_atomic.load(Ordering::Acquire);

        if head == tail {
            return 0;
        }

        let mut messages_read = 0i32;
        let mut current = head;

        while current != tail {
            let index = (current as usize) & self.mask;

            // Read the record header.
            let msg_type;
            let record_length;
            unsafe {
                let rec_ptr = self.buffer.add(index);
                let hdr = std::slice::from_raw_parts(rec_ptr, RECORD_HEADER_LENGTH);
                msg_type = read_i32(hdr, 0);
                record_length = read_i32(hdr, 4) as usize;
            }

            // msg_type == 0 means the producer hasn't committed yet - stop.
            if msg_type == 0 {
                break;
            }

            let aligned = align_record(record_length);

            if msg_type != MSG_TYPE_PADDING {
                // Deliver the payload to the handler.
                let payload_len = record_length - RECORD_HEADER_LENGTH;
                unsafe {
                    let payload_ptr = self.buffer.add(index + RECORD_HEADER_LENGTH);
                    let payload = std::slice::from_raw_parts(payload_ptr, payload_len);
                    handler(msg_type, payload);
                }
                messages_read += 1;
            }

            // Clear the record header so it is not read again (zero msg_type).
            unsafe {
                let rec_ptr = self.buffer.add(index);
                write_i32(
                    std::slice::from_raw_parts_mut(rec_ptr, RECORD_HEADER_LENGTH),
                    0,
                    0,
                );
            }

            current = current.wrapping_add(aligned as i64);
        }

        // Advance head position.
        if current != head {
            head_atomic.store(current, Ordering::Release);
        }

        messages_read
    }

    /// Current size (bytes used in the ring, approximate).
    pub fn size(&self) -> usize {
        let head = self.head_position_atomic().load(Ordering::Acquire);
        let tail = self.tail_position_atomic().load(Ordering::Acquire);
        tail.wrapping_sub(head) as usize
    }
}

// ---- Helper: create a heap-backed ring buffer for testing ----

/// Allocate a ring buffer backed by a heap Vec. For testing only.
/// The Vec is returned alongside the ring buffer to keep the memory alive.
pub fn new_heap_ring_buffer(capacity: usize) -> Result<(Vec<u8>, MpscRingBuffer), RingBufferError> {
    let total = required_buffer_size(capacity);
    let mut buffer = vec![0u8; total];
    let rb = unsafe { MpscRingBuffer::new(buffer.as_mut_ptr(), capacity)? };
    Ok((buffer, rb))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CAPACITY: usize = 1024;

    #[test]
    fn create_ring_buffer() {
        let (_, rb) = new_heap_ring_buffer(TEST_CAPACITY).expect("create");
        assert_eq!(rb.capacity(), TEST_CAPACITY);
        assert_eq!(rb.max_message_length(), TEST_CAPACITY - RECORD_HEADER_LENGTH);
    }

    #[test]
    fn invalid_capacity_not_power_of_two() {
        assert_eq!(
            new_heap_ring_buffer(100).unwrap_err(),
            RingBufferError::InvalidCapacity,
        );
    }

    #[test]
    fn invalid_capacity_too_small() {
        assert_eq!(
            new_heap_ring_buffer(16).unwrap_err(),
            RingBufferError::InvalidCapacity,
        );
    }

    #[test]
    fn write_and_read_single_message() {
        let (_buf, rb) = new_heap_ring_buffer(TEST_CAPACITY).expect("create");
        let payload = [1u8, 2, 3, 4];
        rb.write(42, &payload).expect("write");

        let mut received = Vec::new();
        let count = rb.read(|msg_type, data| {
            received.push((msg_type, data.to_vec()));
        });

        assert_eq!(count, 1);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, 42);
        assert_eq!(received[0].1, &[1, 2, 3, 4]);
    }

    #[test]
    fn write_and_read_multiple_messages() {
        let (_buf, rb) = new_heap_ring_buffer(TEST_CAPACITY).expect("create");

        for i in 0..10 {
            let payload = (i as u32).to_le_bytes();
            rb.write(i + 1, &payload).expect("write");
        }

        let mut received = Vec::new();
        let count = rb.read(|msg_type, data| {
            received.push((msg_type, data.to_vec()));
        });

        assert_eq!(count, 10);
        for i in 0..10 {
            assert_eq!(received[i].0, i as i32 + 1);
            let val = u32::from_le_bytes(received[i].1[..4].try_into().unwrap());
            assert_eq!(val, i as u32);
        }
    }

    #[test]
    fn read_empty_returns_zero() {
        let (_buf, rb) = new_heap_ring_buffer(TEST_CAPACITY).expect("create");
        let count = rb.read(|_, _| { panic!("should not be called"); });
        assert_eq!(count, 0);
    }

    #[test]
    fn ring_full_returns_error() {
        // Small ring - 32 bytes capacity.
        let (_buf, rb) = new_heap_ring_buffer(32).expect("create");
        // Max payload is 32 - 8 = 24 bytes.
        let payload = [0u8; 24];
        rb.write(1, &payload).expect("first write");
        // Ring should now be full.
        assert_eq!(rb.write(1, &[0]), Err(RingBufferError::Full));
    }

    #[test]
    fn message_too_large() {
        let (_buf, rb) = new_heap_ring_buffer(64).expect("create");
        let payload = [0u8; 64]; // payload + header > capacity
        assert_eq!(rb.write(1, &payload), Err(RingBufferError::MessageTooLarge));
    }

    #[test]
    fn wrap_around() {
        // 128-byte ring.
        let (_buf, rb) = new_heap_ring_buffer(128).expect("create");
        let payload = [0xAAu8; 20]; // 20 + 8 = 28, aligned to 32.

        // Write 3 messages (3 * 32 = 96 bytes).
        rb.write(1, &payload).expect("write 1");
        rb.write(2, &payload).expect("write 2");
        rb.write(3, &payload).expect("write 3");

        // Read all 3 to free space.
        let count = rb.read(|_, _| {});
        assert_eq!(count, 3);

        // Now head is at 96, tail at 96. Write 4 more to force wrap.
        rb.write(4, &payload).expect("write 4 - last 32 bytes");
        // Next write should wrap.
        rb.write(5, &payload).expect("write 5 - wrapped");

        let mut received = Vec::new();
        let count = rb.read(|msg_type, _| {
            received.push(msg_type);
        });

        assert!(count >= 1, "should read at least 1 message after wrap");
        assert!(received.contains(&4) || received.contains(&5));
    }

    #[test]
    fn size_tracks_usage() {
        let (_buf, rb) = new_heap_ring_buffer(TEST_CAPACITY).expect("create");
        assert_eq!(rb.size(), 0);

        let payload = [0u8; 4]; // 4 + 8 = 12, aligned to 32.
        rb.write(1, &payload).expect("write");
        assert_eq!(rb.size(), 32);

        rb.write(2, &payload).expect("write");
        assert_eq!(rb.size(), 64);

        rb.read(|_, _| {});
        assert_eq!(rb.size(), 0);
    }

    #[test]
    fn concurrent_writes() {
        let total = required_buffer_size(4096);
        let mut buffer = vec![0u8; total];
        let rb = unsafe { MpscRingBuffer::new(buffer.as_mut_ptr(), 4096).unwrap() };

        // SAFETY: We keep the buffer alive and the ring buffer valid.
        // Transmute to 'static for thread sharing (test-only).
        let rb_ref: &'static MpscRingBuffer = unsafe { &*(&rb as *const MpscRingBuffer) };

        let num_producers = 4;
        let msgs_per_producer = 100;

        let handles: Vec<_> = (0..num_producers)
            .map(|producer_id| {
                std::thread::spawn(move || {
                    for i in 0..msgs_per_producer {
                        let payload = ((producer_id * 1000 + i) as u32).to_le_bytes();
                        // Retry on Full.
                        loop {
                            match rb_ref.write(1, &payload) {
                                Ok(()) => break,
                                Err(RingBufferError::Full) => {
                                    std::hint::spin_loop();
                                    continue;
                                }
                                Err(e) => panic!("unexpected error: {e}"),
                            }
                        }
                    }
                })
            })
            .collect();

        let expected_total = num_producers * msgs_per_producer;
        let mut total_received = 0;

        // Consumer reads until all messages are received.
        while total_received < expected_total {
            let count = rb_ref.read(|msg_type, data| {
                assert_eq!(msg_type, 1);
                assert_eq!(data.len(), 4);
            });
            total_received += count as usize;
            if count == 0 {
                std::hint::spin_loop();
            }
        }

        for h in handles {
            h.join().expect("producer thread");
        }

        assert_eq!(total_received, expected_total);
    }

    #[test]
    fn display_errors() {
        let e = RingBufferError::Full;
        assert!(e.to_string().contains("full"));
    }

    #[test]
    fn required_buffer_size_correct() {
        assert_eq!(required_buffer_size(1024), 1024 + 128);
    }
}

