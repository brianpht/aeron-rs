// Diagnostic: verify run_one_rtt completes and measure actual cycle count.
// Run with: cargo test --release --test rtt_diag -- --nocapture

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

const SESSION_ID: i32 = 0x44;
const STREAM_ID: i32 = 30;
const INITIAL_TERM_ID: i32 = 0;
const BENCH_TERM_LENGTH: u32 = 256 * 1024;
const BENCH_MTU: u32 = 1408;
const PAYLOAD_SIZE: usize = BENCH_MTU as usize - DATA_HEADER_LENGTH;
const WARMUP_DURATION: Duration = Duration::from_millis(200);

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

fn setup_agents() -> (SenderAgent, ReceiverAgent, usize) {
    let ctx = make_bench_ctx();
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

    let deadline = Instant::now() + WARMUP_DURATION;
    while Instant::now() < deadline {
        let _ = sender.do_work();
        let _ = receiver.do_work();
    }
    (sender, receiver, pub_idx)
}

#[test]
fn rtt_diagnostic() {
    let (mut sender, mut receiver, pub_idx) = setup_agents();
    let payload = [0xABu8; PAYLOAD_SIZE];

    // Check initial state
    let initial_limit = sender.publication_sender_limit(pub_idx);
    let initial_setup = sender.publication_needs_setup(pub_idx);
    eprintln!("Initial sender_limit: {:?}", initial_limit);
    eprintln!("Initial needs_setup: {:?}", initial_setup);
    eprintln!("Receiver image_count: {}", receiver.image_count());

    // Try 10 RTTs with detailed logging
    for rtt_num in 0..10 {
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        // Offer
        let offer_result = if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(&payload) {
                Ok(pos) => format!("Ok({})", pos),
                Err(e) => format!("Err({:?})", e),
            }
        } else {
            "no publication".to_string()
        };

        // Spin
        let start = Instant::now();
        let mut cycles = 0u32;
        let max_cycles = 100u32;
        loop {
            let s_work = sender.do_work().unwrap_or(0);
            let r_work = receiver.do_work().unwrap_or(0);
            cycles += 1;

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);

            if cycles <= 5 || cycles == max_cycles {
                eprintln!(
                    "  RTT#{} cycle={}: s_work={}, r_work={}, old_limit={}, new_limit={}, diff={}",
                    rtt_num, cycles, s_work, r_work, old_limit, new_limit, diff
                );
            }

            if diff > 0 && diff < (i64::MAX >> 1) {
                let elapsed = start.elapsed();
                eprintln!(
                    "RTT#{}: DONE in {} cycles, {:?}, offer={}, old={}, new={}",
                    rtt_num, cycles, elapsed, offer_result, old_limit, new_limit
                );
                break;
            }
            if cycles >= max_cycles {
                let elapsed = start.elapsed();
                eprintln!(
                    "RTT#{}: TIMEOUT after {} cycles, {:?}, offer={}, old={}, new={}",
                    rtt_num, cycles, elapsed, offer_result, old_limit,
                    sender.publication_sender_limit(pub_idx).unwrap_or(0)
                );
                break;
            }
        }
    }
}

