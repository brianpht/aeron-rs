// Integration test for the client library.
//
// Tests the full Aeron client flow: MediaDriver::launch -> connect ->
// add_publication -> offer -> close.
//
// Subscription tests (Option B): launch MediaDriver, add_subscription
// (which creates a real UDP receive endpoint), send raw Setup + Data
// frames via a separate UdpSocket, and verify Subscription::poll()
// delivers the fragments.

use aeron_rs::client::{Aeron, MediaDriver, Publication, Subscription};
use aeron_rs::context::DriverContext;
use aeron_rs::frame::{
    CURRENT_VERSION, DATA_FLAG_BEGIN, DATA_FLAG_END, DATA_HEADER_LENGTH, DataHeader,
    FRAME_TYPE_DATA, FRAME_TYPE_SETUP, FrameHeader, SETUP_TOTAL_LENGTH, SetupHeader,
};
use aeron_rs::media::network_publication::OfferError;

fn make_ctx() -> DriverContext {
    DriverContext {
        uring_ring_size: 64,
        uring_recv_slots_per_transport: 4,
        uring_send_slots: 16,
        uring_buf_ring_entries: 16,
        term_buffer_length: 1024,
        ..DriverContext::default()
    }
}

#[test]
fn media_driver_launch_and_close() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    // Let it run briefly.
    std::thread::sleep(std::time::Duration::from_millis(10));
    driver.close().expect("close");
}

#[test]
fn connect_returns_aeron_client() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let aeron = driver.connect().expect("connect");
    assert!(aeron.is_driver_alive());
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn add_publication_returns_publication() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40199", 10)
        .expect("add_publication");

    assert_eq!(publication.stream_id(), 10);
    assert_eq!(publication.term_length(), 1024);
    assert_eq!(publication.position(), 0);

    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn publication_offer_advances_position() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let mut publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40200", 10)
        .expect("add_publication");

    // Offer a frame.
    let result = publication.offer(&[0xDE, 0xAD, 0xBE, 0xEF]);
    match result {
        Ok(pos) => {
            assert!(pos > 0, "position should advance");
            assert_eq!(publication.position(), pos);
        }
        Err(OfferError::AdminAction) => {
            // Term rotation - retry.
            let pos = publication
                .offer(&[0xDE, 0xAD, 0xBE, 0xEF])
                .expect("offer after admin action");
            assert!(pos > 0);
        }
        Err(e) => panic!("unexpected offer error: {e}"),
    }

    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn multiple_publications_on_different_streams() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let pub1 = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40201", 10)
        .expect("pub1");
    let pub2 = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40202", 20)
        .expect("pub2");

    assert_eq!(pub1.stream_id(), 10);
    assert_eq!(pub2.stream_id(), 20);
    assert_ne!(pub1.session_id(), pub2.session_id());

    drop(pub1);
    drop(pub2);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn add_subscription_returns_subscription() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let sub = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40203", 30)
        .expect("add_subscription");

    assert_eq!(sub.stream_id(), 30);
    assert_eq!(sub.channel(), "aeron:udp?endpoint=127.0.0.1:40203");

    drop(sub);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn publication_channel_accessor() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40204", 10)
        .expect("add_publication");

    assert_eq!(publication.channel(), "aeron:udp?endpoint=127.0.0.1:40204");
    assert!(publication.registration_id() > 0);

    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn heartbeat_does_not_panic() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    // Multiple heartbeats should be fine.
    for _ in 0..10 {
        aeron.heartbeat();
    }

    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn publication_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Publication>();
}

#[test]
fn subscription_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Subscription>();
}

#[test]
fn aeron_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Aeron>();
}

