//! Throughput measurement - full Aeron transport stack.
//!
//! Sends data frames through the complete Aeron protocol:
//!   Publication::offer() -> SenderAgent -> io_uring sendmsg -> UDP loopback
//!   -> io_uring recvmsg -> ReceiverAgent -> image write
//!   -> Subscription::poll() (counted).
//!
//! The main thread interleaves offer() bursts with poll() draining.
//! The three agents (conductor, sender, receiver) run on dedicated threads,
//! processing the network path asynchronously.
//!
//! The subscriber handles UDP packet loss via gap-skip: when the receiver
//! writes frames out of order (due to loss + retransmit), the subscriber
//! detects gaps and skips past them to continue delivering subsequent data.
//! Pad frames (term boundary fill) are also silently skipped.
//!
//! Primary metric: **send rate** (offer -> sender agent -> io_uring -> UDP).
//! Receive rate via Subscription::poll() is reported but will be lower due
//! to UDP packet loss under burst load on loopback. Loss percentage reflects
//! unrecoverable gaps (retransmit + gap-skip).
//!
//! Targets (vs Aeron C EmbeddedThroughput ~2-3 M msg/s):
//!   send rate >= 500K msg/s with MTU payload through full stack
//!
//! ```sh
//! cargo run --release --example throughput
//! cargo run --release --example throughput -- --duration 10 --burst 128
//! cargo run --release --example throughput -- --json
//! ```

use std::time::{Duration, Instant};

use aeron_rs::client::MediaDriver;
use aeron_rs::context::DriverContext;
use aeron_rs::frame::DATA_HEADER_LENGTH;
use aeron_rs::media::network_publication::OfferError;

// -- Defaults --

const DEFAULT_DURATION_SECS: u64 = 5;
const DEFAULT_BURST: usize = 64;
/// Payload size per message (excluding DATA_HEADER_LENGTH).
/// Default matches MTU for maximum per-frame throughput (minimizes
/// header overhead and avoids send-slot exhaustion with small frames).
const PAYLOAD_SIZE: usize = 1376; // 1408 (MTU) - 32 (DATA_HEADER_LENGTH)
const ENDPOINT: &str = "aeron:udp?endpoint=127.0.0.1:40210";
const STREAM_ID: i32 = 10;

// -- Main --

