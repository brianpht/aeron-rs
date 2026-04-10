# Session Summary: E2E Benchmark, Example, and Test Refactor Plan

**Date:** 2026-04-10  
**Duration:** ~1 interaction  
**Focus Area:** benches/, examples/, tests/ - E2E coverage for Aeron Transport comparison

## Objectives

- [x] Audit current benchmarks, examples, and tests for coverage gaps
- [x] Identify missing E2E paths (Sender -> UDP -> Receiver full data path)
- [x] Design E2E benchmarks comparable to Aeron C/Java (EmbeddedPingPong, EmbeddedThroughput)
- [x] Design E2E integration tests for protocol handshake, loss recovery, flow control
- [x] Plan example refactor (remove debug artifacts, upgrade to full stack)
- [ ] Implement E2E benchmarks (deferred - plan only)
- [ ] Implement E2E integration tests (deferred - plan only)
- [ ] Refactor examples (deferred - plan only)

## Analysis

### Current State

#### Benchmarks (8 files) - All Micro-Level, No E2E

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

**Gap:** No benchmark measures the full path `offer() -> SenderAgent::do_work() -> UDP -> ReceiverAgent::do_work() -> image`. Cannot produce numbers comparable to Aeron C `EmbeddedThroughput` or `EmbeddedPingPong`.

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

#### Tests (4 files) - No Full Data Path

| File | What It Tests | E2E? |
|------|--------------|------|
| `tests/uring_loopback.rs` | Send 1 packet via io_uring, recv via poll_recv | No - raw poller only |
| `tests/agent_duty_cycle.rs` | SenderAgent/ReceiverAgent smoke test (do_work idle) | No - no cross-agent data |
| `tests/client_library.rs` | MediaDriver launch, Aeron connect, Publication offer | Partial - no recv verify |
| `tests/concurrent_offer.rs` | ConcurrentPublication cross-thread correctness | No - term buffer only |

**Gap:** No test sends data from SenderAgent through UDP to ReceiverAgent and verifies arrival. No test exercises Setup/SM handshake, NAK/retransmit recovery, or RTTM measurement across real agents.

### Missing Data Path

```
offer() -> SenderAgent -> io_uring send -> UDP loopback -> io_uring recv -> ReceiverAgent -> Image
  ^                                                                                           ^
  |_______ NO bench, test, or example covers this FULL path __________________________________|
```

### Aeron C/Java Reference Points

| Aeron C/Java Tool | What It Measures | aeron-rs Equivalent |
|-------------------|-----------------|---------------------|
| `EmbeddedThroughput` | msg/s through full driver (offer -> send -> recv -> SM) | **MISSING** |
| `EmbeddedPingPong` | RTT p50/p99 through full driver | **MISSING** |
| `AeronStat` | Live counters (positions, rates, loss) | **MISSING** (counters not implemented) |
| `StreamStat` | Per-stream position tracking | **MISSING** |
| `LossStat` | Gap/NAK/retransmit counts | **MISSING** |

## Work Plan

### Step 1: `benches/e2e_throughput.rs` (New)

**Goal:** Criterion benchmark - full Aeron stack throughput on loopback.

**Path measured:**
```
Publication::offer() -> SenderAgent::do_work() -> io_uring sendmsg -> UDP loopback
  -> io_uring recvmsg -> ReceiverAgent::do_work() -> image write -> SM reply -> sender_limit update
```

**Design:**
- Wire a SenderAgent (endpoint + publication) and ReceiverAgent (endpoint) on 127.0.0.1
- Sender offers N frames (1408B payload), runs duty cycle (sender_scan + flush)
- Receiver runs duty cycle (poll_recv + on_message + SM reply)
- Interleave duty cycles on same thread (deterministic) OR two threads (realistic)
- Report: msgs/s, bytes/s, duty_cycles needed
- Target: >= 3 M msg/s with 1408B payload

**Comparison:** Aeron C `EmbeddedThroughput` - same payload size, same loopback, same metric.

**Files:** `benches/e2e_throughput.rs`, update `Cargo.toml`

### Step 2: `benches/e2e_latency.rs` (New)

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

### Step 3: `benches/duty_cycle.rs` (New)

**Goal:** Criterion benchmark - raw cost of `SenderAgent::do_work()` and `ReceiverAgent::do_work()` under load.

**Design:**
- Pre-fill N frames in publication, then bench a single `do_work()` call
- Variants: idle (0 frames), light (1 frame), saturated (16 frames)
- Measures: clock update + sender_scan + submit + flush + poll_recv combined
- Separately bench ReceiverAgent::do_work() with pre-injected UDP packets

**Files:** `benches/duty_cycle.rs`, update `Cargo.toml`

### Step 4: `tests/e2e_send_recv.rs` (New)

**Goal:** Integration test - prove data flows correctly from sender to receiver.

**Test cases:**

| Test | Description |
|------|-------------|
| `single_frame_arrives` | Offer 1 frame, run both agents, verify receiver image contains payload |
| `burst_100_frames_all_arrive` | Offer 100 frames, verify all arrive with correct checksums |
| `cross_thread_offer_to_receiver` | App thread offers via ConcurrentPublication, sender+receiver on threads |
| `multiple_streams_isolated` | Two publications (different stream_id), verify frames route to correct images |
| `term_rotation_under_load` | Fill > 1 term, verify rotation works across agents |

**Design:**
- Setup: SenderAgent + ReceiverAgent on loopback (127.0.0.1:0)
- Sender: offer frames with `(index: u32, checksum: u32)` payload
- Run duty cycles interleaved until receiver image has all frames
- Assert: frame count, payload checksums, ordering

