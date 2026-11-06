//! Пул: один закреплённый worker на ядро и своя очередь задач.
//!
//! Пока в локальной очереди есть работа, worker не смотрит на соседей.
//! Так данные, которые положили на это ядро, доезжают до ALU, пока ещё
//! лежат в его L1/L2. Чужая очередь читается только когда своя пуста.

use crate::affinity;
use crate::gate::FutexGate;
use crate::queue::Queue;
use crate::topology::{CoreId, Cpu, Topology};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

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
    /// Сколько закреплённых задач подряд взять, прежде чем отдать слот общей очереди.
    ///
    /// `1` чередует очереди поровну. Значение меньше единицы [`Pool::with_config`] поднимает до 1.
    /// Счётчик серии не обнуляется на границе пачки.
    pub pinned_per_shared: usize,
    /// Сколько [`Pool::execute`] и [`Pool::execute_on`] ждут свободный слот.
    ///
    /// Ноль не спит: полная очередь сразу даёт [`PoolError::QueueFull`].
    pub submit_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            layout: Layout::PhysicalCores,
            batch: 32,
            pinned_per_shared: 3,
            submit_timeout: Duration::from_secs(1),
        }
    }
}

#[derive(Debug)]
pub enum PoolError {
    UnknownCore(CoreId),
    /// В очереди не нашлось слота до конца ожидания. Замыкание при этом уничтожается.
    QueueFull,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCore(core) => write!(f, "в пуле нет ядра {}", core.0),
            Self::QueueFull => write!(f, "очередь ядра заполнена"),
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
    /// Общий gate закреплённых очередей этого физического ядра.
    pinned_gate: usize,
}

struct Inner {
    workers: Vec<Worker>,
    batch: usize,
    pinned_per_shared: usize,
    submit_timeout: Duration,
    shutdown: AtomicBool,
    rr: AtomicUsize,
    /// Любой `execute` берёт слот из любой общей очереди, в том числе после кражи.
    shared_gate: FutexGate,
    /// По одному на физическое ядро: pop соседа по SMT не должен будить чужое ядро.
    pinned_gates: Vec<FutexGate>,
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
        Self::from_cpus(
            select_cpus(&topology, config.layout),
            config.batch.max(1),
            config.pinned_per_shared.max(1),
            config.submit_timeout,
        )
    }

    fn from_cpus(
        cpus: Vec<Cpu>,
        batch: usize,
        pinned_per_shared: usize,
        submit_timeout: Duration,
    ) -> io::Result<Self> {
        if cpus.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "нет CPU, на которые можно посадить worker",
            ));
        }

        let mut core_ids = Vec::new();
        let mut pinned_gates_of = Vec::with_capacity(cpus.len());
        for cpu in &cpus {
            let gate = if let Some(found) = core_ids.iter().position(|id| *id == cpu.core_id) {
                found
            } else {
                core_ids.push(cpu.core_id);
                core_ids.len() - 1
            };
            pinned_gates_of.push(gate);
        }
        let workers = cpus
            .into_iter()
            .zip(pinned_gates_of)
            .map(|(cpu, pinned_gate)| Worker {
                cpu: cpu.logical_id,
                core_id: cpu.core_id,
                pinned: Queue::new(QUEUE_CAPACITY),
                shared: Queue::new(QUEUE_CAPACITY),
                wake: Mutex::new(None),
                pinned_gate,
            })
            .collect();

        let inner = Arc::new(Inner {
            workers,
            batch,
            pinned_per_shared,
            submit_timeout,
            shutdown: AtomicBool::new(false),
            rr: AtomicUsize::new(0),
            shared_gate: FutexGate::new(),
            pinned_gates: (0..core_ids.len()).map(|_| FutexGate::new()).collect(),
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

    /// Кладёт задачу в общую очередь наименее загруженного ядра.
    ///
    /// При равной длине очереди обходятся по кругу, а не липнут к нулевому ядру.
    /// Если слот есть у соседа, задача уходит туда. Когда свободного слота нет,
    /// вызов ждёт не дольше [`Config::submit_timeout`] и возвращает
    /// [`PoolError::QueueFull`]. Замыкание к этому моменту уже забрано.
    pub fn execute<F, T>(&self, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let candidates: Vec<usize> = (0..self.inner.workers.len()).collect();
        self.submit(&candidates, false, true, f)
    }

    /// Одна попытка [`Self::execute`]. Полная очередь — сразу [`PoolError::QueueFull`].
    pub fn try_execute<F, T>(&self, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let candidates: Vec<usize> = (0..self.inner.workers.len()).collect();
        self.submit(&candidates, false, false, f)
    }

    /// Оставляет задачу на ядре, где её данные уже горячие.
    ///
    /// В режиме [`Layout::LogicalCpus`] пробует обоих worker этого ядра,
    /// начиная с менее загруженного. Ожидание ограничено [`Config::submit_timeout`].
    pub fn execute_on<F, T>(&self, core: CoreId, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.submit_on(core, true, f)
    }

    /// Одна попытка [`Self::execute_on`].
    ///
    /// Нет такого ядра — [`PoolError::UnknownCore`]. Ядро есть, но закреплённая
    /// очередь полна — [`PoolError::QueueFull`].
    pub fn try_execute_on<F, T>(&self, core: CoreId, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.submit_on(core, false, f)
    }

    fn submit_on<F, T>(&self, core: CoreId, wait: bool, f: F) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let candidates: Vec<usize> = self
            .inner
            .workers
            .iter()
            .enumerate()
            .filter(|(_, worker)| worker.core_id == core)
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return Err(PoolError::UnknownCore(core));
        }
        self.submit(&candidates, true, wait, f)
    }

    fn submit<F, T>(
        &self,
        candidates: &[usize],
        pinned: bool,
        wait: bool,
        f: F,
    ) -> Result<JobHandle<T>, PoolError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let mut task: Task = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(f));
            let _ = tx.send(result);
        });
        let gate = if pinned {
            let gate = self.inner.workers[candidates[0]].pinned_gate;
            debug_assert!(
                candidates
                    .iter()
                    .all(|&index| { self.inner.workers[index].pinned_gate == gate })
            );
            &self.inner.pinned_gates[gate]
        } else {
            &self.inner.shared_gate
        };
        let deadline = Instant::now() + self.inner.submit_timeout;

        loop {
            match push_once(&self.inner, candidates, pinned, task) {
                Ok(index) => {
                    unpark_worker(&self.inner.workers[index]);
                    return Ok(JobHandle { rx });
                }
                Err(returned) => task = returned,
            }
            if !wait {
                return Err(PoolError::QueueFull);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PoolError::QueueFull);
            }
            // Сначала публикуем, что мы спим, потом читаем generation и
            // ещё раз пробуем push: pop между этими шагами не теряется.
            let registration = gate.register();
            let expected = registration.generation();
            match push_once(&self.inner, candidates, pinned, task) {
                Ok(index) => {
                    unpark_worker(&self.inner.workers[index]);
                    return Ok(JobHandle { rx });
                }
                Err(returned) => task = returned,
            }
            registration.wait(expected, deadline.saturating_duration_since(Instant::now()));
        }
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

