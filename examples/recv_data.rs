//! Minimal receiver agent example.
//!
//! Constructs a `ReceiverAgent` with a unicast endpoint, runs the duty cycle
//! via `AgentRunner` with backoff idle strategy.
//!
//! Requires Linux with io_uring support (kernel >= 5.6).
//!
//! ```sh
//! cargo run --example recv_data
//! ```

use std::net::SocketAddr;

use aeron_rs::agent::receiver::ReceiverAgent;
use aeron_rs::agent::runner::AgentRunner;
use aeron_rs::context::DriverContext;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

fn main() {
    let ctx = DriverContext::default();

    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40124").expect("channel parse");

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let remote_addr = channel.remote_data;

    let transport = UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
        .expect("transport open");

    println!("Receiver transport bound to {}", transport.bound_addr);

    let endpoint = ReceiveChannelEndpoint::new(channel, transport, 0);

    let mut agent = ReceiverAgent::new(&ctx).expect("receiver agent");
    let _ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

    println!(
        "Starting receiver agent on dedicated thread (100 ms run, no sender - no data expected)..."
    );
    let runner = AgentRunner::new(agent, ctx.idle_strategy());
    let handle = runner.start();

    // Let the agent run for 100 ms, then stop.
    std::thread::sleep(std::time::Duration::from_millis(100));

    match handle.join() {
        Ok(()) => println!("Done - agent stopped cleanly"),
        Err(e) => eprintln!("Agent error: {e}"),
    }
}
