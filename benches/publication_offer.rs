//! Benchmark: publication offer latency.
//!
//! Measures the hot-path cost of constructing a data frame header into a
//! scratch buffer and submitting it via `TransportPoller::submit_send`.
//! This approximates the per-message cost of `NetworkPublication::offer()`.
//!
//! Target: < 40 ns (vs Aeron C ~40–80 ns conductor offer).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::net::SocketAddr;

use aeron_rs::context::DriverContext;
use aeron_rs::frame::*;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::TransportPoller;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

// ──────────────────── Helpers ────────────────────

fn make_poller_and_transport() -> (UringTransportPoller, usize) {
    let ctx = DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 4,
        uring_send_slots: 128,
        ..DriverContext::default()
    };
    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let remote = channel.remote_data;
    let mut transport = UdpChannelTransport::open(&channel, &local, &remote, &ctx).unwrap();
    let t_idx = poller.add_transport(&mut transport).unwrap();
    // Keep transport alive — leak it (bench scope).
    std::mem::forget(transport);
    (poller, t_idx)
}

// ──────────────────── Benchmarks ────────────────────

/// Pure CPU cost of building a DataHeader into a scratch buffer.
/// No io_uring, no syscall — isolates the frame construction.
fn bench_offer_frame_build(c: &mut Criterion) {
    let mut scratch = [0u8; DATA_HEADER_LENGTH + 1408];

    c.bench_function("offer: frame_build (DataHeader into buf)", |b| {
        b.iter(|| {
            let hdr = DataHeader {
                frame_header: FrameHeader {
                    frame_length: black_box(1440i32),
                    version: CURRENT_VERSION,
                    flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                    frame_type: FRAME_TYPE_DATA,
                },
                term_offset: black_box(0),
                session_id: black_box(42),
                stream_id: black_box(10),
                term_id: black_box(0),
                reserved_value: 0,
            };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &hdr as *const DataHeader as *const u8,
                    scratch.as_mut_ptr(),
                    DATA_HEADER_LENGTH,
                );
            }
            black_box(&scratch);
        });
    });
}

/// Full offer path: slot alloc + prepare_send + SQE push via submit_send.
/// Each iteration drains CQEs to recycle send slots.
fn bench_offer_with_submit(c: &mut Criterion) {
    let (mut poller, t_idx) = make_poller_and_transport();

    let mut scratch = [0u8; DATA_HEADER_LENGTH];
    let hdr = DataHeader {
        frame_header: FrameHeader {
            frame_length: DATA_HEADER_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset: 0,
        session_id: 42,
        stream_id: 10,
        term_id: 0,
        reserved_value: 0,
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const DataHeader as *const u8,
            scratch.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }

    c.bench_function("offer: submit_send (alloc+SQE push)", |b| {
        b.iter(|| {
            let _ = poller.submit_send(t_idx, black_box(&scratch), None);
            // Drain CQEs to free send slots for next iteration.
            let _ = poller.poll_recv(|_| {});
        });
    });
}

/// Heartbeat construction + submit through the SendChannelEndpoint codepath.
fn bench_offer_heartbeat(c: &mut Criterion) {
    let ctx = DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 4,
        uring_send_slots: 128,
        ..DriverContext::default()
    };

    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let remote = channel.remote_data;
    let transport = UdpChannelTransport::open(&channel, &local, &remote, &ctx).unwrap();
    let mut ep = SendChannelEndpoint::new(channel, transport);
    ep.register(&mut poller).unwrap();

    c.bench_function("offer: send_heartbeat (build+submit)", |b| {
        b.iter(|| {
            let _ = ep.send_heartbeat(
                &mut poller,
                black_box(42),
                black_box(10),
                black_box(0),
                black_box(0),
                None,
            );
            // Drain CQEs to free send slots.
            let _ = poller.poll_recv(|_| {});
        });
    });
}

criterion_group!(
    benches,
    bench_offer_frame_build,
    bench_offer_with_submit,
    bench_offer_heartbeat,
);
criterion_main!(benches);

