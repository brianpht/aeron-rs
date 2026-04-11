use libc;

use super::channel::UdpChannel;
use super::poller::{PollError, TransportPoller};
use super::transport::UdpChannelTransport;
use crate::frame::*;

// ── Pre-sized ring buffer capacity for hot-path message storage ──
const MAX_RECENT_SM: usize = 64;
const MAX_RECENT_NAK: usize = 64;
const MAX_RECENT_RTTM: usize = 16;

/// A send channel endpoint manages a single UDP transport for sending
/// data frames and receiving control messages (SM, NAK, ERR).
///
/// One endpoint may serve multiple `NetworkPublication`s on different stream_ids.
pub struct SendChannelEndpoint {
    pub channel: UdpChannel,
    pub transport: UdpChannelTransport,
    pub transport_idx: Option<usize>,

    /// Status messages received - pre-sized flat ring, no allocation in steady state.
    recent_sm: [StatusMessageInfo; MAX_RECENT_SM],
    recent_sm_len: usize,
    /// NAKs received - pre-sized flat ring, no allocation in steady state.
    recent_naks: [NakInfo; MAX_RECENT_NAK],
    recent_naks_len: usize,
    /// RTTM replies received - pre-sized flat ring, no allocation in steady state.
    recent_rttms: [RttmInfo; MAX_RECENT_RTTM],
    recent_rttms_len: usize,

    /// Heartbeat scratch buffer.
    heartbeat_buf: [u8; DATA_HEADER_LENGTH],
    /// Setup scratch buffer.
    setup_buf: [u8; SETUP_TOTAL_LENGTH],
    /// RTTM scratch buffer.
    rttm_buf: [u8; RTTM_TOTAL_LENGTH],
}

/// Stored SM - only holds the wire-format struct (session_id / stream_id
/// are already inside `StatusMessage`, no need for redundant copies).
#[derive(Clone, Copy)]
struct StatusMessageInfo {
    sm: StatusMessage,
}

impl Default for StatusMessageInfo {
    fn default() -> Self {
        Self {
            sm: unsafe { std::mem::zeroed() },
        }
    }
}

/// Stored NAK - only holds the wire-format struct.
#[derive(Clone, Copy)]
struct NakInfo {
    nak: NakHeader,
}

impl Default for NakInfo {
    fn default() -> Self {
        Self {
            nak: unsafe { std::mem::zeroed() },
        }
    }
}

/// Stored RTTM reply - only holds the wire-format struct.
#[derive(Clone, Copy)]
struct RttmInfo {
    rttm: RttmHeader,
}

impl Default for RttmInfo {
    fn default() -> Self {
        Self {
            rttm: unsafe { std::mem::zeroed() },
        }
    }
}

impl SendChannelEndpoint {
    pub fn new(channel: UdpChannel, transport: UdpChannelTransport) -> Self {
        Self {
            channel,
            transport,
            transport_idx: None,
            recent_sm: [StatusMessageInfo::default(); MAX_RECENT_SM],
            recent_sm_len: 0,
            recent_naks: [NakInfo::default(); MAX_RECENT_NAK],
            recent_naks_len: 0,
            recent_rttms: [RttmInfo::default(); MAX_RECENT_RTTM],
            recent_rttms_len: 0,
            heartbeat_buf: [0u8; DATA_HEADER_LENGTH],
            setup_buf: [0u8; SETUP_TOTAL_LENGTH],
            rttm_buf: [0u8; RTTM_TOTAL_LENGTH],
        }
    }

    /// Register with the poller (monomorphized - no vtable dispatch).
    pub fn register<P: TransportPoller>(&mut self, poller: &mut P) -> std::io::Result<()> {
        let idx = poller.add_transport(&mut self.transport)?;
        self.transport_idx = Some(idx);
        Ok(())
    }

