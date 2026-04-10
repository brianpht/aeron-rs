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

use crate::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use crate::media::shared_image::SubscriberImage;

/// Maximum concurrent in-flight image transfers.
pub(crate) const SUB_BRIDGE_CAPACITY: usize = 32;

/// Maximum concurrent in-flight endpoint transfers (client -> receiver).
pub(crate) const RECV_ENDPOINT_BRIDGE_CAPACITY: usize = 16;

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

    /// Peek at the stream_id of the item in the given slot without taking it.
    ///
    /// Returns `Some(stream_id)` if the slot is filled, `None` if empty or
    /// out of bounds. The item remains in the slot.
    ///
    /// Called from the client thread only (single consumer).
    /// One Acquire load on the state, one read of stream_id if filled.
    pub(crate) fn peek_stream_id(&self, idx: usize) -> Option<i32> {
        let slot = self.slots.get(idx)?;
        if slot.state.load(Ordering::Acquire) == SLOT_FILLED {
            // SAFETY: Single consumer. State is FILLED so no producer
            // is writing this slot. Reading stream_id is safe.
            let data_ref = unsafe { &*slot.data.get() };
            data_ref.as_ref().map(|p| p.stream_id)
        } else {
            None
        }
    }

    /// Number of slots in the bridge.
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }
}

// ---- Receive endpoint bridge (client -> receiver agent) ----

/// Everything the receiver agent needs to add a receive endpoint.
/// Created by the client (add_subscription), consumed by the receiver.
/// Moved, not copied. Cold path.
pub(crate) struct PendingRecvEndpoint {
    pub endpoint: ReceiveChannelEndpoint,
}

struct EndpointBridgeSlot {
    state: AtomicU8,
    data: UnsafeCell<Option<PendingRecvEndpoint>>,
}

/// Lock-free SPSC bridge for receive endpoint transfer.
///
/// Producer: Aeron client (add_subscription)
/// Consumer: ReceiverAgent (poll_recv_endpoint_bridge)
///
/// Same atomic state machine as PublicationBridge and SubscriptionBridge:
/// EMPTY -> FILLED (producer) -> EMPTY (consumer).
///
/// Pre-allocated at driver init. Never resized.
pub(crate) struct RecvEndpointBridge {
    slots: Box<[EndpointBridgeSlot]>,
}

// SAFETY: Single producer (client) and single consumer (receiver).
// Same invariants as PublicationBridge - see bridge.rs.
unsafe impl Send for RecvEndpointBridge {}
unsafe impl Sync for RecvEndpointBridge {}

impl RecvEndpointBridge {
    /// Create a new bridge with pre-allocated slots. Cold path.
    pub(crate) fn new() -> Arc<Self> {
        let slots: Vec<EndpointBridgeSlot> = (0..RECV_ENDPOINT_BRIDGE_CAPACITY)
            .map(|_| EndpointBridgeSlot {
                state: AtomicU8::new(SLOT_EMPTY),
                data: UnsafeCell::new(None),
            })
            .collect();
        Arc::new(Self {
            slots: slots.into_boxed_slice(),
        })
    }

    /// Deposit a pending endpoint for the receiver to pick up.
    ///
    /// Returns `true` on success, `false` if all slots are occupied.
    /// Called from the client thread only (single producer). Cold path.
    pub(crate) fn deposit(&self, item: PendingRecvEndpoint) -> bool {
        for slot in self.slots.iter() {
            if slot.state.load(Ordering::Acquire) == SLOT_EMPTY {
                // SAFETY: Single producer. State is EMPTY so no consumer
                // is reading this slot.
                unsafe { *slot.data.get() = Some(item); }
                slot.state.store(SLOT_FILLED, Ordering::Release);
                return true;
            }
        }
        false
    }

