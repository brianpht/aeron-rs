# AERON-RS

A zero-copy, io_uring-native Aeron media driver in Rust, inspired by [Aeron](https://github.com/real-logic/aeron) by Real Logic.

## Overview

`aeron-rs` is a high-performance UDP transport layer implementing the Aeron wire protocol. It's designed for situations where you need ultra-low latency, zero-allocation messaging over UDP — such as high-frequency trading, real-time telemetry, or latency-critical distributed systems.

The entire I/O path runs through `io_uring` with multishot receive and provided buffer rings, eliminating per-message syscalls in steady state.

## Features

- **io_uring Native**: Multishot `RecvMsgMulti` + provided buffer ring — zero syscalls per received message
- **Zero-Copy Receive**: Kernel picks buffers from a shared ring; no userspace-to-kernel copy
- **Allocation-Free Hot Path**: All buffers pre-allocated at init; zero heap allocation in steady state
- **Single-Threaded Agents**: One thread per agent, one io_uring ring per agent — no locks, no contention
- **Aeron Wire Protocol**: Full frame parsing (Data, SM, NAK, Setup, RTTM, Heartbeat) in < 0.5 ns
- **Sub-Microsecond Offer Path**: ~8 ns from frame build to SQE push (userspace)
- **High Throughput**: ≥ 3 M msg/s with 1408-byte frames on commodity hardware
- **Cache-Oriented**: 64-byte aligned slots, stack-local CQE batching, flat-array dispatch

## Requirements

- **Linux** with io_uring support (kernel ≥ 5.19 for provided buffer rings)
- **x86_64** little-endian target (compile-time enforced)
- Rust 2024 edition

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
aeron-rs = { path = "." }
```

## Quick Start

### Sender Agent

```rust
use std::net::SocketAddr;
use aeron_rs::agent::Agent;
use aeron_rs::agent::sender::SenderAgent;
use aeron_rs::context::DriverContext;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

let ctx = DriverContext::default();

// Parse an Aeron channel URI and open a UDP transport.
let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40123")
    .expect("channel parse");
let local: SocketAddr = "0.0.0.0:0".parse().unwrap();
let transport = UdpChannelTransport::open(
    &channel, &local, &channel.remote_data, &ctx,
).expect("transport open");

// Create agent, register endpoint, add a publication.
let endpoint = SendChannelEndpoint::new(channel, transport);
let mut agent = SenderAgent::new(&ctx).expect("sender agent");
let ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

agent.add_publication(
    ep_idx,
    /*session_id=*/ 1001,
    /*stream_id=*/ 10,
    /*initial_term_id=*/ 0,
    /*term_length=*/ 1 << 16,
    /*mtu=*/ 1408,
);

// Run the duty cycle — heartbeats and setups are sent automatically.
loop {
    let _work = agent.do_work().expect("duty cycle");
}
```

### Receiver Agent

```rust
use std::net::SocketAddr;
use aeron_rs::agent::Agent;
use aeron_rs::agent::receiver::ReceiverAgent;
use aeron_rs::context::DriverContext;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

let ctx = DriverContext::default();

let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40123")
    .expect("channel parse");
let local: SocketAddr = "0.0.0.0:0".parse().unwrap();
let transport = UdpChannelTransport::open(
    &channel, &local, &channel.remote_data, &ctx,
).expect("transport open");

let endpoint = ReceiveChannelEndpoint::new(channel, transport);
let mut agent = ReceiverAgent::new(&ctx).expect("receiver agent");
agent.add_endpoint(endpoint).expect("add endpoint");

// Run the duty cycle — data frames are dispatched, SM/NAK sent back.
loop {
    let _work = agent.do_work().expect("duty cycle");
}
```

### Direct Poller (Low-Level)

```rust
use std::net::SocketAddr;
use aeron_rs::context::DriverContext;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::{RecvMessage, TransportPoller};
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

let ctx = DriverContext::default();
let mut poller = UringTransportPoller::new(&ctx).expect("poller");

let ch = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
let mut transport = UdpChannelTransport::open(
    &ch, &local, &ch.remote_data, &ctx,
).unwrap();
let tidx = poller.add_transport(&mut transport).unwrap();

// Submit a send.
let frame = [0xABu8; 64];
poller.submit_send(tidx, &frame, None).unwrap();
poller.flush().unwrap();

// Poll completions.
let result = poller.poll_recv(|msg: RecvMessage<'_>| {
    println!("received {} bytes from transport {}", msg.data.len(), msg.transport_idx);
});
```

## How It Works

The driver implements the Aeron media layer with io_uring replacing all traditional socket syscalls:

1. **Channel URIs**: Parsed from Aeron-style strings (`aeron:udp?endpoint=host:port`)
2. **Transports**: UDP sockets managed via `socket2`, registered with the io_uring poller
3. **Multishot Receive**: One `RecvMsgMulti` SQE stays armed across all completions — no re-arm per message
4. **Provided Buffer Ring**: The kernel picks receive buffers from a pre-registered shared ring
5. **Send Slots**: Pre-allocated `msghdr` + `iovec` structures; the kernel copies data into skb during SQE processing
6. **CQE Dispatch**: Completions are batch-harvested to a stack buffer, then dispatched via packed `UserData` encoding
7. **Agent Duty Cycle**: Single-threaded spin loop — poll CQEs, dispatch frames, generate control messages, flush SQEs

```
┌─────────────┐  SQE   ┌─────────────────────┐  io_uring_enter  ┌────────┐
│ Agent       │───────▶│ UringTransportPoller │────────────────▶│ Kernel │
│ (single-    │  CQE   │  SlotPool (pinned)   │◀────────────────│        │
│  threaded)  │◀───────│  BufRingPool (shared)│                 └────────┘
└─────────────┘        └─────────────────────┘
```

## Configuration

```rust
use aeron_rs::context::DriverContext;

