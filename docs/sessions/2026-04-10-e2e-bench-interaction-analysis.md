# Session Summary: E2E Benchmark Interaction - Flow Control Analysis

**Date:** 2026-04-10  
**Duration:** ~1 interaction  
**Focus Area:** benches/e2e_throughput.rs - benchmark hang root cause analysis

## Objectives

- [x] Trace the full data path from offer() through sender/receiver agents to SM feedback
- [x] Identify why the e2e_throughput benchmark stalls at Criterion "Warming up" phase
- [x] Map all flow control interactions between SenderAgent and ReceiverAgent
- [x] Quantify send slot, buf_ring, and SM timing interactions
- [ ] Implement fix (deferred - analysis only)

## Work Completed

### Root Cause Analysis

Traced the full sender/receiver data path through six subsystems:

1. `NetworkPublication::offer()` - term buffer write, back-pressure check
2. `SenderAgent::do_send()` - sender_scan, send slot allocation, io_uring submit
3. `UringTransportPoller::poll_recv()` - CQE harvest, multishot recv, buf_ring
4. `ReceiverAgent::poll_data()` - frame dispatch, image write, consumption advance
5. `ReceiverAgent::send_control_messages()` - SM generation, sm_interval_ns gating
6. `SenderAgent::process_sm_and_nak()` - SM drain, sender_limit advance

### Key Finding: SM Flow Control Bottleneck

The benchmark stalls due to a three-way interaction between sender_limit initialization,
receiver_window sizing, and SM timer gating.

**Benchmark parameters** (from `make_bench_ctx()`):
- `term_buffer_length` = 262,144 (256 KiB)
- `sender_limit` initial = 262,144 (one full term)
- `receiver_window` = `term_length / 2` = 131,072 (set in `on_setup`)
- `sm_interval_ns` = 1,000,000 (1ms)
- `BATCH_SIZE` = 1,000 messages
- `BURST_LIMIT` = 16 frames per burst
- `uring_send_slots` = 128

**Timeline of one benchmark iteration:**

```
Phase 1 (0-60us): Fast offer+scan
  - 12 bursts x 16 frames = 192 offered (186 in term 0, 6 in term 1)
  - sender_position = 262,144 (term 0 fully scanned including pad)
  - sender_limit = 262,144 (exhausted)
  - First SM sent by receiver but proposed (153,600) < current (262,144)

Phase 2 (60-240us): Offers continue, no scan
  - Publisher writes to terms 1-3 (sender can't scan, limit exhausted)
  - sender.do_work() -> bytes_sent=0 -> poll_control every cycle
  - No SM CQEs (receiver's sm_interval not elapsed)

Phase 3 (240us-1ms): BackPressured spin
  - pub_position reaches ~1,048,576 (sender_position + 3*term_length)
  - All offers return BackPressured
  - Both agents spin calling do_work()
  - Receiver processes data from phase 1 sends, consumption advances
  - Still no SM (interval not elapsed)

Phase 4 (1ms): First useful SM arrives
  - Receiver sm_interval elapses, SM sent
  - consumption ~261,888, proposed = 261,888 + 131,072 = 392,960
  - sender_limit advances to 392,960 (delta = 130,816 ~= 93 frames)

Phase 5 (1-4ms): SM-gated rounds
  - Each round: sender scans ~93 frames, waits ~1ms for next SM
  - Rounds needed: ceil((1,408,000 - 262,144) / 131,072) = 9 rounds
  - Total iteration time: ~4-9ms
```

```mermaid
sequenceDiagram
    participant P as Publisher (offer)
    participant S as SenderAgent
    participant K as Kernel (io_uring)
    participant R as ReceiverAgent

    Note over P,R: Phase 1: Fast scan (0-60us)
    loop 12 bursts x 16 frames
        P->>S: offer(payload) -> term buffer
        S->>K: sender_scan -> sendmsg SQEs
        K->>R: UDP loopback -> recv CQEs
        R->>R: on_data -> advance consumption
    end
    Note over S: sender_position = sender_limit = 262,144

    Note over P,R: Phase 2-3: Stalled (60us-1ms)
    P->>P: offer() -> BackPressured
    S->>S: scan limit = 0, poll_control (no SM)
    R->>R: sm_interval not elapsed, no SM sent

    Note over P,R: Phase 4: SM unblocks (1ms)
    R->>K: SM sendmsg (consumption + window)
    K->>S: SM recv CQE
    S->>S: sender_limit += 131,072

    Note over P,R: Phase 5: Repeat until 1000 offered
    loop ~9 SM rounds, 1ms each
        S->>K: scan + send ~93 frames
        K->>R: data -> advance consumption
        R->>K: SM (after 1ms interval)
        K->>S: SM -> advance sender_limit
    end
```