fn main() {
    // Parse CLI args.
    let args: Vec<String> = std::env::args().collect();
    let mut duration_secs = DEFAULT_DURATION_SECS;
    let mut burst = DEFAULT_BURST;
    let mut json = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--duration" if i + 1 < args.len() => {
                duration_secs = args[i + 1].parse().unwrap_or(DEFAULT_DURATION_SECS);
                i += 2;
            }
            "--burst" if i + 1 < args.len() => {
                burst = args[i + 1].parse().unwrap_or(DEFAULT_BURST);
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    let ctx = DriverContext {
        // Fast heartbeat/SM for quick Setup handshake.
        heartbeat_interval_ns: Duration::from_millis(5).as_nanos() as i64,
        sm_interval_ns: Duration::from_millis(10).as_nanos() as i64,
        // Adaptive SM: queue SM immediately on data receipt.
        send_sm_on_data: true,
        // Poll control every duty cycle for maximum throughput.
        send_duty_cycle_ratio: 1,
        // Aggressive idle strategy so agents wake up quickly under load.
        idle_strategy_max_spins: 100,
        idle_strategy_max_yields: 50,
        idle_strategy_min_park_ns: 100,    // 100 ns
        idle_strategy_max_park_ns: 10_000, // 10 us
        // Large send slot pool: each sender_scan frame consumes one slot.
        // Must exceed frames-per-term to avoid silent frame drops during scan.
        uring_send_slots: 1024,
        // Large socket buffers for burst absorption.
        socket_sndbuf: 4 * 1024 * 1024,
        socket_rcvbuf: 4 * 1024 * 1024,
        // 256 KiB term buffer: 768 KiB back-pressure headroom (~545 frames).
        // Default 64 KiB only gives 192 KiB (~136 frames), which a burst
        // of 128 nearly exhausts in a single iteration.
        term_buffer_length: 256 * 1024,
        // Receiver window: 256 KiB covers 1 full term of frames so sender_limit
        // advances sufficiently per SM round-trip. Default (term_length / 2)
        // restricts the sender to ~22 frames per SM.
        receiver_window: Some(256 * 1024),
        ..DriverContext::default()
    };

    // Frame size = header + payload (for MB/s calculation).
    let frame_size = DATA_HEADER_LENGTH + PAYLOAD_SIZE;

    let driver = MediaDriver::launch(ctx).expect("MediaDriver launch");
    let mut aeron = driver.connect().expect("Aeron connect");

    // Subscription binds to the endpoint (receiver listens here).
    let mut sub = aeron
        .add_subscription(ENDPOINT, STREAM_ID)
        .expect("add subscription");

    // Publication sends to the same endpoint.
    let mut publication = aeron
        .add_publication(ENDPOINT, STREAM_ID)
        .expect("add publication");

    let dur = Duration::from_secs(duration_secs);

    if !json {
        println!("Throughput Measurement (Full Aeron Stack)");
        println!("  endpoint:  {ENDPOINT}");
        println!("  payload:   {PAYLOAD_SIZE} B  frame: {frame_size} B");
        println!("  duration:  {duration_secs}s  burst: {burst}");
        println!();
    }

    // Wait for Setup/SM handshake.
    std::thread::sleep(Duration::from_millis(500));

    // -- Send + receive loop --
    let payload = [0xABu8; PAYLOAD_SIZE];
    let t0 = Instant::now();
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let mut backpressured: u64 = 0;

    while t0.elapsed() < dur {
        // Offer a burst of messages.
        let mut bp_this_burst = false;
        for _ in 0..burst {
            match publication.offer(&payload) {
                Ok(_) => sent += 1,
                Err(OfferError::AdminAction) => {
                    // Term rotation - retry once.
                    if publication.offer(&payload).is_ok() {
                        sent += 1;
                    }
                }
                Err(OfferError::BackPressured) => {
                    backpressured += 1;
                    bp_this_burst = true;
                    break; // Back off from this burst.
                }
                Err(_) => break,
            }
        }

        // Drain received fragments (non-blocking).
        let polled = sub.poll(|_, _, _| {}, 256);
        recvd += polled as u64;

        // If back-pressured and no data polled, yield to let agent
        // threads run. On machines with fewer cores than threads, this
        // releases the timeslice so agents can make progress.
        if bp_this_burst && polled == 0 {
            std::thread::yield_now();
        }
    }

    // Drain remaining in-flight frames.
    let drain_deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < drain_deadline {
        let n = sub.poll(|_, _, _| {}, 256);
        recvd += n as u64;
        if n == 0 {
            std::hint::spin_loop();
        }
    }

    let elapsed = t0.elapsed();

    // -- Report --
    let srate = sent as f64 / elapsed.as_secs_f64();
    let rrate = recvd as f64 / elapsed.as_secs_f64();
    let smbps = srate * frame_size as f64 / 1e6;
    let rmbps = rrate * frame_size as f64 / 1e6;
    let loss_pct = if sent > 0 {
        (1.0 - recvd as f64 / sent as f64) * 100.0
    } else {
        0.0
    };

    if json {
        // Flat JSON for CI parsing. No trailing newline in the object.
        println!(
            concat!(
                "{{",
                "\"sent\":{},",
                "\"received\":{},",
                "\"elapsed_s\":{:.3},",
                "\"send_rate_mps\":{:.2},",
                "\"recv_rate_mps\":{:.2},",
                "\"send_mbps\":{:.1},",
                "\"recv_mbps\":{:.1},",
                "\"loss_pct\":{:.2},",
                "\"backpressured\":{}",
                "}}",
            ),
            sent,
            recvd,
            elapsed.as_secs_f64(),
            srate / 1e6,
            rrate / 1e6,
            smbps,
            rmbps,
            loss_pct,
            backpressured,
        );
    } else {
        println!("Results:");
        println!("  elapsed        {:.2} s", elapsed.as_secs_f64());
        println!("  sent           {sent} msgs  ({backpressured} back-pressured)");
        println!("  received       {recvd} msgs");
        println!(
            "  send rate      {:.2} K msg/s  ({:.1} MB/s)",
            srate / 1e3,
            smbps,
        );
        println!(
            "  recv rate      {:.2} K msg/s  ({:.1} MB/s)",
            rrate / 1e3,
            rmbps,
        );
        println!("  loss           {loss_pct:.2}%");
        println!();
        let target_tag = if srate >= 500_000.0 { "PASS" } else { "FAIL" };
        println!(
            "Target: >= 500K msg/s -> {:.0}K msg/s  [{target_tag}]",
            srate / 1e3,
        );
    }

    // Cleanup.
    drop(sub);
    drop(publication);
    drop(aeron);
    let _ = driver.close();
}
