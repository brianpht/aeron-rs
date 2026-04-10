// Benchmark: end-to-end throughput through the full Aeron transport stack.
//
// Measures the hot-path cost of:
//   Publication::offer() -> SenderAgent::do_work() -> io_uring sendmsg
//   -> UDP loopback -> io_uring recvmsg -> ReceiverAgent::do_work()
//   -> SharedLogBuffer image write -> SM reply -> sender_limit update
//
// Both agents run on the same thread with interleaved do_work() calls
// for deterministic, jitter-free measurement.
//
// Comparable to: Aeron C `EmbeddedThroughput` (same payload, same metric).
//
// Target: >= 3 M msg/s with 1408-byte frames on loopback.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use aeron_rs::agent::receiver::ReceiverAgent;
use aeron_rs::agent::sender::SenderAgent;
use aeron_rs::agent::Agent;
use aeron_rs::context::DriverContext;
use aeron_rs::frame::DATA_HEADER_LENGTH;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::network_publication::OfferError;
use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

// -- Constants --

const SESSION_ID: i32 = 0x42;
const STREAM_ID: i32 = 10;
const INITIAL_TERM_ID: i32 = 0;
const BENCH_TERM_LENGTH: u32 = 256 * 1024; // 256 KiB per partition
const BENCH_MTU: u32 = 1408;
/// Payload bytes per frame (MTU minus the data header).
const PAYLOAD_SIZE: usize = BENCH_MTU as usize - DATA_HEADER_LENGTH;
/// Number of messages per Criterion iteration.
const BATCH_SIZE: usize = 1000;
/// Maximum offer attempts per burst before calling do_work().
const BURST_LIMIT: usize = 16;
/// Duration to spin both agents for Setup/SM handshake completion.
const WARMUP_DURATION: Duration = Duration::from_millis(200);
/// Trailing duty-cycle pairs after all offers to drain in-flight frames.
const DRAIN_CYCLES: usize = 128;

// -- Helpers --

/// Build a DriverContext tuned for benchmark throughput.
/// Fast timers ensure the Setup/SM handshake completes quickly during warmup.
fn make_bench_ctx() -> DriverContext {
    DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 8,
        uring_send_slots: 128,
        uring_buf_ring_entries: 64,
        term_buffer_length: BENCH_TERM_LENGTH,
        // Poll SM every duty cycle (no skip) for maximum throughput.
        send_duty_cycle_ratio: 1,
        // Fast timers for warmup handshake.
        heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
        sm_interval_ns: Duration::from_millis(1).as_nanos() as i64,
        ..DriverContext::default()
    }
}

/// Set up a SenderAgent and ReceiverAgent wired together on UDP loopback.
///
/// Returns (sender, receiver, pub_idx) with a completed Setup/SM handshake.
/// Cold path - allocations and socket setup are acceptable here.
fn setup_agents() -> (SenderAgent, ReceiverAgent, usize) {
    let ctx = make_bench_ctx();

    // -- Receiver side: bind to ephemeral port --
    let recv_channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0")
        .expect("parse recv channel");
    let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
    let remote = recv_channel.remote_data;
    let recv_transport =
        UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
            .expect("open recv transport");
    let recv_port = recv_transport.bound_addr.port();

    let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
    let mut receiver = ReceiverAgent::new(&ctx).expect("receiver agent");
    receiver.add_endpoint(recv_ep).expect("add recv endpoint");

    // -- Sender side: connect to receiver's bound port --
    let send_uri = format!("aeron:udp?endpoint=127.0.0.1:{recv_port}");
    let send_channel = UdpChannel::parse(&send_uri).expect("parse send channel");
    let send_remote = send_channel.remote_data;
    let mut send_transport =
        UdpChannelTransport::open(&send_channel, &local, &send_remote, &ctx)
            .expect("open send transport");
    // Connect the socket so sendmsg works without a per-frame dest_addr.
    send_transport
        .connect(&send_remote)
        .expect("connect send transport");

    let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
    let mut sender = SenderAgent::new(&ctx).expect("sender agent");
    sender.add_endpoint(send_ep).expect("add send endpoint");
    let pub_idx = sender
        .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, BENCH_TERM_LENGTH, BENCH_MTU)
        .expect("add publication");

    // -- Warmup: spin both agents until Setup/SM handshake completes --
    //
    // The sender sends Setup frames on heartbeat_interval_ns (1ms).
    // The receiver creates an image on on_setup and sends SM immediately.
    // The sender processes SM in poll_control, clears needs_setup, advances sender_limit.
    let deadline = Instant::now() + WARMUP_DURATION;
    while Instant::now() < deadline {
        let _ = sender.do_work();
        let _ = receiver.do_work();
    }

    (sender, receiver, pub_idx)
}

