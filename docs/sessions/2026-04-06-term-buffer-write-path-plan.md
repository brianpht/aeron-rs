# Session Summary: Term Buffer Write Path Plan

**Date:** 2026-04-06  
**Duration:** ~3 interactions  
**Focus Area:** media/term_buffer, media/network_publication, agent/sender

## Objectives

- [x] Analyze existing write path gaps (no term buffer, no offer API, sender TODO)
- [x] Design term buffer architecture following Aeron model with project constraints
- [x] Produce detailed implementation plan for RawLog, NetworkPublication, SenderAgent integration
- [x] Identify key design decisions and document trade-offs
- [ ] Implement term_buffer.rs (deferred - plan only)
- [ ] Implement network_publication.rs (deferred - plan only)
- [ ] Integrate into SenderAgent::do_send() (deferred - plan only)

## Work Completed

### Analysis

- Audited full codebase: `sender.rs`, `send_channel_endpoint.rs`, `receiver.rs`, `buffer_pool.rs`, `uring_poller.rs`, `frame.rs`, `context.rs`, `poller.rs`, `transport.rs`
- Identified the phase 3 TODO at `sender.rs:163-164` as the primary integration point
- Confirmed no term buffer, no `offer()` API, no sender scan exists yet
- Mapped Aeron C/Java term buffer concepts to this project's constraints (bitmask-only indexing, zero-allocation hot path, wrapping arithmetic, no modulo)

### Plan Design

- Designed `RawLog` struct: single `Vec<u8>` allocated once at init, 4 partitions of `term_length` bytes each
- Designed `NetworkPublication` struct: owns `RawLog`, tracks `active_term_id`, `term_offset`, `pub_position`, `sender_position`
- Designed `offer(payload)` method: partition lookup via bitmask, frame append with 32-byte alignment, term rotation on fill
- Designed `sender_scan(limit, emit)` method: walk committed frames from `sender_position`, call emit for each frame slice
- Designed `OfferError` as stack-only `Copy` enum (no heap allocation)
- Planned `SenderAgent` refactor: `PublicationEntry` owns `NetworkPublication`, `do_send()` calls `sender_scan` and submits via `send_data()`
- Planned `DriverContext` addition: `term_buffer_length` field with power-of-two validation

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| 4 partitions instead of Aeron's 3 | Enables `& 3` bitmask indexing; `% 3` is forbidden by coding rules | [ADR-001](../decisions/ADR-001-four-term-partitions.md) |
| Single-thread offer (same thread as SenderAgent) | Matches project's single-threaded agent model; cross-thread deferred | N/A |
| Retransmit deferred | Term buffer enables retransmit but scheduling not in scope for this plan | N/A |
| `OfferError` as stack-only Copy enum | Consistent with `PollError` pattern; no heap allocation in hot path | N/A |
| Frame alignment = 32 bytes | Matches `DATA_HEADER_LENGTH`; ensures DataHeader overlay is always aligned | N/A |

## Tests Added/Modified

| Test Class | Method | Type | Status |
|------------|--------|------|--------|
| - | - | - | Planned (not yet implemented) |

Planned tests:
- `term_buffer.rs`: append-then-scan roundtrip, partition rotation, frame alignment, term-full boundary
- `network_publication.rs`: offer + sender_scan end-to-end, term rotation on fill, BackPressured error
- `sender.rs`: SenderAgent::offer() through do_work() sends data frames

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| Aeron uses 3 partitions with `% 3` - forbidden by coding rules | Use 4 partitions with `& 3` bitmask; 33% more memory per publication | No |
| Cross-thread offer needs atomics on pub_position | Defer to future phase; current plan is single-threaded | No |
| Retransmit requires reading old frames from non-active partitions | Term buffer design supports this but retransmit scheduling deferred | No |

## Next Steps

1. **High:** Implement `src/media/term_buffer.rs` - RawLog, append_frame, scan_frames with unit tests
2. **High:** Implement `src/media/network_publication.rs` - NetworkPublication, offer(), sender_scan() with unit tests
3. **High:** Refactor `src/agent/sender.rs` - replace PublicationEntry scalars with NetworkPublication, wire do_send() scan
4. **Medium:** Add `term_buffer_length` to `src/context.rs` with power-of-two validation
5. **Medium:** Update `src/media/mod.rs` module exports
6. **Medium:** Add benchmark for offer() with term buffer (extend `benches/publication_offer.rs`)
7. **Low:** Design cross-thread offer with AtomicI64 pub_position (Release/Acquire)
8. **Low:** Design RetransmitHandler for NAK-driven retransmission from term buffer

## Files Changed

| Status | File |
|--------|------|
| - | No files modified (planning session only) |

### Files to be created/modified in implementation:

| Status | File |
|--------|------|
| A | `src/media/term_buffer.rs` |
| A | `src/media/network_publication.rs` |
| M | `src/media/mod.rs` |
| M | `src/agent/sender.rs` |
| M | `src/context.rs` |
| M | `benches/publication_offer.rs` |

