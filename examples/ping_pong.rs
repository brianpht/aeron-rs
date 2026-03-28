//! Ping-Pong RTT measurement.
//!
//! Self-contained latency test: sends a frame from an io_uring-backed
//! transport to a std::net::UdpSocket "pong" reflector on loopback,
//! then measures the full roundtrip (send → pong recv → pong echo →
//! io_uring recv).  Reports min/avg/p50/p90/p99/p99.9/max.
//!
//! Targets (vs Aeron C UDP ~8–12 µs p50 / ~15–20 µs p99):
//!   p50  < 8 µs
//!   p99  < 15 µs
//!
//! ```sh
//! cargo run --release --example ping_pong
//! cargo run --release --example ping_pong -- --warmup 5000 --samples 200000
//! ```

use std::net::{SocketAddr, UdpSocket};
use std::time::Instant;

use aeron_rs::context::DriverContext;
use aeron_rs::frame::*;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::{RecvMessage, TransportPoller};
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

// ──────────────────── Defaults ────────────────────

const DEFAULT_WARMUP: usize = 1_000;
const DEFAULT_SAMPLES: usize = 100_000;

// ──────────────────── Helpers ────────────────────

fn build_ping(seq: u32) -> [u8; DATA_HEADER_LENGTH + 4] {
    let hdr = DataHeader {
        frame_header: FrameHeader {
            frame_length: (DATA_HEADER_LENGTH + 4) as i32,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset: 0,
        session_id: 1,
        stream_id: 1,
        term_id: 0,
        reserved_value: 0,
    };
    let mut buf = [0u8; DATA_HEADER_LENGTH + 4];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const DataHeader as *const u8,
            buf.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }
    buf[DATA_HEADER_LENGTH..].copy_from_slice(&seq.to_le_bytes());
    buf
}

fn addr_to_storage(addr: SocketAddr) -> libc::sockaddr_storage {
    let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    if let SocketAddr::V4(v4) = addr {
        let sin = unsafe { &mut *(&mut s as *mut _ as *mut libc::sockaddr_in) };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = v4.port().to_be();
        sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
    }
    s
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    let idx = ((sorted.len() as f64) * pct / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ──────────────────── Main ────────────────────

fn main() {
    // Parse simple CLI args.
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
        uring_ring_size: 256,
        uring_recv_slots_per_transport: 16,
        uring_send_slots: 64,
        socket_rcvbuf: 4 * 1024 * 1024,
        socket_sndbuf: 4 * 1024 * 1024,
        ..DriverContext::default()
    };

    // ── Ping side: io_uring poller ──
    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let ch = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut t = UdpChannelTransport::open(&ch, &local, &ch.remote_data, &ctx).unwrap();
    let ping_addr = t.bound_addr;
    let tidx = poller.add_transport(&mut t).unwrap();

    // ── Pong side: std socket (spin-recv + echo) ──
    let pong = UdpSocket::bind("127.0.0.1:0").unwrap();
    pong.set_nonblocking(true).unwrap();
    let pong_addr = pong.local_addr().unwrap();
    let dest = addr_to_storage(pong_addr);

    println!("┌──────────────────────────────────────────┐");
    println!("│         Ping-Pong RTT Measurement        │");
    println!("├──────────────────────────────────────────┤");
    println!("│  ping (io_uring) @ {:<22}│", format!("{ping_addr}"));
    println!("│  pong (std UDP)  @ {:<22}│", format!("{pong_addr}"));
    println!("│  warmup: {warmup:<8} samples: {samples:<12}  │");
    println!("└──────────────────────────────────────────┘");
    println!();

    let mut latencies: Vec<u64> = Vec::with_capacity(samples);
    let mut pong_buf = [0u8; 2048];

    for round in 0..(warmup + samples) {
        let frame = build_ping(round as u32);

        let start = Instant::now();

        // 1) Send ping → pong via io_uring.
        let _ = poller.submit_send(tidx, &frame, Some(&dest));
        let _ = poller.flush();

        // 2) Pong: spin-recv + echo back.
        loop {
            match pong.recv_from(&mut pong_buf) {
                Ok((n, _)) => {
                    let _ = pong.send_to(&pong_buf[..n], ping_addr);
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::hint::spin_loop();
                }
                Err(_) => break,
            }
        }

        // 3) Recv echo via io_uring poll_recv.
        let mut done = false;
        while !done {
            let _ = poller.poll_recv(|_: RecvMessage<'_>| {
                done = true;
            });
            if !done {
                std::hint::spin_loop();
            }
        }

        let elapsed_ns = start.elapsed().as_nanos() as u64;
        if round >= warmup {
            latencies.push(elapsed_ns);
        }
    }

    // ── Compute and print results ──
    latencies.sort_unstable();
    let n = latencies.len();
    if n == 0 {
        eprintln!("No measurements recorded");
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
    println!("  min   = {:>10.2} µs", min as f64 / 1e3);
    println!("  avg   = {:>10.2} µs", avg as f64 / 1e3);
    println!("  p50   = {:>10.2} µs", p50 as f64 / 1e3);
    println!("  p90   = {:>10.2} µs", p90 as f64 / 1e3);
    println!("  p99   = {:>10.2} µs", p99 as f64 / 1e3);
    println!("  p99.9 = {:>10.2} µs", p999 as f64 / 1e3);
    println!("  max   = {:>10.2} µs", max as f64 / 1e3);

    println!();
    println!("Target comparison (Aeron C UDP baseline):");
    println!(
        "  p50:  {:>8.2} µs  {}  target <  8 µs",
        p50 as f64 / 1e3,
        if p50 < 8_000 { "✓" } else { "✗" }
    );
    println!(
        "  p99:  {:>8.2} µs  {}  target < 15 µs",
        p99 as f64 / 1e3,
        if p99 < 15_000 { "✓" } else { "✗" }
    );
}

