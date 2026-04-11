# Session Summary: Benchmark Suite for Aeron C Comparison

**Date:** 2026-04-11  
**Duration:** ~2 hours (review + fixes + planning)  
**Focus Area:** benchmarks, examples, performance comparison

## Objectives

- [x] Review all 12 bench files and 5 examples for issues
- [x] Fix coding-rule violations (em-dashes, emoji)
- [x] Create missing `poll_fragments` benchmark (7 variants)
- [x] Run quick `poll_fragments` benchmark to validate
- [x] Classify benchmarks into tiers for Aeron C comparison
- [ ] Run full Tier 1 + Tier 2 benchmark suite
- [ ] Collect results into Aeron C comparison table
- [ ] Update `docs/performance_design.md` with new numbers

## Work Completed

### Benchmark Fixes

- Replaced 13 em-dashes across 3 bench files: `benches/io_uring_submit.rs`, `benches/cqe_dispatch.rs`, `benches/loss_recovery.rs`
- Replaced emoji in `examples/frame_roundtrip.rs`
- Added `.expect("add publication")` to `examples/send_heartbeat.rs`

### New `poll_fragments` Benchmark

- Created `benches/poll_fragments.rs` with 7 benchmarks covering the full subscriber read path
- Added `ReceiverImage::log_ptr()` unsafe accessor for pad-frame writes in bench
- Added `[[bench]]` entry in `Cargo.toml`
- Fixed `BatchSize::SmallInput` to `BatchSize::PerIteration` for gap/pad benches (was causing 3.9s results due to allocation in setup)

### Benchmark Tier Classification

Classified all 12 bench files + 2 examples into 3 tiers for Aeron C comparison.

## Decisions Made

| Decision | Rationale | ADR |
|----------|-----------|-----|
| 3-tier benchmark classification | Focus limited time on benchmarks with direct Aeron C comparables first | N/A |
| `PerIteration` for gap/pad benches | Setup allocates 256 KiB SharedLogBuffer; `SmallInput` measures allocation not poll cost | N/A |
| Separate threaded examples from single-thread benches | Aeron C Embedded* uses threaded agents; `ping_pong`/`throughput` examples are fairer comparison | N/A |

## Tests Added/Modified

| Test Class | Method | Type | Status |
|------------|--------|------|--------|
| `poll_fragments` bench | `bench_poll_empty` | Bench | Pass (~4 ns) |
| `poll_fragments` bench | `bench_poll_single_frame` | Bench | Pass (~9.4 ns) |
| `poll_fragments` bench | `bench_poll_batch_16` | Bench | Pass (~122 ns / 7.6 ns per frame) |
| `poll_fragments` bench | `bench_poll_batch_64` | Bench | Pass (~550 ns / 8.6 ns per frame) |
| `poll_fragments` bench | `bench_poll_gap_skip` | Bench | Pass (~71 ns) |
| `poll_fragments` bench | `bench_poll_pad_skip` | Bench | Pass (~53 ns) |
| `poll_fragments` bench | `bench_poll_single_with_payload` | Bench | Pass (~14 ns) |

## Issues Encountered

| Issue | Resolution | Blocking |
|-------|------------|----------|
| `BatchSize::SmallInput` caused ~3.9s gap/pad bench results | Changed to `BatchSize::PerIteration` - results dropped to ~71 ns / ~53 ns | No |
| `unsafe_op_in_unsafe_fn` warning on `log_ptr()` | Wrapped body in `unsafe { }` block (Rust 2024 edition requirement) | No |
| `mem::forget(transport)` leak in `cqe_dispatch.rs` and `publication_offer.rs` | Deferred - low priority, bench-only code | No |

## Benchmark Execution Plan

### Tier 1 - Direct Aeron C Comparables (MUST run, ~13 min)

These have published Aeron C reference numbers to compare against:

| Bench | Key Functions | Aeron C Equivalent | Aeron C Reference |
|-------|---------------|--------------------|-------------------|
| `e2e_latency` | single_msg_rtt, 1k_msgs_rtt_avg, header_only_rtt, small_64B_rtt | EmbeddedPingPong | p50 ~8-12 us, p99 ~15-20 us |
| `e2e_throughput` | 1k_msgs_1408B (elements + bytes) | EmbeddedThroughput | ~2-3 M msg/s |
| `publication_offer` | offer (empty/64B/1KiB), sender_scan (1/16), offer+scan | aeron_publication_offer | ~40-80 ns |
| `poll_fragments` | single frame, batch 16/64, gap skip, pad skip | aeron_image_poll | ~40-60 ns/fragment |
| `duty_cycle` | sender idle/1/16, receiver idle/1/16, combined | Conductor duty cycle | ~100-300 ns idle |

Threaded examples for fair comparison:

| Example | Aeron C Equivalent | Note |
|---------|--------------------|------|
| `ping_pong` | EmbeddedPingPong | Threaded, 2-hop RTT |
| `throughput` | EmbeddedThroughput | Threaded, burst+poll |

### Tier 2 - io_uring Advantage (SHOULD run, ~5 min)

No direct Aeron C equivalent but show the I/O floor:

