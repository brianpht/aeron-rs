use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, RawFd};

use crate::context::DriverContext;
use super::channel::UdpChannel;

/// A UDP channel transport encapsulating one or two sockets
/// (one send, optionally a separate recv for multicast).
///
/// Mirrors `aeron_udp_channel_transport_t` in the C driver.
pub struct UdpChannelTransport {
    send_socket: Socket,
    #[allow(dead_code)]
    recv_socket: Option<Socket>,
    pub send_fd: RawFd,
    pub recv_fd: RawFd,
    pub is_multicast: bool,
    pub bound_addr: SocketAddr,
    pub connect_addr: Option<SocketAddr>,

    /// Opaque pointer to the owning endpoint (set by endpoint).
    pub dispatch_clientd: usize,
    /// Opaque pointer to the destination context (set by endpoint).
    pub destination_clientd: usize,
    /// Index assigned by the poller.
    pub poller_index: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("socket: {0}")]
    Socket(#[source] io::Error),
    #[error("bind {addr}: {source}")]
    Bind { addr: SocketAddr, source: io::Error },
    #[error("connect {addr}: {source}")]
    Connect { addr: SocketAddr, source: io::Error },
    #[error("setsockopt: {0}")]
    SockOpt(#[source] io::Error),
    #[error("channel: {0}")]
    Channel(#[from] super::channel::ChannelError),
}

impl UdpChannelTransport {
    /// Open a transport for the given channel.
    pub fn open(
        channel: &UdpChannel,
        local_addr: &SocketAddr,
        remote_addr: &SocketAddr,
        ctx: &DriverContext,
    ) -> Result<Self, TransportError> {
        let domain = match remote_addr {
            SocketAddr::V4(_) => Domain::IPV4,
            SocketAddr::V6(_) => Domain::IPV6,
        };

        let send_socket =
            Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(TransportError::Socket)?;

        send_socket
            .set_nonblocking(true)
            .map_err(TransportError::Socket)?;

        if ctx.socket_sndbuf > 0 {
            send_socket
                .set_send_buffer_size(ctx.socket_sndbuf)
                .map_err(TransportError::SockOpt)?;
        }

        let recv_socket;
        let bound_addr;

        if channel.is_multicast {
            let (rs, ba) =
                Self::setup_multicast(&send_socket, channel, local_addr, remote_addr, ctx, domain)?;
            recv_socket = Some(rs);
            bound_addr = ba;
        } else {
            send_socket
                .bind(&(*local_addr).into())
                .map_err(|e| TransportError::Bind {
                    addr: *local_addr,
                    source: e,
                })?;
            bound_addr = send_socket
                .local_addr()
                .map_err(TransportError::Socket)?
                .as_socket()
                .unwrap_or(*local_addr);
            recv_socket = None;
        }

        // Set recv buffer on the receiving socket.
        let recv_sock_ref = recv_socket.as_ref().unwrap_or(&send_socket);
        if ctx.socket_rcvbuf > 0 {
            recv_sock_ref
                .set_recv_buffer_size(ctx.socket_rcvbuf)
                .map_err(TransportError::SockOpt)?;
        }

        let send_fd = send_socket.as_raw_fd();
        let recv_fd = recv_socket.as_ref().map_or(send_fd, |s| s.as_raw_fd());

        Ok(Self {
            send_socket,
            recv_socket,
            send_fd,
            recv_fd,
            is_multicast: channel.is_multicast,
            bound_addr,
            connect_addr: None,
            dispatch_clientd: 0,
            destination_clientd: 0,
            poller_index: None,
        })
    }

    /// Connect the send socket (for unicast destinations).
    pub fn connect(&mut self, addr: &SocketAddr) -> Result<(), TransportError> {
        self.send_socket
            .connect(&(*addr).into())
            .map_err(|e| TransportError::Connect {
                addr: *addr,
                source: e,
            })?;
        self.connect_addr = Some(*addr);
        Ok(())
    }

    fn setup_multicast(
        send_socket: &Socket,
        channel: &UdpChannel,
        local_addr: &SocketAddr,
        remote_addr: &SocketAddr,
        ctx: &DriverContext,
        domain: Domain,
    ) -> Result<(Socket, SocketAddr), TransportError> {
        let recv_socket =
            Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(TransportError::Socket)?;

        recv_socket
            .set_reuse_address(true)
            .map_err(TransportError::SockOpt)?;

        #[cfg(not(target_os = "windows"))]
        recv_socket
            .set_reuse_port(true)
            .map_err(TransportError::SockOpt)?;

        recv_socket
            .set_nonblocking(true)
            .map_err(TransportError::Socket)?;

        // Bind recv socket to the multicast port on INADDR_ANY.
        let bind_addr: SocketAddr = match remote_addr {
            SocketAddr::V4(v4) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, v4.port())),
            SocketAddr::V6(v6) => SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), v6.port()),
        };
        recv_socket
            .bind(&bind_addr.into())
            .map_err(|e| TransportError::Bind {
                addr: bind_addr,
                source: e,
            })?;

        // Join multicast group.
        match remote_addr {
            SocketAddr::V4(v4) => {
                let iface = channel
                    .interface_addr
                    .and_then(|ip| match ip {
                        std::net::IpAddr::V4(v) => Some(v),
                        _ => None,
                    })
                    .unwrap_or(Ipv4Addr::UNSPECIFIED);
                recv_socket
                    .join_multicast_v4(v4.ip(), &iface)
                    .map_err(TransportError::SockOpt)?;
                send_socket
                    .set_multicast_if_v4(&iface)
                    .map_err(TransportError::SockOpt)?;
                if ctx.multicast_ttl > 0 {
                    send_socket
                        .set_multicast_ttl_v4(ctx.multicast_ttl as u32)
                        .map_err(TransportError::SockOpt)?;
                }
                send_socket
                    .set_multicast_loop_v4(false)
                    .map_err(TransportError::SockOpt)?;
            }
            SocketAddr::V6(v6) => {
                let iface_idx = 0u32; // default
                recv_socket
                    .join_multicast_v6(v6.ip(), iface_idx)
                    .map_err(TransportError::SockOpt)?;
                send_socket
                    .set_multicast_if_v6(iface_idx)
                    .map_err(TransportError::SockOpt)?;
                send_socket
                    .set_multicast_loop_v6(false)
                    .map_err(TransportError::SockOpt)?;
            }
        }

        // Bind send socket to local.
        send_socket
            .bind(&(*local_addr).into())
            .map_err(|e| TransportError::Bind {
                addr: *local_addr,
                source: e,
            })?;

        let bound = recv_socket
            .local_addr()
            .map_err(TransportError::Socket)?
            .as_socket()
            .unwrap_or(bind_addr);

        Ok((recv_socket, bound))
    }

    pub fn close(&mut self) {
        // socket2::Socket closes fd on Drop.
    }
}