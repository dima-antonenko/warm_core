# Замер affinity

[English](#english) · [Русский](#русский)

Отдельная заметка о сравнении в [`examples/compare.rs`](examples/compare.rs) и [`benches/locality.rs`](benches/locality.rs). Цифры того же прогона есть в [README](README.md).

## English

### What was wrong

The old example compared two different programs at once:

- 24 fresh `std::thread`s, four times the number of physical cores, each calling `yield_now()` after every pass;
- 6 `warm_core` workers, created before the timer, with no `yield_now`.

The 37 ms and 31 ms (about 1.2×) from that run mixed oversubscription, extra context switches, and the cost of spawning threads. They did not measure CPU affinity.

### What we run now

All three variants do the same work. On the i7-8750H that is 24 tasks, each with its own 256 KiB buffer (one core's L2) and 40 passes over it. Nothing calls `yield_now`. Checksums are compared at the end.

Each variant has 6 workers, one per physical core. The pools are created before the timer starts.

| Variant | How it runs |
|---|---|
| Rayon, no affinity | `ThreadPool` with `num_threads = pool.worker_count()`, one `into_par_iter` |
| Rayon + affinity | The same pool and the same iterator. Each worker calls `pin_to` on one CPU from `Pool::pinned_cpus()` (here 0–5) |
| `warm_core` | The same six CPUs. Tasks go through `execute` / `wait`, and each worker drains its own queue in batches |

The Rayon pair shares a scheduler and a thread count. The only difference is the `sched_setaffinity` mask. `warm_core` also has its own queues.

`pin_to` is the same function the pool uses on its workers. It is re-exported from the crate so the example and the criterion benchmark can call it. Rayon is a dev-dependency; the library still depends only on `libc`.

### Result

`cargo run --release --example compare` on an Intel Core i7-8750H (6 cores, 12 threads):

| | Rayon | Rayon + affinity | `warm_core` |
|---|---:|---:|---:|
| Time | 37.7 ms | 39.5 ms | 26.6 ms |
| Versus Rayon | 1.0 | 0.95 | **1.4** |

The pin did not shorten this run: pinned Rayon took about 1.8 ms longer (`39.5 − 37.7`). `warm_core` finished 11 ms earlier than unpinned Rayon (`37.7 / 26.6 ≈ 1.42`) and was also faster than pinned Rayon (`39.5 / 26.6 ≈ 1.5`).

Each task allocates its buffer on the worker that runs it and then scans that buffer 40 times without yielding. The Rayon pair is the affinity mask. The shorter `warm_core` time is how the 24 tasks are handed out.

This is one short run on one machine. A pin still pays off when the kernel would otherwise move a busy thread. This run did not show that cost.

### How to repeat

```bash
cargo run --release --example compare
```

The example prints the logical CPU count, the worker count, the buffer size, the pinned CPUs, the three times, and the ratios against unpinned Rayon.

The same three variants are in the criterion benchmark, with 8 passes instead of 40. The functions are `rayon`, `rayon_affinity`, and `warm_core`. The number of tasks comes from `available_parallelism()`.

```bash
cargo bench
```

## Русский

### Что было не так

Старый пример сравнивал сразу две разные программы:

- 24 новых потока `std::thread`, в четыре раза больше физических ядер, и после каждого прохода `yield_now()`;
- 6 рабочих потоков `warm_core`, созданных до секундомера и без `yield_now`.

37 мс и 31 мс (около 1.2×) с того прогона смешивали переподписку, лишние переключения и стоимость создания потоков. По ним нельзя судить об affinity.

### Что запускаем сейчас

Все три варианта делают одну и ту же работу. На i7-8750H это 24 задачи, у каждой свой буфер 256 КБ (L2 одного ядра) и 40 проходов по нему. `yield_now` никто не вызывает. В конце сравниваются контрольные суммы.

В каждом варианте 6 рабочих потоков, по одному на физическое ядро. Пулы создаются до начала замера.

| Вариант | Как устроен |
|---|---|
| Rayon без affinity | `ThreadPool` с `num_threads = pool.worker_count()`, один `into_par_iter` |
| Rayon с affinity | Тот же пул и тот же итератор. Каждый поток вызывает `pin_to` для одного CPU из `Pool::pinned_cpus()` (здесь 0–5) |
| `warm_core` | Те же шесть CPU. Задачи идут через `execute` / `wait`, и каждый поток сначала доделывает свою очередь пачками |

Пара Rayon делит один планировщик и одно число потоков. Отличается только маска `sched_setaffinity`. У `warm_core` ещё и свои очереди.

`pin_to` — тот же вызов, которым пул привязывает свои потоки. Он реэкспортирован из крейта, чтобы пример и бенчмарк criterion могли его вызвать. Rayon стоит в dev-зависимостях; сама библиотека по-прежнему зависит только от `libc`.

### Результат

`cargo run --release --example compare` на Intel Core i7-8750H (6 ядер, 12 потоков):

| | Rayon | Rayon + affinity | `warm_core` |
|---|---:|---:|---:|
| Время | 37.7 мс | 39.5 мс | 26.6 мс |
| Относительно Rayon | 1.0 | 0.95 | **1.4** |

Привязка этот прогон не сократила: привязанный Rayon потратил примерно на 1.8 мс больше (`39.5 − 37.7`). `warm_core` закончил на 11 мс раньше непривязанного Rayon (`37.7 / 26.6 ≈ 1.42`) и обогнал привязанный Rayon тоже (`39.5 / 26.6 ≈ 1.5`).

Каждая задача выделяет буфер на том потоке, который её выполняет, и 40 раз проходит по нему, не отдавая процессор. Пара Rayon — это маска affinity. Более короткое время `warm_core` — это то, как раздаются 24 задачи.

Это один короткий прогон на одной машине. Привязка по-прежнему окупается, когда ядро системы иначе перенесло бы занятый поток. Этот прогон такой цены не показал.

### Как повторить

```bash
cargo run --release --example compare
```

Пример печатает число логических CPU, число рабочих потоков, размер буфера, список закреплённых CPU, три времени и отношения к непривязанному Rayon.

Те же три варианта есть в бенчмарке criterion, только проходов 8, а не 40. Функции называются `rayon`, `rayon_affinity` и `warm_core`. Число задач берётся из `available_parallelism()`.

```bash
cargo bench
```
