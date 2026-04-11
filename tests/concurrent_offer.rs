//! Integration test: cross-thread concurrent publication.
//!
//! Verifies that ConcurrentPublication (publisher handle on one thread) and
//! SenderPublication (sender view on another thread) work correctly via
//! Release/Acquire atomics - no Mutex, no SeqCst.
//!
//! Requires Linux with io_uring support (kernel >= 5.6).

#[cfg(target_os = "linux")]
mod concurrent_offer {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use aeron_rs::media::concurrent_publication::new_concurrent;
    use aeron_rs::media::network_publication::OfferError;

    const TERM_LENGTH: u32 = 64 * 1024; // 64 KiB
    const MTU: u32 = 1408;

    // ---- Single-threaded smoke tests ----

    #[test]
    fn offer_scan_roundtrip_single_thread() {
        let (mut pub_h, mut send_h) =
            new_concurrent(42, 7, 0, TERM_LENGTH, MTU).expect("valid params");

        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        pub_h.offer(&payload).expect("offer");

        let mut found = false;
        send_h.sender_scan(u32::MAX, |_off, data| {
            assert!(data.len() >= 36); // 32 header + 4 payload
            found = true;
        });
        assert!(found, "sender_scan should find the offered frame");
        assert_eq!(send_h.sender_position(), pub_h.pub_position());
    }

    // ---- Cross-thread tests ----

    #[test]
    fn frame_commit_visible_cross_thread() {
        // Publisher on thread A, scanner on thread B.
        let (mut pub_h, mut send_h) =
            new_concurrent(99, 5, 0, TERM_LENGTH, MTU).expect("valid params");

        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);

        // Publisher thread: offer 100 messages.
        let publisher = thread::spawn(move || {
            let payload = [0xABu8; 64];
            let mut offered = 0u32;
            while offered < 100 {
                match pub_h.offer(&payload) {
                    Ok(_) => offered += 1,
                    Err(OfferError::AdminAction) => {}
                    Err(OfferError::BackPressured) => {
                        // Spin-wait for sender to catch up.
                        thread::yield_now();
                    }
                    Err(e) => panic!("unexpected offer error: {e}"),
                }
            }
            done_clone.store(true, Ordering::Release);
            offered
        });

        // Sender thread (main): scan until publisher is done and all caught up.
        let mut total_frames = 0u32;
        loop {
            let scanned = send_h.sender_scan(u32::MAX, |_, _| {
                total_frames += 1;
            });
            if scanned == 0 && done.load(Ordering::Acquire) {
                // One final scan to catch any trailing frames.
                send_h.sender_scan(u32::MAX, |_, _| {
                    total_frames += 1;
                });
                break;
            }
        }

        let offered = publisher.join().expect("publisher thread panicked");
        assert_eq!(total_frames, offered, "all offered frames must be scanned");
    }

    #[test]
    fn sustained_throughput_no_corruption() {
        // Offer 10_000 messages with checksum payload, verify no corruption.
        let (mut pub_h, mut send_h) =
            new_concurrent(42, 7, 0, TERM_LENGTH, MTU).expect("valid params");

        let msg_count = 10_000u32;
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = Arc::clone(&done);

        // Publisher: encode message index into payload for verification.
        let publisher = thread::spawn(move || {
            let mut offered = 0u32;
            while offered < msg_count {
                // Payload: 4-byte LE message index + 4-byte LE checksum (bitwise NOT).
                let idx_bytes = offered.to_le_bytes();
                let check_bytes = (!offered).to_le_bytes();
                let payload = [
                    idx_bytes[0],
                    idx_bytes[1],
                    idx_bytes[2],
                    idx_bytes[3],
                    check_bytes[0],
                    check_bytes[1],
                    check_bytes[2],
                    check_bytes[3],
                ];

                match pub_h.offer(&payload) {
                    Ok(_) => offered += 1,
                    Err(OfferError::AdminAction) => {}
                    Err(OfferError::BackPressured) => {
                        thread::yield_now();
                    }
                    Err(e) => panic!("unexpected offer error: {e}"),
                }
            }
            done_clone.store(true, Ordering::Release);
        });

        // Scanner: verify each frame's payload checksum.
        let mut next_expected = 0u32;
        loop {
            send_h.sender_scan(u32::MAX, |_, data| {
                // Skip 32-byte DataHeader, read payload.
                assert!(data.len() >= 40, "frame too short: {}", data.len());
                let payload = &data[32..40];
                let idx = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let check = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                assert_eq!(
                    check, !idx,
                    "checksum mismatch at msg {idx}: expected {}, got {check}",
                    !idx
                );
                assert_eq!(
                    idx, next_expected,
                    "out-of-order: expected {next_expected}, got {idx}"
                );
                next_expected += 1;
            });

            if next_expected >= msg_count && done.load(Ordering::Acquire) {
                break;
            }
            if next_expected < msg_count {
                thread::yield_now();
            }
        }

        publisher.join().expect("publisher thread panicked");
        assert_eq!(next_expected, msg_count, "all messages received");
    }

    #[test]
    fn back_pressure_blocks_publisher() {
        // Tiny term (64 bytes) - publisher should get BackPressured quickly
        // if sender doesn't scan.
        let (mut pub_h, mut send_h) = new_concurrent(1, 1, 0, 64, MTU).expect("valid params");

        let mut back_pressured = false;
        for _ in 0..100 {
            match pub_h.offer(&[]) {
                Ok(_) => {}
                Err(OfferError::AdminAction) => {}
                Err(OfferError::BackPressured) => {
                    back_pressured = true;
                    break;
                }
                Err(e) => panic!("unexpected: {e}"),
            }
        }
        assert!(
            back_pressured,
            "publisher should get back-pressured with tiny term"
        );

        // Scan to free space.
        let mut freed = 0u32;
        loop {
            let s = send_h.sender_scan(u32::MAX, |_, _| {});
            if s == 0 {
                break;
            }
            freed += s;
        }
        assert!(freed > 0, "sender should scan some frames");

        // Publisher should be able to offer again.
        let mut success = false;
        for _ in 0..4 {
            match pub_h.offer(&[]) {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(OfferError::AdminAction) => {}
                Err(e) => panic!("unexpected after scan: {e}"),
            }
        }
        assert!(success, "publisher should succeed after sender frees space");
    }

    // ---- SenderAgent integration ----

    #[test]
    fn sender_agent_concurrent_publication() {
        use aeron_rs::agent::Agent;
        use aeron_rs::agent::sender::SenderAgent;
        use aeron_rs::context::DriverContext;
        use aeron_rs::media::channel::UdpChannel;
        use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
        use aeron_rs::media::transport::UdpChannelTransport;
        use std::net::SocketAddr;

        let ctx = DriverContext::default();
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote_addr = channel.remote_data;

        let transport = UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
            .expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");
        let ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

        // Add a concurrent publication - get the publisher handle.
        let mut pub_handle = agent
            .add_concurrent_publication(ep_idx, 2001, 20, 0, 1u32 << 16, 1408u32)
            .expect("add concurrent pub");

        // Offer data from "this thread" (simulating app thread).
        let payload = [0xCA, 0xFE, 0xBA, 0xBE];
        pub_handle.offer(&payload).expect("offer");

        // Run sender duty cycle - should scan and send the frame.
        let work = agent.do_work().expect("do_work");
        assert!(
            work > 0,
            "expected work_count > 0 after concurrent offer, got {work}"
        );
    }
}
