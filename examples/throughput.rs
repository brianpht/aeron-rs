//! Throughput measurement — 1408-byte data frames via io_uring → std recv.
//!
//! Sends data frames from an io_uring-backed sender to a std::net::UdpSocket
//! receiver on loopback. Measures sustained send rate, receive rate, and loss.
//!
//! Target: ≥ 3 M msg/s with 1408B payload (vs Aeron C ~2–3 M msg/s).
//!
//! ```sh
//! cargo run --release --example throughput
//! cargo run --release --example throughput -- --duration 10 --burst 128
//! ```

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use aeron_rs::context::DriverContext;
use aeron_rs::frame::*;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::TransportPoller;
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;

// ──────────────────── Defaults ────────────────────

const MSG_LEN: usize = 1408;
const FRAME_LEN: usize = DATA_HEADER_LENGTH + MSG_LEN;
const DEFAULT_DURATION_SECS: u64 = 5;
const DEFAULT_BURST: usize = 64;

// ──────────────────── Helpers ────────────────────

fn build_frame() -> Vec<u8> {
    let hdr = DataHeader {
        frame_header: FrameHeader {
            frame_length: FRAME_LEN as i32,
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
    let mut buf = vec![0u8; FRAME_LEN];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const DataHeader as *const u8,
            buf.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }
    // Fill payload with a recognisable pattern.
    for i in DATA_HEADER_LENGTH..buf.len() {
        buf[i] = (i & 0xFF) as u8;
    }
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

// ──────────────────── Main ────────────────────

fn main() {
    // Parse simple CLI args.
    let args: Vec<String> = std::env::args().collect();
    let mut duration_secs = DEFAULT_DURATION_SECS;
    let mut burst = DEFAULT_BURST;
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
            _ => { i += 1; }
        }
    }

    let ctx = DriverContext {
        uring_ring_size: 4096,
        uring_recv_slots_per_transport: 4,
        uring_send_slots: 512,
        socket_sndbuf: 8 * 1024 * 1024,
        socket_rcvbuf: 8 * 1024 * 1024,
        ..DriverContext::default()
    };

    // ── Sender: io_uring ──
    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let ch = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut transport =
        UdpChannelTransport::open(&ch, &local, &ch.remote_data, &ctx).unwrap();
    let tidx = poller.add_transport(&mut transport).unwrap();

    // ── Receiver: std socket in a separate thread ──
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_nonblocking(true).unwrap();
    let _ = rx.set_read_timeout(Some(Duration::from_millis(1)));
    let rx_addr = rx.local_addr().unwrap();

    let dest = addr_to_storage(rx_addr);
    let frame = build_frame();
    let dur = Duration::from_secs(duration_secs);

    println!("┌──────────────────────────────────────────────┐");
    println!("│           Throughput Measurement             │");
    println!("├──────────────────────────────────────────────┤");
    println!("│ payload:  {MSG_LEN} B  frame: {FRAME_LEN} B              │");
    println!("│ duration: {duration_secs}s      burst: {burst:<20}│");
    println!("│ recv @ {:<37} │", format!("{rx_addr}"));
    println!("└──────────────────────────────────────────────┘");
    println!();

    // Spawn receiver thread.
    let recv_deadline = Instant::now() + dur + Duration::from_secs(1);
    let recv_handle = std::thread::spawn(move || {
        let mut total: u64 = 0;
        let mut buf = [0u8; 65536];
        while Instant::now() < recv_deadline {
            match rx.recv_from(&mut buf) {
                Ok((n, _)) if n >= DATA_HEADER_LENGTH => {
                    total += 1;
                }
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // If we're past the main send window, break.
                    if Instant::now() > recv_deadline - Duration::from_millis(500) {
                        break;
                    }
                }
                _ => {}
            }
        }
        total
    });

    // ── Send loop ──
    let t0 = Instant::now();
    let mut sent: u64 = 0;
    let mut errs: u64 = 0;

    while t0.elapsed() < dur {
        for _ in 0..burst {
            match poller.submit_send(tidx, &frame, Some(&dest)) {
                Ok(()) => sent += 1,
                Err(_) => {
                    errs += 1;
                    // Drain CQEs to free send slots.
                    let _ = poller.poll_recv(|_| {});
                    break;
                }
            }
        }
        let _ = poller.flush();
        let _ = poller.poll_recv(|_| {});
    }
    // Final flush.
    let _ = poller.flush();
    let _ = poller.poll_recv(|_| {});

    let elapsed = t0.elapsed();
    let recvd = recv_handle.join().unwrap_or(0);

    // ── Report ──
    let srate = sent as f64 / elapsed.as_secs_f64();
    let rrate = recvd as f64 / elapsed.as_secs_f64();
    let smbps = srate * FRAME_LEN as f64 / 1e6;
    let rmbps = rrate * FRAME_LEN as f64 / 1e6;
    let loss_pct = if sent > 0 {
        (1.0 - recvd as f64 / sent as f64) * 100.0
    } else {
        0.0
    };

    println!("Results:");
    println!("  elapsed      {:.2} s", elapsed.as_secs_f64());
    println!("  sent         {sent} msgs  ({errs} submit errors)");
    println!("  received     {recvd} msgs");
    println!(
        "  send rate    {:.2} M msg/s  ({:.1} MB/s)",
        srate / 1e6,
        smbps
    );
    println!(
        "  recv rate    {:.2} M msg/s  ({:.1} MB/s)",
        rrate / 1e6,
        rmbps
    );
    println!("  loss         {loss_pct:.2}%");

    println!();
    println!(
        "Target: ≥ 3 M msg/s → {:.2} M msg/s  {}",
        srate / 1e6,
        if srate >= 3e6 { "✓" } else { "✗" }
    );
}

