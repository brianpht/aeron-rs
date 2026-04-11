//! Ping-Pong RTT measurement - full Aeron transport stack.
//!
//! Self-contained latency test exercising the complete Aeron protocol:
//!   Publication::offer() -> SenderAgent -> io_uring sendmsg -> UDP loopback
//!   -> io_uring recvmsg -> ReceiverAgent -> image write
//!   -> Subscription::poll() -> echo -> Publication::offer()
//!   -> (same full path in reverse) -> Subscription::poll() (RTT complete).
//!
//! Uses two channel endpoints (ping + pong) with a self-reflecting loop
//! on the main thread: poll the ping subscription, echo via the pong
//! publication, then poll the pong subscription to complete the RTT.
//!
//! Targets (vs Aeron C EmbeddedPingPong ~8-12 us p50 / ~15-20 us p99):
//!   p50  < 15 us  (threaded 2-hop RTT through full stack)
//!   p99  < 50 us
//!
//! ```sh
//! cargo run --release --example ping_pong
//! cargo run --release --example ping_pong -- --warmup 5000 --samples 200000
//! ```

use std::time::{Duration, Instant};

use aeron_rs::client::MediaDriver;
use aeron_rs::context::DriverContext;
use aeron_rs::media::network_publication::OfferError;

// -- Defaults --

const DEFAULT_WARMUP: usize = 1_000;
const DEFAULT_SAMPLES: usize = 100_000;

/// Ping channel - ping subscription listens here.
const PING_ENDPOINT: &str = "aeron:udp?endpoint=127.0.0.1:40200";
/// Pong channel - pong subscription listens here.
const PONG_ENDPOINT: &str = "aeron:udp?endpoint=127.0.0.1:40201";
/// Distinct stream IDs so each subscription only takes its own images
/// from the shared SubscriptionBridge (which filters by stream_id).
const PING_STREAM_ID: i32 = 1;
const PONG_STREAM_ID: i32 = 2;

/// Maximum time to wait for a single poll before declaring timeout.
const POLL_TIMEOUT: Duration = Duration::from_secs(2);

