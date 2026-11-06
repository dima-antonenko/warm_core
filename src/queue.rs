//! Ограниченная MPMC-очередь Вьюкова.
//!
//! `enqueue` и `dequeue` стоят на разных cache line. Иначе каждый push
//! инвалидирует линию, которую в тот же момент читает worker, и ядра
//! гоняют одну линию через L3 туда-сюда (false sharing).

#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Поле, которое занимает целую линию и не делит её с соседом.
#[repr(align(64))]
struct CachePadded<T> {
    value: T,
}

/// Слот тоже на своей линии: соседние задачи не делят sequence/data.
#[repr(align(64))]
struct Slot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// Очередь задач одного ядра. Несколько потоков кладут, несколько забирают.
pub struct Queue<T> {
    slots: Box<[Slot<T>]>,
    mask: usize,
    enqueue_pos: CachePadded<AtomicUsize>,
    dequeue_pos: CachePadded<AtomicUsize>,
    /// Приблизительная длина для выбора наименее загруженного ядра.
    len: CachePadded<AtomicUsize>,
}

// Слот пишет только поток, выигравший CAS на enqueue, и читает только поток,
// выигравший CAS на dequeue. Release/Acquire у sequence не даёт прочитать
// значение до записи и не даёт записать поверх ещё живого.
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let slots = (0..capacity)
            .map(|index| Slot {
                sequence: AtomicUsize::new(index),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            slots,
            mask: capacity - 1,
            enqueue_pos: CachePadded {
                value: AtomicUsize::new(0),
            },
            dequeue_pos: CachePadded {
                value: AtomicUsize::new(0),
            },
            len: CachePadded {
                value: AtomicUsize::new(0),
            },
        }
    }

    pub fn len(&self) -> usize {
        self.len.value.load(Ordering::Relaxed)
    }

    /// `Err` возвращает значение, если очередь полна.
    pub fn push(&self, value: T) -> Result<(), T> {
        let mut pos = self.enqueue_pos.value.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos & self.mask];
            let seq = slot.sequence.load(Ordering::Acquire);
            let dif = seq.wrapping_sub(pos) as isize;
            if dif == 0 {
                match self.enqueue_pos.value.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        unsafe {
                            (*slot.value.get()).write(value);
                        }
                        // Длина растёт до публикации слота. Иначе pop успевает
                        // сделать fetch_sub раньше этого fetch_add, и счётчик
                        // на мгновение становится usize::MAX: ядро выглядит
                        // самым загруженным. Acquire в pop читает этот Release,
                        // поэтому уменьшение длины случается строго позже.
                        self.len.value.fetch_add(1, Ordering::Relaxed);
                        slot.sequence.store(pos.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(actual) => pos = actual,
                }
            } else if dif < 0 {
                return Err(value);
            } else {
                pos = self.enqueue_pos.value.load(Ordering::Relaxed);
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        let mut pos = self.dequeue_pos.value.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos & self.mask];
            let seq = slot.sequence.load(Ordering::Acquire);
            let dif = seq.wrapping_sub(pos.wrapping_add(1)) as isize;
            if dif == 0 {
                match self.dequeue_pos.value.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        let value = unsafe { (*slot.value.get()).assume_init_read() };
                        // Следующий проход по кольцу ждёт sequence == pos + capacity.
                        slot.sequence
                            .store(pos.wrapping_add(self.slots.len()), Ordering::Release);
                        self.len.value.fetch_sub(1, Ordering::Relaxed);
                        return Some(value);
                    }
                    Err(actual) => pos = actual,
                }
            } else if dif < 0 {
                return None;
            } else {
                pos = self.dequeue_pos.value.load(Ordering::Relaxed);
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::mem::align_of;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn positions_sit_on_distinct_cache_lines() {
        let queue = Queue::<u8>::new(8);
        let enqueue = &queue.enqueue_pos as *const _ as usize;
        let dequeue = &queue.dequeue_pos as *const _ as usize;
        let len = &queue.len as *const _ as usize;
        assert!(enqueue.abs_diff(dequeue) >= 64);
        assert!(dequeue.abs_diff(len) >= 64);
        assert!(enqueue.abs_diff(len) >= 64);
        assert_eq!(align_of::<Slot<u64>>(), 64);
    }

    #[test]
    fn push_pop_wraps_the_ring() {
        let queue = Queue::new(2);
        assert!(queue.push(1).is_ok());
        assert!(queue.push(2).is_ok());
        assert!(queue.push(3).is_err());
        assert_eq!(queue.pop(), Some(1));
        assert!(queue.push(3).is_ok());
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), None);

        for i in 0..10_000 {
            queue.push(i).unwrap();
            assert_eq!(queue.pop(), Some(i));
        }
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn multi_producer_multi_consumer_keeps_every_value_once() {
        const PRODUCERS: usize = 4;
        const CONSUMERS: usize = 4;
        const PER: usize = 2_000;
        let total = PRODUCERS * PER;

        let queue = Arc::new(Queue::new(128));
        let left = Arc::new(AtomicUsize::new(total));
        let got = Arc::new(Mutex::new(Vec::with_capacity(total)));

        let mut producers = Vec::new();
        for producer in 0..PRODUCERS {
            let queue = Arc::clone(&queue);
            producers.push(thread::spawn(move || {
                let start = producer * PER;
                for value in start..start + PER {
                    let mut value = value;
                    loop {
                        match queue.push(value) {
                            Ok(()) => break,
                            Err(returned) => {
                                value = returned;
                                thread::yield_now();
                            }
                        }
                    }
                }
            }));
        }

        let mut consumers = Vec::new();
        for _ in 0..CONSUMERS {
            let queue = Arc::clone(&queue);
            let left = Arc::clone(&left);
            let got = Arc::clone(&got);
            consumers.push(thread::spawn(move || {
                let mut local = Vec::new();
                loop {
                    if let Some(value) = queue.pop() {
                        local.push(value);
                        if left.fetch_sub(1, Ordering::AcqRel) == 1 {
                            break;
                        }
                    } else if left.load(Ordering::Acquire) == 0 {
                        break;
                    } else {
                        thread::yield_now();
                    }
                }
                got.lock().unwrap().extend(local);
            }));
        }

        for handle in producers {
            handle.join().unwrap();
        }
        for handle in consumers {
            handle.join().unwrap();
        }

        let mut values = got.lock().unwrap().clone();
        values.sort_unstable();
        let expected = (0..total).collect::<Vec<_>>();
        assert_eq!(values, expected);
    }
}

/// `RUSTFLAGS="--cfg loom" cargo test --lib --release len_does_not_underflow -- --test-threads=1`
#[cfg(all(test, loom))]
mod loom_tests {
    use super::Queue;
    use std::sync::Arc;

    #[test]
    fn len_does_not_underflow_on_handoff() {
        loom::model(|| {
            let queue = Arc::new(Queue::new(2));

            let pushed = Arc::clone(&queue);
            let producer = loom::thread::spawn(move || {
                pushed.push(1).unwrap();
            });

            let popped = Arc::clone(&queue);
            let consumer = loom::thread::spawn(move || {
                loop {
                    if let Some(value) = popped.pop() {
                        break value;
                    }
                    loom::thread::yield_now();
                }
            });

            // Один замер. Loom ставит его между атомными операциями push и pop.
            let observed = Arc::clone(&queue);
            let observer = loom::thread::spawn(move || observed.len());

            producer.join().unwrap();
            assert_eq!(consumer.join().unwrap(), 1);
            let len = observer.join().unwrap();
            assert!(len <= 1, "длина ушла ниже нуля и обернулась в {len}");
            assert_eq!(queue.len(), 0);
        });
    }
}
