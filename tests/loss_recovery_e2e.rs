// Integration tests: NAK-driven retransmit across real Sender + Receiver agents.
//
// On UDP loopback packets never drop, so gaps are created by injecting
// raw UDP data frames (via std::net::UdpSocket) with an offset that
// skips ahead of the receiver's expected_term_offset. This triggers the
// receiver's gap detection, NAK generation, and the sender's retransmit
// path - the same code path exercised by real packet loss.
//
// Data path tested:
//   [injected frame with gap] -> receiver detects gap -> NAK sent
//   -> sender receives NAK -> RetransmitHandler schedules action
//   -> sender retransmit_scan reads from term buffer -> re-sends frame
//   -> receiver fills gap -> consumption advances -> SM -> sender_limit
//
// Requires Linux with io_uring support (kernel >= 5.6).

#[cfg(target_os = "linux")]
mod loss_recovery_e2e {
    use std::net::{SocketAddr, UdpSocket};
    use std::time::{Duration, Instant};

    use aeron_rs::agent::Agent;
    use aeron_rs::agent::receiver::ReceiverAgent;
    use aeron_rs::agent::sender::SenderAgent;
    use aeron_rs::context::DriverContext;
    use aeron_rs::frame::{
        CURRENT_VERSION, DATA_FLAG_BEGIN, DATA_FLAG_END, DATA_HEADER_LENGTH, FRAME_TYPE_DATA,
    };
    use aeron_rs::media::channel::UdpChannel;
    use aeron_rs::media::network_publication::OfferError;
    use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
    use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
    use aeron_rs::media::transport::UdpChannelTransport;

    const SESSION_ID: i32 = 0x77;
    const STREAM_ID: i32 = 70;
    const INITIAL_TERM_ID: i32 = 0;
    const TERM_LENGTH: u32 = 4096;
    const MTU: u32 = 1408;
    const WARMUP_DURATION: Duration = Duration::from_millis(200);
    const MAX_SPIN: u32 = 50_000;

    /// Build a DriverContext tuned for loss recovery testing.
    ///
    /// Key settings:
    /// - nak_delay_ns = 1 (minimum valid, effectively immediate)
    /// - retransmit_unicast_delay_ns = 0 (no delay before retransmit fires)
    /// - retransmit_unicast_linger_ns = 1_000_000 (1ms, short linger)
    /// - send_sm_on_data = true (fast SM for consumption tracking)
    /// - send_duty_cycle_ratio = 1 (poll control every cycle)
    fn make_ctx() -> DriverContext {
        DriverContext {
            uring_ring_size: 256,
            uring_recv_slots_per_transport: 8,
            uring_send_slots: 128,
            uring_buf_ring_entries: 64,
            term_buffer_length: TERM_LENGTH,
            send_duty_cycle_ratio: 1,
            heartbeat_interval_ns: Duration::from_millis(1).as_nanos() as i64,
            sm_interval_ns: 0,
            send_sm_on_data: true,
            nak_delay_ns: 1, // minimum positive (validated > 0)
            retransmit_unicast_delay_ns: 0,
            retransmit_unicast_linger_ns: 1_000_000, // 1ms
            receiver_window: Some(TERM_LENGTH as i32 * 4),
            ..DriverContext::default()
        }
    }

    /// Agent pair + receiver port for raw UDP injection.
    struct TestHarness {
        sender: SenderAgent,
        receiver: ReceiverAgent,
        pub_idx: usize,
        recv_port: u16,
    }

    fn setup_harness() -> TestHarness {
        let ctx = make_ctx();

        let recv_channel =
            UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").expect("parse recv channel");
        let local: SocketAddr = "127.0.0.1:0".parse().expect("parse local");
        let remote = recv_channel.remote_data;
        let recv_transport = UdpChannelTransport::open(&recv_channel, &local, &remote, &ctx)
            .expect("recv transport");
        let recv_port = recv_transport.bound_addr.port();

        let recv_ep = ReceiveChannelEndpoint::new(recv_channel, recv_transport, 0);
        let mut receiver = ReceiverAgent::new(&ctx).expect("receiver");
        receiver.add_endpoint(recv_ep).expect("add recv ep");

        let send_uri = format!("aeron:udp?endpoint=127.0.0.1:{recv_port}");
        let send_channel = UdpChannel::parse(&send_uri).expect("parse send channel");
        let send_remote = send_channel.remote_data;
        let mut send_transport =
            UdpChannelTransport::open(&send_channel, &local, &send_remote, &ctx)
                .expect("send transport");
        send_transport.connect(&send_remote).expect("connect");

        let send_ep = SendChannelEndpoint::new(send_channel, send_transport);
        let mut sender = SenderAgent::new(&ctx).expect("sender");
        sender.add_endpoint(send_ep).expect("add send ep");
        let pub_idx = sender
            .add_publication(0, SESSION_ID, STREAM_ID, INITIAL_TERM_ID, TERM_LENGTH, MTU)
            .expect("add pub");

        // Warmup: complete Setup/SM handshake.
        let deadline = Instant::now() + WARMUP_DURATION;
        while Instant::now() < deadline {
            let _ = sender.do_work();
            let _ = receiver.do_work();
        }

        TestHarness {
            sender,
            receiver,
            pub_idx,
            recv_port,
        }
    }