    /// Try to take one item from the given slot index.
    ///
    /// Called from the receiver thread only (single consumer).
    pub(crate) fn try_take(&self, idx: usize) -> Option<PendingRecvEndpoint> {
        let slot = self.slots.get(idx)?;
        if slot.state.load(Ordering::Acquire) == SLOT_FILLED {
            // SAFETY: Single consumer. State is FILLED so no producer
            // is writing this slot.
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
    use crate::media::shared_image::new_shared_image;

    /// Helper: create a PendingImage with given ids. Allocates a real
    /// SubscriberImage via new_shared_image (cold path, test only).
    fn make_pending(session_id: i32, stream_id: i32, correlation_id: i64) -> PendingImage {
        let (_recv, sub) = new_shared_image(session_id, stream_id, 0, 0, 1024, 0)
            .expect("valid shared image for test");
        PendingImage {
            image: sub,
            session_id,
            stream_id,
            correlation_id,
        }
    }

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

    #[test]
    fn deposit_and_take_single() {
        let bridge = SubscriptionBridge::new();
        let pending = make_pending(42, 7, 100);

        assert!(bridge.deposit(pending), "deposit should succeed");

        // First slot should contain the item.
        let taken = bridge.try_take(0);
        assert!(taken.is_some(), "try_take(0) should return the item");
        let item = taken.expect("checked above");
        assert_eq!(item.session_id, 42);
        assert_eq!(item.stream_id, 7);
        assert_eq!(item.correlation_id, 100);

        // Slot should be empty now.
        assert!(bridge.try_take(0).is_none(), "slot should be empty after take");
    }

    #[test]
    fn deposit_fills_first_empty() {
        let bridge = SubscriptionBridge::new();

        assert!(bridge.deposit(make_pending(1, 10, 1)));
        assert!(bridge.deposit(make_pending(2, 20, 2)));
        assert!(bridge.deposit(make_pending(3, 30, 3)));

        // Items should be in slots 0, 1, 2 in order.
        let t0 = bridge.try_take(0).expect("slot 0");
        assert_eq!(t0.session_id, 1);
        let t1 = bridge.try_take(1).expect("slot 1");
        assert_eq!(t1.session_id, 2);
        let t2 = bridge.try_take(2).expect("slot 2");
        assert_eq!(t2.session_id, 3);

        // No more items.
        assert!(bridge.try_take(3).is_none());
    }

    #[test]
    fn deposit_when_full_returns_false() {
        let bridge = SubscriptionBridge::new();

        // Fill all slots.
        for i in 0..SUB_BRIDGE_CAPACITY {
            assert!(
                bridge.deposit(make_pending(i as i32, i as i32, i as i64)),
                "deposit {i} should succeed"
            );
        }

        // Next deposit should fail.
        assert!(
            !bridge.deposit(make_pending(999, 999, 999)),
            "deposit should fail when bridge is full"
        );

        // Taking one slot should free it.
        let _ = bridge.try_take(0);
        assert!(
            bridge.deposit(make_pending(888, 888, 888)),
            "deposit should succeed after freeing a slot"
        );
    }

    #[test]
    fn take_then_deposit_reuses_slot() {
        let bridge = SubscriptionBridge::new();

        // Deposit + take from slot 0.
        assert!(bridge.deposit(make_pending(1, 1, 1)));
        let first = bridge.try_take(0).expect("slot 0 first round");
        assert_eq!(first.session_id, 1);

        // Deposit again - should reuse slot 0 (it is now EMPTY).
        assert!(bridge.deposit(make_pending(2, 2, 2)));
        let second = bridge.try_take(0).expect("slot 0 second round");
        assert_eq!(second.session_id, 2);
    }

    #[test]
    fn cross_thread_deposit_take() {
        let bridge = SubscriptionBridge::new();
        let bridge_clone = Arc::clone(&bridge);

        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_clone = done.clone();

        // Producer thread (simulates receiver agent).
        let producer = std::thread::spawn(move || {
            for i in 0..10 {
                // Spin until deposit succeeds (bridge might be full).
                loop {
                    let pending = make_pending(i, i * 10, i as i64);
                    if bridge_clone.deposit(pending) {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
            done_clone.store(true, std::sync::atomic::Ordering::Release);
        });

        // Consumer thread (main, simulates client subscription).
        let mut taken_ids = Vec::new();
        loop {
            for idx in 0..SUB_BRIDGE_CAPACITY {
                if let Some(item) = bridge.try_take(idx) {
                    taken_ids.push(item.session_id);
                }
            }
            if taken_ids.len() >= 10 {
                break;
            }
            if done.load(std::sync::atomic::Ordering::Acquire) {
                // Final drain.
                for idx in 0..SUB_BRIDGE_CAPACITY {
                    if let Some(item) = bridge.try_take(idx) {
                        taken_ids.push(item.session_id);
                    }
                }
                break;
            }
            std::thread::yield_now();
        }

        producer.join().expect("producer thread");
        assert_eq!(taken_ids.len(), 10, "should have taken all 10 items");

        // Verify all session_ids 0..10 were received (order may vary).
        taken_ids.sort();
        assert_eq!(taken_ids, (0..10).collect::<Vec<i32>>());
    }
}

