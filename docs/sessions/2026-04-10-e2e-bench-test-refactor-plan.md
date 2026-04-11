# Session Summary: E2E Benchmark, Example, and Test Refactor Plan

**Date:** 2026-04-10 (updated 2026-04-11)
**Duration:** ~4 interactions
**Focus Area:** benches/, examples/, tests/ - E2E coverage for Aeron Transport comparison

## Objectives

- [x] Audit current benchmarks, examples, and tests for coverage gaps
- [x] Identify missing E2E paths (Sender -> UDP -> Receiver full data path)
- [x] Design E2E benchmarks comparable to Aeron C/Java (EmbeddedPingPong, EmbeddedThroughput)
- [x] Design E2E integration tests for protocol handshake, loss recovery, flow control
- [x] Plan example refactor (remove debug artifacts, upgrade to full stack)
- [x] Implement E2E benchmarks (e2e_throughput, duty_cycle done; e2e_latency remaining)
- [x] Implement E2E integration tests (e2e_send_recv, protocol_handshake done; loss_recovery_e2e remaining)
- [x] Implement RTT completion tests (rtt_completion - 12 tests covering bench RTT pattern)
- [x] Implement adaptive SM tests (adaptive_sm - 4 tests covering send_sm_on_data + duty cycle gating)
- [x] Implement flow control E2E tests (flow_control_e2e - 5 tests covering sender_limit lifecycle)
- [x] Discover and fix RTT #64 sender_limit stall bug (consumption tracking across term rotation)
- [ ] Implement loss recovery E2E tests (loss_recovery_e2e - planned)
- [ ] Refactor examples (deferred - plan only)
- [ ] Clean up diagnostic files (rtt_diag.rs - redundant with rtt_completion.rs)

## Analysis

### Current State

#### Benchmarks (10 files) - Micro-Level + E2E

| File | What It Measures | Aeron C Comparable? |
|------|-----------------|---------------------|
| `benches/clock.rs` | NanoClock vs CachedNanoClock latency | N/A (infra) |
| `benches/frame_parse.rs` | FrameHeader/DataHeader/SM/NAK/Setup parse | N/A (infra) |
| `benches/slot_pool.rs` | SlotPool alloc/free cycle | N/A (infra) |
| `benches/cqe_dispatch.rs` | io_uring CQE reap + callback dispatch | N/A (infra) |
| `benches/io_uring_submit.rs` | Raw io_uring NOP/sendmsg/recvmsg baseline | N/A (infra) |
| `benches/publication_offer.rs` | NetworkPublication::offer + sender_scan | Partial - no network |
| `benches/loss_recovery.rs` | Gap detection arithmetic + NAK build | Partial - simulated only |
| `benches/retransmit.rs` | RetransmitHandler on_nak + process_timeouts | Partial - no network |
| `benches/e2e_throughput.rs` | **Full stack offer -> send -> recv -> SM -> sender_limit** | **Yes** - EmbeddedThroughput (632K msgs/s) |
| `benches/duty_cycle.rs` | **SenderAgent/ReceiverAgent do_work cost under load** | **Yes** - agent overhead (idle ~1.6 us) |

**Covered:** `e2e_throughput.rs` measures the full path. `duty_cycle.rs` measures per-call agent overhead. **Remaining gap:** No RTT latency benchmark (`e2e_latency.rs` - Step 2).

#### Examples (6 files) - Mixed Quality

| File | Purpose | Issue |
|------|---------|-------|
| `examples/frame_roundtrip.rs` | Wire protocol demo | OK - keep |
| `examples/send_heartbeat.rs` | Minimal sender agent | OK - keep |
| `examples/recv_data.rs` | Minimal receiver agent | OK - keep |
| `examples/ping_pong.rs` | RTT measurement | Uses raw poller, not Aeron stack |
| `examples/throughput.rs` | Throughput measurement | Uses raw poller, not Aeron stack |
| `examples/diag_ping.rs` | Debug pointer dumps | Artifact - `format!` allocs, no value |

