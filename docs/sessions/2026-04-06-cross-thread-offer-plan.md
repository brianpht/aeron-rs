# Session Summary: Cross-Thread Offer with AtomicI64 pub_position

**Date:** 2026-04-06
**Duration:** Planning session
**Focus Area:** media/network_publication, media/term_buffer, agent/sender
**Prerequisite:** [Term Buffer Write Path](2026-04-06-term-buffer-write-path-plan.md) (steps 1-6 complete)

## Objective

Split `NetworkPublication` into a publisher handle (`ConcurrentPublication`) and a
sender-side view (`SenderPublication`) so that an application thread calls
`offer(payload)` while the `SenderAgent` thread runs `sender_scan()` - coordinated
entirely via `Release`/`Acquire` atomics on `pub_position`, `sender_position`, and
the in-buffer `frame_length` commit word.

No Mutex. No SeqCst. Zero allocation in steady state.

## Current State (Single-Threaded)

```
Application thread                SenderAgent thread
       |                                |
       |  (same thread)                 |
       v                                v
  offer(&mut self)            sender_scan(&mut self)
       |                                |
       +---- pub_position: i64 ---------+
       +---- sender_position: i64 ------+
       +---- raw_log: RawLog -----------+
```

`NetworkPublication` holds all state as plain `i64` fields and `RawLog` with
`&mut self` access. Everything is single-threaded - the caller of `offer()` and
the sender agent must be on the same thread.

## Target State (Cross-Thread)

```
Application thread                SenderAgent thread
       |                                |
       v                                v
  ConcurrentPublication          SenderPublication
  (owns &mut cursor state)       (owns &mut scan state)
       |                                |
       +------- Arc<PublicationInner> ---+
       |        - SharedLogBuffer       |
       |        - pub_position: AtomicI64
       |        - sender_position: AtomicI64
       |        - immutable config      |
       |                                |
  offer(&mut self)            sender_scan(&mut self)
  -> Release store            -> Acquire load frame_length
     frame_length                Release store sender_position
  -> Release store
     pub_position
```

## Design

### A. Shared State Model

**`PublicationInner`** (wrapped in `Arc`, allocated once at construction):

```rust
struct PublicationInner {
    log: SharedLogBuffer,          // UnsafeCell<Vec<u8>>, allocated once
    pub_position: AtomicI64,       // written by publisher, read by sender
    sender_position: AtomicI64,    // written by sender, read by publisher
    // Immutable after construction (no atomics needed):
    initial_term_id: i32,
    term_length: u32,
    term_length_mask: u32,
    position_bits_to_shift: u32,
    session_id: i32,
    stream_id: i32,
    mtu: u32,
    max_payload_length: u32,
}
```

**`SharedLogBuffer`** (new wrapper in term_buffer.rs):

```rust
struct SharedLogBuffer {
    buffer: UnsafeCell<Vec<u8>>,   // allocated once, never resized
    term_length: u32,
}

// SAFETY: Single writer per partition at a time. Readers only read
// committed frames (guarded by Acquire load of frame_length).
// Back-pressure guarantees writer and reader never access the same
// partition simultaneously for writes.
unsafe impl Send for SharedLogBuffer {}
unsafe impl Sync for SharedLogBuffer {}
```

**`ConcurrentPublication`** (publisher handle, moved to application thread):

```rust
pub struct ConcurrentPublication {
    inner: Arc<PublicationInner>,
    // Publisher-local mutable state (not shared):
    active_term_id: i32,
    term_offset: u32,
    pub_position_local: i64,       // cached copy, avoids atomic load
}
```

Takes `&mut self` - enforces single publisher at compile time.
Implements `Send` (movable to another thread) but NOT `Clone`.

**`SenderPublication`** (sender-side view, owned by SenderAgent):

```rust
pub struct SenderPublication {
    inner: Arc<PublicationInner>,
    // Sender-local mutable state:
    sender_position_local: i64,    // cached copy, avoids atomic load
}
```

Takes `&mut self` for `sender_scan`.
Implements `Send`.

