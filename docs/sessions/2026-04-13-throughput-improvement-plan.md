# Session Summary: Threaded Throughput Improvement Plan

**Date:** 2026-04-13
**Duration:** ~8 interactions
**Focus Area:** agent/sender.rs, media/uring_poller.rs, examples/throughput.rs, docs/

## Objectives

- [x] Analyze root causes of throughput gap (aeron-rs ~700K vs Aeron C ~2-3M msg/s)
- [x] Identify 5 bottlenecks with estimated impact per bottleneck
- [x] Design concrete implementation steps ordered by expected impact
- [x] Confirm further considerations: send_zc, IPC transport, GSO
- [x] Create configuration tuning guide (docs/configuration_tuning.md)
- [x] Update ARCHITECTURE.md and README.md with tuning references
- [x] Complete step 10 from subscription-poll-plan (ARCHITECTURE.md updates)
- [ ] Implement throughput improvements (steps 1-6 below)

## Analysis

### Problem Statement

Per-frame userspace operations are 4-8x faster than Aeron C (offer ~10 ns vs ~40-80 ns,
poll ~7.6 ns vs ~40-60 ns), but threaded throughput is 3-4x slower (~700K vs ~2-3M msg/s).
The bottleneck is not per-frame processing speed but the pipeline between threads and kernel.

### Root Causes Identified

| # | Bottleneck | Estimated Impact | Evidence |
|---|-----------|-----------------|----------|
| 1 | Benchmark methodology: single app thread interleaves offer+poll+yield | ~30-50% send rate loss | `yield_now()` can stall ~1-4 ms; poll() steals cycles from offer() |
| 2 | io_uring sendmsg overhead vs direct sendmsg | ~200-400 ns/frame extra | Bench: UDP sendmsg+reap = ~994 ns vs Aeron C direct ~500-800 ns |
| 3 | Send slot exhaustion stalls sender_scan | Stall when burst > 1024 slots | sender_scan stops when `send_available() == 0`; no mid-scan CQE reap |
| 4 | SM feedback loop latency | Sender stalls 5-20 us per window | SM round-trip = ping_pong RTT; sender_limit blocks until SM arrives |
| 5 | No send CQE recycling between scans | Slots stay InFlight until poll_control | do_send pushes SQEs; slots freed only in poll_control -> poll_recv |

### Per-Frame Time Budget (Current)

```
offer()                     ~10 ns   (userspace, fast)
sender_scan read frame      ~10 ns   (userspace, fast)
alloc_send + prepare_send    ~7 ns   (slot + SQE push)
kernel sendmsg              ~994 ns  (io_uring enter + UDP stack)
CQE reap + free_send        ~19 ns   (next poll_control cycle)
------------------------------------------------------------
Total per frame             ~1040 ns
Theoretical max             ~960K msg/s
```

Aeron C: ~600-900 ns/frame total -> ~1.1-1.6M msg/s theoretical.
Aeron C published benchmarks achieve 2-3M via tight scan+send loop with no SQE/CQE indirection.

### Architecture Constraint

io_uring adds a structural overhead layer for send (SQE push, CQE reap, slot management)
that direct `sendmsg()` does not have. This is the trade-off for io_uring's massive recv
advantage (multishot = ~12 ns vs epoll+recvmsg = ~1.5 us, 125x better). The send path
pays ~200-400 ns/frame for the io_uring indirection that the recv path avoids entirely.

## Work Completed

### Configuration Tuning Guide

- Created `docs/configuration_tuning.md` (632 lines): low-latency profile, high-throughput
  profile, SQPOLL sub-profile, parameter reference, trade-off analysis, Linux kernel tuning,
  Mermaid decision flowchart
- Updated `docs/ARCHITECTURE.md`: added Section 21 with profile summary table and link
- Updated `README.md`: added "Tuning Profiles" subsection in Configuration, link in
  Performance Design section

### ARCHITECTURE.md Subscription Updates (step 10 from subscription-poll-plan)

