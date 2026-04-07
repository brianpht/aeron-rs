// Criterion benchmarks for RetransmitHandler hot-path operations.

use std::hint::black_box;
use criterion::{Criterion, criterion_group, criterion_main};

use aeron_rs::frame::{
    FrameHeader, NakHeader, CURRENT_VERSION, FRAME_TYPE_NAK, NAK_TOTAL_LENGTH,
};
use aeron_rs::media::retransmit_handler::{RetransmitHandler, MAX_RETRANSMIT_ACTIONS};

fn make_nak(session_id: i32, term_offset: i32) -> NakHeader {
    NakHeader {
        frame_header: FrameHeader {
            frame_length: NAK_TOTAL_LENGTH as i32,
            version: CURRENT_VERSION,
            flags: 0,
            frame_type: FRAME_TYPE_NAK,
        },
        session_id,
        stream_id: 1,
        active_term_id: 0,
        term_offset,
        length: 1408,
    }
}

fn bench_on_nak_empty_table(c: &mut Criterion) {
    c.bench_function("retransmit: on_nak (schedule, empty table)", |b| {
        let nak = make_nak(42, 512);
        b.iter(|| {
            let mut handler = RetransmitHandler::new(0, 60_000_000);
            black_box(handler.on_nak(black_box(&nak), black_box(100)));
        });
    });
}

fn bench_on_nak_dedup_full_scan(c: &mut Criterion) {
    c.bench_function("retransmit: on_nak (dedup, full table scan)", |b| {
        let mut handler = RetransmitHandler::new(1_000_000, 60_000_000);
        // Fill table with unique entries.
        for i in 0..MAX_RETRANSMIT_ACTIONS {
            let nak = make_nak(i as i32, 0);
            handler.on_nak(&nak, 0);
        }
        // NAK that matches the last entry - forces full scan.
        let nak = make_nak((MAX_RETRANSMIT_ACTIONS - 1) as i32, 0);
        b.iter(|| {
            black_box(handler.on_nak(black_box(&nak), black_box(100)));
        });
    });
}

fn bench_process_timeouts_one_expired(c: &mut Criterion) {
    c.bench_function(
        "retransmit: process_timeouts (64 entries, 1 expired)",
        |b| {
            b.iter_batched(
                || {
                    let mut h2 = RetransmitHandler::new(0, 60_000_000);
                    // Entry 0: delay_ns=0, so expiry_ns = now (immediately expired).
                    let nak0 = make_nak(0, 0);
                    h2.on_nak(&nak0, 0);
                    // Fill rest with far-future entries (delay_ns=0 but scheduled at now=1B).
                    for i in 1..MAX_RETRANSMIT_ACTIONS {
                        let nak = make_nak(i as i32, 0);
                        h2.on_nak(&nak, 1_000_000_000);
                    }
                    h2
                },
                |mut handler| {
                    handler.process_timeouts(black_box(1), |s, st, t, to, l| {
                        black_box((s, st, t, to, l));
                    });
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
}

fn bench_process_timeouts_none_expired(c: &mut Criterion) {
    c.bench_function(
        "retransmit: process_timeouts (64 entries, 0 expired)",
        |b| {
            let mut handler = RetransmitHandler::new(1_000_000_000, 60_000_000);
            for i in 0..MAX_RETRANSMIT_ACTIONS {
                let nak = make_nak(i as i32, 0);
                handler.on_nak(&nak, 0);
            }
            b.iter(|| {
                handler.process_timeouts(black_box(100), |s, st, t, to, l| {
                    black_box((s, st, t, to, l));
                });
            });
        },
    );
}

fn bench_full_nak_to_callback(c: &mut Criterion) {
    c.bench_function("retransmit: full NAK-to-callback (1 frame)", |b| {
        let nak = make_nak(42, 512);
        b.iter(|| {
            let mut handler = RetransmitHandler::new(0, 60_000_000);
            handler.on_nak(black_box(&nak), black_box(100));
            handler.process_timeouts(black_box(100), |s, st, t, to, l| {
                black_box((s, st, t, to, l));
            });
        });
    });
}

criterion_group!(
    benches,
    bench_on_nak_empty_table,
    bench_on_nak_dedup_full_scan,
    bench_process_timeouts_one_expired,
    bench_process_timeouts_none_expired,
    bench_full_nak_to_callback,
);
criterion_main!(benches);

