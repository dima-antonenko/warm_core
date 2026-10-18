//! Пул: один закреплённый worker на ядро и своя очередь задач.
//!
//! Пока в локальной очереди есть работа, worker не смотрит на соседей.
//! Так данные, которые положили на это ядро, доезжают до ALU, пока ещё
//! лежат в его L1/L2. Чужая очередь читается только когда своя пуста.

use crate::affinity;
use crate::queue::Queue;
use crate::topology::{CoreId, Cpu, Topology};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, Thread};

const QUEUE_CAPACITY: usize = 4096;

/// Сколько worker запускать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Один worker на физическое ядро. SMT-сосед не делит с ним L1/L2.
    PhysicalCores,
    /// Worker на каждый логический CPU.
    LogicalCpus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub layout: Layout,
    /// Сколько локальных задач выполнить подряд, прежде чем смотреть на чужие очереди.
    pub batch: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            layout: Layout::PhysicalCores,
            batch: 32,
        }
    }
}

#[derive(Debug)]
pub enum PoolError {
    UnknownCore(CoreId),
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCore(core) => write!(f, "в пуле нет ядра {}", core.0),
        }
    }
}

impl std::error::Error for PoolError {}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Worker {
    cpu: usize,
    core_id: CoreId,
    /// `execute_on`: с этого ядра задачу не крадут, данные остаются в его L1/L2.
    pinned: Queue<Task>,
    /// `execute`: можно забрать, только если у вора своя очередь уже пуста.
    shared: Queue<Task>,
    wake: Mutex<Option<Thread>>,
}

struct Inner {
    workers: Vec<Worker>,
    batch: usize,
    shutdown: AtomicBool,
    rr: AtomicUsize,
}

/// Пул закреплённых потоков.
pub struct Pool {
    inner: Arc<Inner>,
    joins: Vec<JoinHandle<()>>,
}

/// Результат задачи. [`JobHandle::wait`] блокируется, пока worker её не выполнит.
pub struct JobHandle<T> {
    rx: Receiver<thread::Result<T>>,
}

impl<T> JobHandle<T> {
    pub fn wait(self) -> T {
        match self.rx.recv() {
            Ok(Ok(value)) => value,
            Ok(Err(payload)) => panic::resume_unwind(payload),
            Err(_) => panic!("warm_core: worker завершился, не выполнив задачу"),
        }
    }
}

