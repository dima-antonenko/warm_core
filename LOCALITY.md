# Cpuset и узел NUMA

[English](#english) · [Русский](#русский)

Отдельная заметка о том, какие CPU вообще попадают в пул, где после этого оседает память и остаётся ли рабочий набор в кэше ядра, на которое его положили. Коротко те же шаги описаны в [README](README.md).

## English

### What was wrong

`Topology::from_sysfs` listed every logical CPU under `/sys/devices/system/cpu`. A container usually sees the host's sysfs, while the process may run only on a cpuset. `Pool::new` then called `pin_to` on a forbidden CPU, and that worker failed to start.

The calling thread's `sched_getaffinity` mask is the wrong list to filter by. `taskset` narrows it. A thread created afterwards inherits that narrow mask, and `sched_setaffinity` may widen it again up to the cpuset. The pool wants the cpuset, not the mask of whoever called `Pool::new`.

A pin also does not place memory. An anonymous page is filled on the NUMA node of the CPU that faults it. Pages the caller already touched stay on the caller's node after the worker is pinned somewhere else. A remote DRAM read costs more than an L3 hit, which is the distance this pool is trying to avoid.

### Where the cpuset is read

[`src/cpuset.rs`](src/cpuset.rs) reads `/proc/self/cgroup` and `/proc/self/mountinfo`.

A cgroup v2 line looks like `0::/path`. The mount whose type is `cgroup2` supplies the directory. The files are `cpuset.cpus.effective` and `cpuset.mems.effective`. The search starts at the process's cgroup directory and walks up to the mount point. The leaf often has no cpuset file; an ancestor does. The first file that exists is the answer, and an empty file is an empty set, not a signal to keep walking.

A cgroup v1 line lists `cpuset` among its controllers. The mount is a `cgroup` filesystem whose options include `cpuset`. The names tried, in order, are `cpuset.effective_cpus` then `cpuset.cpus`, and `cpuset.effective_mems` then `cpuset.mems`.

When both hierarchies are visible, the two CPU lists are intersected, and so are the two memory-node lists. Lists that fail to parse are ignored. If no file is found, the limit is `None`: every CPU from sysfs stays. `Topology::from_sysfs` itself does not apply this. `Topology::detect` does, through `restrict_to`, and that is what `Pool::new` calls. An empty CPU list after the cut is an error: `в cpuset процесса нет CPU, на которые можно посадить worker`.

### NUMA nodes

`Cpu::numa_node` comes from `/sys/devices/system/node/nodeN/cpulist`, read next to the cpu directory. A machine without that directory leaves every CPU on node 0.

`restrict_to` then drops CPUs whose node is outside `cpuset.mems`, but only when at least one CPU remains. If the memory nodes and the CPU nodes do not overlap, the CPUs stay: the process can still run, and pages will land on whatever node the kernel is allowed to use. Dropping every CPU there would refuse to start the pool. Sibling lists and `shared_cpu_list` on caches are trimmed to the CPUs that remain, so a forbidden hyper-thread is no longer advertised as a sibling.

`physical_workers` still keeps the lowest remaining logical id of each `(package, core)` pair. If the cpuset contains only CPU 4 of the SMT pair `0,4`, the worker for that core is CPU 4.

### Memory policy and `execute`

`worker_entry` pins the thread, then calls `prefer_node`. That is `set_mempolicy` with `MPOL_PREFERRED` and a nodemask of one bit, the worker's `numa_node`. Anonymous faults in that thread after the call prefer this node. The syscall's error is ignored: a sandbox may reject it, and the pin is already in place. Pages faulted earlier, on another thread, are not moved.

`execute` sorts workers by queue length, then by whether the worker's node is the caller's node (`getcpu`), then round-robin inside that group. A shorter queue on a remote node still wins. Equal lengths stay on the caller's node, next to memory that thread already touched. If `getcpu` fails, every worker is treated as local and the order is the old one: shortest queue, then round-robin.

`execute_on` does not look at the node. The caller named the core.

### Cache of one core

A pin keeps the worker on one CPU. It does not keep a shared task on the core that already holds the task's lines. `execute` puts the task on the shortest shared queue. `steal` then pops another worker's shared queue when its own queues are empty, and it never pops a pinned queue. A buffer last scanned on core A is not promised to be scanned there on the next `execute`.

`execute_on` uses the pinned queue. Repeated calls with the same `CoreId` keep the working set on that core.

`examples/stateful.rs` is that workload. One buffer per core, sized to L2, is faulted with `execute_on`. The example then times many rounds of `execute_on` and of `execute`. It prints p50 and p99 of the time inside the task. L1d and last-level read misses come from `perf_event_open` opened on each worker thread, so the counters follow the thread that runs the scan, including a stolen task. A kernel that returns `EPERM` still leaves the latencies. The numbers are this machine's. Nothing in `cargo test` asserts them.

`pool::tests::pinned_task_is_not_stolen_while_a_shared_one_is` blocks every worker, queues a shared task and a pinned task on worker 0, and releases only a neighbour. The neighbour runs the shared task. The pinned task has not run yet.

### What the tests check

`cpuset::tests::effective_cpus_come_from_an_ancestor` builds a fake tree. The leaf cgroup has no cpuset file. `user.slice` above it contains `0-1,4` and memory node `0`. The walk stops at that ancestor.

`cpuset::tests::v1_and_v2_sets_are_intersected` gives v2 the CPUs `0-5` and nodes `0-1`, and v1 the CPUs `1-3` and node `1`. The process is allowed CPUs `1, 2, 3` and node `1`.

`cpuset::tests::host_cgroup_v2_reports_a_cpu_list` runs only when `/proc/self/cgroup` has a `0::` line. `current()` must then return a non-empty CPU list.

`topology::tests::numa_nodes_follow_node_cpulists` builds five CPUs. `node0/cpulist` is `0,1,4` and `node1/cpulist` is `2-3`. CPUs 0 and 4 report node 0, CPUs 2 and 3 report node 1.

`topology::tests::restrict_to_keeps_the_allowed_smt_sibling` allows only CPU 4. `physical_workers` returns that one CPU, on core 0, and the sibling list no longer mentions CPU 0.

`topology::tests::restrict_to_drops_nodes_outside_mems` allows memory node 0 and keeps CPUs `0, 1, 4`.

`topology::tests::restrict_to_keeps_cpus_when_no_node_is_allowed` allows only node 9. The filter would remove every CPU, so all five stay.

`affinity::tests::prefer_node_accepts_the_node_the_thread_is_on` reads the node with `getcpu` and calls `set_mempolicy` on a fresh thread, so the test process's own policy stays as it was.

### How to repeat

```bash
cargo test --lib effective_cpus_come_from_an_ancestor
cargo test --lib v1_and_v2_sets_are_intersected
cargo test --lib numa_nodes_follow_node_cpulists
cargo test --lib restrict_to_keeps_the_allowed_smt_sibling
cargo test --lib restrict_to_drops_nodes_outside_mems
cargo test --lib restrict_to_keeps_cpus_when_no_node_is_allowed
cargo test --lib prefer_node_accepts_the_node_the_thread_is_on
cargo test --lib pinned_task_is_not_stolen_while_a_shared_one_is
cargo run --release --example stateful
```

The rest of the library tests run with `cargo test`. The example prints this machine's p99 and cache misses. It does not fail on a threshold.

## Русский

### Что было не так

`Topology::from_sysfs` перечислял каждый логический CPU из `/sys/devices/system/cpu`. В контейнере sysfs обычно показывает процессор хоста, а исполняться процесс может только на своём cpuset. `Pool::new` затем звал `pin_to` для запрещённого CPU, и этот worker не стартовал.

Маска `sched_getaffinity` вызывающего потока — не тот список, по которому надо резать. `taskset` её сужает. Новый поток наследует узкую маску, а `sched_setaffinity` может снова расширить её до cpuset. Пулу нужен cpuset, а не маска того, кто вызвал `Pool::new`.

Привязка не размещает память. Анонимная страница заполняется на узле NUMA того CPU, который в неё впервые попал. Страницы, которые вызывающий поток уже тронул, остаются на его узле, даже если worker потом привязан в другом месте. Чтение с чужого узла дороже попадания в L3 — как раз ту дистанцию пул и старается не набирать.

### Откуда читается cpuset

[`src/cpuset.rs`](src/cpuset.rs) читает `/proc/self/cgroup` и `/proc/self/mountinfo`.

Строка cgroup v2 выглядит как `0::/path`. Каталог берётся у точки монтирования с типом `cgroup2`. Файлы — `cpuset.cpus.effective` и `cpuset.mems.effective`. Поиск начинается в каталоге группы процесса и идёт вверх до точки монтирования. У листа файла cpuset часто нет, он лежит у предка. Берётся первый существующий файл. Пустой файл — это пустое множество, а не повод идти выше.

Строка cgroup v1 перечисляет контроллер `cpuset`. Точка монтирования — файловая система `cgroup`, в опциях которой есть `cpuset`. Имена по порядку: `cpuset.effective_cpus`, затем `cpuset.cpus`, и пара `cpuset.effective_mems`, `cpuset.mems`.

Когда видны обе иерархии, списки CPU пересекаются, и списки узлов памяти тоже. Список, который не разбирается, пропускается. Если файла нет, ограничение равно `None`: остаются все CPU из sysfs. Сам `Topology::from_sysfs` это не применяет. Применяет `Topology::detect` через `restrict_to`, и именно его зовёт `Pool::new`. Пустой список CPU после отсечения — ошибка: `в cpuset процесса нет CPU, на которые можно посадить worker`.

### Узлы NUMA

`Cpu::numa_node` берётся из `/sys/devices/system/node/nodeN/cpulist`, каталог лежит рядом с каталогом cpu. Если этого каталога нет, каждый CPU остаётся на узле 0.

Дальше `restrict_to` выкидывает CPU, чей узел не входит в `cpuset.mems`, но только когда после этого остаётся хотя бы один CPU. Если узлы памяти и узлы CPU не пересекаются, CPU остаются: процессу всё ещё есть где исполняться, а страницы лягут на тот узел, который ядру разрешён, пусть и удалённый. Выкинуть всех означало бы отказаться запускать пул. Списки соседей и `shared_cpu_list` у кэшей подрезаются до оставшихся CPU, поэтому запрещённый hyper-thread больше не числится соседом.

`physical_workers` по-прежнему оставляет младший из оставшихся логических id пары `(package, core)`. Если в cpuset из пары `0,4` есть только CPU 4, worker этого ядра — CPU 4.

### Политика памяти и `execute`

`worker_entry` сначала привязывает поток, затем зовёт `prefer_node`. Это `set_mempolicy` с `MPOL_PREFERRED` и маской из одного бита — `numa_node` этого worker. Следующие анонимные fault'ы в этом потоке предпочитают этот узел. Ошибка syscall игнорируется: песочница может его запрещать, а привязка к этому моменту уже стоит. Страницы, заполненные раньше и на другом потоке, не переносятся.

`execute` сортирует worker по длине очереди, затем по тому, совпадает ли узел worker с узлом вызывающего (`getcpu`), и внутри этой группы — по кругу. Более короткая очередь на чужом узле всё равно побеждает. При равной длине задача остаётся на узле вызывающего, рядом с памятью, которую тот уже трогал. Если `getcpu` не удался, все worker считаются локальными, и порядок прежний: сначала короткая очередь, потом круг.

`execute_on` на узел не смотрит. Ядро назвал вызывающий.

### Кэш одного ядра

Привязка держит worker на одном CPU. Она не держит общую задачу на том ядре, где уже лежат её строки. `execute` кладёт задачу в самую короткую общую очередь. `steal` забирает чужую общую очередь, когда свои пусты, и закреплённую не читает. Буфер, который в прошлый раз сканировали на ядре A, следующий `execute` не обязан прочитать там же.

`execute_on` использует закреплённую очередь. Повторные вызовы с тем же `CoreId` оставляют рабочий набор на этом ядре.

`examples/stateful.rs` — эта нагрузка. По одному буферу размера L2 на ядро заполняется через `execute_on`. Дальше пример много раз меряет и `execute_on`, и `execute`. Печатаются p50 и p99 времени внутри задачи. Промахи чтения L1d и кэша последнего уровня берутся из `perf_event_open`, открытого на потоке каждого worker, поэтому счётчик следует за потоком, который сканирует буфер, в том числе за украденной задачей. Если ядро вернёт `EPERM`, время всё равно останется. Цифры — этой машины. `cargo test` порог по ним не проверяет.

`pool::tests::pinned_task_is_not_stolen_while_a_shared_one_is` блокирует каждого worker, кладёт общую и закреплённую задачи на worker 0 и отпускает только соседа. Сосед выполняет общую. Закреплённая к этому моменту не выполнялась.

### Что проверяют тесты

`cpuset::tests::effective_cpus_come_from_an_ancestor` строит поддельное дерево. У листовой группы файла cpuset нет. Выше, в `user.slice`, записаны CPU `0-1,4` и узел памяти `0`. Поиск останавливается на этом предке.

`cpuset::tests::v1_and_v2_sets_are_intersected` даёт v2 CPU `0-5` и узлы `0-1`, а v1 — CPU `1-3` и узел `1`. Процессу остаются CPU `1, 2, 3` и узел `1`.

`cpuset::tests::host_cgroup_v2_reports_a_cpu_list` запускается только если в `/proc/self/cgroup` есть строка `0::`. Тогда `current()` обязан вернуть непустой список CPU.

`topology::tests::numa_nodes_follow_node_cpulists` собирает пять CPU. `node0/cpulist` — это `0,1,4`, `node1/cpulist` — `2-3`. CPU 0 и 4 сообщают узел 0, CPU 2 и 3 — узел 1.

`topology::tests::restrict_to_keeps_the_allowed_smt_sibling` разрешает только CPU 4. `physical_workers` возвращает этот один CPU, на ядре 0, и в списке соседей CPU 0 уже нет.

`topology::tests::restrict_to_drops_nodes_outside_mems` разрешает узел памяти 0 и оставляет CPU `0, 1, 4`.

`topology::tests::restrict_to_keeps_cpus_when_no_node_is_allowed` разрешает только узел 9. Фильтр снял бы всех, поэтому все пять CPU остаются.

`affinity::tests::prefer_node_accepts_the_node_the_thread_is_on` читает узел через `getcpu` и зовёт `set_mempolicy` в свежем потоке, так что политика самого тестового процесса не меняется.

### Как повторить

```bash
cargo test --lib effective_cpus_come_from_an_ancestor
cargo test --lib v1_and_v2_sets_are_intersected
cargo test --lib numa_nodes_follow_node_cpulists
cargo test --lib restrict_to_keeps_the_allowed_smt_sibling
cargo test --lib restrict_to_drops_nodes_outside_mems
cargo test --lib restrict_to_keeps_cpus_when_no_node_is_allowed
cargo test --lib prefer_node_accepts_the_node_the_thread_is_on
cargo test --lib pinned_task_is_not_stolen_while_a_shared_one_is
cargo run --release --example stateful
```

Остальные тесты библиотеки запускаются через `cargo test`. Пример печатает p99 и промахи кэша этой машины и не падает из-за порога.
