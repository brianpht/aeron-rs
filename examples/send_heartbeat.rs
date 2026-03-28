//! Minimal sender agent example.
//!
//! Constructs a `SenderAgent` with a unicast endpoint, adds one publication,
//! and runs the duty cycle for a fixed number of iterations to demonstrate
//! heartbeat / setup frame generation.
//!
//! Requires Linux with io_uring support (kernel ≥ 5.6).
//!
//! ```sh
//! cargo run --example send_heartbeat
//! ```

use std::net::SocketAddr;

use aeron_rs::agent::Agent;
use aeron_rs::context::DriverContext;
use aeron_rs::agent::sender::SenderAgent;
use aeron_rs::media::channel::UdpChannel;
use aeron_rs::media::send_channel_endpoint::SendChannelEndpoint;
use aeron_rs::media::transport::UdpChannelTransport;

fn main() {
    let ctx = DriverContext::default();

    let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:40123")
        .expect("channel parse");

    let local_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let remote_addr = channel.remote_data;

    let transport = UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
        .expect("transport open");

    println!("Transport bound to {}", transport.bound_addr);

    let endpoint = SendChannelEndpoint::new(channel, transport);

    let mut agent = SenderAgent::new(&ctx).expect("sender agent");
    let ep_idx = agent.add_endpoint(endpoint).expect("add endpoint");

    agent.add_publication(
        ep_idx,
        /*session_id=*/ 1001,
        /*stream_id=*/ 10,
        /*initial_term_id=*/ 0,
        /*term_length=*/ 1 << 16,
        /*mtu=*/ 1408,
    );

    println!("Running sender duty cycle for 100 iterations...");
    for i in 0..100 {
        match agent.do_work() {
            Ok(work) => {
                if i % 25 == 0 {
                    println!("  iteration {i}: work_count={work}");
                }
            }
            Err(e) => {
                eprintln!("  error at iteration {i}: {e}");
                break;
            }
        }
        // Simulate ~1 ms between duty cycle ticks.
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    println!("Done ✓");
}

