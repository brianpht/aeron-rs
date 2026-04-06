# Performance Design

> **Zero-copy, io_uring-native Aeron media driver in Rust.**
>
> [!] If a change triggers allocation in steady state, moves a pinned buffer, or adds a syscall in the duty cycle - *
*REJECT**.

---

## Table of Contents

- [Project Overview](#project-overview)
- [Governance Model](#governance-model)
- [Deployment Assumptions](#deployment-assumptions)
- [Performance Targets](#performance-targets)
- [Core Design Principles](#core-design-principles)
    - [1. Determinism First](#1-determinism-first)
    - [2. Zero-Copy I/O via io_uring](#2-zero-copy-io-via-io_uring)
    - [3. Allocation-Free Hot Path](#3-allocation-free-hot-path)
    - [4. Single-Threaded Agent Model](#4-single-threaded-agent-model)
    - [5. Cache-Oriented Design](#5-cache-oriented-design)
    - [6. Pinned Memory & Slot Lifecycle](#6-pinned-memory--slot-lifecycle)
    - [7. Multishot Receive with Provided Buffer Ring](#7-multishot-receive-with-provided-buffer-ring)
    - [8. Sequence & Term Arithmetic](#8-sequence--term-arithmetic)
    - [9. Ring Buffer Discipline](#9-ring-buffer-discipline)
    - [10. Wire Format (Little-Endian)](#10-wire-format-little-endian)
    - [11. Error Handling in Hot Path](#11-error-handling-in-hot-path)
    - [12. Dispatch & Polymorphism](#12-dispatch--polymorphism)
    - [13. Unsafe Policy](#13-unsafe-policy)
    - [14. Performance Budget](#14-performance-budget)
- [Architecture Overview](#architecture-overview)
- [Final Principle](#final-principle)

---

## Project Overview

### Target Domains

| Domain                          | Use Case                               |
|---------------------------------|----------------------------------------|
| HFT (High-Frequency Trading)    | Ultra-low latency market data / orders |
| Real-time telemetry             | Sensor data ingestion at scale         |
| Latency-critical infrastructure | Distributed systems messaging (Aeron)  |
| High-throughput data planes     | > 3 M msg/s on commodity hardware      |

### Inspiration

| Source               | Contribution                                   |
|----------------------|------------------------------------------------|
| Aeron (Real Logic)   | Wire protocol, agent model, term buffer design |
| io_uring             | Syscall-free I/O, provided buffer rings        |
| Mechanical Sympathy  | Hardware-aware, cache-local data structures    |
| Aeron C media driver | Reference latency floor (~40-80 ns offer path) |

---

## Governance Model

This project defines **two layers** of performance governance:

| Layer            | Document                          | Purpose                       |
|------------------|-----------------------------------|-------------------------------|
| **Architecture** | `docs/performance_design.md`      | Defines intent and reasoning  |
| **Enforcement**  | `.github/copilot-instructions.md` | Enforces non-negotiable rules |

### Conflict Resolution

```
If enforcement rules conflict with architecture
    → Architecture must be updated first

Benchmarks are the final authority.
```

### Auto-Reject Rules (Enforced)

```
[x] Mutex / RwLock in agent duty cycle
[x] HashMap in hot path - use pre-sized flat array + index
[x] % (modulo) for ring / term index - use & (capacity - 1)
[x] unwrap() / expect() in parsing or CQE handling
[x] Trait object (dyn) in duty cycle - monomorphize or enum dispatch
[x] Allocation (Vec::push growth, Box, String, format!) inside duty cycle
[x] Sequence comparison using > or < - use wrapping_sub + half-range
[x] Vec resize / realloc while io_uring slots are in-flight
[x] Pointer cast for wire format parsing - use from_le_bytes or repr(C, packed)
[x] Host-endian assumption in protocol frames
[x] SeqCst atomic ordering in hot path
[x] std::io::Error construction in hot path (heap-allocates)
```

---

## Deployment Assumptions

| Assumption           | Value                                               |
|----------------------|-----------------------------------------------------|
| Primary target       | x86_64 Linux (kernel >= 5.19 for buf_ring)          |
| Wire format          | **Little-endian (Aeron protocol)**                  |
| I/O model            | **io_uring** (no epoll/select fallback in hot path) |
| Thread model         | One thread per agent - no shared mutable state      |
| Cluster architecture | Same-architecture expected                          |
| Priority             | Deterministic latency > cross-platform portability  |

> [!] **Note**: Big-endian targets are rejected at compile time (`compile_error!` in `frame.rs`). Cross-endian support
> would require a versioned protocol change.

---

## Performance Targets

### Userspace Operations

| Metric                       | Target   | Measured | Status |
|------------------------------|----------|----------|--------|
| FrameHeader::parse           | < 5 ns   | ~0.3 ns  | PASS   |
| DataHeader::parse            | < 5 ns   | ~0.4 ns  | PASS   |
| classify_frame               | < 5 ns   | ~0.5 ns  | PASS   |
| SlotPool alloc + free        | < 10 ns  | ~3.0 ns  | PASS   |
| SQE push (alloc + prepare)   | < 10 ns  | ~3.8 ns  | PASS   |
| Heartbeat build + submit     | < 15 ns  | ~4.5 ns  | PASS   |
| CQE dispatch (single msg)    | < 50 ns  | ~13.7 ns | PASS   |
| NAK build + write            | < 10 ns  | ~1.5 ns  | PASS   |
| Loss scan (16 frames, 1 gap) | < 200 ns | ~0.7 ns  | PASS   |
| Gap detection (wrapping sub) | < 5 ns   | ~0.4 ns  | PASS   |
| CachedNanoClock::cached      | < 1 ns   | ~0.2 ns  | PASS   |

### io_uring Kernel Roundtrip

| Metric                        | Target   | Measured | Status |
|-------------------------------|----------|----------|--------|
| NOP submit + reap (single)    | < 500 ns | ~163 ns  | PASS   |
| NOP submit + reap (burst 16)  | < 1 us   | ~371 ns  | PASS   |
| UDP sendmsg + reap            | < 2 us   | ~874 ns  | PASS   |
| Multishot recv reap + recycle | < 50 ns  | ~14.8 ns | PASS   |
| submit() empty ring (syscall) | < 200 ns | ~65.1 ns | PASS   |
| SQE push only (no submit)     | < 50 ns  | ~31.4 ns | PASS   |

### System-Level

| Metric                    | Target                      | Status |
|---------------------------|-----------------------------|--------|
| Throughput (1408B frames) | >= 3 M msg/s                | PASS   |
| Steady-state allocation   | **Zero**                    | PASS   |
| Syscalls per duty cycle   | 0-1 (`io_uring_enter` only) | PASS   |

### Regression Policy

- **> 10% regression** - requires justification and benchmark comparison
- **Tail latency** matters more than average latency
- `p99 > p50 x 2` - investigate

---

## Core Design Principles

### 1. Determinism First

```
Correctness > Determinism > Latency > Throughput
```

> [!] **Unbounded memory or nondeterministic latency is a correctness failure.**

#### Must Be Deterministic Under

| Condition            | Required |
|----------------------|----------|
| Packet loss          | Yes      |
| Reordering           | Yes      |
| Duplication          | Yes      |
| Sequence / term wrap | Yes      |
| io_uring CQE reorder | Yes      |
| Buffer exhaustion    | Yes      |

**No randomness in protocol logic. No unbounded retries.**

---

### 2. Zero-Copy I/O via io_uring

The entire I/O path uses io_uring - no `epoll`, no `select`, no `recvmsg` syscall in the duty cycle.

```
┌─────────────┐  SQE   ┌─────────────────────┐  io_uring_enter  ┌────────┐
│ Agent       │───────▶│ UringTransportPoller │────────────────▶│ Kernel │
│ (single-    │  CQE   │  SlotPool (pinned)   │◀────────────────│        │
│  threaded)  │◀───────│  BufRingPool (shared)│                 └────────┘
└─────────────┘        └─────────────────────┘
```

| Principle                           | Rationale                                        |
|-------------------------------------|--------------------------------------------------|
| One `io_uring_enter` per flush      | Amortises syscall across all pending SQEs        |
| Multishot RecvMsgMulti stays active | No SQE re-arm per received packet                |
| Provided buffer ring (buf_ring)     | Kernel picks buffers from shared ring - no copy  |
| SendMsg via slot pool               | Pre-allocated msghdr + iov, kernel copies to skb |
| CQE batch harvest to stack buffer   | Breaks borrow on ring; cache-local iteration     |

#### Why Not epoll + recvmsg?

| Property         | epoll + recvmsg          | io_uring                          |
|------------------|--------------------------|-----------------------------------|
| Syscalls/msg     | 2 (epoll_wait + recvmsg) | 0 (multishot stays armed)         |
| Buffer ownership | Userspace → kernel copy  | Kernel picks from provided ring   |
| Batching         | Manual                   | Native (submit N SQEs, one enter) |
| Re-arm cost      | Per-message              | Zero (multishot)                  |

---

### 3. Allocation-Free Hot Path

#### No Heap Allocation During

| Operation              | Allocation Allowed? |
|------------------------|---------------------|
| `Agent::do_work`       | NO                  |
| `poll_recv` (CQE reap) | NO                  |
| `submit_send`          | NO                  |
| Frame parse / classify | NO                  |
| SM / NAK generation    | NO                  |
| Loss detection         | NO                  |
| Heartbeat / setup send | NO                  |

- All buffers **pre-allocated at initialization** (`SlotPool::new`, `BufRingPool::new`)
- Scratch buffers for control frames live **inline in the endpoint struct** (`heartbeat_buf`, `sm_buf`, `nak_buf`)
- Pending SM / NAK queues are **pre-sized flat arrays** with length counter - no Vec growth
- Image lookup uses **fixed-size hash table** with linear probing and bitmask
- **Reuse everything**

---

### 4. Single-Threaded Agent Model

```
┌─────────────────┐         ┌──────────────────┐
│  SenderAgent    │         │  ReceiverAgent   │
│  ┌────────────┐ │         │  ┌─────────────┐ │
│  │ io_uring   │ │         │  │ io_uring    │ │
│  │ ring       │ │         │  │ ring        │ │
│  ├────────────┤ │         │  ├─────────────┤ │
│  │ SlotPool   │ │         │  │ BufRingPool │ │
│  │ Endpoints[]│ │         │  │ Endpoints[] │ │
│  │ Pubs[]     │ │         │  │ Images[]    │ │
│  └────────────┘ │         │  └─────────────┘ │
└─────────────────┘         └──────────────────┘
    Thread A                     Thread B
```

| Rule                                         | Rationale                                 |
|----------------------------------------------|-------------------------------------------|
| One io_uring ring per agent                  | No contention, no shared SQ/CQ            |
| One thread per agent                         | No locks, no atomic CAS in duty cycle     |
| No shared mutable state between agents       | Eliminates all synchronization overhead   |
| `Agent::do_work()` called in tight spin loop | Bounded, deterministic work per iteration |
| `CachedNanoClock` - one clock read per cycle | Avoids repeated `clock_gettime` syscalls  |

#### Duty Cycle Structure

```
SenderAgent::do_work(now_ns):
    1. do_send()        → scan publications, send heartbeat / setup / data
    2. poll_control()   → reap CQEs for incoming SM / NAK (ratio-gated)
    3. process_sm_nak() → update publication state from received control

ReceiverAgent::do_work(now_ns):
    1. poll_data()             → reap CQEs, dispatch data frames to images
    2. send_control_messages() → generate SM / NAK, submit via io_uring
```

---

### 5. Cache-Oriented Design

#### CPU Memory Latency Reference

| Level | Latency |
|-------|---------|
| L1    | ~1 ns   |
| L2    | ~3 ns   |
| L3    | ~10 ns  |
| RAM   | ~100 ns |

#### Rules

| Rule                           | Implementation                                             |
|--------------------------------|------------------------------------------------------------|
| Contiguous memory for hot data | `Vec<SendSlot>`, `Vec<RecvSlot>` - linear in memory        |
| Cache-line aligned slots       | `#[repr(C, align(64))]` on `RecvSlot`, `SendSlot`          |
| CQE batch to stack buffer      | `[MaybeUninit<(u64,i32,u32)>; 256]` - 4 KiB, L1-resident   |
| Avoid pointer chasing          | Flat arrays + index, not `HashMap` / `BTreeMap`            |
| Power-of-two ring buffers      | Bitmask indexing, never modulo                             |
| Hot/cold separation            | Wire fields in `repr(C, packed)` struct, metadata separate |
| Pre-sized pending queues       | `[PendingSm; 64]` inline in endpoint - no heap indirection |
| Image hash table               | `[u16; 256]` with bitmask probing - no `HashMap`           |

---

### 6. Pinned Memory & Slot Lifecycle

Slots are allocated once and **never moved**. The kernel holds raw pointers into slot memory between SQE submission and
CQE completion.

```
alloc_send()          prepare_send()           CQE reaped              free_send()
┌──────┐  slot_idx   ┌──────────┐  SQE submit ┌──────────┐  callback  ┌──────┐
│ Free │────────────▶│ Owned    │────────────▶│ InFlight │──────────▶ │ Free │
└──────┘             └──────────┘             └──────────┘            └──────┘

[!] WHILE InFlight:
- DO NOT move the slot (Vec must not reallocate)
- DO NOT modify hdr / iov / addr / buffer
- DO NOT free the slot
- Kernel holds raw pointers into these fields
```

| Rule                                        | Status   |
|---------------------------------------------|----------|
| Pre-allocate all slots in `SlotPool::new()` | Required |
| Track state with `SlotState` enum           | Required |
| Free send slot only after CQE confirms      | Required |
| Never push to slot Vecs after init          | Required |
| Never hold `&mut Slot` while InFlight       | Required |
| Never assume CQE order matches SQE order    | Required |

#### `RecvSlot` - Stable Pointer Initialization

```rust
// Called once after Vec is fully constructed (never resized after)
unsafe fn init_stable_pointers(&mut self) {
    self.iov.iov_base = self.buffer.as_mut_ptr() as *mut _;
    self.iov.iov_len = MAX_RECV_BUFFER;
    self.hdr.msg_name = &mut self.addr as *mut _ as *mut _;
    self.hdr.msg_iov = &mut self.iov;
    self.hdr.msg_iovlen = 1;
    // ... pointers are stable for the lifetime of the slot
}
```

#### `SendSlot` - Per-Send Setup

```rust
// Called each time a send is prepared (points to external data)
unsafe fn prepare_send(&mut self, data_ptr: *const u8, data_len: usize, dest: ...) {
    self.iov.iov_base = data_ptr as *mut _;
    self.iov.iov_len = data_len;
    // copy dest addr, set msg_name / msg_iov / msg_iovlen
    self.state = SlotState::InFlight;
}
```

---

### 7. Multishot Receive with Provided Buffer Ring

Traditional io_uring receive requires re-submitting an SQE after every received packet. Multishot `RecvMsgMulti` with a
**provided buffer ring** eliminates this entirely:

```
                ┌─────────────────────────────────────┐
                │        Provided Buffer Ring         │
                │  ┌─────┬─────┬─────┬─────┬─────┐    │
 kernel picks → │  │ buf0│ buf1│ buf2│ ... │bufN │    │
                │  └─────┴─────┴─────┴─────┴─────┘    │
                │       ↑ tail (AtomicU16, Release)   │
                └─────────────────────────────────────┘
                         │                    ↑
                   CQE reaped            return_buf()
                   (bid in flags)        (recycle)
```

| Property         | Traditional RecvMsg          | Multishot + buf_ring          |
|------------------|------------------------------|-------------------------------|
| SQEs per message | 1 (re-arm each time)         | 0 (stays armed across CQEs)   |
| Buffer ownership | Userspace pre-assigns        | Kernel picks from shared ring |
| Reap cost        | ~1100 ns (reap+rearm+submit) | ~14.8 ns (reap+recycle only)  |
| Syscall per recv | 1 (`io_uring_enter`)         | 0 (only on multishot restart) |

#### buf_ring Lifecycle

1. **Init**: Allocate `entries` buffers (page-aligned ring, cache-aligned data). Register with kernel.
2. **Steady state**: Kernel picks buffer → delivers CQE with `buffer_select(flags)` → userspace processes →
   `return_buf(bid)` publishes tail with `Release` ordering.
3. **Multishot restart**: Only if kernel terminates multishot (e.g., `-ENOBUFS`). Re-submit one SQE.

---

### 8. Sequence & Term Arithmetic

Aeron uses 32-bit signed term IDs and offsets that wrap. Naive comparison breaks at `i32::MAX`.

| Rule                             | Status        |
|----------------------------------|---------------|
| Use `wrapping_sub` for all diffs | Required      |
| Half-range rule for ordering     | Required      |
| Bare `>` / `<` comparison        | **Forbidden** |
| Non-wrapping subtraction         | **Forbidden** |
| Test all wrap-around cases       | Required      |

```rust
// [PASS] CORRECT - wrapping comparison with half-range check
fn is_past(proposed: i32, current: i32) -> bool {
    let diff = proposed.wrapping_sub(current);
    diff > 0 && diff < (i32::MAX / 2)
}

// [PASS] CORRECT - gap detection in receiver image
let gap = received_offset.wrapping_sub(expected_offset);
if gap > 0 & & gap < (i32::MAX / 2) {
// gap detected - schedule NAK
}

// [FAIL] FORBIDDEN
if term_id_a > term_id_b { ... }      // wraps at i32::MAX
let diff = term_id_a - term_id_b;     // panics on overflow in debug
```

---

### 9. Ring Buffer Discipline

#### Capacity

- **MUST** be power-of-two (io_uring rings, buf_ring entries, image hash table)

#### Indexing

```rust
// [PASS] Correct - bitmask
let index = offset & (capacity - 1);
let slot  = (hash.wrapping_add(probe)) & IMAGE_INDEX_MASK;
let tail  = local_tail & mask;

// [FAIL] Forbidden
let index = offset % capacity;
```

| Rule                              | Status   |
|-----------------------------------|----------|
| Never use `%` in hot path         | Required |
| buf_ring entries: power-of-two    | Required |
| Image hash table: power-of-two    | Required |
| Round-robin wrap: branch, not mod | Required |

#### Round-Robin Without Modulo

```rust
// [PASS] CORRECT - branch-based wrap
idx += 1;
if idx > = len {
idx = 0;
}

// [FAIL] FORBIDDEN
idx = (idx + 1) % len;
```

---

### 10. Wire Format (Little-Endian)

Aeron wire format is **little-endian** and parsed via `repr(C, packed)` overlay on x86_64.

#### Compile-Time Guard

```rust
#[cfg(not(target_endian = "little"))]
compile_error!("aeron-rs assumes little-endian byte order.");
```

#### Zero-Copy Parse (< 0.5 ns per header)

```rust
#[repr(C, packed)]
pub struct FrameHeader {
    pub frame_length: i32,
    pub version: u8,
    pub flags: u8,
    pub frame_type: u16,
}

impl FrameHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&FrameHeader> {
        if buf.len() < FRAME_HEADER_LENGTH { return None; }
        // SAFETY: repr(C, packed), all bit patterns valid, LE target
        Some(unsafe { &*(buf.as_ptr() as *const FrameHeader) })
    }
}
```

#### Write Path

```rust
pub fn write(&self, buf: &mut [u8]) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            self as *const Self as *const u8,
            buf.as_mut_ptr(),
            HEADER_LENGTH,
        );
    }
}
```

#### Rules

| Rule                                      | Status     |
|-------------------------------------------|------------|
| `repr(C, packed)` overlay on LE target    | Required   |
| Compile-time reject on big-endian         | Required   |
| No pointer cast without documented safety | Required   |
| Fixed-size headers only                   | Required   |
| Header parse target                       | **< 1 ns** |

---

### 11. Error Handling in Hot Path

`std::io::Error` **heap-allocates** (`Box<Custom>` internally). The hot path uses a stack-only error type:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollError {
    RingFull,
    NoSendSlot,
    NotRegistered,
    Os(i32),   // raw errno - no String, no Box
}
```

| Rule                                    | Status        |
|-----------------------------------------|---------------|
| No `std::io::Error` in duty cycle       | Required      |
| `PollError` is `Copy` + stack-only      | Required      |
| `unwrap()` / `expect()` in CQE handling | **Forbidden** |
| Error paths marked `#[cold]`            | Preferred     |

---

### 12. Dispatch & Polymorphism

#### No `dyn` in Hot Path

All poller dispatch is monomorphized via generic type parameter:

```rust
// [PASS] Monomorphized - zero-cost dispatch
pub fn send_heartbeat<P: TransportPoller>(&mut self, poller: &mut P, ...) { ... }
pub fn send_pending<P: TransportPoller>(&mut self, poller: &mut P, ...) { ... }

// [FAIL] FORBIDDEN
pub fn send_heartbeat(&mut self, poller: &mut dyn TransportPoller, ...) { ... }
```

#### O(1) Image Lookup (No HashMap)

```rust
const MAX_IMAGES: usize = 256;
const IMAGE_INDEX_MASK: usize = MAX_IMAGES - 1;

fn image_hash(session_id: i32, stream_id: i32) -> usize {
    let h = (session_id as u32).wrapping_mul(0x9E37_79B9)
        ^ (stream_id as u32).wrapping_mul(0x517C_C1B7);
    (h as usize) & IMAGE_INDEX_MASK
}
```

| Rule                                        | Status   |
|---------------------------------------------|----------|
| No `dyn Trait` in duty cycle                | Required |
| No `HashMap` / `BTreeMap` in hot path       | Required |
| Pre-sized flat arrays with bitmask probing  | Required |
| Transport index = endpoint index (O(1) map) | Required |

---

### 13. Unsafe Policy

The driver uses `unsafe` in three controlled categories:

| Category             | Examples                               | Justification                    |
|----------------------|----------------------------------------|----------------------------------|
| Wire format parse    | `repr(C, packed)` pointer cast         | Zero-copy, LE-only, benchmarked  |
| io_uring interaction | SQE push, CQE reap, buf_ring access    | Required by `io_uring` crate API |
| Pinned slot pointers | `init_stable_pointers`, `prepare_send` | Kernel holds raw ptrs into slots |

#### Allowed Only If

| Condition                | Required |
|--------------------------|----------|
| Measurable gain proven   | Yes      |
| Benchmarked before/after | Yes      |
| Invariants documented    | Yes      |
| Safety comment at site   | Yes      |

> [FAIL] **Unsafe without documented safety invariant - reject.**

---

### 14. Performance Budget

#### Per-Message Send Path

| Stage                   | Budget      | Measured               |
|-------------------------|-------------|------------------------|
| Frame header build      | < 5 ns      | ~1.5 ns                |
| Slot alloc              | < 5 ns      | ~3.0 ns                |
| prepare_send            | < 5 ns      | (included in SQE push) |
| SQE push                | < 10 ns     | ~3.8 ns                |
| **Total userspace**     | **< 25 ns** | **~8.3 ns**            |
| io_uring_enter + kernel | < 1 us      | ~874 ns                |

#### Per-Message Receive Path (Multishot)

| Stage                     | Budget      | Measured     |
|---------------------------|-------------|--------------|
| CQE harvest + parse       | < 20 ns     | ~13.7 ns     |
| buf_ring recycle          | < 5 ns      | (included)   |
| Frame dispatch + callback | < 10 ns     | ~0.5 ns      |
| **Total userspace**       | **< 35 ns** | **~14.2 ns** |

#### Control Message Generation

| Stage                    | Budget  | Measured |
|--------------------------|---------|----------|
| Heartbeat build + submit | < 15 ns | ~4.5 ns  |
| NAK build + write        | < 10 ns | ~1.5 ns  |
| SM generation + submit   | < 15 ns | ~4.5 ns  |

#### Allocation Budget

| Phase          | Heap allocation allowed?                 |
|----------------|------------------------------------------|
| Initialization | Yes (SlotPool, BufRing, Vec with_capacity) |
| Steady state   | NO **Zero**                              |
| Teardown       | Yes (Drop impls)                         |

#### Investigation Triggers

```
p99 > p50 x 2       - investigate tail latency
> 10% regression     - requires justification + benchmark comparison
any allocation       - reject (use heaptrack / DHAT to verify)
new syscall          - reject (use strace to verify)
```

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                        aeron-rs Media Driver                        │
├───────────────────────────┬─────────────────────────────────────────┤
│       SenderAgent         │          ReceiverAgent                  │
│  ┌─────────────────────┐  │  ┌───────────────────────────────────┐  │
│  │ UringTransportPoller│  │  │ UringTransportPoller              │  │
│  │  ├─ IoUring ring    │  │  │  ├─ IoUring ring                  │  │
│  │  ├─ SlotPool        │  │  │  ├─ BufRingPool (provided bufs)   │  │
│  │  │   └─ SendSlot[]  │  │  │  ├─ SlotPool                      │  │
│  │  └─ TransportEntry[]│  │  │  │   └─ SendSlot[] (for SM/NAK)   │  │
│  ├─────────────────────┤  │  │  └─ TransportEntry[]              │  │
│  │ SendChannelEndpoint │  │  ├───────────────────────────────────┤  │
│  │  ├─ UdpTransport    │  │  │ ReceiveChannelEndpoint            │  │
│  │  ├─ heartbeat_buf   │  │  │  ├─ UdpTransport                  │  │
│  │  ├─ setup_buf       │  │  │  ├─ sm_buf / nak_buf              │  │
│  │  ├─ recent_sm[]     │  │  │  ├─ pending_sms[]                 │  │
│  │  └─ recent_naks[]   │  │  │  └─ pending_naks[]                │  │
│  ├─────────────────────┤  │  ├───────────────────────────────────┤  │
│  │ PublicationEntry[]  │  │  │ ImageEntry[256] + hash index      │  │
│  └─────────────────────┘  │  └───────────────────────────────────┘  │
│       Thread A            │           Thread B                      │
└───────────────────────────┴─────────────────────────────────────────┘
```

### Data Flow

```
Sender:
  Publication → DataHeader build → scratch buf → submit_send()
    → SlotPool::alloc_send() → prepare_send() → SQE push
    → flush() (io_uring_enter) → kernel sendmsg → CQE → free_send()

Receiver:
  Kernel recvmsg (multishot) → CQE with buf_ring buffer
    → RecvMsgOut::parse → on_message() → DataFrameHandler::on_data()
    → image update (wrapping arithmetic) → return_buf()
    → SM/NAK generation → submit_send() → flush()
```

---

## Final Principle

| Layer        | Role               |
|--------------|--------------------|
| Architecture | Defines intent     |
| Enforcement  | Ensures invariants |
| Benchmarks   | Validates reality  |

```
Architecture defines intent.
Enforcement ensures invariants.
Benchmarks validate reality.

If it allocates in steady state - reject.
If it adds a syscall to the duty cycle - reject.
If it moves pinned memory - reject.
If it can't be measured - it doesn't exist.
```
