//! Benchmark: io_uring baseline latency.
//!
//! Establishes the raw io_uring overhead floor using NOP and sendmsg
//! operations. These numbers are the absolute minimum for any io_uring-based
//! operation in the driver, independent of Aeron protocol logic.
//!
//! Benchmarks:
//!   - NOP submit+reap (single / burst)  — kernel roundtrip floor
//!   - SQE push only (no submit)         — userspace ring cost
//!   - submit() with empty ring           — syscall overhead
//!   - UDP sendmsg+reap                  — real network I/O baseline

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use io_uring::{IoUring, opcode};

// ──────────────────── NOP benchmarks ────────────────────

/// Single NOP: push SQE → submit → wait for 1 CQE → reap.
/// This measures the absolute io_uring kernel roundtrip floor.
fn bench_uring_nop_single(c: &mut Criterion) {
    let mut ring = IoUring::new(256).expect("io_uring::new");

    c.bench_function("io_uring: NOP submit+reap (single)", |b| {
        b.iter(|| {
            let nop = opcode::Nop::new().build().user_data(0x42);
            unsafe {
                ring.submission().push(&nop).unwrap();
            }
            ring.submit_and_wait(1).unwrap();
            let cq = ring.completion();
            for cqe in cq {
                black_box(cqe.user_data());
            }
        });
    });
}

/// Burst of 16 NOPs: push 16 → single submit → wait → reap all.
/// Amortises syscall overhead; divide reported time by 16 for per-op cost.
fn bench_uring_nop_burst_16(c: &mut Criterion) {
    let burst = 16usize;
    let mut ring = IoUring::new(256).expect("io_uring::new");

    c.bench_function("io_uring: NOP submit+reap (burst 16)", |b| {
        b.iter(|| {
            for i in 0..burst {
                let nop = opcode::Nop::new().build().user_data(i as u64);
                unsafe {
                    ring.submission().push(&nop).unwrap();
                }
            }
            ring.submit_and_wait(burst).unwrap();
            let mut count = 0u32;
            let cq = ring.completion();
            for cqe in cq {
                black_box(cqe.user_data());
                count += 1;
            }
            black_box(count);
        });
    });
}

// ──────────────────── SQE push only ────────────────────

/// Just the userspace cost of pushing an SQE into the submission ring.
/// No io_uring_enter syscall — isolates the ring buffer write.
fn bench_uring_sqe_push_only(c: &mut Criterion) {
    let mut ring = IoUring::new(4096).expect("io_uring::new");
    let mut pushed = 0u32;

    c.bench_function("io_uring: SQE push only (no submit)", |b| {
        b.iter(|| {
            let nop = opcode::Nop::new().build().user_data(black_box(0x42));
            unsafe {
                ring.submission().push(&nop).unwrap();
            }
            pushed += 1;
            // Drain periodically to avoid SQ overflow.
            if pushed >= 256 {
                let _ = ring.submit();
                let cq = ring.completion();
                for cqe in cq {
                    black_box(cqe.user_data());
                }
                pushed = 0;
            }
        });
        // Final drain.
        let _ = ring.submit();
        let cq = ring.completion();
        for cqe in cq {
            black_box(cqe.user_data());
        }
    });
}

// ──────────────────── Empty submit ────────────────────

/// Cost of calling ring.submit() when the SQ is empty.
/// Measures the raw io_uring_enter syscall overhead.
fn bench_uring_submit_empty(c: &mut Criterion) {
    let ring = IoUring::new(256).expect("io_uring::new");

    c.bench_function("io_uring: submit (empty ring)", |b| {
        b.iter(|| {
            let n = ring.submit().unwrap();
            black_box(n);
        });
    });
}

// ──────────────────── UDP sendmsg ────────────────────

