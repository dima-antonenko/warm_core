//! Состояние, которым владеет один worker.
//!
//! Значение создаётся на его потоке и там же уничтожается. Чужой поток не
//! получает `&T`: он ставит замыкание в закреплённую очередь, которую не
//! крадут. [`Shard::call`] ждёт результат. [`Shard::defer`] не ждёт — так
//! ядро посылает работу соседу, не блокируя свой цикл.

use crate::pool::{Inner, Pool, PoolError, ShardSlot, Task};
use crate::topology::CoreId;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::thread::{self, ThreadId};

/// Значение одного ядра.
///
/// Клоны делят одну ячейку. Ссылка наружу не отдаётся: читать и менять
/// состояние можно только замыканием, которое выполнится на потоке-владельце.
/// `T` не обязан быть [`Send`]: с ядра уезжает только то, что вернул [`Shard::call`].
///
/// # Example
///
/// ```
/// let pool = warm_core::Pool::new().expect("пул");
/// let core = pool.cores()[0];
/// let shard = pool.shard_on(core, || 0).unwrap();
/// shard.call(|value| *value += 2).unwrap();
/// assert_eq!(shard.call(|value| *value).unwrap(), 2);
/// ```
pub struct Shard<T> {
    slot: Arc<Slot<T>>,
}

struct Slot<T> {
    value: std::cell::UnsafeCell<Option<Box<T>>>,
    disposed: AtomicBool,
    /// 1, пока [`Shard::call`] или [`Shard::defer`] держит `&mut T` на владельце.
    depth: AtomicUsize,
    /// Последнюю ручку уронили внутри этого `call`. Снять значение можно только после него.
    dispose_pending: AtomicBool,
    handles: AtomicUsize,
    owner: OnceLock<ThreadId>,
    worker: usize,
    core: CoreId,
    pool: Weak<Inner>,
    /// `Drop` шарда не может требовать `T: 'static`, а задача уничтожения — может.
    /// Указатель записывается в [`Pool::shard_on`], где эта граница уже есть.
    retire_fn: fn(&Arc<Slot<T>>),
}

// `T` трогает только поток-владелец. Остальные держат `Arc` и ставят задачи.
unsafe impl<T: 'static> Send for Slot<T> {}
unsafe impl<T: 'static> Sync for Slot<T> {}

impl<T: 'static> ShardSlot for Slot<T> {
    fn dispose_on_owner(&self) {
        debug_assert_eq!(self.owner.get().copied(), Some(thread::current().id()));
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        unsafe {
            (*self.value.get()).take();
        }
    }

    fn addr(&self) -> *const () {
        self as *const Slot<T> as *const ()
    }
}

impl<T> Drop for Slot<T> {
    fn drop(&mut self) {
        let value = unsafe { (*self.value.get()).take() };
        if value.is_none() || self.disposed.load(Ordering::Acquire) {
            return;
        }
        if self.owner.get().copied() == Some(thread::current().id()) {
            drop(value);
            return;
        }
        // Сюда попадаем только если владелец так и не снял значение.
        // `Drop` на чужом потоке для `!Send` хуже утечки.
        std::mem::forget(value);
    }
}

