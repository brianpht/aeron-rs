// NAK-driven retransmit scheduler. Pre-sized flat array, zero allocation.
//
// State machine per action:
//   Inactive -> Delay(expiry_ns) -> [callback fires] -> Linger(expiry_ns) -> Inactive
//
// Delay coalesces duplicate NAKs for the same (session, stream, term_id, offset).
// Linger prevents re-triggering the same range too soon after retransmit.
//
// One RetransmitHandler per SenderAgent (not per-publication). Matches Aeron C.

use crate::frame::NakHeader;

/// Maximum concurrent retransmit actions. Matches Aeron C default.
/// Each action is ~40 bytes, total ~2.5 KiB on stack.
pub const MAX_RETRANSMIT_ACTIONS: usize = 64;

/// State machine for a single retransmit action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetransmitState {
    /// Slot is free.
    Inactive,
    /// Waiting for delay to elapse before retransmitting.
    /// Coalesces duplicate NAKs for the same range.
    Delay { expiry_ns: i64 },
    /// Retransmit completed. Waiting for linger to prevent
    /// re-retransmitting the same range too quickly.
    Linger { expiry_ns: i64 },
}

/// One pending retransmit action. Stack-only, Copy.
#[derive(Clone, Copy)]
struct RetransmitAction {
    session_id: i32,
    stream_id: i32,
    term_id: i32,
    term_offset: i32,
    length: i32,
    state: RetransmitState,
}

impl RetransmitAction {
    const INACTIVE: Self = Self {
        session_id: 0,
        stream_id: 0,
        term_id: 0,
        term_offset: 0,
        length: 0,
        state: RetransmitState::Inactive,
    };

    /// Exact (session, stream, term_id, offset) match for dedup.
    /// Matches Aeron C semantics - range overlap coalescing is a future opt.
    #[inline]
    fn matches(&self, session_id: i32, stream_id: i32, term_id: i32, term_offset: i32) -> bool {
        self.session_id == session_id
            && self.stream_id == stream_id
            && self.term_id == term_id
            && self.term_offset == term_offset
    }
}

/// NAK-driven retransmit scheduler. Pre-sized flat array, zero allocation.
pub struct RetransmitHandler {
    actions: [RetransmitAction; MAX_RETRANSMIT_ACTIONS],
    delay_ns: i64,
    linger_ns: i64,
}

impl RetransmitHandler {
    /// Cold path - construct with delay and linger intervals.
    pub fn new(delay_ns: i64, linger_ns: i64) -> Self {
        Self {
            actions: [RetransmitAction::INACTIVE; MAX_RETRANSMIT_ACTIONS],
            delay_ns,
            linger_ns,
        }
    }

    /// Schedule a retransmit from a received NAK.
    ///
    /// Returns true if a new Delay action was created.
    /// Returns false if:
    /// - A matching (session, stream, term_id, offset) action already
    ///   exists in Delay or Linger state (dedup)
    /// - No free slot available (table full, NAK dropped)
    ///
    /// Zero-allocation, O(MAX_RETRANSMIT_ACTIONS). Hot path.
    pub fn on_nak(&mut self, nak: &NakHeader, now_ns: i64) -> bool {
        // Copy fields from packed struct to avoid unaligned references.
        let session_id = { nak.session_id };
        let stream_id = { nak.stream_id };
        let term_id = { nak.active_term_id };
        let term_offset = { nak.term_offset };
        let length = { nak.length };

        // Single pass: dedup check + find first free slot.
        let mut free_idx: Option<usize> = None;
        for i in 0..MAX_RETRANSMIT_ACTIONS {
            let action = &self.actions[i];
            match action.state {
                RetransmitState::Inactive => {
                    if free_idx.is_none() {
                        free_idx = Some(i);
                    }
                }
                RetransmitState::Delay { .. } | RetransmitState::Linger { .. } => {
                    if action.matches(session_id, stream_id, term_id, term_offset) {
                        return false; // Dedup - already scheduled or lingering.
                    }
                }
            }
        }

        // Insert into first free slot.
        if let Some(idx) = free_idx {
            self.actions[idx] = RetransmitAction {
                session_id,
                stream_id,
                term_id,
                term_offset,
                length,
                state: RetransmitState::Delay {
                    expiry_ns: now_ns.wrapping_add(self.delay_ns),
                },
            };
            true
        } else {
            false // Table full, NAK dropped.
        }
    }

