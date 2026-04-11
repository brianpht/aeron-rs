// Integration tests: end-to-end flow control behavior.
//
// Validates:
// 1. Sender stalls when no receiver (sender_limit stays at initial window).
// 2. Sender resumes after SM is received.
// 3. sender_limit only advances forward (no regression).
// 4. Multiple RTTs with consumption tracking.
//
// Requires Linux with io_uring support (kernel >= 5.6).

#[cfg(target_os = "linux")]
mod flow_control_e2e {
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use aeron_rs::agent::Agent;
    use aeron_rs::agent::receiver::ReceiverAgent;
    use aeron_rs::agent::sender::SenderAgent;
    use aeron_rs::context::DriverContext;
    use aeron_rs::frame::DATA_HEADER_LENGTH;
    use aeron_rs::media::channel::UdpChannel;
    use aeron_rs::media::network_publication::OfferError;
    use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
    use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
    use aeron_rs::media::transport::UdpChannelTransport;

    const SESSION_ID: i32 = 0x66;
    const STREAM_ID: i32 = 50;
    const INITIAL_TERM_ID: i32 = 0;
    const TERM_LENGTH: u32 = 1024;
    const MTU: u32 = 1408;
    const WARMUP_DURATION: Duration = Duration::from_millis(200);
    const MAX_SPIN_CYCLES: u32 = 10_000;

    fn make_flow_ctx() -> DriverContext {
        DriverContext {
            uring_ring_size: 64,
            uring_recv_slots_per_transport: 4,
            uring_send_slots: 16,
            uring_buf_ring_entries: 16,
            term_buffer_length: TERM_LENGTH,
            send_duty_cycle_ratio: 1,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            sm_interval_ns: 0,
            send_sm_on_data: true,
            receiver_window: Some(TERM_LENGTH as i32 * 4),
            ..DriverContext::default()
        }
    }

    /// Sender without a connected receiver. sender_limit should stay at its
    /// initial value (term_length) because no SM will arrive. The sender can
    /// still transmit data within the initial window but cannot advance beyond.
    #[test]
    fn sender_limit_stays_at_initial_without_receiver() {
        let ctx = make_flow_ctx();
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse channel");
        let local: SocketAddr = "0.0.0.0:0".parse().expect("parse local addr");
        let remote = channel.remote_data;

        let transport =
            UdpChannelTransport::open(&channel, &local, &remote, &ctx).expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender agent");
        sender.add_endpoint(endpoint).expect("add endpoint");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, TERM_LENGTH, MTU)
            .expect("add pub");

        let initial_limit = sender
            .publication_sender_limit(pub_idx)
            .expect("sender_limit available");
        assert_eq!(
            initial_limit, TERM_LENGTH as i64,
            "initial sender_limit should equal term_length"
        );

        // Offer a frame and run duty cycles - no receiver to send SM.
        if let Some(pub_ref) = sender.publication_mut(pub_idx) {
            let _ = pub_ref.offer(&[0xABu8; 4]);
        }
        for _ in 0..100 {
            let _ = sender.do_work();
        }

