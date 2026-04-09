// Aeron wire protocol frames - zero-copy parsing from byte slices.
// Mirrors the C structs in aeron_protocol.h.
//
// All `parse()` methods use `repr(C, packed)` overlay for zero-copy access.
// This is safe ONLY on little-endian targets where all bit patterns are valid.

#[cfg(not(target_endian = "little"))]
compile_error!(
    "aeron-rs wire format assumes little-endian byte order. \
     Big-endian targets require field-by-field from_le_bytes parsing."
);

use std::fmt;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use libc;

// ──────────────────────────── Constants ────────────────────────────

pub const FRAME_HEADER_LENGTH: usize = 8;
pub const DATA_HEADER_LENGTH: usize = 32;
pub const SM_HEADER_LENGTH: usize = 36;
pub const NAK_HEADER_LENGTH: usize = 28;
pub const SETUP_HEADER_LENGTH: usize = 40;
pub const RTTM_HEADER_LENGTH: usize = 40;

pub const CURRENT_VERSION: u8 = 0;

pub const FRAME_TYPE_PAD: u16 = 0x0000;
pub const FRAME_TYPE_DATA: u16 = 0x0001;
pub const FRAME_TYPE_NAK: u16 = 0x0002;
pub const FRAME_TYPE_SM: u16 = 0x0003;
pub const FRAME_TYPE_ERR: u16 = 0x0004;
pub const FRAME_TYPE_SETUP: u16 = 0x0005;
pub const FRAME_TYPE_RTTM: u16 = 0x0006;
pub const FRAME_TYPE_RESOLUTION: u16 = 0x0007;
pub const FRAME_TYPE_HEARTBEAT: u16 = 0x0008;

pub const DATA_FLAG_BEGIN: u8 = 0x80;
pub const DATA_FLAG_END: u8 = 0x40;
pub const DATA_FLAG_EOS: u8 = 0x20;

pub const SM_FLAG_SETUP: u8 = 0x80;

pub const SETUP_FLAG_RESPONSE: u8 = 0x80;

pub const RTTM_FLAG_REPLY: u8 = 0x80;

// ──────────────────────────── Frame Header ────────────────────────────

/// Common 8-byte frame header for all Aeron protocol messages.
///
/// Wire layout (little-endian):
///   offset 0: frame_length (i32)
///   offset 4: version (u8)
///   offset 5: flags (u8)
///   offset 6: frame_type (u16)
#[derive(Clone, Copy)]
#[repr(C, packed)]
pub struct FrameHeader {
    pub frame_length: i32,
    pub version: u8,
    pub flags: u8,
    pub frame_type: u16,
}

impl FrameHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&FrameHeader> {
        if buf.len() < FRAME_HEADER_LENGTH {
            return None;
        }
        // SAFETY: FrameHeader is repr(C, packed) and all bit patterns are valid.
        Some(unsafe { &*(buf.as_ptr() as *const FrameHeader) })
    }

    pub fn write(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= FRAME_HEADER_LENGTH);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const FrameHeader as *const u8,
                buf.as_mut_ptr(),
                FRAME_HEADER_LENGTH,
            );
        }
    }
}

impl fmt::Debug for FrameHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fl = self.frame_length;
        let v = self.version;
        let flags = self.flags;
        let ft = self.frame_type;
        f.debug_struct("FrameHeader")
            .field("frame_length", &fl)
            .field("version", &v)
            .field("flags", &flags)
            .field("frame_type", &ft)
            .finish()
    }
}

// ──────────────────────────── Data / Pad Header ────────────────────────────

/// 32-byte Data (or Pad) frame header.
///
/// Wire layout:
///   offset  0: [FrameHeader]  (8 bytes)
///   offset  8: term_offset    (i32)
///   offset 12: session_id     (i32)
///   offset 16: stream_id      (i32)
///   offset 20: term_id        (i32)
///   offset 24: reserved_value (i64)
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct DataHeader {
    pub frame_header: FrameHeader,
    pub term_offset: i32,
    pub session_id: i32,
    pub stream_id: i32,
    pub term_id: i32,
    pub reserved_value: i64,
}

impl DataHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&DataHeader> {
        if buf.len() < DATA_HEADER_LENGTH {
            return None;
        }
        Some(unsafe { &*(buf.as_ptr() as *const DataHeader) })
    }

    #[inline]
    pub fn parse_mut(buf: &mut [u8]) -> Option<&mut DataHeader> {
        if buf.len() < DATA_HEADER_LENGTH {
            return None;
        }
        Some(unsafe { &mut *(buf.as_mut_ptr() as *mut DataHeader) })
    }

    #[inline]
    pub fn payload<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        let frame_len = self.frame_header.frame_length as usize;
        if frame_len > DATA_HEADER_LENGTH && frame_len <= buf.len() {
            &buf[DATA_HEADER_LENGTH..frame_len]
        } else {
            &[]
        }
    }
}

