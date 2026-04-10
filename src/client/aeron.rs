// Aeron client - connects to a running media driver and provides
// add_publication / add_subscription / heartbeat operations.
//
// For v1, works in-process only (same process as the driver).
// Publications use ConcurrentPublication (Arc-shared log buffer) with
// a lock-free bridge to deliver SenderPublication to the sender agent.
// Subscriptions use SharedLogBuffer images (receiver writes, client reads
// via Acquire/Release) with a SubscriptionBridge for image handle transfer.
//
// Not Sync (single-threaded client). Send (movable to a client thread).
// No allocation in steady state (keepalive is pre-encoded).

use std::net::SocketAddr;
use std::sync::Arc;

use crate::cnc::cnc_file::ClientCnc;
use crate::cnc::command::*;
use crate::media::channel::UdpChannel;
use crate::media::concurrent_publication::new_concurrent;
use crate::media::send_channel_endpoint::SendChannelEndpoint;
use crate::media::transport::UdpChannelTransport;

use super::bridge::{PendingPublication, PublicationBridge};
use super::sub_bridge::SubscriptionBridge;
use super::publication::Publication;
use super::subscription::Subscription;
use super::ClientError;

/// Default driver heartbeat timeout for liveness checks (5 seconds).
const DEFAULT_DRIVER_TIMEOUT_MS: i64 = 5_000;

/// Aeron client handle. Connects to a running media driver and provides
/// the user-facing API for adding publications and subscriptions.
///
/// For v1, in-process only. Created via `MediaDriver::connect()`.
pub struct Aeron {
    cnc: ClientCnc,
    client_id: i64,
    next_correlation_id: i64,
    next_session_id: i32,
    pub_bridge: Arc<PublicationBridge>,
    sub_bridge: Arc<SubscriptionBridge>,
    /// Socket config for transport creation (cold path).
    socket_rcvbuf: usize,
    socket_sndbuf: usize,
    multicast_ttl: u8,
    /// Publication config.
    term_length: u32,
    mtu: u32,
    /// Pre-encoded keepalive scratch buffer.
    keepalive_buf: [u8; CLIENT_KEEPALIVE_LENGTH],
}

impl Aeron {
    /// Create an in-process Aeron client.
    ///
    /// # Safety
    ///
    /// `cnc_base` must point to a valid, initialized CnC region that
    /// remains valid for the lifetime of this client.
    pub(crate) unsafe fn connect_in_process(
        cnc_base: *mut u8,
        cnc_length: usize,
        pub_bridge: Arc<PublicationBridge>,
        sub_bridge: Arc<SubscriptionBridge>,
        socket_rcvbuf: usize,
        socket_sndbuf: usize,
        multicast_ttl: u8,
        term_length: u32,
        mtu: u32,
    ) -> Result<Self, ClientError> {
        let cnc = unsafe {
            ClientCnc::from_ptr(cnc_base, cnc_length)
        }.map_err(|_| ClientError::CncError)?;

        if !cnc.is_driver_alive(DEFAULT_DRIVER_TIMEOUT_MS) {
            return Err(ClientError::DriverNotRunning);
        }

        let client_id = cnc.driver_pid() as i64 ^ 0x1234_5678;

        let mut client = Self {
            cnc,
            client_id,
            next_correlation_id: 1,
            next_session_id: 1,
            pub_bridge,
            sub_bridge,
            socket_rcvbuf,
            socket_sndbuf,
            multicast_ttl,
            term_length,
            mtu,
            keepalive_buf: [0u8; CLIENT_KEEPALIVE_LENGTH],
        };

        // Send initial keepalive.
        client.heartbeat();

        Ok(client)
    }

    /// Allocate the next correlation ID.
    fn alloc_correlation_id(&mut self) -> i64 {
        let id = self.next_correlation_id;
        self.next_correlation_id = id.wrapping_add(1);
        id
    }

    /// Allocate the next session ID.
    fn alloc_session_id(&mut self) -> i32 {
        let id = self.next_session_id;
        self.next_session_id = id.wrapping_add(1);
        id
    }

    /// Add a publication on the given channel and stream.
    ///
    /// Creates a concurrent publication pair, opens a UDP transport,
    /// and deposits the sender-side handle in the bridge for the sender
    /// agent to pick up. Returns a `Publication` handle for offering data.
    ///
    /// Cold path - allocates the log buffer and opens a socket.
    pub fn add_publication(
        &mut self,
        channel: &str,
        stream_id: i32,
    ) -> Result<Publication, ClientError> {
        // Parse channel URI.
        let udp_channel = UdpChannel::parse(channel)
            .map_err(|_| ClientError::ChannelInvalid)?;

        let correlation_id = self.alloc_correlation_id();
        let session_id = self.alloc_session_id();

        // Create concurrent publication pair (Arc-shared log buffer).
        let (pub_handle, sender_pub) = new_concurrent(
            session_id,
            stream_id,
            0, // initial_term_id
            self.term_length,
            self.mtu,
        ).ok_or(ClientError::InvalidParams)?;

        // Build a minimal DriverContext for transport creation.
        let transport_ctx = crate::context::DriverContext {
            socket_rcvbuf: self.socket_rcvbuf,
            socket_sndbuf: self.socket_sndbuf,
            multicast_ttl: self.multicast_ttl,
            mtu_length: self.mtu as usize,
            term_buffer_length: self.term_length,
            ..crate::context::DriverContext::default()
        };

        // Open UDP transport.
        let local_addr: SocketAddr = match udp_channel.remote_data {
            SocketAddr::V4(_) => "0.0.0.0:0".parse().map_err(|_| ClientError::ChannelInvalid)?,
            SocketAddr::V6(_) => "[::]:0".parse().map_err(|_| ClientError::ChannelInvalid)?,
        };
        let transport = UdpChannelTransport::open(
            &udp_channel,
            &local_addr,
            &udp_channel.remote_data,
            &transport_ctx,
        ).map_err(|_| ClientError::TransportError)?;

        // Build endpoint.
        let endpoint = SendChannelEndpoint::new(udp_channel.clone(), transport);

        // Convert destination address to sockaddr_storage.
        let dest_addr = sockaddr_to_storage(udp_channel.remote_data);

        // Deposit in bridge for the sender agent.
        let pending = PendingPublication {
            sender_pub,
            endpoint,
            dest_addr,
        };
        if !self.pub_bridge.deposit(pending) {
            return Err(ClientError::MaxPublicationsReached);
        }

        // Send AddPublication via CnC for bookkeeping (conductor tracks it).
        let _ = self.send_add_publication(correlation_id, stream_id, channel);

        Ok(Publication::new(pub_handle, correlation_id, channel))
    }

