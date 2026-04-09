// Command and response protocol for CnC IPC.
//
// Fixed-size command structs sent from clients to the conductor via
// the to-driver MPSC ring buffer. Fixed-size response structs sent
// from the conductor to clients via the to-clients broadcast buffer.
//
// All encode/decode uses from_le_bytes field-by-field (per coding rules).
// No pointer casts for wire format. No allocation.

// ---- Command message types (client -> driver) ----

/// Add a publication on the given channel/stream.
pub const CMD_ADD_PUBLICATION: i32 = 1;
/// Remove a publication by registration_id.
pub const CMD_REMOVE_PUBLICATION: i32 = 2;
/// Add a subscription on the given channel/stream.
pub const CMD_ADD_SUBSCRIPTION: i32 = 3;
/// Remove a subscription by registration_id.
pub const CMD_REMOVE_SUBSCRIPTION: i32 = 4;
/// Client keepalive - resets the client liveness timeout.
pub const CMD_CLIENT_KEEPALIVE: i32 = 5;
/// Client close - signals graceful disconnect.
pub const CMD_CLIENT_CLOSE: i32 = 6;

// ---- Response message types (driver -> client) ----

/// Confirms a publication is ready. Contains the registration_id and
/// session_id assigned by the driver.
pub const RSP_PUBLICATION_READY: i32 = 101;
/// Confirms a subscription is ready.
pub const RSP_SUBSCRIPTION_READY: i32 = 102;
/// Reports an error for a command (keyed by correlation_id).
pub const RSP_OPERATION_ERROR: i32 = 103;
/// Driver heartbeat - indicates the driver is alive.
pub const RSP_DRIVER_HEARTBEAT: i32 = 104;

// ---- Wire sizes ----

/// Channel URI is stored inline with a max length.
/// This keeps commands fixed-size (no heap allocation for strings).
pub const MAX_CHANNEL_URI_LENGTH: usize = 256;

// ---- Command: AddPublication ----

/// Fixed-size command for adding a publication.
///
/// Wire layout (little-endian, field-by-field):
///   [i64 correlation_id | i64 client_id | i32 stream_id | i32 channel_len |
///    [u8; 256] channel_uri]
pub const ADD_PUBLICATION_LENGTH: usize = 8 + 8 + 4 + 4 + MAX_CHANNEL_URI_LENGTH;

#[derive(Debug, Clone)]
pub struct AddPublication {
    pub correlation_id: i64,
    pub client_id: i64,
    pub stream_id: i32,
    pub channel_uri_len: usize,
    pub channel_uri: [u8; MAX_CHANNEL_URI_LENGTH],
}

impl AddPublication {
    /// Encode into a byte buffer. Returns the number of bytes written.
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < ADD_PUBLICATION_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.client_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[20..24].copy_from_slice(&(self.channel_uri_len as i32).to_le_bytes());
        buf[24..24 + MAX_CHANNEL_URI_LENGTH].copy_from_slice(&self.channel_uri);
        Some(ADD_PUBLICATION_LENGTH)
    }

    /// Decode from a byte buffer.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < ADD_PUBLICATION_LENGTH {
            return None;
        }
        let correlation_id = i64::from_le_bytes(buf[0..8].try_into().ok()?);
        let client_id = i64::from_le_bytes(buf[8..16].try_into().ok()?);
        let stream_id = i32::from_le_bytes(buf[16..20].try_into().ok()?);
        let channel_uri_len = i32::from_le_bytes(buf[20..24].try_into().ok()?) as usize;
        if channel_uri_len > MAX_CHANNEL_URI_LENGTH {
            return None;
        }
        let mut channel_uri = [0u8; MAX_CHANNEL_URI_LENGTH];
        channel_uri.copy_from_slice(&buf[24..24 + MAX_CHANNEL_URI_LENGTH]);
        Some(Self {
            correlation_id,
            client_id,
            stream_id,
            channel_uri_len,
            channel_uri,
        })
    }

    /// Get the channel URI as a string slice.
    pub fn channel_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.channel_uri[..self.channel_uri_len]).ok()
    }

    /// Create from a channel URI string.
    pub fn from_channel(
        correlation_id: i64,
        client_id: i64,
        stream_id: i32,
        channel: &str,
    ) -> Option<Self> {
        if channel.len() > MAX_CHANNEL_URI_LENGTH {
            return None;
        }
        let mut channel_uri = [0u8; MAX_CHANNEL_URI_LENGTH];
        channel_uri[..channel.len()].copy_from_slice(channel.as_bytes());
        Some(Self {
            correlation_id,
            client_id,
            stream_id,
            channel_uri_len: channel.len(),
            channel_uri,
        })
    }
}