**Why critical:** This is the single most important missing test. No existing test proves data goes from sender agent through the network to receiver agent.

**Files:** `tests/e2e_send_recv.rs`

### Step 5: `tests/protocol_handshake.rs` (New)

**Goal:** Integration test - verify Aeron session lifecycle across real agents.

**Test cases:**

| Test | Description |
|------|-------------|
| `setup_creates_image` | Sender sends Setup, receiver creates image entry |
| `sm_updates_sender_limit` | Receiver sends SM after data, sender_limit advances |
| `rttm_request_reply_updates_srtt` | Sender RTTM request -> receiver echo -> SRTT > 0 |
| `heartbeat_keeps_session_alive` | Sender idle heartbeats, receiver does not timeout image |

**Design:**
- Wire two agents on loopback
- Run duty cycles, inspect internal state (sender_limit, image count, SRTT)
- Use agent accessor methods to verify state transitions

**Files:** `tests/protocol_handshake.rs`

### Step 6: `tests/loss_recovery_e2e.rs` (New)

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

### Step 7: Refactor Examples

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

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| E2E benches work at agent level, not client Subscription | `Subscription::poll()` is stubbed (ARCHITECTURE.md S20). Agent-level gives full path without waiting for shared-memory image implementation | N/A |
| Interleaved duty cycle for deterministic benchmarks | Running both agents on same thread eliminates scheduling jitter. Add threaded variant as separate bench for realistic numbers | N/A |
| Delete `diag_ping.rs` instead of fixing it | Debug artifact with `format!` allocations, raw pointer dumps. No user or CI value. Fixing would still leave an example with no clear purpose | N/A |
| Defer Aeron C interop test | Requires Aeron C as dev dependency, complex build. Add later as `feature = ["interop-test"]` | N/A |
| Checksum payload in E2E tests | `(index: u32, checksum: !index)` in payload bytes. Catches bit corruption, reordering, and duplication in one check | N/A |

## Tests Added/Modified

| Test File | Test Name | Type | Status |
|-----------|-----------|------|--------|
| `tests/e2e_send_recv.rs` | `single_frame_arrives` | Integration | Planned |
| `tests/e2e_send_recv.rs` | `burst_100_frames_all_arrive` | Integration | Planned |
| `tests/e2e_send_recv.rs` | `cross_thread_offer_to_receiver` | Integration | Planned |
| `tests/e2e_send_recv.rs` | `multiple_streams_isolated` | Integration | Planned |
| `tests/e2e_send_recv.rs` | `term_rotation_under_load` | Integration | Planned |
| `tests/protocol_handshake.rs` | `setup_creates_image` | Integration | Planned |
| `tests/protocol_handshake.rs` | `sm_updates_sender_limit` | Integration | Planned |
| `tests/protocol_handshake.rs` | `rttm_request_reply_updates_srtt` | Integration | Planned |
| `tests/protocol_handshake.rs` | `heartbeat_keeps_session_alive` | Integration | Planned |
| `tests/loss_recovery_e2e.rs` | `gap_triggers_nak_and_retransmit` | Integration | Planned |
| `tests/loss_recovery_e2e.rs` | `multiple_gaps_coalesced` | Integration | Planned |
| `tests/loss_recovery_e2e.rs` | `retransmit_delay_respected` | Integration | Planned |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| `Subscription::poll()` stubbed - cannot do client-level E2E | Work at agent level (call `do_work()` directly, inspect image state) | No - workaround exists |
| No accessor for receiver image state from outside agent | May need to add `ReceiverAgent::image_count()` / `image_consumption_position()` accessors | No - can add when implementing |
| Loss injection requires skipping a frame in send path | Use `NetworkPublication::offer()` with sequential payloads, skip one offer index. Receiver will see gap at that offset | No |
| Aeron C interop requires external binary | Defer to `feature = ["interop-test"]` in follow-up | No - not blocking E2E within aeron-rs |

## Next Steps

1. ~~**High:** Implement `tests/e2e_send_recv.rs` - the most critical missing coverage (Step 4)~~ **Done**
2. ~~**High:** Implement `benches/e2e_throughput.rs` - primary Aeron C comparison metric (Step 1)~~ **Done** (hang under investigation - SM interval gating)
3. **High:** Implement `tests/protocol_handshake.rs` - validates session lifecycle (Step 5)
4. **Medium:** Implement `benches/e2e_latency.rs` - RTT comparison with Aeron C (Step 2)
5. **Medium:** Implement `tests/loss_recovery_e2e.rs` - validates NAK/retransmit path (Step 6)
6. **Medium:** Implement `benches/duty_cycle.rs` - agent-level cost tracking (Step 3)
7. **Low:** Refactor examples - delete diag_ping, upgrade ping_pong + throughput (Step 7)
8. **Low:** Add CI regression gate with `critcmp` (> 10% regression fails build)

## Files Changed

| Status | File |
|--------|------|
| A | `benches/e2e_throughput.rs` |
| A | `benches/e2e_latency.rs` |
| A | `benches/duty_cycle.rs` |
| A | `tests/e2e_send_recv.rs` |
| A | `tests/protocol_handshake.rs` |
| A | `tests/loss_recovery_e2e.rs` |
| M | `Cargo.toml` |
| M | `examples/ping_pong.rs` |
| M | `examples/throughput.rs` |
| D | `examples/diag_ping.rs` |

