# Session Summary: NetworkPublication Implementation Plan

**Date:** 2026-04-06  
**Duration:** ~1 interaction  
**Focus Area:** media/network_publication  
**Prerequisite:** Step 1 complete (term_buffer.rs - RawLog)

## Objectives

- [x] Analyze RawLog API surface and how NetworkPublication wraps it
- [x] Design NetworkPublication struct, offer(), sender_scan() methods
- [x] Define OfferError enum (stack-only, Copy)
- [x] Design position arithmetic (i64 position <-> term_id + term_offset)
- [x] Design term rotation and back-pressure flow control
- [x] Identify all unit test cases
- [x] Implement network_publication.rs

## Analysis

### What RawLog Provides (Step 1 - Complete)

```
RawLog::new(term_length) -> Option<Self>       // cold path, allocates once
RawLog::partition_index(term_id, init) -> usize // bitmask: wrapping_sub + & 3
RawLog::append_frame(part, off, hdr, payload)   // hot path, zero-alloc
RawLog::scan_frames(part, off, limit, emit)     // hot path, zero-alloc
RawLog::clean_partition(part)                    // cold path (term rotation)
```

### What NetworkPublication Must Provide

The gap between RawLog and SenderAgent:

| Concern | RawLog | NetworkPublication |
|---------|--------|--------------------|
| Term tracking | None (stateless) | Tracks active_term_id, initial_term_id |
| Offset tracking | None (caller passes offset) | Tracks term_offset, advances on each offer |
| Position | None | Computes i64 pub_position and sender_position |
| Term rotation | clean_partition (manual) | Auto-rotates on TermFull, cleans old partition |
| Back-pressure | Returns AppendError::TermFull | Returns OfferError::BackPressured when sender laps |
| Frame building | Caller builds DataHeader | Builds DataHeader internally from payload |
| MTU enforcement | None | Rejects payload > max_payload_length |

### Position Arithmetic (Critical)

Position is a monotonically increasing i64 encoding both term progression and offset:

```
position = (term_id.wrapping_sub(initial_term_id) as i64) * (term_length as i64)
          + (term_offset as i64)
```

Reverse (for sender_scan - derive term_id + offset from sender_position):

```
term_count = sender_position >> position_bits_to_shift   // term_length is power-of-two
term_id    = initial_term_id.wrapping_add(term_count as i32)
term_offset = (sender_position & term_length_mask as i64) as u32
```

`position_bits_to_shift = term_length.trailing_zeros()` - precomputed at construction.

### Back-Pressure Check

When the publisher gets too far ahead of the sender (unscanned data fills nearly all partitions), further writes would overwrite data the sender has not yet transmitted. This must be prevented:

```
// In offer(), before append_frame:
let max_ahead = (term_length as i64) * ((PARTITION_COUNT as i64) - 1);
if pub_position.wrapping_sub(sender_position) >= max_ahead {
    return Err(OfferError::BackPressured);
}
```

This limits the publisher to at most `(PARTITION_COUNT - 1) * term_length` bytes ahead of the sender. With 4 partitions and 64 KiB terms, that is 192 KiB.

### Term Rotation Algorithm

When `append_frame` returns `AppendError::TermFull`:

```
1. active_term_id = active_term_id.wrapping_add(1)
2. term_offset = 0
3. new_idx = partition_index(active_term_id, initial_term_id)
4. raw_log.clean_partition(new_idx)
```

Step 3-4 clean the partition we are rotating INTO. This is always safe:
- First cycle: partition is already zero from construction (no-op).
- Subsequent cycles: back-pressure guarantees the sender has fully scanned
  this partition's previous data before the publisher wraps around to reuse it.
  (With 4 partitions, the partition was last used 4 terms ago; back-pressure
  limits the publisher to 3 terms ahead of the sender.)

NOTE: An earlier design proposed "clean PARTITION_COUNT - 1 ahead" but this was
found to clean the partition the publisher just left - destroying data the
sender had not yet scanned. The "clean entering partition" strategy was adopted
instead.

