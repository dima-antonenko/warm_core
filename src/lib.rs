//! Пул потоков, который не размазывает работу по ядрам.
//!
//! L1 и L2 принадлежат физическому ядру. Перенос потока на другое ядро
//! оставляет эти кэши холодными: линии никто не обнуляет, их вытесняет
//! чужой working set. [`Pool`] держит по одному закреплённому worker на
//! ядро и выполняет локальные задачи пачками. [`Pool::execute_on`] кладёт
//! задачу в очередь, которую не крадут, и повторный вызов остаётся на этом
//! ядре. [`Pool::execute`] кладёт её в общую очередь: простаивающий сосед
//! может забрать задачу, поэтому кэш конкретного ядра этот вызов не обещает.
//! [`Shard`] оставляет на ядре само значение: чужой поток его не видит и
//! посылает туда замыкание.
//!
//! # Example
//!
//! ```
//! let pool = warm_core::Pool::new().expect("пул");
//! let handle = pool.execute(|| 2 + 2).unwrap();
//! assert_eq!(handle.wait(), 4);
//! ```

mod affinity;
mod cpuset;
mod gate;
mod pool;
mod queue;
mod shard;
mod topology;

pub use affinity::pin_to;
pub use pool::{Config, JobHandle, Layout, Pool, PoolError};
pub use shard::Shard;
pub use topology::{CacheInfo, CoreId, Cpu, Topology};
