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

        agent.add_publication(ep_idx, 1001, 10, 0, 1 << 16, 1408);

        // Run duty cycle — should generate heartbeats/setups without error.
        for _ in 0..20 {
            let work = agent.do_work().expect("do_work");
            assert!(work >= 0);
        }
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