// ──────────────────────────── Status Message (SM) ────────────────────────────

/// 36-byte Status Message frame.
///
/// Wire layout:
///   offset  0: [FrameHeader]                (8 bytes)
///   offset  8: session_id                   (i32)
///   offset 12: stream_id                    (i32)
///   offset 16: consumption_term_id          (i32)
///   offset 20: consumption_term_offset      (i32)
///   offset 24: receiver_window              (i32)
///   offset 28: receiver_id                  (i64)
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct StatusMessage {
    pub frame_header: FrameHeader,
    pub session_id: i32,
    pub stream_id: i32,
    pub consumption_term_id: i32,
    pub consumption_term_offset: i32,
    pub receiver_window: i32,
    pub receiver_id: i64,
}

pub const SM_TOTAL_LENGTH: usize = mem::size_of::<StatusMessage>();

impl StatusMessage {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&StatusMessage> {
        if buf.len() < SM_TOTAL_LENGTH {
            return None;
        }
        Some(unsafe { &*(buf.as_ptr() as *const StatusMessage) })
    }

    pub fn write(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= SM_TOTAL_LENGTH);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const StatusMessage as *const u8,
                buf.as_mut_ptr(),
                SM_TOTAL_LENGTH,
            );
        }
    }
}

// ──────────────────────────── NAK ────────────────────────────

/// 28-byte NAK frame.
///
/// Wire layout:
///   offset  0: [FrameHeader]   (8 bytes)
///   offset  8: session_id      (i32)
///   offset 12: stream_id       (i32)
///   offset 16: active_term_id  (i32)
///   offset 20: term_offset     (i32)
///   offset 24: length          (i32)
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct NakHeader {
    pub frame_header: FrameHeader,
    pub session_id: i32,
    pub stream_id: i32,
    pub active_term_id: i32,
    pub term_offset: i32,
    pub length: i32,
}

pub const NAK_TOTAL_LENGTH: usize = mem::size_of::<NakHeader>();

impl NakHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&NakHeader> {
        if buf.len() < NAK_TOTAL_LENGTH {
            return None;
        }
        Some(unsafe { &*(buf.as_ptr() as *const NakHeader) })
    }

    pub fn write(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= NAK_TOTAL_LENGTH);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const NakHeader as *const u8,
                buf.as_mut_ptr(),
                NAK_TOTAL_LENGTH,
            );
        }
    }
}

// ──────────────────────────── Setup ────────────────────────────

/// 40-byte Setup frame.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct SetupHeader {
    pub frame_header: FrameHeader,
    pub term_offset: i32,
    pub session_id: i32,
    pub stream_id: i32,
    pub initial_term_id: i32,
    pub active_term_id: i32,
    pub term_length: i32,
    pub mtu: i32,
    pub ttl: i32,
}

pub const SETUP_TOTAL_LENGTH: usize = mem::size_of::<SetupHeader>();

impl SetupHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&SetupHeader> {
        if buf.len() < SETUP_TOTAL_LENGTH {
            return None;
        }
        Some(unsafe { &*(buf.as_ptr() as *const SetupHeader) })
    }

    pub fn write(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= SETUP_TOTAL_LENGTH);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const SetupHeader as *const u8,
                buf.as_mut_ptr(),
                SETUP_TOTAL_LENGTH,
            );
        }
    }
}

// ──────────────────────────── RTT Measurement ────────────────────────────

/// 40-byte RTTM frame.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct RttmHeader {
    pub frame_header: FrameHeader,
    pub session_id: i32,
    pub stream_id: i32,
    pub echo_timestamp: i64,
    pub reception_delta: i64,
    pub receiver_id: i64,
}

pub const RTTM_TOTAL_LENGTH: usize = mem::size_of::<RttmHeader>();

impl RttmHeader {
    #[inline]
    pub fn parse(buf: &[u8]) -> Option<&RttmHeader> {
        if buf.len() < RTTM_TOTAL_LENGTH {
            return None;
        }
        Some(unsafe { &*(buf.as_ptr() as *const RttmHeader) })
    }

    pub fn write(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= RTTM_TOTAL_LENGTH);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self as *const RttmHeader as *const u8,
                buf.as_mut_ptr(),
                RTTM_TOTAL_LENGTH,
            );
        }
    }
}

