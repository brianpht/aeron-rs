// Benchmark: end-to-end single-message RTT through the full Aeron transport stack.
//
// Measures the round-trip latency of:
//   Publication::offer() -> SenderAgent::do_work() -> io_uring sendmsg
//   -> UDP loopback -> io_uring recvmsg -> ReceiverAgent::do_work()
//   -> SharedLogBuffer image write -> SM reply -> io_uring sendmsg
//   -> UDP loopback -> io_uring recvmsg -> SenderAgent::do_work()
//   -> SM reap -> sender_limit advance
//
// Each Criterion iteration = one full RTT. Both agents run on the same
// thread with interleaved do_work() calls for deterministic measurement.
//
// Completion signal: sender_limit advances past its pre-offer value,
// confirming the receiver processed the frame and the sender reaped
// the resulting SM.
//
// Comparable to: Aeron C `EmbeddedPingPong` (~8-12 us p50, ~15-20 us p99).
//
// Target: p50 < 10 us, p99 < 20 us on loopback.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use aeron_rs::agent::Agent;
use aeron_rs::agent::receiver::ReceiverAgent;
use aeron_rs::agent::sender::SenderAgent;
use aeron_rs::context::DriverContext;
use aeron_rs::frame::DATA_HEADER_LENGTH;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::network_publication::OfferError;
use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

// -- Constants --

const SESSION_ID: i32 = 0x44;
const STREAM_ID: i32 = 30;
const INITIAL_TERM_ID: i32 = 0;
const BENCH_TERM_LENGTH: u32 = 256 * 1024; // 256 KiB per partition
const BENCH_MTU: u32 = 1408;
/// Payload bytes per frame (MTU minus the data header).
const PAYLOAD_SIZE: usize = BENCH_MTU as usize - DATA_HEADER_LENGTH;
/// Duration to spin both agents for Setup/SM handshake completion.
const WARMUP_DURATION: Duration = Duration::from_millis(200);
/// Maximum duty-cycle spins per RTT before giving up (safety valve).
const MAX_SPIN_CYCLES: u32 = 10_000;
/// Number of RTTs per iter_custom batch (for the batch variant).
const BATCH_SIZE: u64 = 1000;

// -- Helpers --

/// Build a DriverContext tuned for latency measurement.
///
/// Key settings:
/// - send_sm_on_data = true: receiver sends SM immediately on data receipt
///   (no waiting for sm_interval timer). This minimizes the SM delay and
///   makes each RTT deterministic.
/// - sm_interval_ns = 0: SM eligible on every send_control_messages call.
/// - send_duty_cycle_ratio = 1: poll SM every duty cycle (no skip).
/// - receiver_window covers full 4-partition buffer so sender_limit
///   always has room to advance.
fn make_bench_ctx() -> DriverContext {
    DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 8,
        uring_send_slots: 128,
        uring_buf_ring_entries: 64,
        term_buffer_length: BENCH_TERM_LENGTH,
        // Poll SM every duty cycle for minimum latency.
        send_duty_cycle_ratio: 1,
        // Fast timers for warmup handshake.
        heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
        // Zero interval: SM eligible on every send_control_messages call.
        sm_interval_ns: 0,
        // Adaptive SM: queue SM immediately on data receipt.
        send_sm_on_data: true,
        // Large receiver_window: each SM advances sender_limit by the
        // consumption delta, ensuring measurable progress per frame.
        receiver_window: Some(BENCH_TERM_LENGTH as i32 * 4),
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
    let recv_channel =
        UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
    let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
    let remote = recv_channel.remote_data;
    let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
        .expect("open recv transport");
    let recv_port = recv_transport.bound_addr.port();

    let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
    let mut receiver = ReceiverAgent::new(&ctx).expect("receiver agent");
    receiver.add_endpoint(recv_ep).expect("add recv endpoint");

    // -- Sender side: connect to receiver's bound port --
    let send_uri = format!("aeron:udp?endpoint=127.0.0.1:{recv_port}");
    let send_channel = UdpChannel::parse(&send_uri).expect("parse send channel");
    let send_remote = send_channel.remote_data;
    let mut send_transport = UdpChannelTransport::open(&send_channel, &local, &send_remote, &ctx)
        .expect("open send transport");
    // Connect the socket so sendmsg works without a per-frame dest_addr.
    send_transport
        .connect(&send_remote)
        .expect("connect send transport");

    let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
    let mut sender = SenderAgent::new(&ctx).expect("sender agent");
    sender.add_endpoint(send_ep).expect("add send endpoint");
    let pub_idx = sender
        .add_publication(
            0,
            SESSION_ID,
            STREAM_ID,
            INITIAL_TERM_ID,
            BENCH_TERM_LENGTH,
            BENCH_MTU,
        )
        .expect("add publication");

    // -- Warmup: spin both agents until Setup/SM handshake completes --
    let deadline = Instant::now() + WARMUP_DURATION;
    while Instant::now() < deadline {
        let _ = sender.do_work();
        let _ = receiver.do_work();
    }

    (sender, receiver, pub_idx)
}

