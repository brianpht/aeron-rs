// Integration tests: adaptive SM (send_sm_on_data) and duty cycle gating.
//
// Validates that:
// 1. When send_sm_on_data = true, the receiver queues SM immediately on
//    data receipt (not waiting for the timer-based SM interval).
// 2. The sender's duty_cycle_counter / send_duty_cycle_ratio correctly
//    gates poll_control behavior.
//
// These paths are critical for deterministic single-RTT completion in
// the e2e_latency benchmark.
//
// Requires Linux with io_uring support (kernel >= 5.6).

#[cfg(target_os = "linux")]
mod adaptive_sm {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use aeron_rs::agent::receiver::ReceiverAgent;
    use aeron_rs::agent::sender::SenderAgent;
    use aeron_rs::agent::Agent;
    use aeron_rs::context::DriverContext;
    use aeron_rs::media::channel::UdpChannel;
    use aeron_rs::media::network_publication::OfferError;
    use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
    use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
    use aeron_rs::media::transport::UdpChannelTransport;

    const SESSION_ID: i32 = 0x55;
    const STREAM_ID: i32 = 40;
    const INITIAL_TERM_ID: i32 = 0;
    const TERM_LENGTH: u32 = 256 * 1024;
    const MTU: u32 = 1408;
    const WARMUP_DURATION: Duration = Duration::from_millis(200);
    const MAX_SPIN_CYCLES: u32 = 10_000;

    /// Build agents with configurable send_sm_on_data and duty_cycle_ratio.
    fn setup_agents_with(
        send_sm_on_data: bool,
        send_duty_cycle_ratio: usize,
    ) -> (SenderAgent, ReceiverAgent, usize) {
        let ctx = DriverContext {
            uring_ring_size: 256,
            uring_recv_slots_per_transport: 8,
            uring_send_slots: 128,
            uring_buf_ring_entries: 64,
            term_buffer_length: TERM_LENGTH,
            send_duty_cycle_ratio,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            // Use a large sm_interval so timer-based SM effectively never fires
            // during the test window (only adaptive path is available).
            sm_interval_ns: if send_sm_on_data {
                Duration::from_secs(60).as_nanos() as i64
            } else {
                0 // timer fires every cycle when adaptive is disabled
            },
            send_sm_on_data,
            receiver_window: Some(TERM_LENGTH as i32 * 4),
            ..DriverContext::default()
        };

        let recv_channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0")
            .expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
        let remote = recv_channel.remote_data;
        let recv_transport =
            UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
                .expect("open recv transport");
        let recv_port = recv_transport.bound_addr.port();

        let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
        let mut receiver = ReceiverAgent::new(&ctx).expect("receiver agent");
        receiver.add_endpoint(recv_ep).expect("add recv endpoint");

        let send_uri = format!("aeron:udp?endpoint=127.0.0.1:{recv_port}");
        let send_channel = UdpChannel::parse(&send_uri).expect("parse send channel");
        let send_remote = send_channel.remote_data;
        let mut send_transport =
            UdpChannelTransport::open(&send_channel, &local, &send_remote, &ctx)
                .expect("open send transport");
        send_transport
            .connect(&send_remote)
            .expect("connect send transport");

        let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender agent");
        sender.add_endpoint(send_ep).expect("add send endpoint");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, TERM_LENGTH, MTU)
            .expect("add publication");