// -- Helpers --

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    let idx = ((sorted.len() as f64) * pct / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// -- Main --

fn main() {
    // Parse CLI args.
    let args: Vec<String> = std::env::args().collect();
    let mut warmup = DEFAULT_WARMUP;
    let mut samples = DEFAULT_SAMPLES;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--warmup" if i + 1 < args.len() => {
                warmup = args[i + 1].parse().unwrap_or(DEFAULT_WARMUP);
                i += 2;
            }
            "--samples" if i + 1 < args.len() => {
                samples = args[i + 1].parse().unwrap_or(DEFAULT_SAMPLES);
                i += 2;
            }
            _ => { i += 1; }
        }
    }

    let ctx = DriverContext {
        // Fast heartbeat/SM for quick Setup handshake.
        heartbeat_interval_ns: Duration::from_millis(5).as_nanos() as i64,
        sm_interval_ns: Duration::from_millis(10).as_nanos() as i64,
        // Adaptive SM: queue SM immediately on data receipt.
        send_sm_on_data: true,
        // Poll control every duty cycle for minimum latency.
        send_duty_cycle_ratio: 1,
        // Aggressive idle strategy for low-latency measurement.
        // More spinning and shorter parks reduce agent wake-up jitter.
        idle_strategy_max_spins: 100,
        idle_strategy_max_yields: 50,
        idle_strategy_min_park_ns: 100,       // 100 ns
        idle_strategy_max_park_ns: 10_000,    // 10 us
        ..DriverContext::default()
    };

    // Launch MediaDriver with all three agents on dedicated threads.
    let driver = MediaDriver::launch(ctx).expect("MediaDriver launch");
    let mut aeron = driver.connect().expect("Aeron connect");

    // Ping direction: publication sends to ping endpoint, subscription
    // listens on ping endpoint. The main thread polls this subscription
    // to receive pings and echo them back.
    let mut pub_ping = aeron
        .add_publication(PING_ENDPOINT, PING_STREAM_ID)
        .expect("add ping publication");
    let mut sub_ping = aeron
        .add_subscription(PING_ENDPOINT, PING_STREAM_ID)
        .expect("add ping subscription");

    // Pong direction: publication sends to pong endpoint, subscription
    // listens on pong endpoint. After echoing a ping via pub_pong, the
    // main thread polls sub_pong to complete the RTT.
    let mut pub_pong = aeron
        .add_publication(PONG_ENDPOINT, PONG_STREAM_ID)
        .expect("add pong publication");
    let mut sub_pong = aeron
        .add_subscription(PONG_ENDPOINT, PONG_STREAM_ID)
        .expect("add pong subscription");

    println!("Ping-Pong RTT Measurement (Full Aeron Stack)");
    println!("  ping channel: {PING_ENDPOINT}");
    println!("  pong channel: {PONG_ENDPOINT}");
    println!("  warmup: {warmup}  samples: {samples}");
    println!();

    // Wait for Setup/SM handshake on both channels.
    // With fast timers (5ms heartbeat, 10ms SM), 500ms is generous.
    std::thread::sleep(Duration::from_millis(500));

    let mut latencies: Vec<u64> = Vec::with_capacity(samples);

    for round in 0..(warmup + samples) {
        let payload = (round as u32).to_le_bytes();
        let start = Instant::now();

        // Step 1: Send ping via pub_ping.
        offer_bounded(&mut pub_ping, &payload);

        // Step 2: Receive ping on sub_ping, echo back via pub_pong.
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut echoed = false;
        while !echoed && Instant::now() < deadline {
            sub_ping.poll(
                |data, _, _| {
                    // Echo received ping data back as a pong.
                    offer_bounded(&mut pub_pong, data);
                    echoed = true;
                },
                1,
            );
            if !echoed {
                std::hint::spin_loop();
            }
        }
        if !echoed {
            eprintln!("ping receive timeout at round {round}");
            continue;
        }

        // Step 3: Receive pong on sub_pong - RTT complete.
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut got_pong = false;
        while !got_pong && Instant::now() < deadline {
            let n = sub_pong.poll(|_, _, _| {}, 1);
            if n > 0 {
                got_pong = true;
            } else {
                std::hint::spin_loop();
            }
        }
        if !got_pong {
            eprintln!("pong receive timeout at round {round}");
            continue;
        }

        let elapsed_ns = start.elapsed().as_nanos() as u64;
        if round >= warmup {
            latencies.push(elapsed_ns);
        }
    }

    // -- Compute and print results --
    latencies.sort_unstable();
    let n = latencies.len();
    if n == 0 {
        eprintln!("No measurements recorded");
        drop(sub_pong);
        drop(sub_ping);
        drop(pub_pong);
        drop(pub_ping);
        drop(aeron);
        let _ = driver.close();
        return;
    }

    let min = latencies[0];
    let max = latencies[n - 1];
    let avg: u64 = latencies.iter().sum::<u64>() / n as u64;
    let p50 = percentile(&latencies, 50.0);
    let p90 = percentile(&latencies, 90.0);
    let p99 = percentile(&latencies, 99.0);
    let p999 = percentile(&latencies, 99.9);

    println!("Results ({n} samples):");
    println!("  min   = {:>10.2} us", min as f64 / 1e3);
    println!("  avg   = {:>10.2} us", avg as f64 / 1e3);
    println!("  p50   = {:>10.2} us", p50 as f64 / 1e3);
    println!("  p90   = {:>10.2} us", p90 as f64 / 1e3);
    println!("  p99   = {:>10.2} us", p99 as f64 / 1e3);
    println!("  p99.9 = {:>10.2} us", p999 as f64 / 1e3);
    println!("  max   = {:>10.2} us", max as f64 / 1e3);

    println!();
    println!("Target comparison (Aeron C EmbeddedPingPong baseline):");
    let p50_tag = if p50 < 15_000 { "PASS" } else { "FAIL" };
    let p99_tag = if p99 < 50_000 { "PASS" } else { "FAIL" };
    println!(
        "  p50:  {:>8.2} us  [{p50_tag}]  target < 15 us",
        p50 as f64 / 1e3,
    );
    println!(
        "  p99:  {:>8.2} us  [{p99_tag}]  target < 50 us",
        p99 as f64 / 1e3,
    );

    // Cleanup.
    drop(sub_pong);
    drop(sub_ping);
    drop(pub_pong);
    drop(pub_ping);
    drop(aeron);
    let _ = driver.close();
}

/// Offer a payload with bounded retry on AdminAction / BackPressured.
///
/// Retries up to 10_000 iterations. For an example running on loopback
/// this is always sufficient; in production code use a deadline-based loop.
fn offer_bounded(publication: &mut aeron_rs::client::Publication, payload: &[u8]) {
    for _ in 0..10_000 {
        match publication.offer(payload) {
            Ok(_) => return,
            Err(OfferError::AdminAction) => continue,
            Err(OfferError::BackPressured) => {
                std::hint::spin_loop();
            }
            Err(_) => return,
        }
    }
}
