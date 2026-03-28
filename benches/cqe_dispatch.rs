//! Benchmark: CQE dispatch per message.
//!
//! Measures the cost of the io_uring completion reap + callback dispatch
//! path in `UringTransportPoller::poll_recv`. We send packets from a std
//! socket, then time a single `poll_recv` that reaps them.
//!
//! Target: < 50 ns/msg (vs Aeron C conductor ~100–300 ns).

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::net::{SocketAddr, UdpSocket};

use aeron_rs::context::DriverContext;
use aeron_rs::frame::*;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::{RecvMessage, TransportPoller};
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

// ──────────────────── Helpers ────────────────────

fn build_data_frame() -> [u8; DATA_HEADER_LENGTH] {
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
    let mut buf = [0u8; DATA_HEADER_LENGTH];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const DataHeader as *const u8,
            buf.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }
    buf
}

struct TestFixture {
    poller: UringTransportPoller,
    sender: UdpSocket,
    bound: SocketAddr,
    frame: [u8; DATA_HEADER_LENGTH],
}

fn make_fixture(recv_slots: usize) -> TestFixture {
    let ctx = DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: recv_slots,
        uring_send_slots: 16,
        ..DriverContext::default()
    };
    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let remote = channel.remote_data;
    let mut transport = UdpChannelTransport::open(&channel, &local, &remote, &ctx).unwrap();
    let bound = transport.bound_addr;
    let _t_idx = poller.add_transport(&mut transport).unwrap();
    // Leak transport to keep fd alive for the bench lifetime.
    std::mem::forget(transport);

    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let frame = build_data_frame();

    TestFixture { poller, sender, bound, frame }
}

// ──────────────────── Benchmarks ────────────────────

/// Single message: send 1 packet, wait for delivery, measure poll_recv + callback.
fn bench_cqe_dispatch_single(c: &mut Criterion) {
    let mut f = make_fixture(16);

    c.bench_function("cqe_dispatch: single msg", |b| {
        b.iter_batched(
            || {
                // Setup: inject one packet, wait for kernel to complete recvmsg.
                f.sender.send_to(&f.frame, f.bound).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(50));
            },
            |()| {
                let mut count = 0u32;
                let _ = f.poller.poll_recv(|_msg: RecvMessage<'_>| {
                    count += 1;
                });
                black_box(count);
            },
            BatchSize::SmallInput,
        );
    });
}

/// Burst: send 16 packets, wait, measure a single poll_recv that reaps them all.
/// Divide reported time by 16 for per-message cost.
fn bench_cqe_dispatch_burst_16(c: &mut Criterion) {
    let burst = 16usize;
    let mut f = make_fixture(32);

    c.bench_function("cqe_dispatch: burst 16 msgs (total)", |b| {
        b.iter_batched(
            || {
                for _ in 0..burst {
                    let _ = f.sender.send_to(&f.frame, f.bound);
                }
                std::thread::sleep(std::time::Duration::from_micros(200));
            },
            |()| {
                let mut count = 0u32;
                let _ = f.poller.poll_recv(|_msg: RecvMessage<'_>| {
                    count += 1;
                });
                black_box(count);
            },
            BatchSize::SmallInput,
        );
    });
}

/// Pure CPU cost of the UserData decode (bit-shift extraction from u64).
/// No io_uring involved — isolates the decode logic.
fn bench_userdata_decode(c: &mut Criterion) {
    // Simulates the decode path inside poll_recv:
    //   transport_idx = bits[63:48], op = bits[47:40], slot_idx = bits[39:24]
    let encoded: u64 = ((5u64) << 48) | ((1u64) << 40) | ((42u64) << 24);

    c.bench_function("cqe_dispatch: UserData decode", |b| {
        b.iter(|| {
            let ud = black_box(encoded);
            let tidx = (ud >> 48) as u16;
            let op = (ud >> 40) as u8;
            let sidx = (ud >> 24) as u16;
            black_box((tidx, op, sidx));
        });
    });
}

/// CQE batch harvest simulation — iterate a pre-filled (u64, i32) buffer
/// and extract UserData fields. Measures the reap loop without kernel interaction.
fn bench_cqe_batch_iterate(c: &mut Criterion) {
    const BATCH: usize = 256;
    let mut cqe_buf = [(0u64, 0i32); BATCH];
    for i in 0..BATCH {
        cqe_buf[i] = (
            ((0u64) << 48) | ((1u64) << 40) | ((i as u64) << 24),
            1440i32,
        );
    }

    c.bench_function("cqe_dispatch: batch iterate 256 CQEs", |b| {
        b.iter(|| {
            let mut bytes = 0i64;
            let mut msgs = 0u32;
            for &(ud_raw, ret) in black_box(&cqe_buf[..]) {
                let _tidx = (ud_raw >> 48) as u16;
                let op = (ud_raw >> 40) as u8;
                let _sidx = (ud_raw >> 24) as u16;
                if op == 1 && ret > 0 {
                    bytes += ret as i64;
                    msgs += 1;
                }
            }
            black_box((bytes, msgs));
        });
    });
}

criterion_group!(
    benches,
    bench_cqe_dispatch_single,
    bench_cqe_dispatch_burst_16,
    bench_userdata_decode,
    bench_cqe_batch_iterate,
);
criterion_main!(benches);

