//! Benchmark: NanoClock vs CachedNanoClock latency.
//!
//! Quantifies the syscall savings of using cached() vs nano_time().

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use aeron_rs::clock::{CachedNanoClock, NanoClock};

fn bench_nano_clock(c: &mut Criterion) {
    let clock = NanoClock::new();

    c.bench_function("NanoClock::nano_time", |b| {
        b.iter(|| {
            black_box(clock.nano_time());
        });
    });
}

fn bench_cached_clock_cached(c: &mut Criterion) {
    let mut clock = CachedNanoClock::new();
    clock.update();

    c.bench_function("CachedNanoClock::cached", |b| {
        b.iter(|| {
            black_box(clock.cached());
        });
    });
}

fn bench_cached_clock_update(c: &mut Criterion) {
    let mut clock = CachedNanoClock::new();

    c.bench_function("CachedNanoClock::update", |b| {
        b.iter(|| {
            black_box(clock.update());
        });
    });
}

criterion_group!(benches, bench_nano_clock, bench_cached_clock_cached, bench_cached_clock_update);
criterion_main!(benches);

