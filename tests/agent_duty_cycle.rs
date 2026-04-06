//! Integration test: smoke-test the agent duty cycle.
//!
//! Constructs SenderAgent and ReceiverAgent with default DriverContext,
//! calls do_work a few iterations, and asserts no errors.
//!
//! Requires Linux with io_uring support (kernel ≥ 5.6).

#[cfg(target_os = "linux")]
mod agent_duty_cycle {
    use std::net::SocketAddr;

    use aeron_rs::agent::Agent;
    use aeron_rs::agent::receiver::ReceiverAgent;
    use aeron_rs::agent::sender::SenderAgent;
    use aeron_rs::context::DriverContext;
    use aeron_rs::media::channel::UdpChannel;
    use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
    use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
    use aeron_rs::media::transport::UdpChannelTransport;

    #[test]
    fn sender_agent_runs_without_error() {
        let ctx = DriverContext::default();
        let mut agent = SenderAgent::new(&ctx).expect("create sender agent");

        // Run a few idle duty cycle iterations.
        for _ in 0..10 {
            let work = agent.do_work().expect("do_work should not fail");
            assert!(work >= 0);
        }
    }

    #[test]
    fn receiver_agent_runs_without_error() {
        let ctx = DriverContext::default();
        let mut agent = ReceiverAgent::new(&ctx).expect("create receiver agent");

        for _ in 0..10 {
            let work = agent.do_work().expect("do_work should not fail");
            assert!(work >= 0);
        }
    }

    #[test]
    fn sender_with_endpoint_and_publication() {
        let ctx = DriverContext::default();
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote_addr = channel.remote_data;

        let transport =
            UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
                .expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");
        let ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

        let pub_idx = agent
            .add_publication(ep_idx, 1001, 10, 0, 1u32 << 16, 1408u32)
            .expect("add pub");
        assert_eq!(pub_idx, 0);

        // Run duty cycle - should generate heartbeats/setups without error.
        for _ in 0..20 {
            let work = agent.do_work().expect("do_work");
            assert!(work >= 0);
        }
    }

    #[test]
    fn sender_offer_and_send() {
        let ctx = DriverContext::default();
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote_addr = channel.remote_data;

        let transport =
            UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
                .expect("transport open");

        let endpoint = SendChannelEndpoint::new(channel, transport);
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");
        let ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

        let pub_idx = agent
            .add_publication(ep_idx, 1001, 10, 0, 1u32 << 16, 1408u32)
            .expect("add pub");

        // Offer data into the publication.
        let payload = [0xCA, 0xFE, 0xBA, 0xBE];
        {
            let publication = agent.publication_mut(pub_idx).expect("publication_mut");
            publication.offer(&payload).expect("offer");
        }

        // Run duty cycle - sender_scan should find the frame and send it.
        let work = agent.do_work().expect("do_work");
        assert!(work > 0, "expected work_count > 0 after offer, got {work}");
    }

    #[test]
    fn sender_add_publication_returns_index() {
        let ctx = DriverContext::default();
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");

        let idx0 = agent
            .add_publication(0, 1001, 10, 0, 1u32 << 16, 1408u32)
            .expect("first pub");
        let idx1 = agent
            .add_publication(0, 1002, 11, 0, 1u32 << 16, 1408u32)
            .expect("second pub");

        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
    }

    #[test]
    fn sender_add_publication_invalid_returns_none() {
        let ctx = DriverContext::default();
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");

        // term_length not power-of-two -> None.
        assert!(agent.add_publication(0, 1001, 10, 0, 100, 1408).is_none());
        // mtu too small -> None.
        assert!(agent.add_publication(0, 1001, 10, 0, 1u32 << 16, 0).is_none());
    }

    #[test]
    fn sender_publication_mut_accessor() {
        let ctx = DriverContext::default();
        let mut agent = SenderAgent::new(&ctx).expect("sender agent");

        assert!(agent.publication_mut(0).is_none(), "no publications yet");

        agent
            .add_publication(0, 42, 7, 0, 1u32 << 16, 1408u32)
            .expect("add pub");

        let pub_ref = agent.publication_mut(0).expect("should exist");
        assert_eq!(pub_ref.session_id(), 42);
        assert_eq!(pub_ref.stream_id(), 7);

        assert!(agent.publication_mut(999).is_none(), "out of bounds");
    }

    #[test]
    fn receiver_with_endpoint() {
        let ctx = DriverContext::default();
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let remote_addr = channel.remote_data;

        let transport =
            UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
                .expect("transport open");

        let endpoint = ReceiveChannelEndpoint::new(channel, transport);
        let mut agent = ReceiverAgent::new(&ctx).expect("receiver agent");
        let _ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

        for _ in 0..20 {
            let work = agent.do_work().expect("do_work");
            assert!(work >= 0);
        }
    }

    #[test]
    fn agent_on_start_on_close_defaults() {
        let ctx = DriverContext::default();
        let mut sender = SenderAgent::new(&ctx).expect("sender");
        assert!(sender.on_start().is_ok());
        assert!(sender.on_close().is_ok());

        let mut receiver = ReceiverAgent::new(&ctx).expect("receiver");
        assert!(receiver.on_start().is_ok());
        assert!(receiver.on_close().is_ok());
    }
}

