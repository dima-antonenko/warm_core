//! Один и тот же буфер на ядре: `execute_on` против `execute`.
//!
//! Буфер размера L2 заполняется циклом через `execute_on`, затем много раз
//! обходится снова. Следующий индекс читается из только что загруженной
//! ячейки, поэтому предвыборка не прячет промах. `execute_on` оставляет
//! задачу на этом ядре. `execute` кладёт её в общую очередь, и сосед может
//! её украсть, поэтому строки буфера на прежнем ядре уже не обещаны.
//!
//! Печатается время внутри задачи (p50 и p99), не время ожидания в очереди.
//! Промахи L1d и LLC читаются через `perf_event_open` на потоке каждого
//! worker. Если ядро запретило счётчики, пример всё равно печатает время.

use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::time::Instant;

use warm_core::{CoreId, Pool, Topology};

const WARMUP_ROUNDS: usize = 2;
const ROUNDS: usize = 128;

struct Buffer {
    core: CoreId,
    bytes: Vec<u64>,
}

struct Phase {
    buffers: Vec<Buffer>,
    samples: Vec<u128>,
    checksum: u64,
    misses: Option<(u64, u64)>,
}

fn main() {
    let pool = Pool::new().expect("пул");
    let bytes = Topology::detect()
        .ok()
        .and_then(|topology| topology.l2_size_bytes())
        .unwrap_or(256 * 1024) as usize;
    let slots = (bytes / 8).max(2);
    let cores = pool.cores();
    println!(
        "ядер: {}, буфер: {} КБ, шагов на задачу: {slots}, раундов: {ROUNDS}",
        cores.len(),
        (slots * 8) / 1024,
    );
    println!("время — внутри задачи, от начала обхода до его конца");

    let buffers = warm(&pool, &cores, slots);
    let counters = match attach(&pool, &cores) {
        Ok(counters) => Some(counters),
        Err(err) => {
            println!(
                "счётчики недоступны ({err}). При perf_event_paranoid >= 3 ядро не даёт их обычному пользователю. Время ниже всё равно посчитано."
            );
            None
        }
    };

    let pinned = measure(&pool, buffers, true, counters.as_deref());
    let shared = measure(&pool, pinned.buffers, false, counters.as_deref());
    let pinned_samples = pinned.samples;
    let shared_samples = shared.samples;
    let pinned_misses = pinned.misses;
    let shared_misses = shared.misses;
    let pinned_sum = pinned.checksum;
    let shared_sum = shared.checksum;

    println!(
        "{:<12}  {:>12}  {:>12}  {:>16}  {:>16}",
        "вызов", "p50", "p99", "L1d read miss", "LLC load miss"
    );
    print_row("execute_on", &pinned_samples, pinned_misses);
    print_row("execute", &shared_samples, shared_misses);
    assert_eq!(
        pinned_sum, shared_sum,
        "оба вызова должны обойти одни и те же буферы"
    );
    println!("контрольная сумма: {pinned_sum:#x}");
}

fn warm(pool: &Pool, cores: &[CoreId], slots: usize) -> Vec<Buffer> {
    let mut handles = Vec::with_capacity(cores.len());
    for (index, core) in cores.iter().copied().enumerate() {
        let mut buf = vec![0u64; slots];
        handles.push((
            core,
            pool.execute_on(core, move || {
                init_cycle(&mut buf, index as u64);
                buf
            })
            .expect("слот"),
        ));
    }
    handles
        .into_iter()
        .map(|(core, handle)| Buffer {
            core,
            bytes: handle.wait(),
        })
        .collect()
}

fn measure(pool: &Pool, buffers: Vec<Buffer>, pinned: bool, counters: Option<&[Pair]>) -> Phase {
    let buffers = replay(pool, buffers, true, WARMUP_ROUNDS, false).buffers;
    let counting = counters.and_then(begin_counting).is_some();
    let played = replay(pool, buffers, pinned, ROUNDS, true);
    let misses = if counting {
        counters.and_then(finish_counting)
    } else {
        None
    };
    Phase {
        buffers: played.buffers,
        samples: played.samples,
        checksum: played.checksum,
        misses,
    }
}

fn replay(
    pool: &Pool,
    mut buffers: Vec<Buffer>,
    pinned: bool,
    rounds: usize,
    record: bool,
) -> Phase {
    let mut samples = Vec::new();
    let mut checksum = 0u64;
    for _ in 0..rounds {
        let mut handles = Vec::with_capacity(buffers.len());
        for Buffer { core, bytes } in buffers {
            let job = move || {
                let started = Instant::now();
                let acc = chase(&bytes);
                (core, bytes, started.elapsed().as_nanos(), acc)
            };
            let handle = if pinned {
                pool.execute_on(core, job).expect("слот")
            } else {
                pool.execute(job).expect("слот")
            };
            handles.push(handle);
        }
        buffers = Vec::with_capacity(handles.len());
        for handle in handles {
            let (core, bytes, nanos, acc) = handle.wait();
            checksum = checksum.wrapping_add(acc);
            if record {
                samples.push(nanos);
            }
            buffers.push(Buffer { core, bytes });
        }
    }
    Phase {
        buffers,
        samples,
        checksum,
        misses: None,
    }
}

