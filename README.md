# warm_core

[English](#english) · [Русский](#русский)

## English

`warm_core` is a small thread pool for Linux that tries not to shuffle data between CPU cores. Each worker thread is pinned to its own physical core, and a task, once a worker starts it, runs to completion there. `execute_on` keeps the task on the core whose L1 and L2 already hold its data. `execute` may place it on another core, and an idle neighbour may steal it, so that call does not promise a particular core's cache.

The pool discovers the CPU layout on its own from `/sys/devices/system/cpu` and `/sys/devices/system/node`: how many physical cores there are, which logical CPUs share a core, how large the caches are, and which NUMA node each CPU belongs to. CPUs outside the process cpuset are left out, so a container does not grow a worker it is not allowed to run. Threads are pinned with `sched_setaffinity`. New allocations on a worker prefer that CPU's NUMA node: the pin alone does not place memory. The only external dependency is `libc`.

The sections below cover, in order: a bit of background on cores and caches, how the pool works, how to use it, and what the benchmark showed.

### In short

On an Intel i7-8750H, the same workload took:

| Variant | Time |
|---|---|
| Rayon, 6 workers, no affinity | 37.7 ms |
| Rayon, 6 workers, pinned like the pool | 39.5 ms |
| `warm_core` | 26.6 ms |

The affinity mask by itself did not shorten the work: pinned Rayon came out at **0.95×**. `warm_core` finished about **1.4× faster** than unpinned Rayon, 11 ms less, and it was also faster than the pinned Rayon pool. All three variants produced identical checksums, so they really did the same work. Details and caveats are in [What the benchmark shows](#what-the-benchmark-shows).

`cargo test` passes: 29 library tests and 1 doctest.

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

1. The topology is read from `/sys/devices/system/cpu` and `/sys/devices/system/node`: the physical core of every logical CPU, its hyper-threading siblings, the cache parameters, and the NUMA node. CPUs missing from the process cpuset are dropped. A CPU whose NUMA node is outside `cpuset.mems` is dropped too, when that still leaves a CPU to run on.
2. By default, **one worker thread per physical core** is started, not one per logical CPU. On the i7-8750H that is 6 threads on CPUs `0–5`. The other halves of the pairs (`6–11`) are left alone: two pool threads on the same core would share L1 and L2 and evict each other's data. If the cpuset only contains the other half of a pair, the worker is pinned to that half.
3. Each worker is pinned to a single logical CPU via `sched_setaffinity` (`pin_to`), so the scheduler no longer moves it to other cores. It then prefers its NUMA node for new anonymous pages (`set_mempolicy`). Pages already allocated on another node stay there: the pin does not migrate them. The system can still preempt the thread for another process, and other programs can still use the idle hyper-threading sibling.
4. Each worker has two task queues:
   - a **shared** queue for `execute`; other workers may take tasks from it;
   - a **pinned** queue for `execute_on`; these tasks run only on their own core.
5. Each queue is a ring buffer of 4096 tasks based on Dmitry Vyukov's algorithm. Several threads can push and pop at the same time. The write and read positions live on separate cache lines, and each slot occupies its own line as well. Without that, every push would dirty the line the worker is reading at that moment, and cores would bounce it back and forth. This is called false sharing: the data is logically independent but physically sits in one line.

How the cpuset is found, and why a pin does not place memory, is written up in [LOCALITY.md](LOCALITY.md).

Each worker then runs a simple loop:

1. It runs up to 32 tasks in a row from its own queues. While both have work, it takes three pinned tasks and then one shared task (`Config::pinned_per_shared`, default 3). An empty queue does not spend its turn. The count carries across batches, so a short batch cannot starve the shared queue. During this time the worker ignores its neighbours, so the core keeps working with the same data.
2. If it has no tasks of its own, it tries to take one from a neighbour's shared queue, starting with the busiest. It never touches other cores' pinned tasks: those were sent to a specific core precisely for its cache.
3. If there is no work anywhere, it goes to sleep (`park`) until a new task is pushed into its queue.

The quota, and why the count has to survive a batch boundary, is written up in [FAIRNESS.md](FAIRNESS.md).

`Layout::LogicalCpus` starts a worker on every logical CPU, including the second halves of hyper-threading pairs. It can be useful when tasks barely depend on the cache. If a task's working set is comparable to L2, the default layout is usually the better choice.

### Life of a task

`execute` puts the task into the shared queue of the core that currently has the fewest tasks. On a tie it prefers the NUMA node of the calling thread, then round-robin, so work does not pile up on core zero and the task stays next to memory that thread already touched. If that queue is full, the other cores are tried too. If another core becomes idle first, it may take the task. A buffer warmed on one core is therefore not guaranteed to be scanned there again.

`execute_on(core, ...)` puts the task into the pinned queue of the given physical core, and that is where it runs. An idle neighbour does not steal it. Calling it again with the same core keeps the working set in that core's L1 and L2. In `LogicalCpus` mode, where a core has two workers, both pinned queues are tried, least loaded first.

Both calls return a `JobHandle`, so the calling thread can keep working. The result is retrieved with `wait()`, which blocks until the task finishes. Each queue holds 4096 tasks. When every queue the call may use is full, `execute` and `execute_on` sleep until a slot appears, but no longer than `Config::submit_timeout` (one second by default), and then return `PoolError::QueueFull`. `try_execute` and `try_execute_on` do not sleep: a full queue is an immediate `QueueFull`. The closure has already been moved into the call, so on `QueueFull` it is dropped and the task never starts. How the wait is bounded, and why a full queue sleeps on a futex, is written up in [BACKPRESSURE.md](BACKPRESSURE.md).

If a task panics, the panic is propagated to `wait()`. The worker itself keeps running and picks up the next task.

When the pool is dropped, it tells the workers to stop, wakes them up, and waits until they finish every task already in their queues.

### State that stays on a core

`execute_on` keeps a task on a core. It does not keep the value: the closure still borrows or owns whatever the caller moved in, and the result comes back on a channel. [`Shard`](src/shard.rs) is the value that does not leave. `shard_on` runs the constructor on that core's only worker, so the first write of the allocation happens there. Another thread never gets `&T`. `call` runs a closure on the owner and waits. `defer` only enqueues one, which is how one core asks another to do something without blocking its own loop. The reply is a `defer` back. A `call` from one worker into another can deadlock if the other side calls back. Two workers on one core (`LogicalCpus`) cannot share a shard. The details are in [OWNERSHIP.md](OWNERSHIP.md).

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
    let sum = pool.execute(|| 2 + 2)?.wait();
    assert_eq!(sum, 4);

    // The task runs on exactly the chosen core.
    // Handy when several tasks in a row work with the same data.
    let core = pool.cores()[0];
    let pinned = pool.execute_on(core, || 10)?.wait();
    assert_eq!(pinned, 10);

    // The value stays on this core. The caller only gets back what `call` returns.
    let shard = pool.shard_on(core, || 0)?;
    shard.call(|value| *value += 1)?;
    assert_eq!(shard.call(|value| *value)?, 1);

    // A worker on every logical CPU, hyper-threading siblings included.
    let _all_logical = Pool::with_config(Config {
        layout: Layout::LogicalCpus,
        batch: 32,
        pinned_per_shared: 3,
        submit_timeout: std::time::Duration::from_secs(1),
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

If the pool has no such core, `execute_on` and `try_execute_on` return `PoolError::UnknownCore`. A full pinned queue of a core that does exist is `PoolError::QueueFull`.

`pin_to` is public, so other threads — the Rayon workers in `examples/compare.rs`, for instance — can be pinned with the same system call the pool uses.

A closure passed to the pool must be `Send + 'static`: it runs on another thread, so it cannot borrow local variables of the calling code. Move the data it needs inside with `move`.

### What the benchmark shows

How this comparison was put together, and why the old 37 ms / 31 ms figure was retired, is written up in [BENCHMARK.md](BENCHMARK.md).

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
- These tasks call `execute` and allocate the buffer inside the task. The first touch happens on whichever worker runs it, and a later task is a new buffer. The run does not show a working set that already lives on one core. That measurement is `examples/stateful.rs`.

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

### Stateful working set

`examples/stateful.rs` keeps one L2-sized buffer per core. `execute_on` builds a cycle in that buffer on the core, then the example walks the cycle again. The next index is the value just loaded, so a cache miss stays on the critical path. `execute_on` stays on that core. `execute` goes through the shared queue, which an idle neighbour may steal, so the same bytes are not promised to that core's L1 and L2.

The example prints p50 and p99 of the time spent inside the task, not the time spent waiting in the queue. L1d and last-level load misses come from `perf_event_open` on each worker thread. If the kernel refuses the counters, the latencies are still printed. The figures belong to the machine they were measured on. `cargo test` does not assert them.

```bash
cargo run --release --example stateful
```

One release run on the Intel i7-8750H (6 cores, 256 KiB of L2, 128 rounds, 32768 dependent steps per task):

| Call | p50 | p99 |
|---|---:|---:|
| `execute_on` | 154 µs | 268 µs |
| `execute` | 159 µs | 485 µs |

p50 stays close: many `execute` tasks still find the lines in a nearby cache. p99 does not. `485 / 268 ≈ 1.8`, and that interval is the walk itself, not the wait in the queue. `perf_event_open` returned `EPERM` because `perf_event_paranoid` is 4, so this run has no miss counts. A kernel that allows the counters will fill those two columns. The latencies are this machine's.

### Tests

`cargo test` runs 38 library tests and the doc examples in `src/lib.rs` and `src/shard.rs`.

Queue:

- values are pushed and popped around the ring, and a full queue rejects new ones;
- the write and read positions sit on different cache lines;
- four threads push and four pop 8000 numbers, with none lost or duplicated;
- a Loom model checks that `len` does not wrap while one value moves from `push` to `pop`. It stays outside `cargo test`; see [QUEUE.md](QUEUE.md).

Pool:

- 200 tasks return correct results;
- each worker may run on exactly one CPU — the one it is pinned to;
- `execute_on` with a nonexistent core returns an error;
- a panic in a task does not stop the worker;
- in `LogicalCpus` mode a worker is started on every logical CPU;
- a shared task still starts within `pinned_per_shared` pinned tasks when that worker's pinned queue is kept full. See [FAIRNESS.md](FAIRNESS.md);
- `try_execute` on a full shared queue returns `QueueFull` without waiting out `submit_timeout`;
- `execute` returns `QueueFull` once `submit_timeout` expires;
- `execute` finishes if a worker frees a slot while the caller is asleep;
- `try_execute_on` returns `UnknownCore` for a missing core and `QueueFull` when that core's pinned queue is full;
- the waiter count and the futex word sit on different cache lines;
- a wait ends when its timeout expires;
- freeing a slot wakes a thread that is waiting for one;
- an idle neighbour runs a shared task and does not run a pinned task queued beside it.

Shard:

- `init` and `call` run on the CPU that worker is pinned to, and a later `call` sees the mutation;
- `T` does not have to be `Send`;
- `call` from the owner thread runs inline, so it does not wait for a queue that only this thread drains;
- a nested `call` on the same shard returns `Reentrant`, and the shard still accepts the next call;
- `defer` from one shard to another runs on the target CPU, and the reply `defer` writes the result back;
- a missing core and a core with two workers are different errors;
- a panic inside `call` does not stop the worker;
- dropping the last handle from another thread, and dropping the pool while a handle is still held, both drop the value on the owner's CPU.

Topology and pinning:

- CPU lists such as `0,6` and `0-3,8` are parsed;
- cache sizes such as `32K`, `256K`, `9M` are parsed;
- on the current machine the cache line size is a power of two, and each physical core gets at most one worker;
- `sched_setaffinity` really narrows the thread's mask down to one CPU;
- a cgroup v2 cpuset is read from the ancestor that actually has `cpuset.cpus.effective`, and a hybrid v1+v2 setup intersects the two lists;
- NUMA nodes come from `nodeN/cpulist`;
- a cpuset that only contains one hyper-thread keeps that thread as the worker for its core;
- CPUs on a memory node outside `cpuset.mems` are dropped when another node is still usable;
- `set_mempolicy` accepts the node the thread is running on.

### Project layout

| File | Contents |
|---|---|
| `src/topology.rs` | Reads cores, SMT pairs, caches and NUMA nodes from `/sys`, then drops CPUs outside the cpuset |
| `src/affinity.rs` | Pins a thread to a single CPU (`pin_to`) and prefers that CPU's NUMA node for new pages |
| `src/cpuset.rs` | Reads the process cpuset: allowed CPUs and memory nodes |
| `src/queue.rs` | Task queue with positions on separate cache lines |
| `src/gate.rs` | Bounded sleep until a queue slot is free, without a lock on every pop |
| `src/pool.rs` | The pool itself: two queues per core, batched execution, `execute`, `try_execute`, and `execute_on` |
| `src/shard.rs` | Per-core state: `shard_on`, `call`, and `defer` |
| `examples/compare.rs` | Rayon with and without affinity, plus `warm_core` |
| `examples/stateful.rs` | `execute_on` against `execute` on one buffer per core: in-task p99 and cache misses |
| `examples/shards.rs` | Two cores, each owning a buffer, talking with `defer` |
| `benches/locality.rs` | The same comparison using criterion |
| `BENCHMARK.md` | Why the comparison changed, and what the run showed |
| `QUEUE.md` | Why `len` grows before a slot is published, and how Loom checks it |
| `FAIRNESS.md` | Why a steady pinned load leaves a slot for the shared queue |
| `BACKPRESSURE.md` | Why a full queue sleeps for a bounded time instead of spinning |
| `LOCALITY.md` | Why the pool drops CPUs outside the cpuset, and why a pin does not place memory |
| `OWNERSHIP.md` | Why a pinned task is not yet per-core state, and how `call` differs from `defer` |

### Limitations

The crate works on Linux only: it needs `/sys/devices/system/cpu` and `sched_setaffinity`. When cgroup files are mounted, it also reads the process cpuset. The topology is read once when the pool is created. If that cpuset leaves no CPU, `Pool::new()` returns an error. `set_mempolicy` is best-effort: a sandbox that rejects it still starts the pool. `taskset` on the calling thread does not shrink the pool, because workers may widen their affinity back to the whole cpuset. A warm L1/L2 for a particular buffer is what `execute_on` provides. `execute` may move the task, including by work stealing. A value that must stay on one core is a [`Shard`](src/shard.rs): other cores reach it only through `call` and `defer`. `call` from one worker into another deadlocks if that worker calls back. This is not a reactor. See [OWNERSHIP.md](OWNERSHIP.md).

## Русский

`warm_core` — небольшой пул потоков для Linux, который старается не гонять данные между ядрами процессора. Каждый рабочий поток закреплён за своим физическим ядром, и задача, которую он начал, дорабатывается на нём до конца. `execute_on` оставляет задачу на ядре, в чьих L1 и L2 её данные уже лежат. `execute` может положить её на другое ядро, а простаивающий сосед — украсть, поэтому кэш конкретного ядра этот вызов не обещает.

Пул сам узнаёт устройство процессора из `/sys/devices/system/cpu` и `/sys/devices/system/node`: сколько физических ядер, какие логические CPU делят одно ядро, какого размера кэши и к какому узлу NUMA относится каждый CPU. CPU вне cpuset процесса не занимаются, поэтому контейнер не получает worker, которому запрещено исполняться. Потоки привязываются вызовом `sched_setaffinity`. Новые выделения на worker предпочитают узел NUMA этого CPU: сама привязка память не размещает. Из внешних зависимостей только `libc`.

Ниже по порядку: немного теории про ядра и кэши, как устроен пул, как им пользоваться и что показал замер.

### Коротко

На Intel i7-8750H одну и ту же работу удалось выполнить так:

| Вариант | Время |
|---|---|
| Rayon, 6 потоков, без affinity | 37.7 мс |
| Rayon, 6 потоков, с той же привязкой, что у пула | 39.5 мс |
| `warm_core` | 26.6 мс |

Сама маска affinity работу не сократила: привязанный Rayon показал **0.95×**. `warm_core` справился примерно в **1.4 раза быстрее** непривязанного Rayon, на 11 мс меньше, и обогнал привязанный Rayon тоже. Все три варианта посчитали одинаковые контрольные суммы, так что работа действительно одна и та же. Подробности замера и оговорки к нему — в разделе [«Что показал замер»](#что-показал-замер).

`cargo test` проходит: 29 тестов библиотеки и 1 doctest.

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

1. Из `/sys/devices/system/cpu` и `/sys/devices/system/node` читается топология: номер физического ядра у каждого логического CPU, его соседи по hyper-threading, параметры кэшей и узел NUMA. CPU, которых нет в cpuset процесса, отбрасываются. CPU, чей узел не входит в `cpuset.mems`, тоже отбрасывается, если после этого ещё есть где исполняться.
2. По умолчанию запускается **по одному рабочему потоку на физическое ядро**, а не на каждый логический CPU. На i7-8750H это 6 потоков на CPU `0–5`. Вторые половины пар (`6–11`) пул не занимает: два потока пула на одном ядре делили бы L1 и L2 и вытесняли бы данные друг друга. Если cpuset оставил только вторую половину пары, worker сажается на неё.
3. Каждый рабочий поток привязывается к одному логическому CPU через `sched_setaffinity` (`pin_to`), и планировщик больше не переносит его на другие ядра. Затем поток предпочитает узел NUMA этого CPU для новых анонимных страниц (`set_mempolicy`). Страницы, уже выделенные на другом узле, остаются там: привязка их не переносит. Вытеснить поток на время другим процессом система по-прежнему может, и свободный «сосед» по hyper-threading тоже могут занять другие программы.
4. У каждого рабочего потока две очереди задач:
   - **общая** — для `execute`; из неё задачи может забрать другой поток;
   - **закреплённая** — для `execute_on`; эти задачи выполняются только на своём ядре.
5. Каждая очередь — кольцевой буфер на 4096 задач по алгоритму Дмитрия Вьюкова. Класть и забирать задачи могут сразу несколько потоков. Позиции записи и чтения лежат на разных кэш-линиях, и каждый слот тоже занимает свою линию. Без этого каждое добавление задачи портило бы линию, которую рабочий поток в этот момент читает, и ядра перебрасывали бы её друг другу. Это называется false sharing: данные логически независимы, но физически лежат в одной линии.

Как находится cpuset и почему привязка потока не размещает память, разобрано в [LOCALITY.md](LOCALITY.md).

Дальше рабочий поток живёт в простом цикле:

1. Выполняет подряд до 32 задач из своих очередей. Пока задачи есть в обеих, он берёт три закреплённые и затем одну общую (`Config::pinned_per_shared`, по умолчанию 3). Пустая очередь не сжигает свой слот. Счётчик не обнуляется на границе пачки, поэтому короткая пачка не морит общую очередь голодом. В это время он не смотрит на соседей, и ядро работает с одним и тем же набором данных.
2. Если своих задач нет, пробует забрать задачу из общей очереди соседа, начиная с самого загруженного. Закреплённые задачи чужих ядер не трогает: их отправили на конкретное ядро как раз ради его кэша.
3. Если работы нет нигде, засыпает (`park`) до тех пор, пока в его очередь не положат новую задачу.

Квота и то, почему счётчик должен переживать границу пачки, разобраны в [FAIRNESS.md](FAIRNESS.md).

Режим `Layout::LogicalCpus` запускает рабочий поток на каждом логическом CPU, включая вторые половины пар hyper-threading. Он может пригодиться, когда задачи почти не зависят от кэша. Если рабочий набор задачи сравним с L2, режим по умолчанию обычно выгоднее.

### Путь одной задачи

`execute` кладёт задачу в общую очередь ядра, у которого сейчас меньше всего задач. При равенстве сначала выбирается узел NUMA вызывающего потока, и только потом круг, чтобы задачи не скапливались на нулевом ядре и оставались рядом с памятью, которую этот поток уже трогал. Если эта очередь полна, пробуются и остальные ядра. Если какое-то ядро освободится раньше, оно может забрать эту задачу себе. Буфер, прогретый на одном ядре, следующий вызов не обязан прочитать там же.

`execute_on(core, ...)` кладёт задачу в закреплённую очередь указанного физического ядра. Там она и будет выполнена: простаивающий сосед её не крадёт. Повторный вызов с тем же ядром держит рабочий набор в L1 и L2 этого ядра. В режиме `LogicalCpus`, где на ядре два рабочих потока, пробуются обе закреплённые очереди, сначала у менее загруженного.

Оба вызова возвращают `JobHandle`, и вызывающий поток может продолжать работу. Результат забирается через `wait()`, который ждёт завершения задачи. В каждой очереди 4096 задач. Когда все очереди, куда вызов может положить задачу, заполнены, `execute` и `execute_on` спят, пока не появится слот, но не дольше `Config::submit_timeout` (по умолчанию одна секунда), а затем возвращают `PoolError::QueueFull`. `try_execute` и `try_execute_on` не спят: полная очередь — это сразу `QueueFull`. Замыкание к этому моменту уже передано вызову, поэтому при `QueueFull` оно уничтожается и задача не запускается. Как ограничен срок ожидания и почему полная очередь засыпает на futex, разобрано в [BACKPRESSURE.md](BACKPRESSURE.md).

Если задача запаникует, паника будет передана в `wait()`. Сам рабочий поток при этом продолжит работу и возьмёт следующую задачу.

Когда пул уничтожается, он сообщает потокам об остановке, будит их и ждёт, пока они выполнят все задачи, которые уже стоят в очередях.

### Состояние, которое остаётся на ядре

`execute_on` удерживает на ядре задачу, но не значение: замыкание по-прежнему забирает то, что передал вызывающий, а результат возвращается по каналу. [`Shard`](src/shard.rs) — это значение, которое не уезжает. `shard_on` запускает конструктор на единственном worker этого ядра, так что первая запись выделения происходит там. Чужой поток не получает `&T`. `call` выполняет замыкание у владельца и ждёт. `defer` только ставит замыкание в очередь: так одно ядро просит другое что-то сделать и не блокирует свой цикл. Ответ — встречный `defer`. `call` с одного worker на другой зависнет, если тот позовёт обратно. Два worker на одном ядре (`LogicalCpus`) шард не делят. Подробности — в [OWNERSHIP.md](OWNERSHIP.md).

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
    let sum = pool.execute(|| 2 + 2)?.wait();
    assert_eq!(sum, 4);

    // Задача выполнится именно на выбранном ядре.
    // Удобно, когда несколько задач подряд работают с одними и теми же данными.
    let core = pool.cores()[0];
    let pinned = pool.execute_on(core, || 10)?.wait();
    assert_eq!(pinned, 10);

    // Значение остаётся на этом ядре. Наружу уходит только то, что вернул `call`.
    let shard = pool.shard_on(core, || 0)?;
    shard.call(|value| *value += 1)?;
    assert_eq!(shard.call(|value| *value)?, 1);

    // Рабочий поток на каждом логическом CPU, включая пары hyper-threading.
    let _all_logical = Pool::with_config(Config {
        layout: Layout::LogicalCpus,
        batch: 32,
        pinned_per_shared: 3,
        submit_timeout: std::time::Duration::from_secs(1),
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

Если в пуле нет указанного ядра, `execute_on` и `try_execute_on` вернут `PoolError::UnknownCore`. Полная закреплённая очередь существующего ядра — это `PoolError::QueueFull`.

`pin_to` публичный: тем же системным вызовом, что и пул, можно привязать и чужие потоки. Так пример `examples/compare.rs` закрепляет рабочие потоки Rayon.

Замыкание, которое передаётся в пул, должно быть `Send + 'static`: его выполняет другой поток, поэтому оно не может ссылаться на локальные переменные вызывающего кода. Нужные данные передаются внутрь через `move`.

### Что показал замер

Как собрано это сравнение и почему старые 37 мс / 31 мс больше не цитируются как результат, записано в [BENCHMARK.md](BENCHMARK.md).

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
- Эти задачи вызывают `execute` и выделяют буфер внутри задачи. Первое касание происходит на том worker, который её взял, а следующая задача — это уже новый буфер. Прогон не показывает рабочий набор, который уже живёт на одном ядре. Такой замер — `examples/stateful.rs`.

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

### Повторно используемый набор

`examples/stateful.rs` держит по одному буферу размера L2 на ядро. `execute_on` строит в этом буфере цикл на самом ядре, затем пример обходит цикл снова. Следующий индекс — это только что прочитанное значение, поэтому промах кэша остаётся на критическом пути. `execute_on` остаётся на этом ядре. `execute` идёт через общую очередь, которую простаивающий сосед может украсть, поэтому те же байты в L1 и L2 этого ядра уже не обещаны.

Пример печатает p50 и p99 времени внутри задачи, а не времени ожидания в очереди. Промахи L1d и кэша последнего уровня читаются через `perf_event_open` на потоке каждого worker. Если ядро отказало в счётчиках, время всё равно печатается. Цифры принадлежат той машине, на которой их сняли. `cargo test` порог по ним не проверяет.

```bash
cargo run --release --example stateful
```

Один прогон `--release` на Intel i7-8750H (6 ядер, L2 256 КБ, 128 раундов, 32768 зависимых шагов на задачу):

| Вызов | p50 | p99 |
|---|---:|---:|
| `execute_on` | 154 мкс | 268 мкс |
| `execute` | 159 мкс | 485 мкс |

p50 почти не расходится: многие задачи `execute` всё ещё находят строки в ближнем кэше. p99 расходится. `485 / 268 ≈ 1.8`, и это время самого обхода, а не ожидания в очереди. `perf_event_open` вернул `EPERM`, потому что `perf_event_paranoid` равен 4, и промахов в этом прогоне нет. Ядро, которое пускает счётчики, заполнит эти две колонки. Время — этой машины.

### Тесты

`cargo test` запускает 38 тестов библиотеки и примеры из документации в `src/lib.rs` и `src/shard.rs`.

Очередь:

- значения кладутся и забираются по кольцу, а заполненная очередь отказывается принимать новые;
- позиции записи и чтения лежат на разных кэш-линиях;
- четыре потока кладут и четыре забирают 8000 чисел, при этом ни одно не теряется и не дублируется;
- модель Loom проверяет, что `len` не оборачивается, пока одно значение переходит из `push` в `pop`. В обычный `cargo test` она не входит; см. [QUEUE.md](QUEUE.md).

Пул:

- 200 задач возвращают правильные результаты;
- каждый рабочий поток может исполняться ровно на одном CPU — на том, за которым он закреплён;
- `execute_on` с несуществующим ядром возвращает ошибку;
- паника в задаче не останавливает рабочий поток;
- в режиме `LogicalCpus` рабочий поток запускается на каждом логическом CPU;
- общая задача всё равно стартует в пределах `pinned_per_shared` закреплённых, когда закреплённая очередь этого потока не пустеет. См. [FAIRNESS.md](FAIRNESS.md);
- `try_execute` на полной общей очереди возвращает `QueueFull` и не ждёт `submit_timeout`;
- `execute` возвращает `QueueFull`, когда выходит `submit_timeout`;
- `execute` завершается, если рабочий поток освобождает слот, пока вызывающий спит;
- `try_execute_on` возвращает `UnknownCore` для несуществующего ядра и `QueueFull`, когда закреплённая очередь этого ядра полна;
- счётчик ожидающих и слово futex лежат на разных кэш-линиях;
- ожидание заканчивается, когда выходит его срок;
- освобождение слота будит поток, который этого слота ждёт;
- простаивающий сосед выполняет общую задачу и не выполняет закреплённую, которая лежит рядом.

Шард:

- `init` и `call` идут на том CPU, к которому привязан worker, и следующий `call` видит изменение;
- `T` не обязан быть `Send`;
- `call` с потока-владельца выполняется сразу и не ждёт очередь, которую разбирает только этот поток;
- вложенный `call` того же шарда возвращает `Reentrant`, и следующий `call` шард по-прежнему принимает;
- `defer` с одного шарда на другой выполняется на CPU адресата, а ответный `defer` дописывает результат владельцу;
- отсутствующее ядро и ядро с двумя worker — разные ошибки;
- паника внутри `call` не останавливает worker;
- и сброс последней ручки с другого потока, и уничтожение пула при живой ручке уничтожают значение на CPU владельца.

Топология и привязка:

- разбираются списки CPU вида `0,6` и `0-3,8`;
- разбираются размеры кэшей вида `32K`, `256K`, `9M`;
- на текущей машине размер кэш-линии — степень двойки, и на каждое физическое ядро приходится не больше одного рабочего потока;
- `sched_setaffinity` действительно сужает маску потока до одного CPU;
- cpuset cgroup v2 читается у предка, где реально лежит `cpuset.cpus.effective`, а гибрид v1 и v2 пересекает оба списка;
- узлы NUMA берутся из `nodeN/cpulist`;
- cpuset, в котором остался только один hyper-thread, сажает worker этого ядра именно на него;
- CPU чужого узла памяти выбрасываются, если `cpuset.mems` ещё оставляет где исполняться;
- `set_mempolicy` принимает узел, на котором поток уже исполняется.

### Структура проекта

| Файл | Что внутри |
|---|---|
| `src/topology.rs` | Чтение ядер, пар SMT, кэшей и узлов NUMA из `/sys`, затем отбрасывание CPU вне cpuset |
| `src/affinity.rs` | Привязка потока к одному CPU (`pin_to`) и предпочтение узла NUMA этого CPU для новых страниц |
| `src/cpuset.rs` | Чтение cpuset процесса: разрешённые CPU и узлы памяти |
| `src/queue.rs` | Очередь задач с позициями на разных кэш-линиях |
| `src/gate.rs` | Сон до свободного слота очереди, без замка на каждом pop |
| `src/pool.rs` | Сам пул: две очереди на ядро, выполнение пачками, `execute`, `try_execute` и `execute_on` |
| `src/shard.rs` | Состояние ядра: `shard_on`, `call` и `defer` |
| `examples/compare.rs` | Rayon с affinity и без неё, плюс `warm_core` |
| `examples/stateful.rs` | `execute_on` против `execute` на одном буфере на ядро: p99 внутри задачи и промахи кэша |
| `examples/shards.rs` | Два ядра, у каждого свой буфер, разговор через `defer` |
| `benches/locality.rs` | То же сравнение на criterion |
| `BENCHMARK.md` | Почему сравнение поменялось и что показал прогон |
| `QUEUE.md` | Почему `len` растёт до публикации слота и как это проверяет Loom |
| `FAIRNESS.md` | Почему постоянный поток закреплённых задач оставляет слот общей очереди |
| `BACKPRESSURE.md` | Почему полная очередь спит ограниченное время, а не крутится в `yield_now` |
| `LOCALITY.md` | Почему пул отбрасывает CPU вне cpuset и почему привязка не размещает память |
| `OWNERSHIP.md` | Почему закреплённая задача ещё не состояние ядра и чем `call` отличается от `defer` |

### Ограничения

Крейт работает только в Linux: ему нужны `/sys/devices/system/cpu` и `sched_setaffinity`. Если смонтированы файлы cgroup, пул читает и cpuset процесса. Топология читается один раз при создании пула. Если в cpuset не осталось ни одного CPU, `Pool::new()` вернёт ошибку. `set_mempolicy` — по возможности: песочница, которая его запрещает, пул всё равно запускает. `taskset` на вызывающем потоке пул не сужает, потому что worker может расширить свою маску обратно до всего cpuset. Тёплые L1 и L2 конкретного буфера даёт `execute_on`. `execute` может перенести задачу, в том числе кражей. Значение, которое должно остаться на одном ядре, — это [`Shard`](src/shard.rs): остальные ядра дотягиваются до него только через `call` и `defer`. `call` с одного worker на другой зависнет, если тот позовёт обратно. Реактора здесь нет. См. [OWNERSHIP.md](OWNERSHIP.md).
