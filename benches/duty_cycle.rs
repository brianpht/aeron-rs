// Benchmark: raw cost of SenderAgent::do_work() and ReceiverAgent::do_work()
// under varying load levels.
//
// Measures the per-call cost of the agent duty cycle decomposed by load:
//   - idle:      0 frames pending (baseline: clock update, SM/heartbeat timer checks)
//   - light:     1 frame pending  (single frame scan + send or recv + dispatch)
//   - saturated: 16 frames pending (burst scan + multi-CQE reap)
//
// Sender do_work() includes: clock update, sender_scan, submit_send, flush,
//   poll_control (SM/NAK reap), heartbeat/RTTM timer checks.
//
// Receiver do_work() includes: clock update, poll_recv (io_uring CQE reap),
//   on_message dispatch (Setup/Data/SM), SM generation, NAK queue drain, flush.
//
// Setup agents run on loopback with completed Setup/SM handshake.
// Interleaved on a single thread for deterministic measurement.

use criterion::{Criterion, criterion_group, criterion_main};
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

const SESSION_ID: i32 = 0x43;
const STREAM_ID: i32 = 20;
const INITIAL_TERM_ID: i32 = 0;
const BENCH_TERM_LENGTH: u32 = 256 * 1024;
const BENCH_MTU: u32 = 1408;
const PAYLOAD_SIZE: usize = BENCH_MTU as usize - DATA_HEADER_LENGTH;
const WARMUP_DURATION: Duration = Duration::from_millis(200);

// Load levels for parameterized benchmarks.
const LOAD_LIGHT: usize = 1;
const LOAD_SATURATED: usize = 16;

// -- Helpers --

/// DriverContext tuned for duty cycle benchmarks.
/// Fast timers ensure Setup/SM handshake completes quickly during warmup.
/// Large receiver_window prevents flow control stalls during saturated benches.
fn make_bench_ctx() -> DriverContext {
    DriverContext {
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 8,
        uring_send_slots: 128,
        uring_buf_ring_entries: 64,
        term_buffer_length: BENCH_TERM_LENGTH,
        send_duty_cycle_ratio: 1,
        heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
        sm_interval_ns: 0,
        send_sm_on_data: true,
        receiver_window: Some(BENCH_TERM_LENGTH as i32 * 4),
        ..DriverContext::default()
    }
}

/// Set up SenderAgent + ReceiverAgent wired on UDP loopback with completed
/// Setup/SM handshake.
///
/// Returns (sender, receiver, pub_idx). Cold path - allocations acceptable.
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
    send_transport
        .connect(&send_remote)
        .expect("connect send transport");

    let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
    let mut sender = SenderAgent::new(&ctx).expect("sender agent");
    sender.add_endpoint(send_ep).expect("add send endpoint");
    let pub_idx = sender
        .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, BENCH_TERM_LENGTH, BENCH_MTU)
        .expect("add publication");

    // -- Warmup: spin both agents for Setup/SM handshake --
    let deadline = Instant::now() + WARMUP_DURATION;
    while Instant::now() < deadline {
        let _ = sender.do_work();
        let _ = receiver.do_work();
    }

    (sender, receiver, pub_idx)
}

/// Offer `n` frames to the publication, retrying AdminAction (term rotation).
/// Returns the number of frames successfully offered.
#[inline]
fn offer_n_frames(
    sender: &mut SenderAgent,
    pub_idx: usize,
    payload: &[u8],
    n: usize,
) -> usize {
    let mut offered = 0usize;
    let mut attempts = 0usize;
    let max_attempts = n * 4; // guard against infinite loop
    while offered < n && attempts < max_attempts {
        attempts += 1;
        if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(payload) {
                Ok(_) => offered += 1,
                Err(OfferError::AdminAction) => continue,
                Err(OfferError::BackPressured) => {
                    // Need to drain - run one duty cycle pair.
                    let _ = sender.do_work();
                    continue;
                }
                Err(_) => break,
            }
        } else {
            break;
        }
    }
    offered
}

// -- Sender Duty Cycle Benchmarks --

fn bench_sender_duty_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender_duty_cycle");
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    // Each variant gets its own agent pair to avoid cross-contamination.

    // -- idle: no frames offered, pure overhead --
    {
        let (mut sender, mut receiver, _pub_idx) = setup_agents();

        group.bench_function("idle", |b| {
            b.iter(|| {
                let work = sender.do_work();
                // Drain receiver to process any heartbeat/SM traffic.
                let _ = receiver.do_work();
                black_box(work)
            });
        });
    }

    // -- light: 1 frame offered per do_work() --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("1_frame", |b| {
            b.iter(|| {
                // Offer 1 frame (fast: just writes to term buffer).
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_LIGHT);
                // Measure: sender scans + sends 1 frame.
                let work = sender.do_work();
                // Drain receiver to prevent backpressure buildup.
                let _ = receiver.do_work();
                black_box(work)
            });
        });
    }

    // -- saturated: 16 frames offered per do_work() --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("16_frames", |b| {
            b.iter(|| {
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_SATURATED);
                let work = sender.do_work();
                let _ = receiver.do_work();
                black_box(work)
            });
        });
    }

    group.finish();
}

// -- Receiver Duty Cycle Benchmarks --

fn bench_receiver_duty_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("receiver_duty_cycle");
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    // -- idle: no data in flight, pure overhead --
    {
        let (mut sender, mut receiver, _pub_idx) = setup_agents();

        group.bench_function("idle", |b| {
            b.iter(|| {
                // Keep sender alive (heartbeats).
                let _ = sender.do_work();
                // Measure: receiver polls io_uring (no CQEs), checks SM timers.
                let work = receiver.do_work();
                black_box(work)
            });
        });
    }

    // -- light: 1 frame in flight --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("1_frame", |b| {
            b.iter(|| {
                // Inject: offer 1 frame, sender sends it to UDP.
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_LIGHT);
                let _ = sender.do_work();
                // Measure: receiver polls io_uring (1 CQE), dispatches frame,
                // writes to image, possibly generates SM.
                let work = receiver.do_work();
                black_box(work)
            });
        });
    }

    // -- saturated: 16 frames in flight --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("16_frames", |b| {
            b.iter(|| {
                // Inject: offer 16 frames, sender sends them all.
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_SATURATED);
                let _ = sender.do_work();
                // Measure: receiver reaps up to 16 CQEs, dispatches all,
                // writes to image, generates SM.
                let work = receiver.do_work();
                black_box(work)
            });
        });
    }

    group.finish();
}

// -- Combined: interleaved sender + receiver do_work() pair --

fn bench_combined_duty_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("combined_duty_cycle");
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    // -- idle: pure overhead of one full interleaved pair --
    {
        let (mut sender, mut receiver, _pub_idx) = setup_agents();

        group.bench_function("idle", |b| {
            b.iter(|| {
                let s = sender.do_work();
                let r = receiver.do_work();
                black_box((s, r))
            });
        });
    }

    // -- light: 1 frame through full send + recv cycle --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("1_frame", |b| {
            b.iter(|| {
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_LIGHT);
                let s = sender.do_work();
                let r = receiver.do_work();
                black_box((s, r))
            });
        });
    }

    // -- saturated: 16 frames through full send + recv cycle --
    {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        group.bench_function("16_frames", |b| {
            b.iter(|| {
                offer_n_frames(&mut sender, pub_idx, &payload, LOAD_SATURATED);
                let s = sender.do_work();
                let r = receiver.do_work();
                black_box((s, r))
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_sender_duty_cycle,
    bench_receiver_duty_cycle,
    bench_combined_duty_cycle,
);
criterion_main!(benches);