/// Real UDP sendmsg via io_uring: push SendMsg SQE → submit → reap CQE.
/// This is the actual network I/O baseline for the poller's send path.
fn bench_uring_udp_sendmsg(c: &mut Criterion) {
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let bound = sock.local_addr().unwrap();
    sock.set_nonblocking(true).unwrap();
    let fd = sock.as_raw_fd();

    // Build a minimal 64-byte UDP payload.
    let payload = [0xAAu8; 64];
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut _,
        iov_len: payload.len(),
    };

    // Target: send to ourselves on loopback.
    let mut dest_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let sin = unsafe { &mut *(&mut dest_storage as *mut _ as *mut libc::sockaddr_in) };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = bound.port().to_be();
    sin.sin_addr.s_addr = u32::to_be(0x7F000001); // 127.0.0.1

    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_name = &mut dest_storage as *mut _ as *mut libc::c_void;
    hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;

    let mut ring = IoUring::new(256).expect("io_uring::new");

    c.bench_function("io_uring: UDP sendmsg+reap", |b| {
        b.iter(|| {
            let sqe = opcode::SendMsg::new(io_uring::types::Fd(fd), &hdr)
                .build()
                .user_data(0x1);
            unsafe {
                ring.submission().push(&sqe).unwrap();
            }
            ring.submit_and_wait(1).unwrap();
            let cq = ring.completion();
            for cqe in cq {
                black_box(cqe.result());
            }
        });
    });
}

/// UDP sendmsg burst of 16 — amortised send cost.
fn bench_uring_udp_sendmsg_burst_16(c: &mut Criterion) {
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let bound = sock.local_addr().unwrap();
    sock.set_nonblocking(true).unwrap();
    let fd = sock.as_raw_fd();

    let payload = [0xAAu8; 64];
    let mut iov = libc::iovec {
        iov_base: payload.as_ptr() as *mut _,
        iov_len: payload.len(),
    };

    let mut dest_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let sin = unsafe { &mut *(&mut dest_storage as *mut _ as *mut libc::sockaddr_in) };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = bound.port().to_be();
    sin.sin_addr.s_addr = u32::to_be(0x7F000001);

    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_name = &mut dest_storage as *mut _ as *mut libc::c_void;
    hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;

    let burst = 16usize;
    let mut ring = IoUring::new(256).expect("io_uring::new");

    c.bench_function("io_uring: UDP sendmsg+reap (burst 16)", |b| {
        b.iter(|| {
            for i in 0..burst {
                let sqe = opcode::SendMsg::new(io_uring::types::Fd(fd), &hdr)
                    .build()
                    .user_data(i as u64);
                unsafe {
                    ring.submission().push(&sqe).unwrap();
                }
            }
            ring.submit_and_wait(burst).unwrap();
            let mut count = 0u32;
            let cq = ring.completion();
            for cqe in cq {
                black_box(cqe.result());
                count += 1;
            }
            black_box(count);
        });
    });
}

// ──────────────────── UDP recvmsg ────────────────────
// These benchmarks establish the TRUE recv-path baseline.
// Compare against cqe_dispatch benchmarks to measure Aeron dispatch overhead.
// (Previous analysis incorrectly compared send baseline vs recv dispatch.)

/// Raw io_uring RecvMsg on UDP: CQ drain only (packet already received).
/// This is the absolute floor for recv completion visibility.
fn bench_uring_udp_recvmsg_reap_only(c: &mut Criterion) {
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let bound = sock.local_addr().unwrap();
    let fd = sock.as_raw_fd();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let payload = [0xAAu8; 32];

    let mut ring = IoUring::new(256).expect("io_uring::new");

    let mut buffer = [0u8; 65536];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr() as *mut _,
        iov_len: buffer.len(),
    };
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_name = &mut addr as *mut _ as *mut libc::c_void;
    hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;

    // Raw pointer to bypass borrow-checker for iter_batched closures.
    // SAFETY: single-threaded benchmark, closures never execute concurrently.
    let ring_ptr = &mut ring as *mut IoUring;
    let hdr_ptr = &mut hdr as *mut libc::msghdr;

    c.bench_function("io_uring: UDP recvmsg reap only", |b| {
        b.iter_batched(
            || {
                let ring = unsafe { &mut *ring_ptr };
                let hdr = unsafe { &mut *hdr_ptr };
                hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                hdr.msg_flags = 0;
                let sqe = opcode::RecvMsg::new(io_uring::types::Fd(fd), hdr as *mut libc::msghdr)
                    .build()
                    .user_data(0x1);
                unsafe {
                    ring.submission().push(&sqe).unwrap();
                }
                ring.submit().unwrap();
                sender.send_to(&payload, bound).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(50));
            },
            |()| {
                // Measure: just reading CQE from the memory-mapped CQ ring.
                let ring = unsafe { &mut *ring_ptr };
                let cq = ring.completion();
                for cqe in cq {
                    black_box(cqe.result());
                }
            },
            BatchSize::SmallInput,
        );
    });
}