let ctx = DriverContext {
    // Socket buffers
    socket_rcvbuf: 2 * 1024 * 1024,        // SO_RCVBUF
    socket_sndbuf: 2 * 1024 * 1024,        // SO_SNDBUF
    mtu_length: 1408,                       // Max frame payload

    // io_uring
    uring_ring_size: 512,                   // SQ/CQ ring entries
    uring_recv_slots_per_transport: 16,     // Recv slots per transport
    uring_send_slots: 128,                  // Total send slots
    uring_buf_ring_entries: 256,            // Provided buffer ring (power-of-two)
    uring_sqpoll: false,                    // Kernel-side SQ polling
    uring_sqpoll_idle_ms: 1000,             // SQPOLL idle timeout

    // Sender timing
    heartbeat_interval_ns: 100_000_000,     // 100 ms
    send_duty_cycle_ratio: 4,               // Send:poll ratio

    // Receiver timing
    sm_interval_ns: 200_000_000,            // Status message interval (200 ms)
    nak_delay_ns: 60_000_000,               // NAK delay (60 ms)

    ..DriverContext::default()
};
```

## Wire Protocol

Aeron wire format — little-endian, fixed-size headers, zero-copy parsed via `repr(C, packed)` overlay:

| Frame Type       | Size     | Purpose                           |
|------------------|----------|-----------------------------------|
| Data / Pad       | 32 bytes | Application data + term position  |
| Status Message   | 36 bytes | Flow control (consumption + window) |
| NAK              | 28 bytes | Loss notification (offset + length) |
| Setup            | 40 bytes | Session establishment              |
| RTTM             | 40 bytes | Round-trip time measurement        |
| Heartbeat        | 32 bytes | Keepalive (Data header, no payload) |

```rust
use aeron_rs::frame::*;