**Gap:** `ping_pong.rs` and `throughput.rs` bypass the entire Aeron protocol stack (no Setup handshake, no SM flow control, no image management). Results are io_uring raw UDP numbers, not Aeron Transport numbers.

#### Tests (12 files, 434 total tests) - Comprehensive E2E + Unit Coverage

| File | Tests | What It Tests | E2E? | Status |
|------|-------|--------------|------|--------|
| `tests/uring_loopback.rs` | 1 | Send 1 packet via io_uring, recv via poll_recv | No - raw poller only | Pass |
| `tests/agent_duty_cycle.rs` | 4 | SenderAgent/ReceiverAgent smoke test (do_work idle) | No - no cross-agent data | Pass |
| `tests/client_library.rs` | 15 | MediaDriver launch, Aeron connect, Publication offer | Partial - no recv verify | Pass |
| `tests/concurrent_offer.rs` | 5 | ConcurrentPublication cross-thread correctness | No - term buffer only | Pass |
| `tests/e2e_send_recv.rs` | 3 | **Full Publication -> Subscription data path** | **Yes** | Pass |
| `tests/protocol_handshake.rs` | 4 | **Setup/SM/RTTM/heartbeat lifecycle** | **Yes** | Pass |
| `tests/rtt_completion.rs` | 12 | **RTT completion pattern (bench single_msg_rtt)** | **Yes** | Pass |
| `tests/adaptive_sm.rs` | 4 | **Adaptive SM + duty cycle gating** | **Yes** | Pass |
| `tests/flow_control_e2e.rs` | 5 | **Flow control: sender_limit lifecycle, back-pressure** | **Yes** | Pass |
| `tests/rtt_diag.rs` | 1 | Diagnostic RTT cycle-count (early debug tool) | Yes | Pass (redundant) |
| Unit: `src/agent/sender.rs` | 17 | sender_limit updates, RTT EWMA, flow control clamping | No - unit | Pass |
| Unit: `src/agent/receiver.rs` | 34 | Consumption tracking, term rotation, gap detection | No - unit | Pass |

**Total: 434 tests, 0 failures** (371 unit + 63 integration).

### Data Path Coverage

```
offer() -> SenderAgent -> io_uring send -> UDP loopback -> io_uring recv -> ReceiverAgent -> Image
  ^                                                                                           ^
  |_____ COVERED by: e2e_throughput bench, e2e_send_recv tests, protocol_handshake tests,     |
  |                  rtt_completion tests, adaptive_sm tests, flow_control_e2e tests     _____|
```

**Remaining gaps:** NAK/retransmit E2E path, RTT latency benchmark.

### Aeron C/Java Reference Points

| Aeron C/Java Tool | What It Measures | aeron-rs Equivalent |
|-------------------|-----------------|---------------------|
| `EmbeddedThroughput` | msg/s through full driver (offer -> send -> recv -> SM) | `benches/e2e_throughput.rs` (632K msgs/s) |
| `EmbeddedPingPong` | RTT p50/p99 through full driver | **MISSING** (`e2e_latency.rs` planned) |
| `AeronStat` | Live counters (positions, rates, loss) | **MISSING** (counters not implemented) |
| `StreamStat` | Per-stream position tracking | **MISSING** |
| `LossStat` | Gap/NAK/retransmit counts | **MISSING** |

## Bugs Discovered

### BUG-001: RTT #64 sender_limit stall (FIXED)

**Symptom:** `rtt_across_multiple_term_rotations` and `rtt_with_wrapping_initial_term_id` tests failed at exactly RTT #64 with `term_length=1024`, `receiver_window=4096`.

**Debug output:**
```
RTT #63: old_limit=8128, offer=ok
  completed in 1 cycles, new_limit=8192
RTT #64: old_limit=8192, offer=admin_action
  TIMEOUT! final_limit=8192, old_limit=8192, diff=0
```

**Root cause:** When the receiver saw a term rotation (data arriving in term N+1 while active_term_id was N), it reset `consumption_term_offset = 0` at `receiver.rs:328`. This caused the SM to report a consumption_position that did not advance the sender's `sender_limit` past its current value. After 64 RTTs with 64-byte frames, consumption filled exactly `receiver_window` bytes (4096), and the sender_limit reached the ceiling of `consumption_position + receiver_window`. The next term rotation reset consumption to the new term base, making `proposed = new_base + receiver_window = current_limit`, so `diff = 0` and the update was rejected.

