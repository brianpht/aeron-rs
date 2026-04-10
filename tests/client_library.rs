// Integration test for the client library.
//
// Tests the full Aeron client flow: MediaDriver::launch -> connect ->
// add_publication -> offer -> close.

use aeron_rs::client::{MediaDriver, Aeron, Publication, Subscription};
use aeron_rs::context::DriverContext;
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

    let publication = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40199",
        10,
    ).expect("add_publication");

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

    let mut publication = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40200",
        10,
    ).expect("add_publication");

    // Offer a frame.
    let result = publication.offer(&[0xDE, 0xAD, 0xBE, 0xEF]);
    match result {
        Ok(pos) => {
            assert!(pos > 0, "position should advance");
            assert_eq!(publication.position(), pos);
        }
        Err(OfferError::AdminAction) => {
            // Term rotation - retry.
            let pos = publication.offer(&[0xDE, 0xAD, 0xBE, 0xEF])
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

    let pub1 = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40201",
        10,
    ).expect("pub1");
    let pub2 = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40202",
        20,
    ).expect("pub2");

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

    let sub = aeron.add_subscription(
        "aeron:udp?endpoint=127.0.0.1:40203",
        30,
    ).expect("add_subscription");

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

    let publication = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40204",
        10,
    ).expect("add_publication");

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

    let mut publication = aeron.add_publication(
        "aeron:udp?endpoint=127.0.0.1:40205",
        10,
    ).expect("add_publication");

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

