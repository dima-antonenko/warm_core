//! Сравнение: Rayon без привязки, Rayon с той же привязкой и `warm_core`.
//!
//! У всех трёх вариантов одинаковое число worker — по одному на физическое
//! ядро — и одна и та же работа без `yield_now`. Пара Rayon отличается только
//! маской `sched_setaffinity`. `warm_core` к той же привязке добавляет свои очереди.

use std::hint::black_box;
use std::time::{Duration, Instant};

use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

use warm_core::{Pool, Topology, pin_to};

const PASSES: usize = 40;

fn scan(buf: &mut [u8]) -> u64 {
    let buf = black_box(buf);
    let mut acc = 0u64;
    for byte in buf.iter_mut() {
        *byte = byte.wrapping_add(1);
        acc = acc.wrapping_add(*byte as u64);
    }
    black_box(acc)
}

fn work(id: usize, bytes: usize) -> u64 {
    let mut buf = vec![id as u8; bytes];
    let mut acc = 0u64;
    for _ in 0..PASSES {
        acc ^= scan(&mut buf);
    }
    acc
}

fn run_rayon(pool: &rayon::ThreadPool, jobs: usize, bytes: usize) -> u64 {
    pool.install(|| {
        (0..jobs)
            .into_par_iter()
            .map(|id| work(id, bytes))
            .reduce(|| 0, |a, b| a ^ b)
    })
}

fn run_warm(pool: &Pool, jobs: usize, bytes: usize) -> u64 {
    let handles: Vec<_> = (0..jobs)
        .map(|id| pool.execute(move || work(id, bytes)).expect("слот"))
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.wait())
        .fold(0, |acc, value| acc ^ value)
}

fn speedup(baseline: Duration, variant: Duration) -> Option<f64> {
    let variant = variant.as_secs_f64();
    if variant > 0.0 {
        Some(baseline.as_secs_f64() / variant)
    } else {
        None
    }
}

fn main() {
    let topology = Topology::detect().expect("топология CPU");
    let bytes = topology.l2_size_bytes().unwrap_or(256 * 1024) as usize;
    let warm = Pool::new().expect("пул");
    let workers = warm.worker_count();
    let cpus = warm.pinned_cpus();
    let jobs = topology.cpus.len().saturating_mul(2).max(2);

    let rayon_free = ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .expect("rayon");
    let pinned_cpus = cpus.clone();
    let rayon_pinned = ThreadPoolBuilder::new()
        .num_threads(workers)
        .start_handler(move |index| {
            pin_to(pinned_cpus[index]).expect("привязка rayon");
        })
        .build()
        .expect("rayon с affinity");

    println!(
        "логических CPU: {}, физических ядер в пуле: {}, буфер: {} КБ, задач: {}, проходов: {}",
        topology.cpus.len(),
        workers,
        bytes / 1024,
        jobs,
        PASSES,
    );
    println!("закреплённые CPU: {cpus:?}");

    let started = Instant::now();
    let free_acc = run_rayon(&rayon_free, jobs, bytes);
    let free = started.elapsed();

    let started = Instant::now();
    let pinned_acc = run_rayon(&rayon_pinned, jobs, bytes);
    let pinned = started.elapsed();

    let started = Instant::now();
    let warm_acc = run_warm(&warm, jobs, bytes);
    let warm_time = started.elapsed();

    assert_eq!(
        free_acc, pinned_acc,
        "оба варианта rayon должны посчитать одно и то же"
    );
    assert_eq!(
        free_acc, warm_acc,
        "rayon и warm_core должны посчитать одно и то же"
    );

    println!("rayon:                       {free:.3?}");
    println!("rayon + affinity:            {pinned:.3?}");
    println!("warm_core:                   {warm_time:.3?}");
    if let Some(factor) = speedup(free, pinned) {
        println!("rayon + affinity быстрее в:  {factor:.2}");
    }
    if let Some(factor) = speedup(free, warm_time) {
        println!("warm_core быстрее в:         {factor:.2}");
    }
}