    /// Process all actions: fire expired Delays, expire Lingers.
    ///
    /// For each Delay past expiry: calls `on_retransmit(session_id,
    /// stream_id, term_id, term_offset, length)` and transitions
    /// to Linger state.
    ///
    /// For each Linger past expiry: transitions to Inactive (frees slot).
    ///
    /// Zero-allocation, O(MAX_RETRANSMIT_ACTIONS). Hot path.
    pub fn process_timeouts<F>(&mut self, now_ns: i64, mut on_retransmit: F)
    where
        F: FnMut(i32, i32, i32, i32, i32),
    {
        for i in 0..MAX_RETRANSMIT_ACTIONS {
            match self.actions[i].state {
                RetransmitState::Delay { expiry_ns } => {
                    // Wrapping sub + half-range for time comparison.
                    let elapsed = now_ns.wrapping_sub(expiry_ns);
                    if elapsed >= 0 && elapsed < (i64::MAX / 2) {
                        let a = &self.actions[i];
                        on_retransmit(
                            a.session_id,
                            a.stream_id,
                            a.term_id,
                            a.term_offset,
                            a.length,
                        );
                        self.actions[i].state = RetransmitState::Linger {
                            expiry_ns: now_ns.wrapping_add(self.linger_ns),
                        };
                    }
                }
                RetransmitState::Linger { expiry_ns } => {
                    let elapsed = now_ns.wrapping_sub(expiry_ns);
                    if elapsed >= 0 && elapsed < (i64::MAX / 2) {
                        self.actions[i].state = RetransmitState::Inactive;
                    }
                }
                RetransmitState::Inactive => {}
            }
        }
    }