impl<T: 'static> Shard<T> {
    /// Ядро, на котором живёт значение.
    pub fn core(&self) -> CoreId {
        self.slot.core
    }

    /// Выполняет замыкание на потоке-владельце и возвращает результат.
    ///
    /// Чужой поток ставит задачу в закреплённую очередь и ждёт не дольше
    /// [`crate::Config::submit_timeout`]. Поток-владелец выполняет её сразу:
    /// ждать самого себя нельзя, worker заблокировался бы в собственной задаче.
    /// Повторный `call`, пока внешний ещё держит `&mut T`, — [`PoolError::Reentrant`].
    /// Паника замыкания доходит до вызывающего, значение остаётся на ядре.
    pub fn call<R, F>(&self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let pool = self.live()?;
        if pool.worker_thread(self.slot.worker) == Some(thread::current().id()) {
            return unwrap_outcome(self.slot.invoke(f));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let slot = Arc::clone(&self.slot);
        let task: Task = Box::new(move || {
            let outcome = slot.invoke(f);
            let _ = tx.send(outcome);
        });
        pool.push_pinned(self.slot.worker, task, true)?;
        match rx.recv() {
            Ok(outcome) => unwrap_outcome(outcome),
            Err(_) => Err(PoolError::Stopped),
        }
    }

    /// Ставит замыкание владельцу и не ждёт, пока оно выполнится.
    ///
    /// Так одно ядро передаёт работу другому. Ответ — встречный `defer`,
    /// а не `call`: `call` с чужого worker блокирует его цикл, и взаимное
    /// ожидание двух ядер уже не разрывается.
    /// Панику здесь некому вернуть, worker её глотает и берёт следующую задачу.
    /// С потока-владельца полный слот — сразу [`PoolError::QueueFull`]:
    /// ждать место в своей очереди он не может.
    pub fn defer<F>(&self, f: F) -> Result<(), PoolError>
    where
        F: FnOnce(&mut T) + Send + 'static,
    {
        let pool = self.live()?;
        let slot = Arc::clone(&self.slot);
        let task: Task = Box::new(move || {
            let _ = slot.invoke(f);
        });
        pool.push_pinned(self.slot.worker, task, true)
    }

    fn live(&self) -> Result<Arc<Inner>, PoolError> {
        let pool = self.slot.pool.upgrade().ok_or(PoolError::Stopped)?;
        if pool.is_shutdown() || self.slot.disposed.load(Ordering::Acquire) {
            return Err(PoolError::Stopped);
        }
        Ok(pool)
    }
}

impl<T> Clone for Shard<T> {
    fn clone(&self) -> Self {
        self.slot.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            slot: Arc::clone(&self.slot),
        }
    }
}

impl<T> Drop for Shard<T> {
    fn drop(&mut self) {
        if self.slot.handles.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        (self.slot.retire_fn)(&self.slot);
    }
}

impl Pool {
    /// Создаёт значение на единственном worker этого ядра.
    ///
    /// `init` выполняется там же, поэтому выделенный внутри буфер впервые
    /// пишется на NUMA-узле этого CPU. Два worker на одном ядре
    /// ([`crate::Layout::LogicalCpus`]) дают [`PoolError::AmbiguousCore`]:
    /// у `&mut T` может быть только один поток. Нет такого ядра —
    /// [`PoolError::UnknownCore`].
    pub fn shard_on<T, F>(&self, core: CoreId, init: F) -> Result<Shard<T>, PoolError>
    where
        T: 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        if self.inner.is_shutdown() {
            return Err(PoolError::Stopped);
        }
        let worker = self.inner.sole_worker(core)?;
        let slot = Arc::new(Slot {
            value: std::cell::UnsafeCell::new(None),
            disposed: AtomicBool::new(false),
            depth: AtomicUsize::new(0),
            dispose_pending: AtomicBool::new(false),
            handles: AtomicUsize::new(1),
            owner: OnceLock::new(),
            worker,
            core,
            pool: Arc::downgrade(&self.inner),
            retire_fn: retire::<T>,
        });
        if self.inner.worker_thread(worker) == Some(thread::current().id()) {
            install(&slot, init)?;
            return Ok(Shard { slot });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let slot_for_task = Arc::clone(&slot);
        let task: Task = Box::new(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| install(&slot_for_task, init)));
            let _ = tx.send(result);
        });
        self.inner.push_pinned(worker, task, true)?;
        match rx.recv() {
            Ok(Ok(Ok(()))) => Ok(Shard { slot }),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(payload)) => panic::resume_unwind(payload),
            Err(_) => Err(PoolError::Stopped),
        }
    }
}

fn install<T: 'static>(slot: &Arc<Slot<T>>, init: impl FnOnce() -> T) -> Result<(), PoolError> {
    let value = Box::new(init());
    unsafe {
        *slot.value.get() = Some(value);
    }
    let _ = slot.owner.set(thread::current().id());
    let Some(pool) = slot.pool.upgrade() else {
        slot.dispose_on_owner();
        return Err(PoolError::Stopped);
    };
    let registered = pool.register_shard(slot.worker, Arc::clone(slot) as Arc<dyn ShardSlot>);
    if !registered {
        slot.dispose_on_owner();
        return Err(PoolError::Stopped);
    }
    Ok(())
}

