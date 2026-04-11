// Integration tests: RTT completion pattern used by e2e_latency benchmark.
//
// Validates the exact offer -> sender.do_work -> receiver.do_work ->
// sender.do_work -> sender_limit advance cycle. Each test uses real
// io_uring and UDP loopback, mirroring the benchmark setup.
//
// These tests exist to prevent regressions in the single_msg_rtt
// benchmark by catching RTT stalls, SM delivery failures, and
// flow control misconfigurations early in CI.
//
// Requires Linux with io_uring support (kernel >= 5.6).

#[cfg(target_os = "linux")]
mod rtt_completion {
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

    const SESSION_ID: i32 = 0x44;
    const STREAM_ID: i32 = 30;
    const INITIAL_TERM_ID: i32 = 0;
    const BENCH_TERM_LENGTH: u32 = 256 * 1024;
    const BENCH_MTU: u32 = 1408;
    const PAYLOAD_SIZE: usize = BENCH_MTU as usize - DATA_HEADER_LENGTH;
    const WARMUP_DURATION: Duration = Duration::from_millis(200);
    /// Safety valve - RTT should complete well within this many spin cycles.
    const MAX_SPIN_CYCLES: u32 = 10_000;

    /// Build a DriverContext tuned for deterministic RTT measurement.
    ///
    /// Key settings:
    /// - send_sm_on_data = true: SM queued immediately on data receipt
    /// - sm_interval_ns = 0: SM eligible every send_control_messages call
    /// - send_duty_cycle_ratio = 1: poll control every duty cycle
    /// - receiver_window = full 4-partition buffer
    fn make_bench_ctx() -> DriverContext {
        DriverContext {
            uring_ring_size: 256,
            uring_recv_slots_per_transport: 8,
            uring_send_slots: 128,
            uring_buf_ring_entries: 64,
            term_buffer_length: BENCH_TERM_LENGTH,
            send_duty_cycle_ratio: 1,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            sm_interval_ns: 0,
            send_sm_on_data: true,
            receiver_window: Some(BENCH_TERM_LENGTH as i32 * 4),
            ..DriverContext::default()
        }
    }

    /// Set up a SenderAgent and ReceiverAgent wired on UDP loopback.
    /// Returns (sender, receiver, pub_idx) with completed Setup/SM handshake.
    fn setup_agents() -> (SenderAgent, ReceiverAgent, usize) {
        setup_agents_with_ctx(make_bench_ctx())
    }

    fn setup_agents_with_ctx(ctx: DriverContext) -> (SenderAgent, ReceiverAgent, usize) {
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
            .add_publication(
                0,
                SESSION_ID,
                STREAM_ID,
                INITIAL_TERM_ID,
                ctx.term_buffer_length,
                BENCH_MTU,
            )
            .expect("add publication");

        // Warmup: spin both agents until Setup/SM handshake completes.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        (sender, receiver, pub_idx)
    }

    /// Like `setup_agents_with_ctx` but allows overriding initial_term_id.
    /// Used to test term_id wrapping near i32::MAX.
    fn setup_agents_full(
        ctx: DriverContext,
        initial_term_id: i32,
    ) -> (SenderAgent, ReceiverAgent, usize) {
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
            .add_publication(
                0,
                SESSION_ID,
                STREAM_ID,
                initial_term_id,
                ctx.term_buffer_length,
                BENCH_MTU,
            )
            .expect("add publication");

        // Warmup: spin both agents until Setup/SM handshake completes.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        (sender, receiver, pub_idx)
    }

