// io_uring-based transport poller. Single-threaded, no async/await.
// Designed for Aeron's synchronous agent duty-cycle model.
//
// Uses multishot RecvMsgMulti with io_uring provided buffer ring (buf_ring)
// for zero-copy, re-arm-free receive. One RecvMsgMulti SQE per transport
// stays active across completions; the kernel picks buffers from the shared
// ring. Eliminates the SQE re-arm + submit overhead per received packet.

use io_uring::types::{BufRingEntry, RecvMsgOut};
use io_uring::{IoUring, cqueue, opcode, types};
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU16, Ordering};

use libc;

use super::buffer_pool::{MAX_RECV_BUFFER, SlotPool, SlotState};
use super::poller::{PollError, PollResult, RecvMessage, TransportPoller};
use super::transport::UdpChannelTransport;
use crate::context::DriverContext;

// ──────────────────────────── Constants ────────────────────────────

/// Buffer group ID for the provided buffer ring.
const BUF_GROUP_ID: u16 = 0;

/// Overhead per buffer for multishot RecvMsg output format:
/// `io_uring_recvmsg_out` header (16 bytes) + `sockaddr_storage` name field.
const RECV_MSG_OVERHEAD: usize = 16 + mem::size_of::<libc::sockaddr_storage>();

/// Total size of each provided buffer: max payload + RecvMsgOut overhead.
const BUF_ENTRY_SIZE: usize = MAX_RECV_BUFFER + RECV_MSG_OVERHEAD;

// ──────────────────────────── BufRingPool ────────────────────────────

/// Pre-allocated io_uring provided buffer ring for multishot RecvMsg.
///
/// Allocated once at init and registered with the kernel. The kernel picks
/// buffers from this ring for `RecvMsgMulti` operations. After CQE processing,
/// buffers are returned via [`return_buf`](Self::return_buf).
///
/// # Safety invariant
/// - Ring entries and buffers are allocated at init and never moved/resized.
/// - The ring is registered with the kernel and remains valid until the
///   io_uring fd is closed (struct field drop order in `UringTransportPoller`
///   ensures the ring is dropped first).
/// - While a buffer is in-flight, we must not access or recycle it.
struct BufRingPool {
    /// Page-aligned ring entry memory (kernel-shared).
    ring_ptr: *mut u8,
    ring_layout: Layout,
    /// Contiguous pre-allocated buffer memory.
    bufs_ptr: *mut u8,
    bufs_layout: Layout,
    /// Number of ring entries (power-of-two).
    #[allow(dead_code)]
    entries: u16,
    /// Size of each individual buffer in bytes.
    buf_size: u32,
    /// Mask for tail wrapping (entries - 1). Bitmask, not modulo.
    mask: u16,
    /// Local tail counter (written atomically to the shared ring tail).
    local_tail: u16,
}

