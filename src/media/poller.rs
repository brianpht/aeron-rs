use std::io;
use std::fmt;

use libc;

use super::transport::UdpChannelTransport;

// ──────────────────────────── Hot-path error type ────────────────────────────

/// Stack-only error for hot-path poller operations.
///
/// Unlike `std::io::Error`, this **never heap-allocates** — safe for use in
/// the agent duty cycle. `Copy` + `Clone` so it can be returned by value
/// through the entire send/recv pipeline without boxing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollError {
    /// io_uring submission queue is full.
    RingFull,
    /// No send slot available in the pool.
    NoSendSlot,
    /// Transport not registered with the poller.
    NotRegistered,
    /// Kernel error code (raw errno).
    Os(i32),
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PollError::RingFull => f.write_str("io_uring SQ full"),
            PollError::NoSendSlot => f.write_str("no send slot available"),
            PollError::NotRegistered => f.write_str("transport not registered"),
            PollError::Os(errno) => write!(f, "os error {errno}"),
        }
    }
}

impl From<io::Error> for PollError {
    #[inline]
    fn from(e: io::Error) -> Self {
        PollError::Os(e.raw_os_error().unwrap_or(libc::EIO))
    }
}

impl From<PollError> for io::Error {
    fn from(e: PollError) -> Self {
        io::Error::from_raw_os_error(match e {
            PollError::RingFull => libc::ENOSPC,
            PollError::NoSendSlot => libc::ENOBUFS,
            PollError::NotRegistered => libc::ENOTCONN,
            PollError::Os(errno) => errno,
        })
    }
}

// ──────────────────────────── RecvMessage / PollResult ────────────────────────────

/// Information about a received packet.
pub struct RecvMessage<'a> {
    pub transport_idx: usize,
    pub dispatch_clientd: usize,
    pub destination_clientd: usize,
    pub data: &'a [u8],
    pub source_addr: &'a libc::sockaddr_storage,
    pub source_addr_len: libc::socklen_t,
}

/// Result of a single poll cycle.
#[derive(Default, Debug, Clone)]
pub struct PollResult {
    pub bytes_received: i64,
    pub bytes_sent: i64,
    pub messages_received: u32,
    pub send_completions: u32,
    pub recv_errors: u32,
    pub send_errors: u32,
}

// ──────────────────────────── TransportPoller trait ────────────────────────────

/// Abstract transport poller interface.
/// Concrete implementations: `UringTransportPoller` (io_uring), `EpollTransportPoller` (fallback).
///
/// Hot-path methods (`poll_recv`, `submit_send`, `flush`) return `Result<_, PollError>`
/// which is stack-only and never heap-allocates. Cold-path methods (`add_transport`,
/// `remove_transport`) use `io::Result`.
pub trait TransportPoller {
    /// Register a transport for recv polling. Returns the transport index.
    fn add_transport(&mut self, transport: &mut UdpChannelTransport) -> io::Result<usize>;

    /// Unregister a transport, cancelling all in-flight operations.
    fn remove_transport(&mut self, transport_idx: usize) -> io::Result<()>;

    /// Poll completions and invoke callback for each received message.
    /// Non-blocking. Zero-allocation.
    fn poll_recv<F>(&mut self, callback: F) -> Result<PollResult, PollError>
    where
        F: FnMut(RecvMessage<'_>);

    /// Submit a send operation. Data must remain valid until next `poll_recv`.
    ///
    /// For io_uring: the kernel copies UDP data during SQE processing,
    /// so `data` only needs to be valid at call time.
    fn submit_send(
        &mut self,
        transport_idx: usize,
        data: &[u8],
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError>;

    /// Submit the SQE ring and flush pending operations.
    fn flush(&mut self) -> Result<(), PollError>;
}