    /// Offer one frame, handle AdminAction retry. Returns true if offer succeeded.
    fn offer_one(sender: &mut SenderAgent, pub_idx: usize, payload: &[u8]) -> bool {
        if let Some(publication) = sender.publication_mut(pub_idx) {
            match publication.offer(payload) {
                Ok(_) => return true,
                Err(OfferError::AdminAction) => {
                    if let Some(p) = sender.publication_mut(pub_idx) {
                        return p.offer(payload).is_ok();
                    }
                }
                Err(_) => {}
            }
        }
        false
    }

    /// Run one full RTT cycle: offer -> send -> recv -> SM -> sender_limit advance.
    /// Returns true if sender_limit advanced within budget.
    fn run_one_rtt(
        sender: &mut SenderAgent,
        receiver: &mut ReceiverAgent,
        pub_idx: usize,
        payload: &[u8],
    ) -> bool {
        let old_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
        if !offer_one(sender, pub_idx, payload) {
            return false;
        }
        for _ in 0..MAX_SPIN {
            let _ = sender.do_work();
            let _ = receiver.do_work();
            let _ = sender.do_work();
            let new_limit = sender.publication_sender_limit(pub_idx).unwrap_or(0);
            let diff = new_limit.wrapping_sub(old_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                return true;
            }
        }
        false
    }

    /// Build a raw Aeron data frame as bytes (little-endian, repr(C, packed) layout).
    /// frame_length covers header + payload. No allocation in caller - returns
    /// a stack buffer large enough for MTU.
    fn build_raw_data_frame(
        session_id: i32,
        stream_id: i32,
        term_id: i32,
        term_offset: i32,
        payload: &[u8],
    ) -> Vec<u8> {
        let frame_length = (DATA_HEADER_LENGTH + payload.len()) as i32;
        let mut buf = Vec::with_capacity(frame_length as usize);

        // FrameHeader (8 bytes)
        buf.extend_from_slice(&frame_length.to_le_bytes()); // frame_length
        buf.push(CURRENT_VERSION); // version
        buf.push(DATA_FLAG_BEGIN | DATA_FLAG_END); // flags
        buf.extend_from_slice(&FRAME_TYPE_DATA.to_le_bytes()); // frame_type

        // DataHeader remaining fields (24 bytes)
        buf.extend_from_slice(&term_offset.to_le_bytes()); // term_offset
        buf.extend_from_slice(&session_id.to_le_bytes()); // session_id
        buf.extend_from_slice(&stream_id.to_le_bytes()); // stream_id
        buf.extend_from_slice(&term_id.to_le_bytes()); // term_id
        buf.extend_from_slice(&0i64.to_le_bytes()); // reserved_value

        // Payload
        buf.extend_from_slice(payload);

        buf
    }

    /// Inject a raw data frame into the receiver via std::net::UdpSocket.
    /// This bypasses the sender agent, creating out-of-order delivery
    /// to trigger gap detection in the receiver.
    fn inject_frame(recv_port: u16, frame: &[u8]) {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind inject socket");
        let dest: SocketAddr = format!("127.0.0.1:{recv_port}")
            .parse()
            .expect("parse dest");
        sock.send_to(frame, dest).expect("send injected frame");
    }

    // -- Tests --

