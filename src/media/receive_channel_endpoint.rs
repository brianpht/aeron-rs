
// Receive channel endpoint - receives data, sends control messages (SM, NAK, RTTM).

use libc;

use crate::frame::*;
use super::channel::UdpChannel;
use super::poller::{PollError, TransportPoller};
use super::transport::UdpChannelTransport;

// ── Pre-sized capacity for pending control messages ──
const MAX_PENDING_SM: usize = 64;
const MAX_PENDING_NAK: usize = 64;

/// Pending status message to send.
#[derive(Clone, Copy)]
pub struct PendingSm {
    pub dest_addr: libc::sockaddr_storage,
    pub session_id: i32,
    pub stream_id: i32,
    pub consumption_term_id: i32,
    pub consumption_term_offset: i32,
    pub receiver_window: i32,
    pub receiver_id: i64,
}

/// Pending NAK to send.
#[derive(Clone, Copy)]
pub struct PendingNak {
    pub dest_addr: libc::sockaddr_storage,
    pub session_id: i32,
    pub stream_id: i32,
    pub active_term_id: i32,
    pub term_offset: i32,
    pub length: i32,
}

/// Callback for data frames received on this endpoint.
pub trait DataFrameHandler {
    fn on_data(
        &mut self,
        data_header: &DataHeader,
        payload: &[u8],
        source: &libc::sockaddr_storage,
    );

    fn on_setup(
        &mut self,
        setup: &SetupHeader,
        source: &libc::sockaddr_storage,
    );
}

/// A receive channel endpoint manages a UDP transport for receiving
/// data frames and sending control messages back (SM, NAK, RTTM).
pub struct ReceiveChannelEndpoint {
    pub channel: UdpChannel,
    pub transport: UdpChannelTransport,
    pub transport_idx: Option<usize>,
    /// Pre-sized flat array for pending SMs - no allocation in steady state.
    pending_sms: [PendingSm; MAX_PENDING_SM],
    pending_sms_len: usize,
    /// Pre-sized flat array for pending NAKs - no allocation in steady state.
    pending_naks: [PendingNak; MAX_PENDING_NAK],
    pending_naks_len: usize,
    /// Scratch buffer for SM frames.
    sm_buf: [u8; SM_TOTAL_LENGTH],
    /// Scratch buffer for NAK frames.
    nak_buf: [u8; NAK_TOTAL_LENGTH],
}

impl ReceiveChannelEndpoint {
    pub fn new(channel: UdpChannel, transport: UdpChannelTransport) -> Self {
        Self {
            channel,
            transport,
            transport_idx: None,
            pending_sms: [unsafe { std::mem::zeroed() }; MAX_PENDING_SM],
            pending_sms_len: 0,
            pending_naks: [unsafe { std::mem::zeroed() }; MAX_PENDING_NAK],
            pending_naks_len: 0,
            sm_buf: [0u8; SM_TOTAL_LENGTH],
            nak_buf: [0u8; NAK_TOTAL_LENGTH],
        }
    }

    /// Register with the poller (monomorphized - no vtable dispatch).
    pub fn register<P: TransportPoller>(&mut self, poller: &mut P) -> std::io::Result<()> {
        let idx = poller.add_transport(&mut self.transport)?;
        self.transport_idx = Some(idx);
        Ok(())
    }

    /// Dispatch an incoming message. Returns true if handled.
    pub fn on_message(
        &self,
        data: &[u8],
        source: &libc::sockaddr_storage,
        handler: &mut impl DataFrameHandler,
    ) -> bool {
        let Some(hdr) = FrameHeader::parse(data) else {
            return false;
        };

        match FrameType::from(hdr.frame_type) {
            FrameType::Data | FrameType::Pad => {
                if let Some(dh) = DataHeader::parse(data) {
                    let payload = dh.payload(data);
                    handler.on_data(dh, payload, source);
                    return true;
                }
            }
            FrameType::Setup => {
                if let Some(setup) = SetupHeader::parse(data) {
                    handler.on_setup(setup, source);
                    return true;
                }
            }
            FrameType::Rttm => {
                // RTTM processing (phase 3).
                return true;
            }
            _ => {}
        }

        false
    }

    /// Queue a status message to be sent (zero-allocation, drops on overflow).
    pub fn queue_sm(&mut self, sm: PendingSm) {
        if self.pending_sms_len < MAX_PENDING_SM {
            self.pending_sms[self.pending_sms_len] = sm;
            self.pending_sms_len += 1;
        }
    }

    /// Queue a NAK to be sent (zero-allocation, drops on overflow).
    pub fn queue_nak(&mut self, nak: PendingNak) {
        if self.pending_naks_len < MAX_PENDING_NAK {
            self.pending_naks[self.pending_naks_len] = nak;
            self.pending_naks_len += 1;
        }
    }

