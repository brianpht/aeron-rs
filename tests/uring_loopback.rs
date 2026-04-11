//! Integration test: io_uring UDP loopback send → recv.
//!
//! Opens a UdpChannelTransport on 127.0.0.1:0, registers it with a
//! UringTransportPoller, sends a known data frame from a separate std socket,
//! and asserts the callback fires with matching data on poll_recv.
//!
//! Requires Linux with io_uring support (kernel ≥ 5.6).

#[cfg(target_os = "linux")]
mod uring_loopback {
    use std::net::{SocketAddr, UdpSocket};

    use aeron_rs::context::DriverContext;
    use aeron_rs::frame::*;
    use aeron_rs::media::channel::UdpChannel;
    use aeron_rs::media::poller::{RecvMessage, TransportPoller};
    use aeron_rs::media::transport::UdpChannelTransport;
    use aeron_rs::media::uring_poller::UringTransportPoller;

    #[test]
    fn send_then_recv_loopback() {
        let ctx = DriverContext {
            uring_ring_size: 64,
            uring_recv_slots_per_transport: 4,
            uring_send_slots: 8,
            ..DriverContext::default()
        };

        let mut poller = UringTransportPoller::new(&ctx).expect("create poller");

        // Open a transport on a random port.
        let channel = UdpChannel::parse("aeron:udp?endpoint=127.0.0.1:0").unwrap();
        let local_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let remote_addr = channel.remote_data;

        let mut transport = UdpChannelTransport::open(&channel, &local_addr, &remote_addr, &ctx)
            .expect("transport open");

        let bound_addr = transport.bound_addr;

        // Register transport with poller (submits initial recv SQEs).
        let _t_idx = poller.add_transport(&mut transport).expect("add_transport");

        // Build a data frame to send.
        let data_hdr = DataHeader {
            frame_header: FrameHeader {
                frame_length: (DATA_HEADER_LENGTH + 4) as i32,
                version: CURRENT_VERSION,
                flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
                frame_type: FRAME_TYPE_DATA,
            },
            term_offset: 0,
            session_id: 0x1234,
            stream_id: 10,
            term_id: 0,
            reserved_value: 0,
        };
        let mut send_buf = [0u8; DATA_HEADER_LENGTH + 4];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &data_hdr as *const DataHeader as *const u8,
                send_buf.as_mut_ptr(),
                DATA_HEADER_LENGTH,
            );
        }
        send_buf[DATA_HEADER_LENGTH..].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        // Send from a separate std UDP socket to the transport's bound address.
        let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        sender.send_to(&send_buf, bound_addr).expect("send_to");

        // Poll until we receive the packet (with a reasonable timeout).
        let mut received_data: Vec<u8> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while received_data.is_empty() && std::time::Instant::now() < deadline {
            let _ = poller.poll_recv(|msg: RecvMessage<'_>| {
                if received_data.is_empty() {
                    received_data.extend_from_slice(msg.data);
                }
            });
            if received_data.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        assert!(!received_data.is_empty(), "should have received a packet");
        assert!(
            received_data.len() >= DATA_HEADER_LENGTH + 4,
            "packet too short: {}",
            received_data.len()
        );

        let parsed = DataHeader::parse(&received_data).expect("parse data header");
        assert_eq!({ parsed.session_id }, 0x1234);
        assert_eq!({ parsed.stream_id }, 10);
        assert_eq!(
            &received_data[DATA_HEADER_LENGTH..DATA_HEADER_LENGTH + 4],
            &[0xDE, 0xAD, 0xBE, 0xEF]
        );
    }
}
