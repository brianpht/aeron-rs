// Subscription - user-facing handle for receiving data from an Aeron stream.
//
// Polls committed data fragments from shared image buffers. The receiver
// agent writes frames into SharedLogBuffer; the subscription reads them
// using Acquire loads on frame_length (same commit protocol as
// ConcurrentPublication but reversed: receiver writes, client reads).
//
// Images are discovered via SubscriptionBridge (lock-free SPSC from the
// receiver agent). Each poll() call drains the bridge for new images
// matching this subscription's stream_id, then scans each image for
// committed fragments.
//
// Not Clone (single subscriber per subscription). Send but not Sync.
// Zero-allocation in steady state (poll is O(n) in fragments, no alloc).

use std::sync::Arc;

use crate::client::sub_bridge::{SubscriptionBridge, SUB_BRIDGE_CAPACITY};
use crate::media::shared_image::SubscriberImage;

/// Maximum images tracked per subscription. Pre-sized flat array, no allocation.
const MAX_SUB_IMAGES: usize = 16;

/// A subscription handle for receiving data from an Aeron stream.
///
/// Obtained from `Aeron::add_subscription`. Discovers images via the
/// subscription bridge and polls committed fragments from shared buffers.
///
/// Takes `&mut self` for `poll()` - single subscriber enforced at compile time.
pub struct Subscription {
    registration_id: i64,
    stream_id: i32,
    channel_uri_buf: [u8; 256],
    channel_uri_len: usize,
    /// Bridge to discover new images deposited by the receiver agent.
    sub_bridge: Arc<SubscriptionBridge>,
    /// Active images for this subscription. Pre-sized, never resized.
    images: [Option<SubscriberImage>; MAX_SUB_IMAGES],
    image_count: usize,
}

impl Subscription {
    pub(crate) fn new(
        registration_id: i64,
        stream_id: i32,
        channel: &str,
        sub_bridge: Arc<SubscriptionBridge>,
    ) -> Self {
        let len = channel.len().min(256);
        let mut buf = [0u8; 256];
        buf[..len].copy_from_slice(&channel.as_bytes()[..len]);
        Self {
            registration_id,
            stream_id,
            channel_uri_buf: buf,
            channel_uri_len: len,
            sub_bridge,
            images: std::array::from_fn(|_| None),
            image_count: 0,
        }
    }

    /// Poll for incoming data fragments.
    ///
    /// Calls `handler(payload, session_id, stream_id)` for each received
    /// fragment, up to `limit` fragments per call.
    ///
    /// Returns the number of fragments delivered.
    ///
    /// Zero-allocation, O(n) in fragments polled. Hot path.
    pub fn poll<F>(&mut self, mut handler: F, limit: i32) -> i32
    where
        F: FnMut(&[u8], i32, i32),
    {
        if limit <= 0 {
            return 0;
        }

        // Step 1: Drain bridge for new images matching our stream_id.
        self.drain_bridge();

        // Step 2: Remove closed images.
        self.remove_closed_images();

        // Step 3: Poll each image for committed fragments.
        if self.image_count == 0 {
            return 0;
        }

        let mut total_fragments = 0i32;
        let mut remaining = limit;

        for i in 0..self.image_count {
            if remaining <= 0 {
                break;
            }
            if let Some(img) = &mut self.images[i] {
                // Fair division of remaining limit across images.
                let images_left = (self.image_count - i) as i32;
                let per_image = if remaining < images_left {
                    1
                } else {
                    remaining / images_left
                };
                let fragments = img.poll_fragments(&mut handler, per_image.max(1));
                total_fragments += fragments;
                remaining -= fragments;
            }
        }

        total_fragments
    }

    /// Drain the bridge for new images matching this subscription's stream_id.
    fn drain_bridge(&mut self) {
        for idx in 0..SUB_BRIDGE_CAPACITY {
            if self.image_count >= MAX_SUB_IMAGES {
                break;
            }
            if let Some(pending) = self.sub_bridge.try_take(idx) {
                if pending.stream_id == self.stream_id {
                    self.images[self.image_count] = Some(pending.image);
                    self.image_count += 1;
                }
                // If stream_id doesn't match, the image is for a different
                // subscription. For v1 with single bridge, this should not
                // happen. The image handle is dropped (Arc refcount decrements).
            }
        }
    }

    /// Remove images whose receiver side has been closed.
    fn remove_closed_images(&mut self) {
        let mut i = 0;
        while i < self.image_count {
            let closed = self.images[i]
                .as_ref()
                .map_or(false, |img| img.is_closed());
            if closed {
                // Swap-remove with last active.
                let last = self.image_count - 1;
                if i != last {
                    self.images[i] = self.images[last].take();
                } else {
                    self.images[i] = None;
                }
                self.image_count -= 1;
                // Don't increment i - check the swapped-in entry.
            } else {
                i += 1;
            }
        }
    }

    /// Number of active images for this subscription.
    #[inline]
    pub fn image_count(&self) -> usize {
        self.image_count
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
            .field("image_count", &self.image_count)
            .finish()
    }
}