// ---- Command: RemovePublication ----

/// Wire layout: [i64 correlation_id | i64 client_id | i64 registration_id]
pub const REMOVE_PUBLICATION_LENGTH: usize = 8 + 8 + 8;

#[derive(Debug, Clone, Copy)]
pub struct RemovePublication {
    pub correlation_id: i64,
    pub client_id: i64,
    pub registration_id: i64,
}

impl RemovePublication {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < REMOVE_PUBLICATION_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.client_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.registration_id.to_le_bytes());
        Some(REMOVE_PUBLICATION_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < REMOVE_PUBLICATION_LENGTH {
            return None;
        }
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            client_id: i64::from_le_bytes(buf[8..16].try_into().ok()?),
            registration_id: i64::from_le_bytes(buf[16..24].try_into().ok()?),
        })
    }
}

// ---- Command: AddSubscription ----

/// Same wire layout as AddPublication.
pub const ADD_SUBSCRIPTION_LENGTH: usize = ADD_PUBLICATION_LENGTH;

#[derive(Debug, Clone)]
pub struct AddSubscription {
    pub correlation_id: i64,
    pub client_id: i64,
    pub stream_id: i32,
    pub channel_uri_len: usize,
    pub channel_uri: [u8; MAX_CHANNEL_URI_LENGTH],
}

impl AddSubscription {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < ADD_SUBSCRIPTION_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.client_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[20..24].copy_from_slice(&(self.channel_uri_len as i32).to_le_bytes());
        buf[24..24 + MAX_CHANNEL_URI_LENGTH].copy_from_slice(&self.channel_uri);
        Some(ADD_SUBSCRIPTION_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < ADD_SUBSCRIPTION_LENGTH {
            return None;
        }
        let correlation_id = i64::from_le_bytes(buf[0..8].try_into().ok()?);
        let client_id = i64::from_le_bytes(buf[8..16].try_into().ok()?);
        let stream_id = i32::from_le_bytes(buf[16..20].try_into().ok()?);
        let channel_uri_len = i32::from_le_bytes(buf[20..24].try_into().ok()?) as usize;
        if channel_uri_len > MAX_CHANNEL_URI_LENGTH {
            return None;
        }
        let mut channel_uri = [0u8; MAX_CHANNEL_URI_LENGTH];
        channel_uri.copy_from_slice(&buf[24..24 + MAX_CHANNEL_URI_LENGTH]);
        Some(Self {
            correlation_id,
            client_id,
            stream_id,
            channel_uri_len,
            channel_uri,
        })
    }

    pub fn channel_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.channel_uri[..self.channel_uri_len]).ok()
    }

    pub fn from_channel(
        correlation_id: i64,
        client_id: i64,
        stream_id: i32,
        channel: &str,
    ) -> Option<Self> {
        if channel.len() > MAX_CHANNEL_URI_LENGTH {
            return None;
        }
        let mut channel_uri = [0u8; MAX_CHANNEL_URI_LENGTH];
        channel_uri[..channel.len()].copy_from_slice(channel.as_bytes());
        Some(Self {
            correlation_id,
            client_id,
            stream_id,
            channel_uri_len: channel.len(),
            channel_uri,
        })
    }
}

