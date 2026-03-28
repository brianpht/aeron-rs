# Copilot Instructions

> **Zero-copy, io_uring-native Aeron media driver in Rust.**
>
> ⚠️ If a change triggers allocation in steady state, moves a pinned buffer, or adds syscall in the duty cycle → *
*REJECT**.

---

## Critical Rules (Auto-Reject)

```
❌ Mutex / RwLock in agent duty cycle
❌ HashMap in hot path → use pre-sized flat array + index
❌ % (modulo) for ring / term index → use & (capacity - 1)
❌ unwrap() / expect() in parsing or CQE handling
❌ Trait object (dyn) in duty cycle → monomorphize or enum dispatch
❌ Allocation (Vec::push growth, Box, String, format!) inside duty cycle
❌ Sequence comparison using > or < → use wrapping_sub + half-range
❌ Vec resize / realloc while io_uring slots are in-flight
❌ Pointer cast for wire format parsing → use from_le_bytes field-by-field
❌ Host-endian assumption in protocol frames
❌ alert() / prompt() in any HTML tooling
❌ SeqCst atomic ordering in hot path
❌ std::io::Error construction in hot path (heap-allocates)
```

---

## Architecture Invariants

```
┌─────────────┐  SQE   ┌─────────────────────┐  io_uring_enter ┌────────┐
│ Agent       │───────▶│ UringTransportPoller│────────────────▶│ Kernel │
│ (single-    │  CQE   │  SlotPool (pinned)  │◀────────────────│        │
│  threaded)  │◀───────│  TransportEntry[]   │                 └────────┘
└─────────────┘        └─────────────────────┘
```

- **One io_uring ring per agent** (Sender has one, Receiver has one)
- **One thread per agent** — no shared mutable state between agents
- **SlotPool is allocated once at init** — Vec never resized after construction
- **Slots are pinned by invariant** — kernel holds raw pointers into slot memory between SQE submit and CQE reap

---

## Hot Path Operations

### Agent Duty Cycle

`SenderAgent::do_work` | `ReceiverAgent::do_work`

### io_uring Interaction

`UringTransportPoller::poll_recv` | `UringTransportPoller::submit_send` | `UringTransportPoller::flush`

### Wire Protocol Dispatch

`FrameHeader::parse` | `DataHeader::parse` | `classify_frame` | `SendChannelEndpoint::on_message` |
`ReceiveChannelEndpoint::on_message`

### Control Message Generation

`ReceiveChannelEndpoint::send_pending` | `SendChannelEndpoint::send_heartbeat`

**Requirements**: Zero-allocation, O(1) per message, cache-local, no syscall except `io_uring_enter`, single-writer

---

## Slot Lifecycle (CRITICAL)

```
alloc_recv()          prepare_recv()           CQE reaped              free_recv()
┌──────┐  slot_idx   ┌──────────┐  SQE submit ┌──────────┐  callback  ┌──────┐
│ Free │────────────▶│ Owned    │────────────▶│ InFlight │──────────▶ │ Free │
└──────┘             └──────────┘             └──────────┘            └──────┘

⚠️ WHILE InFlight:
- DO NOT move the slot (Vec must not reallocate)
- DO NOT modify hdr / iov / addr / buffer
- DO NOT free the slot
- Kernel holds raw pointers into these fields
```

### Rules

```
✅ Pre-allocate all slots in SlotPool::new()
✅ Track state with SlotState enum (Free | InFlight)
✅ Re-submit recv SQE immediately after CQE reap (keeps ring full)
✅ Free send slot only after CQE confirms completion

❌ NEVER push to recv_slots / send_slots Vec after init
❌ NEVER hold a &mut RecvSlot while it is InFlight
❌ NEVER assume CQE order matches SQE submission order
```

---

## Sequence & Term Arithmetic

```rust
// ✅ CORRECT — wrapping arithmetic for term_id progression
fn term_id_compare(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

fn is_past_term(proposed: i32, current: i32) -> bool {
    proposed.wrapping_sub(current) > 0
        && proposed.wrapping_sub(current) < (i32::MAX / 2)
}

// ✅ CORRECT — term offset indexing
let index = (term_offset as u32 & (term_length - 1)) as usize;

// ❌ FORBIDDEN
if term_id_a > term_id_b { ... }     // wraps at i32::MAX
let idx = offset % term_length;       // not power-of-two safe
let diff = term_id_a - term_id_b;     // panics on overflow in debug
```

---

## Wire Format

```rust
// ✅ CORRECT — field-by-field little-endian decode
let frame_length = i32::from_le_bytes(buf[0..4].try_into().ok()?);
let version = buf[4];
let flags = buf[5];
let frame_type = u16::from_le_bytes(buf[6..8].try_into().ok()?);

// ✅ ACCEPTABLE (with documented safety) — repr(C, packed) overlay
//    Only when: target is little-endian, struct is #[repr(C, packed)],
//    all bit patterns valid, alignment handled
let hdr = unsafe { &*(buf.as_ptr() as *const FrameHeader) };

// ❌ FORBIDDEN
let hdr: FrameHeader = std::ptr::read(buf.as_ptr() as *const _);  // UB if misaligned
let val = *(buf.as_ptr() as *const i32);                           // alignment U