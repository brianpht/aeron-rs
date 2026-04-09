// Conductor agent - reads client commands from the CnC to-driver ring buffer,
// dispatches them to sender and receiver agents via internal SPSC command
// queues, and writes responses to the to-clients broadcast buffer.
//
// In the 3-thread model:
//   - Conductor thread: reads CnC commands, dispatches, sends responses
//   - Sender thread: reads conductor->sender queue, mutates SenderAgent
//   - Receiver thread: reads conductor->receiver queue, mutates ReceiverAgent
//
// The internal command queues are MpscRingBuffers (used as SPSC since there
// is only one conductor). This avoids any shared mutable state between agents.
//
// Implements the `Agent` trait - monomorphized, no dyn, zero-allocation
// in steady state.

use super::{Agent, AgentError};
use crate::clock::CachedNanoClock;
use crate::cnc::cnc_file::DriverCnc;
use crate::cnc::command::*;
use crate::cnc::ring_buffer::{self, MpscRingBuffer};

// ---- Internal command types for agent queues ----

/// Command sent from conductor to sender agent.
/// Fixed-size, Copy. No allocation.
#[derive(Clone, Copy)]
pub enum SenderCommand {
    /// Add an endpoint + publication.
    AddPublication {
        correlation_id: i64,
        stream_id: i32,
        session_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    },
    /// Remove a publication by registration_id.
    RemovePublication {
        correlation_id: i64,
        registration_id: i64,
    },
}

/// Command sent from conductor to receiver agent.
/// Fixed-size, Copy. No allocation.
#[derive(Clone, Copy)]
pub enum ReceiverCommand {
    /// Add a subscription (endpoint + stream).
    AddSubscription {
        correlation_id: i64,
        stream_id: i32,
    },
    /// Remove a subscription.
    RemoveSubscription {
        correlation_id: i64,
        registration_id: i64,
    },
}

// ---- Internal command encoding (for the SPSC queues) ----

const INTERNAL_CMD_ADD_PUB: i32 = 1;
const INTERNAL_CMD_REMOVE_PUB: i32 = 2;
const INTERNAL_CMD_ADD_SUB: i32 = 1;
const INTERNAL_CMD_REMOVE_SUB: i32 = 2;

/// Encode a SenderCommand into bytes for the internal queue.
fn encode_sender_cmd(cmd: &SenderCommand, buf: &mut [u8; 64]) -> (i32, usize) {
    match cmd {
        SenderCommand::AddPublication {
            correlation_id, stream_id, session_id,
            initial_term_id, term_length, mtu,
        } => {
            buf[0..8].copy_from_slice(&correlation_id.to_le_bytes());
            buf[8..12].copy_from_slice(&stream_id.to_le_bytes());
            buf[12..16].copy_from_slice(&session_id.to_le_bytes());
            buf[16..20].copy_from_slice(&initial_term_id.to_le_bytes());
            buf[20..24].copy_from_slice(&term_length.to_le_bytes());
            buf[24..28].copy_from_slice(&mtu.to_le_bytes());
            (INTERNAL_CMD_ADD_PUB, 28)
        }
        SenderCommand::RemovePublication { correlation_id, registration_id } => {
            buf[0..8].copy_from_slice(&correlation_id.to_le_bytes());
            buf[8..16].copy_from_slice(&registration_id.to_le_bytes());
            (INTERNAL_CMD_REMOVE_PUB, 16)
        }
    }
}

/// Decode a SenderCommand from the internal queue.
fn decode_sender_cmd(msg_type: i32, data: &[u8]) -> Option<SenderCommand> {
    match msg_type {
        INTERNAL_CMD_ADD_PUB if data.len() >= 28 => {
            Some(SenderCommand::AddPublication {
                correlation_id: i64::from_le_bytes(data[0..8].try_into().ok()?),
                stream_id: i32::from_le_bytes(data[8..12].try_into().ok()?),
                session_id: i32::from_le_bytes(data[12..16].try_into().ok()?),
                initial_term_id: i32::from_le_bytes(data[16..20].try_into().ok()?),
                term_length: u32::from_le_bytes(data[20..24].try_into().ok()?),
                mtu: u32::from_le_bytes(data[24..28].try_into().ok()?),
            })
        }
        INTERNAL_CMD_REMOVE_PUB if data.len() >= 16 => {
            Some(SenderCommand::RemovePublication {
                correlation_id: i64::from_le_bytes(data[0..8].try_into().ok()?),
                registration_id: i64::from_le_bytes(data[8..16].try_into().ok()?),
            })
        }
        _ => None,
    }
}