| Bench | Key Functions | What It Shows |
|-------|---------------|---------------|
| `io_uring_submit` | NOP single/burst, sendmsg, recvmsg multishot | Raw kernel roundtrip floor |
| `cqe_dispatch` | single msg, burst 16, UserData decode | Aeron dispatch overhead above io_uring floor |

### Tier 3 - Already Documented (SKIP)

Already measured in `docs/performance_design.md` with PASS status:

| Bench | Status | Last Measured |
|-------|--------|---------------|
| `frame_parse` | All PASS (0.3-0.5 ns) | Documented |
| `slot_pool` | PASS (~3.0 ns) | Documented |
| `clock` | PASS (~0.2 ns cached) | Documented |
| `loss_recovery` | PASS (~0.7 ns scan) | Documented |
| `retransmit` | PASS | Documented |

### Run Commands

```bash
# Step 1: Tier 1 - Criterion benchmarks (single-threaded, deterministic)
cargo bench --bench publication_offer  # ~2 min
cargo bench --bench poll_fragments     # ~2 min
cargo bench --bench duty_cycle         # ~3 min
cargo bench --bench e2e_latency        # ~3 min
cargo bench --bench e2e_throughput     # ~3 min

# Step 2: Tier 1 - Examples (threaded, real-world)
cargo run --release --example ping_pong -- --warmup 5000 --samples 100000
cargo run --release --example throughput -- --duration 10 --burst 128

# Step 3: Tier 2 - io_uring baseline
cargo bench --bench io_uring_submit    # ~3 min
cargo bench --bench cqe_dispatch       # ~2 min
```

### Expected Comparison Table

```
Category           | aeron-rs       | Aeron C        | Delta  | Notes
-------------------|----------------|----------------|--------|------
RTT p50 (thread)   | ? us           | ~8-12 us       | ?      | ping_pong example
RTT p99 (thread)   | ? us           | ~15-20 us      | ?      | ping_pong example
Throughput (thread) | ? K msg/s      | ~2-3 M msg/s   | ?      | throughput example
offer() empty      | ? ns           | ~40-80 ns      | ?      | publication_offer
offer() 64B        | ? ns           | ~40-80 ns      | ?      | publication_offer
sender_scan 1fr    | ? ns           | ~30-50 ns      | ?      | publication_offer
poll single frame  | ? ns           | ~40-60 ns      | ?      | poll_fragments
poll batch 16      | ? ns (per-fr)  | ~40-60 ns      | ?      | poll_fragments
gap skip           | ? ns           | N/A            | -      | aeron-rs specific
duty cycle idle    | ? ns           | ~100-300 ns    | ?      | duty_cycle
duty cycle 1fr     | ? us           | ~1-3 us        | ?      | duty_cycle
```

## Comparison Caveats

- **Threading model differs** - `e2e_latency`/`e2e_throughput` bench = single thread interleaved. Aeron C Embedded* = multi-threaded. Use `ping_pong`/`throughput` examples for fair comparison.
- **I/O backend differs** - aeron-rs uses io_uring (zero-syscall multishot); Aeron C uses epoll + recvmsg. The `io_uring_submit` bench quantifies this structural advantage.
- **Term partitions differ** - aeron-rs uses 4 partitions (ADR-001, `& 3` bitmask); Aeron C uses 3 (`% 3`). 33% more memory, one fewer instruction per index.
- **Aeron C numbers are from published benchmarks** - machine-specific. The comparison shows relative positioning, not absolute equivalence.
- **Core isolation recommended** - `taskset -c 2 cargo bench --bench ...` for fair comparison with Aeron C benchmarks that typically run on isolated cores.

## Quick poll_fragments Results (Already Collected)

| Variant | Time | Per-Frame |
|---------|------|-----------|
| empty (no data) | ~4 ns | - |
| single frame | ~9.4 ns | 9.4 ns |
| batch 16 frames | ~122 ns | 7.6 ns |
| batch 64 frames | ~550 ns | 8.6 ns |
| gap skip (1 gap in 16) | ~71 ns | - |
| pad frame skip | ~53 ns | - |
| single frame 64B payload | ~14 ns | 14 ns |

## Next Steps

1. **High:** Run Tier 1 benchmarks (`publication_offer`, `poll_fragments`, `duty_cycle`, `e2e_latency`, `e2e_throughput`)
2. **High:** Run Tier 1 examples (`ping_pong`, `throughput`) for threaded comparison
3. **Medium:** Run Tier 2 benchmarks (`io_uring_submit`, `cqe_dispatch`)
4. **Medium:** Collect all results into Aeron C comparison table
5. **Medium:** Update `docs/performance_design.md` with `poll_fragments` numbers and comparison table
6. **Low:** Fix `mem::forget` leak in `cqe_dispatch.rs` and `publication_offer.rs`

## Files Changed

| Status | File |
|--------|------|
| M | `benches/io_uring_submit.rs` |
| M | `benches/cqe_dispatch.rs` |
| M | `benches/loss_recovery.rs` |
| M | `examples/frame_roundtrip.rs` |
| M | `examples/send_heartbeat.rs` |
| A | `benches/poll_fragments.rs` |
| M | `src/media/shared_image.rs` |
| M | `Cargo.toml` |

## Verification

- 446 tests passing (380 lib + 66 integration)
- Zero clippy warnings (lib + examples + benches)
- Formatting clean (`cargo fmt -- --check`)