// ──────────────────────────── Dispatch helper ────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Pad,
    Data,
    Nak,
    StatusMessage,
    Error,
    Setup,
    Rttm,
    Resolution,
    Heartbeat,
    Unknown(u16),
}

impl From<u16> for FrameType {
    fn from(val: u16) -> Self {
        match val {
            FRAME_TYPE_PAD => FrameType::Pad,
            FRAME_TYPE_DATA => FrameType::Data,
            FRAME_TYPE_NAK => FrameType::Nak,
            FRAME_TYPE_SM => FrameType::StatusMessage,
            FRAME_TYPE_ERR => FrameType::Error,
            FRAME_TYPE_SETUP => FrameType::Setup,
            FRAME_TYPE_RTTM => FrameType::Rttm,
            FRAME_TYPE_RESOLUTION => FrameType::Resolution,
            FRAME_TYPE_HEARTBEAT => FrameType::Heartbeat,
            other => FrameType::Unknown(other),
        }
    }
}

/// Classify the frame type from a raw buffer without full parsing.
#[inline]
pub fn classify_frame(buf: &[u8]) -> Option<FrameType> {
    let hdr = FrameHeader::parse(buf)?;
    Some(FrameType::from(hdr.frame_type))
}

/// Convert a sockaddr_storage to a Rust SocketAddr.
pub fn sockaddr_to_std(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<SocketAddr> {
    if (len as usize) < mem::size_of::<libc::sa_family_t>() {
        return None;
    }
    match storage.ss_family as i32 {
        libc::AF_INET => {
            if (len as usize) < mem::size_of::<libc::sockaddr_in>() {
                return None;
            }
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        libc::AF_INET6 => {
            if (len as usize) < mem::size_of::<libc::sockaddr_in6>() {
                return None;
            }
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => None,
    }
}

// ──────────────────────────── Compile-time size assertions ────────────────────────────

const _: () = assert!(FRAME_HEADER_LENGTH == mem::size_of::<FrameHeader>());
const _: () = assert!(DATA_HEADER_LENGTH == mem::size_of::<DataHeader>());
const _: () = assert!(SM_HEADER_LENGTH == mem::size_of::<StatusMessage>());
const _: () = assert!(NAK_HEADER_LENGTH == mem::size_of::<NakHeader>());
const _: () = assert!(SETUP_HEADER_LENGTH == mem::size_of::<SetupHeader>());
const _: () = assert!(RTTM_HEADER_LENGTH == mem::size_of::<RttmHeader>());

#[cfg(test)]
mod tests {
    use super::*;

    /// Copy a field out of a packed struct to avoid creating an unaligned reference.
    /// Usage: `let val = { parsed.field_name };`
    /// The block expression copies the field by value.

    // ── FrameHeader ──

    #[test]
    fn frame_header_parse_short_buffer_returns_none() {
        assert!(FrameHeader::parse(&[]).is_none());
        assert!(FrameHeader::parse(&[0u8; 7]).is_none());
    }

    #[test]
    fn frame_header_write_parse_roundtrip() {
        let hdr = FrameHeader {
            frame_length: 64,
            version: CURRENT_VERSION,
            flags: 0xAB,
            frame_type: FRAME_TYPE_DATA,
        };
        let mut buf = [0u8; FRAME_HEADER_LENGTH];
        hdr.write(&mut buf);
        let p = FrameHeader::parse(&buf).unwrap();
        assert_eq!({ p.frame_length }, 64);
        assert_eq!({ p.version }, CURRENT_VERSION);
        assert_eq!({ p.flags }, 0xAB);
        assert_eq!({ p.frame_type }, FRAME_TYPE_DATA);
    }

    // ── DataHeader ──

    #[test]
    fn data_header_parse_short_buffer_returns_none() {
        assert!(DataHeader::parse(&[0u8; DATA_HEADER_LENGTH - 1]).is_none());
    }

    #[test]
    fn data_header_write_parse_roundtrip() {
        let data = DataHeader {
            frame_header: FrameHeader {
                frame_length: 64,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 1024,
            session_id: 42,
            stream_id: 7,
            term_id: 200,
            reserved_value: 0xDEAD,
        };
        let mut buf = [0u8; 64];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data as *const DataHeader as *const u8,
                buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
        let p = DataHeader::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 42);
        assert_eq!({ p.stream_id }, 7);
        assert_eq!({ p.term_id }, 200);
        assert_eq!({ p.term_offset }, 1024);
        assert_eq!({ p.reserved_value }, 0xDEAD);
        assert_eq!({ p.frame_header.frame_type }, FRAME_TYPE_DATA);
    }

    #[test]
    fn data_header_payload_extraction() {
        let mut buf = [0u8; 36];
        let data = DataHeader {
            frame_header: FrameHeader {
                frame_length: 36,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 0,
            session_id: 1,
            stream_id: 1,
            term_id: 0,
            reserved_value: 0,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data as *const DataHeader as *const u8,
                buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
        buf[32..36].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);

        let parsed = DataHeader::parse(&buf).unwrap();
        let payload = parsed.payload(&buf);
        assert_eq!(payload, &[0xCA, 0xFE, 0xBA, 0xBE]);
    }

    #[test]
    fn data_header_payload_empty_when_frame_length_equals_header() {
        let mut buf = [0u8; DATA_HEADER_LENGTH];
        let data = DataHeader {
            frame_header: FrameHeader {
                frame_length: DATA_HEADER_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 0,
            session_id: 1,
            stream_id: 1,
            term_id: 0,
            reserved_value: 0,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data as *const DataHeader as *const u8,
                buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
        let parsed = DataHeader::parse(&buf).unwrap();
        assert!(parsed.payload(&buf).is_empty());
    }

    #[test]
    fn data_header_parse_mut() {
        let mut buf = [0u8; DATA_HEADER_LENGTH];
        let data = DataHeader {
            frame_header: FrameHeader {
                frame_length: DATA_HEADER_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 0,
            session_id: 99,
            stream_id: 1,
            term_id: 0,
            reserved_value: 0,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data as *const DataHeader as *const u8,
                buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
        let hdr = DataHeader::parse_mut(&mut buf).unwrap();
        hdr.session_id = 200;
        let p = DataHeader::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 200);
    }

    // ── StatusMessage ──

    #[test]
    fn status_message_parse_short_returns_none() {
        assert!(StatusMessage::parse(&[0u8; SM_TOTAL_LENGTH - 1]).is_none());
    }

    #[test]
    fn status_message_write_parse_roundtrip() {
        let sm = StatusMessage {
            frame_header: FrameHeader {
                frame_length: SM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SM,
            },
            session_id: 42,
            stream_id: 7,
            consumption_term_id: 100,
            consumption_term_offset: 2048,
            receiver_window: 65536,
            receiver_id: 0xBEEF,
        };
        let mut buf = [0u8; SM_TOTAL_LENGTH];
        sm.write(&mut buf);
        let p = StatusMessage::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 42);
        assert_eq!({ p.stream_id }, 7);
        assert_eq!({ p.consumption_term_id }, 100);
        assert_eq!({ p.consumption_term_offset }, 2048);
        assert_eq!({ p.receiver_window }, 65536);
        assert_eq!({ p.receiver_id }, 0xBEEF);
    }

    // ── NakHeader ──

    #[test]
    fn nak_parse_short_returns_none() {
        assert!(NakHeader::parse(&[0u8; NAK_TOTAL_LENGTH - 1]).is_none());
    }

    #[test]
    fn nak_write_parse_roundtrip() {
        let nak = NakHeader {
            frame_header: FrameHeader {
                frame_length: NAK_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_NAK,
            },
            session_id: 42,
            stream_id: 7,
            active_term_id: 100,
            term_offset: 512,
            length: 1408,
        };
        let mut buf = [0u8; NAK_TOTAL_LENGTH];
        nak.write(&mut buf);
        let p = NakHeader::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 42);
        assert_eq!({ p.active_term_id }, 100);
        assert_eq!({ p.term_offset }, 512);
        assert_eq!({ p.length }, 1408);
    }

    // ── SetupHeader ──

    #[test]
    fn setup_parse_short_returns_none() {
        assert!(SetupHeader::parse(&[0u8; SETUP_TOTAL_LENGTH - 1]).is_none());
    }

    #[test]
    fn setup_write_parse_roundtrip() {
        let setup = SetupHeader {
            frame_header: FrameHeader {
                frame_length: SETUP_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_SETUP,
            },
            term_offset: 0,
            session_id: 42,
            stream_id: 7,
            initial_term_id: 10,
            active_term_id: 12,
            term_length: 1 << 16,
            mtu: 1408,
            ttl: 8,
        };
        let mut buf = [0u8; SETUP_TOTAL_LENGTH];
        setup.write(&mut buf);
        let p = SetupHeader::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 42);
        assert_eq!({ p.stream_id }, 7);
        assert_eq!({ p.initial_term_id }, 10);
        assert_eq!({ p.active_term_id }, 12);
        assert_eq!({ p.term_length }, 1 << 16);
        assert_eq!({ p.mtu }, 1408);
        assert_eq!({ p.ttl }, 8);
    }

    // ── RttmHeader ──

    #[test]
    fn rttm_parse_short_returns_none() {
        assert!(RttmHeader::parse(&[0u8; RTTM_TOTAL_LENGTH - 1]).is_none());
    }

    #[test]
    fn rttm_write_parse_roundtrip() {
        let rttm = RttmHeader {
            frame_header: FrameHeader {
                frame_length: RTTM_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_RTTM,
            },
            session_id: 42,
            stream_id: 7,
            echo_timestamp: 123_456_789,
            reception_delta: 5000,
            receiver_id: 0xCAFE,
        };
        let mut buf = [0u8; RTTM_TOTAL_LENGTH];
        rttm.write(&mut buf);
        let p = RttmHeader::parse(&buf).unwrap();
        assert_eq!({ p.session_id }, 42);
        assert_eq!({ p.echo_timestamp }, 123_456_789);
        assert_eq!({ p.reception_delta }, 5000);
        assert_eq!({ p.receiver_id }, 0xCAFE);
    }

    // ── classify_frame ──

    #[test]
    fn classify_frame_empty_returns_none() {
        assert!(classify_frame(&[]).is_none());
    }

    #[test]
    fn classify_frame_all_types() {
        let cases: &[(u16, FrameType)] = &[
            (FRAME_TYPE_PAD, FrameType::Pad),
            (FRAME_TYPE_DATA, FrameType::Data),
            (FRAME_TYPE_NAK, FrameType::Nak),
            (FRAME_TYPE_SM, FrameType::StatusMessage),
            (FRAME_TYPE_ERR, FrameType::Error),
            (FRAME_TYPE_SETUP, FrameType::Setup),
            (FRAME_TYPE_RTTM, FrameType::Rttm),
            (FRAME_TYPE_RESOLUTION, FrameType::Resolution),
            (FRAME_TYPE_HEARTBEAT, FrameType::Heartbeat),
        ];
        for &(wire_val, expected) in cases {
            let hdr = FrameHeader {
                frame_length: FRAME_HEADER_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: wire_val,
            };
            let mut buf = [0u8; FRAME_HEADER_LENGTH];
            hdr.write(&mut buf);
            assert_eq!(classify_frame(&buf), Some(expected), "frame_type=0x{wire_val:04X}");
        }
    }

    #[test]
    fn classify_frame_unknown_type() {
        let hdr = FrameHeader {
            frame_length: FRAME_HEADER_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: 0xFFFF,
        };
        let mut buf = [0u8; FRAME_HEADER_LENGTH];
        hdr.write(&mut buf);
        assert_eq!(classify_frame(&buf), Some(FrameType::Unknown(0xFFFF)));
    }

    // ── FrameType ──

    #[test]
    fn frame_type_from_u16() {
        assert_eq!(FrameType::from(FRAME_TYPE_DATA), FrameType::Data);
        assert_eq!(FrameType::from(0x9999), FrameType::Unknown(0x9999));
    }

    // ── sockaddr_to_std ──

    #[test]
    fn sockaddr_to_std_ipv4() {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = 40123u16.to_be();
        // 127.0.0.1 in network byte order (big-endian).
        sin.sin_addr.s_addr = u32::to_be(0x7F000001);
        let len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

        let addr = sockaddr_to_std(&storage, len).expect("should parse IPv4");
        assert_eq!(addr, "127.0.0.1:40123".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn sockaddr_to_std_ipv6() {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
        sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        sin6.sin6_port = 9999u16.to_be();
        sin6.sin6_addr.s6_addr = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let len = mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;

        let addr = sockaddr_to_std(&storage, len).expect("should parse IPv6");
        assert_eq!(addr, "[::1]:9999".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn sockaddr_to_std_short_len_returns_none() {
        let storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        assert!(sockaddr_to_std(&storage, 0).is_none());
    }

    #[test]
    fn sockaddr_to_std_unknown_family_returns_none() {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        storage.ss_family = 255;
        let len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        assert!(sockaddr_to_std(&storage, len).is_none());
    }

    // ── Debug formatting ──

    #[test]
    fn frame_header_debug_does_not_panic() {
        let hdr = FrameHeader {
            frame_length: 32,
            version: 0,
            flags: 0xC0,
            frame_type: FRAME_TYPE_DATA,
        };
        let s = format!("{hdr:?}");
        assert!(s.contains("FrameHeader"));
        assert!(s.contains("frame_length"));
    }
}

