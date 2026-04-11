use std::mem;

use libc;

/// Maximum UDP payload we will ever receive.
pub const MAX_RECV_BUFFER: usize = 65536;

/// Cache-line aligned receive slot.
///
/// # Safety invariant
/// Once placed into a `Vec` that is never resized, the addresses of all
/// fields are stable. The io_uring kernel holds raw pointers into `hdr`,
/// `iov`, `addr`, and `buffer` between SQE submission and CQE completion.
/// We MUST NOT reallocate the backing `Vec` while any slot is in-flight.
#[repr(C, align(64))]
pub struct RecvSlot {
    pub hdr: libc::msghdr,
    pub iov: libc::iovec,
    pub addr: libc::sockaddr_storage,
    pub control: [u8; 128],
    pub buffer: [u8; MAX_RECV_BUFFER],
    pub transport_idx: u16,
    pub state: SlotState,
}

/// Send slot - smaller, no large buffer (points to external data).
#[repr(C, align(64))]
pub struct SendSlot {
    pub hdr: libc::msghdr,
    pub iov: libc::iovec,
    pub addr: libc::sockaddr_storage,
    pub state: SlotState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    Free,
    InFlight,
}

impl Default for RecvSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl RecvSlot {
    pub fn new() -> Self {
        Self {
            hdr: unsafe { mem::zeroed() },
            iov: unsafe { mem::zeroed() },
            addr: unsafe { mem::zeroed() },
            control: [0u8; 128],
            buffer: [0u8; MAX_RECV_BUFFER],
            transport_idx: 0,
            state: SlotState::Free,
        }
    }

    /// Set up internal pointers that remain stable for the lifetime of the slot.
    ///
    /// Must be called **once** after the slot is placed in its final Vec position
    /// (the Vec must never be resized after this). The kernel holds these raw
    /// pointers between SQE submit and CQE reap.
    ///
    /// # Safety
    /// The caller must ensure `self` will not move after this call (i.e. the
    /// backing Vec is fully constructed and will never grow/reallocate).
    pub unsafe fn init_stable_pointers(&mut self) {
        self.iov.iov_base = self.buffer.as_mut_ptr() as *mut libc::c_void;
        self.iov.iov_len = MAX_RECV_BUFFER;

        self.hdr.msg_name = &mut self.addr as *mut _ as *mut libc::c_void;
        self.hdr.msg_iov = &mut self.iov;
        self.hdr.msg_iovlen = 1;
        self.hdr.msg_control = self.control.as_mut_ptr() as *mut libc::c_void;
        self.hdr.msg_controllen = self.control.len();
    }

    /// Prepare for a recvmsg re-arm. Only resets the volatile fields that the
    /// kernel modifies during completion (`msg_namelen`, `msg_flags`).
    /// Stable pointers are set once by [`init_stable_pointers`].
    ///
    /// # Safety
    /// The caller must ensure `self` will not move until the corresponding
    /// CQE has been reaped, and that [`init_stable_pointers`] was called
    /// before the first use.
    pub unsafe fn prepare_recv(&mut self) {
        // Only reset fields the kernel overwrites on each recvmsg completion.
        self.hdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        self.hdr.msg_flags = 0;

        self.state = SlotState::InFlight;
    }

    /// Return the received data slice clamped to buffer capacity.
    /// Prevents panic if a corrupt CQE returns a byte count > MAX_RECV_BUFFER.
    #[inline]
    pub fn received_data(&self, bytes: usize) -> &[u8] {
        &self.buffer[..bytes.min(MAX_RECV_BUFFER)]
    }
}

impl Default for SendSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl SendSlot {
    pub fn new() -> Self {
        Self {
            hdr: unsafe { mem::zeroed() },
            iov: unsafe { mem::zeroed() },
            addr: unsafe { mem::zeroed() },
            state: SlotState::Free,
        }
    }