    /// Add a subscription on the given channel and stream.
    ///
    /// Sends an AddSubscription command via CnC. The receiver agent
    /// sets up the receive endpoint and deposits image handles into the
    /// subscription bridge. `Subscription::poll()` drains the bridge and
    /// reads committed fragments from shared image buffers.
    pub fn add_subscription(
        &mut self,
        channel: &str,
        stream_id: i32,
    ) -> Result<Subscription, ClientError> {
        let correlation_id = self.alloc_correlation_id();

        // Send AddSubscription via CnC.
        let cmd = AddSubscription::from_channel(
            correlation_id,
            self.client_id,
            stream_id,
            channel,
        ).ok_or(ClientError::ChannelInvalid)?;

        let mut buf = [0u8; ADD_SUBSCRIPTION_LENGTH];
        cmd.encode(&mut buf).ok_or(ClientError::InvalidParams)?;
        self.cnc.to_driver()
            .write(CMD_ADD_SUBSCRIPTION, &buf)
            .map_err(|_| ClientError::RegistrationFailed)?;

        Ok(Subscription::new(
            correlation_id,
            stream_id,
            channel,
            Arc::clone(&self.sub_bridge),
        ))
    }

    /// Send a keepalive heartbeat to the driver.
    ///
    /// Should be called periodically (e.g. every second) to prevent
    /// the driver from timing out this client. Pre-encoded, zero-alloc.
    pub fn heartbeat(&mut self) {
        let cmd = ClientKeepalive {
            correlation_id: 0,
            client_id: self.client_id,
        };
        if cmd.encode(&mut self.keepalive_buf).is_some() {
            let _ = self.cnc.to_driver()
                .write(CMD_CLIENT_KEEPALIVE, &self.keepalive_buf);
        }
    }

    /// Send a close command to the driver.
    pub fn close(&mut self) {
        let cmd = ClientKeepalive {
            correlation_id: 0,
            client_id: self.client_id,
        };
        let mut buf = [0u8; CLIENT_KEEPALIVE_LENGTH];
        if cmd.encode(&mut buf).is_some() {
            let _ = self.cnc.to_driver().write(CMD_CLIENT_CLOSE, &buf);
        }
    }

    /// Check if the driver is still alive.
    pub fn is_driver_alive(&self) -> bool {
        self.cnc.is_driver_alive(DEFAULT_DRIVER_TIMEOUT_MS)
    }

    /// Client ID assigned to this connection.
    #[inline]
    pub fn client_id(&self) -> i64 {
        self.client_id
    }

    // ---- Internal helpers ----

    fn send_add_publication(
        &self,
        correlation_id: i64,
        stream_id: i32,
        channel: &str,
    ) -> Result<(), ClientError> {
        let cmd = AddPublication::from_channel(
            correlation_id,
            self.client_id,
            stream_id,
            channel,
        ).ok_or(ClientError::ChannelInvalid)?;

        let mut buf = [0u8; ADD_PUBLICATION_LENGTH];
        cmd.encode(&mut buf).ok_or(ClientError::InvalidParams)?;
        self.cnc.to_driver()
            .write(CMD_ADD_PUBLICATION, &buf)
            .map_err(|_| ClientError::RegistrationFailed)?;
        Ok(())
    }
}

/// Convert a `SocketAddr` to a `libc::sockaddr_storage`.
fn sockaddr_to_storage(addr: SocketAddr) -> libc::sockaddr_storage {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: sockaddr_in fits within sockaddr_storage.
            let sin = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in)
            };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
        }
        SocketAddr::V6(v6) => {
            // SAFETY: sockaddr_in6 fits within sockaddr_storage.
            let sin6 = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage as *mut libc::sockaddr_in6)
            };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
        }
    }
    storage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_v4_to_storage() {
        let addr: SocketAddr = "127.0.0.1:40123".parse().unwrap();
        let storage = sockaddr_to_storage(addr);
        assert_eq!(storage.ss_family, libc::AF_INET as libc::sa_family_t);
    }

    #[test]
    fn sockaddr_v6_to_storage() {
        let addr: SocketAddr = "[::1]:40123".parse().unwrap();
        let storage = sockaddr_to_storage(addr);
        assert_eq!(storage.ss_family, libc::AF_INET6 as libc::sa_family_t);
    }
}

