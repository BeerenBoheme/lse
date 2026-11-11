//! 全量（重）建索引：预统计 → 并行遍历 → SQLite 落库 → 内存索引构建。
//! daemon 与 CLI `--update` 共用此入口。

use crate::db::{self, Rebuilder};
use crate::index::Index;
use crate::scan::{walk_to_channel, Progress, ScanStats, Scanner};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;

pub struct BuildOutcome {
    pub index: Index,
    pub stats: ScanStats,
    pub raw_path_bytes: u64,
    /// 落盘完成时间（unix 秒）
    pub last_scan: i64,
}

/// 全量重建。daemon 场景：调用方在持有 db 写锁期间执行，
/// 事件循环此期间把增量积压到 dirty，完成后由调用方重放。
/// 失败时旧数据保留（未提交的事务回滚）。
pub fn full_build(
    scanner: &Arc<Scanner>,
    db_conn: &Connection,
    progress: &Arc<Progress>,
) -> Result<BuildOutcome, String> {
    // 阶段 1：快速预统计（d_type，无 stat），为进度条给出总数与 ETA
    let pre = scanner.precount(progress);
    eprintln!(
        "[lse] 预统计完成: {} 目录 / {} 文件 ({:?})",
        pre.dirs,
        pre.files,
        Duration::from_millis(pre.elapsed_ms)
    );

    // 阶段 2：正式遍历 + 写库 + 构建新索引
    let (rx, handle) = walk_to_channel(Arc::clone(scanner), Arc::clone(progress));
    let mut new_idx = Index::new();
    let mut raw_path_bytes: u64 = 0;

    let mut reb = Rebuilder::begin(db_conn).map_err(|e| format!("开始重建事务失败: {e}"))?;
    for s in rx {
        raw_path_bytes += s.path.len() as u64;
        new_idx.append(&s.path, s.size, s.mtime, s.is_dir);
        if s.is_dir {
            if reb.insert_dir(&s.path, s.mtime).is_err() {
                continue;
            }
        } else {
            let (dir, name) = db::split_path(&s.path);
            if reb.insert_file(dir, name, s.size, s.mtime).is_err() {
                continue;
            }
        }
    }
    let stats = handle.join().unwrap_or_default();
    let now = db::now_unix();
    reb.finish(now).map_err(|e| format!("索引落盘失败: {e}"))?;

    // 打开频率并入新索引
    if let Ok(opens) = db::load_opens(db_conn) {
        for (p, c) in opens {
            new_idx.set_open(&p, c);
        }
    }
    eprintln!(
        "[lse] 扫描完成: {} 目录 / {} 文件, 耗时 {}ms, 原始路径字节 {}",
        stats.dirs,
        stats.files,
        stats.elapsed_ms,
        raw_path_bytes
    );

    Ok(BuildOutcome {
        index: new_idx,
        stats,
        raw_path_bytes,
        last_scan: now,
    })
}
