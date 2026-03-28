//! Minimal receiver agent example.
//!
//! Constructs a `ReceiverAgent` with a unicast endpoint, runs the duty cycle
//! for a fixed number of iterations, and prints poll results.
//!
//! Requires Linux with io_uring support (kernel ≥ 5.6).
//!
//! ```sh
//! cargo run --example recv_data
//! ```

use std::net::SocketAddr;

use aeron_rs::agent::Agent;
use aeron_rs::context::DriverContext;
use aeron_rs::agent::receiver::ReceiverAgent;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::receive_channel_endpoint::ReceiveChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

fn main() {
    let ctx = DriverContext::default();

    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40124")
        .expect("channel parse");

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let remote_addr = channel.remote_data;

    let transport = UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
        .expect("transport open");

    println!("Receiver transport bound to {}", transport.bound_addr);

    let endpoint = ReceiveChannelEndpoint::new(channel, transport);

    let mut agent = ReceiverAgent::new(&ctx).expect("receiver agent");
    let _ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

    println!("Running receiver duty cycle for 50 iterations (no sender → no data expected)...");
    for i in 0..50 {
        match agent.do_work() {
            Ok(work) => {
                if i % 10 == 0 {
                    println!("  iteration {i}: work_count={work}");
                }
            }
            Err(e) => {
                eprintln!("  error at iteration {i}: {e}");
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    println!("Done ✓");
}