        // Warmup: complete Setup/SM handshake.
        // Use a short sm_interval temporarily by spinning for enough time
        // that even timer-based SM fires during warmup.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        (sender, receiver, pub_idx)
    }

    /// With send_sm_on_data = true and a 60s SM timer, the SM should still
    /// be sent immediately after data receipt via the adaptive path.
    /// The RTT should complete in a small number of cycles.
    #[test]
    fn adaptive_sm_enables_fast_rtt() {
        let (mut sender, mut receiver, pub_idx) =
            setup_agents_with(true, 1);
        let payload = [0xABu8; 64];

        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        // Offer one frame.
        if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(&payload) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {
                    if let Some(p) = sender.publication_mut(pub_idx) {
                        let _ = p.offer(&payload);
                    }
                }
                Err(_) => {}
            }
        }

        // The adaptive path should deliver SM within a few cycles.
        let mut completed = false;
        for cycle in 0..50u32 {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                completed = true;
                assert!(
                    cycle < 20,
                    "adaptive SM should complete RTT quickly, took {cycle} cycles"
                );
                break;
            }
        }
        assert!(
            completed,
            "RTT should complete with adaptive SM even with 60s timer"
        );
    }

    /// With send_sm_on_data = false and sm_interval = 0 (timer fires every
    /// cycle), RTT should still complete but we verify the timer path works.
    #[test]
    fn timer_based_sm_still_works() {
        let (mut sender, mut receiver, pub_idx) =
            setup_agents_with(false, 1);
        let payload = [0xABu8; 64];

        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(&payload) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {
                    if let Some(p) = sender.publication_mut(pub_idx) {
                        let _ = p.offer(&payload);
                    }
                }
                Err(_) => {}
            }
        }

        let mut completed = false;
        for _ in 0..MAX_SPIN_CYCLES {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "timer-based SM should still complete RTT"
        );
    }

    /// With send_duty_cycle_ratio = 1, the sender polls control on every
    /// duty cycle (even when bytes_sent > 0). This means the SM CQE can
    /// be reaped in the same cycle that data was sent, enabling fast RTT.
    #[test]
    fn duty_cycle_ratio_1_enables_fast_control_poll() {
        let (mut sender, mut receiver, pub_idx) =
            setup_agents_with(true, 1);
        let payload = [0xABu8; 64];

        // Do a few warmup RTTs.
        for _ in 0..3 {
            let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            if let Some(pub_ref) = sender.publication_mut(pub_idx) {
                let _ = pub_ref.offer(&payload);
            }
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();
                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                if new_limit.wrapping_sub(old_limit) > 0 {
                    break;
                }
            }
        }

        // Now measure: with ratio=1, RTT should be very fast.
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        if let Some(pub_ref) = sender.publication_mut(pub_idx) {
            let _ = pub_ref.offer(&payload);
        }
        let mut cycle_count = 0u32;
        for _ in 0..100 {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();
            cycle_count += 1;

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                break;
            }
        }
        assert!(
            cycle_count <= 10,
            "with duty_cycle_ratio=1, RTT should complete in few cycles, got {cycle_count}"
        );
    }

    /// With a high duty_cycle_ratio, the sender skips control polling on
    /// data-sending cycles. This means the SM takes more cycles to be reaped.
    /// Verify RTT still completes (just takes more cycles).
    #[test]
    fn duty_cycle_ratio_high_delays_but_completes() {
        let ctx = DriverContext {
            uring_ring_size: 256,
            uring_recv_slots_per_transport: 8,
            uring_send_slots: 128,
            uring_buf_ring_entries: 64,
            term_buffer_length: TERM_LENGTH,
            // High ratio: only poll control after 10 data-sending cycles.
            send_duty_cycle_ratio: 10,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            sm_interval_ns: 0,
            send_sm_on_data: true,
            receiver_window: Some(TERM_LENGTH as i32 * 4),
            ..DriverContext::default()
        };

        let recv_channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0")
            .expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
        let remote = recv_channel.remote_data;
        let recv_transport =
            UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
                .expect("open recv transport");
        let recv_port = recv_transport.bound_addr.port();

        let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
        let mut receiver = ReceiverAgent::new(&ctx).expect("receiver agent");
        receiver.add_endpoint(recv_ep).expect("add recv endpoint");

        let send_uri = format!("aeron:udp?endpoint=127.0.0.1:{recv_port}");
        let send_channel = UdpChannel::parse(&send_uri).expect("parse send channel");
        let send_remote = send_channel.remote_data;
        let mut send_transport =
            UdpChannelTransport::open(&send_channel, &local, &send_remote, &ctx)
                .expect("open send transport");
        send_transport
            .connect(&send_remote)
            .expect("connect send transport");

        let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender agent");
        sender.add_endpoint(send_ep).expect("add send endpoint");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, TERM_LENGTH, MTU)
            .expect("add publication");

        // Warmup.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        let payload = [0xABu8; 64];
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        if let Some(pub_ref) = sender.publication_mut(pub_idx) {
            let _ = pub_ref.offer(&payload);
        }

        let mut completed = false;
        for _ in 0..MAX_SPIN_CYCLES {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "RTT should eventually complete even with high duty_cycle_ratio"
        );
    }
}