**Fix:** Corrected consumption tracking in receiver's term rotation path to properly account for the frame being appended in the same `on_data` call. The consumption_term_offset now correctly advances past 0 before the SM is generated, ensuring `proposed > current` after rotation.

**Status:** Fixed. Both tests now pass (500 RTTs with 30+ rotations, 80 RTTs crossing i32::MAX boundary).

## Work Plan

### Step 1: `benches/e2e_throughput.rs` - DONE

**Status:** Done (632K msgs/s, 842 MiB/s on loopback)

### Step 2: `benches/e2e_latency.rs` - REMAINING

**Goal:** Criterion benchmark - single-message RTT through full Aeron protocol.

**Path measured:**
```
offer() -> sender do_work -> UDP -> receiver do_work -> SM back -> sender do_work (SM reap)
```

**Design:**
- Same agent setup as Step 1
- Per iteration: offer 1 frame, spin sender+receiver duty cycles until SM arrives back
- Measure wall-clock ns from offer() to sender_limit advance (confirms SM received)
- Report: p50/p90/p99/p99.9 in ns
- Target: p50 < 10 us, p99 < 20 us

**Comparison:** Aeron C `EmbeddedPingPong` (~8-12 us p50, ~15-20 us p99).

**Files:** `benches/e2e_latency.rs`, update `Cargo.toml`

### Step 3: `benches/duty_cycle.rs` - DONE

**Status:** Done (idle ~1.6 us, 1 frame ~3.3 us, 16 frames ~21.9 us)

### Step 4: `tests/e2e_send_recv.rs` - DONE

**Status:** Done (3 tests: single_fragment, multi_fragment, different_streams_isolated)

### Step 5: `tests/protocol_handshake.rs` - DONE

**Status:** Done (4 tests: setup_creates_image, sm_updates_sender_limit, rttm_request_reply_updates_srtt, heartbeat_keeps_session_alive)

### Step 5b: `tests/rtt_completion.rs` - DONE (NEW)

**Goal:** Integration tests validating the exact RTT pattern used by `bench_e2e_latency::single_msg_rtt`.

**Test cases (12):**

| Test | Description | Status |
|------|-------------|--------|
| `single_rtt_completes` | Full-size payload (1408B - header) completes within spin budget | Pass |
| `single_rtt_header_only` | Header-only frame (0-byte payload) RTT | Pass |
| `rtt_term_boundary_retry` | RTT completes after AdminAction (term rotation) retry | Pass |
| `sustained_100_rtts_no_stall` | 100 sequential RTTs, none stall | Pass |
| `sender_limit_advances_per_rtt` | sender_limit strictly increases after each RTT | Pass |
| `warmup_completes_handshake` | After warmup: image exists, setup complete, sender_limit > 0 | Pass |
| `three_call_pattern_efficient` | 3-call spin (sender->receiver->sender) completes in <= 10 cycles avg | Pass |
| `single_rtt_small_payload` | RTT with 4-byte payload (different alignment) | Pass |
| `rtt_across_multiple_term_rotations` | 500 RTTs, term_length=1024, 30+ term rotations | Pass (was BUG-001) |
| `pad_frame_advances_sender_past_term_boundary` | Pad frame scanned, not transmitted, RTT in new term completes | Pass |
| `rtt_with_wrapping_initial_term_id` | initial_term_id=i32::MAX-2, 80 RTTs crossing i32 wrap | Pass (was BUG-001) |
| `rtt_timeout_without_receiver` | No connected receiver - spin loop terminates, no hang | Pass |

**Files:** `tests/rtt_completion.rs` (702 lines)

### Step 5c: `tests/adaptive_sm.rs` - DONE (NEW)

**Goal:** Integration tests validating adaptive SM (send_sm_on_data) and duty cycle gating.

**Test cases (4):**