impl Pool {
    pub fn new() -> io::Result<Self> {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> io::Result<Self> {
        let topology = Topology::detect()?;
        Self::from_cpus(select_cpus(&topology, config.layout), config.batch.max(1))
    }

    fn from_cpus(cpus: Vec<Cpu>, batch: usize) -> io::Result<Self> {
        if cpus.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "нет CPU, на которые можно посадить worker",
            ));
        }

        let workers = cpus
            .into_iter()
            .map(|cpu| Worker {
                cpu: cpu.logical_id,
                core_id: cpu.core_id,
                pinned: Queue::new(QUEUE_CAPACITY),
                shared: Queue::new(QUEUE_CAPACITY),
                wake: Mutex::new(None),
            })
            .collect();

        let inner = Arc::new(Inner {
            workers,
            batch,
            shutdown: AtomicBool::new(false),
            rr: AtomicUsize::new(0),
        });

        let worker_count = inner.workers.len();
        let (tx, rx) = mpsc::channel();
        let mut joins = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let worker_inner = Arc::clone(&inner);
            let tx = tx.clone();
            let cpu = inner.workers[index].cpu;
            match thread::Builder::new()
                .name(format!("warm-core-{cpu}"))
                .spawn(move || worker_entry(worker_inner, index, tx))
            {
                Ok(handle) => joins.push(handle),
                Err(err) => {
                    stop(&inner, joins);
                    return Err(err);
                }
            }
        }
        drop(tx);

        for _ in 0..worker_count {
            match rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    stop(&inner, joins);
                    return Err(err);
                }
                Err(_) => {
                    stop(&inner, joins);
                    return Err(io::Error::other("worker завершился при старте"));
                }
            }
        }

        Ok(Self { inner, joins })
    }

    pub fn worker_count(&self) -> usize {
        self.inner.workers.len()
    }

    pub fn cores(&self) -> Vec<CoreId> {
        self.inner
            .workers
            .iter()
            .map(|worker| worker.core_id)
            .collect()
    }

    /// Логические CPU, за которыми закреплены worker. Порядок совпадает с [`Self::cores`].
    pub fn pinned_cpus(&self) -> Vec<usize> {
        self.inner.workers.iter().map(|worker| worker.cpu).collect()
    }

    /// Кладёт задачу на наименее загруженное ядро.
    ///
    /// При равной длине очередей задачи разъезжаются по кругу, а не липнут к нулевому ядру.
    pub fn execute<F, T>(&self, f: F) -> JobHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.submit(self.pick_least_loaded(), false, f)
    }

    /// Оставляет задачу на ядре, где её данные уже горячие.
    pub fn execute_on<F, T>(&self, core: CoreId, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut chosen: Option<(usize, usize)> = None;
        for (index, worker) in self.inner.workers.iter().enumerate() {
            if worker.core_id != core {
                continue;
            }
            let len = worker_load(worker);
            if chosen.map(|(_, best)| len < best).unwrap_or(true) {
                chosen = Some((index, len));
            }
        }
        let Some((index, _)) = chosen else {
            return Err(PoolError::UnknownCore(core));
        };
        Ok(self.submit(index, true, f))
    }

    fn pick_least_loaded(&self) -> usize {
        let mut min_len = usize::MAX;
        let mut candidates = Vec::new();
        for (index, worker) in self.inner.workers.iter().enumerate() {
            let len = worker_load(worker);
            if len < min_len {
                min_len = len;
                candidates.clear();
                candidates.push(index);
            } else if len == min_len {
                candidates.push(index);
            }
        }
        let ticket = self.inner.rr.fetch_add(1, Ordering::Relaxed);
        candidates[ticket % candidates.len()]
    }

    fn submit<F, T>(&self, index: usize, pinned: bool, f: F) -> JobHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let task: Task = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            let _ = tx.send(result);
        });
        let worker = &self.inner.workers[index];
        let queue = if pinned {
            &worker.pinned
        } else {
            &worker.shared
        };
        push_until_accepted(queue, task);
        if let Some(thread) = worker.wake.lock().unwrap().as_ref() {
            thread.unpark();
        }
        JobHandle { rx }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let joins = std::mem::take(&mut self.joins);
        stop(&self.inner, joins);
    }
}

fn worker_load(worker: &Worker) -> usize {
    worker.pinned.len().saturating_add(worker.shared.len())
}

fn select_cpus(topology: &Topology, layout: Layout) -> Vec<Cpu> {
    match layout {
        Layout::PhysicalCores => topology.physical_workers(),
        Layout::LogicalCpus => topology.logical_workers(),
    }
}

fn push_until_accepted(queue: &Queue<Task>, mut task: Task) {
    loop {
        match queue.push(task) {
            Ok(()) => return,
            Err(returned) => {
                task = returned;
                thread::yield_now();
            }
        }
    }
}

/// Будит worker и ждёт, пока они доедят уже поставленные задачи.
fn stop(inner: &Inner, joins: Vec<JoinHandle<()>>) {
    inner.shutdown.store(true, Ordering::Release);
    for worker in &inner.workers {
        if let Some(thread) = worker.wake.lock().unwrap().as_ref() {
            thread.unpark();
        }
    }
    for join in joins {
        join.thread().unpark();
        let _ = join.join();
    }
}

fn worker_entry(inner: Arc<Inner>, index: usize, started: Sender<io::Result<()>>) {
    let cpu = inner.workers[index].cpu;
    if let Err(err) = affinity::pin_to(cpu) {
        let _ = started.send(Err(err));
        return;
    }
    {
        let mut wake = inner.workers[index].wake.lock().unwrap();
        *wake = Some(thread::current());
    }
    if started.send(Ok(())).is_err() {
        return;
    }
    worker_loop(&inner, index);
}