// ---- Command: RemoveSubscription ----

pub const REMOVE_SUBSCRIPTION_LENGTH: usize = REMOVE_PUBLICATION_LENGTH;

#[derive(Debug, Clone, Copy)]
pub struct RemoveSubscription {
    pub correlation_id: i64,
    pub client_id: i64,
    pub registration_id: i64,
}

impl RemoveSubscription {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < REMOVE_SUBSCRIPTION_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.client_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.registration_id.to_le_bytes());
        Some(REMOVE_SUBSCRIPTION_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < REMOVE_SUBSCRIPTION_LENGTH {
            return None;
        }
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            client_id: i64::from_le_bytes(buf[8..16].try_into().ok()?),
            registration_id: i64::from_le_bytes(buf[16..24].try_into().ok()?),
        })
    }
}

// ---- Command: ClientKeepalive ----

/// Wire layout: [i64 correlation_id | i64 client_id]
pub const CLIENT_KEEPALIVE_LENGTH: usize = 8 + 8;

#[derive(Debug, Clone, Copy)]
pub struct ClientKeepalive {
    pub correlation_id: i64,
    pub client_id: i64,
}

impl ClientKeepalive {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < CLIENT_KEEPALIVE_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.client_id.to_le_bytes());
        Some(CLIENT_KEEPALIVE_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < CLIENT_KEEPALIVE_LENGTH {
            return None;
        }
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            client_id: i64::from_le_bytes(buf[8..16].try_into().ok()?),
        })
    }
}

// ---- Response: PublicationReady ----

/// Wire layout: [i64 correlation_id | i64 registration_id | i32 session_id |
///               i32 stream_id | i32 position_limit_counter_id | i32 channel_status_indicator_id]
pub const PUBLICATION_READY_LENGTH: usize = 8 + 8 + 4 + 4 + 4 + 4;

#[derive(Debug, Clone, Copy)]
pub struct PublicationReady {
    pub correlation_id: i64,
    pub registration_id: i64,
    pub session_id: i32,
    pub stream_id: i32,
    pub position_limit_counter_id: i32,
    pub channel_status_indicator_id: i32,
}

impl PublicationReady {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < PUBLICATION_READY_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.registration_id.to_le_bytes());
        buf[16..20].copy_from_slice(&self.session_id.to_le_bytes());
        buf[20..24].copy_from_slice(&self.stream_id.to_le_bytes());
        buf[24..28].copy_from_slice(&self.position_limit_counter_id.to_le_bytes());
        buf[28..32].copy_from_slice(&self.channel_status_indicator_id.to_le_bytes());
        Some(PUBLICATION_READY_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < PUBLICATION_READY_LENGTH {
            return None;
        }
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            registration_id: i64::from_le_bytes(buf[8..16].try_into().ok()?),
            session_id: i32::from_le_bytes(buf[16..20].try_into().ok()?),
            stream_id: i32::from_le_bytes(buf[20..24].try_into().ok()?),
            position_limit_counter_id: i32::from_le_bytes(buf[24..28].try_into().ok()?),
            channel_status_indicator_id: i32::from_le_bytes(buf[28..32].try_into().ok()?),
        })
    }
}

// ---- Response: SubscriptionReady ----

/// Wire layout: [i64 correlation_id | i32 channel_status_indicator_id]
pub const SUBSCRIPTION_READY_LENGTH: usize = 8 + 4;

#[derive(Debug, Clone, Copy)]
pub struct SubscriptionReady {
    pub correlation_id: i64,
    pub channel_status_indicator_id: i32,
}

impl SubscriptionReady {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < SUBSCRIPTION_READY_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.channel_status_indicator_id.to_le_bytes());
        Some(SUBSCRIPTION_READY_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < SUBSCRIPTION_READY_LENGTH {
            return None;
        }
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            channel_status_indicator_id: i32::from_le_bytes(buf[8..12].try_into().ok()?),
        })
    }
}

