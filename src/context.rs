use std::time::Duration;

use crate::media::buffer_pool::MAX_RECV_BUFFER;

/// Validation error for `DriverContext`. Stack-only, no heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextValidationError {
    /// `uring_ring_size` must be a power of two.
    RingSizeNotPowerOfTwo,
    /// `uring_buf_ring_entries` must be a power of two.
    BufRingEntriesNotPowerOfTwo,
    /// `uring_buf_ring_entries` must be <= 32768.
    BufRingEntriesTooLarge,
    /// `uring_buf_ring_entries` must be > 0.
    BufRingEntriesZero,
    /// `uring_send_slots` must be > 0.
    SendSlotsZero,
    /// `mtu_length` must be in [64, MAX_RECV_BUFFER].
    MtuOutOfRange,
    /// `heartbeat_interval_ns` must be > 0.
    HeartbeatIntervalZero,
    /// `sm_interval_ns` must be >= 0.
    SmIntervalNegative,
    /// `nak_delay_ns` must be > 0.
    NakDelayZero,
    /// `timer_interval_ns` must be > 0.
    TimerIntervalZero,
    /// `socket_rcvbuf` must be > 0.
    SocketRcvbufZero,
    /// `socket_sndbuf` must be > 0.
    SocketSndbufZero,
    /// `term_buffer_length` must be a power-of-two >= 32.
    TermBufferLengthInvalid,
    /// `retransmit_unicast_linger_ns` must be > 0.
    RetransmitLingerZero,
    /// `rttm_interval_ns` must be > 0.
    RttmIntervalZero,
    /// `max_receiver_images` must be in [1, 256].
    MaxReceiverImagesInvalid,
    /// `idle_strategy_min_park_ns` must be > 0.
    IdleMinParkZero,
    /// `idle_strategy_max_park_ns` must be >= `idle_strategy_min_park_ns`.
    IdleMaxParkLessThanMin,
}

impl std::fmt::Display for ContextValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RingSizeNotPowerOfTwo => f.write_str("uring_ring_size must be power-of-two"),
            Self::BufRingEntriesNotPowerOfTwo => {
                f.write_str("uring_buf_ring_entries must be power-of-two")
            }
            Self::BufRingEntriesTooLarge => f.write_str("uring_buf_ring_entries must be <= 32768"),
            Self::BufRingEntriesZero => f.write_str("uring_buf_ring_entries must be > 0"),
            Self::SendSlotsZero => f.write_str("uring_send_slots must be > 0"),
            Self::MtuOutOfRange => write!(f, "mtu_length must be in [64, {MAX_RECV_BUFFER}]"),
            Self::HeartbeatIntervalZero => f.write_str("heartbeat_interval_ns must be > 0"),
            Self::SmIntervalNegative => f.write_str("sm_interval_ns must be >= 0"),
            Self::NakDelayZero => f.write_str("nak_delay_ns must be > 0"),
            Self::TimerIntervalZero => f.write_str("timer_interval_ns must be > 0"),
            Self::SocketRcvbufZero => f.write_str("socket_rcvbuf must be > 0"),
            Self::SocketSndbufZero => f.write_str("socket_sndbuf must be > 0"),
            Self::TermBufferLengthInvalid => {
                f.write_str("term_buffer_length must be power-of-two >= 32")
            }
            Self::RetransmitLingerZero => f.write_str("retransmit_unicast_linger_ns must be > 0"),
            Self::RttmIntervalZero => f.write_str("rttm_interval_ns must be > 0"),
            Self::MaxReceiverImagesInvalid => {
                f.write_str("max_receiver_images must be in [1, 256]")
            }
            Self::IdleMinParkZero => f.write_str("idle_strategy_min_park_ns must be > 0"),
            Self::IdleMaxParkLessThanMin => {
                f.write_str("idle_strategy_max_park_ns must be >= min_park_ns")
            }
        }
    }
}

/// Driver-level configuration. Mirrors aeron_driver_context_t.
pub struct DriverContext {
    // ── Socket ──
    pub socket_rcvbuf: usize,
    pub socket_sndbuf: usize,
    pub mtu_length: usize,
    pub multicast_ttl: u8,

