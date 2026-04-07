# Session Summary: Term Buffer Write Path Plan

**Date:** 2026-04-06  
**Duration:** ~3 interactions  
**Focus Area:** media/term_buffer, media/network_publication, agent/sender

## Objectives

- [x] Analyze existing write path gaps (no term buffer, no offer API, sender TODO)
- [x] Design term buffer architecture following Aeron model with project constraints
- [x] Produce detailed implementation plan for RawLog, NetworkPublication, SenderAgent integration
- [x] Identify key design decisions and document trade-offs
- [x] Implement term_buffer.rs (complete - 731 lines, 28 tests)
- [x] Implement network_publication.rs (complete - 794 lines, 32 tests)
- [x] Integrate into SenderAgent::do_send() (complete - 286 lines, 5 new integration tests)

## Work Completed

### Analysis

- Audited full codebase: `sender.rs`, `send_channel_endpoint.rs`, `receiver.rs`, `buffer_pool.rs`, `uring_poller.rs`, `frame.rs`, `context.rs`, `poller.rs`, `transport.rs`
- Mapped Aeron C/Java term buffer concepts to this project's constraints (bitmask-only indexing, zero-allocation hot path, wrapping arithmetic, no modulo)

### Plan Design

- Designed `RawLog` struct: single `Vec<u8>` allocated once at init, 4 partitions of `term_length` bytes each
- Designed `NetworkPublication` struct: owns `RawLog`, tracks `active_term_id`, `term_offset`, `pub_position`, `sender_position`
- Designed `offer(payload)` method: partition lookup via bitmask, frame append with 32-byte alignment, term rotation on fill
- Designed `sender_scan(limit, emit)` method: walk committed frames from `sender_position`, call emit for each frame slice
- Designed `OfferError` as stack-only `Copy` enum (no heap allocation)
- Planned `SenderAgent` refactor: `PublicationEntry` owns `NetworkPublication`, `do_send()` calls `sender_scan` and submits via `send_data()`
- Planned `DriverContext` addition: `term_buffer_length` field with power-of-two validation

### SenderAgent Integration (step 3)

- Replaced 6 scalar fields in `PublicationEntry` with `publication: NetworkPublication`
- Refactored `add_publication()`: params `term_length`/`mtu` changed from `i32` to `u32`, returns `Option<usize>`, constructs `NetworkPublication::new()` internally
- Added `publication_mut(idx)` accessor for external `offer()` calls
- Refactored `do_send()` with disjoint field borrow pattern (destructure `self` into `publications`, `endpoints`, `poller`):
  - Phase 1: Copy scalars from publication accessors (all Copy, zero-alloc)
  - Phase 2: Setup/heartbeat via `&mut endpoints[ep_idx]` + `&mut poller`
  - Phase 3: `sender_scan` closure calls `ep.send_data(poller, data, dest)` using disjoint borrows
- Updated `process_sm_and_nak()` to use `pub_entry.publication.session_id()` instead of `pub_entry.session_id`

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| 4 partitions instead of Aeron's 3 | Enables `& 3` bitmask indexing; `% 3` is forbidden by coding rules | [ADR-001](../decisions/ADR-001-four-term-partitions.md) |
| Single-thread offer (same thread as SenderAgent) | Matches project's single-threaded agent model; cross-thread deferred | N/A |
| Retransmit deferred | Term buffer enables retransmit but scheduling not in scope for this plan | N/A |
| `OfferError` as stack-only Copy enum | Consistent with `PollError` pattern; no heap allocation in hot path | N/A |
| Frame alignment = 32 bytes | Matches `DATA_HEADER_LENGTH`; ensures DataHeader overlay is always aligned | N/A |
| Destructure self for disjoint field borrows | Clean Rust pattern, no unsafe. Allows simultaneous &mut publication + &endpoints + &mut poller | N/A |
| `add_publication` returns `Option<usize>` | Propagates `NetworkPublication::new` failure. Index used for `publication_mut()` | N/A |
| Cast u32 to i32 for endpoint send_setup/heartbeat | Safe for valid term_length/mtu. Wire format uses i32. Follow-up to unify types | N/A |

## Tests Added/Modified

| Test Class | Method | Type | Status |
|------------|--------|------|--------|
| `media::term_buffer` | 28 tests (constructor, partition_index, append, scan, clean, roundtrip) | Unit | Complete |
| `media::network_publication` | 32 tests (constructor, offer, rotation, back-pressure, scan, position) | Unit | Complete |
| `agent_duty_cycle` | `sender_with_endpoint_and_publication` (updated API) | Integration | Complete |
| `agent_duty_cycle` | `sender_offer_and_send` (offer data, do_work, verify work_count > 0) | Integration | Complete |
| `agent_duty_cycle` | `sender_add_publication_returns_index` (sequential indices) | Integration | Complete |
| `agent_duty_cycle` | `sender_add_publication_invalid_returns_none` (bad params) | Integration | Complete |
| `agent_duty_cycle` | `sender_publication_mut_accessor` (accessor returns correct pub or None) | Integration | Complete |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| Aeron uses 3 partitions with `% 3` - forbidden by coding rules | Use 4 partitions with `& 3` bitmask; 33% more memory per publication | No |
| Cross-thread offer needs atomics on pub_position | Defer to future phase; current plan is single-threaded | No |
| Retransmit requires reading old frames from non-active partitions | Term buffer design supports this but retransmit scheduling deferred | Resolved (2026-04-07) |

## Next Steps

1. ~~**High:** Implement `src/media/term_buffer.rs`~~ - DONE (731 lines, 28 tests)
2. ~~**High:** Implement `src/media/network_publication.rs`~~ - DONE (794 lines, 32 tests)
3. ~~**High:** Refactor `src/agent/sender.rs`~~ - DONE (286 lines, 5 new integration tests)
4. ~~**Medium:** Add `term_buffer_length` to `src/context.rs` with power-of-two validation~~ - DONE (5 new tests)
5. ~~**Medium:** Update `src/media/mod.rs` module exports~~ - DONE (added network_publication)
6. ~~**Medium:** Add benchmark for offer() with term buffer~~ - DONE (6 new benchmarks in publication_offer.rs)
7. ~~**Low:** Design cross-thread offer with AtomicI64 pub_position (Release/Acquire)~~ - DONE (860 lines, 24 unit tests, 5 integration tests)
8. ~~**Low:** Design RetransmitHandler for NAK-driven retransmission from term buffer~~ - DONE (see [retransmit-handler-plan](2026-04-06-retransmit-handler-plan.md): 370 lines, 11 unit tests, 5 benchmarks)

## Files Changed

### Completed

| Status | File | Lines | Tests |
|--------|------|-------|-------|
| A | `src/media/term_buffer.rs` | 1049 | 42 |
| A | `src/media/network_publication.rs` | 794 | 32 |
| A | `src/media/concurrent_publication.rs` | 1017 | 30 |
| A | `src/media/retransmit_handler.rs` | 370 | 11 |
| M | `src/media/mod.rs` | +3 | - |
| M | `src/agent/sender.rs` | 591 | - |
| M | `tests/agent_duty_cycle.rs` | +80 | 5 new |
| A | `tests/concurrent_offer.rs` | 190 | 5 new |
| M | `src/context.rs` | +29 | 8 new |
| M | `benches/publication_offer.rs` | +140 | 6 new benchmarks |
| A | `benches/retransmit.rs` | 122 | 5 new benchmarks |