### Contributing Factors

**Factor 1 - Initial sender_limit vs receiver_window mismatch:**

The sender starts with `sender_limit = term_length` (262,144) but the receiver
advertises `receiver_window = term_length / 2` (131,072). The first SM from the
warmup phase reports `consumption(0,0) + 131,072 = 131,072 < 262,144` so
sender_limit never advances during warmup. The sender exhausts its full initial
window before any SM can help.

**Factor 2 - SM interval gates throughput:**

With `sm_interval_ns = 1ms` and the single-threaded interleaved model, SM
feedback arrives at most once per millisecond. Each SM advances sender_limit
by `receiver_window` (131,072 bytes = ~93 frames). To send 1,000 frames,
the benchmark needs ~9 SM rounds = ~9ms minimum per Criterion iteration.

**Factor 3 - Silent send_data errors in sender_scan:**

In `do_send`, the emit closure ignores send failures:
```rust
let _ = ep.send_data(poller, data, dest);
```
If `alloc_send()` fails (NoSendSlot), the frame is scanned (sender_position
advances) but never transmitted. This creates silent data loss. With 16-frame
bursts and 128 send slots it does not trigger in practice, but a single large
scan exceeding 128 frames would silently drop data.

**Factor 4 - CachedNanoClock epoch isolation:**

The sender and receiver each have independent `CachedNanoClock` instances
with different epochs. This is correct by design (no cross-agent clock
comparison), but means SM interval tracking is purely per-agent.

### Quantified Impact

| Metric | Value | Notes |
|--------|-------|-------|
| Frames per term | 186 | `262,144 / 1,408` |
| Frames before first SM stall | 186 | Initial sender_limit = term_length |
| Frames before BackPressured | ~744 | `3 * term_length / 1,408` |
| SM rounds to send 1000 frames | ~9 | `ceil((1,408,000 - 262,144) / 131,072)` |
| Time per SM round | ~1ms | Dominated by sm_interval_ns |
| Iteration time (observed) | ~4-9ms | vs target < 1ms |
| Send slots consumed per burst | 16 | Well under 128 limit |
| Buf ring entries consumed per burst | 16 | Well under 64 limit |

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| Reduce sm_interval_ns in bench context | 1ms interval is the primary throughput bottleneck in single-threaded mode. Reducing to 1us or 0 eliminates the SM-gated stall | N/A |
| Consider larger initial sender_limit | `term_length` is conservative. Aeron C uses `initial_window_length` which can be multiple terms. Larger initial window defers the first SM stall | N/A |
| Do not change receiver_window formula yet | `term_length / 2` matches Aeron C default. Changing it would affect protocol compatibility | N/A |
| Address silent send_data errors separately | Not the cause of the hang but a correctness risk. Track in a separate issue | N/A |

## Tests Added/Modified

| Test File | Test Name | Type | Status |
|-----------|-----------|------|--------|
| N/A | N/A | N/A | Analysis only - no code changes |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| Benchmark stalls ~1ms per SM round, ~9ms per iteration | Root cause identified: SM interval gates flow control in single-threaded model | Yes - benchmark unusable at current throughput |
| Silent send_data errors in sender_scan | `let _ = ep.send_data(...)` drops frames without signaling. Not triggered in current bench but risky | No - separate fix needed |
| First SM never advances sender_limit | `proposed = 0 + 131,072 < initial 262,144`. Only matters for the first term, subsequent SMs do advance | No - self-correcting after phase 4 |

## Next Steps

1. **High:** Fix `sm_interval_ns` in bench context - set to 0 or 1,000 (1us) to eliminate SM-gated stall. Alternatively, send SM on every `receiver.do_work()` call when data has been received.
2. **High:** Increase `receiver_window` or `sender_limit` initialization to cover the full BATCH_SIZE in the bench context, so the sender never stalls waiting for SM during a single iteration.
3. **Medium:** Fix silent `send_data` errors - return error count from `sender_scan` emit closure, or cap scan limit to available send slots before scanning.
4. **Medium:** Add sender_limit / sender_position / SM_count tracing under `cfg(debug_assertions)` so flow control stalls are visible during development.
5. **Low:** Consider adaptive SM - receiver sends SM immediately after processing data (Aeron C `SEND_SM_ON_DATA`) in addition to the timer-based path.

## Files Changed

| Status | File |
|--------|------|
| A | `docs/sessions/2026-04-10-e2e-bench-interaction-analysis.md` |

