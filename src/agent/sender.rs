// The Sender Agent - owns the send-side io_uring poller and all
// SendChannelEndpoints. Mirrors aeron_driver_sender_t.

use std::sync::Arc;

use super::{Agent, AgentError};
use crate::client::bridge::PublicationBridge;
use crate::clock::CachedNanoClock;

use crate::context::DriverContext;
use crate::media::concurrent_publication::{
    ConcurrentPublication, SenderPublication, new_concurrent,
};
use crate::media::network_publication::NetworkPublication;
use crate::media::poller::{RecvMessage, TransportPoller};
use crate::media::retransmit_handler::RetransmitHandler;
use crate::media::send_channel_endpoint::SendChannelEndpoint;
use crate::media::term_buffer::{PARTITION_COUNT, RawLog};
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
        /// Flow control: max absolute position the sender is permitted to
        /// send up to. Derived from SM: consumption_position + receiver_window.
        /// Initialized to term_length (initial window). Only advances forward.
        sender_limit: i64,
        /// Timestamp of last RTTM request sent.
        time_of_last_rttm_ns: i64,
        /// Smoothed RTT in nanoseconds (SRTT). Updated from RTTM replies.
        last_rtt_ns: i64,
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
        /// Flow control: max absolute position the sender is permitted to
        /// send up to. Derived from SM: consumption_position + receiver_window.
        /// Initialized to term_length (initial window). Only advances forward.
        sender_limit: i64,
        /// Timestamp of last RTTM request sent.
        time_of_last_rttm_ns: i64,
        /// Smoothed RTT in nanoseconds (SRTT). Updated from RTTM replies.
        last_rtt_ns: i64,
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
            PublicationEntry::Local {
                time_of_last_send_ns,
                ..
            } => *time_of_last_send_ns,
            PublicationEntry::Concurrent {
                time_of_last_send_ns,
                ..
            } => *time_of_last_send_ns,
        }
    }

    #[inline]
    fn set_time_of_last_send_ns(&mut self, val: i64) {
        match self {
            PublicationEntry::Local {
                time_of_last_send_ns,
                ..
            } => *time_of_last_send_ns = val,
            PublicationEntry::Concurrent {
                time_of_last_send_ns,
                ..
            } => *time_of_last_send_ns = val,
        }
    }

    #[inline]
    fn time_of_last_setup_ns(&self) -> i64 {
        match self {
            PublicationEntry::Local {
                time_of_last_setup_ns,
                ..
            } => *time_of_last_setup_ns,
            PublicationEntry::Concurrent {
                time_of_last_setup_ns,
                ..
            } => *time_of_last_setup_ns,
        }
    }

    #[inline]
    fn set_time_of_last_setup_ns(&mut self, val: i64) {
        match self {
            PublicationEntry::Local {
                time_of_last_setup_ns,
                ..
            } => *time_of_last_setup_ns = val,
            PublicationEntry::Concurrent {
                time_of_last_setup_ns,
                ..
            } => *time_of_last_setup_ns = val,
        }
    }

    /// Current sender_limit (flow control position ceiling).
    #[inline]
    fn sender_limit(&self) -> i64 {
        match self {
            PublicationEntry::Local { sender_limit, .. } => *sender_limit,
            PublicationEntry::Concurrent { sender_limit, .. } => *sender_limit,
        }
    }

    /// Update sender_limit from a Status Message.
    ///
    /// Computes: new_limit = consumption_position + receiver_window.
    /// Only advances forward (never retracts) using wrapping_sub half-range.
    /// Zero-allocation, O(1). Called from process_sm_and_nak.
    #[inline]
    fn update_sender_limit_from_sm(
        &mut self,
        consumption_term_id: i32,
        consumption_term_offset: i32,
        receiver_window: i32,
    ) {
        let consumption_position =
            self.compute_position(consumption_term_id, consumption_term_offset as u32);
        let proposed = consumption_position.wrapping_add(receiver_window as i64);
        let current = match self {
            PublicationEntry::Local { sender_limit, .. } => sender_limit,
            PublicationEntry::Concurrent { sender_limit, .. } => sender_limit,
        };
        // Only advance: half-range wrapping check for i64.
        let diff = proposed.wrapping_sub(*current);
        if diff > 0 && diff < (i64::MAX >> 1) {
            *current = proposed;
        }
    }

    #[inline]
    fn time_of_last_rttm_ns(&self) -> i64 {
        match self {
            PublicationEntry::Local {
                time_of_last_rttm_ns,
                ..
            } => *time_of_last_rttm_ns,
            PublicationEntry::Concurrent {
                time_of_last_rttm_ns,
                ..
            } => *time_of_last_rttm_ns,
        }
    }

    #[inline]
    fn set_time_of_last_rttm_ns(&mut self, val: i64) {
        match self {
            PublicationEntry::Local {
                time_of_last_rttm_ns,
                ..
            } => *time_of_last_rttm_ns = val,
            PublicationEntry::Concurrent {
                time_of_last_rttm_ns,
                ..
            } => *time_of_last_rttm_ns = val,
        }
    }

    #[inline]
    fn last_rtt_ns(&self) -> i64 {
        match self {
            PublicationEntry::Local { last_rtt_ns, .. } => *last_rtt_ns,
            PublicationEntry::Concurrent { last_rtt_ns, .. } => *last_rtt_ns,
        }
    }

    /// Update smoothed RTT from a new sample.
    ///
    /// Uses exponential moving average: srtt = srtt * 7/8 + sample * 1/8
    /// (integer shifts, no division). Only updates if sample is positive
    /// and within sane half-range.
    #[inline]
    fn update_rtt(&mut self, sample_ns: i64) {
        if sample_ns <= 0 || sample_ns >= (i64::MAX >> 1) {
            return;
        }
        let current = match self {
            PublicationEntry::Local { last_rtt_ns, .. } => last_rtt_ns,
            PublicationEntry::Concurrent { last_rtt_ns, .. } => last_rtt_ns,
        };
        if *current == 0 {
            // First sample - use directly.
            *current = sample_ns;
        } else {
            // SRTT = SRTT * 7/8 + sample * 1/8 (shift-based, no division).
            *current = (*current - (*current >> 3)) + (sample_ns >> 3);
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

    /// Read frames from term buffer for retransmit (enum dispatch, no dyn).
    /// Takes &self - does not advance sender_position.
    #[inline]
    fn retransmit_scan<F>(&self, term_id: i32, offset: u32, limit: u32, emit: F) -> u32
    where
        F: FnMut(u32, &[u8]),
    {
        let initial = self.initial_term_id();
        let part_idx = RawLog::partition_index(term_id, initial);
        match self {
            PublicationEntry::Local { publication, .. } => publication
                .raw_log()
                .scan_frames(part_idx, offset, limit, emit),
            PublicationEntry::Concurrent { sender_pub, .. } => {
                sender_pub.scan_term_at(part_idx, offset, limit, emit)
            }
        }
    }

    /// Compute absolute position from term_id and term_offset.
    #[inline]
    fn compute_position(&self, term_id: i32, term_offset: u32) -> i64 {
        match self {
            PublicationEntry::Local { publication, .. } => {
                publication.compute_position(term_id, term_offset)
            }
            PublicationEntry::Concurrent { sender_pub, .. } => {
                sender_pub.compute_position(term_id, term_offset)
            }
        }
    }

    /// Sender position (highest scanned byte).
    #[inline]
    fn sender_position(&self) -> i64 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.sender_position(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.sender_position(),
        }
    }

    /// Pub position (highest published byte).
    #[inline]
    fn pub_position(&self) -> i64 {
        match self {
            PublicationEntry::Local { publication, .. } => publication.pub_position(),
            PublicationEntry::Concurrent { sender_pub, .. } => sender_pub.pub_position(),
        }
    }

    /// Destination address for this publication.
    #[inline]
    fn dest_addr(&self) -> Option<&libc::sockaddr_storage> {
        match self {
            PublicationEntry::Local { dest_addr, .. } => dest_addr.as_ref(),
            PublicationEntry::Concurrent { dest_addr, .. } => dest_addr.as_ref(),
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
    retransmit_handler: RetransmitHandler,
    clock: CachedNanoClock,
    duty_cycle_ratio: usize,
    duty_cycle_counter: usize,
    round_robin_index: usize,
    heartbeat_interval_ns: i64,
    rttm_interval_ns: i64,
    /// Publication bridge for receiving concurrent publications from the
    /// Aeron client. None when no client is connected.
    /// Scanned per duty cycle: O(BRIDGE_CAPACITY) Acquire loads when empty.
    pub_bridge: Option<Arc<PublicationBridge>>,
}

// SAFETY: SenderAgent exclusively owns all its fields including the
// UringTransportPoller (which contains raw pointers into kernel-mapped
// memory). The agent is single-threaded by design - it is created on one
// thread and moved (not shared) to a dedicated agent thread via
// AgentRunner::start(). No concurrent access occurs.
unsafe impl Send for SenderAgent {}

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
            retransmit_handler: RetransmitHandler::new(
                ctx.retransmit_unicast_delay_ns,
                ctx.retransmit_unicast_linger_ns,
            ),
            clock: CachedNanoClock::new(),
            duty_cycle_ratio: ctx.send_duty_cycle_ratio,
            duty_cycle_counter: 0,
            round_robin_index: 0,
            heartbeat_interval_ns: ctx.heartbeat_interval_ns,
            rttm_interval_ns: ctx.rttm_interval_ns,
            pub_bridge: None,
        })
    }

    /// Set the publication bridge for receiving concurrent publications
    /// from the Aeron client. Call before starting the agent.
    pub(crate) fn set_publication_bridge(&mut self, bridge: Arc<PublicationBridge>) {
        self.pub_bridge = Some(bridge);
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
            // Initial window: allow one full term before first SM arrives.
            // Matches Aeron C max_flow_control on_setup: position + initial_window_length.
            sender_limit: term_length as i64,
            time_of_last_rttm_ns: 0,
            last_rtt_ns: 0,
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
            // Initial window: allow one full term before first SM arrives.
            sender_limit: term_length as i64,
            time_of_last_rttm_ns: 0,
            last_rtt_ns: 0,
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

    /// Current sender_limit (flow control ceiling) for the given publication.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn publication_sender_limit(&self, idx: usize) -> Option<i64> {
        self.publications.get(idx).map(|p| p.sender_limit())
    }

    /// Whether the publication still needs a Setup/SM handshake to complete.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn publication_needs_setup(&self, idx: usize) -> Option<bool> {
        self.publications.get(idx).map(|p| p.needs_setup())
    }

    /// Smoothed RTT (SRTT) in nanoseconds for the given publication.
    /// Returns 0 if no RTTM reply has been processed yet.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn publication_last_rtt_ns(&self, idx: usize) -> Option<i64> {
        self.publications.get(idx).map(|p| p.last_rtt_ns())
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
        let rttm_interval_ns = self.rttm_interval_ns;

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
            let last_rttm_ns = publications[idx].time_of_last_rttm_ns();
            let dest_addr_copy: Option<libc::sockaddr_storage> = match &publications[idx] {
                PublicationEntry::Local { dest_addr, .. } => *dest_addr,
                PublicationEntry::Concurrent { dest_addr, .. } => *dest_addr,
            };

            // Phase 2: Send setup if needed (needs &mut endpoint + &mut poller).
            if needs_setup && (now_ns.wrapping_sub(last_setup_ns) > heartbeat_interval_ns) {
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

            // Send periodic RTTM request if interval has elapsed.
            if now_ns.wrapping_sub(last_rttm_ns) > rttm_interval_ns {
                let ep = &mut endpoints[ep_idx];
                let _ = ep.send_rttm(
                    poller,
                    session_id,
                    stream_id,
                    now_ns,
                    dest_addr_copy.as_ref(),
                );
                publications[idx].set_time_of_last_rttm_ns(now_ns);
            }

            // Phase 3: Scan committed frames and send via endpoint.
            // Uses enum dispatch for sender_scan (no dyn).
            // Flow control: clamp scan to available receiver window AND
            // available send slots (prevents silent frame drops).
            {
                let dest = dest_addr_copy.as_ref();
                let pub_entry = &mut publications[idx];
                let ep = &endpoints[ep_idx];

                let sender_lim = pub_entry.sender_limit();
                let sender_pos = pub_entry.sender_position();
                let pub_pos = pub_entry.pub_position();

                // Scan limit = min(flow-control window, committed data).
                //
                // sender_limit gates how far *ahead* the receiver allows.
                // pub_position gates how far *data* the publisher has written.
                // Both use wrapping half-range comparison against sender_pos.
                // Without the pub_position clamp, sender_scan reads stale
                // frame_length fields from prior term cycles, advancing
                // sender_position past the actual published data.
                let window_avail = sender_lim.wrapping_sub(sender_pos);
                let data_avail = pub_pos.wrapping_sub(sender_pos);

                let available = if window_avail <= 0 || data_avail <= 0 {
                    0
                } else {
                    window_avail.min(data_avail)
                };

                // Use full available window (not capped at MTU) so the
                // sender can scan multiple frames per do_work() cycle.
                let mut limit = if available > u32::MAX as i64 {
                    u32::MAX
                } else {
                    available as u32
                };

                // Clamp by send slot availability: each scanned frame
                // consumes one send slot. send_available * mtu is an upper
                // bound on bytes we can transmit. This prevents sender_scan
                // from advancing sender_position past frames that cannot be
                // sent (silent data loss).
                let send_slots = poller.send_available();
                let slot_limit = (send_slots as u64).saturating_mul(mtu_val as u64);
                if slot_limit < limit as u64 {
                    limit = slot_limit as u32;
                }

                if limit > 0 {
                    let scanned = pub_entry.sender_scan(limit, |_off, data| {
                        // Skip pad frames - they are scan-advance markers
                        // only, not transmitted over the wire. Check
                        // frame_type at bytes [6..8] (little-endian u16).
                        if data.len() >= 8 {
                            let ft = u16::from_le_bytes([data[6], data[7]]);
                            if ft == crate::frame::FRAME_TYPE_PAD {
                                return;
                            }
                        }
                        let _ = ep.send_data(poller, data, dest);
                    });

                    if scanned > 0 {
                        pub_entry.set_time_of_last_send_ns(now_ns);
                    }
                    total_bytes = total_bytes.wrapping_add(scanned as i32);

                    #[cfg(debug_assertions)]
                    if scanned > 0 {
                        tracing::debug!(
                            session_id,
                            stream_id,
                            sender_limit = sender_lim,
                            sender_position = pub_entry.sender_position(),
                            pub_position = pub_entry.pub_position(),
                            scanned,
                            send_slots,
                            "sender scan"
                        );
                    }
                }
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

    fn process_sm_and_nak(&mut self, now_ns: i64) {
        // Destructure self for disjoint field borrows.
        let publications = &mut self.publications;
        let endpoints = &mut self.endpoints;
        let retransmit_handler = &mut self.retransmit_handler;

        for ep in endpoints.iter_mut() {
            ep.drain_sm(|sm| {
                // Copy fields from packed struct to avoid unaligned refs.
                let sm_session_id = sm.session_id;
                let sm_stream_id = sm.stream_id;
                let consumption_term_id = sm.consumption_term_id;
                let consumption_term_offset = sm.consumption_term_offset;
                let receiver_window = sm.receiver_window;

                for pub_entry in publications.iter_mut() {
                    if pub_entry.session_id() == sm_session_id
                        && pub_entry.stream_id() == sm_stream_id
                    {
                        pub_entry.set_needs_setup(false);
                        pub_entry.update_sender_limit_from_sm(
                            consumption_term_id,
                            consumption_term_offset,
                            receiver_window,
                        );
                    }
                }
            });

            ep.drain_naks(|nak| {
                retransmit_handler.on_nak(nak, now_ns);
            });

            ep.drain_rttm_replies(|rttm| {
                // Copy fields from packed struct to avoid unaligned refs.
                let rttm_session_id = rttm.session_id;
                let rttm_stream_id = rttm.stream_id;
                let echo_timestamp = rttm.echo_timestamp;
                let reception_delta = rttm.reception_delta;

                let rtt_sample = now_ns
                    .wrapping_sub(echo_timestamp)
                    .wrapping_sub(reception_delta);

                for pub_entry in publications.iter_mut() {
                    if pub_entry.session_id() == rttm_session_id
                        && pub_entry.stream_id() == rttm_stream_id
                    {
                        pub_entry.update_rtt(rtt_sample);
                        tracing::trace!(
                            session_id = rttm_session_id,
                            stream_id = rttm_stream_id,
                            rtt_sample_ns = rtt_sample,
                            srtt_ns = pub_entry.last_rtt_ns(),
                            "RTTM reply processed"
                        );
                    }
                }
            });
        }
    }

    fn do_retransmit(&mut self, now_ns: i64) -> i32 {
        let publications = &self.publications;
        let endpoints = &self.endpoints;
        let poller = &mut self.poller;
        let handler = &mut self.retransmit_handler;
        let mut work_count = 0i32;

        handler.process_timeouts(now_ns, |sid, stid, term_id, term_off, len| {
            // Find matching publication (linear scan, n <= 64).
            let pub_match = publications
                .iter()
                .find(|p| p.session_id() == sid && p.stream_id() == stid);

            let Some(pub_entry) = pub_match else {
                return; // Publication removed since NAK received.
            };

            // Validate NAK range is still in the term buffer.
            let nak_position = pub_entry.compute_position(term_id, term_off as u32);
            let sender_pos = pub_entry.sender_position();
            let term_length = pub_entry.term_length() as i64;
            let buffer_start = sender_pos.wrapping_sub((PARTITION_COUNT as i64 - 1) * term_length);

            // Half-range wrapping check: nak_position must be
            // >= buffer_start and < pub_position.
            let from_start = nak_position.wrapping_sub(buffer_start);
            let pub_pos = pub_entry.pub_position();
            let range = pub_pos.wrapping_sub(buffer_start);
            if from_start < 0 || from_start >= range {
                return; // Data overwritten or not yet written.
            }

            // Read frames from term buffer and send.
            let ep_idx = pub_entry.endpoint_idx();
            let dest = pub_entry.dest_addr();
            let mtu = pub_entry.mtu();
            let limit = if (len as u32) < mtu { len as u32 } else { mtu };

            pub_entry.retransmit_scan(term_id, term_off as u32, limit, |_off, data| {
                if let Some(ep) = endpoints.get(ep_idx) {
                    let _ = ep.send_data(poller, data, dest);
                    work_count += 1;
                }
            });
        });

        if work_count > 0 {
            let _ = poller.flush();
        }
        work_count
    }

    /// Poll the publication bridge for new concurrent publications
    /// deposited by the Aeron client.
    ///
    /// For each pending publication: register the endpoint with the
    /// poller, add the SenderPublication as a Concurrent entry.
    ///
    /// Cold-path per item (socket registration, Vec push). The scan
    /// itself is O(BRIDGE_CAPACITY) Acquire loads when empty.
    fn poll_publication_bridge(&mut self) -> i32 {
        let Some(ref bridge) = self.pub_bridge else {
            return 0;
        };

        let cap = bridge.capacity();
        let mut count = 0i32;

        for i in 0..cap {
            // Scope the borrow of self.pub_bridge to the try_take call.
            // After this block, the borrow is released and we can use &mut self.
            let item = {
                let Some(ref bridge) = self.pub_bridge else {
                    break;
                };
                bridge.try_take(i)
            };

            if let Some(pending) = item {
                // Register endpoint with the poller (cold path - involves io_uring SQE).
                let mut endpoint = pending.endpoint;
                let register_result = endpoint.register(&mut self.poller);
                if register_result.is_err() {
                    tracing::warn!("failed to register endpoint from bridge");
                    continue;
                }

                let ep_idx = self.endpoints.len();
                self.endpoints.push(endpoint);

                let term_length = pending.sender_pub.term_length();

                self.publications.push(PublicationEntry::Concurrent {
                    sender_pub: pending.sender_pub,
                    endpoint_idx: ep_idx,
                    dest_addr: Some(pending.dest_addr),
                    needs_setup: true,
                    time_of_last_send_ns: 0,
                    time_of_last_setup_ns: 0,
                    sender_limit: term_length as i64,
                    time_of_last_rttm_ns: 0,
                    last_rtt_ns: 0,
                });

                count += 1;
            }
        }

        count
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
            work_count += self.poll_publication_bridge();
            self.process_sm_and_nak(now_ns);
            work_count += self.do_retransmit(now_ns);
            self.duty_cycle_counter = 0;
        } else {
            self.duty_cycle_counter += 1;
        }

        work_count += bytes_sent;
        Ok(work_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::network_publication::NetworkPublication;

    const TEST_TERM_LENGTH: u32 = 1024;
    const TEST_MTU: u32 = 1408;

    fn make_local_entry(term_length: u32) -> PublicationEntry {
        let publication =
            NetworkPublication::new(42, 7, 0, term_length, TEST_MTU).expect("valid params");
        PublicationEntry::Local {
            publication,
            endpoint_idx: 0,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
            sender_limit: term_length as i64,
            time_of_last_rttm_ns: 0,
            last_rtt_ns: 0,
        }
    }

    // ---- sender_limit initialization ----

    #[test]
    fn sender_limit_initialized_to_term_length() {
        let entry = make_local_entry(TEST_TERM_LENGTH);
        assert_eq!(entry.sender_limit(), TEST_TERM_LENGTH as i64);
    }

    // ---- update_sender_limit_from_sm ----

    #[test]
    fn update_sender_limit_advances_forward() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // Initial limit = 1024. SM says consumption at 0 + window 2048 -> 2048.
        entry.update_sender_limit_from_sm(0, 0, 2048);
        assert_eq!(entry.sender_limit(), 2048);
    }

    #[test]
    fn update_sender_limit_rejects_backward() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // Advance to 4096.
        entry.update_sender_limit_from_sm(0, 0, 4096);
        assert_eq!(entry.sender_limit(), 4096);
        // SM with lower proposed limit (512) should not regress.
        entry.update_sender_limit_from_sm(0, 0, 512);
        assert_eq!(entry.sender_limit(), 4096);
    }

    #[test]
    fn update_sender_limit_advances_incrementally() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // Simulate progressive SM with advancing consumption.
        entry.update_sender_limit_from_sm(0, 512, 1024);
        // consumption_position = 512, proposed = 512 + 1024 = 1536.
        assert_eq!(entry.sender_limit(), 1536);

        entry.update_sender_limit_from_sm(0, 1024, 1024);
        // proposed = 1024 + 1024 = 2048. Greater than 1536, advances.
        assert_eq!(entry.sender_limit(), 2048);
    }

    #[test]
    fn update_sender_limit_with_wrapping_term_id() {
        // initial_term_id = 0, consumption at term_id=2, offset=0.
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // consumption_position = 2 * 1024 = 2048, proposed = 2048 + 4096 = 6144.
        entry.update_sender_limit_from_sm(2, 0, 4096);
        assert_eq!(entry.sender_limit(), 6144);
    }

    #[test]
    fn update_sender_limit_equal_is_noop() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // Advance to 2048.
        entry.update_sender_limit_from_sm(0, 0, 2048);
        assert_eq!(entry.sender_limit(), 2048);
        // Same value -> no change (diff = 0, not > 0).
        entry.update_sender_limit_from_sm(0, 0, 2048);
        assert_eq!(entry.sender_limit(), 2048);
    }

    // ---- flow control clamping in scan ----

    #[test]
    fn flow_control_allows_scan_within_limit() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // Offer one frame.
        match &mut entry {
            PublicationEntry::Local { publication, .. } => {
                publication.offer(&[0xAB; 4]).expect("offer");
            }
            _ => panic!("expected Local"),
        }

        // sender_limit = 1024 (term_length), sender_position = 0.
        // available = 1024, which >= mtu -> limit = mtu.
        let sender_lim = entry.sender_limit();
        let sender_pos = entry.sender_position();
        let available = sender_lim.wrapping_sub(sender_pos);
        assert!(available > 0);

        let mut count = 0u32;
        entry.sender_scan(entry.mtu(), |_, _| count += 1);
        assert_eq!(count, 1, "frame should be scanned when within sender_limit");
    }

    #[test]
    fn flow_control_blocks_scan_when_at_limit() {
        let mut entry = make_local_entry(64);
        // Offer two frames to fill the term (64 bytes = 2 x 32-byte headers).
        match &mut entry {
            PublicationEntry::Local { publication, .. } => {
                publication.offer(&[]).expect("offer 1");
                publication.offer(&[]).expect("offer 2");
            }
            _ => panic!("expected Local"),
        }

        // Scan everything to advance sender_position to 64.
        entry.sender_scan(u32::MAX, |_, _| {});
        assert_eq!(entry.sender_position(), 64);

        // sender_limit was initialized to 64 (term_length).
        // available = 64 - 64 = 0, so scan should be blocked.
        let available = entry.sender_limit().wrapping_sub(entry.sender_position());
        assert_eq!(available, 0);
    }

    #[test]
    fn flow_control_partial_window() {
        // Set sender_limit to 48 (less than one full frame = 32 header + payload).
        let publication =
            NetworkPublication::new(42, 7, 0, TEST_TERM_LENGTH, TEST_MTU).expect("valid params");
        let mut entry = PublicationEntry::Local {
            publication,
            endpoint_idx: 0,
            dest_addr: None,
            needs_setup: true,
            time_of_last_send_ns: 0,
            time_of_last_setup_ns: 0,
            sender_limit: 48, // Only 48 bytes of window.
            time_of_last_rttm_ns: 0,
            last_rtt_ns: 0,
        };

        // Offer a frame (32 bytes aligned).
        match &mut entry {
            PublicationEntry::Local { publication, .. } => {
                publication.offer(&[]).expect("offer");
            }
            _ => panic!("expected Local"),
        }

        // available = 48 - 0 = 48, which < mtu (1408).
        // Scan limit should be 48. Frame is 32 bytes, fits within 48.
        let sender_lim = entry.sender_limit();
        let sender_pos = entry.sender_position();
        let available = sender_lim.wrapping_sub(sender_pos);
        assert_eq!(available, 48);
        let limit = if available >= entry.mtu() as i64 {
            entry.mtu()
        } else {
            available as u32
        };
        assert_eq!(limit, 48);

        let mut count = 0u32;
        entry.sender_scan(limit, |_, _| count += 1);
        assert_eq!(count, 1, "frame fits within partial window");
    }

    // ---- SM processing updates sender_limit ----

    #[test]
    fn sm_fields_update_sender_limit_correctly() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // initial_term_id = 0, term_length = 1024.
        // SM: consumption_term_id=1, consumption_term_offset=512, receiver_window=2048.
        // consumption_position = 1*1024 + 512 = 1536.
        // proposed = 1536 + 2048 = 3584.
        entry.update_sender_limit_from_sm(1, 512, 2048);
        assert_eq!(entry.sender_limit(), 3584);
    }

    // ---- RTTM / RTT tracking ----

    #[test]
    fn rtt_initialized_to_zero() {
        let entry = make_local_entry(TEST_TERM_LENGTH);
        assert_eq!(entry.last_rtt_ns(), 0);
        assert_eq!(entry.time_of_last_rttm_ns(), 0);
    }

    #[test]
    fn update_rtt_first_sample_sets_directly() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        entry.update_rtt(10_000);
        assert_eq!(entry.last_rtt_ns(), 10_000);
    }

    #[test]
    fn update_rtt_subsequent_uses_ewma() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        // First sample.
        entry.update_rtt(8000);
        assert_eq!(entry.last_rtt_ns(), 8000);

        // Second sample with higher value.
        // srtt = 8000 - (8000 >> 3) + (16000 >> 3) = 8000 - 1000 + 2000 = 9000.
        entry.update_rtt(16000);
        assert_eq!(entry.last_rtt_ns(), 9000);
    }

    #[test]
    fn update_rtt_rejects_negative_sample() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        entry.update_rtt(10_000);
        // Negative sample should be rejected.
        entry.update_rtt(-5000);
        assert_eq!(entry.last_rtt_ns(), 10_000);
    }

    #[test]
    fn update_rtt_rejects_zero_sample() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        entry.update_rtt(10_000);
        entry.update_rtt(0);
        assert_eq!(entry.last_rtt_ns(), 10_000);
    }

    #[test]
    fn update_rtt_rejects_huge_sample() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        entry.update_rtt(10_000);
        // Sample beyond half-range should be rejected.
        entry.update_rtt(i64::MAX);
        assert_eq!(entry.last_rtt_ns(), 10_000);
    }

    #[test]
    fn time_of_last_rttm_ns_accessors() {
        let mut entry = make_local_entry(TEST_TERM_LENGTH);
        assert_eq!(entry.time_of_last_rttm_ns(), 0);
        entry.set_time_of_last_rttm_ns(999_999);
        assert_eq!(entry.time_of_last_rttm_ns(), 999_999);
    }
}