fn retire<T: 'static>(slot: &Arc<Slot<T>>) {
    let Some(pool) = slot.pool.upgrade() else {
        return;
    };
    let on_owner = pool.worker_thread(slot.worker) == Some(thread::current().id());
    if on_owner {
        if slot.depth.load(Ordering::Relaxed) != 0 {
            slot.dispose_pending.store(true, Ordering::Release);
            return;
        }
        slot.dispose_on_owner();
        pool.unregister_shard(slot.worker, slot.addr());
        return;
    }
    let queued = Arc::clone(slot);
    let task: Task = Box::new(move || {
        debug_assert_eq!(queued.depth.load(Ordering::Relaxed), 0);
        queued.dispose_on_owner();
        if let Some(pool) = queued.pool.upgrade() {
            pool.unregister_shard(queued.worker, queued.addr());
        }
    });
    let _ = pool.enqueue_dispose(slot.worker, task);
}

impl<T: 'static> Slot<T> {
    fn invoke<R>(&self, f: impl FnOnce(&mut T) -> R) -> thread::Result<Result<R, PoolError>> {
        if self.disposed.load(Ordering::Acquire) {
            return Ok(Err(PoolError::Stopped));
        }
        if self.depth.load(Ordering::Relaxed) != 0 {
            return Ok(Err(PoolError::Reentrant));
        }
        self.depth.store(1, Ordering::Relaxed);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let value =
                unsafe { (*self.value.get()).as_mut().expect("шард без значения") };
            f(value.as_mut())
        }));
        self.depth.store(0, Ordering::Relaxed);
        if self.dispose_pending.swap(false, Ordering::AcqRel) {
            self.dispose_on_owner();
            if let Some(pool) = self.pool.upgrade() {
                pool.unregister_shard(self.worker, self.addr());
            }
        }
        match result {
            Ok(value) => Ok(Ok(value)),
            Err(payload) => Err(payload),
        }
    }
}

