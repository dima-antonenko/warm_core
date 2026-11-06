# warm_core

[English](#english) · [Русский](#русский)

## English

`warm_core` is a small thread pool for Linux that tries not to shuffle data between CPU cores. Each worker thread is pinned to its own physical core, and every task runs from start to finish in one place. As a result, the data a task works on settles into that core's fast caches (L1 and L2) and stays there until the task is done.

The pool discovers the CPU layout on its own from `/sys/devices/system/cpu`: how many physical cores there are, which logical CPUs share a core, and how large the caches are. Threads are pinned with the `sched_setaffinity` system call. The only external dependency is `libc`.

The sections below cover, in order: a bit of background on cores and caches, how the pool works, how to use it, and what the benchmark showed.

### In short

On an Intel i7-8750H, the same workload took:

| Variant | Time |
|---|---|
| Rayon, 6 workers, no affinity | 37.7 ms |
| Rayon, 6 workers, pinned like the pool | 39.5 ms |
| `warm_core` | 26.6 ms |

The affinity mask by itself did not shorten the work: pinned Rayon came out at **0.95×**. `warm_core` finished about **1.4× faster** than unpinned Rayon, 11 ms less, and it was also faster than the pinned Rayon pool. All three variants produced identical checksums, so they really did the same work. Details and caveats are in [What the benchmark shows](#what-the-benchmark-shows).

`cargo test` passes: 12 library tests and 1 doctest.

### Background

#### Cores and logical CPUs

A **physical core** is a separate execution unit of the processor. The i7-8750H has six. Each core has its own small and very fast L1 and L2 caches.

A **logical CPU** is what the operating system presents as a separate processor. This machine has twelve, numbered `0` to `11`. There are twelve because each physical core runs two instruction streams at once using hyper-threading (generically called SMT). Both threads of a core share its L1 and L2.

The pairs on the i7-8750H look like this:

| Physical core | Logical CPUs | Core caches |
|---|---|---|
| 0 | 0 and 6 | L1d 32 KiB, L2 256 KiB |
| 1 | 1 and 7 | L1d 32 KiB, L2 256 KiB |
| 2 | 2 and 8 | L1d 32 KiB, L2 256 KiB |
| 3 | 3 and 9 | L1d 32 KiB, L2 256 KiB |
| 4 | 4 and 10 | L1d 32 KiB, L2 256 KiB |
| 5 | 5 and 11 | L1d 32 KiB, L2 256 KiB |

The 9 MiB **L3** cache is shared by the whole processor.

#### Caches

A trip to main memory costs the processor hundreds of cycles; a hit in L1 costs a few. That is why the processor keeps recently used data in caches and looks there first.

A handy analogy is a desk and a cabinet in the next room:

- **L1** is what you are already holding. Very little room, but instant access.
- **L2** is the edge of the desk. More room, still very fast.
- **L3** is a drawer nearby, shared by everyone.
- **Main memory** is the cabinet. Plenty of room, but every trip takes a while.

The processor exchanges data with memory in **cache lines** of 64 bytes rather than individual bytes: even if you need a single variable, the whole 64 bytes around it are fetched.

The **working set** is the data a program is touching right now. If the working set fits in L2 and nothing evicts it, the program rarely has to go to slow memory.

#### What actually happens on a thread switch

A common belief is that the cache gets "wiped" when a thread is taken off a core. It does not: the processor does not clear anything, and the lines stay where they are. The real problem is different.

When another thread with its own data runs on the same core, that data is not in the core's L1 and L2, so the processor loads it from L3 or memory. The new lines take up space and evict the old ones. When the first thread comes back, its data has been evicted and it has to fetch it again.

It is even worse when the scheduler moves a thread to **another physical core**. That core has its own L1 and L2, and everything that was "warm" on the old core has to be loaded again. Some of the data may still be in the shared L3, but L3 is noticeably slower and also gets evicted by other data.

A regular program cannot stop the operating system from preempting its threads. What it can do is pin a thread to one core and keep a task from giving that core up in the middle of its work. `warm_core` does both. The benchmark further down runs the same Rayon pool with and without a pin, so the mask and the pool's own queues are visible separately.

### How the pool works

When you call `Pool::new()`:

1. The topology is read from `/sys/devices/system/cpu`: the physical core of every logical CPU, its hyper-threading siblings, and the cache parameters.
2. By default, **one worker thread per physical core** is started, not one per logical CPU. On the i7-8750H that is 6 threads on CPUs `0–5`. The other halves of the pairs (`6–11`) are left alone: two pool threads on the same core would share L1 and L2 and evict each other's data.
3. Each worker is pinned to a single logical CPU via `sched_setaffinity` (`pin_to`), so the scheduler no longer moves it to other cores. The system can still preempt it temporarily for another process, and other programs can still use the idle hyper-threading sibling — the pool does not control that.
4. Each worker has two task queues:
   - a **shared** queue for `execute`; other workers may take tasks from it;
   - a **pinned** queue for `execute_on`; these tasks run only on their own core.
5. Each queue is a ring buffer of 4096 tasks based on Dmitry Vyukov's algorithm. Several threads can push and pop at the same time. The write and read positions live on separate cache lines, and each slot occupies its own line as well. Without that, every push would dirty the line the worker is reading at that moment, and cores would bounce it back and forth. This is called false sharing: the data is logically independent but physically sits in one line.

Each worker then runs a simple loop:

1. It runs up to 32 tasks in a row from its own queues: pinned ones first, then shared ones. During this time it ignores its neighbours, so the core keeps working with the same data.
2. If it has no tasks of its own, it tries to take one from a neighbour's shared queue, starting with the busiest. It never touches other cores' pinned tasks: those were sent to a specific core precisely for its cache.
3. If there is no work anywhere, it goes to sleep (`park`) until a new task is pushed into its queue.

`Layout::LogicalCpus` starts a worker on every logical CPU, including the second halves of hyper-threading pairs. It can be useful when tasks barely depend on the cache. If a task's working set is comparable to L2, the default layout is usually the better choice.

### Life of a task

`execute` puts the task into the shared queue of the core that currently has the fewest tasks. On a tie, cores are picked round-robin so that work does not pile up on core zero. If another core becomes idle first, it may take the task.

`execute_on(core, ...)` puts the task into the pinned queue of the given physical core, and that is where it runs. In `LogicalCpus` mode, where a core has two workers, the task goes to the less loaded of the two.

Both calls return a `JobHandle`, so the calling thread can keep working. The result is retrieved with `wait()`, which blocks until the task finishes. If the core's queue is full (4096 tasks), the call does not return an error: it waits until there is room.

If a task panics, the panic is propagated to `wait()`. The worker itself keeps running and picks up the next task.

When the pool is dropped, it tells the workers to stop, wakes them up, and waits until they finish every task already in their queues.

### Usage

```rust
use warm_core::{Config, Layout, Pool, Topology};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default pool: one pinned worker per physical core,
    // up to 32 tasks in a row from its own queue.
    let pool = Pool::new()?;

    println!("workers: {}", pool.worker_count());
    println!("pinned to CPUs: {:?}", pool.pinned_cpus());

    // The task goes to the least loaded core.
    // wait() blocks until it is done and returns the result.
    let sum = pool.execute(|| 2 + 2).wait();
    assert_eq!(sum, 4);

    // The task runs on exactly the chosen core.
    // Handy when several tasks in a row work with the same data.
    let core = pool.cores()[0];
    let pinned = pool.execute_on(core, || 10)?.wait();
    assert_eq!(pinned, 10);

    // A worker on every logical CPU, hyper-threading siblings included.
    let _all_logical = Pool::with_config(Config {
        layout: Layout::LogicalCpus,
        batch: 32,
    })?;

    // The topology can be read without a pool.
    let topology = Topology::detect()?;
    println!("logical CPUs: {}", topology.cpus.len());
    println!("L2 per core: {:?} bytes", topology.l2_size_bytes());

    // The same call each worker makes at startup. `cpu` is a logical CPU id.
    warm_core::pin_to(0)?;

    Ok(())
}
```

If the pool has no such core, `execute_on` returns `PoolError::UnknownCore`.

`pin_to` is public, so other threads — the Rayon workers in `examples/compare.rs`, for instance — can be pinned with the same system call the pool uses.

A closure passed to the pool must be `Send + 'static`: it runs on another thread, so it cannot borrow local variables of the calling code. Move the data it needs inside with `move`.

### What the benchmark shows

The benchmark lives in `examples/compare.rs`. Built with `--release` on an Intel Core i7-8750H (6 cores, 12 threads, 256 KiB of L2 per core). Rayon is a dev-dependency of the example and the criterion benchmark; the library itself still depends only on `libc`.

All three variants do identical work:

- there are twice as many tasks as logical CPUs — 24 on this machine;
- each task has its own buffer the size of one core's L2 — 256 KiB here;
- each task makes 40 passes over its buffer, incrementing every byte by 1 and accumulating a checksum along the way;
- at the end the checksums of all three variants are compared: this keeps the compiler from optimizing the work away and confirms the results match.

Each variant uses 6 workers, one per physical core. The pools are created before the timer starts, and no task calls `yield_now` between passes.

**Rayon, no affinity** is a Rayon pool of 6 threads. The tasks are one `into_par_iter`. The kernel may move those threads from core to core.

**Rayon + affinity** is the same pool and the same `into_par_iter`. As each worker starts, it calls `pin_to` on one CPU from `Pool::pinned_cpus()` — on this machine, CPUs 0–5. The only difference from the first pool is the `sched_setaffinity` mask.

**`warm_core`** uses those same six CPUs. Tasks are submitted with `execute` and collected with `wait`. Each worker drains its own queue in batches before it looks at another core's queue.

An earlier run of this example compared 24 `std::thread`s that called `yield_now` after every pass (37 ms) with the pool (31 ms, about 1.2×). That number mixed oversubscription, extra context switches, and the cost of spawning threads. The figures below replace it.

Results:

| | Rayon | Rayon + affinity | `warm_core` |
|---|---:|---:|---:|
| Time | 37.7 ms | 39.5 ms | 26.6 ms |
| Versus Rayon | 1.0 | 0.95 | **1.4** |

How to read these numbers:

- The pinned Rayon pool took about 1.8 ms longer: `39.5 − 37.7 = 1.8`. The ratio `37.7 / 39.5 ≈ 0.95` means the pin did not speed this run up.
- `warm_core` finished 11 ms earlier than unpinned Rayon: `37.7 − 26.6 = 11.1`. Speedup: `37.7 / 26.6 ≈ 1.42`, rounded to 1.4. It was also faster than pinned Rayon (`39.5 / 26.6 ≈ 1.5`).
- If you ran this batch continuously, unpinned Rayon would complete about 27 batches per second (`1000 / 37.7`) and the pool about 38 (`1000 / 26.6`).

Important caveats:

- The two Rayon columns share a scheduler and a thread count, so the gap between them is the affinity mask. On this run that gap is small and points the wrong way for a "pin makes it faster" reading. `warm_core` was faster than both, which points at its queues and batched local execution.
- Each task allocates its buffer on the worker that runs it and then scans that buffer 40 times without yielding. A pin and a local queue are doing different jobs here. The Rayon pair is the pin. The shorter `warm_core` time is how the 24 tasks are handed out.
- This is a single short run on a single machine. On a different processor, with different cache sizes, or under background load the numbers will differ, so 1.4 and 0.95 are not guaranteed results.
- A pin pays off when the kernel would otherwise move a busy thread to another core. This run did not show that cost. Keeping a cache warm matters when the same bytes are scanned many times and something else would evict them between passes. A single streaming pass over memory has little to gain from either.

To reproduce:

```bash
cargo run --release --example compare
```

The example prints the number of logical CPUs and pool workers, the buffer size, the list of CPUs the workers are pinned to, and the time of all three variants.

Hardware counters via `perf` help explain where the difference comes from. Build the example first and run the binary directly, otherwise `cargo` itself ends up in the statistics:

```bash
cargo build --release --example compare
perf stat -e L1-dcache-load-misses,LLC-load-misses,context-switches,cpu-migrations \
  ./target/release/examples/compare
```

The counters cover the whole process, i.e. all three variants together. To compare them separately, temporarily comment two of them out in `examples/compare.rs`. Pinned workers stay on one CPU each, so a pinned run should record fewer `cpu-migrations` than the unpinned Rayon pool.

The same scenario is also available as a criterion benchmark:

```bash
cargo bench
```

It uses 8 passes instead of 40, and the number of tasks is derived from `available_parallelism()`. The three functions are `rayon`, `rayon_affinity`, and `warm_core`. Criterion repeats the runs and estimates the spread. The example in `examples/` is easier to read; the benchmark gives more reliable numbers.

### Tests

`cargo test` runs 12 library tests and the doc example in `src/lib.rs`.

Queue:

- values are pushed and popped around the ring, and a full queue rejects new ones;
- the write and read positions sit on different cache lines;
- four threads push and four pop 8000 numbers, with none lost or duplicated.

Pool:

- 200 tasks return correct results;
- each worker may run on exactly one CPU — the one it is pinned to;
- `execute_on` with a nonexistent core returns an error;
- a panic in a task does not stop the worker;
- in `LogicalCpus` mode a worker is started on every logical CPU.

Topology and pinning:

- CPU lists such as `0,6` and `0-3,8` are parsed;
- cache sizes such as `32K`, `256K`, `9M` are parsed;
- on the current machine the cache line size is a power of two, and each physical core gets at most one worker;
- `sched_setaffinity` really narrows the thread's mask down to one CPU.

### Project layout

| File | Contents |
|---|---|
| `src/topology.rs` | Reads cores, hyper-threading pairs and caches from `/sys` |
| `src/affinity.rs` | Pins a thread to a single CPU (`pin_to`) |
| `src/queue.rs` | Task queue with positions on separate cache lines |
| `src/pool.rs` | The pool itself: two queues per core, batched execution, `execute` and `execute_on` |
| `examples/compare.rs` | Rayon with and without affinity, plus `warm_core` |
| `benches/locality.rs` | The same comparison using criterion |

### Limitations

The crate works on Linux only: it needs `/sys/devices/system/cpu` and `sched_setaffinity`. The topology is read once when the pool is created. If the process runs with a restricted CPU set (for example via `taskset` or cgroups) and the pool tries to pin a worker to an unavailable CPU, `Pool::new()` returns an error.

## Русский

`warm_core` — небольшой пул потоков для Linux, который старается не гонять данные между ядрами процессора. Каждый рабочий поток закреплён за своим физическим ядром, и задача от начала до конца выполняется на одном месте. Поэтому данные, с которыми она работает, успевают осесть в быстрых кэшах этого ядра (L1 и L2) и остаются там, пока задача не закончится.

Пул сам узнаёт устройство процессора из `/sys/devices/system/cpu`: сколько физических ядер, какие логические CPU делят одно ядро, какого размера кэши. Потоки привязываются к ядрам системным вызовом `sched_setaffinity`. Из внешних зависимостей только `libc`.

Ниже по порядку: немного теории про ядра и кэши, как устроен пул, как им пользоваться и что показал замер.

### Коротко

На Intel i7-8750H одну и ту же работу удалось выполнить так:

| Вариант | Время |
|---|---|
| Rayon, 6 потоков, без affinity | 37.7 мс |
| Rayon, 6 потоков, с той же привязкой, что у пула | 39.5 мс |
| `warm_core` | 26.6 мс |

Сама маска affinity работу не сократила: привязанный Rayon показал **0.95×**. `warm_core` справился примерно в **1.4 раза быстрее** непривязанного Rayon, на 11 мс меньше, и обогнал привязанный Rayon тоже. Все три варианта посчитали одинаковые контрольные суммы, так что работа действительно одна и та же. Подробности замера и оговорки к нему — в разделе [«Что показал замер»](#что-показал-замер).

`cargo test` проходит: 12 тестов библиотеки и 1 doctest.

### Немного теории

#### Ядра и логические CPU

**Физическое ядро** — отдельный вычислительный блок процессора. У i7-8750H их шесть. У каждого ядра есть собственные маленькие и очень быстрые кэши L1 и L2.

**Логический CPU** — то, что операционная система показывает как отдельный процессор. На этой машине их двенадцать, с номерами от `0` до `11`. Двенадцать получается потому, что каждое физическое ядро с помощью hyper-threading (общее название — SMT) исполняет два потока команд одновременно. Оба потока одного ядра делят его L1 и L2.

Пары на i7-8750H выглядят так:

| Физическое ядро | Логические CPU | Кэши ядра |
|---|---|---|
| 0 | 0 и 6 | L1d 32 КБ, L2 256 КБ |
| 1 | 1 и 7 | L1d 32 КБ, L2 256 КБ |
| 2 | 2 и 8 | L1d 32 КБ, L2 256 КБ |
| 3 | 3 и 9 | L1d 32 КБ, L2 256 КБ |
| 4 | 4 и 10 | L1d 32 КБ, L2 256 КБ |
| 5 | 5 и 11 | L1d 32 КБ, L2 256 КБ |

Кэш **L3** на 9 МБ один на весь процессор, им пользуются все ядра.

#### Кэши

Обращение к оперативной памяти стоит процессору сотни тактов, а к L1 — единицы. Поэтому процессор держит недавно использованные данные в кэшах и сначала ищет их там.

Удобная аналогия — рабочий стол и шкаф в соседней комнате:

- **L1** — то, что уже в руках. Места мало, зато доступ мгновенный.
- **L2** — край стола. Места больше, и это всё ещё очень быстро.
- **L3** — тумбочка рядом, одна на всех.
- **Оперативная память** — шкаф. Места много, но каждый поход туда долгий.

Процессор обменивается с памятью не байтами, а **кэш-линиями** по 64 байта: даже если нужна одна переменная, подтягиваются все 64 байта вокруг неё.

**Рабочий набор** (working set) — данные, к которым программа обращается прямо сейчас. Если рабочий набор помещается в L2 и никто его оттуда не вытесняет, программа почти не ходит в медленную память.

#### Что на самом деле происходит при переключении потоков

Есть популярное представление, что при снятии потока с ядра кэш «стирается». Это не так: процессор ничего не обнуляет, линии остаются на месте. Проблема в другом.

Когда на то же ядро садится другой поток со своими данными, их в L1 и L2 этого ядра нет, и процессор подгружает их из L3 или из памяти. Новые линии занимают место и вытесняют старые. Когда первый поток возвращается, его данные уже вытеснены, и ему снова приходится за ними ходить.

Ещё хуже, если планировщик переносит поток на **другое физическое ядро**. L1 и L2 там свои, и всё, что было «тёплым» на старом ядре, на новом приходится загружать заново. Часть данных может найтись в общем L3, но он заметно медленнее и тоже вытесняется чужими данными.

Обычная программа не может запретить операционной системе вытеснять свои потоки. Зато можно привязать поток к одному ядру и не отдавать это ядро посреди задачи. `warm_core` делает и то и другое. Замер ниже гоняет один и тот же пул Rayon с привязкой и без неё, так что маска и собственные очереди пула видны по отдельности.

### Как устроен пул

При вызове `Pool::new()` происходит следующее:

1. Из `/sys/devices/system/cpu` читается топология: номер физического ядра у каждого логического CPU, его соседи по hyper-threading и параметры кэшей.
2. По умолчанию запускается **по одному рабочему потоку на физическое ядро**, а не на каждый логический CPU. На i7-8750H это 6 потоков на CPU `0–5`. Вторые половины пар (`6–11`) пул не занимает: два потока пула на одном ядре делили бы L1 и L2 и вытесняли бы данные друг друга.
3. Каждый рабочий поток привязывается к одному логическому CPU через `sched_setaffinity` (`pin_to`), и планировщик больше не переносит его на другие ядра. Вытеснить его на время другим процессом система по-прежнему может, и свободный «сосед» по hyper-threading тоже могут занять другие программы — пул этим не управляет.
4. У каждого рабочего потока две очереди задач:
   - **общая** — для `execute`; из неё задачи может забрать другой поток;
   - **закреплённая** — для `execute_on`; эти задачи выполняются только на своём ядре.
5. Каждая очередь — кольцевой буфер на 4096 задач по алгоритму Дмитрия Вьюкова. Класть и забирать задачи могут сразу несколько потоков. Позиции записи и чтения лежат на разных кэш-линиях, и каждый слот тоже занимает свою линию. Без этого каждое добавление задачи портило бы линию, которую рабочий поток в этот момент читает, и ядра перебрасывали бы её друг другу. Это называется false sharing: данные логически независимы, но физически лежат в одной линии.

Дальше рабочий поток живёт в простом цикле:

1. Выполняет подряд до 32 задач из своих очередей: сначала закреплённые, потом общие. В это время он не смотрит на соседей, и ядро работает с одним и тем же набором данных.
2. Если своих задач нет, пробует забрать задачу из общей очереди соседа, начиная с самого загруженного. Закреплённые задачи чужих ядер не трогает: их отправили на конкретное ядро как раз ради его кэша.
3. Если работы нет нигде, засыпает (`park`) до тех пор, пока в его очередь не положат новую задачу.

Режим `Layout::LogicalCpus` запускает рабочий поток на каждом логическом CPU, включая вторые половины пар hyper-threading. Он может пригодиться, когда задачи почти не зависят от кэша. Если рабочий набор задачи сравним с L2, режим по умолчанию обычно выгоднее.

### Путь одной задачи

`execute` кладёт задачу в общую очередь ядра, у которого сейчас меньше всего задач. При равенстве ядра выбираются по кругу, чтобы всё не скапливалось на нулевом. Если какое-то ядро освободится раньше, оно может забрать эту задачу себе.

`execute_on(core, ...)` кладёт задачу в закреплённую очередь указанного физического ядра. Там она и будет выполнена. В режиме `LogicalCpus`, где на ядре два рабочих потока, задача достанется менее загруженному из них.

Оба вызова возвращают `JobHandle`, и вызывающий поток может продолжать работу. Результат забирается через `wait()`, который ждёт завершения задачи. Если очередь ядра заполнена (4096 задач), вызов не возвращает ошибку: он ждёт, пока в очереди освободится место.

Если задача запаникует, паника будет передана в `wait()`. Сам рабочий поток при этом продолжит работу и возьмёт следующую задачу.

Когда пул уничтожается, он сообщает потокам об остановке, будит их и ждёт, пока они выполнят все задачи, которые уже стоят в очередях.

### Как пользоваться

```rust
use warm_core::{Config, Layout, Pool, Topology};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Пул по умолчанию: один закреплённый поток на физическое ядро,
    // до 32 задач подряд из своей очереди.
    let pool = Pool::new()?;

    println!("рабочих потоков: {}", pool.worker_count());
    println!("закреплены за CPU: {:?}", pool.pinned_cpus());

    // Задача уходит на наименее загруженное ядро.
    // wait() ждёт, пока её выполнят, и возвращает результат.
    let sum = pool.execute(|| 2 + 2).wait();
    assert_eq!(sum, 4);

    // Задача выполнится именно на выбранном ядре.
    // Удобно, когда несколько задач подряд работают с одними и теми же данными.
    let core = pool.cores()[0];
    let pinned = pool.execute_on(core, || 10)?.wait();
    assert_eq!(pinned, 10);

    // Рабочий поток на каждом логическом CPU, включая пары hyper-threading.
    let _all_logical = Pool::with_config(Config {
        layout: Layout::LogicalCpus,
        batch: 32,
    })?;

    // Топологию можно прочитать и без пула.
    let topology = Topology::detect()?;
    println!("логических CPU: {}", topology.cpus.len());
    println!("L2 одного ядра: {:?} байт", topology.l2_size_bytes());

    // Тот же вызов, который каждый рабочий поток делает при старте.
    // `cpu` — номер логического CPU.
    warm_core::pin_to(0)?;

    Ok(())
}
```

Если в пуле нет указанного ядра, `execute_on` вернёт `PoolError::UnknownCore`.

`pin_to` публичный: тем же системным вызовом, что и пул, можно привязать и чужие потоки. Так пример `examples/compare.rs` закрепляет рабочие потоки Rayon.

Замыкание, которое передаётся в пул, должно быть `Send + 'static`: его выполняет другой поток, поэтому оно не может ссылаться на локальные переменные вызывающего кода. Нужные данные передаются внутрь через `move`.

### Что показал замер

Замер лежит в `examples/compare.rs`. Сборка `--release`, процессор Intel Core i7-8750H (6 ядер, 12 потоков, L2 256 КБ на ядро). Rayon — dev-зависимость примера и бенчмарка criterion; сама библиотека по-прежнему зависит только от `libc`.

Все три варианта выполняют одинаковую работу:

- задач вдвое больше, чем логических CPU, — на этой машине 24;
- у каждой задачи свой буфер размером с L2 одного ядра — здесь 256 КБ;
- задача 40 раз проходит по буферу, увеличивает каждый байт на 1 и попутно считает контрольную сумму;
- в конце суммы всех трёх вариантов сравниваются: так компилятор не выбросит вычисления, а мы убеждаемся, что результат одинаковый.

В каждом варианте 6 рабочих потоков, по одному на физическое ядро. Пулы создаются до начала замера, и между проходами никто не вызывает `yield_now`.

**Rayon без affinity** — пул Rayon из 6 потоков. Задачи — один `into_par_iter`. Планировщик может переносить эти потоки с одного ядра процессора на другое.

**Rayon с affinity** — тот же пул и тот же `into_par_iter`. При старте каждый поток вызывает `pin_to` для одного CPU из `Pool::pinned_cpus()` — на этой машине это CPU 0–5. От первого пула его отличает только маска `sched_setaffinity`.

**`warm_core`** занимает те же шесть CPU. Задачи отправляются через `execute` и забираются через `wait`. Каждый поток сначала доделывает свою очередь пачками и только потом смотрит в очередь другого ядра.

Прежний прогон этого примера сравнивал 24 потока `std::thread` с `yield_now` после каждого прохода (37 мс) и пул (31 мс, около 1.2×). В ту цифру попадали переподписка, лишние переключения и создание потоков. Ниже — цифры, которые её заменяют.

Результат:

| | Rayon | Rayon + affinity | `warm_core` |
|---|---:|---:|---:|
| Время | 37.7 мс | 39.5 мс | 26.6 мс |
| Относительно Rayon | 1.0 | 0.95 | **1.4** |

Как читать эти цифры:

- Привязанный пул Rayon потратил примерно на 1.8 мс больше: `39.5 − 37.7 = 1.8`. Отношение `37.7 / 39.5 ≈ 0.95` значит, что привязка этот прогон не ускорила.
- `warm_core` закончил на 11 мс раньше непривязанного Rayon: `37.7 − 26.6 = 11.1`. Ускорение: `37.7 / 26.6 ≈ 1.42`, после округления 1.4. Он быстрее и привязанного Rayon (`39.5 / 26.6 ≈ 1.5`).
- Если запускать такую пачку непрерывно, непривязанный Rayon успеет около 27 пачек в секунду (`1000 / 37.7`), а пул — около 38 (`1000 / 26.6`).

Важные оговорки:

- У двух колонок Rayon один и тот же планировщик и одно и то же число потоков, поэтому разница между ними — это маска affinity. На этом прогоне она маленькая и в сторону замедления. `warm_core` быстрее обоих пулов Rayon, и это уже его очереди и выполнение пачками.
- Каждая задача выделяет буфер на том потоке, который её выполняет, и 40 раз проходит по нему, не отдавая процессор. Привязка и локальная очередь здесь делают разную работу. Пара Rayon — это привязка. Более короткое время `warm_core` — это то, как раздаются 24 задачи.
- Это один короткий прогон на одной машине. На другом процессоре, с другими размерами кэшей или под фоновой нагрузкой цифры будут другими, поэтому 1.4 и 0.95 — не гарантированный результат.
- Привязка окупается, когда ядро системы иначе перенесло бы занятый поток на другое ядро процессора. Этот прогон такой цены не показал. Держать кэш тёплым имеет смысл, когда одни и те же байты читаются много раз и между проходами их кто-то вытесняет. Один сквозной проход по памяти почти ничего не выигрывает ни от привязки, ни от очередей.

Повторить замер:

```bash
cargo run --release --example compare
```

Пример выведет число логических CPU и рабочих потоков пула, размер буфера, список CPU, за которыми закреплены потоки, и время всех трёх вариантов.

Понять, откуда берётся разница, помогают аппаратные счётчики через `perf`. Пример лучше собрать заранее и запускать готовый бинарник, иначе в статистику попадёт и сам `cargo`:

```bash
cargo build --release --example compare
perf stat -e L1-dcache-load-misses,LLC-load-misses,context-switches,cpu-migrations \
  ./target/release/examples/compare
```

Счётчики собираются по всему процессу сразу, то есть по всем трём вариантам вместе. Чтобы сравнить варианты по отдельности, временно закомментируйте два из них в `examples/compare.rs`. Привязанные потоки остаются на одном CPU каждый, поэтому у такого прогона `cpu-migrations` должно быть меньше, чем у непривязанного Rayon.

Тот же сценарий есть в виде бенчмарка на criterion:

```bash
cargo bench
```

Там 8 проходов вместо 40, а число задач считается от `available_parallelism()`. Три функции — `rayon`, `rayon_affinity` и `warm_core`. Criterion сам повторяет прогоны и оценивает разброс. Пример из `examples/` проще читать, а бенчмарк даёт более надёжные цифры.

### Тесты

`cargo test` запускает 12 тестов библиотеки и пример из документации в `src/lib.rs`.

Очередь:

- значения кладутся и забираются по кольцу, а заполненная очередь отказывается принимать новые;
- позиции записи и чтения лежат на разных кэш-линиях;
- четыре потока кладут и четыре забирают 8000 чисел, при этом ни одно не теряется и не дублируется.

Пул:

- 200 задач возвращают правильные результаты;
- каждый рабочий поток может исполняться ровно на одном CPU — на том, за которым он закреплён;
- `execute_on` с несуществующим ядром возвращает ошибку;
- паника в задаче не останавливает рабочий поток;
- в режиме `LogicalCpus` рабочий поток запускается на каждом логическом CPU.

Топология и привязка:

- разбираются списки CPU вида `0,6` и `0-3,8`;
- разбираются размеры кэшей вида `32K`, `256K`, `9M`;
- на текущей машине размер кэш-линии — степень двойки, и на каждое физическое ядро приходится не больше одного рабочего потока;
- `sched_setaffinity` действительно сужает маску потока до одного CPU.

### Структура проекта

| Файл | Что внутри |
|---|---|
| `src/topology.rs` | Чтение ядер, пар hyper-threading и кэшей из `/sys` |
| `src/affinity.rs` | Привязка потока к одному CPU (`pin_to`) |
| `src/queue.rs` | Очередь задач с позициями на разных кэш-линиях |
| `src/pool.rs` | Сам пул: две очереди на ядро, выполнение пачками, `execute` и `execute_on` |
| `examples/compare.rs` | Rayon с affinity и без неё, плюс `warm_core` |
| `benches/locality.rs` | То же сравнение на criterion |

### Ограничения

Крейт работает только в Linux: ему нужны `/sys/devices/system/cpu` и `sched_setaffinity`. Топология читается один раз при создании пула. Если процесс запущен с ограниченным набором CPU (например, через `taskset` или cgroups), а пул попытается закрепить поток за недоступным CPU, `Pool::new()` вернёт ошибку.
