# ADR-001: Use 4 Term Partitions Instead of Aeron's 3

**Date:** 2026-04-06  
**Status:** Proposed  
**Deciders:** developer  
**Related Tasks:** Term buffer write path implementation  
**Related Sessions:** [Session 2026-04-06](../sessions/2026-04-06-term-buffer-write-path-plan.md)

## Context

Aeron's log buffer uses **3 term partitions** (triple buffering). The active partition rotates as each fills:

```
partition_index = (active_term_id - initial_term_id) % 3
```

This project's coding rules ([copilot-instructions.md](../../.github/copilot-instructions.md)) forbid modulo (`%`) in ring/term index computation on the hot path:

> `[x] % (modulo) for ring / term index - use & (capacity - 1)`

The `offer()` call computes the partition index on every invocation - this is a hot-path operation. Using `% 3` violates the rule. We need an alternative.

## Options Considered

### Option A: 4 Partitions with Bitmask

- **Description:** Use 4 partitions instead of 3. Partition index = `(term_id - initial_term_id) & 3`. Power-of-two count enables bitmask indexing.
- **Pros:** Zero-cost index computation (single AND instruction). Compliant with coding rules. Simple and predictable.
- **Cons:** 33% more memory per publication (4 * term_length instead of 3 * term_length). Diverges from Aeron protocol convention.
- **Effort:** Low

### Option B: 3 Partitions with Branchless mod-3

- **Description:** Keep 3 partitions. Implement mod-3 via multiply-shift: `x - 3 * ((x * 0xAAAB) >> 17)` or a 4-entry lookup table.
- **Pros:** Matches Aeron's partition count exactly. No extra memory.
- **Cons:** More complex code. The multiply-shift trick is fragile (only correct for limited input range). Lookup table adds a dependent load. Arguably still violates the spirit of the bitmask rule.
- **Effort:** Medium

### Option C: 3 Partitions with Conditional Branches

- **Description:** Keep 3 partitions. Use `if idx >= 3 { idx -= 3; }` chain (similar to round-robin wrap pattern already used in sender.rs).
- **Pros:** No modulo. Matches Aeron's count. Pattern already exists in codebase (round-robin in `do_send`).
- **Cons:** Mispredicted branch on every term rotation. Only works if input is in range [0, 5] - needs pre-wrapping. Less general than bitmask.
- **Effort:** Low

## Decision

**Chosen: Option A - 4 Partitions with Bitmask**

Use 4 term partitions with `& 3` bitmask indexing for the partition computation in the hot path.

## Rationale

- The bitmask rule is a non-negotiable enforcement constraint in this project. Option A is the only approach that fully complies without caveats.
- 33% memory overhead is acceptable: for the default `term_length = 64 KiB`, the extra partition costs 64 KiB per publication. With `MAX_PUBLICATIONS = 64`, worst case is 4 MiB total - negligible compared to io_uring buffer ring allocations.
- The partition index is computed on every `offer()` call and every `sender_scan()` frame. A single AND instruction vs any alternative is a clear win for the hot path.
- Aeron's choice of 3 was driven by the minimum needed for triple buffering (active, dirty, clean). 4 partitions provide the same guarantee with one extra clean buffer - no functional regression.
- The wire protocol is unaffected. `term_id` progression and `initial_term_id` semantics are unchanged. Only the internal buffer layout differs.

## Consequences

- **Positive:** Single-instruction partition index. Full compliance with coding rules. Simpler code.
- **Negative:** 33% more term buffer memory per publication. Diverges from Aeron C/Java internal layout.
- **Neutral:** Wire protocol unchanged. Receiver side unaffected. No impact on interop with Aeron peers.

## Affected Components

| Component | Impact | Description |
|-----------|--------|-------------|
| `media/term_buffer.rs` | High | New file - RawLog allocates 4 * term_length |
| `media/network_publication.rs` | High | New file - partition_index uses `& 3` |
| `agent/sender.rs` | Medium | PublicationEntry refactored to use NetworkPublication |
| `context.rs` | Low | Add term_buffer_length config |

## Compliance Checklist

- [ ] Code reflects decision
- [ ] Tests updated
- [ ] Documentation updated
- [ ] Superseded ADRs updated

## Revision History

| Date | Change | Author |
|------|--------|--------|
| 2026-04-06 | Initial draft | developer |

