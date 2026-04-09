// Broadcast buffer - single producer (conductor), multiple consumers (clients).
//
// The conductor writes responses/notifications. Each client maintains its
// own read cursor and can independently consume messages.
//
// Layout over `&[u8]` of capacity `C` (must be power-of-two):
//
//   Bytes [0..C)            : message data region
//   Bytes [C..C+128)        : trailer (tail_counter, latest_counter)
//
// Record format (32-byte aligned):
//   [i32 record_length | i32 msg_type | payload... | padding to 32B alignment]
//
// Writer bumps tail_counter with Release. Readers detect lapped messages
// by comparing their cursor against tail_counter.
//
// Atomics: Acquire/Release only (no SeqCst per coding rules).
// Indexing: bitmask only (no modulo per coding rules).
// Zero-allocation in steady state.

use std::sync::atomic::{AtomicI64, Ordering};

// ---- Constants ----

/// Record header: [i32 record_length | i32 msg_type].
pub const RECORD_HEADER_LENGTH: usize = 8;

/// Record alignment. Power-of-two for bitmask.
const RECORD_ALIGNMENT: usize = 32;

/// Alignment mask.
const ALIGNMENT_MASK: usize = !(RECORD_ALIGNMENT - 1);

/// Padding record type.
pub const MSG_TYPE_PADDING: i32 = -1;

/// Trailer layout.
const TRAILER_LENGTH: usize = 128;
const TAIL_COUNTER_OFFSET: usize = 0;
const LATEST_COUNTER_OFFSET: usize = 64; // separate cache line

// ---- Error types ----

/// Stack-only error. No heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastError {
    /// Capacity is not a power-of-two or too small.
    InvalidCapacity,
    /// Message too large for the buffer.
    MessageTooLarge,
}

impl std::fmt::Display for BroadcastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCapacity => f.write_str("capacity must be power-of-two >= 32"),
            Self::MessageTooLarge => f.write_str("message too large for broadcast buffer"),
        }
    }
}

// ---- Helpers ----

#[inline]
const fn align_record(len: usize) -> usize {
    (len + RECORD_ALIGNMENT - 1) & ALIGNMENT_MASK
}

/// Total buffer size needed for a given data capacity.
pub const fn required_buffer_size(capacity: usize) -> usize {
    capacity + TRAILER_LENGTH
}

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

#[inline]
fn write_i32(buf: &mut [u8], offset: usize, val: i32) {
    let bytes = val.to_le_bytes();
    buf[offset] = bytes[0];
    buf[offset + 1] = bytes[1];
    buf[offset + 2] = bytes[2];
    buf[offset + 3] = bytes[3];
}

// ---- BroadcastTransmitter (driver/conductor side) ----

/// Single-producer broadcast writer. Owned by the conductor agent.
///
/// Writes messages that all connected clients can read. Overwrites the
/// oldest messages when the buffer wraps (lapped readers detect this
/// via tail_counter).
pub struct BroadcastTransmitter {
    buffer: *mut u8,
    capacity: usize,
    mask: usize,
    max_record_length: usize,
}

impl std::fmt::Debug for BroadcastTransmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastTransmitter")
            .field("capacity", &self.capacity)
            .finish()
    }
}

unsafe impl Send for BroadcastTransmitter {}