After rotation, `offer()` returns `Err(OfferError::AdminAction)`. The caller (SenderAgent) retries on the next duty cycle. This matches Aeron C/Java semantics and avoids unbounded retry within offer().

## Design

### OfferError Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferError {
    BackPressured,   // sender has not caught up - retry later
    AdminAction,     // term rotated - retry immediately
    PayloadTooLarge, // exceeds mtu - DATA_HEADER_LENGTH
    InvalidState,    // should never happen (partition_index bug)
}
```

Stack-only, Copy. Matches PollError / AppendError pattern. impl Display - no format!.

### NetworkPublication Struct

```rust
pub struct NetworkPublication {
    raw_log: RawLog,
    initial_term_id: i32,
    active_term_id: i32,
    term_offset: u32,
    term_length: u32,
    term_length_mask: u32,          // term_length - 1
    position_bits_to_shift: u32,    // term_length.trailing_zeros()
    pub_position: i64,
    sender_position: i64,
    session_id: i32,
    stream_id: i32,
    mtu: u32,
    max_payload_length: u32,        // mtu - DATA_HEADER_LENGTH
}
```

All fields are scalars - no Box, no Arc, no dyn. Owned RawLog (Vec inside) allocated once at construction.

### Constructor

```rust
pub fn new(
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    term_length: u32,
    mtu: u32,
) -> Option<Self>
```

- Validates term_length via RawLog::new (power-of-two, >= 32)
- Validates mtu >= DATA_HEADER_LENGTH
- Precomputes masks and shift
- Returns None on invalid params (cold path - Option is fine)

### offer() Method

```rust
pub fn offer(&mut self, payload: &[u8]) -> Result<i64, OfferError>
```

Algorithm:
1. Guard: `payload.len() > max_payload_length` -> PayloadTooLarge
2. Guard: `pub_position - sender_position >= max_ahead` -> BackPressured
3. Build DataHeader on stack (zero-alloc)
4. `append_frame(partition_idx, term_offset, &hdr, payload)`
5. On Ok: advance term_offset, recompute pub_position, return Ok(position)
6. On TermFull: call rotate_term(), return Err(AdminAction)
7. On InvalidPartition: return Err(InvalidState)

### sender_scan() Method

```rust
pub fn sender_scan<F>(&mut self, limit: u32, mut emit: F) -> u32
where
    F: FnMut(u32, &[u8]),
```

Algorithm:
1. Derive (term_id, term_offset) from sender_position
2. Compute partition_idx
3. Clamp limit to remaining bytes in current term
4. Call raw_log.scan_frames(partition_idx, term_offset, clamped_limit, emit)
5. Advance sender_position by bytes scanned
6. Return bytes scanned
7. Does NOT cross term boundaries (caller invokes repeatedly)

### Public Accessors

All `#[inline]`, trivial field getters:
- `pub_position()`, `sender_position()`, `active_term_id()`, `term_offset()`
- `session_id()`, `stream_id()`, `initial_term_id()`, `term_length()`, `mtu()`
- `raw_log() -> &RawLog` (for future retransmit reads)

## Unit Tests Plan