// Parse a frame from a byte buffer (< 0.5 ns)
if let Some(hdr) = FrameHeader::parse(&buf) {
    match FrameType::from(hdr.frame_type) {
        FrameType::Data => {
            let data = DataHeader::parse(&buf).unwrap();
            let payload = data.payload(&buf);
            // process payload...
        }
        FrameType::StatusMessage => {
            let sm = StatusMessage::parse(&buf).unwrap();
            // process SM...
        }
        _ => {}
    }
}
```

## Performance

Benchmarks on x86_64 Linux (Criterion):

### Userspace Operations

| Operation                       | Latency    |
|---------------------------------|------------|
| FrameHeader::parse              | ~0.3 ns    |
| DataHeader::parse               | ~0.4 ns    |
| classify_frame                  | ~0.5 ns    |
| NAK build + write               | ~1.5 ns    |
| Frame build (DataHeader)        | ~1.5 ns    |
| SlotPool alloc + free           | ~3.0 ns    |
| SQE push (alloc + prepare)      | ~3.8 ns    |
| Heartbeat build + submit        | ~4.5 ns    |
| CQE dispatch (single msg)       | ~13.7 ns   |
| CQE dispatch (16 msg burst)     | ~20.5 ns   |
| CQE batch iterate (256 CQEs)    | ~200 ns    |

### io_uring Kernel Roundtrip

| Operation                       | Latency    |
|---------------------------------|------------|
| Multishot recv reap + recycle   | ~14.8 ns   |
| submit() empty ring             | ~65 ns     |
| NOP submit + reap (single)      | ~163 ns    |
| NOP submit + reap (burst 16)    | ~371 ns    |
| UDP sendmsg + reap              | ~874 ns    |

### System-Level Targets

| Metric                       | Target          | Status |
|------------------------------|-----------------|--------|
| Throughput (1408B frames)    | ≥ 3 M msg/s     | ✅      |
| Steady-state allocation      | Zero             | ✅      |
| Syscalls per duty cycle      | 0–1              | ✅      |
| Offer path (userspace)       | < 25 ns          | ✅ (~8 ns) |
| Recv path (multishot)        | < 35 ns          | ✅ (~14 ns) |

## Performance Design

This driver is built with deterministic, low-latency performance as a core design principle. Key architectural decisions include:

- **io_uring multishot + provided buffer ring**: Zero SQE re-arm, zero per-message syscalls for receive
- **Allocation-free hot path**: All slot pools, scratch buffers, and pending queues pre-allocated at init
- **Single-threaded agent model**: One io_uring ring per agent — no locks, no atomic CAS in duty cycle
- **Pinned slot memory**: Kernel holds raw pointers into slot fields; Vec never resized after construction
- **Cache-line aligned slots**: `#[repr(C, align(64))]` for send/recv slots
- **Stack-only error type**: `PollError` is `Copy` — no `std::io::Error` heap allocation in hot path
- **Monomorphized dispatch**: Generic `<P: TransportPoller>` — no `dyn` vtable in duty cycle
- **O(1) image lookup**: Pre-sized `[u16; 256]` hash table with bitmask probing, not `HashMap`
- **Wrapping arithmetic**: All sequence/term comparisons use `wrapping_sub` + half-range check
- **Little-endian wire format**: `repr(C, packed)` overlay parsing — zero byte-swap on x86_64

For detailed performance design principles, architecture decisions, and optimization guidelines, see [docs/performance_design.md](docs/performance_design.md).

## Examples

```sh
# Wire-protocol frame roundtrip (no io_uring required)
cargo run --example frame_roundtrip

# Sender agent with heartbeat generation
cargo run --example send_heartbeat

# Receiver agent (listens for data)
cargo run --example recv_data

# Ping-pong RTT measurement (io_uring → std echo)
cargo run --example ping_pong

# Throughput measurement (≥ 3 M msg/s target)
cargo run --release --example throughput
cargo run --release --example throughput -- --duration 10 --burst 128
```

## Running Tests

```sh
# Unit tests
cargo test

# Integration tests (requires Linux with io_uring)
cargo test -- --test-threads=1

# Benchmarks
cargo bench
```

## Project Structure

```
src/
├── lib.rs                          # Public module exports
├── frame.rs                        # Wire protocol frames (parse + write)
├── clock.rs                        # Monotonic nanosecond clock
├── context.rs                      # DriverContext configuration
├── agent/
│   ├── mod.rs                      # Agent trait (do_work / on_start / on_close)
│   ├── sender.rs                   # SenderAgent — heartbeat, setup, data send
│   └── receiver.rs                 # ReceiverAgent — data dispatch, SM/NAK generation
└── media/
    ├── mod.rs                      # Media layer module exports
    ├── channel.rs                  # Aeron URI parsing (aeron:udp?endpoint=...)
    ├── transport.rs                # UDP socket management (socket2)
    ├── poller.rs                   # TransportPoller trait + PollError
    ├── uring_poller.rs             # io_uring implementation (multishot + buf_ring)
    ├── buffer_pool.rs              # SlotPool (pinned send/recv slots)
    ├── send_channel_endpoint.rs    # Send endpoint (heartbeat, setup, SM/NAK ingest)
    └── receive_channel_endpoint.rs # Recv endpoint (data dispatch, SM/NAK generation)
```

## License

BSD 3-Clause License. See [LICENSE](LICENSE) for details.

## Credits

This is a Rust implementation of the Aeron media driver, inspired by [Aeron](https://github.com/real-logic/aeron) by Real Logic.

Related projects:
- [Aeron](https://github.com/real-logic/aeron) — High-performance messaging (Java / C / C++)
- [io_uring](https://kernel.dk/io_uring.pdf) — Linux asynchronous I/O interface
- [io-uring crate](https://github.com/tokio-rs/io-uring) — Rust bindings for io_uring
- [Mechanical Sympathy](https://mechanical-sympathy.blogspot.com/) — Hardware-aware software design

