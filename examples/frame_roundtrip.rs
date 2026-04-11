//! Demonstrates building each Aeron wire-protocol frame, serialising it
//! into a byte buffer via `.write()`, then parsing it back via `.parse()`.
//!
//! No sockets or io_uring required — runs on any platform.
//!
//! ```sh
//! cargo run --example frame_roundtrip
//! ```

use aeron_rs::frame::*;

fn main() {
    // ── Data Header ──
    let data = DataHeader {
        frame_header: FrameHeader {
            frame_length: DATA_HEADER_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset: 0,
        session_id: 42,
        stream_id: 1,
        term_id: 100,
        reserved_value: 0,
    };
    let mut buf = [0u8; DATA_HEADER_LENGTH];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &data as *const DataHeader as *const u8,
            buf.as_mut_ptr(),
            DATA_HEADER_LENGTH,
        );
    }
    let parsed = DataHeader::parse(&buf).expect("DataHeader parse failed");
    // Copy fields out of packed struct before printing (avoids UB from references).
    let sid = parsed.session_id;
    let stid = parsed.stream_id;
    let tid = parsed.term_id;
    let toff = parsed.term_offset;
    println!("DataHeader: session={sid}, stream={stid}, term_id={tid}, term_offset={toff}");

    // ── Status Message ──
    let sm = StatusMessage {
        frame_header: FrameHeader {
            frame_length: SM_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_SM,
        },
        session_id: 42,
        stream_id: 1,
        consumption_term_id: 100,
        consumption_term_offset: 1024,
        receiver_window: 65536,
        receiver_id: 0xDEAD_BEEF,
    };
    let mut sm_buf = [0u8; SM_TOTAL_LENGTH];
    sm.write(&mut sm_buf);
    let p = StatusMessage::parse(&sm_buf).expect("SM parse failed");
    let (sid, stid, ctid, ctoff, rw) = (
        p.session_id,
        p.stream_id,
        p.consumption_term_id,
        p.consumption_term_offset,
        p.receiver_window,
    );
    println!(
        "StatusMessage: session={sid}, stream={stid}, term_id={ctid}, term_offset={ctoff}, window={rw}"
    );

    // ── NAK Header ──
    let nak = NakHeader {
        frame_header: FrameHeader {
            frame_length: NAK_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_NAK,
        },
        session_id: 42,
        stream_id: 1,
        active_term_id: 100,
        term_offset: 512,
        length: 1408,
    };
    let mut nak_buf = [0u8; NAK_TOTAL_LENGTH];
    nak.write(&mut nak_buf);
    let p = NakHeader::parse(&nak_buf).expect("NAK parse failed");
    let (sid, stid, atid, toff, nlen) = (
        p.session_id,
        p.stream_id,
        p.active_term_id,
        p.term_offset,
        p.length,
    );
    println!("NakHeader: session={sid}, stream={stid}, term_id={atid}, offset={toff}, len={nlen}");

    // ── Setup Header ──
    let setup = SetupHeader {
        frame_header: FrameHeader {
            frame_length: SETUP_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_SETUP,
        },
        term_offset: 0,
        session_id: 42,
        stream_id: 1,
        initial_term_id: 100,
        active_term_id: 100,
        term_length: 1 << 16,
        mtu: 1408,
        ttl: 0,
    };
    let mut setup_buf = [0u8; SETUP_TOTAL_LENGTH];
    setup.write(&mut setup_buf);
    let p = SetupHeader::parse(&setup_buf).expect("Setup parse failed");
    let (sid, stid, itid, atid, tlen, mtu) = (
        p.session_id,
        p.stream_id,
        p.initial_term_id,
        p.active_term_id,
        p.term_length,
        p.mtu,
    );
    println!(
        "SetupHeader: session={sid}, stream={stid}, init_term={itid}, active_term={atid}, term_len={tlen}, mtu={mtu}"
    );

    // ── RTTM Header ──
    let rttm = RttmHeader {
        frame_header: FrameHeader {
            frame_length: RTTM_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_RTTM,
        },
        session_id: 42,
        stream_id: 1,
        echo_timestamp: 123_456_789,
        reception_delta: 0,
        receiver_id: 0xCAFE,
    };
    let mut rttm_buf = [0u8; RTTM_TOTAL_LENGTH];
    rttm.write(&mut rttm_buf);
    let p = RttmHeader::parse(&rttm_buf).expect("RTTM parse failed");
    let (sid, stid, ets, rid) = (p.session_id, p.stream_id, p.echo_timestamp, p.receiver_id);
    println!("RttmHeader: session={sid}, stream={stid}, echo_ts={ets}, receiver_id=0x{rid:X}");

    // ── classify_frame ──
    println!("\nclassify_frame on each buffer:");
    println!("  data   → {:?}", classify_frame(&buf));
    println!("  sm     → {:?}", classify_frame(&sm_buf));
    println!("  nak    → {:?}", classify_frame(&nak_buf));
    println!("  setup  → {:?}", classify_frame(&setup_buf));
    println!("  rttm   → {:?}", classify_frame(&rttm_buf));
    println!("  empty  → {:?}", classify_frame(&[]));

    println!("\nAll frame roundtrips passed [OK]");
}
