//! Тот же рабочий набор, что в `examples/compare.rs`, только короче — для criterion.

use std::hint::black_box;
use std::thread;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rayon::ThreadPool;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

use warm_core::{Pool, pin_to};

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

fn work(id: usize) -> u64 {
    let mut buf = vec![id as u8; BUF];
    let mut acc = 0u64;
    for _ in 0..PASSES {
        acc ^= scan(&mut buf);
    }
    acc
}

fn jobs() -> usize {
    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .saturating_mul(2)
}

fn run_rayon(pool: &ThreadPool) -> u64 {
    let jobs = jobs();
    pool.install(|| {
        (0..jobs)
            .into_par_iter()
            .map(work)
            .reduce(|| 0, |a, b| a ^ b)
    })
}

fn run_warm(pool: &Pool) -> u64 {
    let handles: Vec<_> = (0..jobs())
        .map(|id| pool.execute(move || work(id)))
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.wait())
        .fold(0, |acc, value| acc ^ value)
}

fn locality(c: &mut Criterion) {
    let warm = Pool::new().expect("пул");
    let workers = warm.worker_count();
    let cpus = warm.pinned_cpus();
    let rayon_free = ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .expect("rayon");
    let rayon_pinned = ThreadPoolBuilder::new()
        .num_threads(workers)
        .start_handler(move |index| {
            pin_to(cpus[index]).expect("привязка rayon");
        })
        .build()
        .expect("rayon с affinity");

    let mut group = c.benchmark_group("l2_working_set");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));
    group.bench_function("rayon", |b| b.iter(|| black_box(run_rayon(&rayon_free))));
    group.bench_function("rayon_affinity", |b| {
        b.iter(|| black_box(run_rayon(&rayon_pinned)))
    });
    group.bench_function("warm_core", |b| b.iter(|| black_box(run_warm(&warm))));
    group.finish();
}

criterion_group!(benches, locality);
criterion_main!(benches);
