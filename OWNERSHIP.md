# Владение состоянием ядра

[English](#english) · [Русский](#русский)

Отдельная заметка о том, чем шард отличается от закреплённой задачи. Коротко тот же шаг описан в [README](README.md).

## English

### What a pin does not own

`execute_on` puts a closure on one core, and an idle neighbour does not steal it. The value the closure closes over is still the caller's. In `examples/stateful.rs` the buffer is built outside, moved into the task, and sent back through the channel. The next `execute` may run those same bytes on another core. Pinning the worker does not stop that: the scheduler kept the task, it did not keep the state.

A Seastar shard is the other way around. The value lives on one core. Another core never receives `&T`. It sends work, and the owner runs that work next to the bytes it already has.

`warm_core` takes that step and stops there. There is no reactor, no futures, and no claim that `execute` goes away. Stateless tasks still use the two queues they used before.

### The shard

[`Pool::shard_on`](src/shard.rs) picks the only worker of a [`CoreId`](src/topology.rs) and runs `init` on that thread. The `Box` around the value is allocated there, so the first write of those pages happens on that CPU's NUMA node. Pages the caller already touched stay where they were: `init` does not migrate them. This is the same rule as [`LOCALITY.md`](LOCALITY.md).

`T` does not have to be `Send`. The handle is an `Arc` that other threads may clone. The bytes stay in the slot. What crosses a core is the `Send` value `call` returns, or the closure `defer` carries.

Two workers on one core — [`Layout::LogicalCpus`](src/pool.rs), when the hyper-thread sibling is in the cpuset — cannot share one `&mut T`. `shard_on` returns `PoolError::AmbiguousCore`. A missing core is still `UnknownCore`.

### `call` and `defer`

Both land in the pinned queue. [`steal`](src/pool.rs) never pops that queue, so a neighbour cannot run the closure, and it cannot see the value either.

`Shard::call` from any other thread pushes the closure and waits, bounded by `Config::submit_timeout`, the same bound as `execute_on`. A full queue is `PoolError::QueueFull`, and the closure is dropped. On the owner thread `call` runs immediately. The owner is the thread that has to pop the queue. If it blocked inside `call` waiting for a task that only it can run, it would wait for itself.

A second `call` on the same shard, while the outer one still holds `&mut T`, returns `PoolError::Reentrant`. The borrow checker does not see the alias: both closures are separate `FnOnce`s. The depth counter on the owner does. After the outer `call` returns, the shard takes another one. A panic inside `call` is resumed at the caller, as with `JobHandle::wait`. The worker stays up, and the value stays in the slot.

`Shard::defer` pushes the closure and returns once the slot is taken. It does not wait for the body. That is how one core hands work to another without stalling its own loop. The reply is a `defer` back, not a `call`. From the owner thread a full pinned queue is an immediate `QueueFull`: that thread cannot sleep for a slot in the queue it alone drains. A panic inside `defer` has no caller waiting for it. The worker catches it and takes the next task.

`defer` from inside `call` only enqueues. The new closure runs after the current one returns, so it does not alias the `&mut T` the outer call still holds.

After `Pool` starts shutting down, `call` and `defer` return `PoolError::Stopped` instead of pushing into a queue nobody will finish. A handle whose value the worker has already dropped gets the same error.

### Two cores waiting on each other

`call` from a worker onto another worker blocks the caller until the target runs the closure. If that closure `call`s back onto the first worker, the first worker is asleep inside its own task and will not pop the reply. Both sides wait. `defer` in both directions does not take this lock: each closure is queued, and each owner runs it when it is next free. The waiting that remains is the caller's, outside the pool, on a channel or on `call` from a thread that is not a worker.

### Drop

The value is dropped on the owner thread. Dropping the last handle from that thread does it inline. Dropping it from anywhere else pushes a destroy task into the same pinned queue. If the outer `call` is still on the stack, the destroy waits until that `call` returns: the `&mut T` is still in use.

On pool shutdown the worker drains its queues, then drops every shard still registered to it, on its own CPU, before the thread exits. A handle that outlives the pool finds the slot already empty. `Shard` holds a `Weak` to the pool and does not keep the workers alive.

### What this is not

The owner is a thread in the existing pool, not a reactor. `defer` is a closure on the pinned queue, not a typed message bus and not a future. Nothing here polls I/O. `execute` and `execute_on` are unchanged, and a shared task can still be stolen. Only a shard's value cannot.

## Русский

### Чего привязка не даёт

`execute_on` кладёт замыкание на одно ядро, и простаивающий сосед его не крадёт. Значение, которое замыкание захватило, по-прежнему принадлежит вызывающему. В `examples/stateful.rs` буфер собирается снаружи, переезжает в задачу и возвращается через канал. Следующий `execute` может прочитать те же байты на другом ядре. Привязка worker этого не останавливает: планировщик удержал задачу, а не состояние.

Шард в Seastar устроен наоборот. Значение живёт на одном ядре. Другое ядро не получает `&T`. Оно посылает работу, и владелец выполняет её рядом с байтами, которые у него уже есть.

`warm_core` делает этот шаг и на нём останавливается. Реактора нет, futures нет, и `execute` никуда не девается. Задачи без своего состояния по-прежнему идут в те две очереди, что и раньше.

### Шард

[`Pool::shard_on`](src/shard.rs) берёт единственного worker этого [`CoreId`](src/topology.rs) и выполняет `init` на его потоке. `Box` вокруг значения выделяется там же, поэтому первая запись этих страниц происходит на NUMA-узле этого CPU. Страницы, которые вызывающий уже тронул, остаются где были: `init` их не переносит. То же правило, что в [`LOCALITY.md`](LOCALITY.md).

`T` не обязан быть `Send`. Ручка — это `Arc`, который другие потоки могут клонировать. Байты остаются в слоте. Между ядрами едет только `Send`-значение, которое вернул `call`, или замыкание, которое несёт `defer`.

Два worker на одном ядре — [`Layout::LogicalCpus`](src/pool.rs), когда сосед по hyper-threading входит в cpuset — не могут делить один `&mut T`. `shard_on` возвращает `PoolError::AmbiguousCore`. Нет такого ядра — по-прежнему `UnknownCore`.

### `call` и `defer`

Оба попадают в закреплённую очередь. [`steal`](src/pool.rs) её не читает, поэтому сосед не выполнит замыкание и не увидит значение.

`Shard::call` с чужого потока ставит замыкание и ждёт, не дольше `Config::submit_timeout`, с тем же пределом, что `execute_on`. Полная очередь — `PoolError::QueueFull`, замыкание при этом уничтожается. На потоке-владельце `call` выполняется сразу. Владелец — это поток, который разбирает очередь. Если бы он заблокировался внутри `call` в ожидании задачи, которую может забрать только он сам, он ждал бы сам себя.

Повторный `call` того же шарда, пока внешний ещё держит `&mut T`, возвращает `PoolError::Reentrant`. Проверка заимствований этого алиаса не видит: замыкания — отдельные `FnOnce`. Счётчик глубины на владельце видит. Когда внешний `call` вернулся, шард принимает следующий. Паника внутри `call` возобновляется у вызывающего, как у `JobHandle::wait`. Worker остаётся, значение остаётся в слоте.

`Shard::defer` ставит замыкание и возвращается, как только занят слот. Тело он не ждёт. Так одно ядро передаёт работу другому, не останавливая свой цикл. Ответ — встречный `defer`, а не `call`. С потока-владельца полная закреплённая очередь — это сразу `QueueFull`: этот поток не может спать в ожидании слота очереди, которую разбирает только он. Панику внутри `defer` некому вернуть. Worker её ловит и берёт следующую задачу.

`defer` изнутри `call` только ставит задачу в очередь. Новое замыкание выполнится после того, как текущее вернётся, и не алиасит `&mut T`, который внешний `call` ещё держит.

Когда `Pool` начал остановку, `call` и `defer` возвращают `PoolError::Stopped` и не пишут в очередь, которую уже никто не дочитает. Та же ошибка у ручки, чьё значение worker уже уничтожил.

### Два ядра ждут друг друга

`call` с worker на другой worker блокирует вызывающего, пока адресат не выполнит замыкание. Если то замыкание сделает `call` обратно на первое ядро, первое ядро спит внутри своей задачи и ответ не заберёт. Ждут обе стороны. `defer` в обе стороны этого замка не берёт: каждое замыкание стоит в очереди, и каждый владелец выполняет его, когда освободится. Ожидание, которое остаётся, — у вызывающего снаружи пула, на канале или на `call` с потока, который не является worker.

### Уничтожение

Значение уничтожается на потоке-владельце. Последняя ручка, уроненная на этом потоке, делает это сразу. Ручка, уроненная где угодно ещё, ставит задачу уничтожения в ту же закреплённую очередь. Если внешний `call` ещё на стеке, уничтожение ждёт его возвращения: `&mut T` ещё занят.

При остановке пула worker доедает очереди, затем уничтожает все ещё зарегистрированные у него шарды, на своём CPU, до выхода потока. Ручка, пережившая пул, застаёт пустой слот. `Shard` держит `Weak` на пул и не продлевает жизнь потоков.

### Чем это не является

Владелец — это поток существующего пула, а не реактор. `defer` — замыкание в закреплённой очереди, а не типизированная шина сообщений и не future. Ввод-вывод здесь никто не опрашивает. `execute` и `execute_on` не изменились, и общую задачу по-прежнему можно украсть. Нельзя украсть только значение шарда.