| Test | Description | Status |
|------|-------------|--------|
| `adaptive_sm_enables_fast_rtt` | send_sm_on_data=true: RTT completes in <= 5 cycles | Pass |
| `timer_based_sm_still_works` | send_sm_on_data=false: RTT completes via timer SM | Pass |
| `duty_cycle_ratio_1_enables_fast_control_poll` | ratio=1: poll_control runs every cycle, fast RTT | Pass |
| `duty_cycle_ratio_high_delays_but_completes` | ratio=16: slower but still completes | Pass |

**Files:** `tests/adaptive_sm.rs` (325 lines)

### Step 5d: `tests/flow_control_e2e.rs` - DONE (NEW)

**Goal:** Integration tests validating end-to-end flow control behavior.

**Test cases (5):**

| Test | Description | Status |
|------|-------------|--------|
| `sender_limit_stays_at_initial_without_receiver` | No receiver: sender_limit stuck at term_length | Pass |
| `sender_limit_advances_after_sm` | With receiver: sender_limit advances on SM | Pass |
| `sender_limit_never_regresses` | 50 RTTs: sender_limit monotonically increases | Pass |
| `back_pressure_when_limit_exhausted` | Offers past sender_limit hit BackPressured | Pass |
| `backpressure_unreachable_under_bench_conditions` | 1000 RTTs with bench config: 0 BackPressured | Pass |

**Files:** `tests/flow_control_e2e.rs` (392 lines)

### Step 6: `tests/loss_recovery_e2e.rs` - REMAINING

**Goal:** Integration test - verify NAK-driven retransmit works across real agents.

**Test cases:**

| Test | Description |
|------|-------------|
| `gap_triggers_nak_and_retransmit` | Skip 1 frame in middle, verify NAK sent, frame retransmitted, image contiguous |
| `multiple_gaps_coalesced` | Skip 3 frames, verify NAK coalescing, all recovered |
| `retransmit_delay_respected` | Verify retransmit fires after delay_ns, not immediately |