#[test]
fn invalid_channel_returns_error() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let result = aeron.add_publication("bad_channel", 10);
    assert!(result.is_err());

    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn publication_offer_and_sender_picks_up() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let mut publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40205", 10)
        .expect("add_publication");

    // Give the sender agent time to poll the bridge and register the endpoint.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Offer multiple frames.
    let mut last_pos = 0i64;
    for i in 0..5 {
        loop {
            match publication.offer(&[i as u8; 4]) {
                Ok(pos) => {
                    assert!(pos > last_pos, "position must advance");
                    last_pos = pos;
                    break;
                }
                Err(OfferError::AdminAction) => continue, // term rotation, retry
                Err(OfferError::BackPressured) => {
                    // Sender hasn't caught up yet - wait briefly.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    }

    assert!(last_pos > 0);

    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}

// ── Subscription integration test helpers ──

/// Build a raw Setup frame as a byte array.
fn build_setup_frame(
    session_id: i32,
    stream_id: i32,
    initial_term_id: i32,
    active_term_id: i32,
    term_offset: i32,
    term_length: i32,
) -> [u8; SETUP_TOTAL_LENGTH] {
    let setup = SetupHeader {
        frame_header: FrameHeader {
            frame_length: SETUP_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_SETUP,
        },
        term_offset,
        session_id,
        stream_id,
        initial_term_id,
        active_term_id,
        term_length,
        mtu: 1408,
        ttl: 0,
    };
    let mut buf = [0u8; SETUP_TOTAL_LENGTH];
    setup.write(&mut buf);
    buf
}

/// Build a raw Data frame (header + payload) as a Vec.
fn build_data_frame(
    session_id: i32,
    stream_id: i32,
    term_id: i32,
    term_offset: i32,
    payload: &[u8],
) -> Vec<u8> {
    let total_len = DATA_HEADER_LENGTH + payload.len();
    let data_hdr = DataHeader {
        frame_header: FrameHeader {
            frame_length: total_len as i32,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset,
        session_id,
        stream_id,
        term_id,
        reserved_value: 0,
    };
    let mut buf = vec![0u8; total_len];
    // Write DataHeader into first DATA_HEADER_LENGTH bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &data_hdr as *const DataHeader as *const u8,
            buf.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }
    buf[DATA_HEADER_LENGTH..].copy_from_slice(payload);
    buf
}

/// Compute aligned frame length (rounds up to 32-byte boundary).
fn aligned_frame_len(payload_len: usize) -> usize {
    let raw = DATA_HEADER_LENGTH + payload_len;
    (raw + 31) & !31
}

/// Helper: poll the subscription in a retry loop until at least `min_fragments`
/// arrive or `timeout` expires. Avoids flaky tests from timing races.
fn poll_until(
    sub: &mut Subscription,
    min_fragments: usize,
    timeout: std::time::Duration,
) -> Vec<(Vec<u8>, i32, i32)> {
    let start = std::time::Instant::now();
    let mut results: Vec<(Vec<u8>, i32, i32)> = Vec::new();
    while results.len() < min_fragments && start.elapsed() < timeout {
        sub.poll(
            |payload, sess, strm| {
                results.push((payload.to_vec(), sess, strm));
            },
            64,
        );
        if results.len() < min_fragments {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    results
}

// ── Subscription integration tests (Option B) ──

#[test]
fn subscription_poll_receives_data() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let mut sub = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40300", 30)
        .expect("add_subscription");

    // Give receiver agent time to drain the endpoint bridge and register.
    std::thread::sleep(std::time::Duration::from_millis(100));

    let sender = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind sender");
    let dest: std::net::SocketAddr = "127.0.0.1:40300".parse().expect("parse dest");

    let session_id = 42i32;
    let stream_id = 30i32;
    let term_length = 1024i32;

    // Step 1: Send Setup frame so the receiver creates an image.
    let setup_buf = build_setup_frame(session_id, stream_id, 0, 0, 0, term_length);
    sender.send_to(&setup_buf, dest).expect("send setup");

    // Give receiver time to process setup and deposit SubscriberImage.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Step 2: Send a single Data frame with 4-byte payload.
    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    let data_buf = build_data_frame(session_id, stream_id, 0, 0, &payload);
    sender.send_to(&data_buf, dest).expect("send data");

    // Step 3: Poll the subscription (with retry loop for timing tolerance).
    let results = poll_until(&mut sub, 1, std::time::Duration::from_secs(2));

    assert_eq!(
        results.len(),
        1,
        "expected 1 fragment, got {}",
        results.len()
    );
    assert_eq!(results[0].0, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(results[0].1, session_id);
    assert_eq!(results[0].2, stream_id);

    drop(sub);
    drop(aeron);
    driver.close().expect("close");
}

#[test]
fn subscription_poll_multiple_fragments() {
    let ctx = make_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");

    let mut sub = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40301", 31)
        .expect("add_subscription");

    std::thread::sleep(std::time::Duration::from_millis(100));

    let sender = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind sender");
    let dest: std::net::SocketAddr = "127.0.0.1:40301".parse().expect("parse dest");

    let session_id = 43i32;
    let stream_id = 31i32;
    let term_length = 1024i32;

    // Send Setup.
    let setup_buf = build_setup_frame(session_id, stream_id, 0, 0, 0, term_length);
    sender.send_to(&setup_buf, dest).expect("send setup");
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Send 3 data frames at sequential offsets.
    let payloads: [&[u8]; 3] = [b"aaaa", b"bbbb", b"cccc"];
    let frame_size = aligned_frame_len(4); // 64 bytes

    for (i, payload) in payloads.iter().enumerate() {
        let offset = (i * frame_size) as i32;
        let data_buf = build_data_frame(session_id, stream_id, 0, offset, payload);
        sender.send_to(&data_buf, dest).expect("send data");
    }

    // Poll and collect fragments.
    let results = poll_until(&mut sub, 3, std::time::Duration::from_secs(2));

    assert_eq!(
        results.len(),
        3,
        "expected 3 fragments, got {} (some data frames may not have been processed)",
        results.len()
    );
    assert_eq!(results[0].0, b"aaaa");
    assert_eq!(results[1].0, b"bbbb");
    assert_eq!(results[2].0, b"cccc");

    // All fragments should have the same session_id/stream_id.
    for r in &results {
        assert_eq!(r.1, session_id);
        assert_eq!(r.2, stream_id);
    }

    drop(sub);
    drop(aeron);
    driver.close().expect("close");
}
