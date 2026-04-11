// Benchmark: SubscriberImage::poll_fragments hot-path cost.
//
// Measures the subscriber read path:
//   - poll_fragments: single frame (append 1, poll 1)
//   - poll_fragments: batch 16 frames (append 16, poll all)
//   - poll_fragments: empty (no data available)
//   - poll_fragments: gap skip (1 gap in 16 frames)
//   - poll_fragments: pad frame skip (1 pad between data frames)
//
// Uses the shared_image API directly (no agent threads, no io_uring).
// Isolates the Acquire-load scan loop, gap detection, and pad handling.
//
// Target: < 30 ns per frame (normal path), < 200 ns per gap skip.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use aeron_rs::frame::*;
use aeron_rs::media::shared_image::{ReceiverImage, SubscriberImage, new_shared_image};
use aeron_rs::media::term_buffer::{FRAME_ALIGNMENT, atomic_frame_length_store};

// -- Constants --

const TERM_LENGTH: u32 = 64 * 1024; // 64 KiB
const SESSION_ID: i32 = 42;
const STREAM_ID: i32 = 10;
const INITIAL_TERM_ID: i32 = 0;

// -- Helpers --

fn make_image() -> (ReceiverImage, SubscriberImage) {
    new_shared_image(
        SESSION_ID,
        STREAM_ID,
        INITIAL_TERM_ID,
        INITIAL_TERM_ID,
        TERM_LENGTH,
        0,
    )
    .expect("valid image params")
}

fn make_data_header(frame_length: i32, term_offset: i32, term_id: i32) -> DataHeader {
    DataHeader {
        frame_header: FrameHeader {
            frame_length,
            version: CURRENT_VERSION,
            flags: DATA_FLAG_BEGIN | DATA_FLAG_END,
            frame_type: FRAME_TYPE_DATA,
        },
        term_offset,
        session_id: SESSION_ID,
        stream_id: STREAM_ID,
        term_id,
        reserved_value: 0,
    }
}

/// Append N header-only frames starting at the current receiver position.
/// Returns the new offset after all appends.
fn append_n_frames(recv: &mut ReceiverImage, n: usize, start_offset: u32, term_id: i32) -> u32 {
    let frame_len = DATA_HEADER_LENGTH as i32;
    let mut offset = start_offset;
    for _ in 0..n {
        let hdr = make_data_header(frame_len, offset as i32, term_id);
        let new_off = recv.append_frame(0, offset, &hdr, &[]).expect("append");
        offset = new_off;
    }
    offset
}

// -- Benchmarks --

/// Empty poll - no data available. Measures the baseline cost of
/// position calculation + partition lookup + single Acquire load
/// that returns frame_length == 0.
fn bench_poll_empty(c: &mut Criterion) {
    let (_recv, mut sub) = make_image();

    c.bench_function("poll_fragments: empty (no data)", |b| {
        b.iter(|| {
            let n = sub.poll_fragments(|_, _, _| {}, black_box(10));
            black_box(n);
        });
    });
}

/// Single frame: append 1 header-only frame, poll it, repeat.
/// Each iteration appends + polls + advances position.
fn bench_poll_single_frame(c: &mut Criterion) {
    let (mut recv, mut sub) = make_image();
    let frame_len = DATA_HEADER_LENGTH as i32;
    let aligned = FRAME_ALIGNMENT as u32;
    let mut offset = 0u32;
    let mut term_id = INITIAL_TERM_ID;

    c.bench_function("poll_fragments: single frame", |b| {
        b.iter(|| {
            // Reset to term start if approaching end.
            if offset + aligned > TERM_LENGTH {
                // Move to next term - create fresh image pair to avoid
                // complexity of term rotation in the benchmark.
                let (r, s) = make_image();
                recv = r;
                sub = s;
                offset = 0;
                term_id = INITIAL_TERM_ID;
            }

            let hdr = make_data_header(frame_len, offset as i32, term_id);
            let new_off = recv.append_frame(0, offset, &hdr, &[]).expect("append");
            recv.advance_receiver_position(new_off as i64);
            offset = new_off;

            let n = sub.poll_fragments(|_, _, _| {}, black_box(1));
            black_box(n);
        });
    });
}

/// Batch of 16 frames: append 16, advance receiver position, poll all.
/// Measures amortised per-frame cost in a burst.
fn bench_poll_batch_16(c: &mut Criterion) {
    let (mut recv, mut sub) = make_image();
    let mut offset = 0u32;
    let batch = 16usize;
    let frame_aligned = FRAME_ALIGNMENT as u32;

    c.bench_function("poll_fragments: batch 16 frames", |b| {
        b.iter(|| {
            // Reset if not enough room.
            if offset + frame_aligned * batch as u32 > TERM_LENGTH {
                let (r, s) = make_image();
                recv = r;
                sub = s;
                offset = 0;
            }

            let new_off = append_n_frames(&mut recv, batch, offset, INITIAL_TERM_ID);
            recv.advance_receiver_position(new_off as i64);
            offset = new_off;

            let n = sub.poll_fragments(|_, _, _| {}, black_box(batch as i32));
            black_box(n);
        });
    });
}

/// Batch of 64 frames: measures throughput at higher batch sizes.
fn bench_poll_batch_64(c: &mut Criterion) {
    let (mut recv, mut sub) = make_image();
    let mut offset = 0u32;
    let batch = 64usize;
    let frame_aligned = FRAME_ALIGNMENT as u32;

    c.bench_function("poll_fragments: batch 64 frames", |b| {
        b.iter(|| {
            if offset + frame_aligned * batch as u32 > TERM_LENGTH {
                let (r, s) = make_image();
                recv = r;
                sub = s;
                offset = 0;
            }

            let new_off = append_n_frames(&mut recv, batch, offset, INITIAL_TERM_ID);
            recv.advance_receiver_position(new_off as i64);
            offset = new_off;

            let n = sub.poll_fragments(|_, _, _| {}, black_box(batch as i32));
            black_box(n);
        });
    });
}