fn worker_loop(inner: &Inner, index: usize) {
    loop {
        if run_local_batch(inner, index) {
            continue;
        }
        if let Some(task) = steal(inner, index) {
            task();
            continue;
        }
        if inner.shutdown.load(Ordering::Acquire) {
            // Drop забирает `&mut Pool`, поэтому новых execute уже нет.
            // Повторный обход доедает то, что уже лежит в очередях.
            if run_local_batch(inner, index) {
                continue;
            }
            if let Some(task) = steal(inner, index) {
                task();
                continue;
            }
            break;
        }
        thread::park();
    }
}

/// Сначала закреплённые задачи этого ядра, потом своя общая очередь.
/// Чужая очередь здесь не читается.
fn run_local_batch(inner: &Inner, index: usize) -> bool {
    let worker = &inner.workers[index];
    let mut ran = false;
    for _ in 0..inner.batch {
        let Some(task) = worker.pinned.pop().or_else(|| worker.shared.pop()) else {
            break;
        };
        task();
        ran = true;
    }
    ran
}

/// Чужая общая очередь — только когда своя пуста. Закреплённые задачи не крадём:
/// их данные должны остаться в L1/L2 того ядра, куда их положили.
/// Сначала самое загруженное ядро.
fn steal(inner: &Inner, index: usize) -> Option<Task> {
    let n = inner.workers.len();
    let mut best: Option<usize> = None;
    let mut best_len = 0;
    for other in 0..n {
        if other == index {
            continue;
        }
        let len = inner.workers[other].shared.len();
        if len > best_len {
            best_len = len;
            best = Some(other);
        }
    }
    if let Some(other) = best
        && let Some(task) = inner.workers[other].shared.pop()
    {
        return Some(task);
    }
    for other in 0..n {
        if other == index || Some(other) == best {
            continue;
        }
        if let Some(task) = inner.workers[other].shared.pop() {
            return Some(task);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::allowed_cpus;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn executes_every_job_once() {
        let pool = Pool::new().unwrap();
        let handles: Vec<_> = (0..200).map(|i| pool.execute(move || i * 2)).collect();
        let mut values: Vec<_> = handles.into_iter().map(|handle| handle.wait()).collect();
        values.sort_unstable();
        assert_eq!(values, (0..200).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn workers_are_pinned_to_a_single_cpu() {
        let pool = Pool::new().unwrap();
        let expected = pool.pinned_cpus();
        let handles: Vec<_> = pool
            .cores()
            .into_iter()
            .enumerate()
            .map(|(index, core)| {
                let want = expected[index];
                pool.execute_on(core, move || (want, allowed_cpus().unwrap()))
                    .unwrap()
            })
            .collect();

        for handle in handles {
            let (want, mask) = handle.wait();
            assert_eq!(mask, vec![want]);
        }
    }

    #[test]
    fn execute_on_rejects_an_unknown_core() {
        let pool = Pool::new().unwrap();
        let result = pool.execute_on(CoreId(u32::MAX), || 1);
        assert!(matches!(
            result,
            Err(PoolError::UnknownCore(CoreId(u32::MAX)))
        ));
    }

    #[test]
    fn a_panicking_job_does_not_take_the_worker_with_it() {
        let pool = Pool::new().unwrap();
        let handle = pool.execute(|| -> i32 { panic!("boom") });
        let panicked = catch_unwind(AssertUnwindSafe(|| handle.wait()));
        assert!(panicked.is_err());
        assert_eq!(pool.execute(|| 7).wait(), 7);
    }

    #[test]
    fn logical_layout_covers_every_cpu() {
        let topology = Topology::detect().unwrap();
        let pool = Pool::with_config(Config {
            layout: Layout::LogicalCpus,
            batch: 8,
        })
        .unwrap();
        assert_eq!(pool.worker_count(), topology.cpus.len());
    }
}