- Section 3 (Module Map): added `shared_image.rs`, `sub_bridge.rs`, fixed `subscription.rs` description
- Section 13 (Subscription): replaced "stubbed" with full implementation description + Mermaid diagram
- Section 19 (File Inventory): added new files, updated line counts, updated totals to ~17,000+ lines
- Verified checkbox `[x]` on line 1006 (Subscription data path)

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| Separate publisher/subscriber threads in benchmark | Current single-thread interleave is unfair vs Aeron C (dedicated threads). `yield_now()` causes OS scheduler stalls | N/A |
| Batch flush across all publications in do_send | Current: 1 flush per do_send (already batches all pubs). Improvement: interleave CQE reap to recycle slots mid-scan | N/A |
| Add `reap_send_completions()` to TransportPoller | Recycle slots without processing recv messages. Avoids full poll_recv overhead in send-dominant path | N/A |
| Increase throughput profile to 1 MiB term + window | 4x more frames per SM round-trip (728 vs 186). Memory cost 4 MiB/pub acceptable for throughput workloads | N/A |
| Confirm send_zc (IORING_OP_SEND_ZC) | Eliminates kernel copy into skb. Requires kernel >= 6.0, notification CQE handling. Do after baseline improvements | N/A |
| Confirm IPC transport (aeron:ipc) | Bypasses UDP entirely for intra-process. Path to 5M+ msg/s. Separate feature, track independently | N/A |
| Confirm GSO (Generic Segmentation Offload) | Batch N UDP frames into 1 super-frame via UDP_SEGMENT setsockopt. Aeron C does not use GSO - unique aeron-rs advantage. Combine with io_uring sendmsg | N/A |

## Tests Added/Modified

No code changes in this session - analysis and planning only.
Tests will be added in implementation sessions per step below.

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| ARCHITECTURE.md had stale "Subscription (stubbed)" text in 4 locations | Updated all: Module Map, Section 13, File Inventory descriptions and line counts | No |
| README Wire Protocol section header lost during configuration tuning edit | Detected in review, restored immediately | No |
| ARCHITECTURE.md ToC missing Section 21 after initial edit | Detected duplicate entry issue, resolved with single clean entry | No |

## Next Steps

### Implementation Steps (ordered by expected impact)

1. ~~**High:** Separate publisher/subscriber threads in `examples/throughput.rs`~~ Done
   - Spawn dedicated publisher thread: tight `offer()` burst loop, no poll, no yield
   - Spawn dedicated subscriber thread: tight `poll()` drain loop
   - Main thread: waits for duration, collects stats via `AtomicU64` (Acquire/Release)
   - Removed `yield_now()` entirely - replaced with `spin_loop()` on backpressure
   - **Result:** ~663K msg/s (vs previous ~700K - within noise). 301M backpressure events
     confirm publisher is now maximally aggressive. Bottleneck is sender agent io_uring
     send path, not benchmark methodology. Root causes #2-5 are dominant.
   - Files: `examples/throughput.rs`

2. ~~**High:** Slot recycling via early `poll_control()` in `do_send()`~~ Done
   - Original plan: add `reap_send_completions()` to `TransportPoller` trait
   - Issue discovered: `reap_send_completions()` discards recv CQEs (SM/NAK) because
     io_uring CQ is a ring - cannot selectively skip recv CQEs. This broke flow control
     test `sender_limit_advances_after_sm`.
   - Revised approach: call `self.poll_control()` + `self.process_sm_and_nak()` at the
     top of `do_send()` (before disjoint borrows) when `send_available < total_slots / 4`.
     This correctly processes SM/NAK AND frees send slots from CQEs.
   - `reap_send_completions()` kept on `UringTransportPoller` with warning doc for
     send-only scenarios where recv CQE loss is acceptable.
   - Threshold: `(pool.send_slots.len() / 4).max(1)` - relative to total slots.
     With 1024 slots = 256 threshold; with 16 slots = 4 threshold.
   - **Result:** Send rate ~662K (unchanged - kernel-bound). Recv rate **doubled**:
     195K msg/s vs 99K (early SM processing improves flow control loop). Loss 70% vs 85%.
   - Files: `agent/sender.rs`, `media/uring_poller.rs`