    /// Number of currently active actions (Delay + Linger).
    pub fn active_count(&self) -> usize {
        let mut count = 0usize;
        for i in 0..MAX_RETRANSMIT_ACTIONS {
            if self.actions[i].state != RetransmitState::Inactive {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::*;

    fn make_nak(
        session_id: i32,
        stream_id: i32,
        term_id: i32,
        term_offset: i32,
        length: i32,
    ) -> NakHeader {
        NakHeader {
            frame_header: FrameHeader {
                frame_length: NAK_TOTAL_LENGTH as i32,
                version: CURRENT_VERSION,
                flags: 0,
                frame_type: FRAME_TYPE_NAK,
            },
            session_id,
            stream_id,
            active_term_id: term_id,
            term_offset,
            length,
        }
    }

    #[test]
    fn retransmit_action_default_inactive() {
        let action = RetransmitAction::INACTIVE;
        assert_eq!(action.state, RetransmitState::Inactive);
    }

    #[test]
    fn on_nak_schedules_delay() {
        let mut handler = RetransmitHandler::new(1_000, 60_000_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        assert!(handler.on_nak(&nak, 100));
        assert_eq!(handler.active_count(), 1);
    }

    #[test]
    fn on_nak_dedup_same_range() {
        let mut handler = RetransmitHandler::new(1_000, 60_000_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        assert!(handler.on_nak(&nak, 100));
        // Same (session, stream, term, offset) - dedup rejects.
        assert!(!handler.on_nak(&nak, 200));
        assert_eq!(handler.active_count(), 1);
    }

    #[test]
    fn on_nak_different_offset_not_deduped() {
        let mut handler = RetransmitHandler::new(1_000, 60_000_000);
        let nak1 = make_nak(42, 7, 0, 512, 1408);
        let nak2 = make_nak(42, 7, 0, 1024, 1408);
        assert!(handler.on_nak(&nak1, 100));
        assert!(handler.on_nak(&nak2, 100));
        assert_eq!(handler.active_count(), 2);
    }

    #[test]
    fn on_nak_rejects_when_full() {
        let mut handler = RetransmitHandler::new(1_000, 60_000_000);
        for i in 0..MAX_RETRANSMIT_ACTIONS {
            let nak = make_nak(i as i32, 1, 0, 0, 32);
            assert!(handler.on_nak(&nak, 100), "slot {i} should succeed");
        }
        // Table full - next NAK should be dropped.
        let nak = make_nak(999, 1, 0, 0, 32);
        assert!(!handler.on_nak(&nak, 100));
        assert_eq!(handler.active_count(), MAX_RETRANSMIT_ACTIONS);
    }

    #[test]
    fn process_timeouts_delay_to_callback() {
        let mut handler = RetransmitHandler::new(1_000, 60_000_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        handler.on_nak(&nak, 100);

        let mut fired = Vec::new();
        handler.process_timeouts(1_101, |sid, stid, tid, toff, len| {
            fired.push((sid, stid, tid, toff, len));
        });

        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0], (42, 7, 0, 512, 1408));
        // Should now be in Linger state, still active.
        assert_eq!(handler.active_count(), 1);
    }

    #[test]
    fn process_timeouts_linger_to_inactive() {
        let mut handler = RetransmitHandler::new(0, 1_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        handler.on_nak(&nak, 100);

        // Fire the delay (delay_ns=0, so immediately expired).
        handler.process_timeouts(100, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 1); // Now in Linger.

        // Advance past linger expiry.
        handler.process_timeouts(1_101, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 0); // Inactive.
    }

    #[test]
    fn process_timeouts_not_yet_expired() {
        let mut handler = RetransmitHandler::new(10_000, 60_000_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        handler.on_nak(&nak, 100);

        let mut fired = Vec::new();
        handler.process_timeouts(5_000, |sid, stid, tid, toff, len| {
            fired.push((sid, stid, tid, toff, len));
        });

        assert!(fired.is_empty());
        assert_eq!(handler.active_count(), 1); // Still in Delay.
    }

    #[test]
    fn linger_prevents_reschedule() {
        let mut handler = RetransmitHandler::new(0, 60_000_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        handler.on_nak(&nak, 100);

        // Fire the delay.
        handler.process_timeouts(100, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 1); // In Linger.

        // Try to schedule same range again - should be deduped by Linger.
        assert!(!handler.on_nak(&nak, 200));
        assert_eq!(handler.active_count(), 1);
    }

    #[test]
    fn linger_expires_allows_reschedule() {
        let mut handler = RetransmitHandler::new(0, 1_000);
        let nak = make_nak(42, 7, 0, 512, 1408);
        handler.on_nak(&nak, 100);

        // Fire delay, enter linger.
        handler.process_timeouts(100, |_, _, _, _, _| {});

        // Expire linger.
        handler.process_timeouts(1_101, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 0);

        // Now reschedule should work.
        assert!(handler.on_nak(&nak, 1_200));
        assert_eq!(handler.active_count(), 1);
    }

    #[test]
    fn active_count_tracks_correctly() {
        let mut handler = RetransmitHandler::new(0, 1_000_000);

        assert_eq!(handler.active_count(), 0);

        let nak1 = make_nak(1, 1, 0, 0, 32);
        let nak2 = make_nak(2, 1, 0, 0, 32);
        let nak3 = make_nak(3, 1, 0, 0, 32);

        handler.on_nak(&nak1, 0);
        assert_eq!(handler.active_count(), 1);

        handler.on_nak(&nak2, 0);
        assert_eq!(handler.active_count(), 2);

        handler.on_nak(&nak3, 0);
        assert_eq!(handler.active_count(), 3);

        // Fire all delays (delay_ns = 0).
        handler.process_timeouts(0, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 3); // All in Linger.

        // Expire all lingers.
        handler.process_timeouts(1_000_001, |_, _, _, _, _| {});
        assert_eq!(handler.active_count(), 0);
    }
}