        let limit_after = sender
            .publication_sender_limit(pub_idx)
            .expect("sender_limit available");
        assert_eq!(
            limit_after, initial_limit,
            "sender_limit should not change without SM"
        );
    }

    /// Sender connected to receiver. After Setup/SM handshake + data offer,
    /// sender_limit should advance beyond the initial window.
    #[test]
    fn sender_limit_advances_after_sm() {
        let ctx = make_flow_ctx();

        let recv_channel =
            UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
        let remote = recv_channel.remote_data;
        let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
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
            .expect("add pub");

        // Initial limit = term_length.
        let initial_limit = sender
            .publication_sender_limit(pub_idx)
            .expect("sender_limit available");
        assert_eq!(initial_limit, TERM_LENGTH as i64);

        // Warmup to complete handshake.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        // Offer data.
        if let Some(pub_ref) = sender.publication_mut(pub_idx) {
            let _ = pub_ref.offer(&[0xABu8; 4]);
        }

        // Run cycles until SM advances sender_limit.
        let mut advanced = false;
        for _ in 0..MAX_SPIN_CYCLES {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(initial_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                advanced = true;
                break;
            }
        }
        assert!(
            advanced,
            "sender_limit should advance after SM from receiver"
        );
    }

    /// sender_limit must never regress. Send multiple SM updates (via
    /// sequential data offers) and verify limit only increases.
    #[test]
    fn sender_limit_never_regresses() {
        let ctx = make_flow_ctx();

        let recv_channel =
            UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
        let remote = recv_channel.remote_data;
        let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
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
            .expect("add pub");

        // Warmup.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        let mut prev_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        for i in 0..20 {
            // Offer a frame.
            if let Some(pub_ref) = sender.publication_mut(pub_idx) {
                match pub_ref.offer(&[i as u8; 4]) {
                    Ok(_) => {}
                    Err(OfferError::AdminAction) => {
                        if let Some(p) = sender.publication_mut(pub_idx) {
                            let _ = p.offer(&[i as u8; 4]);
                        }
                    }
                    Err(_) => {}
                }
            }

            // Spin until sender_limit advances.
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();

                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                let diff = new_limit.wrapping_sub(prev_limit);
                if diff > 0 && diff < (i64::MAX >> 1) {
                    break;
                }
            }

            let current = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let regression = prev_limit.wrapping_sub(current);
            assert!(
                regression <= 0 || regression >= (i64::MAX >> 1),
                "sender_limit regressed at iteration {i}: prev={prev_limit}, current={current}"
            );
            prev_limit = current;
        }
    }

    /// Offer enough data to exhaust the initial sender_limit window
    /// (without a receiver). Sender should hit BackPressured.
    #[test]
    fn back_pressure_when_limit_exhausted() {
        let ctx = DriverContext {
            term_buffer_length: 64, // tiny term
            ..make_flow_ctx()
        };
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse channel");
        let local: SocketAddr = "0.0.0.0:0".parse().expect("parse local addr");
        let remote = channel.remote_data;
        let transport =
            UdpChannelTransport::open(&channel, &local, &remote, &ctx).expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender agent");
        sender.add_endpoint(endpoint).expect("add endpoint");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, 64, MTU)
            .expect("add pub");

        // Fill up the term buffer until back-pressured.
        let mut got_bp = false;
        for _ in 0..100 {
            if let Some(pub_ref) = sender.publication_mut(pub_idx) {
                match pub_ref.offer(&[]) {
                    Err(OfferError::BackPressured) => {
                        got_bp = true;
                        break;
                    }
                    Err(OfferError::AdminAction) => continue,
                    _ => {}
                }
            }
        }
        assert!(
            got_bp,
            "should get BackPressured with tiny term and no receiver"
        );
    }

    /// Under the standard bench configuration (term_length=256KiB,
    /// receiver_window = term * 4, send_sm_on_data = true), offer
    /// should never return BackPressured during sustained RTT traffic.
    /// This validates that the bench's Err(_) => {} arm for
    /// BackPressured is effectively dead code under normal conditions.
    #[test]
    fn backpressure_unreachable_under_bench_conditions() {
        let bench_term: u32 = 256 * 1024;
        let bench_mtu: u32 = 1408;
        let ctx = DriverContext {
            uring_ring_size: 256,
            uring_recv_slots_per_transport: 8,
            uring_send_slots: 128,
            uring_buf_ring_entries: 64,
            term_buffer_length: bench_term,
            send_duty_cycle_ratio: 1,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            sm_interval_ns: 0,
            send_sm_on_data: true,
            receiver_window: Some(bench_term as i32 * 4),
            ..DriverContext::default()
        };

        let recv_channel =
            UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local addr");
        let remote = recv_channel.remote_data;
        let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
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
            .add_publication(0, 0x77, 60, 0, bench_term, bench_mtu)
            .expect("add pub");

        // Warmup.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        let payload_size = bench_mtu as usize - DATA_HEADER_LENGTH;
        let payload = vec![0xABu8; payload_size];
        let mut bp_count = 0u32;

        for _ in 0..1000u32 {
            let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

            if let Some(publication) = sender.publication_mut(pub_idx) {
                match publication.offer(&payload) {
                    Ok(_) => {}
                    Err(OfferError::AdminAction) => {
                        if let Some(p) = sender.publication_mut(pub_idx) {
                            match p.offer(&payload) {
                                Err(OfferError::BackPressured) => bp_count += 1,
                                _ => {}
                            }
                        }
                    }
                    Err(OfferError::BackPressured) => bp_count += 1,
                    Err(_) => {}
                }
            }

            // Complete RTT.
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();

                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                let diff = new_limit.wrapping_sub(old_limit);
                if diff > 0 && diff < (i64::MAX >> 1) {
                    break;
                }
            }
        }

        assert_eq!(
            bp_count, 0,
            "under bench conditions, offer should never return BackPressured (got {bp_count})"
        );
    }
}