### B. Atomic Frame-Length Commit Protocol

The `frame_length` field (first 4 bytes of every frame in the term buffer)
serves as the publish barrier - matching Aeron Java's `putOrdered` /
`getVolatile` pattern on the frame length word.

**Publisher side** (`ConcurrentPublication::offer`):

```
1. Check back-pressure:
     sender_pos = inner.sender_position.load(Acquire)
     if pub_position_local.wrapping_sub(sender_pos) >= max_ahead -> BackPressured

2. Compute partition_idx = (active_term_id.wrapping_sub(initial_term_id) as u32) & 3
   Compute base = partition_idx * term_length + term_offset

3. Write header bytes [4..32] (version, flags, type, term_offset, session, stream, term_id)
   Write payload bytes [32..32+len]
   (All via ptr::copy_nonoverlapping through UnsafeCell pointer)
   NOTE: frame_length at [base..base+4] is still 0 at this point

4. COMMIT: atomic_frame_length_store(base, frame_length, Release)
   This is the publish barrier. All preceding writes become visible
   to any thread that reads this word with Acquire.

5. Advance local cursor:
     term_offset += aligned_frame_length
     pub_position_local = compute_position(active_term_id, term_offset)
     inner.pub_position.store(pub_position_local, Release)

6. If term full: rotate_term() - same logic as today
```

**Sender side** (`SenderPublication::sender_scan`):

```
1. Derive (term_id, term_offset) from sender_position_local

2. For each frame at current offset:
     frame_length = atomic_frame_length_load(base + offset, Acquire)
     if frame_length <= 0 -> stop (not yet committed)
     Read frame data [offset..offset+frame_length] (safe: Acquire pairs with Release)
     Call emit(term_offset, frame_slice)
     Advance offset by aligned_frame_length

3. Update sender position:
     sender_position_local += scanned_bytes
     inner.sender_position.store(sender_position_local, Release)
```

### C. Atomic Helpers (term_buffer.rs)

```rust
/// Release-ordered store of frame_length (little-endian i32) at `offset`.
/// Used by publisher to commit a frame.
///
/// SAFETY: `ptr` must point to a valid, aligned-to-4 offset within
/// the SharedLogBuffer. Caller ensures exclusive write access to this
/// frame slot.
#[inline]
unsafe fn atomic_frame_length_store(ptr: *mut u8, offset: usize, value: i32) {
    let target = ptr.add(offset) as *const AtomicI32;
    // AtomicI32::from_ptr requires the pointer to be naturally aligned (4 bytes).
    // Frame offsets are always 32-byte aligned, so this holds.
    (*target).store(value, Ordering::Release);
}

/// Acquire-ordered load of frame_length (little-endian i32) at `offset`.
/// Used by sender to check if a frame is committed.
#[inline]
unsafe fn atomic_frame_length_load(ptr: *const u8, offset: usize) -> i32 {
    let target = ptr.add(offset) as *const AtomicI32;
    (*target).load(Ordering::Acquire)
}
```

Note: `AtomicI32::from_ptr` is stable since Rust 1.75. Edition 2024 requires
>= 1.85, so this is available. Little-endian assumption is already enforced
by the `compile_error!` guard in frame.rs.

### D. Memory Ordering Justification

| Operation | Ordering | Justification |
|-----------|----------|---------------|
| Publisher: `frame_length` commit write | `Release` | All preceding header+payload bytes become visible to any thread that reads this `frame_length` with `Acquire` |
| Scanner: `frame_length` read | `Acquire` | Pairs with publisher's `Release` - if scanner sees `frame_length > 0`, all payload/header bytes are guaranteed visible |
| Publisher: `pub_position.store` | `Release` | Enables external observers to query progress without scanning the buffer |
| Publisher: `sender_position.load` | `Acquire` | Pairs with scanner's `Release` store - publisher sees accurate freed space for back-pressure |
| Scanner: `sender_position.store` | `Release` | Makes freed-space visible to publisher thread for back-pressure check |
| Scanner: `sender_position.load` (own value) | `Relaxed` | Only the sender thread writes this - no cross-thread sync needed for self-reads |
| Scanner: `pub_position.load` | Not used | Scanner does not read `pub_position` - it stops when `frame_length == 0` in the buffer |