impl BufRingPool {
    /// Create and register a provided buffer ring.
    ///
    /// `entries` must be power-of-two <= 32768.
    /// Allocates `entries` buffers of `buf_size` bytes each.
    fn new(submitter: &io_uring::Submitter<'_>, entries: u16, buf_size: u32) -> io::Result<Self> {
        debug_assert!(
            entries.is_power_of_two(),
            "buf_ring entries must be power-of-two"
        );
        let mask = entries - 1;

        // ── Allocate page-aligned ring entry array ──
        let entry_size = mem::size_of::<BufRingEntry>();
        let ring_size = entry_size * entries as usize;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let ring_layout = Layout::from_size_align(ring_size, page_size)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let ring_ptr = unsafe { alloc_zeroed(ring_layout) };
        if ring_ptr.is_null() {
            return Err(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        // ── Allocate contiguous buffer memory (cache-line aligned) ──
        let bufs_total = buf_size as usize * entries as usize;
        let bufs_layout = Layout::from_size_align(bufs_total, 64).map_err(|_| {
            unsafe { dealloc(ring_ptr, ring_layout) };
            io::Error::from_raw_os_error(libc::EINVAL)
        })?;
        let bufs_ptr = unsafe { alloc_zeroed(bufs_layout) };
        if bufs_ptr.is_null() {
            unsafe { dealloc(ring_ptr, ring_layout) };
            return Err(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        // ── Initialize each ring entry → points to its buffer ──
        for i in 0..entries as usize {
            let entry = unsafe { &mut *(ring_ptr.add(i * entry_size) as *mut BufRingEntry) };
            let buf_addr = unsafe { bufs_ptr.add(i * buf_size as usize) };
            entry.set_addr(buf_addr as u64);
            entry.set_len(buf_size);
            entry.set_bid(i as u16);
        }

        // ── Set initial tail = entries (all buffers available) ──
        let tail_ptr = unsafe { BufRingEntry::tail(ring_ptr as *const BufRingEntry) as *mut u16 };
        unsafe {
            AtomicU16::from_ptr(tail_ptr).store(entries, Ordering::Release);
        }

        // ── Register with the kernel ──
        unsafe {
            submitter
                .register_buf_ring_with_flags(ring_ptr as u64, entries, BUF_GROUP_ID, 0)
                .inspect_err(|_| {
                    dealloc(bufs_ptr, bufs_layout);
                    dealloc(ring_ptr, ring_layout);
                })?;
        }

        Ok(Self {
            ring_ptr,
            ring_layout,
            bufs_ptr,
            bufs_layout,
            entries,
            buf_size,
            mask,
            local_tail: entries,
        })
    }

    /// Get the buffer data for a given buffer ID.
    ///
    /// `bid` is the buffer ID from `cqueue::buffer_select(flags)`.
    /// `len` is clamped to `buf_size`. Returns empty slice if `bid` is
    /// out of range (corrupt CQE safety).
    #[inline]
    fn buf_data(&self, bid: u16, len: usize) -> &[u8] {
        if bid > self.mask {
            return &[];
        }
        let offset = bid as usize * self.buf_size as usize;
        let clamped = len.min(self.buf_size as usize);
        unsafe { std::slice::from_raw_parts(self.bufs_ptr.add(offset), clamped) }
    }

    /// Return a buffer to the ring locally (no atomic publish).
    ///
    /// Must be called exactly once per reaped CQE that consumed a buffer.
    /// After all buffers in a batch are returned, call [`publish_tail`] once
    /// to make them visible to the kernel. This reduces N atomic stores to 1
    /// per poll cycle.
    #[inline]
    fn return_buf_local(&mut self, bid: u16) {
        // Bounds check: corrupt CQE could yield bid >= entries.
        if bid > self.mask {
            return;
        }

        let entry_size = mem::size_of::<BufRingEntry>();
        let idx = (self.local_tail & self.mask) as usize;
        let entry = unsafe { &mut *(self.ring_ptr.add(idx * entry_size) as *mut BufRingEntry) };

        let buf_addr = unsafe { self.bufs_ptr.add(bid as usize * self.buf_size as usize) };
        entry.set_addr(buf_addr as u64);
        entry.set_len(self.buf_size);
        entry.set_bid(bid);

        self.local_tail = self.local_tail.wrapping_add(1);
    }

    /// Publish the current local_tail to the kernel-shared ring tail.
    ///
    /// Call once after a batch of [`return_buf_local`] calls. Single Release
    /// store ensures the kernel sees all entry data before the tail advance.
    #[inline]
    fn publish_tail(&self) {
        let tail_ptr =
            unsafe { BufRingEntry::tail(self.ring_ptr as *const BufRingEntry) as *mut u16 };
        unsafe {
            AtomicU16::from_ptr(tail_ptr).store(self.local_tail, Ordering::Release);
        }
    }
}

impl Drop for BufRingPool {
    fn drop(&mut self) {
        // SAFETY: The io_uring ring fd is closed first (struct field drop order),
        // which cancels all in-flight operations and releases kernel references.
        unsafe {
            dealloc(self.bufs_ptr, self.bufs_layout);
            dealloc(self.ring_ptr, self.ring_layout);
        }
    }
}

// ──────────────────────────── User data encoding ────────────────────────────

/// Packed into the 64-bit CQE user_data field.
///
/// Layout:
///   [63..48] transport_idx  (16 bits - max 65535 transports)
///   [47..40] op_kind        (8 bits)
///   [39..24] slot_idx       (16 bits - send slots only; 0 for multishot recv)
///   [23..0]  reserved       (24 bits)
#[derive(Clone, Copy)]
struct UserData(u64);

impl UserData {
    const OP_RECV: u8 = 1;
    const OP_SEND: u8 = 2;

    #[inline]
    fn encode(transport_idx: u16, op: u8, slot_idx: u16) -> u64 {
        ((transport_idx as u64) << 48) | ((op as u64) << 40) | ((slot_idx as u64) << 24)
    }

    #[inline]
    fn transport_idx(self) -> u16 {
        (self.0 >> 48) as u16
    }

    #[inline]
    fn op(self) -> u8 {
        (self.0 >> 40) as u8
    }

    #[inline]
    fn slot_idx(self) -> u16 {
        (self.0 >> 24) as u16
    }
}

// ──────────────────────────── Transport entry ────────────────────────────

struct TransportEntry {
    recv_fd: RawFd,
    send_fd: RawFd,
    dispatch_clientd: usize,
    destination_clientd: usize,
    /// Whether a multishot RecvMsgMulti SQE is currently active.
    multishot_active: bool,
    active: bool,
}

// ──────────────────────────── UringTransportPoller ────────────────────────────

pub struct UringTransportPoller {
    /// io_uring ring instance. Dropped first to close fd before buf_ring memory.
    ring: IoUring,
    /// Provided buffer ring for multishot RecvMsg. Dropped after ring.
    buf_ring: BufRingPool,
    /// Send-only slot pool (recv is handled by buf_ring).
    pub pool: SlotPool,
    transports: Vec<TransportEntry>,
    /// Template msghdr for RecvMsgMulti. Only `msg_namelen` and
    /// `msg_controllen` are read by the kernel (once at SQE processing).
    recv_msghdr: libc::msghdr,
}

impl UringTransportPoller {
    pub fn new(ctx: &DriverContext) -> io::Result<Self> {
        ctx.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

        let mut builder = IoUring::builder();
        if ctx.uring_sqpoll {
            builder.setup_sqpoll(ctx.uring_sqpoll_idle_ms);
        }
        let ring = builder.build(ctx.uring_ring_size)?;

        // Create and register the provided buffer ring.
        let buf_ring = BufRingPool::new(
            &ring.submitter(),
            ctx.uring_buf_ring_entries,
            BUF_ENTRY_SIZE as u32,
        )?;

        // Send-only slot pool (recv handled by buf_ring multishot).
        let pool = SlotPool::new(0, ctx.uring_send_slots);

        // Template msghdr for multishot RecvMsg - kernel reads msg_namelen
        // and msg_controllen to know how much name/control to capture.
        let mut recv_msghdr: libc::msghdr = unsafe { mem::zeroed() };
        recv_msghdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // msg_controllen = 0: no ancillary data needed for Aeron.

        let max_transports = 64usize;

        Ok(Self {
            ring,
            buf_ring,
            pool,
            transports: Vec::with_capacity(max_transports),
            recv_msghdr,
        })
    }

    /// Submit a multishot RecvMsgMulti SQE for a transport.
    ///
    /// One SQE stays active and repeatedly completes via CQEs until
    /// cancelled or the kernel terminates it (e.g., no buffers available).
    /// The kernel picks buffers from the shared buf_ring; no per-slot
    /// allocation or SQE re-arm is needed.
    fn submit_multishot_recv(&mut self, transport_idx: u16) -> Result<(), PollError> {
        let entry = &mut self.transports[transport_idx as usize];
        if !entry.active {
            return Ok(());
        }
        let fd = types::Fd(entry.recv_fd);
        let msghdr_ptr = &self.recv_msghdr as *const libc::msghdr;
        let ud = UserData::encode(transport_idx, UserData::OP_RECV, 0);

        let sqe = opcode::RecvMsgMulti::new(fd, msghdr_ptr, BUF_GROUP_ID)
            .build()
            .user_data(ud);

        // SAFETY: msghdr is stable in self, kernel reads it once at SQE processing.
        unsafe {
            self.ring
                .submission()
                .push(&sqe)
                .map_err(|_| PollError::RingFull)?;
        }

        entry.multishot_active = true;
        Ok(())
    }

    /// Submit a SendMsg SQE.
    fn submit_send_sqe(
        &mut self,
        transport_idx: u16,
        slot_idx: u16,
        data: &[u8],
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let entry = &self.transports[transport_idx as usize];
        if !entry.active {
            return Ok(());
        }
        let fd = types::Fd(entry.send_fd);
        let slot = &mut self.pool.send_slots[slot_idx as usize];

        // SAFETY: data is valid for the duration of this call. For UDP sendmsg
        // via io_uring the kernel copies data into an skb during SQE processing,
        // so the data pointer does not need to outlive submission.
        unsafe {
            slot.prepare_send(data.as_ptr(), data.len(), dest);
        }

        let ud = UserData::encode(transport_idx, UserData::OP_SEND, slot_idx);

        let sqe = opcode::SendMsg::new(fd, &slot.hdr as *const libc::msghdr)
            .build()
            .user_data(ud);

        unsafe {
            self.ring
                .submission()
                .push(&sqe)
                .map_err(|_| PollError::RingFull)?;
        }

        Ok(())
    }

    /// Recover send slots leaked by CQ overflow.
    ///
    /// When the kernel drops CQEs due to CQ ring overflow, we never receive
    /// the send completion notifications and the slots stay InFlight forever.
    /// This cold-path scan finds and frees them. O(N) where N = total send
    /// slots, but only runs when overflow is detected.
    fn recover_leaked_send_slots(&mut self) {
        let mut recovered = 0u32;
        for i in 0..self.pool.send_slots.len() {
            if self.pool.send_slots[i].state == SlotState::InFlight {
                self.pool.send_slots[i].state = SlotState::Free;
                self.pool.free_send(i as u16);
                recovered += 1;
            }
        }
        if recovered > 0 {
            tracing::warn!(recovered, "recovered leaked send slots after CQ overflow");
        }
    }

    /// Number of free send slots available for submission.
    /// O(1), no allocation. Used to clamp sender_scan limit so frames are
    /// not silently dropped when send slots are exhausted.
    #[inline]
    pub fn send_available(&self) -> usize {
        self.pool.send_available()
    }
}

/// Maximum CQEs to harvest per poll cycle. CQEs beyond this stay in the
/// completion queue and will be reaped on the next poll_recv call.
/// 256 × 16 bytes (u64 + i32 + u32) = 4 KiB on the stack.
const CQE_BATCH_SIZE: usize = 256;

impl TransportPoller for UringTransportPoller {
    fn add_transport(&mut self, transport: &mut UdpChannelTransport) -> io::Result<usize> {
        let idx = self.transports.len();

        self.transports.push(TransportEntry {
            recv_fd: transport.recv_fd,
            send_fd: transport.send_fd,
            dispatch_clientd: transport.dispatch_clientd,
            destination_clientd: transport.destination_clientd,
            multishot_active: false,
            active: true,
        });

        // Submit a single multishot RecvMsgMulti SQE for this transport.
        // Unlike one-shot RecvMsg, this stays active across completions.
        self.submit_multishot_recv(idx as u16)?;
        self.ring.submit()?;

        transport.poller_index = Some(idx);
        Ok(idx)
    }

    fn remove_transport(&mut self, transport_idx: usize) -> io::Result<()> {
        if transport_idx >= self.transports.len() {
            return Ok(());
        }

        let entry = &mut self.transports[transport_idx];
        entry.active = false;
        entry.multishot_active = false;

        // Cancel the multishot RecvMsgMulti SQE via AsyncCancel.
        // The kernel will post a CQE with -ECANCELED for the multishot;
        // poll_recv handles it by ignoring inactive transports.
        let ud = UserData::encode(transport_idx as u16, UserData::OP_RECV, 0);
        let cancel = opcode::AsyncCancel::new(ud).build().user_data(0);

        unsafe {
            let _ = self.ring.submission().push(&cancel);
        }
        let _ = self.ring.submit();

        Ok(())
    }

    fn poll_recv<F>(&mut self, mut callback: F) -> Result<PollResult, PollError>
    where
        F: FnMut(RecvMessage<'_>),
    {
        let mut result = PollResult::default();

        // Harvest CQEs into a stack-allocated buffer to break the borrow
        // on self.ring before we need &mut self for buf_ring / re-submission.
        // Includes flags for buffer_select() and more() checks.
        let mut cqe_buf = [MaybeUninit::<(u64, i32, u32)>::uninit(); CQE_BATCH_SIZE];
        let mut cqe_count = 0usize;
        let mut bufs_returned = false;

        {
            // SAFETY: single-threaded agent - no concurrent access to ring.
            let cq = unsafe { self.ring.completion_shared() };

            // Detect CQ overflow - kernel dropped CQEs because the CQ ring
            // was full. This means we lost send completion notifications and
            // potentially multishot terminations.
            let overflow = cq.overflow();
            if overflow > 0 {
                result.cq_overflows = overflow;
                tracing::warn!(
                    overflow,
                    "io_uring CQ overflow - {} CQEs lost, recovering send slots",
                    overflow
                );
            }

            for cqe in cq {
                if cqe_count < CQE_BATCH_SIZE {
                    cqe_buf[cqe_count].write((cqe.user_data(), cqe.result(), cqe.flags()));
                    cqe_count += 1;
                } else {
                    break;
                }
            }
        }

        let mut needs_submit = false;

        for item in cqe_buf.iter().take(cqe_count) {
            // SAFETY: we wrote cqe_buf[0..cqe_count] in the drain loop above.
            let (ud_raw, ret, cqe_flags) = unsafe { item.assume_init() };
            let ud = UserData(ud_raw);
            let tidx = ud.transport_idx() as usize;
            let op = ud.op();

            match op {
                UserData::OP_RECV => {
                    let active = tidx < self.transports.len() && self.transports[tidx].active;
                    let has_more = cqueue::more(cqe_flags);

                    if let Some(buf_id) = cqueue::buffer_select(cqe_flags) {
                        if ret > 0 && active {
                            // Scope: immutable borrow of buf_ring for buffer
                            // access + callback. Released before return_buf_local.
                            let bytes = {
                                let buf = self.buf_ring.buf_data(buf_id, ret as usize);
                                match RecvMsgOut::parse(buf, &self.recv_msghdr) {
                                    Ok(recv_out) => {
                                        // Copy source addr to stack (avoids
                                        // pointer cast; addr lives in buf_ring
                                        // buffer which is recycled after this).
                                        let mut source_addr: libc::sockaddr_storage =
                                            unsafe { mem::zeroed() };
                                        let name = recv_out.name_data();
                                        let copy_len = name
                                            .len()
                                            .min(mem::size_of::<libc::sockaddr_storage>());
                                        if copy_len > 0 {
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(
                                                    name.as_ptr(),
                                                    &mut source_addr as *mut _ as *mut u8,
                                                    copy_len,
                                                );
                                            }
                                        }

                                        let te = &self.transports[tidx];
                                        let payload = recv_out.payload_data();
                                        let payload_len = payload.len();

                                        let msg = RecvMessage {
                                            transport_idx: tidx,
                                            dispatch_clientd: te.dispatch_clientd,
                                            destination_clientd: te.destination_clientd,
                                            data: payload,
                                            source_addr: &source_addr,
                                            source_addr_len: recv_out.incoming_name_len()
                                                as libc::socklen_t,
                                        };
                                        callback(msg);
                                        Some(payload_len as i64)
                                    }
                                    Err(()) => None,
                                }
                            };
                            // buf borrow released - scope ended.

                            if let Some(b) = bytes {
                                result.bytes_received += b;
                                result.messages_received += 1;
                            }
                        } else if ret < 0 {
                            let err = -ret;
                            if err != libc::EAGAIN && err != libc::EINTR && err != libc::ECANCELED {
                                result.recv_errors += 1;
                            }
                        }

                        // Return buffer locally (deferred atomic publish).
                        self.buf_ring.return_buf_local(buf_id);
                        bufs_returned = true;
                    } else if ret < 0 {
                        // Error CQE without a buffer (e.g., multishot
                        // terminated with -ENOBUFS or -ECANCELED).
                        let err = -ret;
                        if err != libc::EAGAIN && err != libc::EINTR && err != libc::ECANCELED {
                            result.recv_errors += 1;
                        }
                    }

                    // If multishot was terminated, re-submit.
                    if !has_more && active {
                        if tidx < self.transports.len() {
                            self.transports[tidx].multishot_active = false;
                        }
                        let _ = self.submit_multishot_recv(tidx as u16);
                        needs_submit = true;
                    }
                }

                UserData::OP_SEND => {
                    let sidx = ud.slot_idx();
                    if ret >= 0 {
                        result.bytes_sent += ret as i64;
                        result.send_completions += 1;
                    } else {
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "[uring] send error: ret={ret}, errno={}, tidx={tidx}, sidx={sidx}",
                            -ret
                        );
                        result.send_errors += 1;
                    }
                    self.pool.send_slots[sidx as usize].state = SlotState::Free;
                    self.pool.free_send(sidx);
                }

                _ => {}
            }
        }

        // Single atomic publish for all buf_ring returns in this batch.
        // Reduces N Release stores to 1 per poll cycle.
        if bufs_returned {
            self.buf_ring.publish_tail();
        }

        // CQ overflow recovery: scan send slots for leaked in-flight entries.
        // The kernel dropped their completion CQEs so we never freed them.
        // This is a cold path - only runs when overflow is detected.
        if result.cq_overflows > 0 {
            self.recover_leaked_send_slots();
        }

        // Submit only if we pushed re-arm SQEs (multishot restart).
        // On normal cycles, the multishot stays active - no SQE push,
        // no io_uring_enter syscall.
        if needs_submit {
            self.ring.submit().map_err(PollError::from)?;
        }

        Ok(result)
    }

    fn submit_send(
        &mut self,
        transport_idx: usize,
        data: &[u8],
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let slot_idx = self.pool.alloc_send().ok_or(PollError::NoSendSlot)?;

        self.submit_send_sqe(transport_idx as u16, slot_idx, data, dest)
    }

    fn flush(&mut self) -> Result<(), PollError> {
        self.ring.submit().map_err(PollError::from)?;
        Ok(())
    }
}