impl BroadcastTransmitter {
    /// Create a transmitter over an existing byte buffer.
    ///
    /// # Safety
    ///
    /// Same requirements as `MpscRingBuffer::new`.
    pub unsafe fn new(buffer: *mut u8, capacity: usize) -> Result<Self, BroadcastError> {
        if capacity < RECORD_ALIGNMENT || !capacity.is_power_of_two() {
            return Err(BroadcastError::InvalidCapacity);
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

    #[inline]
    fn tail_counter_atomic(&self) -> &AtomicI64 {
        unsafe {
            &*(self.buffer.add(self.capacity + TAIL_COUNTER_OFFSET) as *const AtomicI64)
        }
    }

    #[inline]
    fn latest_counter_atomic(&self) -> &AtomicI64 {
        unsafe {
            &*(self.buffer.add(self.capacity + LATEST_COUNTER_OFFSET) as *const AtomicI64)
        }
    }

    /// Write a message into the broadcast buffer.
    ///
    /// Single-threaded: only the conductor calls this.
    /// Overwrites old data when wrapping (readers detect via tail_counter).
    pub fn transmit(&self, msg_type: i32, payload: &[u8]) -> Result<(), BroadcastError> {
        let record_length = payload.len() + RECORD_HEADER_LENGTH;
        if record_length > self.capacity {
            return Err(BroadcastError::MessageTooLarge);
        }
        let aligned_length = align_record(record_length);

        let tail_atomic = self.tail_counter_atomic();
        let latest_atomic = self.latest_counter_atomic();

        let tail = tail_atomic.load(Ordering::Acquire);
        let tail_index = (tail as usize) & self.mask;

        let contiguous = self.capacity - tail_index;

        if aligned_length > contiguous {
            // Insert padding to wrap.
            unsafe {
                let pad_ptr = self.buffer.add(tail_index);
                let pad_slice = std::slice::from_raw_parts_mut(pad_ptr, RECORD_HEADER_LENGTH);
                write_i32(pad_slice, 0, contiguous as i32);
                write_i32(pad_slice, 4, MSG_TYPE_PADDING);
            }
            let new_tail = tail.wrapping_add(contiguous as i64);
            tail_atomic.store(new_tail, Ordering::Release);

            // Write actual record at index 0.
            self.write_record(0, msg_type, payload, record_length, aligned_length);
            let final_tail = new_tail.wrapping_add(aligned_length as i64);
            latest_atomic.store(new_tail, Ordering::Release);
            tail_atomic.store(final_tail, Ordering::Release);
        } else {
            // Fits contiguously.
            self.write_record(tail_index, msg_type, payload, record_length, aligned_length);
            latest_atomic.store(tail, Ordering::Release);
            let new_tail = tail.wrapping_add(aligned_length as i64);
            tail_atomic.store(new_tail, Ordering::Release);
        }

        Ok(())
    }

    fn write_record(
        &self,
        index: usize,
        msg_type: i32,
        payload: &[u8],
        record_length: usize,
        aligned_length: usize,
    ) {
        unsafe {
            let rec_ptr = self.buffer.add(index);
            let rec_slice = std::slice::from_raw_parts_mut(rec_ptr, aligned_length);
            // Zero padding.
            for b in rec_slice[RECORD_HEADER_LENGTH + payload.len()..].iter_mut() {
                *b = 0;
            }
            // Write payload.
            if !payload.is_empty() {
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    rec_slice[RECORD_HEADER_LENGTH..].as_mut_ptr(),
                    payload.len(),
                );
            }
            // Write header: [record_length | msg_type].
            write_i32(rec_slice, 0, record_length as i32);
            write_i32(rec_slice, 4, msg_type);
        }
    }

    /// Current tail counter value. Useful for initializing new receivers.
    pub fn tail_counter(&self) -> i64 {
        self.tail_counter_atomic().load(Ordering::Acquire)
    }
}

// ---- BroadcastReceiver (client side) ----

/// Per-client broadcast reader. Each client creates its own receiver.
///
/// Tracks a local cursor and detects lapped conditions (when the transmitter
/// has overwritten messages the receiver hasn't read yet).
pub struct BroadcastReceiver {
    buffer: *const u8,
    capacity: usize,
    mask: usize,
    cursor: i64,
    /// Number of times this receiver was lapped (for diagnostics).
    lapped_count: u64,
}

unsafe impl Send for BroadcastReceiver {}

impl BroadcastReceiver {
    /// Create a receiver. `cursor` should be the transmitter's current
    /// `tail_counter` at the time of connection (so the receiver starts
    /// reading from the latest position and doesn't see old messages).
    ///
    /// # Safety
    ///
    /// The buffer must remain valid and be the same buffer used by the
    /// corresponding `BroadcastTransmitter`.
    pub unsafe fn new(buffer: *const u8, capacity: usize, cursor: i64) -> Self {
        Self {
            buffer,
            capacity,
            mask: capacity - 1,
            cursor,
            lapped_count: 0,
        }
    }

    #[inline]
    fn tail_counter_atomic(&self) -> &AtomicI64 {
        unsafe {
            &*(self.buffer.add(self.capacity + TAIL_COUNTER_OFFSET) as *const AtomicI64)
        }
    }

    /// Read available messages, calling `handler(msg_type, payload)` for each.
    ///
    /// Returns the number of messages delivered.
    /// If lapped (transmitter overwrote unread data), skips ahead to latest
    /// and increments lapped_count.
    pub fn receive<F>(&mut self, mut handler: F) -> i32
    where
        F: FnMut(i32, &[u8]),
    {
        let tail = self.tail_counter_atomic().load(Ordering::Acquire);

        if self.cursor == tail {
            return 0;
        }

        // Detect lapped condition: if tail has advanced by more than capacity,
        // the transmitter has overwritten our unread messages.
        let behind = tail.wrapping_sub(self.cursor);
        if behind > self.capacity as i64 {
            self.lapped_count += 1;
            // Jump to latest readable position.
            self.cursor = tail.wrapping_sub(self.capacity as i64);
        }

        let mut messages_read = 0i32;
        let mut current = self.cursor;

        while current != tail {
            let index = (current as usize) & self.mask;

            let record_length;
            let msg_type;
            unsafe {
                let rec_ptr = self.buffer.add(index);
                let hdr = std::slice::from_raw_parts(rec_ptr, RECORD_HEADER_LENGTH);
                record_length = read_i32(hdr, 0) as usize;
                msg_type = read_i32(hdr, 4);
            }

            if record_length == 0 {
                break; // Not yet committed.
            }

            let aligned = align_record(record_length);

            if msg_type != MSG_TYPE_PADDING {
                let payload_len = record_length - RECORD_HEADER_LENGTH;
                unsafe {
                    let payload_ptr = self.buffer.add(index + RECORD_HEADER_LENGTH);
                    let payload = std::slice::from_raw_parts(payload_ptr, payload_len);
                    handler(msg_type, payload);
                }
                messages_read += 1;
            }

            current = current.wrapping_add(aligned as i64);
        }

        self.cursor = current;
        messages_read
    }