/// Execute one full RTT: offer -> send -> recv -> SM -> reap.
///
/// Returns the number of duty-cycle spins required.
/// Zero-allocation in the measured path.
#[inline]
fn run_one_rtt(
    sender: &mut SenderAgent,
    receiver: &mut ReceiverAgent,
    pub_idx: usize,
    payload: &[u8],
) -> u32 {
    // Record baseline: sender_limit before offering.
    let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

    // Offer 1 frame to the term buffer.
    if let Some(publication) = sender.publication_mut(pub_idx) {
        match publication.offer(black_box(payload)) {
            Ok(_) => {}
            Err(OfferError::AdminAction) => {
                // Term rotation - retry once. AdminAction means the
                // publication crossed a term boundary; the next offer
                // will succeed on the new partition.
                if let Some(p) = sender.publication_mut(pub_idx) {
                    let _ = p.offer(black_box(payload));
                }
            }
            Err(_) => {}
        }
    }

    // Spin: interleave duty cycles until SM advances sender_limit.
    //
    // Each cycle runs three do_work calls:
    //   1. sender.do_work  - scans + sends data frame via io_uring
    //   2. receiver.do_work - reaps data CQE, writes image, queues + sends SM
    //   3. sender.do_work  - reaps SM CQE, updates sender_limit
    //
    // The second sender.do_work is essential: after the first call sends
    // data (bytes_sent > 0), the duty_cycle_counter gates control polling.
    // The receiver then sends the SM back. The second sender call has
    // bytes_sent = 0 and polls control immediately, ensuring the SM CQE
    // is reaped in the same cycle. Without it, the SM CQE may not be
    // visible until the next cycle's io_uring_enter, causing extra spins
    // or stalls (especially with small/header-only frames where the
    // kernel's CQE posting for multishot RecvMsg can lag behind the
    // sender's CQ read).
    //
    // The loop exits as soon as sender_limit advances, confirming the
    // full round-trip completed.
    let mut cycles = 0u32;
    loop {
        let _ = sender.do_work();
        let _ = receiver.do_work();
        let _ = sender.do_work();
        cycles = cycles.wrapping_add(1);

        let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        // Wrapping comparison (half-range): new_limit advanced past old_limit.
        let diff = new_limit.wrapping_sub(old_limit);
        if diff > 0 && diff < (i64::MAX >> 1) {
            break;
        }
        if cycles >= MAX_SPIN_CYCLES {
            break; // safety valve - should never trigger on healthy loopback
        }
    }

    cycles
}

// -- Benchmarks --

/// Primary RTT benchmark: single-message round-trip latency (1408B payload).
///
/// Uses iter_custom for precise wall-clock timing. Each Criterion iteration
/// runs exactly one RTT. Criterion collects many samples and computes
/// the statistical distribution (mean, p50, p99 from the confidence interval).
///
/// Comparable to: Aeron C `EmbeddedPingPong` (~8-12 us p50, ~15-20 us p99).
fn bench_e2e_latency(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();
    let payload = [0xABu8; PAYLOAD_SIZE];

    let mut group = c.benchmark_group("e2e_latency");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("single_msg_rtt", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            }
            start.elapsed()
        });
    });

    group.finish();
}

/// Batch RTT benchmark: amortized average RTT over a batch of messages.
///
/// Groups BATCH_SIZE round-trips per Criterion iteration to reduce
/// per-sample measurement overhead. Reports the average RTT cost
/// across sustained traffic (not single-shot).
fn bench_e2e_latency_batch(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();
    let payload = [0xABu8; PAYLOAD_SIZE];

    let mut group = c.benchmark_group("e2e_latency_batch");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("1k_msgs_rtt_avg", |b| {
        b.iter_custom(|iters| {
            let total_rtts = iters * BATCH_SIZE;
            let start = Instant::now();
            for _ in 0..total_rtts {
                run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            }
            start.elapsed()
        });
    });

    group.finish();
}

/// Minimal-payload RTT benchmark: header-only frame (0-byte payload).
///
/// Isolates protocol overhead from payload copy cost by sending the
/// smallest valid data frame (DATA_HEADER_LENGTH only, no payload).
fn bench_e2e_latency_minimal(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();
    // Empty payload - measures pure protocol RTT overhead.
    let payload: [u8; 0] = [];

    let mut group = c.benchmark_group("e2e_latency_minimal");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("header_only_rtt", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            }
            start.elapsed()
        });
    });

    group.finish();
}

/// Small-payload RTT benchmark: 64-byte payload.
///
/// Exercises a different frame alignment and term-fill cadence than the
/// full MTU benchmark. With DATA_HEADER_LENGTH=32 + 64 = 96 bytes raw,
/// aligned to 128 bytes per frame (4x FRAME_ALIGNMENT). This produces
/// more frames per term before rotation, stressing the receiver's
/// consumption tracking at a higher frame rate than full-MTU.
fn bench_e2e_latency_small(c: &mut Criterion) {
    let (mut sender, mut receiver, pub_idx) = setup_agents();
    let payload = [0xABu8; 64];

    let mut group = c.benchmark_group("e2e_latency_small");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("small_64B_rtt", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            }
            start.elapsed()
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_e2e_latency,
    bench_e2e_latency_batch,
    bench_e2e_latency_minimal,
    bench_e2e_latency_small,
);
criterion_main!(benches);
