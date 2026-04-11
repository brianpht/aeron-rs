//! Benchmark: publication offer latency.
//!
//! Measures the hot-path cost of:
//! - Frame header construction into a scratch buffer
//! - `TransportPoller::submit_send` (slot alloc + SQE push)
//! - `SendChannelEndpoint::send_heartbeat` (build + submit)
//! - `NetworkPublication::offer()` (term buffer write)
//! - `NetworkPublication::sender_scan()` (frame walk)
//! - Combined offer + sender_scan round-trip
//!
//! Target: offer < 40 ns, sender_scan < 30 ns per frame.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::net::SocketAddr;

use aeron_rs::context::DriverContext;
use aeron_rs::frame::*;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::network_publication::{NetworkPublication, OfferError};
use aeron_rs::media::poller::TransportPoller;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

// ──────────────────── Constants ────────────────────

const BENCH_TERM_LENGTH: u32 = 64 * 1024; // 64 KiB
const BENCH_MTU: u32 = 1408;

// ──────────────────── Helpers ────────────────────

fn make_poller_and_transport() -> (UringTransportPoller, usize, UdpChannelTransport) {
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
    (poller, t_idx, transport)
}

fn make_publication() -> NetworkPublication {
    NetworkPublication::new(42, 10, 0, BENCH_TERM_LENGTH, BENCH_MTU).expect("valid bench params")
}

// ──────────────────── Benchmarks ────────────────────

/// Pure CPU cost of building a DataHeader into a scratch buffer.
/// No io_uring, no syscall - isolates the frame construction.
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
    let (mut poller, t_idx, _transport) = make_poller_and_transport();

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

/// NetworkPublication::offer() with an empty payload (header-only frame).
/// Measures: partition lookup (bitmask), header build, memcpy into term buffer,
/// position update. No io_uring interaction.
fn bench_publication_offer_empty(c: &mut Criterion) {
    let mut pub_ = make_publication();

    c.bench_function("offer: NetworkPublication::offer (empty payload)", |b| {
        b.iter(|| {
            match pub_.offer(black_box(&[])) {
                Ok(pos) => {
                    black_box(pos);
                }
                Err(OfferError::AdminAction) => {
                    // Term rotated - retry immediately (expected periodically).
                    let _ = pub_.offer(&[]);
                }
                Err(OfferError::BackPressured) => {
                    // Drain sender_position so publisher can continue.
                    pub_.sender_scan(u32::MAX, |_, _| {});
                    let _ = pub_.offer(&[]);
                }
                Err(_) => {}
            }
        });
    });
}

/// NetworkPublication::offer() with a 64-byte payload (typical small message).
fn bench_publication_offer_64b(c: &mut Criterion) {
    let mut pub_ = make_publication();
    let payload = [0xABu8; 64];

    c.bench_function("offer: NetworkPublication::offer (64B payload)", |b| {
        b.iter(|| match pub_.offer(black_box(&payload)) {
            Ok(pos) => {
                black_box(pos);
            }
            Err(OfferError::AdminAction) => {
                let _ = pub_.offer(&payload);
            }
            Err(OfferError::BackPressured) => {
                pub_.sender_scan(u32::MAX, |_, _| {});
                let _ = pub_.offer(&payload);
            }
            Err(_) => {}
        });
    });
}

/// NetworkPublication::offer() with a 1024-byte payload (medium message).
fn bench_publication_offer_1k(c: &mut Criterion) {
    let mut pub_ = make_publication();
    let payload = [0xCDu8; 1024];

    c.bench_function("offer: NetworkPublication::offer (1KiB payload)", |b| {
        b.iter(|| match pub_.offer(black_box(&payload)) {
            Ok(pos) => {
                black_box(pos);
            }
            Err(OfferError::AdminAction) => {
                let _ = pub_.offer(&payload);
            }
            Err(OfferError::BackPressured) => {
                pub_.sender_scan(u32::MAX, |_, _| {});
                let _ = pub_.offer(&payload);
            }
            Err(_) => {}
        });
    });
}

/// sender_scan: walk a single frame after offer.
/// Measures: position calculation, partition lookup, frame length read, emit
/// callback invocation, position advance.
fn bench_sender_scan_single(c: &mut Criterion) {
    let mut pub_ = make_publication();

    c.bench_function("scan: sender_scan (1 frame, re-offer each iter)", |b| {
        b.iter(|| {
            // Offer one frame then scan it.
            match pub_.offer(&[0u8; 64]) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {
                    let _ = pub_.offer(&[0u8; 64]);
                }
                Err(OfferError::BackPressured) => {
                    pub_.sender_scan(u32::MAX, |_, _| {});
                    let _ = pub_.offer(&[0u8; 64]);
                }
                Err(_) => {}
            }
            let scanned = pub_.sender_scan(black_box(BENCH_MTU), |off, data| {
                black_box(off);
                black_box(data);
            });
            black_box(scanned);
        });
    });
}

/// sender_scan: walk a batch of 16 frames.
/// Pre-fills 16 frames, then scans all in one call.
fn bench_sender_scan_batch_16(c: &mut Criterion) {
    let mut pub_ = make_publication();

    c.bench_function("scan: sender_scan (batch 16 frames)", |b| {
        b.iter(|| {
            // Fill 16 frames.
            for _ in 0..16 {
                match pub_.offer(&[0u8; 64]) {
                    Ok(_) => {}
                    Err(OfferError::AdminAction) => {
                        let _ = pub_.offer(&[0u8; 64]);
                    }
                    Err(OfferError::BackPressured) => {
                        pub_.sender_scan(u32::MAX, |_, _| {});
                        let _ = pub_.offer(&[0u8; 64]);
                    }
                    Err(_) => {}
                }
            }
            // Scan all 16.
            let scanned = pub_.sender_scan(black_box(u32::MAX), |off, data| {
                black_box(off);
                black_box(data);
            });
            black_box(scanned);
        });
    });
}

/// Combined offer + sender_scan: write one frame and immediately scan it.
/// Represents the steady-state single-message latency through the term buffer.
fn bench_offer_then_scan(c: &mut Criterion) {
    let mut pub_ = make_publication();

    c.bench_function("offer+scan: offer 64B then sender_scan", |b| {
        let payload = [0xABu8; 64];
        b.iter(|| {
            // Offer.
            match pub_.offer(black_box(&payload)) {
                Ok(pos) => {
                    black_box(pos);
                }
                Err(OfferError::AdminAction) => {
                    let _ = pub_.offer(&payload);
                }
                Err(OfferError::BackPressured) => {
                    pub_.sender_scan(u32::MAX, |_, _| {});
                    let _ = pub_.offer(&payload);
                }
                Err(_) => {}
            }
            // Scan.
            let scanned = pub_.sender_scan(black_box(BENCH_MTU), |off, data| {
                black_box(off);
                black_box(data);
            });
            black_box(scanned);
        });
    });
}

criterion_group!(
    benches,
    bench_offer_frame_build,
    bench_offer_with_submit,
    bench_offer_heartbeat,
    bench_publication_offer_empty,
    bench_publication_offer_64b,
    bench_publication_offer_1k,
    bench_sender_scan_single,
    bench_sender_scan_batch_16,
    bench_offer_then_scan,
);
criterion_main!(benches);
