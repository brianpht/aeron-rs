# Session Summary: ControlledPoll Implementation Plan

**Date:** 2026-04-11  
**Duration:** ~3 interactions  
**Focus Area:** client/subscription.rs, media/shared_image.rs, client/controlled.rs (new)

## Objectives

- [x] Design `ControlledAction` enum (Abort/Break/Commit/Continue) matching Aeron C semantics
- [x] Design `Header` metadata struct for rich handler context (term_id, term_offset, flags)
- [x] Plan `controlled_poll_fragments` on `SubscriberImage` with position rollback/commit semantics
- [x] Plan `controlled_poll` on `Subscription` (delegates to `controlled_poll_fragments`)
- [x] Resolve handler signature decision (keep existing `poll` as-is, new signature for controlled)
- [x] Resolve loss handler scope (defer to separate ADR)
- [x] Confirm monomorphization strategy (generic FnMut, no trait objects)
- [ ] Implement `ControlledAction` and `Header` in `src/client/controlled.rs`
- [ ] Implement `controlled_poll_fragments` on `SubscriberImage`
- [ ] Implement `controlled_poll` on `Subscription`
- [ ] Add unit tests for all 4 actions + gap/pad interactions
- [ ] Create ADR-003

## Analysis

### Problem Statement

`Subscription::poll()` and `SubscriberImage::poll_fragments()` advance the subscriber position
unconditionally after each poll cycle. The handler (`FnMut(&[u8], i32, i32)`) has no way to:

1. **Reject a fragment** - position advances even if the application cannot process the data.
2. **Abort mid-poll** - if the handler detects an error, all fragments already scanned are committed.
3. **Commit intermediate progress** - position is only flushed once at the end of the scan.
4. **Inspect frame metadata** - handler only receives `(payload, session_id, stream_id)`, not
   `term_id`, `term_offset`, or `flags` needed for loss tracking and fragment reassembly.

This means the application cannot implement zero-loss guarantees: gap-skip (ADR-002) silently
advances past lost frames with no way for the application to stall and retry.

### Aeron C Reference

Aeron C provides `aeron_image_controlled_poll()` with `aeron_controlled_fragment_handler_t`:

```c
typedef enum aeron_controlled_fragment_handler_action_en {
    AERON_ACTION_ABORT,     // Abort current poll, rollback position to start
    AERON_ACTION_BREAK,     // Stop polling, commit position up to current frame
    AERON_ACTION_COMMIT,    // Commit position now, continue polling
    AERON_ACTION_CONTINUE   // Continue polling (defer position flush to end)
} aeron_controlled_fragment_handler_action_t;
```

The handler returns an action per fragment. Position advancement is deferred until `COMMIT` or
end-of-poll, and can be rolled back with `ABORT`.

### Current poll_fragments Scan Loop (simplified)

```
subscriber_position_local = cached position
bytes_scanned = 0

for each frame:
    Acquire load frame_length
    if frame_length <= 0: gap-skip or break
    if frame_type == PAD: skip
    handler(payload, session_id, stream_id)   // no return value
    bytes_scanned += aligned_len

subscriber_position_local += bytes_scanned    // unconditional
Release store subscriber_position             // one flush at end
```

### Controlled Variant Differences

```
subscriber_position_local = cached position
bytes_scanned = 0
committed_bytes = 0

for each frame:
    Acquire load frame_length
    if frame_length <= 0: gap-skip or break
    if frame_type == PAD: skip (no handler call)
    action = handler(payload, &header)         // returns ControlledAction
    match action:
        Abort:    bytes_scanned = committed_bytes; break
        Break:    bytes_scanned += aligned_len; break
        Commit:   bytes_scanned += aligned_len
                  committed_bytes = bytes_scanned
                  flush position now (Release store)
        Continue: bytes_scanned += aligned_len

subscriber_position_local += bytes_scanned
if bytes_scanned > committed_bytes:
    Release store subscriber_position          // final flush
```

