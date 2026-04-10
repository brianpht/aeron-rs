// Lock-free single-producer single-consumer transfer bridge for images.
//
// Delivers SubscriberImage handles from the receiver agent thread to the
// client thread without Mutex, allocation, or SeqCst.
//
// Mirrors PublicationBridge (client/bridge.rs) but in reverse direction:
//   - Producer: ReceiverAgent (deposits SubscriberImage when a new image is
//     created from a Setup frame)
//   - Consumer: Subscription::poll() (takes images matching its stream_id)
//
// Pre-allocated at init time (cold path). In steady state, the
// subscription scans SUB_BRIDGE_CAPACITY atomic loads per poll (all Acquire).
//
// Invariants:
// - Single producer (receiver agent) calls deposit()
// - Single consumer (client subscription) calls try_take()
// - Slot transitions: EMPTY -> FILLED (producer) -> EMPTY (consumer)
// - Data is written under EMPTY, read under FILLED (no concurrent access)

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::media::shared_image::SubscriberImage;

/// Maximum concurrent in-flight image transfers.
pub(crate) const SUB_BRIDGE_CAPACITY: usize = 32;

const SLOT_EMPTY: u8 = 0;
const SLOT_FILLED: u8 = 1;

/// Everything the subscription needs to start polling an image.
/// Created by the receiver, consumed by the subscription. Moved, not copied.
pub(crate) struct PendingImage {
    pub image: SubscriberImage,
    pub session_id: i32,
    pub stream_id: i32,
    pub correlation_id: i64,
}

struct BridgeSlot {
    state: AtomicU8,
    data: UnsafeCell<Option<PendingImage>>,
}

/// Lock-free SPSC bridge for image handle transfer.
///
/// Shared between the ReceiverAgent and Subscription(s) via `Arc`.
/// Pre-allocated at driver init. Never resized.
pub(crate) struct SubscriptionBridge {
    slots: Box<[BridgeSlot]>,
}

// SAFETY: Single producer (receiver) and single consumer (client).
// Slot state transitions are exclusive per side. Data in UnsafeCell is
// only written when EMPTY (by producer) and only read when FILLED
// (by consumer). Acquire/Release on the state atomic ensures visibility
// of the data written before the state transition.
unsafe impl Send for SubscriptionBridge {}
unsafe impl Sync for SubscriptionBridge {}

impl SubscriptionBridge {
    /// Create a new bridge with pre-allocated slots. Cold path.
    pub(crate) fn new() -> Arc<Self> {
        let slots: Vec<BridgeSlot> = (0..SUB_BRIDGE_CAPACITY)
            .map(|_| BridgeSlot {
                state: AtomicU8::new(SLOT_EMPTY),
                data: UnsafeCell::new(None),
            })
            .collect();
        Arc::new(Self {
            slots: slots.into_boxed_slice(),
        })
    }

    /// Deposit a pending image for the subscription to pick up.
    ///
    /// Returns `true` on success, `false` if all slots are occupied.
    /// Called from the receiver thread only (single producer). Cold path.
    pub(crate) fn deposit(&self, item: PendingImage) -> bool {
        for slot in self.slots.iter() {
            if slot.state.load(Ordering::Acquire) == SLOT_EMPTY {
                // SAFETY: Single producer. State is EMPTY so no consumer
                // is reading this slot. Write the data, then publish.
                unsafe { *slot.data.get() = Some(item); }
                slot.state.store(SLOT_FILLED, Ordering::Release);
                return true;
            }
        }
        false
    }

    /// Try to take one item from the given slot index.
    ///
    /// Returns the pending image if the slot was filled, None otherwise.
    /// Called from the client thread only (single consumer).
    /// Each call is one Acquire load + (if filled) one Release store.
    pub(crate) fn try_take(&self, idx: usize) -> Option<PendingImage> {
        let slot = self.slots.get(idx)?;
        if slot.state.load(Ordering::Acquire) == SLOT_FILLED {
            // SAFETY: Single consumer. State is FILLED so no producer
            // is writing this slot. Take the data, then release.
            let item = unsafe { (*slot.data.get()).take() };
            slot.state.store(SLOT_EMPTY, Ordering::Release);
            item
        } else {
            None
        }
    }

    /// Number of slots in the bridge.
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_bridge_has_all_empty_slots() {
        let bridge = SubscriptionBridge::new();
        for i in 0..bridge.capacity() {
            assert!(bridge.try_take(i).is_none());
        }
    }

    #[test]
    fn bridge_capacity_matches_constant() {
        let bridge = SubscriptionBridge::new();
        assert_eq!(bridge.capacity(), SUB_BRIDGE_CAPACITY);
    }

    #[test]
    fn try_take_out_of_bounds_returns_none() {
        let bridge = SubscriptionBridge::new();
        assert!(bridge.try_take(SUB_BRIDGE_CAPACITY + 10).is_none());
    }
}