/// Raw io_uring RecvMsg: CQ drain + re-arm + submit.
/// This is the TRUE baseline for comparison with cqe_dispatch: single msg.
///   aeron_dispatch_overhead = cqe_dispatch_single − this_benchmark
fn bench_uring_udp_recvmsg_rearm_submit(c: &mut Criterion) {
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let bound = sock.local_addr().unwrap();
    let fd = sock.as_raw_fd();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let payload = [0xAAu8; 32];

    let mut ring = IoUring::new(256).expect("io_uring::new");

    let mut buffer = [0u8; 65536];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr() as *mut _,
        iov_len: buffer.len(),
    };
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_name = &mut addr as *mut _ as *mut libc::c_void;
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;

    let ring_ptr = &mut ring as *mut IoUring;
    let hdr_ptr = &mut hdr as *mut libc::msghdr;

    c.bench_function("io_uring: UDP recvmsg reap+rearm+submit", |b| {
        b.iter_batched(
            || {
                let ring = unsafe { &mut *ring_ptr };
                let hdr = unsafe { &mut *hdr_ptr };
                hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                hdr.msg_flags = 0;
                let sqe = opcode::RecvMsg::new(io_uring::types::Fd(fd), hdr as *mut libc::msghdr)
                    .build()
                    .user_data(0x1);
                unsafe {
                    ring.submission().push(&sqe).unwrap();
                }
                ring.submit().unwrap();
                sender.send_to(&payload, bound).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(50));
            },
            |()| {
                let ring = unsafe { &mut *ring_ptr };
                let hdr = unsafe { &mut *hdr_ptr };
                // 1. Reap CQE.
                let cq = ring.completion();
                for cqe in cq {
                    black_box(cqe.result());
                }
                // 2. Re-arm: reset volatile fields + push RecvMsg SQE.
                hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                hdr.msg_flags = 0;
                let sqe = opcode::RecvMsg::new(io_uring::types::Fd(fd), hdr as *mut libc::msghdr)
                    .build()
                    .user_data(0x1);
                unsafe {
                    ring.submission().push(&sqe).unwrap();
                }
                // 3. Submit the re-arm SQE.
                black_box(ring.submit().unwrap());
            },
            BatchSize::SmallInput,
        );
    });
}

// ──────────────────── Multishot RecvMsg with buf_ring ────────────────────

