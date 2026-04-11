// Publication - user-facing handle for offering data into an Aeron stream.
//
// Wraps a ConcurrentPublication (Arc-shared log buffer with the sender).
// Provides offer() for writing data frames, plus accessors for position
// and stream metadata.
//
// Not Clone (single publisher per publication, enforced at compile time).
// Send (can be moved to a dedicated publisher thread).
// Zero-allocation in steady state (offer is O(1), no syscall).

use crate::media::concurrent_publication::ConcurrentPublication;
use crate::media::network_publication::OfferError;

/// A publication handle for sending data into an Aeron stream.
///
/// Obtained from `Aeron::add_publication`. The publication writes into a
/// shared log buffer that the sender agent scans and transmits over UDP.
///
/// Takes `&mut self` for `offer()` - enforces single-publisher at
/// compile time. Not Clone. Send but not Sync.
pub struct Publication {
    inner: ConcurrentPublication,
    registration_id: i64,
    channel_uri_buf: [u8; 256],
    channel_uri_len: usize,
}

impl Publication {
    pub(crate) fn new(inner: ConcurrentPublication, registration_id: i64, channel: &str) -> Self {
        let len = channel.len().min(256);
        let mut buf = [0u8; 256];
        buf[..len].copy_from_slice(&channel.as_bytes()[..len]);
        Self {
            inner,
            registration_id,
            channel_uri_buf: buf,
            channel_uri_len: len,
        }
    }

    /// Offer a payload to be published as a data frame.
    ///
    /// Writes header + payload into the shared log buffer. The sender
    /// agent picks it up via `sender_scan` and transmits over UDP.
    ///
    /// Zero-allocation, O(1). Hot path.
    ///
    /// Returns the new publication position on success.
    ///
    /// Errors:
    /// - `PayloadTooLarge` - payload exceeds mtu - DATA_HEADER_LENGTH
    /// - `BackPressured` - sender has not caught up (retry later)
    /// - `AdminAction` - term rotated (retry immediately)
    #[inline]
    pub fn offer(&mut self, payload: &[u8]) -> Result<i64, OfferError> {
        self.inner.offer(payload)
    }

    /// Registration ID assigned to this publication.
    #[inline]
    pub fn registration_id(&self) -> i64 {
        self.registration_id
    }

    /// Session ID for this publication.
    #[inline]
    pub fn session_id(&self) -> i32 {
        self.inner.session_id()
    }

    /// Stream ID for this publication.
    #[inline]
    pub fn stream_id(&self) -> i32 {
        self.inner.stream_id()
    }

    /// Initial term ID.
    #[inline]
    pub fn initial_term_id(&self) -> i32 {
        self.inner.initial_term_id()
    }

    /// Current active term ID (write cursor).
    #[inline]
    pub fn active_term_id(&self) -> i32 {
        self.inner.active_term_id()
    }

    /// Current write offset within the active term.
    #[inline]
    pub fn term_offset(&self) -> u32 {
        self.inner.term_offset()
    }

    /// Term buffer length per partition, in bytes.
    #[inline]
    pub fn term_length(&self) -> u32 {
        self.inner.term_length()
    }

    /// Maximum transmission unit.
    #[inline]
    pub fn mtu(&self) -> u32 {
        self.inner.mtu()
    }

    /// Highest published byte position.
    #[inline]
    pub fn position(&self) -> i64 {
        self.inner.pub_position()
    }

    /// Channel URI string.
    pub fn channel(&self) -> &str {
        // SAFETY: channel_uri_buf[..len] was copied from a valid &str.
        unsafe { std::str::from_utf8_unchecked(&self.channel_uri_buf[..self.channel_uri_len]) }
    }
}

impl std::fmt::Debug for Publication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publication")
            .field("registration_id", &self.registration_id)
            .field("session_id", &self.session_id())
            .field("stream_id", &self.stream_id())
            .field("position", &self.position())
            .finish()
    }
}
