# ADR-001: Use 4 Term Partitions Instead of Aeron's 3

**Date:** 2026-04-06  
**Status:** Accepted  
**Decision Date:** 2026-04-06  
**Deciders:** developer

## Context

Aeron's log buffer uses **3 term partitions** (triple buffering). The active partition rotates as each fills:

```
partition_index = (active_term_id - initial_term_id) % 3
```

This project's coding rules ([copilot-instructions.md](../../.github/copilot-instructions.md)) forbid modulo (`%`) in
ring/term index computation on the hot path:

> `[x] % (modulo) for ring / term index - use & (capacity - 1)`

The `offer()` call computes the partition index on every invocation - this is a hot-path operation. Using `% 3` violates
the rule. We need an alternative.

## Options Considered

### Option A: 4 Partitions with Bitmask

- **Description:** Use 4 partitions instead of 3. Partition index = `(term_id - initial_term_id) & 3`. Power-of-two
  count enables bitmask indexing.
- **Pros:** Zero-cost index computation (single AND instruction). Compliant with coding rules. Simple and predictable.
- **Cons:** 33% more memory per publication (4 * term_length instead of 3 * term_length). Diverges from Aeron protocol
  convention.
- **Effort:** Low

### Option B: 3 Partitions with Branchless mod-3

- **Description:** Keep 3 partitions. Implement mod-3 via multiply-shift: `x - 3 * ((x * 0xAAAB) >> 17)` or a 4-entry
  lookup table.
- **Pros:** Matches Aeron's partition count exactly. No extra memory.
- **Cons:** More complex code. The multiply-shift trick is fragile (only correct for limited input range). Lookup table
  adds a dependent load. Arguably still violates the spirit of the bitmask rule.
- **Effort:** Medium

### Option C: 3 Partitions with Conditional Branches

- **Description:** Keep 3 partitions. Use `if idx >= 3 { idx -= 3; }` chain (similar to round-robin wrap pattern already
  used in sender.rs).
- **Pros:** No modulo. Matches Aeron's count. Pattern already exists in codebase (round-robin in `do_send`).
- **Cons:** Mispredicted branch on every term rotation. Only works if input is in range [0, 5] - needs pre-wrapping.
  Less general than bitmask.
- **Effort:** Low

## Decision

**Chosen: Option A - 4 Partitions with Bitmask**

Use 4 term partitions with `& 3` bitmask indexing for the partition computation in the hot path.

## Rationale

- The bitmask rule is a non-negotiable enforcement constraint in this project. Option A is the only approach that fully
  complies without caveats.
- 33% memory overhead is acceptable: for the default `term_length = 64 KiB`, the extra partition costs 64 KiB per
  publication. With `MAX_PUBLICATIONS = 64`, worst case is 4 MiB total - negligible compared to io_uring buffer ring
  allocations.
- The partition index is computed on every `offer()` call and every `sender_scan()` frame. A single AND instruction vs
  any alternative is a clear win for the hot path.
- Aeron's choice of 3 was driven by the minimum needed for triple buffering (active, dirty, clean). 4 partitions provide
  the same guarantee with one extra clean buffer - no functional regression.
- The wire protocol is unaffected. `term_id` progression and `initial_term_id` semantics are unchanged. Only the
  internal buffer layout differs.

## Consequences

### Positive

- Single AND instruction for partition index on every `offer()` and `sender_scan()` call.
- Full compliance with the project's bitmask-only indexing rule.
- Simpler code - no clever multiply-shift tricks or conditional branches.

### Negative

- 33% more term buffer memory per publication (4 * term_length instead of 3 * term_length).
- Diverges from Aeron C/Java internal layout (3 partitions).

### Neutral

- Wire protocol unchanged. `term_id` and `initial_term_id` semantics are identical.
- Receiver side unaffected. No impact on interop with Aeron peers.

### Observed Outcomes (post-implementation)

- **Back-pressure formula:** `max_ahead = term_length * (PARTITION_COUNT - 1)`. With 4 partitions the publisher can be
  up to 3 terms ahead of the sender. This provides 192 KiB of buffering at the default 64 KiB term length - sufficient
  to absorb sender-side jitter without stalling the publisher.
- **Clean-entering-partition strategy:** During term rotation the publisher cleans the partition it is rotating INTO (
  not "3 ahead"). This avoids a subtle bug discovered during design: cleaning `(current + PARTITION_COUNT - 1) & 3`
  would destroy data in the partition the publisher just left, which the sender may not have scanned yet. Cleaning the
  entering partition is always safe because back-pressure guarantees the sender finished scanning its previous
  contents (the partition was last used 4 terms ago; the sender is at most 3 terms behind).