// ---- Response: OperationError ----

/// Wire layout: [i64 correlation_id | i32 error_code | i32 error_msg_len |
///               [u8; 256] error_msg]
pub const MAX_ERROR_MSG_LENGTH: usize = 256;
pub const OPERATION_ERROR_LENGTH: usize = 8 + 4 + 4 + MAX_ERROR_MSG_LENGTH;

#[derive(Debug, Clone)]
pub struct OperationError {
    pub correlation_id: i64,
    pub error_code: i32,
    pub error_msg_len: usize,
    pub error_msg: [u8; MAX_ERROR_MSG_LENGTH],
}

impl OperationError {
    pub fn encode(&self, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < OPERATION_ERROR_LENGTH {
            return None;
        }
        buf[0..8].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.error_code.to_le_bytes());
        buf[12..16].copy_from_slice(&(self.error_msg_len as i32).to_le_bytes());
        buf[16..16 + MAX_ERROR_MSG_LENGTH].copy_from_slice(&self.error_msg);
        Some(OPERATION_ERROR_LENGTH)
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < OPERATION_ERROR_LENGTH {
            return None;
        }
        let error_msg_len = i32::from_le_bytes(buf[12..16].try_into().ok()?) as usize;
        if error_msg_len > MAX_ERROR_MSG_LENGTH {
            return None;
        }
        let mut error_msg = [0u8; MAX_ERROR_MSG_LENGTH];
        error_msg.copy_from_slice(&buf[16..16 + MAX_ERROR_MSG_LENGTH]);
        Some(Self {
            correlation_id: i64::from_le_bytes(buf[0..8].try_into().ok()?),
            error_code: i32::from_le_bytes(buf[8..12].try_into().ok()?),
            error_msg_len,
            error_msg,
        })
    }

    pub fn error_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.error_msg[..self.error_msg_len]).ok()
    }

    pub fn from_message(correlation_id: i64, error_code: i32, msg: &str) -> Self {
        let len = msg.len().min(MAX_ERROR_MSG_LENGTH);
        let mut error_msg = [0u8; MAX_ERROR_MSG_LENGTH];
        error_msg[..len].copy_from_slice(&msg.as_bytes()[..len]);
        Self {
            correlation_id,
            error_code,
            error_msg_len: len,
            error_msg,
        }
    }
}

// ---- Error codes for OperationError ----

