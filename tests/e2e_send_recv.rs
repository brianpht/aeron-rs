// End-to-end test: Publication::offer() -> SenderAgent -> UDP -> ReceiverAgent -> Subscription::poll()
//
// Unlike client_library.rs subscription tests (which inject raw UDP frames),
// these tests exercise the full data path including automatic Setup/SM
// handshake between sender and receiver agents.
//
// Data flow:
//   Client offer() -> shared buffer -> SenderAgent sender_scan -> UDP send
//   -> ReceiverAgent poll_data (io_uring CQE) -> on_setup (creates image)
//   -> on_data (writes to SharedLogBuffer) -> Subscription::poll() -> handler
use std::time::{Duration, Instant};
use aeron_rs::client::{MediaDriver, Publication, Subscription};
use aeron_rs::context::DriverContext;
use aeron_rs::media::network_publication::OfferError;
/// Build a DriverContext with small buffers and fast timers for tests.
/// Fast heartbeat/SM intervals reduce Setup/SM handshake latency.
fn make_e2e_ctx() -> DriverContext {
    DriverContext {
        uring_ring_size: 64,
        uring_recv_slots_per_transport: 4,
        uring_send_slots: 16,
        uring_buf_ring_entries: 16,
        term_buffer_length: 1024,
        // Fast heartbeat (10ms) so sender sends Setup quickly after bridge drain.
        heartbeat_interval_ns: Duration::from_millis(10).as_nanos() as i64,
        // Fast SM interval (20ms) so receiver replies quickly after on_setup.
        sm_interval_ns: Duration::from_millis(20).as_nanos() as i64,
        ..DriverContext::default()
    }
}
/// Offer a payload with retry loop for BackPressured / AdminAction.
/// Panics on timeout or unexpected error.
fn offer_with_retry(
    publication: &mut Publication,
    payload: &[u8],
    timeout: Duration,
) -> i64 {
    let start = Instant::now();
    loop {
        match publication.offer(payload) {
            Ok(pos) => return pos,
            Err(OfferError::AdminAction) => continue,
            Err(OfferError::BackPressured) => {
                if start.elapsed() > timeout {
                    panic!("offer timed out after {timeout:?} (BackPressured)");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("unexpected offer error: {e}"),
        }
    }
}
/// Poll subscription in a retry loop until at least `min_fragments` arrive
/// or `timeout` expires. Returns collected (payload, session_id, stream_id).
fn poll_until(
    sub: &mut Subscription,
    min_fragments: usize,
    timeout: Duration,
) -> Vec<(Vec<u8>, i32, i32)> {
    let start = Instant::now();
    let mut results: Vec<(Vec<u8>, i32, i32)> = Vec::new();
    while results.len() < min_fragments && start.elapsed() < timeout {
        sub.poll(
            |payload, sess, strm| {
                results.push((payload.to_vec(), sess, strm));
            },
            64,
        );
        if results.len() < min_fragments {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    results
}
// -- E2E tests --
/// Full pipeline: offer a single fragment through Publication, verify it
/// arrives via Subscription::poll() after the sender sends Setup, the
/// receiver creates an image and sends SM, and the sender transmits data.
#[test]
fn publication_to_subscription_single_fragment() {
    let ctx = make_e2e_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");
    // Subscription binds to the endpoint address (receiver listens here).
    let mut sub = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40500", 50)
        .expect("add_subscription");
    // Publication sends to the same endpoint address.
    let mut publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40500", 50)
        .expect("add_publication");
    let expected_session = publication.session_id();
    let expected_stream = publication.stream_id();
    assert_eq!(expected_stream, 50);
    // Wait for bridge draining + Setup/SM handshake.
    // With fast timers (heartbeat=10ms, sm=20ms), 300ms is generous.
    std::thread::sleep(Duration::from_millis(300));
    // Offer a single fragment.
    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    let pos = offer_with_retry(&mut publication, &payload, Duration::from_secs(2));
    assert!(pos > 0, "position should advance after offer");
    // Poll the subscription.
    let results = poll_until(&mut sub, 1, Duration::from_secs(3));
    assert_eq!(results.len(), 1, "expected 1 fragment, got {}", results.len());
    assert_eq!(results[0].0, payload);
    assert_eq!(results[0].1, expected_session);
    assert_eq!(results[0].2, expected_stream);
    drop(sub);
    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}
/// Offer 5 sequential fragments, verify all arrive in order.
#[test]
fn publication_to_subscription_multi_fragment() {
    let ctx = make_e2e_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");
    let mut sub = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40501", 51)
        .expect("add_subscription");
    let mut publication = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40501", 51)
        .expect("add_publication");
    let expected_session = publication.session_id();
    std::thread::sleep(Duration::from_millis(300));
    // Offer 5 distinct fragments.
    let count = 5usize;
    for i in 0..count {
        let payload = [i as u8; 4];
        offer_with_retry(&mut publication, &payload, Duration::from_secs(2));
    }
    // Poll until all 5 arrive.
    let results = poll_until(&mut sub, count, Duration::from_secs(3));
    assert_eq!(
        results.len(),
        count,
        "expected {count} fragments, got {}",
        results.len()
    );
    // Verify payloads are in order.
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.0, vec![i as u8; 4], "fragment {i} payload mismatch");
        assert_eq!(r.1, expected_session);
        assert_eq!(r.2, 51);
    }
    drop(sub);
    drop(publication);
    drop(aeron);
    driver.close().expect("close");
}
/// Two pub/sub pairs on different stream_ids and endpoints.
/// Verify data is isolated - each subscription only receives its stream.
#[test]
fn publication_and_subscription_different_streams_isolated() {
    let ctx = make_e2e_ctx();
    let driver = MediaDriver::launch(ctx).expect("launch");
    let mut aeron = driver.connect().expect("connect");
    // Two subscriptions on different ports/streams.
    let mut sub_a = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40502", 52)
        .expect("add_subscription A");
    let mut sub_b = aeron
        .add_subscription("aeron:udp?endpoint=127.0.0.1:40503", 53)
        .expect("add_subscription B");
    // Two publications targeting those endpoints.
    let mut pub_a = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40502", 52)
        .expect("add_publication A");
    let mut pub_b = aeron
        .add_publication("aeron:udp?endpoint=127.0.0.1:40503", 53)
        .expect("add_publication B");
    std::thread::sleep(Duration::from_millis(300));
    // Offer on pub_a.
    offer_with_retry(&mut pub_a, b"AAAA", Duration::from_secs(2));
    // Offer on pub_b.
    offer_with_retry(&mut pub_b, b"BBBB", Duration::from_secs(2));
    // Poll each subscription independently.
    let results_a = poll_until(&mut sub_a, 1, Duration::from_secs(3));
    let results_b = poll_until(&mut sub_b, 1, Duration::from_secs(3));
    // sub_a should receive only pub_a's data.
    assert_eq!(results_a.len(), 1, "sub_a expected 1 fragment, got {}", results_a.len());
    assert_eq!(results_a[0].0, b"AAAA");
    assert_eq!(results_a[0].1, pub_a.session_id());
    assert_eq!(results_a[0].2, 52);
    // sub_b should receive only pub_b's data.
    assert_eq!(results_b.len(), 1, "sub_b expected 1 fragment, got {}", results_b.len());
    assert_eq!(results_b[0].0, b"BBBB");
    assert_eq!(results_b[0].1, pub_b.session_id());
    assert_eq!(results_b[0].2, 53);
    // Verify session isolation - different sessions.
    assert_ne!(pub_a.session_id(), pub_b.session_id());
    drop(sub_a);
    drop(sub_b);
    drop(pub_a);
    drop(pub_b);
    drop(aeron);
    driver.close().expect("close");
}