/// Encode a ReceiverCommand into bytes for the internal queue.
fn encode_receiver_cmd(cmd: &ReceiverCommand, buf: &mut [u8; 64]) -> (i32, usize) {
    match cmd {
        ReceiverCommand::AddSubscription { correlation_id, stream_id } => {
            buf[0..8].copy_from_slice(&correlation_id.to_le_bytes());
            buf[8..12].copy_from_slice(&stream_id.to_le_bytes());
            (INTERNAL_CMD_ADD_SUB, 12)
        }
        ReceiverCommand::RemoveSubscription { correlation_id, registration_id } => {
            buf[0..8].copy_from_slice(&correlation_id.to_le_bytes());
            buf[8..16].copy_from_slice(&registration_id.to_le_bytes());
            (INTERNAL_CMD_REMOVE_SUB, 16)
        }
    }
}

/// Decode a ReceiverCommand from the internal queue.
fn decode_receiver_cmd(msg_type: i32, data: &[u8]) -> Option<ReceiverCommand> {
    match msg_type {
        INTERNAL_CMD_ADD_SUB if data.len() >= 12 => {
            Some(ReceiverCommand::AddSubscription {
                correlation_id: i64::from_le_bytes(data[0..8].try_into().ok()?),
                stream_id: i32::from_le_bytes(data[8..12].try_into().ok()?),
            })
        }
        INTERNAL_CMD_REMOVE_SUB if data.len() >= 16 => {
            Some(ReceiverCommand::RemoveSubscription {
                correlation_id: i64::from_le_bytes(data[0..8].try_into().ok()?),
                registration_id: i64::from_le_bytes(data[8..16].try_into().ok()?),
            })
        }
        _ => None,
    }
}

// ---- ConductorAgent ----

/// Default capacity for internal conductor->agent command queues.
const INTERNAL_QUEUE_CAPACITY: usize = 4096;

/// Maximum clients tracked for liveness.
const MAX_CLIENTS: usize = 64;

/// Per-client liveness state.
#[derive(Clone, Copy)]
struct ClientEntry {
    client_id: i64,
    last_keepalive_ns: i64,
    active: bool,
}

impl Default for ClientEntry {
    fn default() -> Self {
        Self {
            client_id: 0,
            last_keepalive_ns: 0,
            active: false,
        }
    }
}

/// The conductor agent. Reads commands from the CnC to-driver ring buffer,
/// dispatches to sender/receiver via internal SPSC queues, writes responses.
///
/// Implements `Agent` - runs on its own thread via `AgentRunner`.
pub struct ConductorAgent {
    cnc: DriverCnc,
    /// Internal queue: conductor -> sender.
    to_sender: MpscRingBuffer,
    /// Internal queue: conductor -> receiver.
    to_receiver: MpscRingBuffer,
    /// Backing memory for the queues (must outlive the ring buffers).
    _to_sender_buf: Vec<u8>,
    _to_receiver_buf: Vec<u8>,
    /// Monotonic registration ID counter.
    next_registration_id: i64,
    /// Monotonic session ID counter.
    next_session_id: i32,
    /// Client liveness table.
    clients: [ClientEntry; MAX_CLIENTS],
    client_count: usize,
    /// Configuration.
    #[allow(dead_code)] // Used when client liveness timeout is wired.
    driver_timeout_ns: i64,
    clock: CachedNanoClock,
    /// Scratch buffer for encoding responses (pre-allocated, never resized).
    scratch: [u8; 512],
}

// SAFETY: ConductorAgent is single-threaded (owned by one AgentRunner).
unsafe impl Send for ConductorAgent {}

