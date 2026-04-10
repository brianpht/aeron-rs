// Subscription - user-facing handle for receiving data from an Aeron stream.
//
// For v1, the subscription registers with the driver via CnC but the
// data path (reading from receiver image logs) requires shared-memory
// image buffers between the receiver agent and the client. This will be
// implemented when image logs move to mmap-backed shared memory.
//
// Currently provides: registration tracking, channel/stream metadata.
// Data poll API is stubbed - returns 0 fragments until the shared
// image buffer mechanism is implemented.

/// A subscription handle for receiving data from an Aeron stream.
///
/// Obtained from `Aeron::add_subscription`. Tracks registration state
/// and provides metadata accessors.
///
/// The data path (`poll`) requires shared-memory image buffers between
/// the receiver agent and the client. This is not yet implemented -
/// `poll` currently returns 0.
pub struct Subscription {
    registration_id: i64,
    stream_id: i32,
    channel_uri_buf: [u8; 256],
    channel_uri_len: usize,
}

impl Subscription {
    pub(crate) fn new(
        registration_id: i64,
        stream_id: i32,
        channel: &str,
    ) -> Self {
        let len = channel.len().min(256);
        let mut buf = [0u8; 256];
        buf[..len].copy_from_slice(&channel.as_bytes()[..len]);
        Self {
            registration_id,
            stream_id,
            channel_uri_buf: buf,
            channel_uri_len: len,
        }
    }

    /// Poll for incoming data fragments.
    ///
    /// Calls `handler(data, session_id, stream_id)` for each received
    /// fragment, up to `limit` fragments per call.
    ///
    /// Returns the number of fragments delivered.
    ///
    /// NOTE: Data path not yet implemented - requires shared-memory
    /// image buffers between the receiver agent and the client.
    /// Currently always returns 0.
    pub fn poll<F>(&mut self, _handler: F, _limit: i32) -> i32
    where
        F: FnMut(&[u8], i32, i32),
    {
        // Data path requires mmap-backed image logs shared between the
        // receiver agent and the client. Stubbed for v1.
        0
    }

    /// Registration ID assigned to this subscription.
    #[inline]
    pub fn registration_id(&self) -> i64 {
        self.registration_id
    }

    /// Stream ID for this subscription.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.stream_id
    }

    /// Channel URI string.
    pub fn channel(&self) -> &str {
        // SAFETY: channel_uri_buf[..len] was copied from a valid &str.
        unsafe {
            std::str::from_utf8_unchecked(&self.channel_uri_buf[..self.channel_uri_len])
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("registration_id", &self.registration_id)
            .field("stream_id", &self.stream_id)
            .field("channel", &self.channel())
            .finish()
    }
}