    /// Prepare for a sendmsg operation pointing to external data.
    ///
    /// # Safety
    /// `data_ptr` must remain valid until the CQE is reaped.
    /// `self` must not move until then either.
    pub unsafe fn prepare_send(
        &mut self,
        data_ptr: *const u8,
        data_len: usize,
        dest: Option<&libc::sockaddr_storage>,
    ) {
        unsafe {
            self.iov.iov_base = data_ptr as *mut libc::c_void;
            self.iov.iov_len = data_len;

            if let Some(dest_addr) = dest {
                std::ptr::copy_nonoverlapping(
                    dest_addr as *const libc::sockaddr_storage,
                    &mut self.addr,
                    1,
                );
                self.hdr.msg_name = &mut self.addr as *mut _ as *mut libc::c_void;
                self.hdr.msg_namelen = addr_storage_len(dest_addr);
            } else {
                self.hdr.msg_name = std::ptr::null_mut();
                self.hdr.msg_namelen = 0;
            }

            self.hdr.msg_iov = &mut self.iov;
            self.hdr.msg_iovlen = 1;
            self.hdr.msg_control = std::ptr::null_mut();
            self.hdr.msg_controllen = 0;
            self.hdr.msg_flags = 0;
        }

        self.state = SlotState::InFlight;
    }
}

/// Allocator for recv / send slots. Pre-allocates everything up front.
pub struct SlotPool {
    pub recv_slots: Vec<RecvSlot>,
    pub send_slots: Vec<SendSlot>,
    recv_free: Vec<u16>,
    send_free: Vec<u16>,
}

impl SlotPool {
    pub fn new(max_recv: usize, max_send: usize) -> Self {
        let mut recv_slots = Vec::with_capacity(max_recv);
        let mut send_slots = Vec::with_capacity(max_send);
        let mut recv_free = Vec::with_capacity(max_recv);
        let mut send_free = Vec::with_capacity(max_send);

        for i in 0..max_recv {
            recv_slots.push(RecvSlot::new());
            recv_free.push(i as u16);
        }
        for i in 0..max_send {
            send_slots.push(SendSlot::new());
            send_free.push(i as u16);
        }

        // SAFETY: all recv_slots are now in their final Vec positions.
        // The Vec will never be resized after this point (enforced by the
        // SlotPool invariant). Set up stable pointers for the kernel.
        for slot in recv_slots.iter_mut() {
            unsafe {
                slot.init_stable_pointers();
            }
        }

        Self {
            recv_slots,
            send_slots,
            recv_free,
            send_free,
        }
    }

    #[inline]
    pub fn alloc_recv(&mut self) -> Option<u16> {
        self.recv_free.pop()
    }

    pub fn free_recv(&mut self, idx: u16) {
        debug_assert_eq!(
            self.recv_slots[idx as usize].state,
            SlotState::Free,
            "free_recv: slot {} not in Free state (double-free?)",
            idx
        );
        debug_assert!(
            self.recv_free.len() < self.recv_free.capacity(),
            "free_recv: free list would exceed pre-allocated capacity"
        );
        self.recv_slots[idx as usize].state = SlotState::Free;
        self.recv_free.push(idx);
    }

    #[inline]
    pub fn alloc_send(&mut self) -> Option<u16> {
        self.send_free.pop()
    }

    pub fn free_send(&mut self, idx: u16) {
        debug_assert_eq!(
            self.send_slots[idx as usize].state,
            SlotState::Free,
            "free_send: slot {} not in Free state (double-free?)",
            idx
        );
        debug_assert!(
            self.send_free.len() < self.send_free.capacity(),
            "free_send: free list would exceed pre-allocated capacity"
        );
        self.send_slots[idx as usize].state = SlotState::Free;
        self.send_free.push(idx);
    }

    #[inline]
    pub fn recv_available(&self) -> usize {
        self.recv_free.len()
    }

    #[inline]
    pub fn send_available(&self) -> usize {
        self.send_free.len()
    }
}

