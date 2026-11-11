//! lse-core：索引引擎核心（遍历 / SQLite 持久化 / 内存查询 / inotify 增量）。
//! 本 crate **不包含 GUI**，也不在查询路径上触碰文件系统。

pub mod build;
pub mod config;
pub mod db;
pub mod index;
pub mod scan;
pub mod watch;
pub mod wire;

pub use config::{Config, ExcludeRules};
pub use index::{Hit, Index, KindFilter, Op, Query};
pub use scan::{Progress, ScanStats, Scanner};
pub use watch::{WatchManager, WatchStatus};