/// Gap skip: 16 frames with a gap at frame #8.
/// Frames 0-7 present, frame 8 missing (gap), frames 9-15 present.
/// Measures the gap detection (Acquire load of receiver_position) +
/// forward scan cost.
///
/// Uses PerIteration batch size because setup allocates a fresh image
/// (256 KiB SharedLogBuffer) per iteration.
fn bench_poll_gap_skip(c: &mut Criterion) {
    // Use a fresh image per batch to control gap placement precisely.
    c.bench_function("poll_fragments: gap skip (1 gap in 16 frames)", |b| {
        b.iter_batched(
            || {
                let (mut recv, sub) = make_image();
                let frame_len = DATA_HEADER_LENGTH as i32;
                let aligned = FRAME_ALIGNMENT as u32;

                // Write frames 0-7 at offsets 0, 32, 64, ..., 224.
                for i in 0..8u32 {
                    let off = i * aligned;
                    let hdr = make_data_header(frame_len, off as i32, INITIAL_TERM_ID);
                    recv.append_frame(0, off, &hdr, &[]).expect("append");
                }
                // Skip frame 8 (gap at offset 256).
                // Write frames 9-15 at offsets 288, 320, ..., 480.
                for i in 9..16u32 {
                    let off = i * aligned;
                    let hdr = make_data_header(frame_len, off as i32, INITIAL_TERM_ID);
                    recv.append_frame(0, off, &hdr, &[]).expect("append");
                }
                // Receiver position past all written frames.
                recv.advance_receiver_position(16 * aligned as i64);

                (recv, sub)
            },
            |(_recv, mut sub)| {
                let n = sub.poll_fragments(|_, _, _| {}, black_box(100));
                black_box(n);
            },
            criterion::BatchSize::PerIteration,
        );
    });
}

/// Pad frame skip: data frame, pad frame, data frame.
/// Measures the overhead of FRAME_TYPE_PAD detection in the scan loop.
///
/// Uses PerIteration batch size because setup allocates a fresh image.
fn bench_poll_pad_skip(c: &mut Criterion) {
    c.bench_function("poll_fragments: pad frame skip", |b| {
        b.iter_batched(
            || {
                let (mut recv, sub) = make_image();
                let aligned = FRAME_ALIGNMENT as u32;

                // Data frame at offset 0.
                let hdr0 = make_data_header(DATA_HEADER_LENGTH as i32, 0, INITIAL_TERM_ID);
                recv.append_frame(0, 0, &hdr0, &[]).expect("append");

                // Pad frame at offset 32, covering 64 bytes.
                unsafe {
                    let buf_ptr = recv.log_ptr();
                    let pad_hdr = DataHeader {
                        frame_header: FrameHeader {
                            frame_length: 0, // committed atomically below
                            version: CURRENT_VERSION,
                            flags: 0,
                            frame_type: FRAME_TYPE_PAD,
                        },
                        term_offset: aligned as i32,
                        session_id: SESSION_ID,
                        stream_id: STREAM_ID,
                        term_id: INITIAL_TERM_ID,
                        reserved_value: 0,
                    };
                    std::ptr::copy_nonoverlapping(
                        &pad_hdr as *const DataHeader as *const u8,
                        buf_ptr.add(aligned as usize),
                        DATA_HEADER_LENGTH,
                    );
                    atomic_frame_length_store(buf_ptr, aligned as usize, 2 * aligned as i32);
                }

                // Data frame at offset 96 (after 32-byte data + 64-byte pad).
                let off2 = 3 * aligned;
                let hdr2 =
                    make_data_header(DATA_HEADER_LENGTH as i32, off2 as i32, INITIAL_TERM_ID);
                recv.append_frame(0, off2, &hdr2, &[]).expect("append");

                recv.advance_receiver_position(4 * aligned as i64);

                (recv, sub)
            },
            |(_recv, mut sub)| {
                let n = sub.poll_fragments(|_, _, _| {}, black_box(100));
                black_box(n);
            },
            criterion::BatchSize::PerIteration,
        );
    });
}

/// Single frame with 64-byte payload - measures the cost including
/// payload slice construction and handler invocation with data.
fn bench_poll_single_with_payload(c: &mut Criterion) {
    let (mut recv, mut sub) = make_image();
    let payload = [0xABu8; 64];
    let frame_len = DATA_HEADER_LENGTH as i32 + payload.len() as i32;
    let aligned = FRAME_ALIGNMENT as u32 * 3; // 96 bytes -> aligned to 96 (3 * 32)
    let mut offset = 0u32;

    c.bench_function("poll_fragments: single frame 64B payload", |b| {
        b.iter(|| {
            if offset + aligned > TERM_LENGTH {
                let (r, s) = make_image();
                recv = r;
                sub = s;
                offset = 0;
            }

            let hdr = make_data_header(frame_len, offset as i32, INITIAL_TERM_ID);
            let new_off = recv
                .append_frame(0, offset, &hdr, &payload)
                .expect("append");
            recv.advance_receiver_position(new_off as i64);
            offset = new_off;

            let n = sub.poll_fragments(
                |data, _, _| {
                    black_box(data.len());
                },
                black_box(1),
            );
            black_box(n);
        });
    });
}

criterion_group!(
    benches,
    bench_poll_empty,
    bench_poll_single_frame,
    bench_poll_batch_16,
    bench_poll_batch_64,
    bench_poll_gap_skip,
    bench_poll_pad_skip,
    bench_poll_single_with_payload,
);
criterion_main!(benches);
