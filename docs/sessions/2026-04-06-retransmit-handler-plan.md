# Session Summary: RetransmitHandler Plan

**Date:** 2026-04-06
**Duration:** ~1 interaction
**Focus Area:** media/retransmit_handler, agent/sender
**Implementation Date:** 2026-04-07

## Objectives

- [x] Design RetransmitHandler struct with pre-sized flat array of actions
- [x] Design on_nak() API for NAK scheduling with dedup and delay
- [x] Design process_timeouts() API for DELAY->RETRANSMIT->LINGER lifecycle
- [x] Design integration into SenderAgent duty cycle (process_sm_and_nak + do_retransmit)
- [x] Add scan_term_at() to SenderPublication for concurrent retransmit reads
- [x] Add retransmit_scan() to PublicationEntry via enum dispatch
- [x] Produce implementation plan with test and benchmark coverage

## Design Overview

```
            NAK arrives                  Delay expires            Linger expires
drain_naks ---------> on_nak() ---------> process_timeouts ---------> [Inactive]
                      |                   |
                      v                   v
                [Delay action]   read term buffer + send_data()
                (flat array)     then transition to [Linger]
```

One `RetransmitHandler` per `SenderAgent` (not per-publication). Pre-sized
flat array of `RetransmitAction` slots. Zero allocation in steady state.

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| Per-agent RetransmitHandler, not per-publication | Matches Aeron C (one handler per sender). Simpler lifetime - single flat array shared across all publications. NAK volume is bounded by network, not publication count. | N/A |
| Fixed-size `[RetransmitAction; 64]` | No allocation in hot path. 64 matches Aeron C default. Each action is ~40 bytes, total ~2.5 KiB on stack. | N/A |
| Two-state model: Delay then Linger (matching Aeron C) | Delay coalesces duplicate NAKs. Linger prevents re-triggering same range too soon after retransmit. | N/A |
| Add `retransmit_unicast_linger_ns` to DriverContext (default 60ms) | Needed for linger timeout. `retransmit_unicast_delay_ns` already exists (default 0). | N/A |
| `PublicationEntry::retransmit_scan()` via enum dispatch | Avoids dyn trait. Local delegates to `RawLog::scan_frames`. Concurrent delegates to new `SenderPublication::scan_term_at()`. | N/A |
| Destructure self in `process_sm_and_nak()` to fix borrow issue | Same pattern already used in `do_send()`. Enables closure to access `publications` + `retransmit_handler` while iterating `endpoints`. | N/A |
| Exact (term_id, term_offset) match for dedup (not range overlap) | Matches Aeron C semantics. Range-overlap coalescing is a future optimization. | N/A |
| `process_sm_and_nak` signature changes to accept `now_ns: i64` | `on_nak` needs current time to compute delay expiry. `now_ns` is already available from cached clock in `do_work()`. | N/A |

## Plan Design

### 1. New Types - `src/media/retransmit_handler.rs`

```rust
const MAX_RETRANSMIT_ACTIONS: usize = 64;

/// State machine for a single retransmit action.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RetransmitState {
    /// Slot is free.
    Inactive,
    /// Waiting for delay to elapse before retransmitting.
    /// Coalesces duplicate NAKs for the same range.
    Delay { expiry_ns: i64 },
    /// Retransmit completed. Waiting for linger to prevent
    /// re-retransmitting the same range too quickly.
    Linger { expiry_ns: i64 },
}

/// One pending retransmit action. Stack-only, Copy.
#[derive(Clone, Copy)]
struct RetransmitAction {
    session_id: i32,
    stream_id: i32,
    term_id: i32,
    term_offset: i32,
    length: i32,
    state: RetransmitState,
}

/// NAK-driven retransmit scheduler. Pre-sized flat array, zero allocation.
pub struct RetransmitHandler {
    actions: [RetransmitAction; MAX_RETRANSMIT_ACTIONS],
    delay_ns: i64,
    linger_ns: i64,
}
```

### 2. RetransmitHandler API

