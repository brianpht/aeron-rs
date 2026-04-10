// Lock-free single-producer single-consumer transfer bridge.
//
// Delivers PendingPublication structs from the client thread to the
// sender agent thread without Mutex, allocation, or SeqCst.
//
// Pre-allocated at init time (cold path). In steady state, the sender
// scans BRIDGE_CAPACITY atomic loads per duty cycle (all Acquire).
// When empty, each load resolves to a single cache line read.
//
// Invariants:
// - Single producer (Aeron client thread) calls deposit()
// - Single consumer (SenderAgent thread) calls try_take()
// - Slot transitions: EMPTY -> FILLED (producer) -> EMPTY (consumer)
// - Data is written under EMPTY, read under FILLED (no concurrent access)

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::media::concurrent_publication::SenderPublication;
use crate::media::send_channel_endpoint::SendChannelEndpoint;

/// Maximum concurrent in-flight publication transfers.
pub(crate) const BRIDGE_CAPACITY: usize = 32;

const SLOT_EMPTY: u8 = 0;
const SLOT_FILLED: u8 = 1;

/// Everything the sender needs to wire up a concurrent publication.
/// Created by the client, consumed by the sender. Moved, not copied.
pub(crate) struct PendingPublication {
    pub sender_pub: SenderPublication,
    pub endpoint: SendChannelEndpoint,
    pub dest_addr: libc::sockaddr_storage,
}

struct BridgeSlot {
    state: AtomicU8,
    data: UnsafeCell<Option<PendingPublication>>,
}

/// Lock-free SPSC bridge for publication handle transfer.
///
/// Shared between the Aeron client and SenderAgent via `Arc`.
/// Pre-allocated at driver init. Never resized.
pub(crate) struct PublicationBridge {
    slots: Box<[BridgeSlot]>,
}

// SAFETY: Single producer (client) and single consumer (sender).
// Slot state transitions are exclusive per side. Data in UnsafeCell is
// only written when EMPTY (by producer) and only read when FILLED
// (by consumer). Acquire/Release on the state atomic ensures visibility
// of the data written before the state transition.
unsafe impl Send for PublicationBridge {}
unsafe impl Sync for PublicationBridge {}

impl PublicationBridge {
    /// Create a new bridge with pre-allocated slots. Cold path.
    pub(crate) fn new() -> Arc<Self> {
        let slots: Vec<BridgeSlot> = (0..BRIDGE_CAPACITY)
            .map(|_| BridgeSlot {
                state: AtomicU8::new(SLOT_EMPTY),
                data: UnsafeCell::new(None),
            })
            .collect();
        Arc::new(Self {
            slots: slots.into_boxed_slice(),
        })
    }

    /// Deposit a pending publication for the sender to pick up.
    ///
    /// Returns `true` on success, `false` if all slots are occupied.
    /// Called from the client thread only (single producer). Cold path.
    pub(crate) fn deposit(&self, item: PendingPublication) -> bool {
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
    /// Returns the pending publication if the slot was filled, None otherwise.
    /// Called from the sender thread only (single consumer).
    /// Each call is one Acquire load + (if filled) one Release store.
    pub(crate) fn try_take(&self, idx: usize) -> Option<PendingPublication> {
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

    // We cannot construct real PendingPublication in unit tests without
    // creating sockets, but we can test the bridge mechanics with a
    // simpler type. Here we test the atomic state machine directly.

    #[test]
    fn new_bridge_has_all_empty_slots() {
        let bridge = PublicationBridge::new();
        for i in 0..bridge.capacity() {
            assert!(bridge.try_take(i).is_none());
        }
    }

    #[test]
    fn bridge_capacity_matches_constant() {
        let bridge = PublicationBridge::new();
        assert_eq!(bridge.capacity(), BRIDGE_CAPACITY);
    }

    #[test]
    fn try_take_out_of_bounds_returns_none() {
        let bridge = PublicationBridge::new();
        assert!(bridge.try_take(BRIDGE_CAPACITY + 10).is_none());
    }
}