### E. Term Rotation and Partition Cleaning

Term rotation in `ConcurrentPublication::offer` follows the same logic as
the current `rotate_term()`:
1. Increment `active_term_id` (wrapping)
2. Reset `term_offset = 0`
3. Clean the entering partition (write zeros through `SharedLogBuffer` pointer)

Cleaning is safe because back-pressure guarantees the sender has already
fully scanned the partition being cleaned:
- 4 partitions, publisher max 3 terms ahead of sender
- The entering partition was last used 4 terms ago
- Sender is at most 3 terms behind, so it has finished scanning the
  previous contents of this partition

### F. Back-Pressure

Identical to current design, but using atomics:

```rust
let max_ahead = term_length as i64 * (PARTITION_COUNT as i64 - 1);
let sender_pos = inner.sender_position.load(Ordering::Acquire);
if pub_position_local.wrapping_sub(sender_pos) >= max_ahead {
    return Err(OfferError::BackPressured);
}
```

The publisher caches `pub_position_local` as a plain `i64` (single writer -
no need to reload from the atomic). The sender position is loaded with
`Acquire` to see the latest value stored by the sender thread.

### G. API Surface

**Construction** (cold path - allocation happens here):

```rust
/// Create a concurrent publication pair.
/// Returns (publisher_handle, sender_handle).
/// The publisher handle is moved to the application thread.
/// The sender handle is stored in the SenderAgent.
pub fn new_concurrent(
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    term_length: u32,
    mtu: u32,
) -> Option<(ConcurrentPublication, SenderPublication)> { ... }
```

**SenderAgent integration**:

```rust
impl SenderAgent {
    /// Add a concurrent publication. Returns the publisher handle
    /// to be moved to the application thread.
    pub fn add_concurrent_publication(
        &mut self,
        endpoint_idx: usize,
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> Option<ConcurrentPublication> {
        let (pub_handle, sender_view) = new_concurrent(...)?;
        self.publications.push(PublicationEntry::Concurrent {
            sender_pub: sender_view,
            endpoint_idx,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
        });
        Some(pub_handle)
    }
}
```

### H. What Does NOT Change

- Wire protocol - unchanged
- RawLog's 4-partition layout (ADR-001) - unchanged
- SenderAgent duty cycle structure - unchanged
- Frame alignment (32 bytes) - unchanged
- Single-threaded `NetworkPublication` - kept as-is for same-thread use
- `term_buffer.rs` existing `RawLog` type - kept as-is
- `OfferError` enum - reused as-is

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| Single publisher only (`&mut self`) | Matches Aeron `ExclusivePublication`. Multi-publisher would need CAS on `term_offset` - separate design |
| `Arc<PublicationInner>` for sharing | One allocation at construction. `Arc` clone is cold-path only (during setup) |
| `UnsafeCell<Vec<u8>>` for buffer | Avoids double-indirection. Single alloc. `Vec` never resized. Safety via documented invariants |
| `frame_length` as commit word | Matches Aeron Java's `putOrdered` / `getVolatile` pattern. Single atomic per frame - minimal overhead |
| Keep `NetworkPublication` unchanged | Additive design. No regression for single-threaded users. Concurrent types are opt-in |
| Publisher caches `pub_position_local` | Avoids atomic load on every offer. Single writer - local copy is always accurate |
| Scanner does not read `pub_position` | Scanner uses `frame_length == 0` as the stop signal. More precise than position comparison. One fewer atomic per scan iteration |

## Implementation Phases

### Phase 1: SharedLogBuffer + atomic helpers

- [x] Add `SharedLogBuffer` wrapper with `UnsafeCell<Vec<u8>>` to term_buffer.rs
- [x] Add `atomic_frame_length_store` / `atomic_frame_length_load` helpers
- [x] Add `align_frame_length` helper and `partition_index` free function
- [x] Unit tests for atomic helpers (store then load roundtrip, zero-init)
- [x] Unit tests for SharedLogBuffer (constructor, partition_slice, clean)

