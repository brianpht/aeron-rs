// MediaDriver - in-process media driver orchestrator.
//
// Creates and starts all three agents (conductor, sender, receiver) on
// dedicated threads. Provides connect() to create an Aeron client handle
// that works in-process via the shared CnC mmap and publication bridge.
//
// Usage:
//   let driver = MediaDriver::launch(DriverContext::default())?;
//   let mut aeron = driver.connect()?;
//   let mut pub_h = aeron.add_publication("aeron:udp?endpoint=127.0.0.1:40123", 10)?;
//   pub_h.offer(&[1, 2, 3, 4])?;
//   drop(aeron);
//   driver.close()?;

use std::sync::Arc;

use crate::agent::AgentError;
use crate::agent::conductor::ConductorAgent;
use crate::agent::receiver::ReceiverAgent;
use crate::agent::sender::SenderAgent;
use crate::agent::{AgentRunner, AgentRunnerHandle};
use crate::cnc::cnc_file::DriverCnc;
use crate::context::DriverContext;

use super::ClientError;
use super::aeron::Aeron;
use super::bridge::PublicationBridge;
use super::sub_bridge::{RecvEndpointBridge, SubscriptionBridge};

/// Default CnC ring buffer capacities.
const DEFAULT_TO_DRIVER_CAPACITY: usize = 64 * 1024;
const DEFAULT_TO_CLIENTS_CAPACITY: usize = 64 * 1024;

/// In-process media driver. Owns the conductor, sender, and receiver
/// agent threads plus the shared CnC and publication bridge.
pub struct MediaDriver {
    conductor_handle: AgentRunnerHandle,
    sender_handle: AgentRunnerHandle,
    receiver_handle: AgentRunnerHandle,
    /// Raw base pointer and length of the CnC mmap region.
    /// Valid as long as the conductor agent (which owns the DriverCnc) is alive.
    cnc_base: *mut u8,
    cnc_length: usize,
    /// Publication bridge shared with Aeron clients.
    pub_bridge: Arc<PublicationBridge>,
    /// Subscription bridge shared with Aeron clients.
    sub_bridge: Arc<SubscriptionBridge>,
    /// Receive endpoint bridge shared with Aeron clients.
    recv_endpoint_bridge: Arc<RecvEndpointBridge>,
    /// Config values passed to clients for transport creation.
    socket_rcvbuf: usize,
    socket_sndbuf: usize,
    multicast_ttl: u8,
    term_length: u32,
    mtu: u32,
}

// SAFETY: The cnc_base pointer is to an mmap'd region owned by the DriverCnc
// inside the conductor agent. The region remains valid until the conductor
// is stopped and joined (which happens in close/drop). The MediaDriver
// is the only entity that creates client mappings from this pointer.
unsafe impl Send for MediaDriver {}

impl MediaDriver {
    /// Launch the media driver with all three agents on dedicated threads.
    ///
    /// Cold path - allocates CnC, creates agents, spawns threads.
    pub fn launch(ctx: DriverContext) -> Result<Self, AgentError> {
        ctx.validate().map_err(|e| {
            AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        // Create CnC (anonymous mmap for in-process use).
        let cnc =
            DriverCnc::create_anonymous(DEFAULT_TO_DRIVER_CAPACITY, DEFAULT_TO_CLIENTS_CAPACITY)
                .map_err(|e| AgentError::Io(std::io::Error::other(e.to_string())))?;

        // Save CnC location before moving into conductor.
        let cnc_base = cnc.base_ptr();
        let cnc_length = cnc.length();

        // Create publication bridge.
        let pub_bridge = PublicationBridge::new();

        // Create subscription bridge.
        let sub_bridge = SubscriptionBridge::new();

        // Create receive endpoint bridge (client -> receiver).
        let recv_endpoint_bridge = RecvEndpointBridge::new();

        // Create conductor agent.
        let (conductor, _sender_queue, _receiver_queue) =
            ConductorAgent::new(cnc, ctx.driver_timeout_ns)?;

        // Create sender agent with bridge.
        let mut sender = SenderAgent::new(&ctx)?;
        sender.set_publication_bridge(Arc::clone(&pub_bridge));

        // Create receiver agent with subscription bridge and endpoint bridge.
        let mut receiver = ReceiverAgent::new(&ctx)?;
        receiver.set_subscription_bridge(Arc::clone(&sub_bridge));
        receiver.set_recv_endpoint_bridge(Arc::clone(&recv_endpoint_bridge));

        // Build idle strategies.
        let idle = ctx.idle_strategy();

        // Start agents on dedicated threads.
        let conductor_handle = AgentRunner::new(conductor, idle).start();
        let sender_handle = AgentRunner::new(sender, idle).start();
        let receiver_handle = AgentRunner::new(receiver, idle).start();

        // Save config for client transport creation.
        let socket_rcvbuf = ctx.socket_rcvbuf;
        let socket_sndbuf = ctx.socket_sndbuf;
        let multicast_ttl = ctx.multicast_ttl;
        let term_length = ctx.term_buffer_length;
        let mtu = ctx.mtu_length as u32;

        Ok(Self {
            conductor_handle,
            sender_handle,
            receiver_handle,
            cnc_base,
            cnc_length,
            pub_bridge,
            sub_bridge,
            recv_endpoint_bridge,
            socket_rcvbuf,
            socket_sndbuf,
            multicast_ttl,
            term_length,
            mtu,
        })
    }

    /// Create an in-process Aeron client connected to this driver.
    ///
    /// The client uses the shared CnC for commands/heartbeats and the
    /// publication bridge for delivering SenderPublication handles.
    pub fn connect(&self) -> Result<Aeron, ClientError> {
        unsafe {
            Aeron::connect_in_process(
                self.cnc_base,
                self.cnc_length,
                Arc::clone(&self.pub_bridge),
                Arc::clone(&self.sub_bridge),
                Arc::clone(&self.recv_endpoint_bridge),
                self.socket_rcvbuf,
                self.socket_sndbuf,
                self.multicast_ttl,
                self.term_length,
                self.mtu,
            )
        }
    }

    /// Stop all agents and wait for threads to exit.
    pub fn close(self) -> Result<(), AgentError> {
        // Stop in reverse order: receiver, sender, conductor.
        self.receiver_handle.stop();
        self.sender_handle.stop();
        self.conductor_handle.stop();

        // Join (waits for threads).
        // Ignore individual join errors - collect the first one.
        let r1 = self.receiver_handle.join();
        let r2 = self.sender_handle.join();
        let r3 = self.conductor_handle.join();

        r1.and(r2).and(r3)
    }
}