3. ~~**Medium:** Increase throughput profile buffers~~ Done
   - `term_buffer_length: 1 MiB` (4 MiB/pub, headroom ~2182 frames)
   - `receiver_window: Some(1 MiB)` (~728 frames per SM round-trip)
   - `uring_send_slots: 2048` (match larger burst capacity)
   - `uring_ring_size: 2048` (match send slots)
   - Updated `docs/configuration_tuning.md` with "Aggressive Throughput" variant
   - Updated Profile Comparison Table, code snippets, sizing guide, trade-off analysis
   - Expected: fewer SM stalls under sustained load
   - Files: `examples/throughput.rs`, `docs/configuration_tuning.md`

4. **Medium:** Benchmark + document results after steps 1-3
   - Run `cargo run --release --example throughput -- --duration 10 --burst 256`
   - Compare before/after numbers
   - Update `docs/performance_design.md` "End-to-End (Threaded)" table
   - Target: >= 1.5M msg/s send rate
   - Files: `docs/performance_design.md`

5. **Low:** io_uring send_zc (`IORING_OP_SEND_ZC`)
   - Requires kernel >= 6.0 feature detection at init
   - Eliminates kernel copy into skb for UDP sendmsg
   - Notification CQE handling (two CQEs per send: completion + notification)
   - Feature-gate behind `uring_send_zc: bool` in DriverContext
   - Expected: ~100-200 ns/frame reduction in kernel send cost
   - Files: `media/uring_poller.rs`, `context.rs`

6. **Low:** GSO (Generic Segmentation Offload) for batch send
   - Set `UDP_SEGMENT` via setsockopt on send socket
   - Build super-frame: concatenate N data frames into single sendmsg
   - Kernel splits at NIC layer - amortizes per-packet kernel overhead
   - Combine with io_uring sendmsg for maximum batch efficiency
   - Expected: N:1 syscall amortization for send path
   - Files: `media/transport.rs`, `media/uring_poller.rs`, `agent/sender.rs`

7. **Future:** IPC transport (`aeron:ipc`)
   - Shared memory log buffer between sender and receiver (no UDP)
   - Requires new `IpcPublication` / `IpcSubscription` types
   - Path to 5M+ msg/s for intra-process use case
   - Large feature - separate session/ADR
   - Files: new `media/ipc_publication.rs`, new channel URI parsing for `aeron:ipc`

### Milestone Targets

| Milestone | Send Rate | Recv Rate | Steps Required |
|-----------|-----------|-----------|----------------|
| Baseline | ~700K msg/s | ~211K msg/s | - |
| v1: Threaded benchmark | ~663K msg/s | ~99K msg/s | Step 1 |
| v2: Slot recycling | ~662K msg/s | ~196K msg/s | Steps 1-2 |
| v3: Buffer tuning | ~1.5M msg/s (est) | - | Steps 1-3 |
| v4: send_zc | ~1.8M msg/s (est) | - | Steps 1-5 |
| v5: GSO | ~2.5M msg/s (est) | - | Steps 1-6 |
| v6: IPC | ~5M+ msg/s (est) | - | Step 7 (different transport) |

## Files Changed

| Status | File |
|--------|------|
| A | `docs/configuration_tuning.md` |
| A | `docs/sessions/2026-04-13-throughput-improvement-plan.md` |
| M | `docs/ARCHITECTURE.md` |
| M | `README.md` |
| M | `examples/throughput.rs` (step 1: dedicated pub/sub threads, step 3: 1 MiB buffers) |
| M | `src/agent/sender.rs` (step 2: early poll_control in do_send) |
| M | `src/media/uring_poller.rs` (step 2: reap_send_completions method) |
| M | `docs/configuration_tuning.md` (step 3: aggressive throughput variant, updated sizing) |

