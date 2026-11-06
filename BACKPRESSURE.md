# Обратное давление при постановке задачи

[English](#english) · [Русский](#русский)

Отдельная заметка о том, что делает пул, когда кольцо на 4096 задач уже полное. Коротко те же вызовы описаны в [README](README.md). Очередь сама по себе остаётся неблокирующей: это разобрано в [QUEUE.md](QUEUE.md).

## English

### What was wrong

`push_until_accepted` lived in `src/pool.rs`. `execute` and `execute_on` both called it after they had picked a queue.

`Queue::push` returns the task when the ring is full. The helper put that task back and called `thread::yield_now()`, then tried the same queue again. The loop had no deadline.

A full ring holds 4096 tasks. While it stayed full, the calling thread kept coming back to the same push. `yield_now` hands the core to the scheduler and then the thread is runnable again. A worker stuck inside one task never pops, so the caller never returned, and the full queue was not an error the program could handle.

### The calls

`try_execute` and `try_execute_on` make one pass. If every queue they are allowed to use rejects the task, they return `PoolError::QueueFull` and do not sleep.

`execute` and `execute_on` make the same pass, then sleep until a slot appears or `Config::submit_timeout` runs out. The default is one second. The field is public, so a `Config { .. }` literal names it. A timeout of zero does not sleep: the first pass that finds no slot returns `QueueFull`. After a wake, and after the deadline, the call tries `push` once more. If the ring is still full and the deadline has passed, the result is `QueueFull`.

The closure moves into the call before the first `push`. On `QueueFull` it is dropped, and the task never runs. The error does not hand the closure back.

`execute_on` and `try_execute_on` still return `PoolError::UnknownCore` when the pool has no worker for that `CoreId`. The check happens before any push. A core that does exist, with a full pinned queue, is `QueueFull`.

`execute` walks shared queues. `ordered_targets` copies `worker_load` once and sorts by that snapshot, shortest first. Copying matters: a pop during the sort would change the lengths under the comparison, and the ordering would stop being an order. Equal lengths rotate with the pool's round-robin counter, so a burst does not stick to worker 0. The snapshot is only the order of the attempt. `Queue::push` is what accepts or rejects the task. If the shortest queue is full, the same pass tries the others, so a race between the choice and the push still lands on a neighbour that has room.

`execute_on` keeps only the workers of the given physical core. In `Layout::LogicalCpus` a core has two. Both pinned queues are tried, least loaded first. The wait uses that core's pinned gate. A pop on another core, and a pop of a shared queue, does not wake this caller.

### Why the wait is a futex

A deadline wrapped around `yield_now` would still run the caller until the clock ran out. A `Mutex` and `Condvar` taken on every `pop` would drag a line the producer writes onto the worker's pop path. The queue already keeps the write position and the read position on separate cache lines so that a push does not invalidate the line the worker is reading. The wait stays out of `Queue`, in [`src/gate.rs`](src/gate.rs). The loom model of `len` does not change.

Each gate has two counters, and each counter sits on its own cache line:

- `waiters` counts producers asleep on this gate. While it is zero, `note_slot_freed` is one acquire load. The worker does not write the line, and it does not call into the kernel.
- `generation` moves only when `waiters` is not zero. The increment is release. `FUTEX_WAKE_BITSET` then wakes one sleeper.

Two gates, because the two kinds of queue are not interchangeable. `shared_gate` covers every shared queue, including a slot freed by `steal`: any blocked `execute` can take any shared slot. Each physical core has its own pinned gate, shared by the SMT siblings of that core. `execute_on` waits on that gate alone.

`note_slot_freed` runs after a successful pop in `run_local_batch` and in `steal`. `Queue::drop` still drains leftover tasks by itself and does not notify. `Pool::drop` holds `&mut Pool`, so a submit that borrows `&Pool` is already finished before those leftovers are dropped.

### The sleep protocol

`FUTEX_WAIT_BITSET` sleeps while the word still equals the value the caller sampled. A pop that changes the word in between is `EAGAIN`, and the wait returns at once. `Pool::submit` uses that as follows:

1. Try `push` across the candidate queues. On success, unpark that worker and return a `JobHandle`.
2. A `try_*` call returns `QueueFull` here.
3. If the deadline has passed, return `QueueFull`.
4. `register` increments `waiters` with release. The registration's `Drop` decrements it, including when a later `push` succeeds.
5. Load `generation` with acquire.
6. Try `push` again. A slot freed before this load is taken here. A pop after the load changes `generation`.
7. Sleep on the sampled generation for the time still left. `EINTR` restarts with the remainder. The timeout is an absolute `CLOCK_MONOTONIC` value (`FUTEX_WAIT_BITSET` without `FUTEX_CLOCK_REALTIME`), so a step of `CLOCK_REALTIME` does not stretch the wait. `FUTEX_PRIVATE_FLAG` keeps the futex inside the process. An unexpected errno sleeps out the remainder and returns, so a broken futex does not spin until the deadline.
8. Drop the registration and go back to step 1. That next `push` is the extra attempt after a wake or a timeout.

One freed slot wakes one sleeper. The woken thread walks its candidate queues again, so the free slot does not have to be the queue it sampled first. Two blocked `execute` calls can take each other's slot. Two blocked `execute_on` calls for different cores cannot, which is why they do not share a gate.

### What the tests check

The pool tests park every worker the same way `shared_task_runs_within_pinned_quota` does. Each worker gets one pinned task. The task signals that it has entered, then blocks on a channel. The test waits for every signal, so each worker is inside that task and is not popping. A guard sends the release in `Drop`, and a failed assertion still lets `Pool::drop` return.

The filler uses `submit` with waiting turned off and pushes exactly `QUEUE_CAPACITY` tasks (4096, already a power of two). The running barrier task has already been popped, so the ring starts empty and the last of those pushes fills it.

`try_execute_returns_queue_full_without_waiting` fills every shared queue. The pool's `submit_timeout` is 5 seconds. `try_execute` returns `QueueFull` in under 200 ms.

`execute_returns_queue_full_when_the_timeout_expires` sets `submit_timeout` to 80 ms on the same full pool. `execute` returns `QueueFull`. The elapsed time reaches that timeout, within a few milliseconds, and stays under 2 seconds.

`execute_proceeds_when_a_slot_frees` fills the shared queues and starts `execute` on another thread with a 2 second timeout. It spins until `shared_gate` reports a waiter, then releases one worker. That worker pops, `note_slot_freed` wakes the caller, the task is pushed into the freed slot, and `wait` returns 7.

`try_execute_on_distinguishes_unknown_core_from_full_queue` expects `UnknownCore` for `CoreId(u32::MAX)` before any queue is full. It then fills the pinned queues of `cores()[0]` and expects `QueueFull`.

The gate tests do not start a pool. `waiters_and_generation_do_not_share_a_cache_line` checks that the two counters are at least 64 bytes apart. `wait_returns_when_the_timeout_expires` waits 50 ms on a gate that nobody wakes. `note_slot_freed_reaches_a_waiter` stores a flag and bumps `generation`. The waiter either observes the flag after it has sampled `generation`, or the futex returns because the word changed.

### How to repeat

```bash
cargo test --lib try_execute_returns_queue_full_without_waiting
cargo test --lib execute_returns_queue_full_when_the_timeout_expires
cargo test --lib execute_proceeds_when_a_slot_frees
cargo test --lib try_execute_on_distinguishes_unknown_core_from_full_queue
cargo test --lib note_slot_freed_reaches_a_waiter
```

The rest of the library tests run with `cargo test`.

## Русский

### Что было не так

`push_until_accepted` стоял в `src/pool.rs`. Через него проходили и `execute`, и `execute_on`, уже после выбора очереди.

`Queue::push` возвращает задачу, когда кольцо полное. Помощник клал её обратно и звал `thread::yield_now()`, затем пробовал ту же очередь снова. У цикла не было срока.

Полное кольцо — это 4096 задач. Пока оно оставалось полным, вызывающий поток снова и снова приходил в тот же `push`. `yield_now` отдаёт ядро планировщику, после чего поток опять готов к работе. Рабочий поток, застрявший внутри одной задачи, ничего не забирает, поэтому вызывающий не возвращался, и полную очередь нельзя было обработать как ошибку.

### Вызовы

`try_execute` и `try_execute_on` делают один проход. Если все очереди, куда им можно положить задачу, её отвергают, они возвращают `PoolError::QueueFull` и не спят.

`execute` и `execute_on` делают тот же проход, затем спят, пока не появится слот или не выйдет `Config::submit_timeout`. По умолчанию одна секунда. Поле публичное, поэтому литерал `Config { .. }` называет его тоже. Нулевой срок не спит: первый проход без слота возвращает `QueueFull`. После пробуждения и после срока вызов ещё раз пробует `push`. Если кольцо всё ещё полное и срок вышел, результат — `QueueFull`.

Замыкание переходит в вызов до первого `push`. При `QueueFull` оно уничтожается, и задача не запускается. Ошибка не возвращает замыкание обратно.

`execute_on` и `try_execute_on` по-прежнему возвращают `PoolError::UnknownCore`, если в пуле нет рабочего потока с таким `CoreId`. Проверка стоит до любого `push`. Ядро, которое есть, но с полной закреплённой очередью, — это `QueueFull`.

`execute` обходит общие очереди. `ordered_targets` один раз копирует `worker_load` и сортирует по этому снимку, сначала самая короткая. Копия нужна: pop во время сортировки менял бы длины под сравнением, и оно перестало бы быть порядком. При равной длине начало сдвигается тем же круговым счётчиком пула, поэтому пачка задач не липнет к worker 0. Снимок задаёт только порядок попыток. Принимает или отвергает задачу сам `Queue::push`. Если самая короткая очередь полна, тот же проход пробует остальные: гонка между выбором и `push` всё равно попадает в соседа, у которого место есть.

`execute_on` оставляет только рабочие потоки указанного физического ядра. В `Layout::LogicalCpus` их два. Обе закреплённые очереди пробуются, сначала у менее загруженного. Ожидание идёт на закреплённом gate этого ядра. Pop на другом ядре и pop общей очереди этого вызывающего не будят.

### Почему ожидание — это futex

Срок вокруг `yield_now` всё равно крутил бы вызывающий поток, пока не кончатся часы. `Mutex` и `Condvar` на каждом `pop` тащили бы на путь рабочего потока линию, в которую пишет производитель. Очередь и так держит позицию записи и позицию чтения на разных кэш-линиях, чтобы push не инвалидировал линию, которую worker в этот момент читает. Ожидание живёт снаружи `Queue`, в [`src/gate.rs`](src/gate.rs). Loom-модель `len` от этого не меняется.

У каждого gate два счётчика, и каждый стоит на своей кэш-линии:

- `waiters` — сколько производителей спят на этом gate. Пока там ноль, `note_slot_freed` — это одно acquire-чтение. Worker линию не пишет и в ядро не ходит.
- `generation` двигается только когда `waiters` не ноль. Приращение — release. Затем `FUTEX_WAKE_BITSET` будит одного спящего.

Gate два, потому что два вида очередей не взаимозаменяемы. `shared_gate` покрывает все общие очереди, включая слот, освобождённый `steal`: любой заблокированный `execute` может взять любой общий слот. У каждого физического ядра свой закреплённый gate, общий для SMT-соседей этого ядра. `execute_on` ждёт только на нём.

`note_slot_freed` вызывается после успешного pop в `run_local_batch` и в `steal`. `Queue::drop` по-прежнему сам добирает оставшиеся задачи и никого не будит. `Pool::drop` держит `&mut Pool`, поэтому постановка, которая занимает `&Pool`, к этому моменту уже закончилась.

### Протокол сна

`FUTEX_WAIT_BITSET` спит, пока слово равно тому значению, которое вызывающий успел прочитать. Pop, который меняет слово в промежутке, — это `EAGAIN`, и ожидание сразу возвращается. `Pool::submit` пользуется этим так:

1. Пробовать `push` по очередям-кандидатам. При успехе разбудить этот worker и вернуть `JobHandle`.
2. Вызов `try_*` здесь возвращает `QueueFull`.
3. Если срок вышел, вернуть `QueueFull`.
4. `register` увеличивает `waiters` с release. `Drop` регистрации уменьшает его, в том числе если следующий `push` прошёл.
5. Прочитать `generation` с acquire.
6. Ещё раз пробовать `push`. Слот, освобождённый до этого чтения, забирается здесь. Pop после чтения меняет `generation`.
7. Спать на прочитанном `generation` оставшееся время. `EINTR` запускает ожидание заново с остатком срока. Таймаут — абсолютное значение `CLOCK_MONOTONIC` (`FUTEX_WAIT_BITSET` без `FUTEX_CLOCK_REALTIME`), поэтому шаг `CLOCK_REALTIME` срок не растягивает. `FUTEX_PRIVATE_FLAG` оставляет futex внутри процесса. Неизвестный errno досыпает остаток и возвращается: сломанный futex не крутит поток до конца срока.
8. Сбросить регистрацию и вернуться к шагу 1. Этот следующий `push` и есть повторная попытка после пробуждения или таймаута.

Один освобождённый слот будит одного спящего. Проснувшийся поток заново обходит свои очереди, поэтому свободный слот не обязан быть той очередью, на которую он смотрел первой. Два заблокированных `execute` могут забрать слот друг друга. Два заблокированных `execute_on` на разные ядра — нет, поэтому gate у них разные.

### Что проверяют тесты

Тесты пула останавливают каждый worker так же, как `shared_task_runs_within_pinned_quota`. Каждый поток получает одну закреплённую задачу. Она сообщает, что вошла, и блокируется на канале. Тест ждёт все сигналы, так что каждый поток сидит внутри этой задачи и ничего не забирает. Guard шлёт отпускание в `Drop`: упавший assert всё равно даёт `Pool::drop` вернуться.

Заполнение идёт через `submit` без ожидания и кладёт ровно `QUEUE_CAPACITY` задач (4096, уже степень двойки). Задача-шлагбаум к этому моменту уже снята с кольца, поэтому кольцо начинается пустым, и последний из этих `push` его заполняет.

`try_execute_returns_queue_full_without_waiting` заполняет все общие очереди. `submit_timeout` пула — 5 секунд. `try_execute` возвращает `QueueFull` быстрее чем за 200 мс.

`execute_returns_queue_full_when_the_timeout_expires` ставит `submit_timeout` в 80 мс на таком же полном пуле. `execute` возвращает `QueueFull`. Прошедшее время доходит до этого срока, с допуском в несколько миллисекунд, и остаётся меньше 2 секунд.

`execute_proceeds_when_a_slot_frees` заполняет общие очереди и запускает `execute` в другом потоке со сроком 2 секунды. Он ждёт, пока `shared_gate` не покажет ожидающего, и отпускает один worker. Тот делает pop, `note_slot_freed` будит вызывающего, задача попадает в освободившийся слот, и `wait` возвращает 7.

`try_execute_on_distinguishes_unknown_core_from_full_queue` ждёт `UnknownCore` для `CoreId(u32::MAX)`, пока ни одна очередь ещё не полна. Затем заполняет закреплённые очереди `cores()[0]` и ждёт `QueueFull`.

Тесты gate пул не поднимают. `waiters_and_generation_do_not_share_a_cache_line` проверяет, что два счётчика разнесены хотя бы на 64 байта. `wait_returns_when_the_timeout_expires` ждёт 50 мс на gate, который никто не будит. `note_slot_freed_reaches_a_waiter` записывает флаг и двигает `generation`. Ожидающий либо видит флаг после того, как прочитал `generation`, либо futex возвращается, потому что слово изменилось.

### Как повторить

```bash
cargo test --lib try_execute_returns_queue_full_without_waiting
cargo test --lib execute_returns_queue_full_when_the_timeout_expires
cargo test --lib execute_proceeds_when_a_slot_frees
cargo test --lib try_execute_on_distinguishes_unknown_core_from_full_queue
cargo test --lib note_slot_freed_reaches_a_waiter
```

Остальные тесты библиотеки запускаются через `cargo test`.