### Phase 2: PublicationInner + ConcurrentPublication + SenderPublication

- [x] Create `src/media/concurrent_publication.rs`
- [x] Implement `PublicationInner`, `ConcurrentPublication::offer`, `SenderPublication::sender_scan`
- [x] Unit tests: 24 tests (constructor, offer, rotation, back-pressure, scan, wrapping, accessors, Send checks)

### Phase 3: SenderAgent integration

- [x] Convert `PublicationEntry` to enum (Local | Concurrent) with dispatch methods
- [x] Add `add_concurrent_publication()` method returning `ConcurrentPublication` handle
- [x] Update `do_send()` and `process_sm_and_nak()` to use enum dispatch
- [x] All 9 existing integration tests pass unchanged

### Phase 4: Cross-thread tests

- [x] `tests/concurrent_offer.rs`: 5 tests including cross-thread stress test
- [x] `sustained_throughput_no_corruption`: 10,000 messages with checksum verification across 2 threads
- [x] `sender_agent_concurrent_publication`: full SenderAgent integration with concurrent publication

## Files Created/Modified

| Status | File | Lines | Description |
|--------|------|-------|-------------|
| A | `src/media/concurrent_publication.rs` | 860 | `ConcurrentPublication`, `SenderPublication`, `PublicationInner`, 24 unit tests |
| M | `src/media/term_buffer.rs` | +160 | `SharedLogBuffer`, `atomic_frame_length_store/load`, `align_frame_length`, `partition_index`, 14 new tests |
| M | `src/media/mod.rs` | +1 | Add `pub mod concurrent_publication;` |
| M | `src/agent/sender.rs` | ~380 (rewrite) | `PublicationEntry` enum dispatch, `add_concurrent_publication()` |
| A | `tests/concurrent_offer.rs` | 190 | 5 cross-thread integration tests (including 10K-msg stress test) |

## Test Plan

| Test | Type | Thread Model | Validates |
|------|------|-------------|-----------|
| `offer_scan_roundtrip_concurrent` | Unit | Single | Basic API works through atomic path |
| `back_pressure_concurrent` | Unit | Single | Atomic sender_position load gates publisher |
| `term_rotation_concurrent` | Unit | Single | Partition clean + rotate via shared buffer |
| `wrapping_term_id_concurrent` | Unit | Single | i32 wrap at MAX/MIN boundary |
| `frame_commit_visible_cross_thread` | Integration | 2 threads | Publisher Release -> Scanner Acquire sees frame |
| `sustained_throughput_no_corruption` | Stress | 2 threads | Checksum payload over 1M messages |
| `back_pressure_blocks_publisher` | Integration | 2 threads | Publisher spins on BackPressured until sender catches up |

## Open Questions

1. **Should `ConcurrentPublication` support `try_claim` + `commit` (two-phase)?**
   Aeron Java offers this for zero-copy writes (claim buffer region, write in place,
   then commit). Adds complexity but avoids one memcpy. Defer to follow-up.

2. **IPC via mmap in the future?**
   `SharedLogBuffer` wraps `UnsafeCell<Vec<u8>>` today. For IPC with external
   Aeron clients, the backing buffer would be `mmap`-ed. The `unsafe fn buffer_ptr()`
   surface abstracts this - swap in mmap later without API changes.

3. **Multiple publisher threads?**
   Current design is single-publisher (`&mut self`). Multi-publisher would need
   CAS on term_offset (like Aeron's `ConcurrentPublication` vs `ExclusivePublication`).
   Out of scope - separate design if needed.

## Next Steps (after this plan is implemented)

- **Step 8:** Design RetransmitHandler for NAK-driven retransmission from term buffer
- **Future:** IPC-capable mmap-backed SharedLogBuffer
- **Future:** Multi-publisher CAS-based ConcurrentPublication (Aeron non-exclusive model)

