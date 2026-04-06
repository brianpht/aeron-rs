// The Sender Agent - owns the send-side io_uring poller and all
// SendChannelEndpoints. Mirrors aeron_driver_sender_t.

use super::{Agent, AgentError};
use crate::clock::CachedNanoClock;

use crate::context::DriverContext;
use crate::media::concurrent_publication::{
    ConcurrentPublication, SenderPublication, new_concurrent,
};
use crate::media::network_publication::NetworkPublication;
use crate::media::poller::{RecvMessage, TransportPoller};
use crate::media::send_channel_endpoint::SendChannelEndpoint;
use crate::media::uring_poller::UringTransportPoller;

/// Represents one active network publication within the sender.
/// Enum dispatch (no dyn - per coding rules) to support both local
/// (single-threaded) and concurrent (cross-thread) publication modes.
enum PublicationEntry {
    /// Single-threaded publication - offer and scan on same thread.
    Local {
        publication: NetworkPublication,
        endpoint_idx: usize,
        dest_addr: Option<libc::sockaddr_storage>,
        needs_setup: bool,
        time_of_last_send_ns: i64,
        time_of_last_setup_ns: i64,
    },
    /// Cross-thread publication - sender side only (publisher handle given
    /// to the application thread).
    Concurrent {
        sender_pub: SenderPublication,
        endpoint_idx: usize,
        dest_addr: Option<libc::sockaddr_storage>,
        needs_setup: bool,
        time_of_last_send_ns: i64,
        time_of_last_setup_ns: i64,
    },
}

impl PublicationEntry {
    #[inline]
    fn endpoint_idx(&self) -> usize {
        match self {
            PublicationEntry::Local { endpoint_idx, .. } => *endpoint_idx,
            PublicationEntry::Concurrent { endpoint_idx, .. } => *endpoint_idx,
        }
    }