// -- Benchmarks --

fn bench_e2e_throughput(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();

    // Pre-fill payload on the stack (cold path, outside iter).
    let payload = [0xABu8; PAYLOAD_SIZE];

    let mut group = c.benchmark_group("e2e_throughput");
    group.throughput(Throughput::Elements(BATCH_SIZE as u64));
    group.warm_up_time(Duration::from_secs(3));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("1k_msgs_1408B", |b| {
        b.iter(|| {
            let mut offered = 0usize;

            while offered < BATCH_SIZE {
                // Burst: attempt up to BURST_LIMIT offers before yielding
                // to duty cycles. Minimizes do_work overhead per message.
                let burst_end = (offered + BURST_LIMIT).min(BATCH_SIZE);
                while offered < burst_end {
                    let pub_ref = sender.publication_mut(pub_idx);
                    match pub_ref {
                        Some(publication) => match publication.offer(black_box(&payload)) {
                            Ok(_pos) => {
                                offered += 1;
                            }
                            Err(OfferError::AdminAction) => {
                                // Term rotation - retry immediately.
                                continue;
                            }
                            Err(OfferError::BackPressured) => {
                                // Need duty cycles to drain.
                                break;
                            }
                            Err(_) => break,
                        },
                        None => break,
                    }
                }

                // Interleaved duty cycles: sender scans + sends, receiver
                // receives + sends SM. Both on same thread for determinism.
                let _ = sender.do_work();
                let _ = receiver.do_work();
            }

            // Drain: ensure in-flight frames complete and SM feedback arrives.
            for _ in 0..DRAIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
            }

            black_box(offered)
        });
    });

    group.finish();
}

fn bench_e2e_throughput_bytes(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();

    let payload = [0xABu8; PAYLOAD_SIZE];
    let total_bytes = BATCH_SIZE as u64 * (DATA_HEADER_LENGTH + PAYLOAD_SIZE) as u64;

    let mut group = c.benchmark_group("e2e_throughput_bytes");
    group.throughput(Throughput::Bytes(total_bytes));
    group.warm_up_time(Duration::from_secs(3));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("1k_msgs_1408B", |b| {
        b.iter(|| {
            let mut offered = 0usize;

            while offered < BATCH_SIZE {
                let burst_end = (offered + BURST_LIMIT).min(BATCH_SIZE);
                while offered < burst_end {
                    let pub_ref = sender.publication_mut(pub_idx);
                    match pub_ref {
                        Some(publication) => match publication.offer(black_box(&payload)) {
                            Ok(_pos) => {
                                offered += 1;
                            }
                            Err(OfferError::AdminAction) => continue,
                            Err(OfferError::BackPressured) => break,
                            Err(_) => break,
                        },
                        None => break,
                    }
                }

                let _ = sender.do_work();
                let _ = receiver.do_work();
            }

            for _ in 0..DRAIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
            }

            black_box(offered)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_e2e_throughput, bench_e2e_throughput_bytes);
criterion_main!(benches);

