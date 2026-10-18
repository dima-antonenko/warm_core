//! Тот же рабочий набор, что в `examples/compare.rs`, только короче — для criterion.

use std::hint::black_box;
use std::thread;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};

use warm_core::Pool;

const PASSES: usize = 8;
const BUF: usize = 256 * 1024;

fn scan(buf: &mut [u8]) -> u64 {
    let buf = black_box(buf);
    let mut acc = 0u64;
    for byte in buf.iter_mut() {
        *byte = byte.wrapping_add(1);
        acc = acc.wrapping_add(*byte as u64);
    }
    black_box(acc)
}

fn work(id: usize, yield_between_passes: bool) -> u64 {
    let mut buf = vec![id as u8; BUF];
    let mut acc = 0u64;
    for _ in 0..PASSES {
        acc ^= scan(&mut buf);
        if yield_between_passes {
            thread::yield_now();
        }
    }
    acc
}

fn jobs() -> usize {
    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .saturating_mul(2)
}

fn baseline() -> u64 {
    let handles: Vec<_> = (0..jobs())
        .map(|id| thread::spawn(move || work(id, true)))
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.join().expect("поток базы"))
        .fold(0, |acc, value| acc ^ value)
}

fn warm(pool: &Pool) -> u64 {
    let handles: Vec<_> = (0..jobs())
        .map(|id| pool.execute(move || work(id, false)))
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.wait())
        .fold(0, |acc, value| acc ^ value)
}

fn locality(c: &mut Criterion) {
    let pool = Pool::new().expect("пул");
    let mut group = c.benchmark_group("l2_working_set");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("std_thread_yield", |b| b.iter(|| black_box(baseline())));
    group.bench_function("warm_core", |b| b.iter(|| black_box(warm(&pool))));
    group.finish();
}

criterion_group!(benches, locality);
criterion_main!(benches);
