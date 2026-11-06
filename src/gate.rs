//! Сон производителя, пока в очереди нет слота.
//!
//! `waiters` и `generation` стоят на разных кэш-линиях. Пока никто не ждёт,
//! `note_slot_freed` только читает `waiters` и не пишет линию, которую
//! worker держит у себя. `generation` растёт лишь когда есть спящие:
//! `futex_wait` сравнивает это слово и сразу возвращается, если pop
//! успел случиться между проверкой очереди и сном.

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Поле на своей линии, чтобы соседний счётчик не инвалидировал его.
#[repr(align(64))]
struct CacheLine<T> {
    value: T,
}

pub(crate) struct FutexGate {
    waiters: CacheLine<AtomicU32>,
    generation: CacheLine<AtomicU32>,
}

pub(crate) struct Registration<'a> {
    gate: &'a FutexGate,
}

impl FutexGate {
    pub(crate) fn new() -> Self {
        Self {
            waiters: CacheLine {
                value: AtomicU32::new(0),
            },
            generation: CacheLine {
                value: AtomicU32::new(0),
            },
        }
    }

    /// Пока регистрация жива, освободившийся слот будит одного спящего.
    pub(crate) fn register(&self) -> Registration<'_> {
        self.waiters.value.fetch_add(1, Ordering::Release);
        Registration { gate: self }
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self) -> u32 {
        self.waiters.value.load(Ordering::Acquire)
    }

    /// Слот уже свободен. Будим только если кто-то спит на этом gate.
    pub(crate) fn note_slot_freed(&self) {
        if self.waiters.value.load(Ordering::Acquire) == 0 {
            return;
        }
        self.generation.value.fetch_add(1, Ordering::Release);
        futex_wake(&self.generation.value);
    }
}

impl Registration<'_> {
    pub(crate) fn generation(&self) -> u32 {
        self.gate.generation.value.load(Ordering::Acquire)
    }

    /// Спит, пока `generation` не уйдёт от `expected` или не выйдет `timeout`.
    ///
    /// `true` — слово изменилось или пришёл wake. `false` — вышел срок.
    /// Вызывающий читает [`Self::generation`], ещё раз пробует `push` и только
    /// потом зовёт `wait`: иначе pop между проверкой и сном потеряется.
    pub(crate) fn wait(&self, expected: u32, timeout: Duration) -> bool {
        futex_wait(&self.gate.generation.value, expected, timeout)
    }
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.gate.waiters.value.fetch_sub(1, Ordering::Release);
    }
}

fn futex_wait(addr: &AtomicU32, expected: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        // Абсолютный CLOCK_MONOTONIC: шаг CLOCK_REALTIME не растягивает ожидание.
        // EINTR запускает ожидание заново с остатком срока.
        let ts = monotonic_after(deadline - now);
        let rc = unsafe {
            libc::syscall(
                libc::SYS_futex,
                addr.as_ptr(),
                libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
                expected,
                &ts,
                std::ptr::null::<u32>(),
                libc::FUTEX_BITSET_MATCH_ANY,
            )
        };
        if rc == 0 {
            return true;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EAGAIN) => return true,
            Some(libc::EINTR) => continue,
            Some(libc::ETIMEDOUT) => return false,
            // Неизвестный сбой futex не должен крутить поток до конца срока.
            _ => {
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                return false;
            }
        }
    }
}

fn futex_wake(addr: &AtomicU32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr.as_ptr(),
            libc::FUTEX_WAKE_BITSET | libc::FUTEX_PRIVATE_FLAG,
            1,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            libc::FUTEX_BITSET_MATCH_ANY,
        );
    }
}

fn monotonic_after(timeout: Duration) -> libc::timespec {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        assert_eq!(libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now), 0);
    }
    let extra_sec = i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX);
    let mut sec = now.tv_sec.saturating_add(extra_sec);
    let mut nsec = now.tv_nsec + i64::from(timeout.subsec_nanos());
    if nsec >= 1_000_000_000 {
        sec = sec.saturating_add(1);
        nsec -= 1_000_000_000;
    }
    libc::timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    #[test]
    fn waiters_and_generation_do_not_share_a_cache_line() {
        let gate = FutexGate::new();
        let waiters = &gate.waiters as *const _ as usize;
        let generation = &gate.generation as *const _ as usize;
        assert!(waiters.abs_diff(generation) >= 64);
    }

    #[test]
    fn wait_returns_when_the_timeout_expires() {
        let gate = FutexGate::new();
        let registration = gate.register();
        let expected = registration.generation();
        let timeout = Duration::from_millis(50);
        let started = Instant::now();
        let woken = registration.wait(expected, timeout);
        let elapsed = started.elapsed();
        assert!(!woken);
        assert!(
            elapsed + Duration::from_millis(5) >= timeout,
            "сон закончился слишком рано: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "сон не ограничен сроком: {elapsed:?}"
        );
    }

    #[test]
    fn note_slot_freed_reaches_a_waiter() {
        let gate = Arc::new(FutexGate::new());
        let space = Arc::new(AtomicBool::new(false));
        let waiting = Arc::clone(&gate);
        let waiting_space = Arc::clone(&space);
        let thread = thread::spawn(move || {
            let registration = waiting.register();
            let expected = registration.generation();
            if waiting_space.load(Ordering::Acquire) {
                return true;
            }
            registration.wait(expected, Duration::from_secs(2))
        });

        let started = Instant::now();
        while gate.waiter_count() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "ожидающий не зарегистрировался"
            );
            thread::yield_now();
        }
        space.store(true, Ordering::Release);
        gate.note_slot_freed();
        assert!(thread.join().unwrap());
    }
}
