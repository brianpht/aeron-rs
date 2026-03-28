//! Benchmark: frame parsing latency.
//!
//! Measures the hot-path cost of `FrameHeader::parse`, `DataHeader::parse`,
//! `StatusMessage::parse`, and `classify_frame` on pre-built byte buffers.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use aeron_rs::frame::*;

fn bench_frame_header_parse(c: &mut Criterion) {
    let hdr = FrameHeader {
        frame_length: 64,
        version: CURRENT_VERSION,
        flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
        frame_type: FRAME_TYPE_DATA,
    };
    let mut buf = [0u8; FRAME_HEADER_LENGTH];
    hdr.write(&mut buf);

    c.bench_function("FrameHeader::parse", |b| {
        b.iter(|| {
            black_box(FrameHeader::parse(black_box(&buf)));
        });
    });
}

fn bench_data_header_parse(c: &mut Criterion) {
    let data = DataHeader {
        frame_header: FrameHeader {
            frame_length: DATA_HEADER_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset: 0,
        session_id: 42,
        stream_id: 7,
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

    c.bench_function("DataHeader::parse", |b| {
        b.iter(|| {
            black_box(DataHeader::parse(black_box(&buf)));
        });
    });
}

fn bench_status_message_parse(c: &mut Criterion) {
    let sm = StatusMessage {
        frame_header: FrameHeader {
            frame_length: SM_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_SM,
        },
        session_id: 42,
        stream_id: 7,
        consumption_term_id: 100,
        consumption_term_offset: 2048,
        receiver_window: 65536,
        receiver_id: 0xBEEF,
    };
    let mut buf = [0u8; SM_TOTAL_LENGTH];
    sm.write(&mut buf);

    c.bench_function("StatusMessage::parse", |b| {
        b.iter(|| {
            black_box(StatusMessage::parse(black_box(&buf)));
        });
    });
}

fn bench_classify_frame(c: &mut Criterion) {
    let hdr = FrameHeader {
        frame_length: 64,
        version: CURRENT_VERSION,
        flags: 0,
        frame_type: FRAME_TYPE_DATA,
    };
    let mut buf = [0u8; FRAME_HEADER_LENGTH];
    hdr.write(&mut buf);

    c.bench_function("classify_frame", |b| {
        b.iter(|| {
            black_box(classify_frame(black_box(&buf)));
        });
    });
}

fn bench_nak_parse(c: &mut Criterion) {
    let nak = NakHeader {
        frame_header: FrameHeader {
            frame_length: NAK_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_NAK,
        },
        session_id: 42,
        stream_id: 7,
        active_term_id: 100,
        term_offset: 512,
        length: 1408,
    };
    let mut buf = [0u8; NAK_TOTAL_LENGTH];
    nak.write(&mut buf);

    c.bench_function("NakHeader::parse", |b| {
        b.iter(|| {
            black_box(NakHeader::parse(black_box(&buf)));
        });
    });
}

fn bench_setup_parse(c: &mut Criterion) {
    let setup = SetupHeader {
        frame_header: FrameHeader {
            frame_length: SETUP_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_SETUP,
        },
        term_offset: 0,
        session_id: 42,
        stream_id: 7,
        initial_term_id: 10,
        active_term_id: 12,
        term_length: 1 << 16,
        mtu: 1408,
        ttl: 0,
    };
    let mut buf = [0u8; SETUP_TOTAL_LENGTH];
    setup.write(&mut buf);

    c.bench_function("SetupHeader::parse", |b| {
        b.iter(|| {
            black_box(SetupHeader::parse(black_box(&buf)));
        });
    });
}

criterion_group!(
    benches,
    bench_frame_header_parse,
    bench_data_header_parse,
    bench_status_message_parse,
    bench_classify_frame,
    bench_nak_parse,
    bench_setup_parse,
);
criterion_main!(benches);