**Design:**
- Offer frames 0..N, but "lose" frame K by sending it to a dummy address (or not calling do_work for that frame's flush)
- Run receiver, verify gap detection + NAK generation
- Run sender, verify RetransmitHandler fires + re-sends
- Run receiver again, verify frame K now in image

**Constraint:** Current `loss_recovery.rs` bench only simulates arithmetic (`wrapping_sub`). This test exercises the real NAK -> RetransmitHandler -> resend -> image path.

**Files:** `tests/loss_recovery_e2e.rs`

### Step 7: Refactor Examples - REMAINING

**7a. Delete `examples/diag_ping.rs`**
- Debug artifact: raw pointer dumps, `format!` allocations, `eprintln!` with hex
- No test value, no demo value
- Violates coding rules (`format!` in steady-state-adjacent code)

**7b. Upgrade `examples/ping_pong.rs`**
- Current: raw `UringTransportPoller` + `std::net::UdpSocket` pong reflector
- Target: full `MediaDriver` + `Publication` on sender side, agent-level receiver
- Keep: `--warmup`, `--samples` CLI, percentile reporting, Aeron C target comparison
- Note: blocked on `Subscription::poll()` stub. Use agent-level recv until data path implemented.

**7c. Upgrade `examples/throughput.rs`**
- Current: raw poller send -> `std::net::UdpSocket` recv thread
- Target: full `MediaDriver` + `Publication` send, agent-level receiver
- Add: `--json` flag for CI-parseable output
- Keep: `--duration`, `--burst` CLI, rate reporting

**7d. Update `Cargo.toml`**
- Add `[[bench]]` entries for: `e2e_throughput`, `e2e_latency`, `duty_cycle`
- Remove `[[example]]` entry for `diag_ping`

**Files:**
- Delete: `examples/diag_ping.rs`
- Modify: `examples/ping_pong.rs`, `examples/throughput.rs`, `Cargo.toml`

### Step 8: Cleanup - REMAINING

- Delete `tests/rtt_diag.rs` - diagnostic file now redundant with `tests/rtt_completion.rs`
- `debug_rtt64.rs` already cleaned up (not present)

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| E2E benches work at agent level, not client Subscription | `Subscription::poll()` is stubbed (ARCHITECTURE.md S20). Agent-level gives full path without waiting for shared-memory image implementation | N/A |
| Interleaved duty cycle for deterministic benchmarks | Running both agents on same thread eliminates scheduling jitter. Add threaded variant as separate bench for realistic numbers | N/A |
| Delete `diag_ping.rs` instead of fixing it | Debug artifact with `format!` allocations, raw pointer dumps. No user or CI value. Fixing would still leave an example with no clear purpose | N/A |
| Defer Aeron C interop test | Requires Aeron C as dev dependency, complex build. Add later as `feature = ["interop-test"]` | N/A |
| Checksum payload in E2E tests | `(index: u32, checksum: !index)` in payload bytes. Catches bit corruption, reordering, and duplication in one check | N/A |
| RTT completion tests use sender_limit advance as RTT signal | Matches the exact bench pattern. Catching sender_limit stalls in tests prevents silent bench regressions | N/A |
| Separate test files by concern (rtt, adaptive_sm, flow_control) | Each file has focused setup helpers and clear scope. Easier to debug individual failures | N/A |

## Tests Added/Modified

| Test File | Test Name | Type | Status |
|-----------|-----------|------|--------|
| `tests/e2e_send_recv.rs` | `publication_to_subscription_single_fragment` | Integration | Done |
| `tests/e2e_send_recv.rs` | `publication_to_subscription_multi_fragment` | Integration | Done |
| `tests/e2e_send_recv.rs` | `publication_and_subscription_different_streams_isolated` | Integration | Done |
| `tests/protocol_handshake.rs` | `setup_creates_image` | Integration | Done |
| `tests/protocol_handshake.rs` | `sm_updates_sender_limit` | Integration | Done |
| `tests/protocol_handshake.rs` | `rttm_request_reply_updates_srtt` | Integration | Done |
| `tests/protocol_handshake.rs` | `heartbeat_keeps_session_alive` | Integration | Done |
| `tests/rtt_completion.rs` | `single_rtt_completes` | Integration | Done |
| `tests/rtt_completion.rs` | `single_rtt_header_only` | Integration | Done |
| `tests/rtt_completion.rs` | `rtt_term_boundary_retry` | Integration | Done |
| `tests/rtt_completion.rs` | `sustained_100_rtts_no_stall` | Integration | Done |
| `tests/rtt_completion.rs` | `sender_limit_advances_per_rtt` | Integration | Done |
| `tests/rtt_completion.rs` | `warmup_completes_handshake` | Integration | Done |
| `tests/rtt_completion.rs` | `three_call_pattern_efficient` | Integration | Done |
| `tests/rtt_completion.rs` | `single_rtt_small_payload` | Integration | Done |
| `tests/rtt_completion.rs` | `rtt_across_multiple_term_rotations` | Integration | Done (was BUG-001) |
| `tests/rtt_completion.rs` | `pad_frame_advances_sender_past_term_boundary` | Integration | Done |
| `tests/rtt_completion.rs` | `rtt_with_wrapping_initial_term_id` | Integration | Done (was BUG-001) |
| `tests/rtt_completion.rs` | `rtt_timeout_without_receiver` | Integration | Done |
| `tests/adaptive_sm.rs` | `adaptive_sm_enables_fast_rtt` | Integration | Done |
| `tests/adaptive_sm.rs` | `timer_based_sm_still_works` | Integration | Done |
| `tests/adaptive_sm.rs` | `duty_cycle_ratio_1_enables_fast_control_poll` | Integration | Done |
| `tests/adaptive_sm.rs` | `duty_cycle_ratio_high_delays_but_completes` | Integration | Done |
| `tests/flow_control_e2e.rs` | `sender_limit_stays_at_initial_without_receiver` | Integration | Done |
| `tests/flow_control_e2e.rs` | `sender_limit_advances_after_sm` | Integration | Done |
| `tests/flow_control_e2e.rs` | `sender_limit_never_regresses` | Integration | Done |
| `tests/flow_control_e2e.rs` | `back_pressure_when_limit_exhausted` | Integration | Done |
| `tests/flow_control_e2e.rs` | `backpressure_unreachable_under_bench_conditions` | Integration | Done |
| `src/agent/receiver.rs` | 34 unit tests (consumption, term rotation, gap detection) | Unit | Done |
| `src/agent/sender.rs` | 17 unit tests (sender_limit, RTT EWMA, flow control) | Unit | Done |
| `tests/loss_recovery_e2e.rs` | `gap_triggers_nak_and_retransmit` | Integration | Planned |
| `tests/loss_recovery_e2e.rs` | `multiple_gaps_coalesced` | Integration | Planned |
| `tests/loss_recovery_e2e.rs` | `retransmit_delay_respected` | Integration | Planned |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| `Subscription::poll()` stubbed - cannot do client-level E2E | Work at agent level (call `do_work()` directly, inspect image state) | No - workaround exists |
| No accessor for receiver image state from outside agent | **Resolved:** Added `ReceiverAgent::image_count()`, `has_image()`, and `SenderAgent::publication_sender_limit()`, `publication_needs_setup()`, `publication_last_rtt_ns()` | No - resolved |
| Loss injection requires skipping a frame in send path | Use `NetworkPublication::offer()` with sequential payloads, skip one offer index. Receiver will see gap at that offset | No |
| Aeron C interop requires external binary | Defer to `feature = ["interop-test"]` in follow-up | No - not blocking E2E within aeron-rs |
| **BUG-001: RTT #64 sender_limit stall** | **Resolved:** Receiver consumption tracking during term rotation incorrectly reset consumption_term_offset=0 before the incoming frame was appended. After the frame append advanced consumption past 0, the SM correctly reported a position that advances sender_limit. The fix ensures consumption_position always moves forward across term boundaries. | No - resolved |

## Next Steps

1. **Medium:** Implement `benches/e2e_latency.rs` - RTT comparison with Aeron C (Step 2)
2. **Medium:** Implement `tests/loss_recovery_e2e.rs` - validates NAK/retransmit path (Step 6)
3. **Low:** Delete `tests/rtt_diag.rs` - redundant with `tests/rtt_completion.rs` (Step 8)
4. **Low:** Refactor examples - delete diag_ping, upgrade ping_pong + throughput (Step 7)
5. **Low:** Add CI regression gate with `critcmp` (> 10% regression fails build)

## Files Changed

| Status | File | Notes |
|--------|------|-------|
| Done | `benches/e2e_throughput.rs` | 632K msgs/s, 842 MiB/s |
| Planned | `benches/e2e_latency.rs` | RTT benchmark |
| Done | `benches/duty_cycle.rs` | idle ~1.6 us, 16 frames ~21.9 us |
| Done | `tests/e2e_send_recv.rs` | 3 tests via MediaDriver + Publication/Subscription |
| Done | `tests/protocol_handshake.rs` | 4 tests: Setup, SM, RTTM, heartbeat |
| Done | `tests/rtt_completion.rs` | 12 tests: RTT pattern, term rotation, wrapping, timeout (702 lines) |
| Done | `tests/adaptive_sm.rs` | 4 tests: adaptive SM, duty cycle gating (325 lines) |
| Done | `tests/flow_control_e2e.rs` | 5 tests: sender_limit lifecycle, back-pressure (392 lines) |
| Planned | `tests/loss_recovery_e2e.rs` | NAK/retransmit path |
| Done | `src/agent/sender.rs` | 17 unit tests + publication_sender_limit/needs_setup/last_rtt_ns accessors |
| Done | `src/agent/receiver.rs` | 34 unit tests + image_count/has_image accessors + BUG-001 fix |
| Done | `Cargo.toml` | bench entries for e2e_throughput, duty_cycle |
| Cleanup | `tests/rtt_diag.rs` | Redundant - delete after confirming rtt_completion covers all cases |
| Planned | `examples/ping_pong.rs` | Upgrade to full Aeron stack |
| Planned | `examples/throughput.rs` | Upgrade to full Aeron stack |
| Planned | `examples/diag_ping.rs` | Delete |

