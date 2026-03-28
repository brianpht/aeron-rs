//! Diagnostic: dump pointed-to data.
use std::net::SocketAddr;
use aeron_rs::context::DriverContext;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::poller::TransportPoller;
use aeron_rs::media::transport::UdpChannelTransport;
use aeron_rs::media::uring_poller::UringTransportPoller;
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
fn hex(ptr: *const u8, len: usize) -> String {
    let mut s = String::new();
    for i in 0..len {
        s += &format!("{:02X} ", unsafe { *ptr.add(i) });
    }
    s
}
fn main() {
    let ctx = DriverContext { uring_ring_size: 64, uring_send_slots: 4, uring_buf_ring_entries: 4, ..DriverContext::default() };
    let mut poller = UringTransportPoller::new(&ctx).expect("poller");
    let ch = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
    let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut t = UdpChannelTransport::open(&ch, &local, &ch.remote_data, &ctx).unwrap();
    let send_fd = t.send_fd;
    let tidx = poller.add_transport(&mut t).unwrap();
    let dest_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
    let dest = addr_to_storage(dest_addr);
    let frame = [0xABu8; 32];
    poller.submit_send(tidx, &frame, Some(&dest)).unwrap();
    let slot = &poller.pool.send_slots[3];
    let hdr = &slot.hdr;
    // Dump what msg_iov points to (iovec)
    eprintln!("slot.iov @ {:?}: {}", hdr.msg_iov, hex(hdr.msg_iov as *const u8, 16));
    // Dump what iov_base points to (first 16 bytes of data)
    let iov = unsafe { &*hdr.msg_iov };
    eprintln!("iov_base @ {:?}: {}", iov.iov_base, hex(iov.iov_base as *const u8, 16));
    // Dump what msg_name points to (first 16 bytes = sockaddr_in)
    eprintln!("msg_name @ {:?}: {}", hdr.msg_name, hex(hdr.msg_name as *const u8, 16));
    // Now build WORKING stack version
    let mut stack_dest: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    stack_dest.sin_family = libc::AF_INET as libc::sa_family_t;
    stack_dest.sin_port = 12345u16.to_be();
    stack_dest.sin_addr.s_addr = u32::to_be(0x7F000001);
    eprintln!("stack sin @ {:?}: {}", &stack_dest as *const _, hex(&stack_dest as *const _ as *const u8, 16));
    // Try sendmsg with slot hdr
    eprintln!("\nsendmsg(slot): ret={}", unsafe { libc::sendmsg(send_fd, hdr as *const _, 0) });
    eprintln!("  errno={}", unsafe { *libc::__errno_location() });
    // Fix: point msg_name to the STACK sockaddr_in
    let mut fixed_hdr = unsafe { std::ptr::read(hdr as *const libc::msghdr) };
    fixed_hdr.msg_name = &mut stack_dest as *mut _ as *mut _;
    fixed_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
    eprintln!("\nsendmsg(fixed name): ret={}", unsafe { libc::sendmsg(send_fd, &fixed_hdr, 0) });
    eprintln!("  errno={}", unsafe { *libc::__errno_location() });
    // Or: fix iov
    let mut fixed_hdr2 = unsafe { std::ptr::read(hdr as *const libc::msghdr) };
    let mut stack_iov = libc::iovec { iov_base: frame.as_ptr() as *mut _, iov_len: frame.len() };
    fixed_hdr2.msg_iov = &mut stack_iov;
    eprintln!("\nsendmsg(fixed iov): ret={}", unsafe { libc::sendmsg(send_fd, &fixed_hdr2, 0) });
    eprintln!("  errno={}", unsafe { *libc::__errno_location() });
}