    // ── io_uring ──
    pub uring_ring_size: u32,
    pub uring_recv_slots_per_transport: usize,
    pub uring_send_slots: usize,
    /// Number of entries in the provided buffer ring for multishot RecvMsg.
    /// Must be power-of-two, max 32768. Each entry is ~65 KiB.
    pub uring_buf_ring_entries: u16,
    pub uring_sqpoll: bool,
    pub uring_sqpoll_idle_ms: u32,

    // ── Sender ──
    pub send_duty_cycle_ratio: usize,
    pub retransmit_unicast_delay_ns: i64,
    pub retransmit_unicast_linger_ns: i64,
    pub heartbeat_interval_ns: i64,
    /// Term buffer length per publication partition, in bytes.
    /// Must be power-of-two >= 32. Each publication allocates
    /// `term_buffer_length * 4` total (4 partitions, see ADR-001).
    /// Default: 64 KiB.
    pub term_buffer_length: u32,

    // ── Receiver ──
    pub sm_interval_ns: i64,
    pub nak_delay_ns: i64,
    pub rttm_interval_ns: i64,
    /// Maximum number of receiver images (active subscriptions).
    /// Bounds memory: each image allocates a RawLog (4 * term_length bytes).
    /// Must be in [1, 256]. Default: 256.
    pub max_receiver_images: usize,
    /// Override for the receiver window advertised in SM (bytes).
    /// When `None`, uses the sender's `term_length / 2` (Aeron C default).
    /// Set explicitly to cover larger batches or reduce SM round-trips.
    pub receiver_window: Option<i32>,
    /// When `true`, the receiver queues an SM immediately after processing
    /// data frames (in addition to the timer-based path). Mirrors Aeron C
    /// `SEND_SM_ON_DATA` behaviour. Default: `false`.
    pub send_sm_on_data: bool,

    // ── General ──
    pub driver_timeout_ns: i64,
    pub timer_interval_ns: i64,

    // ── Idle strategy ──
    /// Maximum spin iterations before yielding. Default: 10.
    pub idle_strategy_max_spins: u64,
    /// Maximum yield iterations before parking. Default: 5.
    pub idle_strategy_max_yields: u64,
    /// Minimum park duration in nanoseconds. Default: 1_000 (1 us).
    pub idle_strategy_min_park_ns: u64,
    /// Maximum park duration in nanoseconds. Default: 1_000_000 (1 ms).
    /// Must be >= min_park_ns.
    pub idle_strategy_max_park_ns: u64,
}

impl Default for DriverContext {
    fn default() -> Self {
        Self {
            socket_rcvbuf: 2 * 1024 * 1024,
            socket_sndbuf: 2 * 1024 * 1024,
            mtu_length: 1408,
            multicast_ttl: 0,

            uring_ring_size: 512,
            uring_recv_slots_per_transport: 16,
            uring_send_slots: 128,
            uring_buf_ring_entries: 256,
            uring_sqpoll: false,
            uring_sqpoll_idle_ms: 1000,

            send_duty_cycle_ratio: 4,
            retransmit_unicast_delay_ns: 0,
            retransmit_unicast_linger_ns: Duration::from_millis(60).as_nanos() as i64,
            heartbeat_interval_ns: Duration::from_millis(100).as_nanos() as i64,
            term_buffer_length: 64 * 1024, // 64 KiB

            sm_interval_ns: Duration::from_millis(200).as_nanos() as i64,
            nak_delay_ns: Duration::from_millis(60).as_nanos() as i64,
            rttm_interval_ns: Duration::from_secs(1).as_nanos() as i64,
            max_receiver_images: 256,
            receiver_window: None,
            send_sm_on_data: false,

            driver_timeout_ns: Duration::from_secs(10).as_nanos() as i64,
            timer_interval_ns: Duration::from_millis(1).as_nanos() as i64,

            idle_strategy_max_spins: 10,
            idle_strategy_max_yields: 5,
            idle_strategy_min_park_ns: 1_000,     // 1 us
            idle_strategy_max_park_ns: 1_000_000, // 1 ms
        }
    }
}