impl ConductorAgent {
    /// Create a new conductor agent.
    ///
    /// `cnc` is the driver-side CnC handle (provides to-driver + to-clients).
    /// Returns the conductor plus handles to the internal command queues
    /// that the sender and receiver agents should poll.
    pub fn new(
        cnc: DriverCnc,
        driver_timeout_ns: i64,
    ) -> Result<(Self, SenderCommandQueue, ReceiverCommandQueue), AgentError> {
        let to_sender_size = ring_buffer::required_buffer_size(INTERNAL_QUEUE_CAPACITY);
        let to_receiver_size = ring_buffer::required_buffer_size(INTERNAL_QUEUE_CAPACITY);

        let mut to_sender_buf = vec![0u8; to_sender_size];
        let mut to_receiver_buf = vec![0u8; to_receiver_size];

        let to_sender = unsafe {
            MpscRingBuffer::new(to_sender_buf.as_mut_ptr(), INTERNAL_QUEUE_CAPACITY)
        }.map_err(|_| AgentError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "failed to create sender command queue",
        )))?;

        let to_receiver = unsafe {
            MpscRingBuffer::new(to_receiver_buf.as_mut_ptr(), INTERNAL_QUEUE_CAPACITY)
        }.map_err(|_| AgentError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "failed to create receiver command queue",
        )))?;

        // Create read handles for the sender and receiver agents.
        let sender_queue = SenderCommandQueue {
            rb: unsafe {
                MpscRingBuffer::new(to_sender_buf.as_mut_ptr(), INTERNAL_QUEUE_CAPACITY)
            }.map_err(|_| AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "failed to create sender queue reader",
            )))?,
        };

        let receiver_queue = ReceiverCommandQueue {
            rb: unsafe {
                MpscRingBuffer::new(to_receiver_buf.as_mut_ptr(), INTERNAL_QUEUE_CAPACITY)
            }.map_err(|_| AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "failed to create receiver queue reader",
            )))?,
        };

        let conductor = Self {
            cnc,
            to_sender,
            to_receiver,
            _to_sender_buf: to_sender_buf,
            _to_receiver_buf: to_receiver_buf,
            next_registration_id: 1,
            next_session_id: 1,
            clients: [ClientEntry::default(); MAX_CLIENTS],
            client_count: 0,
            driver_timeout_ns,
            clock: CachedNanoClock::new(),
            scratch: [0u8; 512],
        };

        Ok((conductor, sender_queue, receiver_queue))
    }

    /// Allocate the next registration ID.
    fn alloc_registration_id(&mut self) -> i64 {
        let id = self.next_registration_id;
        self.next_registration_id = id.wrapping_add(1);
        id
    }

    /// Allocate the next session ID.
    fn alloc_session_id(&mut self) -> i32 {
        let id = self.next_session_id;
        self.next_session_id = id.wrapping_add(1);
        id
    }

    /// Update client liveness.
    fn touch_client(&mut self, client_id: i64, now_ns: i64) {
        // Linear scan - MAX_CLIENTS is small (64).
        for i in 0..self.client_count {
            if self.clients[i].active && self.clients[i].client_id == client_id {
                self.clients[i].last_keepalive_ns = now_ns;
                return;
            }
        }
        // New client - add if space.
        if self.client_count < MAX_CLIENTS {
            self.clients[self.client_count] = ClientEntry {
                client_id,
                last_keepalive_ns: now_ns,
                active: true,
            };
            self.client_count += 1;
        }
    }

    /// Process one command from the to-driver ring buffer.
    fn process_command(&mut self, msg_type: i32, payload: &[u8], now_ns: i64) {
        match msg_type {
            CMD_ADD_PUBLICATION => {
                if let Some(cmd) = AddPublication::decode(payload) {
                    self.handle_add_publication(cmd, now_ns);
                }
            }
            CMD_REMOVE_PUBLICATION => {
                if let Some(cmd) = RemovePublication::decode(payload) {
                    self.handle_remove_publication(cmd);
                }
            }
            CMD_ADD_SUBSCRIPTION => {
                if let Some(cmd) = AddSubscription::decode(payload) {
                    self.handle_add_subscription(cmd, now_ns);
                }
            }
            CMD_REMOVE_SUBSCRIPTION => {
                if let Some(cmd) = RemoveSubscription::decode(payload) {
                    self.handle_remove_subscription(cmd);
                }
            }
            CMD_CLIENT_KEEPALIVE => {
                if let Some(cmd) = ClientKeepalive::decode(payload) {
                    self.touch_client(cmd.client_id, now_ns);
                }
            }
            CMD_CLIENT_CLOSE => {
                // Client close - could clean up resources.
                // For now, just let the liveness timeout handle it.
            }
            _ => {
                // Unknown command - ignore.
                tracing::warn!("unknown command type: {msg_type}");
            }
        }
    }

    fn handle_add_publication(&mut self, cmd: AddPublication, now_ns: i64) {
        self.touch_client(cmd.client_id, now_ns);

        let registration_id = self.alloc_registration_id();
        let session_id = self.alloc_session_id();

        // Queue the command to the sender agent.
        let sender_cmd = SenderCommand::AddPublication {
            correlation_id: cmd.correlation_id,
            stream_id: cmd.stream_id,
            session_id,
            initial_term_id: 0,
            term_length: 64 * 1024, // Default - could be from channel params.
            mtu: 1408,              // Default MTU.
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_sender_cmd(&sender_cmd, &mut buf);
        let _ = self.to_sender.write(msg_type, &buf[..len]);

        // Send PublicationReady response to the client.
        let rsp = PublicationReady {
            correlation_id: cmd.correlation_id,
            registration_id,
            session_id,
            stream_id: cmd.stream_id,
            position_limit_counter_id: 0,
            channel_status_indicator_id: 0,
        };
        if let Some(len) = rsp.encode(&mut self.scratch) {
            let _ = self.cnc.to_clients().transmit(RSP_PUBLICATION_READY, &self.scratch[..len]);
        }
    }

    fn handle_remove_publication(&mut self, cmd: RemovePublication) {
        let sender_cmd = SenderCommand::RemovePublication {
            correlation_id: cmd.correlation_id,
            registration_id: cmd.registration_id,
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_sender_cmd(&sender_cmd, &mut buf);
        let _ = self.to_sender.write(msg_type, &buf[..len]);
    }

    fn handle_add_subscription(&mut self, cmd: AddSubscription, now_ns: i64) {
        self.touch_client(cmd.client_id, now_ns);

        // Queue the command to the receiver agent.
        let recv_cmd = ReceiverCommand::AddSubscription {
            correlation_id: cmd.correlation_id,
            stream_id: cmd.stream_id,
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_receiver_cmd(&recv_cmd, &mut buf);
        let _ = self.to_receiver.write(msg_type, &buf[..len]);

        // Send SubscriptionReady response.
        let rsp = SubscriptionReady {
            correlation_id: cmd.correlation_id,
            channel_status_indicator_id: 0,
        };
        if let Some(len) = rsp.encode(&mut self.scratch) {
            let _ = self.cnc.to_clients().transmit(RSP_SUBSCRIPTION_READY, &self.scratch[..len]);
        }
    }

    fn handle_remove_subscription(&mut self, cmd: RemoveSubscription) {
        let recv_cmd = ReceiverCommand::RemoveSubscription {
            correlation_id: cmd.correlation_id,
            registration_id: cmd.registration_id,
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_receiver_cmd(&recv_cmd, &mut buf);
        let _ = self.to_receiver.write(msg_type, &buf[..len]);
    }
}

impl Agent for ConductorAgent {
    fn name(&self) -> &str {
        "aeron-conductor"
    }

    fn do_work(&mut self) -> Result<i32, AgentError> {
        let now_ns = self.clock.update();
        let mut work_count = 0i32;

        // Step 1: Read commands from the to-driver ring buffer.
        // Collect commands into a stack-local buffer to avoid borrowing self
        // during the read callback.
        const MAX_CMDS_PER_CYCLE: usize = 32;
        let mut cmd_buf: [(i32, [u8; 512], usize); MAX_CMDS_PER_CYCLE] =
            [(0, [0u8; 512], 0); MAX_CMDS_PER_CYCLE];
        let mut cmd_count = 0usize;

        let msgs = self.cnc.to_driver().read(|msg_type, data| {
            if cmd_count < MAX_CMDS_PER_CYCLE {
                let len = data.len().min(512);
                cmd_buf[cmd_count].0 = msg_type;
                cmd_buf[cmd_count].2 = len;
                cmd_buf[cmd_count].1[..len].copy_from_slice(&data[..len]);
                cmd_count += 1;
            }
        });
        work_count += msgs;

        // Process collected commands.
        for i in 0..cmd_count {
            let (msg_type, ref payload, len) = cmd_buf[i];
            self.process_command(msg_type, &payload[..len], now_ns);
        }

        // Step 2: Update driver heartbeat.
        self.cnc.update_heartbeat();

        Ok(work_count)
    }
}

// ---- Agent-side command queue readers ----

/// Handle for the sender agent to read commands from the conductor.
///
/// The sender agent calls `poll()` in its duty cycle to pick up
/// publication add/remove commands.
pub struct SenderCommandQueue {
    rb: MpscRingBuffer,
}

// SAFETY: Moved to the sender thread, used single-threaded there.
unsafe impl Send for SenderCommandQueue {}

impl SenderCommandQueue {
    /// Poll for commands from the conductor.
    /// Calls `handler(cmd)` for each command. Returns work count.
    pub fn poll<F>(&self, mut handler: F) -> i32
    where
        F: FnMut(SenderCommand),
    {
        self.rb.read(|msg_type, data| {
            if let Some(cmd) = decode_sender_cmd(msg_type, data) {
                handler(cmd);
            }
        })
    }
}

/// Handle for the receiver agent to read commands from the conductor.
pub struct ReceiverCommandQueue {
    rb: MpscRingBuffer,
}

unsafe impl Send for ReceiverCommandQueue {}

impl ReceiverCommandQueue {
    /// Poll for commands from the conductor.
    pub fn poll<F>(&self, mut handler: F) -> i32
    where
        F: FnMut(ReceiverCommand),
    {
        self.rb.read(|msg_type, data| {
            if let Some(cmd) = decode_receiver_cmd(msg_type, data) {
                handler(cmd);
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cnc::cnc_file::DriverCnc;

    const TO_DRIVER: usize = 4096;
    const TO_CLIENTS: usize = 4096;

    fn make_conductor() -> (ConductorAgent, SenderCommandQueue, ReceiverCommandQueue) {
        let cnc = DriverCnc::create_anonymous(TO_DRIVER, TO_CLIENTS)
            .expect("create CnC");
        ConductorAgent::new(cnc, 10_000_000_000).expect("create conductor")
    }

    #[test]
    fn conductor_creates_successfully() {
        let (conductor, _, _) = make_conductor();
        assert_eq!(conductor.name(), "aeron-conductor");
    }

    #[test]
    fn conductor_idle_do_work() {
        let (mut conductor, _, _) = make_conductor();
        let work = conductor.do_work().expect("do_work");
        assert_eq!(work, 0, "no commands - should be idle");
    }

    #[test]
    fn conductor_processes_add_publication() {
        let (mut conductor, sender_queue, _) = make_conductor();

        // Simulate a client writing an AddPublication command.
        let cmd = AddPublication::from_channel(
            1, 100, 10, "aeron:udp?endpoint=127.0.0.1:40123",
        ).expect("cmd");
        let mut buf = [0u8; ADD_PUBLICATION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        conductor.cnc.to_driver().write(CMD_ADD_PUBLICATION, &buf).expect("write");

        // Run the conductor duty cycle.
        let work = conductor.do_work().expect("do_work");
        assert!(work > 0, "should have processed the command");

        // Check that the sender got a command.
        let mut received = false;
        sender_queue.poll(|cmd| {
            match cmd {
                SenderCommand::AddPublication { stream_id, .. } => {
                    assert_eq!(stream_id, 10);
                    received = true;
                }
                _ => panic!("unexpected command"),
            }
        });
        assert!(received, "sender should have received AddPublication");
    }

    #[test]
    fn conductor_processes_add_subscription() {
        let (mut conductor, _, receiver_queue) = make_conductor();

        let cmd = AddSubscription::from_channel(
            2, 200, 20, "aeron:udp?endpoint=127.0.0.1:40124",
        ).expect("cmd");
        let mut buf = [0u8; ADD_SUBSCRIPTION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        conductor.cnc.to_driver().write(CMD_ADD_SUBSCRIPTION, &buf).expect("write");

        let work = conductor.do_work().expect("do_work");
        assert!(work > 0);

        let mut received = false;
        receiver_queue.poll(|cmd| {
            match cmd {
                ReceiverCommand::AddSubscription { stream_id, .. } => {
                    assert_eq!(stream_id, 20);
                    received = true;
                }
                _ => panic!("unexpected command"),
            }
        });
        assert!(received, "receiver should have received AddSubscription");
    }

    #[test]
    fn conductor_sends_publication_ready_response() {
        let (mut conductor, _, _) = make_conductor();

        // Create a client-side view of the CnC.
        let mut client = unsafe {
            crate::cnc::cnc_file::ClientCnc::from_ptr(
                conductor.cnc.base_ptr(),
                conductor.cnc.length(),
            )
        }.expect("client");

        // Write AddPublication command.
        let cmd = AddPublication::from_channel(
            42, 100, 10, "aeron:udp?endpoint=127.0.0.1:40123",
        ).expect("cmd");
        let mut buf = [0u8; ADD_PUBLICATION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        client.to_driver().write(CMD_ADD_PUBLICATION, &buf).expect("write");

        // Run conductor.
        conductor.do_work().expect("do_work");

        // Client should see PublicationReady response.
        let mut got_response = false;
        client.to_clients().receive(|msg_type, data| {
            assert_eq!(msg_type, RSP_PUBLICATION_READY);
            let rsp = PublicationReady::decode(data).expect("decode");
            assert_eq!(rsp.correlation_id, 42);
            assert_eq!(rsp.stream_id, 10);
            assert!(rsp.registration_id > 0);
            got_response = true;
        });
        assert!(got_response, "client should receive PublicationReady");
    }

    #[test]
    fn conductor_handles_keepalive() {
        let (mut conductor, _, _) = make_conductor();

        let cmd = ClientKeepalive {
            correlation_id: 0,
            client_id: 999,
        };
        let mut buf = [0u8; CLIENT_KEEPALIVE_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        conductor.cnc.to_driver().write(CMD_CLIENT_KEEPALIVE, &buf).expect("write");

        let work = conductor.do_work().expect("do_work");
        assert!(work > 0);

        // Client should be tracked.
        assert_eq!(conductor.client_count, 1);
        assert_eq!(conductor.clients[0].client_id, 999);
        assert!(conductor.clients[0].active);
    }

    #[test]
    fn internal_sender_command_encode_decode() {
        let cmd = SenderCommand::AddPublication {
            correlation_id: 1,
            stream_id: 10,
            session_id: 42,
            initial_term_id: 0,
            term_length: 65536,
            mtu: 1408,
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_sender_cmd(&cmd, &mut buf);
        let decoded = decode_sender_cmd(msg_type, &buf[..len]).expect("decode");
        match decoded {
            SenderCommand::AddPublication { stream_id, session_id, .. } => {
                assert_eq!(stream_id, 10);
                assert_eq!(session_id, 42);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn internal_receiver_command_encode_decode() {
        let cmd = ReceiverCommand::AddSubscription {
            correlation_id: 2,
            stream_id: 20,
        };
        let mut buf = [0u8; 64];
        let (msg_type, len) = encode_receiver_cmd(&cmd, &mut buf);
        let decoded = decode_receiver_cmd(msg_type, &buf[..len]).expect("decode");
        match decoded {
            ReceiverCommand::AddSubscription { stream_id, .. } => {
                assert_eq!(stream_id, 20);
            }
            _ => panic!("wrong variant"),
        }
    }
}