    /// Send all pending control messages via the poller (monomorphized).
    pub fn send_pending<P: TransportPoller>(
        &mut self,
        poller: &mut P,
    ) -> Result<u32, PollError> {
        let idx = self
            .transport_idx
            .ok_or(PollError::NotRegistered)?;
        let mut count = 0u32;

        // Send SMs.
        for i in 0..self.pending_sms_len {
            let pending = self.pending_sms[i];
            let sm = StatusMessage {
                frame_header: FrameHeader {
                    frame_length: SM_TOTAL_LENGTH as i32,
                    version: CURRENT_VERSION,
                    flags: 0,
                    frame_type: FRAME_TYPE_SM,
                },
                session_id: pending.session_id,
                stream_id: pending.stream_id,
                consumption_term_id: pending.consumption_term_id,
                consumption_term_offset: pending.consumption_term_offset,
                receiver_window: pending.receiver_window,
                receiver_id: pending.receiver_id,
            };
            sm.write(&mut self.sm_buf);
            poller.submit_send(idx, &self.sm_buf, Some(&pending.dest_addr))?;
            count += 1;
        }
        self.pending_sms_len = 0;

        // Send NAKs.
        for i in 0..self.pending_naks_len {
            let pending = self.pending_naks[i];
            let nak = NakHeader {
                frame_header: FrameHeader {
                    frame_length: NAK_TOTAL_LENGTH as i32,
                    version: CURRENT_VERSION,
                    flags: 0,
                    frame_type: FRAME_TYPE_NAK,
                },
                session_id: pending.session_id,
                stream_id: pending.stream_id,
                active_term_id: pending.active_term_id,
                term_offset: pending.term_offset,
                length: pending.length,
            };
            nak.write(&mut self.nak_buf);
            poller.submit_send(idx, &self.nak_buf, Some(&pending.dest_addr))?;
            count += 1;
        }
        self.pending_naks_len = 0;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::DriverContext;
    use std::mem;
    use std::net::SocketAddr;

    /// Build a minimal ReceiveChannelEndpoint with a real (but throwaway) transport.
    fn dummy_endpoint() -> ReceiveChannelEndpoint {
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let ctx = DriverContext::default();
        let local: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote = channel.remote_data;
        let transport = UdpChannelTransport::open(&channel, &local, &remote, &ctx).unwrap();
        ReceiveChannelEndpoint::new(channel, transport)
    }

    fn make_pending_sm(session_id: i32, stream_id: i32) -> PendingSm {
        PendingSm {
            dest_addr: unsafe { mem::zeroed() },
            session_id,
            stream_id,
            consumption_term_id: 0,
            consumption_term_offset: 0,
            receiver_window: 65536,
            receiver_id: 1,
        }
    }

    fn make_pending_nak(session_id: i32, stream_id: i32) -> PendingNak {
        PendingNak {
            dest_addr: unsafe { mem::zeroed() },
            session_id,
            stream_id,
            active_term_id: 0,
            term_offset: 0,
            length: 1408,
        }
    }

    #[test]
    fn queue_sm_adds_to_pending() {
        let mut ep = dummy_endpoint();
        ep.queue_sm(make_pending_sm(42, 7));
        assert_eq!(ep.pending_sms_len, 1);
        assert_eq!(ep.pending_sms[0].session_id, 42);
    }

    #[test]
    fn queue_sm_overflow_drops() {
        let mut ep = dummy_endpoint();
        for i in 0..(MAX_PENDING_SM + 5) {
            ep.queue_sm(make_pending_sm(i as i32, 1));
        }
        assert_eq!(ep.pending_sms_len, MAX_PENDING_SM);
    }

    #[test]
    fn queue_nak_adds_to_pending() {
        let mut ep = dummy_endpoint();
        ep.queue_nak(make_pending_nak(99, 3));
        assert_eq!(ep.pending_naks_len, 1);
        assert_eq!(ep.pending_naks[0].session_id, 99);
    }

    #[test]
    fn queue_nak_overflow_drops() {
        let mut ep = dummy_endpoint();
        for i in 0..(MAX_PENDING_NAK + 5) {
            ep.queue_nak(make_pending_nak(i as i32, 1));
        }
        assert_eq!(ep.pending_naks_len, MAX_PENDING_NAK);
    }

    /// Stub handler that records calls.
    struct StubHandler {
        data_count: usize,
        setup_count: usize,
        last_session_id: i32,
    }

    impl StubHandler {
        fn new() -> Self {
            Self { data_count: 0, setup_count: 0, last_session_id: 0 }
        }
    }

    impl DataFrameHandler for StubHandler {
        fn on_data(
            &mut self,
            data_header: &DataHeader,
            _payload: &[u8],
            _source: &libc::sockaddr_storage,
        ) {
            self.data_count += 1;
            self.last_session_id = data_header.session_id;
        }

        fn on_setup(
            &mut self,
            setup: &SetupHeader,
            _source: &libc::sockaddr_storage,
        ) {
            self.setup_count += 1;
            self.last_session_id = setup.session_id;
        }
    }

    #[test]
    fn on_message_dispatches_data_frame() {
        let ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        let data = DataHeader {
            frame_header: FrameHeader {
                frame_length: DATA_HEADER_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 0,
            session_id: 42,
            stream_id: 7,
            term_id: 0,
            reserved_value: 0,
        };
        let mut buf = [0u8; DATA_HEADER_LENGTH];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data as *const DataHeader as *const u8,
                buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }

        let mut handler = StubHandler::new();
        let handled = ep.on_message(&buf, &source, &mut handler);
        assert!(handled);
        assert_eq!(handler.data_count, 1);
        assert_eq!(handler.last_session_id, 42);
    }

    #[test]
    fn on_message_dispatches_setup_frame() {
        let ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        let setup = SetupHeader {
            frame_header: FrameHeader {
                frame_length: SETUP_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SETUP,
            },
            term_offset: 0,
            session_id: 99,
            stream_id: 5,
            initial_term_id: 0,
            active_term_id: 0,
            term_length: 1 << 16,
            mtu: 1408,
            ttl: 0,
        };
        let mut buf = [0u8; SETUP_TOTAL_LENGTH];
        setup.write(&mut buf);

        let mut handler = StubHandler::new();
        let handled = ep.on_message(&buf, &source, &mut handler);
        assert!(handled);
        assert_eq!(handler.setup_count, 1);
        assert_eq!(handler.last_session_id, 99);
    }

    #[test]
    fn on_message_short_buffer_returns_false() {
        let ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut handler = StubHandler::new();
        let handled = ep.on_message(&[0u8; 4], &source, &mut handler);
        assert!(!handled);
        assert_eq!(handler.data_count, 0);
    }
}