#[inline]
pub fn addr_storage_len(addr: &libc::sockaddr_storage) -> libc::socklen_t {
    match addr.ss_family as i32 {
        libc::AF_INET => mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        libc::AF_INET6 => mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_pool_new_allocates_exact_counts() {
        let pool = SlotPool::new(8, 4);
        assert_eq!(pool.recv_slots.len(), 8);
        assert_eq!(pool.send_slots.len(), 4);
        assert_eq!(pool.recv_available(), 8);
        assert_eq!(pool.send_available(), 4);
    }

    #[test]
    fn alloc_recv_returns_unique_indices() {
        let mut pool = SlotPool::new(4, 0);
        let mut seen = [false; 4];
        for _ in 0..4 {
            let idx = pool.alloc_recv().expect("should have slots");
            assert!(!seen[idx as usize], "duplicate index {idx}");
            seen[idx as usize] = true;
        }
    }

    #[test]
    fn alloc_recv_exhaustion_returns_none() {
        let mut pool = SlotPool::new(2, 0);
        assert!(pool.alloc_recv().is_some());
        assert!(pool.alloc_recv().is_some());
        assert!(pool.alloc_recv().is_none());
    }

    #[test]
    fn free_recv_makes_slot_available_again() {
        let mut pool = SlotPool::new(1, 0);
        let idx = pool.alloc_recv().unwrap();
        assert!(pool.alloc_recv().is_none());
        // Slot must be Free to call free_recv.
        pool.recv_slots[idx as usize].state = SlotState::Free;
        pool.free_recv(idx);
        assert_eq!(pool.recv_available(), 1);
        let idx2 = pool.alloc_recv().unwrap();
        assert_eq!(idx, idx2);
    }

    #[test]
    fn alloc_send_returns_unique_indices() {
        let mut pool = SlotPool::new(0, 4);
        let mut seen = [false; 4];
        for _ in 0..4 {
            let idx = pool.alloc_send().expect("should have slots");
            assert!(!seen[idx as usize], "duplicate index {idx}");
            seen[idx as usize] = true;
        }
    }

    #[test]
    fn alloc_send_exhaustion_returns_none() {
        let mut pool = SlotPool::new(0, 2);
        assert!(pool.alloc_send().is_some());
        assert!(pool.alloc_send().is_some());
        assert!(pool.alloc_send().is_none());
    }

    #[test]
    fn free_send_makes_slot_available_again() {
        let mut pool = SlotPool::new(0, 1);
        let idx = pool.alloc_send().unwrap();
        assert!(pool.alloc_send().is_none());
        pool.send_slots[idx as usize].state = SlotState::Free;
        pool.free_send(idx);
        assert_eq!(pool.send_available(), 1);
    }

    #[test]
    fn recv_slot_received_data_clamps_to_buffer() {
        let slot = RecvSlot::new();
        // Request more than MAX_RECV_BUFFER → clamped.
        let data = slot.received_data(MAX_RECV_BUFFER + 100);
        assert_eq!(data.len(), MAX_RECV_BUFFER);
        // Normal case.
        let data = slot.received_data(42);
        assert_eq!(data.len(), 42);
    }

    #[test]
    fn recv_slot_default_state_is_free() {
        let slot = RecvSlot::new();
        assert_eq!(slot.state, SlotState::Free);
    }

    #[test]
    fn send_slot_default_state_is_free() {
        let slot = SendSlot::new();
        assert_eq!(slot.state, SlotState::Free);
    }

    #[test]
    fn recv_slot_prepare_recv_sets_in_flight() {
        let mut slot = RecvSlot::new();
        unsafe {
            slot.prepare_recv();
        }
        assert_eq!(slot.state, SlotState::InFlight);
    }

    #[test]
    fn addr_storage_len_ipv4() {
        let mut addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
        addr.ss_family = libc::AF_INET as libc::sa_family_t;
        assert_eq!(
            addr_storage_len(&addr) as usize,
            mem::size_of::<libc::sockaddr_in>()
        );
    }

    #[test]
    fn addr_storage_len_ipv6() {
        let mut addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
        addr.ss_family = libc::AF_INET6 as libc::sa_family_t;
        assert_eq!(
            addr_storage_len(&addr) as usize,
            mem::size_of::<libc::sockaddr_in6>()
        );
    }

    #[test]
    fn addr_storage_len_unknown_is_zero() {
        let mut addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
        addr.ss_family = 255;
        assert_eq!(addr_storage_len(&addr), 0);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "free list would exceed")]
    fn free_recv_overflow_panics_in_debug() {
        // SlotPool with 1 recv slot. After new(), free list = [0], capacity = 1.
        let mut pool = SlotPool::new(1, 0);
        // Don't alloc - free list is already full (len=1, cap=1).
        // Manually set slot 0 to Free (it already is) and call free_recv.
        // This pushes beyond capacity → should panic on the capacity debug_assert.
        pool.free_recv(0);
    }
}