/// Multishot RecvMsgMulti with provided buffer ring: CQ drain + return_buf only.
/// No SQE re-arm or submit — the multishot stays active across completions.
/// This is the NEW baseline after the multishot migration.
///   improvement = bench_recvmsg_rearm_submit − this_benchmark
fn bench_uring_udp_recvmsg_multishot(c: &mut Criterion) {
    use io_uring::cqueue;
    use io_uring::types::{BufRingEntry, RecvMsgOut};
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicU16, Ordering};

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let bound = sock.local_addr().unwrap();
    let fd = sock.as_raw_fd();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let payload = [0xAAu8; 32];

    let mut ring = IoUring::new(256).expect("io_uring::new");

    // ── Set up provided buffer ring (16 entries × 66 KiB each) ──
    const BUF_COUNT: u16 = 16;
    const BUF_SIZE: u32 = 65536 + 256; // payload + RecvMsgOut overhead
    const BGID: u16 = 0;

    let entry_size = std::mem::size_of::<BufRingEntry>();
    let ring_size = entry_size * BUF_COUNT as usize;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let ring_layout = std::alloc::Layout::from_size_align(ring_size, page_size).unwrap();
    let ring_ptr = unsafe { std::alloc::alloc_zeroed(ring_layout) };
    assert!(!ring_ptr.is_null());

    let bufs_total = BUF_SIZE as usize * BUF_COUNT as usize;
    let bufs_layout = std::alloc::Layout::from_size_align(bufs_total, 64).unwrap();
    let bufs_ptr = unsafe { std::alloc::alloc_zeroed(bufs_layout) };
    assert!(!bufs_ptr.is_null());

    for i in 0..BUF_COUNT as usize {
        let entry = unsafe { &mut *(ring_ptr.add(i * entry_size) as *mut BufRingEntry) };
        let buf_addr = unsafe { bufs_ptr.add(i * BUF_SIZE as usize) };
        entry.set_addr(buf_addr as u64);
        entry.set_len(BUF_SIZE);
        entry.set_bid(i as u16);
    }

    let tail_ptr = unsafe { BufRingEntry::tail(ring_ptr as *const BufRingEntry) as *mut u16 };
    unsafe {
        AtomicU16::from_ptr(tail_ptr).store(BUF_COUNT, Ordering::Release);
    }

    unsafe {
        ring.submitter()
            .register_buf_ring_with_flags(ring_ptr as u64, BUF_COUNT, BGID, 0)
            .expect("register_buf_ring");
    }

    // ── Template msghdr for multishot ──
    let mut msghdr_template: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr_template.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    // ── Submit the multishot RecvMsgMulti SQE ──
    let msghdr_ptr = &msghdr_template as *const libc::msghdr;
    let sqe = opcode::RecvMsgMulti::new(io_uring::types::Fd(fd), msghdr_ptr, BGID)
        .build()
        .user_data(0x1);
    unsafe {
        ring.submission().push(&sqe).unwrap();
    }
    ring.submit().unwrap();

    let mut local_tail = BUF_COUNT;
    let mask = BUF_COUNT - 1;

    // Raw pointers for iter_batched closures.
    let ring_ptr_atomic = ring_ptr;
    let ring_io = &mut ring as *mut IoUring;
    let local_tail_ptr = &mut local_tail as *mut u16;

    c.bench_function("io_uring: UDP recvmsg multishot reap+recycle", |b| {
        b.iter_batched(
            || {
                sender.send_to(&payload, bound).unwrap();
                std::thread::sleep(std::time::Duration::from_micros(50));
            },
            |()| {
                let ring = unsafe { &mut *ring_io };
                // 1. Reap CQE.
                let cq = ring.completion();
                for cqe in cq {
                    let flags = cqe.flags();
                    let result = cqe.result();
                    black_box(result);

                    // 2. Return buffer to the ring (the only per-message work).
                    if let Some(buf_id) = cqueue::buffer_select(flags) {
                        // Parse RecvMsgOut to access payload (simulates real use).
                        if result > 0 {
                            let offset = buf_id as usize * BUF_SIZE as usize;
                            let buf = unsafe {
                                std::slice::from_raw_parts(bufs_ptr.add(offset), result as usize)
                            };
                            if let Ok(out) = RecvMsgOut::parse(buf, &msghdr_template) {
                                black_box(out.payload_data().len());
                            }
                        }

                        let lt = unsafe { &mut *local_tail_ptr };
                        let idx = (*lt & mask) as usize;
                        let entry = unsafe {
                            &mut *(ring_ptr_atomic.add(idx * entry_size) as *mut BufRingEntry)
                        };
                        let buf_addr = unsafe { bufs_ptr.add(buf_id as usize * BUF_SIZE as usize) };
                        entry.set_addr(buf_addr as u64);
                        entry.set_len(BUF_SIZE);
                        entry.set_bid(buf_id);
                        *lt = lt.wrapping_add(1);
                        unsafe {
                            AtomicU16::from_ptr(tail_ptr).store(*lt, Ordering::Release);
                        }
                    }

                    // 3. Check if multishot still active (no re-arm needed).
                    black_box(cqueue::more(flags));
                }
            },
            BatchSize::SmallInput,
        );
    });

    // Cleanup.
    ring.submitter().unregister_buf_ring(BGID).ok();
    unsafe {
        std::alloc::dealloc(bufs_ptr, bufs_layout);
        std::alloc::dealloc(ring_ptr, ring_layout);
    }
}

criterion_group!(
    benches,
    bench_uring_nop_single,
    bench_uring_nop_burst_16,
    bench_uring_sqe_push_only,
    bench_uring_submit_empty,
    bench_uring_udp_sendmsg,
    bench_uring_udp_sendmsg_burst_16,
    bench_uring_udp_recvmsg_reap_only,
    bench_uring_udp_recvmsg_rearm_submit,
    bench_uring_udp_recvmsg_multishot,
);
criterion_main!(benches);