    /// Dispatch an incoming control message.
    /// Zero-allocation: writes into pre-sized arrays, drops oldest on overflow.
    pub fn on_message(&mut self, data: &[u8], _source: &libc::sockaddr_storage) {
        let Some(hdr) = FrameHeader::parse(data) else {
            return;
        };

        match FrameType::from(hdr.frame_type) {
            FrameType::StatusMessage => {
                if let Some(sm) = StatusMessage::parse(data) {
                    if self.recent_sm_len < MAX_RECENT_SM {
                        self.recent_sm[self.recent_sm_len] = StatusMessageInfo { sm: *sm };
                        self.recent_sm_len += 1;
                    }
                    let sid = sm.session_id;
                    let stid = sm.stream_id;
                    tracing::trace!(session_id = sid, stream_id = stid, "recv SM");
                }
            }
            FrameType::Nak => {
                if let Some(nak) = NakHeader::parse(data) {
                    if self.recent_naks_len < MAX_RECENT_NAK {
                        self.recent_naks[self.recent_naks_len] = NakInfo { nak: *nak };
                        self.recent_naks_len += 1;
                    }
                    let sid = nak.session_id;
                    let stid = nak.stream_id;
                    let toff = nak.term_offset;
                    let nlen = nak.length;
                    tracing::trace!(
                        session_id = sid,
                        stream_id = stid,
                        term_offset = toff,
                        length = nlen,
                        "recv NAK"
                    );
                }
            }
            FrameType::Rttm => {
                if let Some(rttm) = RttmHeader::parse(data) {
                    let flags = { rttm.frame_header.flags };
                    // Only store replies (RTTM_FLAG_REPLY set), ignore requests.
                    if (flags & RTTM_FLAG_REPLY) != 0 {
                        if self.recent_rttms_len < MAX_RECENT_RTTM {
                            self.recent_rttms[self.recent_rttms_len] = RttmInfo { rttm: *rttm };
                            self.recent_rttms_len += 1;
                        }
                        let sid = rttm.session_id;
                        let stid = rttm.stream_id;
                        tracing::trace!(session_id = sid, stream_id = stid, "recv RTTM reply");
                    }
                }
            }
            _ => {}
        }
    }

    /// Iterate recently received status messages and reset.
    pub fn drain_sm<F: FnMut(&StatusMessage)>(&mut self, mut f: F) {
        for i in 0..self.recent_sm_len {
            f(&self.recent_sm[i].sm);
        }
        self.recent_sm_len = 0;
    }

    /// Iterate recently received NAKs and reset.
    pub fn drain_naks<F: FnMut(&NakHeader)>(&mut self, mut f: F) {
        for i in 0..self.recent_naks_len {
            f(&self.recent_naks[i].nak);
        }
        self.recent_naks_len = 0;
    }

    /// Iterate recently received RTTM replies and reset.
    pub fn drain_rttm_replies<F: FnMut(&RttmHeader)>(&mut self, mut f: F) {
        for i in 0..self.recent_rttms_len {
            f(&self.recent_rttms[i].rttm);
        }
        self.recent_rttms_len = 0;
    }

    /// Send a data frame via the poller (monomorphized).
    pub fn send_data<P: TransportPoller>(
        &self,
        poller: &mut P,
        data: &[u8],
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let idx = self.transport_idx.ok_or(PollError::NotRegistered)?;
        poller.submit_send(idx, data, dest)
    }

    /// Send a heartbeat for the given session/stream/term (monomorphized).
    pub fn send_heartbeat<P: TransportPoller>(
        &mut self,
        poller: &mut P,
        session_id: i32,
        stream_id: i32,
        term_id: i32,
        term_offset: i32,
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let hdr = DataHeader {
            frame_header: FrameHeader {
                frame_length: DATA_HEADER_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_HEARTBEAT,
            },
            term_offset,
            session_id,
            stream_id,
            term_id,
            reserved_value: 0,
        };

        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const DataHeader as *const u8,
                self.heartbeat_buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }

        let idx = self.transport_idx.ok_or(PollError::NotRegistered)?;
        poller.submit_send(idx, &self.heartbeat_buf, dest)
    }

    /// Send a setup frame (monomorphized).
    #[allow(clippy::too_many_arguments)]
    pub fn send_setup<P: TransportPoller>(
        &mut self,
        poller: &mut P,
        session_id: i32,
        stream_id: i32,
        initial_term_id: i32,
        active_term_id: i32,
        term_offset: i32,
        term_length: i32,
        mtu: i32,
        ttl: i32,
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let setup = SetupHeader {
            frame_header: FrameHeader {
                frame_length: SETUP_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SETUP,
            },
            term_offset,
            session_id,
            stream_id,
            initial_term_id,
            active_term_id,
            term_length,
            mtu,
            ttl,
        };

        setup.write(&mut self.setup_buf);

        let idx = self.transport_idx.ok_or(PollError::NotRegistered)?;
        poller.submit_send(idx, &self.setup_buf, dest)
    }

    /// Send an RTTM request frame (monomorphized).
    pub fn send_rttm<P: TransportPoller>(
        &mut self,
        poller: &mut P,
        session_id: i32,
        stream_id: i32,
        echo_timestamp_ns: i64,
        dest: Option<&libc::sockaddr_storage>,
    ) -> Result<(), PollError> {
        let rttm = RttmHeader {
            frame_header: FrameHeader {
                frame_length: RTTM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0, // request (not reply)
                frame_type: FRAME_TYPE_RTTM,
            },
            session_id,
            stream_id,
            echo_timestamp: echo_timestamp_ns,
            reception_delta: 0,
            receiver_id: 0, // sender does not set receiver_id
        };

        rttm.write(&mut self.rttm_buf);

        let idx = self.transport_idx.ok_or(PollError::NotRegistered)?;
        poller.submit_send(idx, &self.rttm_buf, dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::DriverContext;
    use crate::media::channel::UdpChannel;
    use std::mem;
    use std::net::SocketAddr;

    /// Build a minimal SendChannelEndpoint with a real (but throwaway) transport.
    fn dummy_endpoint() -> SendChannelEndpoint {
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let ctx = DriverContext::default();
        let local: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote = channel.remote_data;
        let transport = UdpChannelTransport::open(&channel, &local, &remote, &ctx).unwrap();
        SendChannelEndpoint::new(channel, transport)
    }

    fn build_sm_bytes(session_id: i32, stream_id: i32) -> [u8; SM_TOTAL_LENGTH] {
        let sm = StatusMessage {
            frame_header: FrameHeader {
                frame_length: SM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SM,
            },
            session_id,
            stream_id,
            consumption_term_id: 1,
            consumption_term_offset: 0,
            receiver_window: 65536,
            receiver_id: 42,
        };
        let mut buf = [0u8; SM_TOTAL_LENGTH];
        sm.write(&mut buf);
        buf
    }

    fn build_nak_bytes(session_id: i32, stream_id: i32) -> [u8; NAK_TOTAL_LENGTH] {
        let nak = NakHeader {
            frame_header: FrameHeader {
                frame_length: NAK_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_NAK,
            },
            session_id,
            stream_id,
            active_term_id: 0,
            term_offset: 512,
            length: 1408,
        };
        let mut buf = [0u8; NAK_TOTAL_LENGTH];
        nak.write(&mut buf);
        buf
    }

    #[test]
    fn on_message_ingests_sm() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let sm_buf = build_sm_bytes(42, 7);

        ep.on_message(&sm_buf, &source);
        assert_eq!(ep.recent_sm_len, 1);
        assert_eq!({ ep.recent_sm[0].sm.session_id }, 42);
        assert_eq!({ ep.recent_sm[0].sm.stream_id }, 7);
    }

    #[test]
    fn on_message_ingests_nak() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let nak_buf = build_nak_bytes(99, 3);

        ep.on_message(&nak_buf, &source);
        assert_eq!(ep.recent_naks_len, 1);
        assert_eq!({ ep.recent_naks[0].nak.session_id }, 99);
        assert_eq!({ ep.recent_naks[0].nak.term_offset }, 512);
    }

    #[test]
    fn on_message_ignores_short_buffer() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        ep.on_message(&[0u8; 4], &source);
        assert_eq!(ep.recent_sm_len, 0);
        assert_eq!(ep.recent_naks_len, 0);
    }

    #[test]
    fn drain_sm_yields_all_and_resets() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        for i in 0..3 {
            let sm_buf = build_sm_bytes(i, 1);
            ep.on_message(&sm_buf, &source);
        }
        assert_eq!(ep.recent_sm_len, 3);

        let mut drained = Vec::new();
        ep.drain_sm(|sm| drained.push(sm.session_id));
        assert_eq!(drained, vec![0, 1, 2]);
        assert_eq!(ep.recent_sm_len, 0);
    }

    #[test]
    fn drain_naks_yields_all_and_resets() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        let nak_buf = build_nak_bytes(55, 2);
        ep.on_message(&nak_buf, &source);
        assert_eq!(ep.recent_naks_len, 1);

        let mut drained = Vec::new();
        ep.drain_naks(|nak| drained.push(nak.session_id));
        assert_eq!(drained, vec![55]);
        assert_eq!(ep.recent_naks_len, 0);
    }

    #[test]
    fn sm_overflow_drops_excess() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        for i in 0..(MAX_RECENT_SM as i32 + 10) {
            let sm_buf = build_sm_bytes(i, 1);
            ep.on_message(&sm_buf, &source);
        }
        // Should have exactly MAX_RECENT_SM, extras silently dropped.
        assert_eq!(ep.recent_sm_len, MAX_RECENT_SM);
    }

    #[test]
    fn nak_overflow_drops_excess() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };

        for i in 0..(MAX_RECENT_NAK as i32 + 5) {
            let nak_buf = build_nak_bytes(i, 1);
            ep.on_message(&nak_buf, &source);
        }
        assert_eq!(ep.recent_naks_len, MAX_RECENT_NAK);
    }

    // ── RTTM tests ──

    fn build_rttm_reply_bytes(
        session_id: i32,
        stream_id: i32,
        echo_ts: i64,
    ) -> [u8; RTTM_TOTAL_LENGTH] {
        let rttm = RttmHeader {
            frame_header: FrameHeader {
                frame_length: RTTM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: RTTM_FLAG_REPLY,
                frame_type: FRAME_TYPE_RTTM,
            },
            session_id,
            stream_id,
            echo_timestamp: echo_ts,
            reception_delta: 0,
            receiver_id: 0xCAFE,
        };
        let mut buf = [0u8; RTTM_TOTAL_LENGTH];
        rttm.write(&mut buf);
        buf
    }

    fn build_rttm_request_bytes(
        session_id: i32,
        stream_id: i32,
        echo_ts: i64,
    ) -> [u8; RTTM_TOTAL_LENGTH] {
        let rttm = RttmHeader {
            frame_header: FrameHeader {
                frame_length: RTTM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0, // request
                frame_type: FRAME_TYPE_RTTM,
            },
            session_id,
            stream_id,
            echo_timestamp: echo_ts,
            reception_delta: 0,
            receiver_id: 0,
        };
        let mut buf = [0u8; RTTM_TOTAL_LENGTH];
        rttm.write(&mut buf);
        buf
    }

    #[test]
    fn on_message_ingests_rttm_reply() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let buf = build_rttm_reply_bytes(42, 7, 123_456);
        ep.on_message(&buf, &source);
        assert_eq!(ep.recent_rttms_len, 1);
        assert_eq!({ ep.recent_rttms[0].rttm.session_id }, 42);
        assert_eq!({ ep.recent_rttms[0].rttm.echo_timestamp }, 123_456);
    }

    #[test]
    fn on_message_ignores_rttm_request() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let buf = build_rttm_request_bytes(42, 7, 999);
        ep.on_message(&buf, &source);
        assert_eq!(
            ep.recent_rttms_len, 0,
            "request should be ignored by sender endpoint"
        );
    }

    #[test]
    fn drain_rttm_replies_yields_all_and_resets() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        for i in 0..3 {
            let buf = build_rttm_reply_bytes(i, 1, i as i64 * 1000);
            ep.on_message(&buf, &source);
        }
        assert_eq!(ep.recent_rttms_len, 3);

        let mut drained = Vec::new();
        ep.drain_rttm_replies(|rttm| drained.push(rttm.session_id));
        assert_eq!(drained, vec![0, 1, 2]);
        assert_eq!(ep.recent_rttms_len, 0);
    }

    #[test]
    fn rttm_overflow_drops_excess() {
        let mut ep = dummy_endpoint();
        let source: libc::sockaddr_storage = unsafe { mem::zeroed() };
        for i in 0..(MAX_RECENT_RTTM as i32 + 5) {
            let buf = build_rttm_reply_bytes(i, 1, i as i64);
            ep.on_message(&buf, &source);
        }
        assert_eq!(ep.recent_rttms_len, MAX_RECENT_RTTM);
    }
}