```rust
impl RetransmitHandler {
    /// Cold path - construct with delay and linger intervals.
    pub fn new(delay_ns: i64, linger_ns: i64) -> Self;

    /// Schedule a retransmit from a received NAK.
    ///
    /// Returns true if a new Delay action was created.
    /// Returns false if:
    /// - A matching (session, stream, term_id, offset) action already
    ///   exists in Delay or Linger state (dedup)
    /// - No free slot available (table full, NAK dropped)
    ///
    /// Zero-allocation, O(MAX_RETRANSMIT_ACTIONS). Hot path.
    pub fn on_nak(&mut self, nak: &NakHeader, now_ns: i64) -> bool;

    /// Process all actions: fire expired Delays, expire Lingers.
    ///
    /// For each Delay past expiry: calls `on_retransmit(session_id,
    /// stream_id, term_id, term_offset, length)` and transitions
    /// to Linger state.
    ///
    /// For each Linger past expiry: transitions to Inactive (frees slot).
    ///
    /// Zero-allocation, O(MAX_RETRANSMIT_ACTIONS). Hot path.
    pub fn process_timeouts<F>(&mut self, now_ns: i64, on_retransmit: F)
    where
        F: FnMut(i32, i32, i32, i32, i32);

    /// Number of currently active actions (Delay + Linger).
    pub fn active_count(&self) -> usize;
}
```

### 3. SenderPublication::scan_term_at() - `src/media/concurrent_publication.rs`

New method for reading frames at an arbitrary position (for retransmit).
Does NOT advance sender_position. Uses Acquire-ordered frame_length loads.

```rust
impl SenderPublication {
    /// Read committed frames from a specific term position.
    /// For retransmit use - does not advance sender_position.
    ///
    /// Uses Acquire-ordered atomic loads to detect committed frames
    /// in the SharedLogBuffer (same protocol as sender_scan).
    ///
    /// Returns total bytes of frames emitted.
    pub fn scan_term_at<F>(
        &self,
        partition_idx: usize,
        offset: u32,
        limit: u32,
        emit: F,
    ) -> u32
    where
        F: FnMut(u32, &[u8]);
}
```

### 4. PublicationEntry changes - `src/agent/sender.rs`

New methods via enum dispatch:

```rust
impl PublicationEntry {
    /// Read frames from term buffer for retransmit (enum dispatch, no dyn).
    #[inline]
    fn retransmit_scan<F>(
        &self,
        term_id: i32,
        offset: u32,
        limit: u32,
        emit: F,
    ) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let initial = self.initial_term_id();
        let part_idx = RawLog::partition_index(term_id, initial);
        match self {
            PublicationEntry::Local { publication, .. } => {
                publication.raw_log().scan_frames(part_idx, offset, limit, emit)
            }
            PublicationEntry::Concurrent { sender_pub, .. } => {
                sender_pub.scan_term_at(part_idx, offset, limit, emit)
            }
        }
    }

    /// Compute absolute position for validation.
    #[inline]
    fn compute_position(&self, term_id: i32, term_offset: u32) -> i64;

    /// Sender position (highest scanned byte).
    #[inline]
    fn sender_position(&self) -> i64;

    /// Pub position (highest published byte).
    #[inline]
    fn pub_position(&self) -> i64;
}
```

### 5. SenderAgent integration - `src/agent/sender.rs`

**New field:**
```rust
pub struct SenderAgent {
    // ...existing fields...
    retransmit_handler: RetransmitHandler,
}
```

**Constructor change:**
```rust
// In SenderAgent::new():
retransmit_handler: RetransmitHandler::new(
    ctx.retransmit_unicast_delay_ns,
    ctx.retransmit_unicast_linger_ns,
),
```

**Fix `process_sm_and_nak` - disjoint borrow + pass now_ns:**
```rust
fn process_sm_and_nak(&mut self, now_ns: i64) {
    // Destructure self for disjoint field borrows.
    let publications = &mut self.publications;
    let endpoints = &mut self.endpoints;
    let retransmit_handler = &mut self.retransmit_handler;

    for ep in endpoints.iter_mut() {
        ep.drain_sm(|sm| {
            for pub_entry in publications.iter_mut() {
                if pub_entry.session_id() == sm.session_id
                    && pub_entry.stream_id() == sm.stream_id
                {
                    pub_entry.set_needs_setup(false);
                }
            }
        });

        ep.drain_naks(|nak| {
            retransmit_handler.on_nak(nak, now_ns);
        });
    }
}
```

**New `do_retransmit` method:**
```rust
fn do_retransmit(&mut self, now_ns: i64) -> i32 {
    let publications = &self.publications;
    let endpoints = &self.endpoints;
    let poller = &mut self.poller;
    let handler = &mut self.retransmit_handler;
    let mut work_count = 0i32;

    handler.process_timeouts(now_ns, |sid, stid, term_id, term_off, len| {
        // Find matching publication (linear scan, n <= 64).
        let pub_match = publications.iter().enumerate().find(|(_, p)| {
            p.session_id() == sid && p.stream_id() == stid
        });

        let Some((_pub_idx, pub_entry)) = pub_match else {
            return; // Publication removed since NAK received.
        };

        // Validate NAK range is still in the term buffer.
        let nak_position = pub_entry.compute_position(
            term_id, term_off as u32,
        );
        let sender_pos = pub_entry.sender_position();
        let term_length = pub_entry.term_length() as i64;
        let buffer_start = sender_pos.wrapping_sub(
            (PARTITION_COUNT as i64 - 1) * term_length,
        );

        // Half-range wrapping check: nak_position must be
        // >= buffer_start and < pub_position.
        let from_start = nak_position.wrapping_sub(buffer_start);
        let pub_pos = pub_entry.pub_position();
        let range = pub_pos.wrapping_sub(buffer_start);
        if from_start < 0 || from_start >= range {
            return; // Data overwritten or not yet written.
        }

        // Read frames from term buffer and send.
        let ep_idx = pub_entry.endpoint_idx();
        let dest = match pub_entry {
            PublicationEntry::Local { dest_addr, .. } => dest_addr.as_ref(),
            PublicationEntry::Concurrent { dest_addr, .. } => dest_addr.as_ref(),
        };
        let mtu = pub_entry.mtu();
        let limit = if (len as u32) < mtu { len as u32 } else { mtu };

        pub_entry.retransmit_scan(
            term_id,
            term_off as u32,
            limit,
            |_off, data| {
                if let Some(ep) = endpoints.get(ep_idx) {
                    let _ = ep.send_data(poller, data, dest);
                    work_count += 1;
                }
            },
        );
    });

    if work_count > 0 {
        let _ = poller.flush();
    }
    work_count
}
```

**Update `do_work`:**
```rust
fn do_work(&mut self) -> Result<i32, AgentError> {
    let now_ns = self.clock.update();
    let mut work_count = 0i32;

    // Step 1: Send data frames.
    let bytes_sent = self.do_send(now_ns);

    // Step 2: Poll for incoming SM / NAK / ERR.
    if bytes_sent == 0 || self.duty_cycle_counter >= self.duty_cycle_ratio {
        work_count += self.poll_control();
        self.process_sm_and_nak(now_ns);
        work_count += self.do_retransmit(now_ns);
        self.duty_cycle_counter = 0;
    } else {
        self.duty_cycle_counter += 1;
    }

    work_count += bytes_sent;
    Ok(work_count)
}
```

### 6. DriverContext change - `src/context.rs`

```rust
// New field:
pub retransmit_unicast_linger_ns: i64,