| # | Test Name | What It Verifies |
|---|-----------|-----------------|
| 1 | `new_valid_params` | Construction with valid term_length (1024) and mtu (1408) returns Some |
| 2 | `new_invalid_term_length_not_power_of_two` | term_length=100 returns None |
| 3 | `new_invalid_term_length_zero` | term_length=0 returns None |
| 4 | `new_invalid_term_length_too_small` | term_length=16 (< FRAME_ALIGNMENT) returns None |
| 5 | `new_invalid_mtu_too_small` | mtu < DATA_HEADER_LENGTH returns None |
| 6 | `offer_single_frame_empty_payload` | offer(&[]) -> Ok(32), position == 32 |
| 7 | `offer_single_frame_with_payload` | offer(4 bytes) -> Ok(64), position == 64 |
| 8 | `offer_advances_position_monotonically` | 5 sequential offers return strictly increasing positions |
| 9 | `offer_payload_too_large` | payload.len() > mtu - 32 returns PayloadTooLarge |
| 10 | `offer_term_rotation_returns_admin_action` | Fill term to capacity, next offer returns AdminAction |
| 11 | `offer_after_admin_action_succeeds` | After AdminAction, retry offer succeeds with new term_id |
| 12 | `offer_term_id_increments_on_rotation` | active_term_id() increases by 1 after rotation |
| 13 | `offer_multiple_rotations_through_all_partitions` | Rotate 4+ times, verify partition_index wraps correctly |
| 14 | `offer_wrapping_term_id_near_max` | initial_term_id = i32::MAX - 1, rotate twice - verify wraps to i32::MIN |
| 15 | `offer_back_pressured` | Set sender_position=0, fill 3 terms, verify BackPressured |
| 16 | `sender_scan_empty_publication` | scan on fresh pub returns 0, emit never called |
| 17 | `sender_scan_single_frame` | offer 1 frame, scan returns 32+ bytes, emit called once |
| 18 | `sender_scan_advances_sender_position` | after scan, sender_position() == bytes scanned |
| 19 | `sender_scan_limit_respected` | offer 4 frames (128 bytes), scan with limit=64 returns 64 |
| 20 | `sender_scan_catches_up_to_pub_position` | offer N frames, scan repeatedly until sender_position == pub_position |
| 21 | `sender_scan_across_term_boundary` | offer until rotation, scan term 0, then scan term 1 |
| 22 | `position_compute_basic` | compute_position(initial, 0) == 0 |
| 23 | `position_compute_mid_term` | compute_position(initial, 512) == 512 |
| 24 | `position_compute_second_term` | compute_position(initial+1, 0) == term_length |
| 25 | `position_compute_wrapping` | initial=i32::MAX, term_id=i32::MIN -> position == term_length |
| 26 | `rotate_cleans_correct_partition` | After rotation into partition 1, verify partition 0 (3 ahead via wrap) is not yet cleaned but partition at index (1+3)&3=0 is |
| 27 | `offer_error_display` | All OfferError variants produce non-empty strings |
| 28 | `offer_error_is_copy` | Verify Clone+Copy by assigning to two variables |

## Decisions Made

| Decision | Rationale |
|----------|-----------|
| Return AdminAction (not auto-retry) on term full | Matches Aeron C/Java. Lets SenderAgent poll control msgs between retries. Avoids unbounded retry in offer(). |
| Back-pressure check: `(PARTITION_COUNT - 1) * term_length` | Prevents overwriting unscanned data. With 4 partitions, publisher can be up to 3 terms ahead. |
| Clean entering partition during rotation (not 3 ahead) | "3 ahead" with 4 partitions cleans the just-left partition, destroying unscanned sender data. Cleaning the entering partition is safe: first cycle is a no-op, subsequent cycles are protected by back-pressure. |
| Expose `raw_log() -> &RawLog` read accessor | Needed for future retransmit (NAK-driven reads from old partitions). Add now to avoid breaking API change. |
| sender_scan does not cross term boundaries | Matches Aeron C. Caller (SenderAgent) invokes repeatedly. Simplifies scan logic. |
| PayloadTooLarge as separate error variant | MTU violation is a caller bug (not transient). Distinct from BackPressured/AdminAction. |

## Files to Create/Modify

| Status | File | Description |
|--------|------|-------------|
| A | `src/media/network_publication.rs` | NetworkPublication struct, offer(), sender_scan(), OfferError |
| M | `src/media/mod.rs` | Add `pub mod network_publication;` |

## Next Steps (After This Implementation)

1. **Step 3 (High):** Refactor sender.rs - replace PublicationEntry with NetworkPublication
2. **Step 4 (Medium):** Add term_buffer_length to DriverContext with power-of-two validation
3. **Step 6 (Medium):** Extend benches/publication_offer.rs with offer() + sender_scan() benchmarks