    /// Number of times this receiver was lapped by the transmitter.
    pub fn lapped_count(&self) -> u64 {
        self.lapped_count
    }
}

// ---- Test helper ----

/// Create a heap-backed broadcast buffer pair for testing.
pub fn new_heap_broadcast(
    capacity: usize,
) -> Result<(Vec<u8>, BroadcastTransmitter), BroadcastError> {
    let total = required_buffer_size(capacity);
    let mut buffer = vec![0u8; total];
    let tx = unsafe { BroadcastTransmitter::new(buffer.as_mut_ptr(), capacity)? };
    Ok((buffer, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CAPACITY: usize = 1024;

    #[test]
    fn create_broadcast() {
        let (_, tx) = new_heap_broadcast(TEST_CAPACITY).expect("create");
        assert_eq!(tx.capacity(), TEST_CAPACITY);
    }

    #[test]
    fn invalid_capacity() {
        assert_eq!(
            new_heap_broadcast(100).unwrap_err(),
            BroadcastError::InvalidCapacity,
        );
    }

    #[test]
    fn transmit_and_receive() {
        let (buf, tx) = new_heap_broadcast(TEST_CAPACITY).expect("create");
        let cursor = tx.tail_counter();
        let mut rx = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), TEST_CAPACITY, cursor)
        };

        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        tx.transmit(42, &payload).expect("transmit");

        let mut received = Vec::new();
        let count = rx.receive(|msg_type, data| {
            received.push((msg_type, data.to_vec()));
        });

        assert_eq!(count, 1);
        assert_eq!(received[0].0, 42);
        assert_eq!(received[0].1, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn transmit_multiple() {
        let (buf, tx) = new_heap_broadcast(TEST_CAPACITY).expect("create");
        let cursor = tx.tail_counter();
        let mut rx = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), TEST_CAPACITY, cursor)
        };

        for i in 0..10 {
            let payload = (i as u32).to_le_bytes();
            tx.transmit(i + 1, &payload).expect("transmit");
        }

        let mut received = Vec::new();
        let count = rx.receive(|msg_type, data| {
            received.push((msg_type, data.to_vec()));
        });

        assert_eq!(count, 10);
        for i in 0..10 {
            assert_eq!(received[i].0, i as i32 + 1);
        }
    }

    #[test]
    fn receive_empty() {
        let (buf, tx) = new_heap_broadcast(TEST_CAPACITY).expect("create");
        let cursor = tx.tail_counter();
        let mut rx = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), TEST_CAPACITY, cursor)
        };

        let count = rx.receive(|_, _| panic!("should not be called"));
        assert_eq!(count, 0);
    }

    #[test]
    fn multiple_receivers() {
        let (buf, tx) = new_heap_broadcast(TEST_CAPACITY).expect("create");
        let cursor = tx.tail_counter();

        let mut rx1 = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), TEST_CAPACITY, cursor)
        };
        let mut rx2 = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), TEST_CAPACITY, cursor)
        };

        tx.transmit(1, &[10, 20]).expect("transmit");

        let count1 = rx1.receive(|msg_type, data| {
            assert_eq!(msg_type, 1);
            assert_eq!(data, &[10, 20]);
        });
        let count2 = rx2.receive(|msg_type, data| {
            assert_eq!(msg_type, 1);
            assert_eq!(data, &[10, 20]);
        });

        assert_eq!(count1, 1);
        assert_eq!(count2, 1);
    }

    #[test]
    fn message_too_large() {
        let (_, tx) = new_heap_broadcast(64).expect("create");
        let payload = [0u8; 64];
        assert_eq!(tx.transmit(1, &payload), Err(BroadcastError::MessageTooLarge));
    }

    #[test]
    fn receiver_detects_lapped() {
        // Small buffer - 64 bytes capacity. Can hold ~1-2 messages.
        let (buf, tx) = new_heap_broadcast(64).expect("create");
        let cursor = tx.tail_counter();
        let mut rx = unsafe {
            BroadcastReceiver::new(buf.as_ptr(), 64, cursor)
        };

        // Write enough messages to lap the receiver.
        let payload = [0xAAu8; 20]; // 20 + 8 = 28, aligned to 32.
        for _ in 0..10 {
            tx.transmit(1, &payload).expect("transmit");
        }

        // Receiver should detect it was lapped.
        let _ = rx.receive(|_, _| {});
        assert!(rx.lapped_count() > 0, "receiver should detect being lapped");
    }

    #[test]
    fn display_errors() {
        let e = BroadcastError::InvalidCapacity;
        assert!(e.to_string().contains("power-of-two"));
    }
}

