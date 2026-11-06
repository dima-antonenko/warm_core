# Квота локальной пачки

[English](#english) · [Русский](#русский)

Отдельная заметка о том, как [`run_local_batch`](src/pool.rs) делит две очереди одного ядра. Цикл рабочего потока целиком описан в [README](README.md).

## English

### What was wrong

Every slot of a local batch did `pinned.pop().or_else(|| shared.pop())`.

`execute_on` pushes onto `pinned`. `execute` pushes onto `shared`. The batch is 32 tasks by default, and for the whole batch the worker does not look at its neighbours. Every slot preferred `pinned`. While something kept writing to that queue, the shared tasks already sitting on the same worker waited.

`steal` does not move them once every worker has local work. A worker steals only after both of its own queues are empty, and it only pops a neighbour's shared queue. Pinned tasks stay on the core they were sent to: that is where their data is hot in L1 and L2. A shared task on a core busy with `execute_on` waits until that stream stops, if the other cores are busy too.

### The fix

`Config::pinned_per_shared` is how many pinned tasks run in a row before the shared queue gets one slot. The default is 3. `Pool::with_config` raises a value below 1 up to 1, the same way it treats `batch`. A quota of 1 alternates the two queues. The field is public, so a `Config { .. }` literal written outside the crate names it too.

The streak is a local in `worker_loop`. Both calls of `run_local_batch`, including the drain during shutdown, share that counter. A batch boundary does not clear it. Clearing it there would bring the starvation back for `batch == 1` and a quota of 3: the single slot of each batch would be pinned, and the next batch would start the streak from zero.

Each slot goes like this:

1. The streak is below the quota. Pop `pinned` and add one. If `pinned` is empty, pop `shared` and set the streak to 0, so the next run of pinned tasks receives a full quota.
2. The streak has reached the quota. Pop `shared` and set the streak to 0. If `shared` is empty, pop `pinned` and leave the streak as it is. The slot is still used, and the shared queue is still owed one.
3. Both queues are empty. The batch ends. `steal` then runs as before and still only touches other cores' shared queues.

### What the test checks

`pool::tests::shared_task_runs_within_pinned_quota` builds a pool with `batch: 8` and `pinned_per_shared: 3`.

Every worker receives one pinned task. The task signals that it has started and then blocks. The test waits for every signal, so each worker is inside that task and is not stealing.

It then pushes 64 pinned tasks and one shared task onto worker 0, and releases only that worker. The other release channels live in a guard that sends on `Drop`, so a failed assertion still lets `Pool::drop` return.

The shared task reads how many of those 64 pinned tasks had already started. The test requires that number to be at most 3.

The gate task is itself pinned, so it spends one credit of the streak before the backlog is queued. The number the shared task sees can be smaller than 3. Under the old `or_else` order the same backlog ran all 64 pinned tasks first.

### How to repeat

```bash
cargo test --lib shared_task_runs_within_pinned_quota
```

The rest of the library tests run with `cargo test`.

## Русский

### Что было не так

Каждый слот локальной пачки делал `pinned.pop().or_else(|| shared.pop())`.

`execute_on` кладёт задачу в `pinned`. `execute` кладёт её в `shared`. Пачка по умолчанию — 32 задачи, и на всё это время рабочий поток не смотрит на соседей. Каждый слот предпочитал `pinned`. Пока в эту очередь кто-то писал, общие задачи, уже лежащие на том же рабочем потоке, ждали.

`steal` их не уносит, когда локальная работа есть у каждого потока. Поток крадёт только после того, как обе его очереди пусты, и забирает задачу только из общей очереди соседа. Закреплённые задачи остаются на том ядре, куда их отправили: там их данные горячие в L1 и L2. Общая задача на ядре, занятом `execute_on`, ждёт, пока этот поток задач кончится, если остальные ядра тоже заняты.

### Правка

`Config::pinned_per_shared` — сколько закреплённых задач идёт подряд, прежде чем общая очередь получит один слот. По умолчанию 3. `Pool::with_config` поднимает значение меньше единицы до 1, так же как `batch`. Квота 1 чередует очереди. Поле публичное, поэтому литерал `Config { .. }` снаружи крейта называет его тоже.

Серия — локальная переменная `worker_loop`. Оба вызова `run_local_batch`, включая добор очередей при остановке, видят один и тот же счётчик. Граница пачки его не обнуляет. Обнуление в этом месте вернуло бы голодание при `batch == 1` и квоте 3: единственный слот каждой пачки доставался бы закреплённой очереди, а следующая пачка начинала бы серию с нуля.

Дальше каждый слот идёт так:

1. Серия меньше квоты. Забрать задачу из `pinned` и прибавить единицу. Если `pinned` пуста, забрать из `shared` и обнулить серию, чтобы следующая пачка закреплённых снова получила полную квоту.
2. Серия достигла квоты. Забрать задачу из `shared` и обнулить серию. Если `shared` пуста, забрать из `pinned` и серию не трогать. Слот всё равно занят, а долг перед общей очередью остаётся.
3. Обе очереди пусты. Пачка заканчивается. Дальше, как и раньше, работает `steal`, и он по-прежнему трогает только общие очереди чужих ядер.

### Что проверяет тест

`pool::tests::shared_task_runs_within_pinned_quota` собирает пул с `batch: 8` и `pinned_per_shared: 3`.

Каждый рабочий поток получает одну закреплённую задачу. Она сообщает, что стартовала, и блокируется. Тест ждёт все сигналы, так что каждый поток сидит внутри этой задачи и не крадёт чужую работу.

Затем на worker 0 кладётся 64 закреплённые задачи и одна общая, и отпускается только он. Остальные каналы отпускания лежат в guard, который шлёт сигнал в `Drop`: упавший assert всё равно даёт `Pool::drop` вернуться.

Общая задача читает, сколько из этих 64 закреплённых уже стартовали. Тест требует, чтобы это число было не больше 3.

Задача-шлагбаум сама закреплена, поэтому она тратит один кредит серии ещё до того, как в очередь попадает залеж. Число, которое увидит общая задача, может быть меньше 3. При старом порядке `or_else` тот же залеж сначала выполнял все 64 закреплённые задачи.

### Как повторить

```bash
cargo test --lib shared_task_runs_within_pinned_quota
```

Остальные тесты библиотеки запускаются через `cargo test`.
