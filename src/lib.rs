//! Пул потоков, который не размазывает работу по ядрам.
//!
//! L1 и L2 принадлежат физическому ядру. Перенос потока на другое ядро
//! оставляет эти кэши холодными: линии никто не обнуляет, их вытесняет
//! чужой working set. [`Pool`] держит по одному закреплённому worker на
//! ядро и выполняет локальные задачи пачками.
//!
//! # Example
//!
//! ```
//! let pool = warm_core::Pool::new().expect("пул");
//! let handle = pool.execute(|| 2 + 2);
//! assert_eq!(handle.wait(), 4);
//! ```

mod affinity;
mod pool;
mod queue;
mod topology;

pub use pool::{Config, JobHandle, Layout, Pool, PoolError};
pub use topology::{CacheInfo, CoreId, Cpu, Topology};