Key differences:
- `committed_bytes` watermark tracks the last COMMIT point
- ABORT rolls back to `committed_bytes` (not to start-of-poll if there were prior COMMITs)
- COMMIT triggers an intermediate Release store of subscriber_position
- Gap-skip and pad-frame paths do NOT invoke the handler (same as current `poll_fragments`)

## Design

### Step 1: `src/client/controlled.rs` (New File)

```rust
// Controlled poll types - application-level flow control for subscriptions.
//
// Mirrors Aeron C's aeron_controlled_fragment_handler_action_t.
// Zero-allocation, Copy types only. No trait objects.

/// Action returned by a controlled fragment handler to direct poll behavior.
///
/// Each variant controls whether the subscriber position advances, commits,
/// or rolls back. Zero-size discriminant - no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlledAction {
    /// Abort the current poll. Rollback subscriber position to the last
    /// COMMIT point (or to the start-of-poll if no COMMIT occurred).
    /// The same fragments will be re-delivered on the next poll() call.
    /// Fragment count returns 0 for the aborted portion.
    Abort = 0,

    /// Stop polling after this fragment. Commit position up to and
    /// including this fragment, then return. Remaining fragments in the
    /// term are deferred to the next poll() call.
    Break = 1,

    /// Commit subscriber position up to and including this fragment,
    /// then continue polling. The committed position becomes visible to
    /// the receiver (Release store) immediately. Useful for long poll
    /// runs where the application wants to checkpoint progress.
    Commit = 2,

    /// Continue polling. Position advancement is deferred until the poll
    /// returns (or until the next COMMIT/BREAK). This is the normal case
    /// and has the same behavior as the uncontrolled poll_fragments.
    Continue = 3,
}

/// Frame metadata passed to the controlled fragment handler.
///
/// All fields are Copy. Constructed on the stack per-frame during
/// poll - zero allocation. Provides the term coordinates needed for
/// loss tracking, fragment reassembly, and position computation.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    /// Term ID of this fragment.
    pub term_id: i32,
    /// Byte offset within the term where this frame starts.
    pub term_offset: i32,
    /// Frame flags byte (BEGIN/END/EOS bits).
    pub flags: u8,
    /// Total frame length (header + payload) as stored on wire.
    pub frame_length: i32,
    /// Session ID of the image.
    pub session_id: i32,
    /// Stream ID of the image.
    pub stream_id: i32,
}

impl Header {
    /// Check if BEGIN flag is set (first fragment of a message).
    #[inline]
    pub fn is_begin(&self) -> bool {
        self.flags & 0x80 != 0
    }

    /// Check if END flag is set (last fragment of a message).
    #[inline]
    pub fn is_end(&self) -> bool {
        self.flags & 0x40 != 0
    }

    /// Check if EOS (End of Stream) flag is set.
    #[inline]
    pub fn is_eos(&self) -> bool {
        self.flags & 0x20 != 0
    }

    /// Compute the absolute position of the end of this frame.
    /// Useful for tracking exactly where the subscriber is in the stream.
    #[inline]
    pub fn position(&self, initial_term_id: i32, position_bits_to_shift: u32) -> i64 {
        let term_count = self.term_id.wrapping_sub(initial_term_id) as i64;
        let term_length = 1i64 << position_bits_to_shift;
        term_count * term_length + self.term_offset as i64 + self.frame_length as i64
    }
}
```

### Step 2: `controlled_poll_fragments` on `SubscriberImage`

Add to `src/media/shared_image.rs`:

```rust
impl SubscriberImage {
    // ...existing poll_fragments...

    /// Controlled poll - handler returns ControlledAction per fragment.
    ///
    /// Same scan loop as poll_fragments but with position rollback/commit
    /// semantics. Gap-skip and pad-frame handling are identical (no handler
    /// call for gaps or pads).
    ///
    /// Handler signature: FnMut(&[u8], &Header) -> ControlledAction
    ///
    /// Position semantics:
    ///   - Continue: defer position flush to end of poll
    ///   - Commit:   flush position now (Release store), update watermark
    ///   - Break:    commit up to this fragment, stop polling
    ///   - Abort:    rollback to last Commit watermark, stop polling
    ///
    /// Returns number of fragments delivered (excluding aborted ones).
    pub fn controlled_poll_fragments<F>(&mut self, mut handler: F, limit: i32) -> i32
    where
        F: FnMut(&[u8], &Header) -> ControlledAction,
    {
        // ... (scan loop with committed_bytes watermark)
    }
}
```

