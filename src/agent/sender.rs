// The Sender Agent — owns the send-side io_uring poller and all
// SendChannelEndpoints. Mirrors aeron_driver_sender_t.

use super::{Agent, AgentError};
use crate::clock::CachedNanoClock;

use crate::context::DriverContext;
use crate::media::poller::{RecvMessage, TransportPoller};
use crate::media::send_channel_endpoint::SendChannelEndpoint;
use crate::media::uring_poller::UringTransportPoller;

/// Represents one active network publication within the sender.
struct PublicationEntry {
    session_id: i32,
    stream_id: i32,
    term_id: i32,
    term_offset: i32,
    term_length: i32,
    mtu: i32,
    endpoint_idx: usize,
    /// Destination for unconnected sends.
    dest_addr: Option<libc::sockaddr_storage>,
    needs_setup: bool,
    time_of_last_send_ns: i64,
    time_of_last_setup_ns: i64,
}

/// Maximum expected endpoints / publications. Pre-sized to avoid
/// reallocation if add_endpoint / add_publication is called during operation.
const MAX_ENDPOINTS: usize = 16;
const MAX_PUBLICATIONS: usize = 64;

pub struct SenderAgent {
    poller: UringTransportPoller,
    endpoints: Vec<SendChannelEndpoint>,
    publications: Vec<PublicationEntry>,
    clock: CachedNanoClock,
    duty_cycle_ratio: usize,
    duty_cycle_counter: usize,
    round_robin_index: usize,
    heartbeat_interval_ns: i64,
}

impl SenderAgent {
    pub fn new(ctx: &DriverContext) -> Result<Self, AgentError> {
        ctx.validate().map_err(|e| {
            AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let poller = UringTransportPoller::new(ctx)?;
        Ok(Self {
            poller,
            endpoints: Vec::with_capacity(MAX_ENDPOINTS),
            publications: Vec::with_capacity(MAX_PUBLICATIONS),
            clock: CachedNanoClock::new(),
            duty_cycle_ratio: ctx.send_duty_cycle_ratio,
            duty_cycle_counter: 0,
            round_robin_index: 0,
            heartbeat_interval_ns: ctx.heartbeat_interval_ns,
        })
    }

    /// Add an endpoint and register it with the poller.
    pub fn add_endpoint(&mut self, mut endpoint: SendChannelEndpoint) -> std::io::Result<usize> {
        endpoint.register(&mut self.poller)?;
        let idx = self.endpoints.len();
        self.endpoints.push(endpoint);
        Ok(idx)
    }

    /// Add a publication to be serviced by the sender agent.
    pub fn add_publication(
        &mut self,
        endpoint_idx: usize,
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: i32,
        mtu: i32,
    ) {
        self.publications.push(PublicationEntry {
            session_id,
            stream_id,
            term_id: initial_term_id,
            term_offset: 0,
            term_length,
            mtu,
            endpoint_idx,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
        });
    }

    // ── Internal duty-cycle steps ──

    fn do_send(&mut self, now_ns: i64) -> i32 {
        if self.publications.is_empty() {
            return 0;
        }
        let len = self.publications.len();
        // Wrap round-robin index without modulo (forbidden in hot path).
        if self.round_robin_index >= len {
            self.round_robin_index = 0;
        }
        let start = self.round_robin_index;
        self.round_robin_index = start.wrapping_add(1);

        let total_bytes = 0i32;

        let mut idx = start;
        for _ in 0..len {
            let pub_entry = &self.publications[idx];
            let ep_idx = pub_entry.endpoint_idx;
            let session_id = pub_entry.session_id;
            let stream_id = pub_entry.stream_id;
            let term_id = pub_entry.term_id;
            let term_offset = pub_entry.term_offset;
            let term_length = pub_entry.term_length;
            let mtu = pub_entry.mtu;
            let needs_setup = pub_entry.needs_setup;
            let last_setup_ns = pub_entry.time_of_last_setup_ns;
            let last_send_ns = pub_entry.time_of_last_send_ns;
            let dest_addr = pub_entry.dest_addr;

            // Send setup if needed.
            if needs_setup
                && (now_ns.wrapping_sub(last_setup_ns) > self.heartbeat_interval_ns)
            {
                let ep = &mut self.endpoints[ep_idx];
                let _ = ep.send_setup(
                    &mut self.poller,
                    session_id,
                    stream_id,
                    term_id,
                    term_id,
                    term_offset,
                    term_length,
                    mtu,
                    0, // ttl
                    dest_addr.as_ref(),
                );
                self.publications[idx].time_of_last_setup_ns = now_ns;
            }

            // Send heartbeat if idle.
            if now_ns.wrapping_sub(last_send_ns) > self.heartbeat_interval_ns {
                let ep = &mut self.endpoints[ep_idx];
                let _ = ep.send_heartbeat(
                    &mut self.poller,
                    session_id,
                    stream_id,
                    term_id,
                    term_offset,
                    dest_addr.as_ref(),
                );
                self.publications[idx].time_of_last_send_ns = now_ns;
            }

            // TODO: scan term buffer for pending data frames and submit via
            // endpoint.send_data(). This is the phase 3 integration point.

            // Advance round-robin index (branch-based wrap, no modulo).
            idx += 1;
            if idx >= len {
                idx = 0;
            }
        }

        let _ = self.poller.flush();
        total_bytes
    }

    fn poll_control(&mut self) -> i32 {
        let endpoints = &mut self.endpoints;
        let result = self.poller.poll_recv(|msg: RecvMessage<'_>| {
            // O(1) dispatch: transport_idx maps 1:1 to endpoint index
            // (both assigned sequentially via add_endpoint → register → add_transport).
            if let Some(ep) = endpoints.get_mut(msg.transport_idx) {
                ep.on_message(msg.data, msg.source_addr);
            }
        });

        match result {
            Ok(r) => r.messages_received as i32,
            Err(e) => {
                tracing::warn!("sender poll error: {e}");
                0
            }
        }
    }

    fn process_sm_and_nak(&mut self) {
        for ep in &mut self.endpoints {
            ep.drain_sm(|sm| {
                // Find matching publication and update sender position.
                for pub_entry in &mut self.publications {
                    if pub_entry.session_id == sm.session_id
                        && pub_entry.stream_id == sm.stream_id
                    {
                        pub_entry.needs_setup = false;
                        // Update flow control state (phase 3).
                    }
                }
            });

            ep.drain_naks(|nak| {
                // Schedule retransmit (phase 3).
                let sid = nak.session_id;
                let stid = nak.stream_id;
                let toff = nak.term_offset;
                let nlen = nak.length;
                tracing::trace!(
                    session = sid,
                    stream = stid,
                    offset = toff,
                    len = nlen,
                    "schedule retransmit"
                );
            });
        }
    }
}

impl Agent for SenderAgent {
    fn name(&self) -> &str {
        "aeron-sender"
    }

    fn do_work(&mut self) -> Result<i32, AgentError> {
        let now_ns = self.clock.update();
        let mut work_count = 0i32;

        // Step 1: Send data frames.
        let bytes_sent = self.do_send(now_ns);

        // Step 2: Poll for incoming SM / NAK / ERR.
        if bytes_sent == 0 || self.duty_cycle_counter >= self.duty_cycle_ratio {
            work_count += self.poll_control();
            self.process_sm_and_nak();
            self.duty_cycle_counter = 0;
        } else {
            self.duty_cycle_counter += 1;
        }

        work_count += bytes_sent;
        Ok(work_count)
    }
}