impl DriverContext {
    /// Validate all configuration invariants.
    ///
    /// Returns the first violated constraint, or `Ok(())` if all checks pass.
    /// Should be called at the top of `UringTransportPoller::new`,
    /// `SenderAgent::new`, and `ReceiverAgent::new`.
    pub fn validate(&self) -> Result<(), ContextValidationError> {
        if self.uring_ring_size == 0 || !self.uring_ring_size.is_power_of_two() {
            return Err(ContextValidationError::RingSizeNotPowerOfTwo);
        }
        if self.uring_buf_ring_entries == 0 {
            return Err(ContextValidationError::BufRingEntriesZero);
        }
        if !self.uring_buf_ring_entries.is_power_of_two() {
            return Err(ContextValidationError::BufRingEntriesNotPowerOfTwo);
        }
        if self.uring_buf_ring_entries > 32768 {
            return Err(ContextValidationError::BufRingEntriesTooLarge);
        }
        if self.uring_send_slots == 0 {
            return Err(ContextValidationError::SendSlotsZero);
        }
        if self.mtu_length < 64 || self.mtu_length > MAX_RECV_BUFFER {
            return Err(ContextValidationError::MtuOutOfRange);
        }
        if self.heartbeat_interval_ns <= 0 {
            return Err(ContextValidationError::HeartbeatIntervalZero);
        }
        if self.sm_interval_ns < 0 {
            return Err(ContextValidationError::SmIntervalNegative);
        }
        if self.nak_delay_ns <= 0 {
            return Err(ContextValidationError::NakDelayZero);
        }
        if self.timer_interval_ns <= 0 {
            return Err(ContextValidationError::TimerIntervalZero);
        }
        if self.socket_rcvbuf == 0 {
            return Err(ContextValidationError::SocketRcvbufZero);
        }
        if self.socket_sndbuf == 0 {
            return Err(ContextValidationError::SocketSndbufZero);
        }
        if self.term_buffer_length < 32 || !self.term_buffer_length.is_power_of_two() {
            return Err(ContextValidationError::TermBufferLengthInvalid);
        }
        if self.retransmit_unicast_linger_ns <= 0 {
            return Err(ContextValidationError::RetransmitLingerZero);
        }
        if self.rttm_interval_ns <= 0 {
            return Err(ContextValidationError::RttmIntervalZero);
        }
        if self.max_receiver_images == 0 || self.max_receiver_images > 256 {
            return Err(ContextValidationError::MaxReceiverImagesInvalid);
        }
        if self.idle_strategy_min_park_ns == 0 {
            return Err(ContextValidationError::IdleMinParkZero);
        }
        if self.idle_strategy_max_park_ns < self.idle_strategy_min_park_ns {
            return Err(ContextValidationError::IdleMaxParkLessThanMin);
        }
        Ok(())
    }

    /// Build an `IdleStrategy::Backoff` from the configured parameters.
    pub fn idle_strategy(&self) -> crate::agent::idle_strategy::IdleStrategy {
        crate::agent::idle_strategy::IdleStrategy::Backoff {
            max_spins: self.idle_strategy_max_spins,
            max_yields: self.idle_strategy_max_yields,
            min_park_ns: self.idle_strategy_min_park_ns,
            max_park_ns: self.idle_strategy_max_park_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;

    #[test]
    fn default_socket_buffers() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.socket_rcvbuf, 2 * 1024 * 1024);
        assert_eq!(ctx.socket_sndbuf, 2 * 1024 * 1024);
    }