fn init_cycle(buf: &mut [u64], seed: u64) {
    let n = buf.len();
    let mut order: Vec<usize> = (0..n).collect();
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..n).rev() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (state >> 33) as usize % (i + 1);
        order.swap(i, j);
    }
    for i in 0..n {
        buf[order[i]] = order[(i + 1) % n] as u64;
    }
}

fn chase(buf: &[u64]) -> u64 {
    let buf = std::hint::black_box(buf);
    let mut index = 0usize;
    let mut acc = 0u64;
    for _ in 0..buf.len() {
        index = buf[index] as usize;
        acc = acc.wrapping_add(index as u64);
    }
    std::hint::black_box(acc)
}

fn print_row(name: &str, samples: &[u128], misses: Option<(u64, u64)>) {
    let mut samples = samples.to_vec();
    let (l1, llc) = match misses {
        Some((l1, llc)) => (l1.to_string(), llc.to_string()),
        None => ("—".to_string(), "—".to_string()),
    };
    println!(
        "{name:<12}  {:>12}  {:>12}  {l1:>16}  {llc:>16}",
        format_ns(percentile(&mut samples, 0.50)),
        format_ns(percentile(&mut samples, 0.99)),
    );
}

fn percentile(samples: &mut [u128], q: f64) -> u128 {
    samples.sort_unstable();
    let index = ((samples.len() as f64) * q).ceil() as usize - 1;
    samples[index.min(samples.len() - 1)]
}

fn format_ns(nanos: u128) -> String {
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.1} µs", nanos as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", nanos as f64 / 1_000_000.0)
    }
}

#[repr(C)]
struct PerfEventAttr {
    typ: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup: u64,
    bp_type: u32,
    _pad: u32,
}

const _: () = assert!(size_of::<PerfEventAttr>() == 64);

struct Pair {
    l1: RawFd,
    llc: RawFd,
}

impl Drop for Pair {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.l1);
            libc::close(self.llc);
        }
    }
}

fn attach(pool: &Pool, cores: &[CoreId]) -> io::Result<Vec<Pair>> {
    let mut handles = Vec::with_capacity(cores.len());
    for core in cores {
        handles.push(
            pool.execute_on(*core, open_pair)
                .map_err(io::Error::other)?,
        );
    }
    let mut pairs = Vec::with_capacity(handles.len());
    let mut failure = None;
    for handle in handles {
        match handle.wait() {
            Ok(pair) => pairs.push(pair),
            Err(err) => failure = Some(err),
        }
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(pairs),
    }
}

fn open_pair() -> io::Result<Pair> {
    let l1 = open_counter(0)?;
    match open_counter(2) {
        Ok(llc) => Ok(Pair { l1, llc }),
        Err(err) => {
            unsafe { libc::close(l1) };
            Err(err)
        }
    }
}

fn open_counter(cache: u64) -> io::Result<RawFd> {
    let mut attr = unsafe { std::mem::zeroed::<PerfEventAttr>() };
    attr.typ = 3;
    attr.size = 64;
    attr.config = cache | (1 << 16);
    attr.flags = (1 << 0) | (1 << 5) | (1 << 6);
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            &attr,
            0 as libc::pid_t,
            -1 as libc::c_int,
            -1 as libc::c_int,
            8 as libc::c_ulong,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd as RawFd)
    }
}

fn begin_counting(pairs: &[Pair]) -> Option<()> {
    if ioctl_all(pairs, 3) && ioctl_all(pairs, 0) {
        Some(())
    } else {
        None
    }
}

fn finish_counting(pairs: &[Pair]) -> Option<(u64, u64)> {
    if !ioctl_all(pairs, 1) {
        return None;
    }
    let mut l1 = 0u64;
    let mut llc = 0u64;
    for pair in pairs {
        l1 = l1.wrapping_add(read_counter(pair.l1).ok()?);
        llc = llc.wrapping_add(read_counter(pair.llc).ok()?);
    }
    Some((l1, llc))
}

fn ioctl_all(pairs: &[Pair], nr: u64) -> bool {
    let request = ((b'$' as libc::c_ulong) << 8) | (nr as libc::c_ulong);
    pairs.iter().all(|pair| unsafe {
        libc::ioctl(pair.l1, request, 0) == 0 && libc::ioctl(pair.llc, request, 0) == 0
    })
}

fn read_counter(fd: RawFd) -> io::Result<u64> {
    let mut value = 0u64;
    let read = unsafe { libc::read(fd, &mut value as *mut u64 as *mut libc::c_void, 8) };
    if read == 8 {
        Ok(value)
    } else {
        Err(io::Error::new(ErrorKind::UnexpectedEof, "счётчик"))
    }
}