pub const ERROR_UNKNOWN_COMMAND: i32 = 1;
pub const ERROR_CHANNEL_PARSE_FAILED: i32 = 2;
pub const ERROR_PUBLICATION_CREATE_FAILED: i32 = 3;
pub const ERROR_SUBSCRIPTION_CREATE_FAILED: i32 = 4;
pub const ERROR_REGISTRATION_NOT_FOUND: i32 = 5;
pub const ERROR_GENERIC: i32 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_publication_roundtrip() {
        let cmd = AddPublication::from_channel(1, 2, 10, "aeron:udp?endpoint=127.0.0.1:40123")
            .expect("create");
        assert_eq!(cmd.correlation_id, 1);
        assert_eq!(cmd.stream_id, 10);
        assert_eq!(
            cmd.channel_str(),
            Some("aeron:udp?endpoint=127.0.0.1:40123"),
        );

        let mut buf = [0u8; ADD_PUBLICATION_LENGTH];
        let len = cmd.encode(&mut buf).expect("encode");
        assert_eq!(len, ADD_PUBLICATION_LENGTH);

        let decoded = AddPublication::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 1);
        assert_eq!(decoded.client_id, 2);
        assert_eq!(decoded.stream_id, 10);
        assert_eq!(
            decoded.channel_str(),
            Some("aeron:udp?endpoint=127.0.0.1:40123"),
        );
    }

    #[test]
    fn add_publication_channel_too_long() {
        let long = "x".repeat(MAX_CHANNEL_URI_LENGTH + 1);
        assert!(AddPublication::from_channel(1, 1, 1, &long).is_none());
    }

    #[test]
    fn remove_publication_roundtrip() {
        let cmd = RemovePublication {
            correlation_id: 5,
            client_id: 2,
            registration_id: 42,
        };
        let mut buf = [0u8; REMOVE_PUBLICATION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        let decoded = RemovePublication::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 5);
        assert_eq!(decoded.registration_id, 42);
    }

    #[test]
    fn add_subscription_roundtrip() {
        let cmd = AddSubscription::from_channel(3, 1, 20, "aeron:udp?endpoint=127.0.0.1:40124")
            .expect("create");
        let mut buf = [0u8; ADD_SUBSCRIPTION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        let decoded = AddSubscription::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 3);
        assert_eq!(decoded.stream_id, 20);
        assert_eq!(
            decoded.channel_str(),
            Some("aeron:udp?endpoint=127.0.0.1:40124"),
        );
    }

    #[test]
    fn remove_subscription_roundtrip() {
        let cmd = RemoveSubscription {
            correlation_id: 7,
            client_id: 1,
            registration_id: 99,
        };
        let mut buf = [0u8; REMOVE_SUBSCRIPTION_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        let decoded = RemoveSubscription::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 7);
        assert_eq!(decoded.registration_id, 99);
    }

    #[test]
    fn client_keepalive_roundtrip() {
        let cmd = ClientKeepalive {
            correlation_id: 10,
            client_id: 3,
        };
        let mut buf = [0u8; CLIENT_KEEPALIVE_LENGTH];
        cmd.encode(&mut buf).expect("encode");
        let decoded = ClientKeepalive::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 10);
        assert_eq!(decoded.client_id, 3);
    }

    #[test]
    fn publication_ready_roundtrip() {
        let rsp = PublicationReady {
            correlation_id: 1,
            registration_id: 100,
            session_id: 42,
            stream_id: 10,
            position_limit_counter_id: 0,
            channel_status_indicator_id: 1,
        };
        let mut buf = [0u8; PUBLICATION_READY_LENGTH];
        rsp.encode(&mut buf).expect("encode");
        let decoded = PublicationReady::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 1);
        assert_eq!(decoded.registration_id, 100);
        assert_eq!(decoded.session_id, 42);
        assert_eq!(decoded.stream_id, 10);
    }

    #[test]
    fn subscription_ready_roundtrip() {
        let rsp = SubscriptionReady {
            correlation_id: 5,
            channel_status_indicator_id: 2,
        };
        let mut buf = [0u8; SUBSCRIPTION_READY_LENGTH];
        rsp.encode(&mut buf).expect("encode");
        let decoded = SubscriptionReady::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 5);
        assert_eq!(decoded.channel_status_indicator_id, 2);
    }

    #[test]
    fn operation_error_roundtrip() {
        let rsp = OperationError::from_message(7, ERROR_CHANNEL_PARSE_FAILED, "bad channel URI");
        assert_eq!(rsp.error_str(), Some("bad channel URI"));

        let mut buf = [0u8; OPERATION_ERROR_LENGTH];
        rsp.encode(&mut buf).expect("encode");
        let decoded = OperationError::decode(&buf).expect("decode");
        assert_eq!(decoded.correlation_id, 7);
        assert_eq!(decoded.error_code, ERROR_CHANNEL_PARSE_FAILED);
        assert_eq!(decoded.error_str(), Some("bad channel URI"));
    }

    #[test]
    fn decode_too_short_returns_none() {
        assert!(AddPublication::decode(&[0u8; 4]).is_none());
        assert!(RemovePublication::decode(&[0u8; 4]).is_none());
        assert!(PublicationReady::decode(&[0u8; 4]).is_none());
        assert!(OperationError::decode(&[0u8; 4]).is_none());
    }
}