The scan loop structure:
1. Same term_id/term_offset/partition computation as `poll_fragments`
2. Same gap-skip logic (no handler call, advance bytes_scanned)
3. Same pad-frame logic (no handler call, advance bytes_scanned)
4. For data frames: construct `Header` on stack, call `handler(payload, &header)`
5. Match on returned `ControlledAction`:
   - `Abort`: `bytes_scanned = committed_bytes`, break
   - `Break`: `bytes_scanned += aligned_len`, break
   - `Commit`: `bytes_scanned += aligned_len`, `committed_bytes = bytes_scanned`,
     immediate Release store of `subscriber_position_local + committed_bytes`
   - `Continue`: `bytes_scanned += aligned_len`
6. After loop: final position update with `bytes_scanned`

### Step 3: `controlled_poll` on `Subscription`

Add to `src/client/subscription.rs`:

```rust
impl Subscription {
    // ...existing poll()...

    /// Controlled poll - handler returns ControlledAction per fragment.
    ///
    /// Same image iteration as poll() but delegates to
    /// controlled_poll_fragments on each image.
    pub fn controlled_poll<F>(&mut self, mut handler: F, limit: i32) -> i32
    where
        F: FnMut(&[u8], &Header) -> ControlledAction,
    {
        // Step 1: drain_bridge (same)
        // Step 2: remove_closed_images (same)
        // Step 3: iterate images, call img.controlled_poll_fragments(&mut handler, per_image)
    }
}
```

### Step 4: Module Wiring

| File | Change |
|------|--------|
| `src/client/controlled.rs` | New file: `ControlledAction`, `Header` |
| `src/client/mod.rs` | Add `pub mod controlled;` and re-export `ControlledAction`, `Header` |
| `src/media/shared_image.rs` | Add `controlled_poll_fragments` method + import `ControlledAction`, `Header` |
| `src/client/subscription.rs` | Add `controlled_poll` method + import `ControlledAction`, `Header` |

### Step 5: Tests

All tests in `src/media/shared_image.rs` (unit tests, same test infrastructure):

| Test Name | Description |
|-----------|-------------|
| `controlled_poll_continue_matches_normal_poll` | All Continue - same result as poll_fragments |
| `controlled_poll_abort_rolls_back_position` | 3 frames, Abort on 2nd - position stays at 0 |
| `controlled_poll_abort_after_commit_rolls_to_watermark` | Commit on 1st, Abort on 3rd - position at frame 1 end |
| `controlled_poll_break_commits_partial` | 3 frames, Break on 2nd - position at frame 2 end |
| `controlled_poll_commit_flushes_midway` | Commit on 1st - verify receiver can see intermediate position via Acquire load |
| `controlled_poll_gap_skip_no_action_call` | Gap present - handler not called for gap bytes |
| `controlled_poll_pad_frame_no_action_call` | Pad frame - handler not called, position advances |
| `controlled_poll_header_fields_correct` | Verify Header term_id, term_offset, flags, frame_length, session_id, stream_id |
| `controlled_poll_abort_returns_zero` | Abort on first fragment returns 0 fragments |

### Step 6: ADR-003

`docs/decisions/ADR-003-controlled-poll.md`:

- Context: poll_fragments has no flow control, gap-skip is silent, app cannot guarantee zero-loss
- Options: (A) ControlledAction enum dispatch, (B) trait object handler, (C) callback + separate position API
- Decision: Option A - matches Aeron C, zero-alloc, monomorphized
- Consequences: app can stall at gaps, Abort enables retry, Commit enables checkpointing

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| Keep existing `poll_fragments` handler signature `FnMut(&[u8], i32, i32)` unchanged | Backward compatibility. Existing callers (throughput, ping_pong, tests) do not need Header metadata. Add `poll_fragments_with_header` later if demand arises | N/A |
| Defer loss handler callback (`on_loss`) to separate implementation | Keep this change focused on position control semantics. Loss notification is orthogonal - gap-skip path already exists and a loss callback can be layered on top without changing ControlledAction semantics | Future ADR |
| Use generic `FnMut` (monomorphized), not `dyn` trait object | Per project coding rules: no trait objects in duty cycle. Generic `F: FnMut(&[u8], &Header) -> ControlledAction` is monomorphized at each call site. Same pattern as existing `poll_fragments` | N/A |
| `Header` is a stack-allocated Copy struct, not a reference into the buffer | Avoids lifetime complexity. 6 scalar fields = 20 bytes, fits in registers. Constructed per-frame on the stack - no allocation | N/A |
| `ControlledAction` is `#[repr(u8)]` enum, not integer constants | Type-safe, exhaustive match, zero-cost (1 byte). Matches Rust idiom while keeping same semantics as Aeron C's integer enum | N/A |
| Abort rolls back to last Commit watermark, not to start-of-poll | Matches Aeron C semantics. If handler did Commit(A), Continue(B), Abort(C), position stays at A-end not at pre-poll. This allows incremental progress even with occasional aborts | N/A |
| Gap-skip and pad-frame do not invoke the controlled handler | Consistent with `poll_fragments` behavior. Gaps and pads are infrastructure-level skips, not application-visible fragments. Loss notification is a separate concern (future loss handler callback) | N/A |

## Tests Added/Modified

(Planned - not yet implemented)

| Test File | Test Name | Type | Status |
|-----------|-----------|------|--------|
| `src/media/shared_image.rs` | `controlled_poll_continue_matches_normal_poll` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_abort_rolls_back_position` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_abort_after_commit_rolls_to_watermark` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_break_commits_partial` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_commit_flushes_midway` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_gap_skip_no_action_call` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_pad_frame_no_action_call` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_header_fields_correct` | Unit | Planned |
| `src/media/shared_image.rs` | `controlled_poll_abort_returns_zero` | Unit | Planned |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| Handler signature change breaks existing callers | Resolved: keep existing `poll` / `poll_fragments` unchanged. New `controlled_poll` / `controlled_poll_fragments` are separate methods with different handler type | No |
| Intermediate Commit requires Release store mid-loop | Acceptable: one extra atomic store per Commit action. Normal path (all Continue) has same single Release store as current poll_fragments | No |
| Abort after gap-skip: should gap bytes be rolled back? | No: gap-skip bytes are not handler-visible. Abort only rolls back handler-delivered fragment bytes to the last Commit watermark. Gap bytes before the aborted fragment are already committed implicitly | No |

## Next Steps

1. **High:** Implement `src/client/controlled.rs` - `ControlledAction` enum + `Header` struct
2. **High:** Implement `controlled_poll_fragments` on `SubscriberImage` in `shared_image.rs`
3. **High:** Implement `controlled_poll` on `Subscription` in `subscription.rs`
4. **High:** Wire module in `client/mod.rs` - add `pub mod controlled` + re-exports
5. **Medium:** Add 9 unit tests in `shared_image.rs` (controlled poll variants)
6. **Medium:** Create ADR-003 (`docs/decisions/ADR-003-controlled-poll.md`)
7. **Low:** Add integration test in `tests/client_library.rs` using `controlled_poll`
8. **Low:** Update ARCHITECTURE.md section 15 and 20 with controlled poll status

## Files Changed

(Planned - not yet implemented)

| Status | File |
|--------|------|
| A | `src/client/controlled.rs` |
| M | `src/client/mod.rs` |
| M | `src/media/shared_image.rs` |
| M | `src/client/subscription.rs` |
| A | `docs/decisions/ADR-003-controlled-poll.md` |
| A | `docs/sessions/2026-04-11-controlled-poll-plan.md` |