// Default:
retransmit_unicast_linger_ns: Duration::from_millis(60).as_nanos() as i64,

// Validation:
// New variant: RetransmitLingerZero
if self.retransmit_unicast_linger_ns <= 0 {
    return Err(ContextValidationError::RetransmitLingerZero);
}
```

### 7. NAK Validation Logic

The handler must reject NAKs for data no longer in the term buffer:

1. Compute `nak_position = compute_position(term_id, term_offset as u32)`
2. Compute `buffer_start = sender_position - (PARTITION_COUNT - 1) * term_length`
   - This is the oldest data still guaranteed in the buffer
3. Check `nak_position >= buffer_start` using wrapping_sub + half-range
4. Check `nak_position < pub_position` using wrapping_sub + half-range
5. If either check fails, skip retransmit (data overwritten or never written)

### 8. Retransmit Frame Reading

The retransmit scan reads from the term buffer at the NAK's
`(term_id, term_offset, length)` range:

1. Compute `partition_idx = RawLog::partition_index(term_id, initial_term_id)`
2. Call `retransmit_scan(term_id, term_offset, min(length, mtu), emit)`
3. Each emitted frame is sent individually via `endpoint.send_data()`
4. Frame length is capped at `mtu` per send

## Tests

### Unit Tests - `media::retransmit_handler` (11 tests)

| Method | Status |
|--------|--------|
| `retransmit_action_default_inactive` | Done |
| `on_nak_schedules_delay` | Done |
| `on_nak_dedup_same_range` | Done |
| `on_nak_different_offset_not_deduped` | Done |
| `on_nak_rejects_when_full` | Done |
| `process_timeouts_delay_to_callback` | Done |
| `process_timeouts_linger_to_inactive` | Done |
| `process_timeouts_not_yet_expired` | Done |
| `linger_prevents_reschedule` | Done |
| `linger_expires_allows_reschedule` | Done |
| `active_count_tracks_correctly` | Done |

### Unit Tests - `media::concurrent_publication` (6 new tests)

| Method | Status |
|--------|--------|
| `scan_term_at_reads_committed` | Done |
| `scan_term_at_does_not_advance_position` | Done |
| `scan_term_at_invalid_partition_returns_zero` | Done |
| `scan_term_at_empty_returns_zero` | Done |
| `sender_pub_compute_position` | Done |
| `sender_pub_position_reflects_offers` | Done |

### Unit Tests - `context` (3 new tests)

| Method | Status |
|--------|--------|
| `default_sender_params` (updated) | Done |
| `validate_retransmit_linger_zero` | Done |
| `validate_retransmit_linger_negative` | Done |

### Integration Tests - `agent_duty_cycle`

| Method | Status |
|--------|--------|
| `sender_retransmit_end_to_end` | Deferred |
| `retransmit_out_of_buffer_rejected` | Deferred |

> Integration tests deferred - requires injecting NAK frames into
> SendChannelEndpoint from a real UDP socket to trigger the full
> drain_naks -> on_nak -> process_timeouts -> retransmit_scan ->
> send_data path end-to-end. Current unit test coverage validates
> each component in isolation.

## Benchmark Results (2026-04-07)

| Benchmark | Target | Actual | Status | Headroom |
|-----------|--------|--------|--------|----------|
| `on_nak (schedule, empty table)` | < 50 ns | 41.4 ns | Pass | 17% |
| `on_nak (dedup, full table scan)` | < 200 ns | 26.2 ns | Pass | 87% |
| `process_timeouts (64 entries, 1 expired)` | < 300 ns | 84.6 ns | Pass | 72% |
| `process_timeouts (64 entries, 0 expired)` | < 200 ns | 25.9 ns | Pass | 87% |
| `full NAK-to-callback (1 frame)` | < 500 ns | 59.6 ns | Pass | 88% |

All 5 benchmarks pass targets with substantial margin. The 2.5 KiB
flat array fits in L1 cache; branch predictor handles the common
"not expired" path efficiently.

## Issues / Risks

| Issue | Mitigation | Blocking |
|-------|------------|----------|
| `process_sm_and_nak` borrow checker issue (endpoints + publications + retransmit_handler) | Destructure self into disjoint field borrows - same pattern as `do_send()` | Resolved |
| `do_retransmit` needs `&self.publications` + `&mut self.poller` + `&mut self.retransmit_handler` simultaneously | Destructure self. `process_timeouts` callback receives scalars, actual send happens in outer scope. | Resolved |
| NAK for data in concurrent publication's SharedLogBuffer requires Acquire-load scan | New `scan_term_at()` method mirrors sender_scan protocol. Safe because back-pressure guarantees partition won't be cleaned while data is still needed. | Resolved |
| Linear scan to match publication on NAK (O(n), n <= 64) | Acceptable for n <= 64. If publications grow, add hash index (like receiver's image_index). | No |
| `retransmit_scan` on PublicationEntry needs `&self` but `do_retransmit` also needs `&mut self.retransmit_handler` | Pass publications as a separate local via destructure. retransmit_scan only needs `&self` on PublicationEntry. | Resolved |
| Integration tests require NAK injection via real UDP socket | Deferred - unit tests cover each component. Integration tests to be added when receiver-side NAK generation is implemented. | No |

## Implementation Order

1. **High:** Add `retransmit_unicast_linger_ns` to `src/context.rs` with validation - **Done**
2. **High:** Create `src/media/retransmit_handler.rs` with RetransmitHandler + unit tests - **Done**
3. **High:** Add `SenderPublication::scan_term_at()` to `src/media/concurrent_publication.rs` - **Done**
4. **High:** Add `retransmit_scan()` + position accessors to PublicationEntry in `src/agent/sender.rs` - **Done**
5. **High:** Wire up RetransmitHandler in SenderAgent (field, constructor, process_sm_and_nak, do_retransmit, do_work) - **Done**
6. **Medium:** Add integration tests in `tests/agent_duty_cycle.rs` - **Deferred**
7. **Medium:** Add `pub mod retransmit_handler` to `src/media/mod.rs` - **Done**
8. **Low:** Add criterion benchmarks in `benches/retransmit.rs` + Cargo.toml bench target - **Done**

## Files Created/Modified

| Status | File | Lines | Tests | Description |
|--------|------|-------|-------|-------------|
| A | `src/media/retransmit_handler.rs` | 370 | 11 | RetransmitHandler, RetransmitAction, RetransmitState, on_nak, process_timeouts |
| M | `src/media/mod.rs` | +1 | - | Register retransmit_handler module |
| M | `src/context.rs` | +14 | +3 | Add retransmit_unicast_linger_ns field, default, validation |
| M | `src/media/concurrent_publication.rs` | +100 | +6 | Add scan_term_at(), compute_position(), pub_position() to SenderPublication |
| M | `src/agent/sender.rs` | +130 | - | RetransmitHandler field, do_retransmit(), fix process_sm_and_nak(), PublicationEntry accessors (retransmit_scan, compute_position, sender_position, pub_position, dest_addr) |
| A | `benches/retransmit.rs` | 122 | - | 5 Criterion benchmarks, all passing targets |
| M | `Cargo.toml` | +4 | - | Bench target for retransmit |
