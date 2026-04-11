//! Benchmark: loss detection scan + NAK generation.
//!
//! Measures the cost of:
//! 1. Gap detection in the receiver's image (wrapping arithmetic scan)
//! 2. NAK frame construction + wire-format write into scratch buffer
//! 3. PendingNak queue into the endpoint's pre-sized array
//! 4. Full end-to-end loss scan simulation (16 frames, 1 gap → NAK)
//!
//! Target: < 200 ns full scan (vs Aeron C ~200–500 ns).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use aeron_rs::frame::*;
use aeron_rs::media::receive_channel_endpoint::PendingNak;

// ──────────────────── Gap detection ────────────────────

/// Wrapping gap detection — mirrors InlineHandler::on_data.
/// Uses wrapping_sub + half-range check as required by coding rules.
#[inline(always)]
fn detect_gap(expected_offset: i32, received_offset: i32) -> bool {
    let gap = received_offset.wrapping_sub(expected_offset);
    gap > 0 && gap < (i32::MAX / 2)
}

fn bench_gap_detection_hit(c: &mut Criterion) {
    c.bench_function("loss: gap_detect (wrapping sub, gap)", |b| {
        b.iter(|| {
            let is_gap = detect_gap(black_box(1024), black_box(2048));
            black_box(is_gap);
        });
    });
}

fn bench_gap_detection_miss(c: &mut Criterion) {
    c.bench_function("loss: gap_detect (wrapping sub, no gap)", |b| {
        b.iter(|| {
            let is_gap = detect_gap(black_box(1024), black_box(1024));
            black_box(is_gap);
        });
    });
}

// ──────────────────── NAK construction + write ────────────────────

fn bench_nak_build_and_write(c: &mut Criterion) {
    let mut buf = [0u8; NAK_TOTAL_LENGTH];

    c.bench_function("loss: NAK build + write", |b| {
        b.iter(|| {
            let nak = NakHeader {
                frame_header: FrameHeader {
                    frame_length: NAK_TOTAL_LENGTH as i32,
                    version: CURRENT_VERSION,
                    flags: 0,
                    frame_type: FRAME_TYPE_NAK,
                },
                session_id: black_box(42),
                stream_id: black_box(10),
                active_term_id: black_box(100),
                term_offset: black_box(1024),
                length: black_box(1408),
            };
            nak.write(&mut buf);
            black_box(&buf);
        });
    });
}

// ──────────────────── PendingNak queue ────────────────────

/// Simulates the queue_nak hot path: copy a PendingNak into a pre-sized
/// flat array at the next free index.
fn bench_queue_nak(c: &mut Criterion) {
    const MAX_PENDING: usize = 64;
    let mut pending: [PendingNak; MAX_PENDING] = [unsafe { std::mem::zeroed() }; MAX_PENDING];

    let nak = PendingNak {
        dest_addr: unsafe { std::mem::zeroed() },
        session_id: 42,
        stream_id: 10,
        active_term_id: 100,
        term_offset: 1024,
        length: 1408,
    };

    c.bench_function("loss: queue_nak (array copy)", |b| {
        b.iter(|| {
            let mut len = 0usize;
            if len < MAX_PENDING {
                pending[len] = black_box(nak);
                len += 1;
            }
            black_box(len);
        });
    });
}

// ──────────────────── Full loss scan simulation ────────────────────

/// End-to-end: scan 16 incoming frame offsets, detect a gap at frame #8,
/// build and write a NAK into a scratch buffer. This is the representative
/// "hot path" cost of loss detection.
fn bench_loss_scan_full(c: &mut Criterion) {
    let mut nak_buf = [0u8; NAK_TOTAL_LENGTH];

    c.bench_function("loss: full scan (16 frames, 1 gap, NAK)", |b| {
        b.iter(|| {
            let mut expected: i32 = 0;
            let frame_len: i32 = 1440; // DATA_HEADER_LENGTH + 1408 payload
            let mut gap_found = false;
            let mut gap_offset: i32 = 0;
            let mut gap_length: i32 = 0;

            // Frame 8 is missing; frames 9..15 arrive shifted.
            for i in 0..16i32 {
                let received = if i < 8 {
                    i * frame_len
                } else {
                    // Frame 8 skipped → frames 8..15 map to offsets 9..16.
                    (i + 1) * frame_len
                };

                let gap = received.wrapping_sub(expected);
                if gap > 0 && gap < (i32::MAX / 2) && !gap_found {
                    gap_found = true;
                    gap_offset = expected;
                    gap_length = gap;
                }
                expected = received.wrapping_add(frame_len);
            }

            if black_box(gap_found) {
                let nak = NakHeader {
                    frame_header: FrameHeader {
                        frame_length: NAK_TOTAL_LENGTH as i32,
                        version: CURRENT_VERSION,
                        flags: 0,
                        frame_type: FRAME_TYPE_NAK,
                    },
                    session_id: 42,
                    stream_id: 10,
                    active_term_id: 0,
                    term_offset: gap_offset,
                    length: gap_length,
                };
                nak.write(&mut nak_buf);
            }
            black_box(&nak_buf);
        });
    });
}

/// Scan with NO gaps — measures the baseline scan cost when data is clean.
fn bench_loss_scan_no_gap(c: &mut Criterion) {
    c.bench_function("loss: full scan (16 frames, 0 gaps)", |b| {
        b.iter(|| {
            let mut expected: i32 = 0;
            let frame_len: i32 = 1440;
            let mut gap_found = false;

            for i in 0..16i32 {
                let received = i * frame_len;
                let gap = received.wrapping_sub(expected);
                if gap > 0 && gap < (i32::MAX / 2) {
                    gap_found = true;
                }
                expected = received.wrapping_add(frame_len);
            }
            black_box(gap_found);
        });
    });
}

criterion_group!(
    benches,
    bench_gap_detection_hit,
    bench_gap_detection_miss,
    bench_nak_build_and_write,
    bench_queue_nak,
    bench_loss_scan_full,
    bench_loss_scan_no_gap,
);
criterion_main!(benches);