    #[inline]
    fn session_id(&self) -> i32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.session_id(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.session_id(),
        }
    }

    #[inline]
    fn stream_id(&self) -> i32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.stream_id(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.stream_id(),
        }
    }

    #[inline]
    fn initial_term_id(&self) -> i32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.initial_term_id(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.initial_term_id(),
        }
    }

    #[inline]
    fn active_term_id(&self) -> i32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.active_term_id(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.active_term_id_approx(),
        }
    }

    #[inline]
    fn term_offset(&self) -> u32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.term_offset(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.term_offset_approx(),
        }
    }

    #[inline]
    fn term_length(&self) -> u32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.term_length(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.term_length(),
        }
    }

    #[inline]
    fn mtu(&self) -> u32 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.mtu(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.mtu(),
        }
    }

    #[inline]
    fn needs_setup(&self) -> bool {
        match self {
            PublicationEntry::Local { needs_setup, .. } => *needs_setup,
            PublicationEntry::Concurrent { needs_setup, .. } => *needs_setup,
        }
    }

    #[inline]
    fn set_needs_setup(&mut self, val: bool) {
        match self {
            PublicationEntry::Local { needs_setup, .. } => *needs_setup = val,
            PublicationEntry::Concurrent { needs_setup, .. } => *needs_setup = val,
        }
    }

    #[inline]
    fn time_of_last_send_ns(&self) -> i64 {
        match self {
            PublicationEntry::Local { time_of_last_send_ns, .. } => *time_of_last_send_ns,
            PublicationEntry::Concurrent { time_of_last_send_ns, .. } => *time_of_last_send_ns,
        }
    }

    #[inline]
    fn set_time_of_last_send_ns(&mut self, val: i64) {
        match self {
            PublicationEntry::Local { time_of_last_send_ns, .. } => *time_of_last_send_ns = val,
            PublicationEntry::Concurrent { time_of_last_send_ns, .. } => *time_of_last_send_ns = val,
        }
    }

    #[inline]
    fn time_of_last_setup_ns(&self) -> i64 {
        match self {
            PublicationEntry::Local { time_of_last_setup_ns, .. } => *time_of_last_setup_ns,
            PublicationEntry::Concurrent { time_of_last_setup_ns, .. } => *time_of_last_setup_ns,
        }
    }

    #[inline]
    fn set_time_of_last_setup_ns(&mut self, val: i64) {
        match self {
            PublicationEntry::Local { time_of_last_setup_ns, .. } => *time_of_last_setup_ns = val,
            PublicationEntry::Concurrent { time_of_last_setup_ns, .. } => *time_of_last_setup_ns = val,
        }
    }


    /// Perform sender_scan via enum dispatch (no dyn - monomorphized paths).
    #[inline]
    fn sender_scan<F>(&mut self, limit: u32, emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        match self {
            PublicationEntry::Local { publication, .. } => publication.sender_scan(limit, emit),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.sender_scan(limit, emit),
        }
    }
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
    ///
    /// Returns `Some(pub_idx)` on success, `None` if `NetworkPublication::new`
    /// fails (e.g. invalid term_length or mtu).
    pub fn add_publication(
        &mut self,
        endpoint_idx: usize,
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> Option<usize> {
        let publication =
            NetworkPublication::new(session_id, stream_id, initial_term_id, term_length, mtu)?;
        let idx = self.publications.len();
        self.publications.push(PublicationEntry::Local {
            publication,
            endpoint_idx,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
        });
        Some(idx)
    }

    /// Add a concurrent (cross-thread) publication.
    ///
    /// Creates a `ConcurrentPublication` + `SenderPublication` pair.
    /// Stores the `SenderPublication` internally for sender_scan.
    /// Returns the `ConcurrentPublication` handle to be moved to the
    /// application thread for calling `offer()`.
    ///
    /// Returns `None` if parameters are invalid.
    pub fn add_concurrent_publication(
        &mut self,
        endpoint_idx: usize,
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        term_length: u32,
        mtu: u32,
    ) -> Option<ConcurrentPublication> {
        let (pub_handle, sender_view) =
            new_concurrent(session_id, stream_id, initial_term_id, term_length, mtu)?;
        self.publications.push(PublicationEntry::Concurrent {
            sender_pub: sender_view,
            endpoint_idx,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
        });
        Some(pub_handle)
    }

    /// Get mutable access to a local publication for calling `offer()`.
    ///
    /// Returns `None` if the index is out of bounds or points to a
    /// concurrent publication (use the `ConcurrentPublication` handle instead).
    pub fn publication_mut(&mut self, idx: usize) -> Option<&mut NetworkPublication> {
        match self.publications.get_mut(idx)? {
            PublicationEntry::Local { publication, .. } => Some(publication),
            PublicationEntry::Concurrent { .. } => None,
        }
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

        let mut total_bytes = 0i32;

        // Destructure self into disjoint field borrows. Rust allows
        // simultaneous &mut to different struct fields when accessed directly
        // (not through &mut self). This lets us hold &mut publication +
        // &endpoints + &mut poller simultaneously in the scan+send closure.
        let publications = &mut self.publications;
        let endpoints = &mut self.endpoints;
        let poller = &mut self.poller;
        let heartbeat_interval_ns = self.heartbeat_interval_ns;

        let mut idx = start;
        for _ in 0..len {
            // Phase 1: Copy scalars from publication entry via enum dispatch.
            // All values are Copy - no allocation.
            let ep_idx = publications[idx].endpoint_idx();
            let session_id = publications[idx].session_id();
            let stream_id = publications[idx].stream_id();
            let initial_term_id = publications[idx].initial_term_id();
            let active_term_id = publications[idx].active_term_id();
            let term_offset = publications[idx].term_offset();
            let term_length = publications[idx].term_length();
            let mtu_val = publications[idx].mtu();
            let needs_setup = publications[idx].needs_setup();
            let last_setup_ns = publications[idx].time_of_last_setup_ns();
            let last_send_ns = publications[idx].time_of_last_send_ns();
            let dest_addr_copy: Option<libc::sockaddr_storage> = match &publications[idx] {
                PublicationEntry::Local { dest_addr, .. } => *dest_addr,
                PublicationEntry::Concurrent { dest_addr, .. } => *dest_addr,
            };

            // Phase 2: Send setup if needed (needs &mut endpoint + &mut poller).
            if needs_setup
                && (now_ns.wrapping_sub(last_setup_ns) > heartbeat_interval_ns)
            {
                let ep = &mut endpoints[ep_idx];
                let _ = ep.send_setup(
                    poller,
                    session_id,
                    stream_id,
                    initial_term_id,
                    active_term_id,
                    term_offset as i32,
                    term_length as i32,
                    mtu_val as i32,
                    0, // ttl
                    dest_addr_copy.as_ref(),
                );
                publications[idx].set_time_of_last_setup_ns(now_ns);
            }

            // Send heartbeat if idle (needs &mut endpoint + &mut poller).
            if now_ns.wrapping_sub(last_send_ns) > heartbeat_interval_ns {
                let ep = &mut endpoints[ep_idx];
                let _ = ep.send_heartbeat(
                    poller,
                    session_id,
                    stream_id,
                    active_term_id,
                    term_offset as i32,
                    dest_addr_copy.as_ref(),
                );
                publications[idx].set_time_of_last_send_ns(now_ns);
            }

            // Phase 3: Scan committed frames and send via endpoint.
            // Uses enum dispatch for sender_scan (no dyn).
            {
                let dest = dest_addr_copy.as_ref();
                let pub_entry = &mut publications[idx];
                let ep = &endpoints[ep_idx];
                let limit = pub_entry.mtu();

                let scanned = pub_entry.sender_scan(limit, |_off, data| {
                    let _ = ep.send_data(poller, data, dest);
                });

                if scanned > 0 {
                    pub_entry.set_time_of_last_send_ns(now_ns);
                }
                total_bytes = total_bytes.wrapping_add(scanned as i32);
            }

            // Advance round-robin index (branch-based wrap, no modulo).
            idx += 1;
            if idx >= len {
                idx = 0;
            }
        }

        let _ = poller.flush();
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
                    if pub_entry.session_id() == sm.session_id
                        && pub_entry.stream_id() == sm.stream_id
                    {
                        pub_entry.set_needs_setup(false);
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