- **Cross-thread SharedLogBuffer:** The concurrent publication (`ConcurrentPublication` + `SenderPublication`) wraps the
  same 4-partition `Vec<u8>` in `UnsafeCell`. The cleaning safety argument carries over unchanged - the
  `Arc<PublicationInner>` shares immutable `PARTITION_COUNT` and `term_length` config, and the atomic frame-length
  commit protocol does not alter the partition layout.
- **Retransmit NAK validation:** The retransmit handler computes the oldest data still in the buffer as
  `sender_position - (PARTITION_COUNT - 1) * term_length`. NAKs for positions outside this range are rejected. The
  formula depends directly on `PARTITION_COUNT = 4`.
- **Compile-time assertion:** `term_buffer.rs` includes `const _: () = assert!(PARTITION_COUNT.is_power_of_two());` to
  prevent regressions.

## Affected Components

| Component                         | Impact | Lines | Tests        | Description                                                                                                                                                                                                                                                                                  |
|-----------------------------------|--------|-------|--------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `media/term_buffer.rs`            | High   | 1049  | 44           | `RawLog` allocates `PARTITION_COUNT * term_length`. `PARTITION_COUNT = 4`, `PARTITION_INDEX_MASK = 3`. `SharedLogBuffer` for cross-thread use. `partition_index()` free function. Compile-time power-of-two assertion.                                                                       |
| `media/network_publication.rs`    | High   | 793   | 32           | `NetworkPublication::offer()` and `sender_scan()` use `RawLog::partition_index()` (bitmask). Back-pressure limit = `(PARTITION_COUNT - 1) * term_length`. Clean-entering-partition on rotation.                                                                                              |
| `media/concurrent_publication.rs` | High   | 1017  | 30           | `ConcurrentPublication` + `SenderPublication` share `Arc<PublicationInner>` with `SharedLogBuffer`. Uses `partition_index()` for bitmask lookup. Atomic frame-length commit protocol over 4-partition buffer. `scan_term_at()` for retransmit reads.                                         |
| `agent/sender.rs`                 | Medium | 590   | -            | `PublicationEntry` enum (Local/Concurrent) with dispatch methods. `do_send()` calls `sender_scan` (bitmask on every frame). `do_retransmit()` uses `PARTITION_COUNT` for NAK range validation. `retransmit_scan()` dispatches to `RawLog::scan_frames` or `SenderPublication::scan_term_at`. |
| `media/retransmit_handler.rs`     | Medium | 369   | 11           | `RetransmitHandler` validates NAK ranges against `(PARTITION_COUNT - 1) * term_length` buffer window.                                                                                                                                                                                        |
| `context.rs`                      | Low    | 417   | 29           | `term_buffer_length` config with power-of-two validation. `retransmit_unicast_linger_ns` for retransmit linger timeout.                                                                                                                                                                      |
| `benches/publication_offer.rs`    | Low    | ~140  | 6 benchmarks | `offer()` + `sender_scan()` benchmarks exercising bitmask partition index.                                                                                                                                                                                                                   |
| `benches/retransmit.rs`           | Low    | 122   | 5 benchmarks | Retransmit handler benchmarks (on_nak, process_timeouts).                                                                                                                                                                                                                                    |
| `tests/agent_duty_cycle.rs`       | Low    | -     | 9            | Integration tests for SenderAgent with NetworkPublication.                                                                                                                                                                                                                                   |
| `tests/concurrent_offer.rs`       | Low    | 190   | 5            | Cross-thread integration tests including 10K-message stress test.                                                                                                                                                                                                                            |

## Compliance Checklist

- [x] Code reflects decision
- [x] Tests updated (160+ unit tests, 14 integration tests, 11 benchmarks)
- [x] Documentation updated (performance_design.md section 15, README project structure)
- [x] Superseded ADRs updated (N/A - no prior ADR)

## Revision History

| Date       | Change                                                                                                         | Author    |
|------------|----------------------------------------------------------------------------------------------------------------|-----------|
| 2026-04-06 | Initial draft                                                                                                  | developer |
| 2026-04-06 | term_buffer.rs (RawLog, 44 tests) + network_publication.rs (offer/scan, 32 tests) implemented                  | developer |
| 2026-04-06 | SenderAgent refactored - PublicationEntry owns NetworkPublication. 5 new integration tests                     | developer |
| 2026-04-06 | Cross-thread ConcurrentPublication + SenderPublication via SharedLogBuffer. 30 unit tests, 5 integration tests | developer |
| 2026-04-07 | RetransmitHandler with NAK range validation using PARTITION_COUNT. 11 unit tests, 5 benchmarks                 | developer |
| 2026-04-09 | Status promoted to Accepted - all implementation complete                                                      | developer |
