use std::time::Duration;

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
    pub heartbeat_interval_ns: i64,

    // ── Receiver ──
    pub sm_interval_ns: i64,
    pub nak_delay_ns: i64,
    pub rttm_interval_ns: i64,

    // ── General ──
    pub driver_timeout_ns: i64,
    pub timer_interval_ns: i64,
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
            heartbeat_interval_ns: Duration::from_millis(100).as_nanos() as i64,

            sm_interval_ns: Duration::from_millis(200).as_nanos() as i64,
            nak_delay_ns: Duration::from_millis(60).as_nanos() as i64,
            rttm_interval_ns: Duration::from_secs(1).as_nanos() as i64,

            driver_timeout_ns: Duration::from_secs(10).as_nanos() as i64,
            timer_interval_ns: Duration::from_millis(1).as_nanos() as i64,
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(ctx.heartbeat_interval_ns, Duration::from_millis(100).as_nanos() as i64);
    }

    #[test]
    fn default_receiver_params() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.sm_interval_ns, Duration::from_millis(200).as_nanos() as i64);
        assert_eq!(ctx.nak_delay_ns, Duration::from_millis(60).as_nanos() as i64);
        assert_eq!(ctx.rttm_interval_ns, Duration::from_secs(1).as_nanos() as i64);
    }

    #[test]
    fn default_general_params() {
        let ctx = DriverContext::default();
        assert_eq!(ctx.driver_timeout_ns, Duration::from_secs(10).as_nanos() as i64);
        assert_eq!(ctx.timer_interval_ns, Duration::from_millis(1).as_nanos() as i64);
    }
}