fn unwrap_outcome<R>(outcome: thread::Result<Result<R, PoolError>>) -> Result<R, PoolError> {
    match outcome {
        Ok(result) => result,
        Err(payload) => panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affinity::allowed_cpus;
    use crate::pool::{Config, Layout};
    use std::panic::catch_unwind;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn pinned_cpu(pool: &Pool, core: CoreId) -> usize {
        pool.cores()
            .iter()
            .zip(pool.pinned_cpus())
            .find(|(found, _)| **found == core)
            .map(|(_, cpu)| cpu)
            .expect("ядро есть в пуле")
    }

    #[test]
    fn init_and_call_run_on_the_pinned_cpu() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let cpu = pinned_cpu(&pool, core);
        let shard = pool
            .shard_on(core, move || {
                assert_eq!(allowed_cpus().unwrap(), vec![cpu]);
                0u64
            })
            .unwrap();
        let seen = shard
            .call(move |value| {
                assert_eq!(allowed_cpus().unwrap(), vec![cpu]);
                *value += 1;
                *value
            })
            .unwrap();
        assert_eq!(seen, 1);
        assert_eq!(shard.call(|value| *value).unwrap(), 1);
    }

    #[test]
    fn value_need_not_be_send() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let shard = pool.shard_on(core, || std::rc::Rc::new(2u32)).unwrap();
        assert_eq!(shard.call(|value| **value).unwrap(), 2);
    }

    #[test]
    fn call_on_the_owner_runs_inline() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let shard = pool.shard_on(core, || 0u32).unwrap();
        let shard_for_task = shard.clone();
        pool.execute_on(core, move || {
            shard_for_task.call(|value| *value += 1).unwrap();
        })
        .unwrap()
        .wait();
        assert_eq!(shard.call(|value| *value).unwrap(), 1);
    }

    #[test]
    fn nested_call_is_reentrant_and_the_shard_stays_usable() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let shard = pool.shard_on(core, || 0u32).unwrap();
        let nested = shard.clone();
        let err = shard
            .call(move |_| nested.call(|value| *value += 1).unwrap_err())
            .unwrap();
        assert!(matches!(err, PoolError::Reentrant));
        assert_eq!(shard.call(|value| *value).unwrap(), 0);
    }

    #[test]
    fn defer_runs_on_the_target_and_replies_to_the_owner() {
        let pool = Pool::new().unwrap();
        let cores = pool.cores();
        if cores.len() < 2 {
            return;
        }
        let left_core = cores[0];
        let right_core = cores[1];
        let left_cpu = pinned_cpu(&pool, left_core);
        let right_cpu = pinned_cpu(&pool, right_core);
        let left = pool.shard_on(left_core, || Vec::<u64>::new()).unwrap();
        let right = pool.shard_on(right_core, || Vec::<u64>::new()).unwrap();
        let (tx, rx) = mpsc::channel();
        let right_for_left = right.clone();
        let left_for_right = left.clone();
        left.call(move |mine| {
            mine.push(1);
            right_for_left
                .defer(move |theirs| {
                    assert_eq!(allowed_cpus().unwrap(), vec![right_cpu]);
                    theirs.push(2);
                    let len = theirs.len() as u64;
                    left_for_right
                        .defer(move |mine| {
                            assert_eq!(allowed_cpus().unwrap(), vec![left_cpu]);
                            mine.push(len);
                            let _ = tx.send(mine.clone());
                        })
                        .unwrap();
                })
                .unwrap();
        })
        .unwrap();
        let mine = rx.recv_timeout(Duration::from_secs(2)).expect("ответ");
        assert_eq!(mine, vec![1, 1]);
        assert_eq!(right.call(|theirs| theirs.clone()).unwrap(), vec![2]);
    }

    #[test]
    fn unknown_core_and_ambiguous_core_are_different_errors() {
        let pool = Pool::new().unwrap();
        let unknown = pool.shard_on(CoreId(u32::MAX), || ());
        assert!(matches!(
            unknown,
            Err(PoolError::UnknownCore(CoreId(u32::MAX)))
        ));

        let logical = Pool::with_config(Config {
            layout: Layout::LogicalCpus,
            batch: 8,
            pinned_per_shared: 3,
            submit_timeout: Duration::from_secs(1),
        })
        .unwrap();
        let mut seen = Vec::new();
        let mut ambiguous = None;
        for core in logical.cores() {
            if seen.contains(&core) {
                ambiguous = Some(core);
                break;
            }
            seen.push(core);
        }
        let Some(core) = ambiguous else {
            return;
        };
        let result = logical.shard_on(core, || ());
        assert!(matches!(result, Err(PoolError::AmbiguousCore(_))));
    }

    #[test]
    fn a_panicking_call_does_not_take_the_worker_with_it() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let shard = pool.shard_on(core, || 0u32).unwrap();
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            shard.call(|_value| -> i32 { panic!("boom") }).unwrap();
        }));
        assert!(panicked.is_err());
        assert_eq!(
            shard
                .call(|value| {
                    *value += 1;
                    *value
                })
                .unwrap(),
            1
        );
        assert_eq!(pool.execute(|| 7).unwrap().wait(), 7);
    }

    struct Probe {
        cpu: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let cpu = allowed_cpus().unwrap()[0];
            self.cpu.store(cpu, Ordering::Release);
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn wait_until_dropped(dropped: &AtomicBool) {
        let started = Instant::now();
        while !dropped.load(Ordering::Acquire) {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "значение не уничтожено"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn drop_from_another_thread_runs_on_the_owner_cpu() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let cpu = pinned_cpu(&pool, core);
        let dropped = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicUsize::new(usize::MAX));
        let flag = Arc::clone(&dropped);
        let seen_cpu = Arc::clone(&seen);
        let shard = pool
            .shard_on(core, move || Probe {
                cpu: seen_cpu,
                dropped: flag,
            })
            .unwrap();
        thread::spawn(move || drop(shard)).join().unwrap();
        wait_until_dropped(&dropped);
        assert_eq!(seen.load(Ordering::Acquire), cpu);
    }

    #[test]
    fn dropping_the_pool_disposes_the_value_on_the_owner() {
        let pool = Pool::new().unwrap();
        let core = pool.cores()[0];
        let cpu = pinned_cpu(&pool, core);
        let dropped = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(AtomicUsize::new(usize::MAX));
        let flag = Arc::clone(&dropped);
        let seen_cpu = Arc::clone(&seen);
        let shard = pool
            .shard_on(core, move || Probe {
                cpu: seen_cpu,
                dropped: flag,
            })
            .unwrap();
        drop(pool);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(seen.load(Ordering::Acquire), cpu);
        assert!(matches!(shard.call(|_probe| ()), Err(PoolError::Stopped)));
    }
}