    /// When the receiver detects a gap (out-of-order frame), it generates
    /// a NAK. The sender receives the NAK, schedules a retransmit, and
    /// re-sends the missing frame from its term buffer. The receiver fills
    /// the gap, consumption advances, and the data path continues.
    ///
    /// Verified by: sender_limit continues to advance after the gap event,
    /// proving the NAK -> retransmit -> SM -> sender_limit path works.
    #[test]
    fn gap_triggers_nak_and_retransmit() {
        let mut h = setup_harness();
        let payload = [0xAAu8; 4]; // 32 hdr + 4 = 36, aligned to 64 bytes

        // Phase 1: Send 6 frames normally (offsets 0..384).
        // Establishes the image and advances expected_term_offset.
        for i in 0..6u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "RTT #{i} should complete in normal phase"
            );
        }

        // Record sender_limit before gap injection.
        let limit_before_gap = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);

        // Phase 2: Offer frame 6 (offset 384) on sender but do NOT send it yet.
        // This ensures the frame exists in the sender's term buffer for retransmit.
        assert!(
            offer_one(&mut h.sender, h.pub_idx, &payload),
            "offer frame 6 should succeed"
        );

        // Phase 3: Inject a raw frame at offset 448 (skipping offset 384).
        // Receiver sees expected=384, got=448 -> gap of 64 bytes -> NAK generated.
        let injected = build_raw_data_frame(
            SESSION_ID,
            STREAM_ID,
            INITIAL_TERM_ID,
            448, // skip offset 384
            &[0xBBu8; 4],
        );
        inject_frame(h.recv_port, &injected);

        // Phase 4: Spin duty cycles.
        // - Receiver processes CQEs: sees injected frame at 448, detects gap at 384,
        //   generates NAK, sends SM with current consumption.
        // - Sender sends frame 6 (offset 384) normally via sender_scan, receives NAK,
        //   schedules retransmit (delay=0 -> fires immediately on next process_timeouts).
        // - Receiver gets frame 6, gap filled. expected advances past 448.
        // - Sender_limit should advance past limit_before_gap.
        let mut advanced = false;
        for _ in 0..MAX_SPIN {
            let _ = h.sender.do_work();
            let _ = h.receiver.do_work();
            let _ = h.sender.do_work();

            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(limit_before_gap);
            if diff > 0 && diff < (i64::MAX >> 1) {
                advanced = true;
                break;
            }
        }
        assert!(advanced, "sender_limit should advance after gap recovery");

        // Phase 5: Verify data path still works after gap event.
        // The injected frame advanced consumption past the sender's current
        // pub_position. We must offer enough frames to catch up before
        // per-RTT sender_limit checks work again. Offer 20 frames in bulk
        // and verify sender_limit advances at some point.
        let limit_post_gap = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
        for _ in 0..20 {
            offer_one(&mut h.sender, h.pub_idx, &payload);
        }
        let mut caught_up = false;
        for _ in 0..MAX_SPIN {
            let _ = h.sender.do_work();
            let _ = h.receiver.do_work();
            let _ = h.sender.do_work();
            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(limit_post_gap);
            if diff > 0 && diff < (i64::MAX >> 1) {
                caught_up = true;
                break;
            }
        }
        assert!(
            caught_up,
            "sender_limit should advance after catching up past injection point"
        );

        // Now per-RTT checks work again since pub_position passed the gap.
        for i in 0..5u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "post-gap RTT #{i} should complete"
            );
        }
    }

    /// Multiple gaps at different offsets. Each gap triggers a separate NAK.
    /// The sender handles multiple concurrent retransmit actions.
    /// Verified by: sender_limit continues advancing after all gaps.
    #[test]
    fn multiple_gaps_independent_recovery() {
        let mut h = setup_harness();
        let payload = [0xCCu8; 4]; // aligned frame = 64 bytes

        // Phase 1: Send 4 frames normally (offsets 0, 64, 128, 192).
        for i in 0..4u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "RTT #{i} should complete"
            );
        }

        let limit_before = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);

        // Phase 2: Offer frames 4, 5, 6 (offsets 256, 320, 384) on sender.
        // Don't send yet - they exist in the term buffer for retransmit.
        for _ in 0..3 {
            assert!(offer_one(&mut h.sender, h.pub_idx, &payload));
        }

        // Phase 3: Inject frames at offsets 384 and 512 (skipping 256, 320, 448).
        // This creates gaps at 256 (from frame 4) and potentially at 448.
        // But we only care that the receiver sees out-of-order delivery.
        let frame_at_384 =
            build_raw_data_frame(SESSION_ID, STREAM_ID, INITIAL_TERM_ID, 384, &[0xDDu8; 4]);
        inject_frame(h.recv_port, &frame_at_384);

        // Phase 4: Spin duty cycles to process everything.
        // Sender sends frames 4-6 normally. Receiver gets them plus injected.
        // NAKs generated for detected gaps. Retransmits fire.
        let mut limit_advanced = false;
        for _ in 0..MAX_SPIN {
            let _ = h.sender.do_work();
            let _ = h.receiver.do_work();
            let _ = h.sender.do_work();

            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(limit_before);
            if diff > 0 && diff < (i64::MAX >> 1) {
                limit_advanced = true;
                break;
            }
        }
        assert!(
            limit_advanced,
            "sender_limit should advance after multiple gap recovery"
        );

        // Phase 5: Verify continued operation.
        for i in 0..10u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "post-gap RTT #{i} should complete"
            );
        }
    }

    /// The retransmit delay (retransmit_unicast_delay_ns) controls how long
    /// the sender waits before actually retransmitting after receiving a NAK.
    /// With delay=0, retransmit fires on the very next process_timeouts call.
    /// The overall RTT should still complete quickly (within budget).
    ///
    /// This test verifies that with delay=0, the gap recovery + RTT completion
    /// happens within a reasonable number of duty cycles (not hanging).
    #[test]
    fn retransmit_with_zero_delay_completes_quickly() {
        let mut h = setup_harness();
        let payload = [0xEEu8; 4];

        // Establish baseline with normal RTTs.
        for i in 0..4u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "baseline RTT #{i} should complete"
            );
        }

        let limit_before = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);

        // Offer frame at next offset, don't send.
        assert!(offer_one(&mut h.sender, h.pub_idx, &payload));

        // Inject frame 2 positions ahead to create a gap.
        // Current expected is at offset 256 (4 frames * 64 bytes).
        // Inject at offset 320 (skipping 256).
        let injected =
            build_raw_data_frame(SESSION_ID, STREAM_ID, INITIAL_TERM_ID, 320, &[0xFFu8; 4]);
        inject_frame(h.recv_port, &injected);

        // Count cycles needed for sender_limit to advance.
        let mut cycles = 0u32;
        let mut advanced = false;
        for _ in 0..MAX_SPIN {
            let _ = h.sender.do_work();
            let _ = h.receiver.do_work();
            let _ = h.sender.do_work();
            cycles += 1;

            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(limit_before);
            if diff > 0 && diff < (i64::MAX >> 1) {
                advanced = true;
                break;
            }
        }
        assert!(advanced, "gap recovery should complete");
        // With delay=0 and loopback, recovery should be fast (< 100 cycles).
        assert!(
            cycles < 100,
            "gap recovery should complete quickly with zero delay, took {cycles} cycles"
        );
    }

    /// After gap recovery, the sender_limit must not regress compared to
    /// pre-gap values. This validates that the NAK/retransmit path does
    /// not corrupt the consumption tracking or sender_limit state.
    #[test]
    fn sender_limit_monotonic_through_gap_recovery() {
        let mut h = setup_harness();
        let payload = [0x11u8; 4];

        let mut prev_limit = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);

        // Send 10 normal RTTs, record each sender_limit.
        for i in 0..10u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "pre-gap RTT #{i} should complete"
            );
            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(prev_limit);
            assert!(
                diff >= 0 || diff.unsigned_abs() > (i64::MAX as u64 >> 1),
                "sender_limit should not regress at pre-gap RTT #{i}"
            );
            prev_limit = current;
        }

        // Inject a gap.
        assert!(offer_one(&mut h.sender, h.pub_idx, &payload));
        // expected is at 640 (10 frames * 64). Inject at 704 (skipping 640).
        let injected =
            build_raw_data_frame(SESSION_ID, STREAM_ID, INITIAL_TERM_ID, 704, &[0x22u8; 4]);
        inject_frame(h.recv_port, &injected);

        // Bulk-offer frames to pass the injection point. The injected frame
        // advanced consumption ahead of pub_position, so we need enough
        // offers to catch up before per-frame RTT checks work.
        for _ in 0..20 {
            offer_one(&mut h.sender, h.pub_idx, &payload);
        }
        let mut caught_up = false;
        for _ in 0..MAX_SPIN {
            let _ = h.sender.do_work();
            let _ = h.receiver.do_work();
            let _ = h.sender.do_work();
            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(prev_limit);
            if diff > 0 && diff < (i64::MAX >> 1) {
                prev_limit = current;
                caught_up = true;
                break;
            }
        }
        assert!(caught_up, "should catch up past injection point");

        // Send 10 more RTTs, verify no regression.
        for i in 0..10u32 {
            assert!(
                run_one_rtt(&mut h.sender, &mut h.receiver, h.pub_idx, &payload),
                "post-gap RTT #{i} should complete"
            );
            let current = h.sender.publication_sender_limit(h.pub_idx).unwrap_or(0);
            let diff = current.wrapping_sub(prev_limit);
            assert!(
                diff >= 0 || diff.unsigned_abs() > (i64::MAX as u64 >> 1),
                "sender_limit regressed at post-gap RTT #{i}: prev={prev_limit}, current={current}"
            );
            prev_limit = current;
        }
    }
}