    #[test]
    fn default_mtu() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.mtu_length, 1408);
    }

    #[test]
    fn default_uring_params() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.uring_ring_size, 512);
        assert_eq!(ctx.uring_recv_slots_per_transport, 16);
        assert_eq!(ctx.uring_send_slots, 128);
        assert_eq!(ctx.uring_buf_ring_entries, 256);
        assert!(!ctx.uring_sqpoll);
        assert_eq!(ctx.uring_sqpoll_idle_ms, 1000);
    }

    #[test]
    fn default_sender_params() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.send_duty_cycle_ratio, 4);
        assert_eq!(
            ctx.heartbeat_interval_ns,
            Duration::from_millis(100).as_nanos() as i64
        );
        assert_eq!(ctx.term_buffer_length, 64 * 1024);
        assert_eq!(ctx.retransmit_unicast_delay_ns, 0);
        assert_eq!(
            ctx.retransmit_unicast_linger_ns,
            Duration::from_millis(60).as_nanos() as i64,
        );
    }

    #[test]
    fn default_receiver_params() {
        let ctx = DriverContext::default();
        assert_eq!(
            ctx.sm_interval_ns,
            Duration::from_millis(200).as_nanos() as i64
        );
        assert_eq!(
            ctx.nak_delay_ns,
            Duration::from_millis(60).as_nanos() as i64
        );
        assert_eq!(
            ctx.rttm_interval_ns,
            Duration::from_secs(1).as_nanos() as i64
        );
        assert_eq!(ctx.max_receiver_images, 256);
    }

    #[test]
    fn default_general_params() {
        let ctx = DriverContext::default();
        assert_eq!(
            ctx.driver_timeout_ns,
            Duration::from_secs(10).as_nanos() as i64
        );
        assert_eq!(
            ctx.timer_interval_ns,
            Duration::from_millis(1).as_nanos() as i64
        );
    }

    // ── Validation tests ──

    #[test]
    fn default_context_validates() {
        let ctx = DriverContext::default();
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validate_ring_size_not_power_of_two() {
        let mut ctx = DriverContext::default();
        ctx.uring_ring_size = 100;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RingSizeNotPowerOfTwo)
        );
    }

    #[test]
    fn validate_ring_size_zero() {
        let mut ctx = DriverContext::default();
        ctx.uring_ring_size = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RingSizeNotPowerOfTwo)
        );
    }

    #[test]
    fn validate_buf_ring_entries_not_power_of_two() {
        let mut ctx = DriverContext::default();
        ctx.uring_buf_ring_entries = 100;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::BufRingEntriesNotPowerOfTwo)
        );
    }

    #[test]
    fn validate_buf_ring_entries_zero() {
        let mut ctx = DriverContext::default();
        ctx.uring_buf_ring_entries = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::BufRingEntriesZero)
        );
    }

    #[test]
    fn validate_buf_ring_entries_too_large() {
        let mut ctx = DriverContext::default();
        ctx.uring_buf_ring_entries = 65535; // not power-of-two
        assert!(ctx.validate().is_err());
        // 32768 is the max valid value (power-of-two, <= 32768).
        ctx.uring_buf_ring_entries = 32768;
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validate_send_slots_zero() {
        let mut ctx = DriverContext::default();
        ctx.uring_send_slots = 0;
        assert_eq!(ctx.validate(), Err(ContextValidationError::SendSlotsZero));
    }

    #[test]
    fn validate_mtu_too_small() {
        let mut ctx = DriverContext::default();
        ctx.mtu_length = 32;
        assert_eq!(ctx.validate(), Err(ContextValidationError::MtuOutOfRange));
    }

    #[test]
    fn validate_mtu_too_large() {
        let mut ctx = DriverContext::default();
        ctx.mtu_length = MAX_RECV_BUFFER + 1;
        assert_eq!(ctx.validate(), Err(ContextValidationError::MtuOutOfRange));
    }

    #[test]
    fn validate_heartbeat_interval_zero() {
        let mut ctx = DriverContext::default();
        ctx.heartbeat_interval_ns = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::HeartbeatIntervalZero)
        );
    }

    #[test]
    fn validate_sm_interval_zero_is_ok() {
        let mut ctx = DriverContext::default();
        ctx.sm_interval_ns = 0;
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validate_sm_interval_negative() {
        let mut ctx = DriverContext::default();
        ctx.sm_interval_ns = -1;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::SmIntervalNegative)
        );
    }

    #[test]
    fn validate_nak_delay_zero() {
        let mut ctx = DriverContext::default();
        ctx.nak_delay_ns = 0;
        assert_eq!(ctx.validate(), Err(ContextValidationError::NakDelayZero));
    }

    #[test]
    fn validate_timer_interval_zero() {
        let mut ctx = DriverContext::default();
        ctx.timer_interval_ns = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::TimerIntervalZero)
        );
    }

    #[test]
    fn validate_socket_rcvbuf_zero() {
        let mut ctx = DriverContext::default();
        ctx.socket_rcvbuf = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::SocketRcvbufZero)
        );
    }

    #[test]
    fn validate_socket_sndbuf_zero() {
        let mut ctx = DriverContext::default();
        ctx.socket_sndbuf = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::SocketSndbufZero)
        );
    }

    #[test]
    fn validate_term_buffer_length_zero() {
        let mut ctx = DriverContext::default();
        ctx.term_buffer_length = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::TermBufferLengthInvalid)
        );
    }

    #[test]
    fn validate_term_buffer_length_not_power_of_two() {
        let mut ctx = DriverContext::default();
        ctx.term_buffer_length = 100;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::TermBufferLengthInvalid)
        );
    }

    #[test]
    fn validate_term_buffer_length_too_small() {
        let mut ctx = DriverContext::default();
        // 16 < 32 (FRAME_ALIGNMENT)
        ctx.term_buffer_length = 16;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::TermBufferLengthInvalid)
        );
    }

    #[test]
    fn validate_term_buffer_length_min_valid() {
        let mut ctx = DriverContext::default();
        ctx.term_buffer_length = 32; // exactly FRAME_ALIGNMENT, power-of-two
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validate_term_buffer_length_large_valid() {
        let mut ctx = DriverContext::default();
        ctx.term_buffer_length = 16 * 1024 * 1024; // 16 MiB
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validate_retransmit_linger_zero() {
        let mut ctx = DriverContext::default();
        ctx.retransmit_unicast_linger_ns = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RetransmitLingerZero),
        );
    }

    #[test]
    fn validate_retransmit_linger_negative() {
        let mut ctx = DriverContext::default();
        ctx.retransmit_unicast_linger_ns = -1;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RetransmitLingerZero),
        );
    }

    #[test]
    fn validate_rttm_interval_zero() {
        let mut ctx = DriverContext::default();
        ctx.rttm_interval_ns = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RttmIntervalZero)
        );
    }

    #[test]
    fn validate_rttm_interval_negative() {
        let mut ctx = DriverContext::default();
        ctx.rttm_interval_ns = -1;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::RttmIntervalZero)
        );
    }

    #[test]
    fn validate_max_receiver_images_zero() {
        let mut ctx = DriverContext::default();
        ctx.max_receiver_images = 0;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::MaxReceiverImagesInvalid),
        );
    }

    #[test]
    fn validate_max_receiver_images_too_large() {
        let mut ctx = DriverContext::default();
        ctx.max_receiver_images = 257;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::MaxReceiverImagesInvalid),
        );
    }

    #[test]
    fn validate_max_receiver_images_boundary() {
        let mut ctx = DriverContext::default();
        ctx.max_receiver_images = 1;
        assert!(ctx.validate().is_ok());
        ctx.max_receiver_images = 256;
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn validation_error_display() {
        let e = ContextValidationError::RingSizeNotPowerOfTwo;
        let s = e.to_string();
        assert!(s.contains("power-of-two"));
    }

    // ── Idle strategy validation ──

    #[test]
    fn default_idle_strategy_params() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.idle_strategy_max_spins, 10);
        assert_eq!(ctx.idle_strategy_max_yields, 5);
        assert_eq!(ctx.idle_strategy_min_park_ns, 1_000);
        assert_eq!(ctx.idle_strategy_max_park_ns, 1_000_000);
    }

    #[test]
    fn validate_idle_min_park_zero() {
        let mut ctx = DriverContext::default();
        ctx.idle_strategy_min_park_ns = 0;
        assert_eq!(ctx.validate(), Err(ContextValidationError::IdleMinParkZero));
    }

    #[test]
    fn validate_idle_max_park_less_than_min() {
        let mut ctx = DriverContext::default();
        ctx.idle_strategy_min_park_ns = 1000;
        ctx.idle_strategy_max_park_ns = 500;
        assert_eq!(
            ctx.validate(),
            Err(ContextValidationError::IdleMaxParkLessThanMin)
        );
    }

    #[test]
    fn validate_idle_max_park_equal_to_min_ok() {
        let mut ctx = DriverContext::default();
        ctx.idle_strategy_min_park_ns = 1000;
        ctx.idle_strategy_max_park_ns = 1000;
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn idle_strategy_from_context() {
        let ctx = DriverContext::default();
        let strategy = ctx.idle_strategy();
        match strategy {
            crate::agent::idle_strategy::IdleStrategy::Backoff {
                max_spins,
                max_yields,
                min_park_ns,
                max_park_ns,
            } => {
                assert_eq!(max_spins, 10);
                assert_eq!(max_yields, 5);
                assert_eq!(min_park_ns, 1_000);
                assert_eq!(max_park_ns, 1_000_000);
            }
            _ => panic!("expected Backoff variant"),
        }
    }
}
