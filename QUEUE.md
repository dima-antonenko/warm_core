# Длина очереди

[English](#english) · [Русский](#русский)

Отдельная заметка о счётчике `len` в [`src/queue.rs`](src/queue.rs). Как пул выбирает ядро по этой длине, описано в [README](README.md).

## English

### What was wrong

`Queue::push` published a slot and only then incremented `len`.

After the producer won the enqueue CAS it wrote the value and performed two atomic operations:

1. `slot.sequence.store(pos + 1, Release)`. This store publishes the slot. A consumer that loads `sequence` with Acquire can take the value.
2. `len.fetch_add(1, Relaxed)`.

`pop`, after winning its own CAS, reads the value, publishes the slot for the next lap around the ring, and then does `len.fetch_sub(1, Relaxed)`.

Nothing tied that `fetch_sub` to the producer's `fetch_add`. The consumer can decrement `len` while it is still 0. `len` is an `AtomicUsize`, so the subtraction wraps to `usize::MAX` (`18_446_744_073_709_551_615`).

When both calls have returned, the later `fetch_add` wraps the counter forward again and `len` is right. The tests in `cargo test` only read `len` after the queue is empty, so they stayed green. The wrapped value lives in the gap between the two atomic operations. That is the gap in which another worker reads `len` to decide where to put a task or where to steal one.

### Why the pool cares

Full and empty are decided by `sequence`. `len` is the approximate occupancy the scheduler reads.

`worker_load` adds the pinned queue and the shared queue with `saturating_add`. One wrapped counter saturates the sum at `usize::MAX`.

`execute` calls `pick_least_loaded`, which keeps the cores with the smallest sum and breaks a tie round-robin. A core sitting at `usize::MAX` drops out of that set, so new tasks go to other cores while this one may already be empty.

`steal` pops from the neighbour whose shared queue reports the largest `len`. `usize::MAX` wins that comparison. An idle worker walks up to a queue that can already be empty, gets nothing, and only then tries the rest.

Both reads are `Relaxed`. A slightly stale length is expected. A wrapped length is a different core from the one that actually has the work.

### The fix

`push` increments `len` before the release store that publishes the slot:

1. the value is written into the slot;
2. `len.fetch_add(1, Relaxed)`;
3. `slot.sequence.store(pos + 1, Release)`.

The consumer observes the item with an Acquire load of that same `sequence`. The Acquire synchronizes with the Release store, so every write the producer performed before the store — including the relaxed `fetch_add` — happens-before the consumer proceeds. `fetch_sub` in `pop` stands after that Acquire, so it follows the matching `fetch_add`. While one item is in flight, `len` is 0 or 1. It does not wrap.

The Release store already publishes the earlier `fetch_add` from the same thread, so `fetch_add` stays `Relaxed`.

`pop` still decrements `len` after it has published the slot for reuse. A new `push` into that slot can increment `len` before the old decrement lands. For that moment the counter is one higher than the number of items still queued, and the core looks a little busier than it is. The counter stays in range. That is the safe side of an approximate length.

### What Loom checked

`queue::loom_tests::len_does_not_underflow_on_handoff` explores the handoff. Plain `cargo test` does not run it. A build with `--cfg loom` compiles the queue against Loom's `AtomicUsize`, and Loom tries the interleavings of three threads:

- one thread pushes a single value;
- one thread pops until it receives that value;
- one thread reads `len` once.

One read is enough. Loom places it between the atomic operations of `push` and `pop`, including the old gap after the slot was published and before `fetch_add`.

On the old order the test failed with `len == 18446744073709551615`. After the reorder the same exploration passes: the sampled length is 0 or 1, and after both threads join it is 0.

The twelve library tests still pass under `cargo test`. They use the ordinary atomics. Loom is declared under `[target.'cfg(loom)'.dependencies]`, so a normal build of the library still depends only on `libc`.

### How to repeat

```bash
RUSTFLAGS="--cfg loom" cargo test --lib --release len_does_not_underflow -- --test-threads=1
```

`--test-threads=1` keeps another test from running on top of the simulation. `--release` only shortens the run. The model is small and finishes in a fraction of a second.

## Русский

### Что было не так

`Queue::push` публиковал слот и только потом увеличивал `len`.

Производитель, выиграв CAS на запись, клал значение и делал две атомарные операции:

1. `slot.sequence.store(pos + 1, Release)`. Этот store публикует слот. Потребитель, прочитавший `sequence` через Acquire, уже может забрать значение.
2. `len.fetch_add(1, Relaxed)`.

`pop`, выиграв свой CAS, читает значение, публикует слот для следующего круга по кольцу и затем делает `len.fetch_sub(1, Relaxed)`.

Этот `fetch_sub` ничем не был связан с `fetch_add` производителя. Потребитель может уменьшить `len`, пока там ещё 0. `len` — это `AtomicUsize`, поэтому вычитание оборачивается в `usize::MAX` (`18_446_744_073_709_551_615`).

Когда оба вызова уже вернулись, запоздавший `fetch_add` прокручивает счётчик дальше, и `len` снова верный. Тесты из `cargo test` читают `len` уже у пустой очереди, поэтому оставались зелёными. Обернувшееся значение живёт в промежутке между двумя атомарными операциями. Именно в этом промежутке другой рабочий поток читает `len`, чтобы решить, куда положить задачу или откуда её забрать.

### Почему это важно пулу

Полная очередь и пустая определяются по `sequence`. `len` — приблизительная заполненность, которую читает планировщик.

`worker_load` складывает закреплённую и общую очереди через `saturating_add`. Один обернувшийся счётчик насыщает сумму до `usize::MAX`.

`execute` вызывает `pick_least_loaded`: остаются ядра с наименьшей суммой, а при равенстве выбор идёт по кругу. Ядро со значением `usize::MAX` из этого набора выпадает, и новые задачи уходят на другие ядра, хотя это уже может быть пустым.

`steal` забирает задачу у соседа, чья общая очередь сообщила наибольший `len`. `usize::MAX` выигрывает это сравнение. Свободный поток идёт к очереди, которая уже может быть пустой, ничего не получает и только потом пробует остальные.

Оба чтения — `Relaxed`. Чуть устаревшая длина ожидаема. Обернувшаяся длина указывает на другое ядро, не на то, где работа действительно лежит.

### Правка

`push` увеличивает `len` до release-записи, которая публикует слот:

1. значение записывается в слот;
2. `len.fetch_add(1, Relaxed)`;
3. `slot.sequence.store(pos + 1, Release)`.

Потребитель видит элемент через Acquire-чтение того же `sequence`. Acquire синхронизируется с Release-записью, поэтому всё, что производитель сделал до неё, включая расслабленный `fetch_add`, происходит раньше, чем потребитель идёт дальше. `fetch_sub` в `pop` стоит после этого Acquire и поэтому следует за парным `fetch_add`. Пока в полёте один элемент, `len` равен 0 или 1 и не оборачивается.

Release-запись и так публикует более ранний `fetch_add` того же потока, поэтому `fetch_add` остаётся `Relaxed`.

`pop` по-прежнему уменьшает `len` уже после того, как слот отдан под повторное использование. Новый `push` в этот слот может увеличить `len` раньше, чем сядет старое уменьшение. На этот миг счётчик на единицу больше числа элементов в очереди, и ядро выглядит чуть более занятым, чем оно есть. Счётчик остаётся в диапазоне. Для приблизительной длины это безопасная сторона.

### Что проверил Loom

`queue::loom_tests::len_does_not_underflow_on_handoff` перебирает передачу элемента. Обычный `cargo test` его не запускает. Сборка с `--cfg loom` компилирует очередь на `AtomicUsize` из Loom, и Loom перебирает чередования трёх потоков:

- один поток кладёт одно значение;
- один поток забирает его из очереди;
- один поток один раз читает `len`.

Одного чтения достаточно. Loom ставит его между атомарными операциями `push` и `pop`, в том числе в старый промежуток после публикации слота и до `fetch_add`.

На старом порядке тест упал с `len == 18446744073709551615`. После перестановки тот же перебор проходит: увиденная длина — 0 или 1, а после завершения обоих потоков она равна 0.

Двенадцать тестов библиотеки по-прежнему проходят через `cargo test`. Они работают на обычных атомиках. Loom объявлен в `[target.'cfg(loom)'.dependencies]`, поэтому обычная сборка библиотеки по-прежнему зависит только от `libc`.

### Как повторить

```bash
RUSTFLAGS="--cfg loom" cargo test --lib --release len_does_not_underflow -- --test-threads=1
```

`--test-threads=1` не даёт другому тесту идти поверх этой симуляции. `--release` только сокращает прогон. Модель маленькая и заканчивается за долю секунды.