fn unpark_worker(worker: &Worker) {
    if let Some(thread) = worker.wake.lock().unwrap().as_ref() {
        thread.unpark();
    }
}

/// Меньшая очередь раньше. При равенстве старт сдвигается по кругу,
/// чтобы серия задач не липла к одному и тому же ядру.
///
/// Длины снимаются один раз: пока идёт сортировка, worker может забрать
/// задачу, и сравнение по живой длине перестало бы быть порядком.
fn ordered_targets(inner: &Inner, candidates: &[usize]) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize)> = candidates
        .iter()
        .copied()
        .map(|index| (worker_load(&inner.workers[index]), index))
        .collect();
    ranked.sort_by_key(|&item| item);
    if ranked.len() > 1 {
        let best = ranked[0].0;
        let tie_end = ranked
            .iter()
            .position(|(load, _)| *load != best)
            .unwrap_or(ranked.len());
        if tie_end > 1 {
            let ticket = inner.rr.fetch_add(1, Ordering::Relaxed);
            ranked[..tie_end].rotate_left(ticket % tie_end);
        }
    }
    ranked.into_iter().map(|(_, index)| index).collect()
}

fn push_once(
    inner: &Inner,
    candidates: &[usize],
    pinned: bool,
    mut task: Task,
) -> Result<usize, Task> {
    for index in ordered_targets(inner, candidates) {
        let queue = if pinned {
            &inner.workers[index].pinned
        } else {
            &inner.workers[index].shared
        };
        match queue.push(task) {
            Ok(()) => return Ok(index),
            Err(returned) => task = returned,
        }
    }
    Err(task)
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
    // Серия живёт дольше пачки: иначе `batch == 1` при квоте 3 снова
    // отдавал бы каждый слот закреплённой очереди.
    let mut pinned_streak = 0;
    loop {
        if run_local_batch(inner, index, &mut pinned_streak) {
            continue;
        }
        if let Some(task) = steal(inner, index) {
            task();
            continue;
        }
        if inner.shutdown.load(Ordering::Acquire) {
            // Drop забирает `&mut Pool`, поэтому новых execute уже нет.
            // Повторный обход доедает то, что уже лежит в очередях.
            if run_local_batch(inner, index, &mut pinned_streak) {
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

/// Своя закреплённая и своя общая очередь делят пачку: `pinned_per_shared`
/// закреплённых, затем одна общая. Пустая предпочитаемая очередь отдаёт слот
/// другой, worker не простаивает. Чужая очередь здесь не читается.
fn run_local_batch(inner: &Inner, index: usize, pinned_streak: &mut usize) -> bool {
    let mut ran = false;
    for _ in 0..inner.batch {
        let task = if *pinned_streak >= inner.pinned_per_shared {
            if let Some(task) = pop_shared(inner, index) {
                *pinned_streak = 0;
                Some(task)
            } else {
                // Общей нет — слот забирает закреплённая, долг перед общей остаётся.
                pop_pinned(inner, index)
            }
        } else if let Some(task) = pop_pinned(inner, index) {
            *pinned_streak += 1;
            Some(task)
        } else if let Some(task) = pop_shared(inner, index) {
            *pinned_streak = 0;
            Some(task)
        } else {
            None
        };
        let Some(task) = task else {
            break;
        };
        task();
        ran = true;
    }
    ran
}

fn pop_pinned(inner: &Inner, index: usize) -> Option<Task> {
    let gate = inner.workers[index].pinned_gate;
    let task = inner.workers[index].pinned.pop()?;
    inner.pinned_gates[gate].note_slot_freed();
    Some(task)
}

fn pop_shared(inner: &Inner, index: usize) -> Option<Task> {
    let task = inner.workers[index].shared.pop()?;
    inner.shared_gate.note_slot_freed();
    Some(task)
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
        && let Some(task) = pop_shared(inner, other)
    {
        return Some(task);
    }
    for other in 0..n {
        if other == index || Some(other) == best {
            continue;
        }
        if let Some(task) = pop_shared(inner, other) {
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
    use std::sync::mpsc::Sender;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn executes_every_job_once() {
        let pool = Pool::new().unwrap();
        let handles: Vec<_> = (0..200)
            .map(|i| pool.execute(move || i * 2).unwrap())
            .collect();
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
        let handle = pool.execute(|| -> i32 { panic!("boom") }).unwrap();
        let panicked = catch_unwind(AssertUnwindSafe(|| handle.wait()));
        assert!(panicked.is_err());
        assert_eq!(pool.execute(|| 7).unwrap().wait(), 7);
    }

    #[test]
    fn logical_layout_covers_every_cpu() {
        let topology = Topology::detect().unwrap();
        let pool = Pool::with_config(Config {
            layout: Layout::LogicalCpus,
            batch: 8,
            pinned_per_shared: 3,
            submit_timeout: Duration::from_secs(1),
        })
        .unwrap();
        assert_eq!(pool.worker_count(), topology.cpus.len());
    }

    #[test]
    fn shared_task_runs_within_pinned_quota() {
        let pool = Pool::with_config(Config {
            layout: Layout::PhysicalCores,
            batch: 8,
            pinned_per_shared: 3,
            submit_timeout: Duration::from_secs(1),
        })
        .unwrap();

        struct ReleaseAll(Vec<Sender<()>>);
        impl Drop for ReleaseAll {
            fn drop(&mut self) {
                for tx in &self.0 {
                    let _ = tx.send(());
                }
            }
        }

        let worker_count = pool.worker_count();
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut releases = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            let ready_tx = ready_tx.clone();
            pool.submit(&[index], true, true, move || {
                let _ = ready_tx.send(());
                let _ = release_rx.recv();
            })
            .unwrap();
        }
        drop(ready_tx);
        for _ in 0..worker_count {
            ready_rx.recv().expect("worker не дошёл до шлагбаума");
        }
        let guard = ReleaseAll(releases);

        let pinned_before_shared = Arc::new(AtomicUsize::new(0));
        let shared_started = Arc::new(AtomicBool::new(false));
        for _ in 0..64 {
            let pinned_before_shared = Arc::clone(&pinned_before_shared);
            let shared_started = Arc::clone(&shared_started);
            pool.submit(&[0], true, true, move || {
                if !shared_started.load(Ordering::Acquire) {
                    pinned_before_shared.fetch_add(1, Ordering::Relaxed);
                }
            })
            .unwrap();
        }
        let pinned_before_shared = Arc::clone(&pinned_before_shared);
        let shared_started = Arc::clone(&shared_started);
        let shared = pool
            .submit(&[0], false, true, move || {
                let before = pinned_before_shared.load(Ordering::Relaxed);
                shared_started.store(true, Ordering::Release);
                before
            })
            .unwrap();

        let _ = guard.0[0].send(());
        let before = shared.wait();
        assert!(
            before <= 3,
            "общая задача стартовала после {before} закреплённых"
        );
        drop(guard);
    }

    fn config_with_timeout(submit_timeout: Duration) -> Config {
        Config {
            layout: Layout::PhysicalCores,
            batch: 8,
            pinned_per_shared: 3,
            submit_timeout,
        }
    }

    struct ReleaseAll(Vec<Sender<()>>);

    impl Drop for ReleaseAll {
        fn drop(&mut self) {
            for tx in &self.0 {
                let _ = tx.send(());
            }
        }
    }

    fn block_workers(pool: &Pool) -> ReleaseAll {
        let worker_count = pool.worker_count();
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut releases = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            let ready_tx = ready_tx.clone();
            pool.submit(&[index], true, true, move || {
                let _ = ready_tx.send(());
                let _ = release_rx.recv();
            })
            .unwrap();
        }
        drop(ready_tx);
        for _ in 0..worker_count {
            ready_rx.recv().expect("worker не дошёл до шлагбаума");
        }
        ReleaseAll(releases)
    }

    fn fill_queue(pool: &Pool, index: usize, pinned: bool) {
        for _ in 0..QUEUE_CAPACITY {
            pool.submit(&[index], pinned, false, || {})
                .expect("слот для заполнения");
        }
    }

    fn fill_shared(pool: &Pool) {
        for index in 0..pool.worker_count() {
            fill_queue(pool, index, false);
        }
    }

    #[test]
    fn try_execute_returns_queue_full_without_waiting() {
        let pool = Pool::with_config(config_with_timeout(Duration::from_secs(5))).unwrap();
        let guard = block_workers(&pool);
        fill_shared(&pool);

        let started = Instant::now();
        let result = pool.try_execute(|| 1);
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(PoolError::QueueFull)));
        assert!(
            elapsed < Duration::from_millis(200),
            "try_execute ждал слот: {elapsed:?}"
        );
        drop(guard);
    }

    #[test]
    fn execute_returns_queue_full_when_the_timeout_expires() {
        let timeout = Duration::from_millis(80);
        let pool = Pool::with_config(config_with_timeout(timeout)).unwrap();
        let guard = block_workers(&pool);
        fill_shared(&pool);

        let started = Instant::now();
        let result = pool.execute(|| 1);
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(PoolError::QueueFull)));
        assert!(
            elapsed + Duration::from_millis(5) >= timeout,
            "execute вернулся раньше срока: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "execute не остановился по таймауту: {elapsed:?}"
        );
        drop(guard);
    }

    #[test]
    fn execute_proceeds_when_a_slot_frees() {
        let pool = Pool::with_config(config_with_timeout(Duration::from_secs(2))).unwrap();
        let guard = block_workers(&pool);
        fill_shared(&pool);

        thread::scope(|scope| {
            let running = scope.spawn(|| pool.execute(|| 7));
            let started = Instant::now();
            while pool.inner.shared_gate.waiter_count() == 0 {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "execute не заснул на полной очереди"
                );
                thread::yield_now();
            }
            let _ = guard.0[0].send(());
            assert_eq!(running.join().unwrap().unwrap().wait(), 7);
        });
        drop(guard);
    }

    #[test]
    fn try_execute_on_distinguishes_unknown_core_from_full_queue() {
        let pool = Pool::new().unwrap();
        let unknown = pool.try_execute_on(CoreId(u32::MAX), || 1);
        assert!(matches!(
            unknown,
            Err(PoolError::UnknownCore(CoreId(u32::MAX)))
        ));

        let guard = block_workers(&pool);
        let core = pool.cores()[0];
        for (index, worker) in pool.inner.workers.iter().enumerate() {
            if worker.core_id == core {
                fill_queue(&pool, index, true);
            }
        }
        let full = pool.try_execute_on(core, || 1);
        assert!(matches!(full, Err(PoolError::QueueFull)));
        drop(guard);
    }
}