    /// Execute one full RTT: offer -> send -> recv -> SM -> reap.
    /// Returns the number of spin cycles needed, or None if timed out.
    fn run_one_rtt(
        sender: &mut SenderAgent,
        receiver: &mut ReceiverAgent,
        pub_idx: usize,
        payload: &[u8],
    ) -> Option<u32> {
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        // Offer one frame.
        if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(payload) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {
                    // Term rotation - retry once.
                    if let Some(p) = sender.publication_mut(pub_idx) {
                        let _ = p.offer(payload);
                    }
                }
                Err(_) => {}
            }
        }

        // Spin: sender -> receiver -> sender until sender_limit advances.
        let mut cycles = 0u32;
        loop {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();
            cycles = cycles.wrapping_add(1);

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                return Some(cycles);
            }
            if cycles >= MAX_SPIN_CYCLES {
                return None; // timed out
            }
        }
    }

    // -- Tests --

    /// Verify that a single RTT with a full-size payload (1408B - header)
    /// completes within the spin budget. This is the exact pattern used
    /// by bench_e2e_latency::single_msg_rtt.
    #[test]
    fn single_rtt_completes() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        let cycles = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
        assert!(
            cycles.is_some(),
            "RTT should complete within {MAX_SPIN_CYCLES} cycles"
        );
        let c = cycles.unwrap();
        assert!(
            c < 100,
            "RTT should complete in a reasonable number of cycles, got {c}"
        );
    }

    /// Verify RTT with a header-only frame (0-byte payload) completes.
    /// This isolates protocol overhead from payload copy cost - the path
    /// exercised by bench_e2e_latency_minimal.
    #[test]
    fn single_rtt_header_only() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload: [u8; 0] = [];

        let cycles = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
        assert!(
            cycles.is_some(),
            "header-only RTT should complete within {MAX_SPIN_CYCLES} cycles"
        );
    }

    /// Verify RTT still completes when the offer triggers a term rotation
    /// (AdminAction). The run_one_rtt pattern retries after AdminAction;
    /// the RTT must still finish.
    #[test]
    fn rtt_term_boundary_retry() {
        // Use a very small term so we hit AdminAction quickly.
        let ctx = DriverContext {
            term_buffer_length: 1024, // small term
            receiver_window: Some(1024 * 4),
            ..make_bench_ctx()
        };
        let (mut sender, mut receiver, pub_idx) = setup_agents_with_ctx(ctx);
        // Payload that fits in a small term.
        let payload = [0xCCu8; 4];

        // Offer until we trigger AdminAction at least once.
        // With term_length=1024, about 16 header+payload frames fill a term.
        let mut hit_admin_action = false;
        for _ in 0..50 {
            let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

            if let Some(publication) = sender.publication_mut(pub_idx) {
                match publication.offer(&payload) {
                    Err(OfferError::AdminAction) => {
                        hit_admin_action = true;
                        // Retry the offer in the new term.
                        if let Some(p) = sender.publication_mut(pub_idx) {
                            let _ = p.offer(&payload);
                        }
                    }
                    _ => {}
                }
            }

            // Complete the RTT.
            let mut done = false;
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();

                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                let diff = new_limit.wrapping_sub(old_limit);
                if diff > 0 && diff < (i64::MAX >> 1) {
                    done = true;
                    break;
                }
            }
            assert!(done, "RTT should complete even near term boundary");

            if hit_admin_action {
                break;
            }
        }

        assert!(
            hit_admin_action,
            "test should have triggered at least one term rotation"
        );
    }

    /// Verify that 100 sequential RTTs all complete without stalling.
    /// Catches drift or regression that only manifests after sustained traffic.
    #[test]
    fn sustained_100_rtts_no_stall() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        for i in 0..100 {
            let cycles = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            assert!(
                cycles.is_some(),
                "RTT #{i} should complete within {MAX_SPIN_CYCLES} cycles"
            );
        }
    }

    /// After each RTT, sender_limit must be strictly greater than before
    /// (wrapping half-range). This confirms the SM carries fresh consumption
    /// position data and sender_limit only advances forward.
    #[test]
    fn sender_limit_advances_per_rtt() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; 64]; // small payload to avoid filling term

        let mut prev_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        for i in 0..10 {
            let cycles = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
            assert!(cycles.is_some(), "RTT #{i} should complete");

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(prev_limit);
            assert!(
                diff > 0 && diff < (i64::MAX >> 1),
                "sender_limit should advance after RTT #{i}: prev={prev_limit}, new={new_limit}, diff={diff}"
            );
            prev_limit = new_limit;
        }
    }

    /// After warmup, the handshake should be complete: receiver has an image,
    /// sender no longer needs setup, and sender_limit is positive.
    #[test]
    fn warmup_completes_handshake() {
        let (sender, receiver, pub_idx) = setup_agents();

        assert_eq!(
            receiver.image_count(),
            1,
            "receiver should have 1 image after warmup"
        );
        assert!(
            receiver.has_image(SESSION_ID, STREAM_ID),
            "image should match session/stream"
        );
        assert_eq!(
            sender.publication_needs_setup(pub_idx),
            Some(false),
            "setup should be complete after warmup"
        );
        let limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        assert!(
            limit > 0,
            "sender_limit should be positive after warmup, got {limit}"
        );
    }

    /// Verify the 3-call spin pattern (sender -> receiver -> sender) is
    /// sufficient for a single RTT to complete in 1-2 cycles.
    /// If the pattern is broken (e.g., missing the second sender.do_work),
    /// RTTs would need many more cycles.
    #[test]
    fn three_call_pattern_efficient() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xABu8; PAYLOAD_SIZE];

        // Warm up the data path with a few RTTs.
        for _ in 0..5 {
            let _ = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
        }

        // Now measure: the 3-call pattern should complete in <= 5 cycles.
        let mut total_cycles = 0u32;
        let count = 10;
        for _ in 0..count {
            match run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload) {
                Some(c) => total_cycles += c,
                None => panic!("RTT timed out"),
            }
        }
        let avg = total_cycles / count;
        assert!(
            avg <= 10,
            "average cycles per RTT should be small with 3-call pattern, got {avg}"
        );
    }

    /// Verify RTT completes with a small payload (4 bytes).
    /// Exercises a different frame alignment path than the full MTU payload.
    #[test]
    fn single_rtt_small_payload() {
        let (mut sender, mut receiver, pub_idx) = setup_agents();
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];

        let cycles = run_one_rtt(&mut sender, &mut receiver, pub_idx, &payload);
        assert!(cycles.is_some(), "small-payload RTT should complete");
    }

    // -- Edge case tests for bench publication behavior --

    /// Sustained 500 RTTs with a small term (1024 bytes), forcing 30+
    /// term rotations. Exercises the full cycle:
    ///   pad frame write -> sender_scan skip -> rotation -> clean partition
    /// This is the path that bench_e2e_latency_batch (1k_msgs_rtt_avg)
    /// traverses. With term_length=1024 and aligned frame size=64 bytes,
    /// each term holds 16 frames. 500 / 16 = 31 term rotations.
    #[test]
    fn rtt_across_multiple_term_rotations() {
        let ctx = DriverContext {
            term_buffer_length: 1024,
            receiver_window: Some(1024 * 4),
            ..make_bench_ctx()
        };
        let (mut sender, mut receiver, pub_idx) = setup_agents_with_ctx(ctx);
        let payload = [0xCCu8; 4]; // 32 hdr + 4 = 36, aligned to 64

        let mut prev_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        let mut rotation_count = 0u32;

        for i in 0..500u32 {
            let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

            // Offer one frame - track AdminAction (term rotation).
            if let Some(publication) = sender.publication_mut(pub_idx) {
                match publication.offer(&payload) {
                    Ok(_) => {}
                    Err(OfferError::AdminAction) => {
                        rotation_count += 1;
                        if let Some(p) = sender.publication_mut(pub_idx) {
                            let _ = p.offer(&payload);
                        }
                    }
                    Err(_) => {}
                }
            }

            // Spin until sender_limit advances.
            let mut done = false;
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();

                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                let diff = new_limit.wrapping_sub(old_limit);
                if diff > 0 && diff < (i64::MAX >> 1) {
                    done = true;
                    break;
                }
            }
            assert!(
                done,
                "RTT #{i} should complete (rotation_count={rotation_count})"
            );

            // sender_limit must not regress.
            let current = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let regression = prev_limit.wrapping_sub(current);
            assert!(
                regression <= 0 || regression >= (i64::MAX >> 1),
                "sender_limit regressed at RTT #{i}: prev={prev_limit}, current={current}"
            );
            prev_limit = current;
        }

        assert!(
            rotation_count >= 20,
            "expected at least 20 term rotations, got {rotation_count}"
        );
    }

    /// When offer triggers AdminAction, a pad frame is written at the
    /// unused tail of the old term. sender_scan in do_send must:
    ///   (a) advance sender_position past the pad (not get stuck)
    ///   (b) NOT transmit the pad frame over the wire
    /// Verified by: offering frames until AdminAction, running do_work
    /// to scan the old term (including pad), then completing an RTT
    /// in the new term. If the pad blocks sender_scan, the new-term
    /// RTT will timeout.
    #[test]
    fn pad_frame_advances_sender_past_term_boundary() {
        let ctx = DriverContext {
            term_buffer_length: 1024,
            receiver_window: Some(1024 * 4),
            ..make_bench_ctx()
        };
        let (mut sender, mut receiver, pub_idx) = setup_agents_with_ctx(ctx);
        let payload = [0xCCu8; 4]; // aligned frame = 64 bytes

        // Fill the first term until AdminAction (pad frame written).
        let mut hit_admin = false;
        for _ in 0..20 {
            if let Some(publication) = sender.publication_mut(pub_idx) {
                match publication.offer(&payload) {
                    Err(OfferError::AdminAction) => {
                        hit_admin = true;
                        break;
                    }
                    _ => {}
                }
            }
            // Run duty cycles so sender_scan advances sender_position
            // (prevents back-pressure from blocking offers).
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();
        }
        assert!(hit_admin, "should trigger AdminAction within 20 offers");

        // Run duty cycles to let sender_scan process the old term
        // (including the pad frame at the tail).
        for _ in 0..50 {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();
        }

        // Now offer in the new term and complete an RTT.
        // If the pad frame blocked sender_scan, sender_position would be
        // stuck and we would never advance sender_limit.
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        if let Some(p) = sender.publication_mut(pub_idx) {
            let _ = p.offer(&payload);
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
            "RTT in new term should complete after pad frame was scanned"
        );
    }

    /// RTT completion with initial_term_id near i32::MAX, forcing
    /// term_id to wrap from i32::MAX -> i32::MIN during rotation.
    /// Validates that compute_position, update_sender_limit_from_sm,
    /// and partition_index all use wrapping arithmetic correctly
    /// through the full end-to-end RTT path.
    #[test]
    fn rtt_with_wrapping_initial_term_id() {
        let ctx = DriverContext {
            term_buffer_length: 1024,
            receiver_window: Some(1024 * 4),
            ..make_bench_ctx()
        };
        // Start 2 terms before i32::MAX so we cross the boundary quickly.
        let initial_term_id = i32::MAX - 2;
        let (mut sender, mut receiver, pub_idx) = setup_agents_full(ctx, initial_term_id);
        let payload = [0xCCu8; 4]; // aligned = 64 bytes, 16 frames per term

        let mut rotation_count = 0u32;

        // 80 RTTs at 16 frames/term = 5 term rotations, crossing i32::MAX.
        for i in 0..80u32 {
            let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

            if let Some(publication) = sender.publication_mut(pub_idx) {
                match publication.offer(&payload) {
                    Ok(_) => {}
                    Err(OfferError::AdminAction) => {
                        rotation_count += 1;
                        if let Some(p) = sender.publication_mut(pub_idx) {
                            let _ = p.offer(&payload);
                        }
                    }
                    Err(_) => {}
                }
            }

            let mut done = false;
            for _ in 0..MAX_SPIN_CYCLES {
                let _ = sender.do_work();
                let _ = receiver.do_work();
                let _ = sender.do_work();

                let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
                let diff = new_limit.wrapping_sub(old_limit);
                if diff > 0 && diff < (i64::MAX >> 1) {
                    done = true;
                    break;
                }
            }
            assert!(
                done,
                "RTT #{i} should complete across i32 wrap (rotations={rotation_count})"
            );
        }

        // With initial_term_id = MAX-2 and 80 frames / 16 per term = 5
        // rotations, we should have crossed i32::MAX -> i32::MIN.
        assert!(
            rotation_count >= 3,
            "expected at least 3 term rotations to cross i32 wrap, got {rotation_count}"
        );
    }

    /// When run_one_rtt is called without a connected receiver, the
    /// sender_limit never advances (no SM arrives). The spin loop
    /// should exhaust its budget rather than hanging indefinitely.
    /// Validates the bench pattern's safety valve and confirms that
    /// silently swallowed errors (AdminAction retry, BackPressured)
    /// do not cause infinite loops.
    #[test]
    fn rtt_timeout_without_receiver() {
        let ctx = DriverContext {
            term_buffer_length: 1024,
            receiver_window: Some(1024 * 4),
            ..make_bench_ctx()
        };

        // Sender with transport (sends into the void).
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse channel");
        let local: SocketAddr = "0.0.0.0:0".parse().expect("parse local addr");
        let remote = channel.remote_data;
        let transport =
            UdpChannelTransport::open(&channel, &local, &remote, &ctx).expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender agent");
        sender.add_endpoint(endpoint).expect("add endpoint");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, 1024, BENCH_MTU)
            .expect("add pub");

        // Disconnected receiver (listens on a different port).
        let recv_channel =
            UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
        let recv_remote = recv_channel.remote_data;
        let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &recv_remote, &ctx)
            .expect("open recv transport");
        let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
        let mut receiver = ReceiverAgent::new(&ctx).expect("receiver agent");
        receiver.add_endpoint(recv_ep).expect("add recv endpoint");

        let payload = [0xABu8; 4];
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);

        // Offer using the bench pattern.
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

        // Spin with a small budget - sender_limit should never advance.
        let small_budget = 500u32;
        let mut advanced = false;
        for _ in 0..small_budget {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();

            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                advanced = true;
                break;
            }
        }
        assert!(
            !advanced,
            "sender_limit should NOT advance without a connected receiver"
        );
    }
}
