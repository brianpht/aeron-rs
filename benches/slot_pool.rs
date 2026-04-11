//! Benchmark: SlotPool alloc/free cycle latency.
//!
//! Measures the O(1) alloc_recv → free_recv and alloc_send → free_send
//! hot-path cost to verify no hidden allocation overhead.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use aeron_rs::media::buffer_pool::{SlotPool, SlotState};

fn bench_alloc_free_recv(c: &mut Criterion) {
    let mut pool = SlotPool::new(64, 0);

    c.bench_function("SlotPool alloc_recv+free_recv", |b| {
        b.iter(|| {
            let idx = pool.alloc_recv().unwrap();
            // Simulate CQE reap: mark slot Free before returning.
            pool.recv_slots[idx as usize].state = SlotState::Free;
            pool.free_recv(black_box(idx));
        });
    });
}

fn bench_alloc_free_send(c: &mut Criterion) {
    let mut pool = SlotPool::new(0, 64);

    c.bench_function("SlotPool alloc_send+free_send", |b| {
        b.iter(|| {
            let idx = pool.alloc_send().unwrap();
            pool.send_slots[idx as usize].state = SlotState::Free;
            pool.free_send(black_box(idx));
        });
    });
}

fn bench_alloc_burst_recv(c: &mut Criterion) {
    let count = 16usize;
    let mut pool = SlotPool::new(count, 0);

    c.bench_function("SlotPool alloc_recv burst 16", |b| {
        b.iter(|| {
            let mut indices = [0u16; 16];
            for item in indices.iter_mut().take(count) {
                *item = pool.alloc_recv().unwrap();
            }
            for item in indices.iter().take(count) {
                pool.recv_slots[*item as usize].state = SlotState::Free;
                pool.free_recv(*item);
            }
            black_box(&indices);
        });
    });
}

criterion_group!(
    benches,
    bench_alloc_free_recv,
    bench_alloc_free_send,
    bench_alloc_burst_recv
);
criterion_main!(benches);
