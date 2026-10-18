//! Сравнение: переподписка `std::thread` с `yield_now` против закреплённого пула.
//!
//! Оба варианта делают одну и ту же работу. База между проходами отдаёт CPU,
//! и чужой буфер размером с L2 вытесняет горячие линии. `warm_core` считает
//! проходы пачкой на одном ядре.

use std::hint::black_box;
use std::thread;
use std::time::Instant;

use warm_core::{Pool, Topology};

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

fn work(id: usize, bytes: usize, yield_between_passes: bool) -> u64 {
    let mut buf = vec![id as u8; bytes];
    let mut acc = 0u64;
    for _ in 0..PASSES {
        acc ^= scan(&mut buf);
        if yield_between_passes {
            thread::yield_now();
        }
    }
    acc
}

fn main() {
    let topology = Topology::detect().expect("топология CPU");
    let bytes = topology.l2_size_bytes().unwrap_or(256 * 1024) as usize;
    let jobs = topology.cpus.len().saturating_mul(2).max(2);
    let pool = Pool::new().expect("пул");

    println!(
        "логических CPU: {}, физических ядер в пуле: {}, буфер: {} КБ, задач: {}, проходов: {}",
        topology.cpus.len(),
        pool.worker_count(),
        bytes / 1024,
        jobs,
        PASSES,
    );
    println!("закреплённые CPU: {:?}", pool.pinned_cpus());

    let started = Instant::now();
    let mut baseline_handles = Vec::with_capacity(jobs);
    for id in 0..jobs {
        baseline_handles.push(thread::spawn(move || work(id, bytes, true)));
    }
    let mut baseline_acc = 0u64;
    for handle in baseline_handles {
        baseline_acc ^= handle.join().expect("поток базы");
    }
    let baseline = started.elapsed();

    let started = Instant::now();
    let mut warm_handles = Vec::with_capacity(jobs);
    for id in 0..jobs {
        warm_handles.push(pool.execute(move || work(id, bytes, false)));
    }
    let mut warm_acc = 0u64;
    for handle in warm_handles {
        warm_acc ^= handle.wait();
    }
    let warm = started.elapsed();

    assert_eq!(
        baseline_acc, warm_acc,
        "оба варианта должны посчитать одно и то же"
    );

    println!("база (std::thread + yield): {baseline:.3?}");
    println!("warm_core:                   {warm:.3?}");
    if warm.as_secs_f64() > 0.0 {
        println!(
            "во сколько раз быстрее:      {:.2}",
            baseline.as_secs_f64() / warm.as_secs_f64()
        );
    }
}
