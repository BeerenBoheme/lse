//! lse-daemon：常驻索引后端。
//! - 冷启动加载 SQLite 索引到内存后即可查询（不遍历文件系统）；
//! - 首次运行 / `Rebuild` 在后台全盘建索引，Status 暴露进度；
//! - inotify 事件批量落库 + 更新内存索引；
//! - D-Bus: `org.lse.Index` / `/org/lse/Index`。

use lse_core::db;
use lse_core::index::{Index, Op, Query};
use lse_core::scan::{Progress, Scanner};
use lse_core::watch::WatchManager;
use lse_core::{Config, ExcludeRules};
use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Shared {
    cfg: Config,
    scanner: Arc<Scanner>,
    idx: Arc<RwLock<Index>>,
    db: Mutex<Connection>,
    progress: Arc<Progress>,
    last_scan: AtomicI64,
    building: AtomicBool,
    dirty: Mutex<Vec<Op>>,
    quit: AtomicBool,
    watch_holder: Mutex<Option<Arc<WatchManager>>>,
}

impl Shared {
    fn db_path(&self) -> String {
        crate_db_path().display().to_string()
    }
}

fn crate_db_path() -> std::path::PathBuf {
    lse_core::config::default_db_path()
}

use lse_core::wire::{HitWire, StatusWire};

struct IndexSvc {
    sh: Arc<Shared>,
    watch: Mutex<Option<Arc<WatchManager>>>,
}

impl IndexSvc {
    fn phase_name(&self) -> String {
        match self.sh.progress.phase.load(Ordering::Relaxed) {
            4 => "loading".into(),
            1 => "counting".into(),
            2 => "scanning".into(),
            3 => "ready".into(),
            _ => "idle".into(),
        }
    }

    fn watch_status_fields(&self) -> (bool, u64, u64, u64, u64, u64, String) {
        match self.watch.lock().unwrap().as_ref() {
            Some(w) if w.is_running() => {
                let s = w.status();
                (
                    s.watching,
                    s.watched_dirs,
                    s.limit,
                    s.budget,
                    s.needed,
                    s.unwatched_dirs,
                    s.warning,
                )
            }
            _ => (false, 0, 0, 0, 0, 0, String::new()),
        }
    }
}

#[zbus::interface(name = "org.lse.Index")]
impl IndexSvc {
    /// 子串搜索（大小写不敏感），仅查内存索引。
    #[zbus(name = "Search")]
    fn search(
        &self,
        text: String,
        limit: u32,
        kind: String,
        exts: String,
        subdir: String,
        regex: String,
    ) -> zbus::fdo::Result<Vec<HitWire>> {
        let q = Query::parse(
            &text,
            limit,
            &kind,
            &exts,
            &subdir,
            &regex,
            self.sh.cfg.query_limit,
        )
        .map_err(zbus::fdo::Error::InvalidArgs)?;
        let idx = self.sh.idx.read().unwrap();
        let hits = idx.search(&q);
        Ok(hits
            .into_iter()
            .map(|h| HitWire {
                path: h.path,
                size: h.size,
                mtime: h.mtime,
                is_dir: h.is_dir,
            })
            .collect())
    }

    #[zbus(name = "Status")]
    fn status(&self) -> StatusWire {
        let (dirs, files) = {
            let g = self.sh.idx.read().unwrap();
            g.counts()
        };
        let (pct, eta) = self.sh.progress.percent_eta();
        let (
            watching,
            watched_dirs,
            watch_limit,
            watch_budget,
            watch_needed,
            unwatched_dirs,
            watch_warning,
        ) = self.watch_status_fields();
        StatusWire {
            version: VERSION.to_string(),
            phase: self.phase_name(),
            dirs,
            files,
            done: self.sh.progress.done.load(Ordering::Relaxed),
            total: self.sh.progress.total_estimate.load(Ordering::Relaxed),
            percent: pct,
            eta_secs: eta,
            last_scan: self.sh.last_scan.load(Ordering::Relaxed),
            building: self.sh.building.load(Ordering::Relaxed),
            watching,
            watched_dirs,
            watch_limit,
            watch_budget,
            watch_needed,
            unwatched_dirs,
            watch_warning,
            db_path: self.sh.db_path(),
            db_bytes: db::db_file_size(&crate_db_path()),
        }
    }

    /// 触发全量重建（后台执行，立即返回）。
    #[zbus(name = "Rebuild")]
    fn rebuild(&self) -> zbus::fdo::Result<bool> {
        let sh = Arc::clone(&self.sh);
        if sh.building.swap(true, Ordering::SeqCst) {
            return Ok(false); // 已在构建中
        }
        std::thread::Builder::new()
            .name("lse-rebuild".into())
            .spawn(move || run_full_scan(sh))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(true)
    }

    /// 记录一次打开（影响排序权重）。
    #[zbus(name = "ReportOpen")]
    fn report_open(&self, path: String) -> zbus::fdo::Result<()> {
        {
            let mut g = self.sh.idx.write().unwrap();
            g.bump_open(&path);
        }
        let db = self.sh.db.lock().unwrap();
        db::bump_open(&db, &path)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    #[zbus(name = "Ping")]
    fn ping(&self) -> String {
        VERSION.to_string()
    }

    /// 停止守护进程（托盘“退出”用；systemd 会按 restart 策略处理）。
    #[zbus(name = "Quit")]
    fn quit(&self) {
        self.sh.quit.store(true, Ordering::SeqCst);
    }
}

/// 全量（重）建索引：后台线程执行；期间查询仍由旧内存索引应答。
fn run_full_scan(sh: Arc<Shared>) {
    let outcome = {
        let db_conn = sh.db.lock().unwrap();
        lse_core::build::full_build(&sh.scanner, &db_conn, &sh.progress)
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[lse] 重建失败: {e}");
            sh.building.store(false, Ordering::SeqCst);
            sh.progress.set_phase(3);
            return;
        }
    };
    sh.last_scan.store(outcome.last_scan, Ordering::Relaxed);
    let new_idx = outcome.index;

    // 换入新索引
    {
        let mut g = sh.idx.write().unwrap();
        *g = new_idx;
    }

    // 重放重建期间积压的增量操作
    let dirty: Vec<Op> = std::mem::take(&mut *sh.dirty.lock().unwrap());
    if !dirty.is_empty() {
        apply_ops(&sh, dirty);
    }

    // 先恢复监听再宣布构建完成（否则 --update 跟踪会在监听重启窗口内提前退出）
    if let Some(w) = sh.watch_for_rebuild() {
        w.restart();
    }

    sh.building.store(false, Ordering::SeqCst);
    sh.progress.set_phase(3);
    sh.progress
        .done
        .store(sh.progress.total_estimate.load(Ordering::Relaxed), Ordering::Relaxed);

}

impl Shared {
    fn watch_for_rebuild(&self) -> Option<Arc<WatchManager>> {
        self.watch_holder
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
    }
}

/// 应用增量操作：先写库，再改内存索引。
fn apply_ops(sh: &Shared, ops: Vec<Op>) {
    if ops.is_empty() {
        return;
    }
    {
        let db = sh.db.lock().unwrap();
        // 单事务批量落库：大批次（子树补扫/退避重扫）从逐条 fsync 变为一次提交
        if let Err(e) = db.execute_batch("BEGIN IMMEDIATE;") {
            eprintln!("[lse] begin 失败: {e}");
        }
        let mut failed = false;
        for op in &ops {
            let r = match op {
                Op::Upsert { path, size, mtime, is_dir } => {
                    db::upsert_entry(&db, path, *size, *mtime, *is_dir)
                }
                Op::Remove { path } => db::remove_entry(&db, path, false),
                Op::RemoveSubtree { prefix } => db::remove_subtree(&db, prefix),
                Op::Opened { path } => db::bump_open(&db, path),
            };
            if let Err(e) = r {
                eprintln!("[lse] 落库失败: {e}");
                failed = true;
                break;
            }
        }
        let tail = if failed { "ROLLBACK;" } else { "COMMIT;" };
        if let Err(e) = db.execute_batch(tail) {
            eprintln!("[lse] 提交失败: {e}");
        }
    }
    let mut g = sh.idx.write().unwrap();
    g.apply_ops(ops);
    // 墓碑压缩
    if g.raw_len() > 0 && g.tombstone_count() as usize * 5 >= g.raw_len() {
        g.compact();
    }
}

fn event_loop(sh: Arc<Shared>, watch: Arc<WatchManager>) {
    loop {
        if sh.quit.load(Ordering::Relaxed) {
            checkpoint_and_exit(&sh);
        }
        let ops = watch.try_recv_ops();
        if ops.is_empty() {
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        if sh.building.load(Ordering::SeqCst) {
            sh.dirty.lock().unwrap().extend(ops);
        } else {
            apply_ops(&sh, ops);
        }
    }
}

fn checkpoint_and_exit(sh: &Shared) {
    let db = sh.db.lock().unwrap();
    let _ = db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(db);
    std::process::exit(0);
}

fn main() {
    // 降低自身优先级，避免与桌面交互争抢 CPU（用户机器性能有限）
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }

    let cfg = Config::load();
    let ex = Arc::new(ExcludeRules::load(&cfg.extra_excludes));
    let scanner = Arc::new(Scanner::new(cfg.clone(), Arc::clone(&ex)));

    let db_path = crate_db_path();
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[lse] 打开索引数据库失败 {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    let last_scan = db::get_meta(&conn, "last_scan")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let has_index = last_scan > 0;

    let sh = Arc::new(Shared {
        idx: Arc::new(RwLock::new(Index::new())),
        db: Mutex::new(conn),
        progress: Arc::new(Progress::default()),
        last_scan: AtomicI64::new(last_scan),
        building: AtomicBool::new(false),
        dirty: Mutex::new(Vec::new()),
        quit: AtomicBool::new(false),
        cfg,
        scanner,
        watch_holder: Mutex::new(None),
    });

    let watch = WatchManager::new(
        sh.cfg.clone(),
        Arc::clone(&sh.idx),
        Arc::clone(&sh.scanner),
    );

    let svc = IndexSvc {
        sh: Arc::clone(&sh),
        watch: Mutex::new(Some(Arc::clone(&watch))),
    };
    *sh.watch_holder.lock().unwrap() = Some(Arc::clone(&watch));

    // 后台初始化线程：加载或建索引 → 启动监听 → 启动事件循环
    {
        let sh = Arc::clone(&sh);
        let watch = Arc::clone(&watch);
        std::thread::Builder::new()
            .name("lse-init".into())
            .spawn(move || {
                if has_index {
                    sh.progress.set_phase(4); // loading
                    let t0 = std::time::Instant::now();
                    let loaded = {
                        let db = sh.db.lock().unwrap();
                        db::load_index(&db, &crate_db_path()).ok()
                    };
                    if let Some(mut idx) = loaded {
                        // lookup 表后台重建不阻塞查询——但 watch 启动前必须就绪，
                        // 这里在 init 线程同步完成（D-Bus 已可应答查询）。
                        idx.build_lookup();
                        if let Ok(opens) = {
                            let db = sh.db.lock().unwrap();
                            db::load_opens(&db)
                        } {
                            for (p, c) in opens {
                                idx.set_open(&p, c);
                            }
                        }
                        *sh.idx.write().unwrap() = idx;
                        eprintln!(
                            "[lse] 冷启动加载索引完成: {:?}",
                            t0.elapsed()
                        );
                        sh.progress.set_phase(3);
                    } else {
                        // 损坏 → 重建
                        sh.building.store(true, Ordering::SeqCst);
                        let sh2 = Arc::clone(&sh);
                        std::thread::spawn(move || run_full_scan(sh2));
                    }
                } else {
                    sh.building.store(true, Ordering::SeqCst);
                    let sh2 = Arc::clone(&sh);
                    std::thread::spawn(move || run_full_scan(sh2));
                }

                // 一致性校验（可选路径）：上次扫描后被改动的目录局部重扫
                consistency_check(&sh);

                // 启动 inotify
                if let Err(e) = watch.start() {
                    eprintln!("[lse] 启动 inotify 失败: {e}");
                }

                // 事件应用循环
                event_loop(sh, watch);
            })
            .expect("spawn init thread");
    }

    // D-Bus 服务
    futures_lite::future::block_on(async {
        let builder = match zbus::connection::Builder::session()
            .and_then(|b| b.name("org.lse.Index"))
            .and_then(|b| b.serve_at("/org/lse/Index", svc))
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[lse] D-Bus 服务构建失败: {e}");
                std::process::exit(1);
            }
        };
        let conn = match builder.build().await {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("NameTaken") || msg.contains("exists") {
                    eprintln!("[lse] D-Bus 名称 org.lse.Index 已被占用——另一个 daemon 正在运行。");
                    std::process::exit(2);
                }
                eprintln!("[lse] D-Bus 服务启动失败: {e}");
                std::process::exit(1);
            }
        };
        // 持有连接直到退出
        std::future::pending::<()>().await;
        #[allow(unreachable_code)]
        {
            let _ = conn;
            std::process::exit(0);
        }
    });
}

/// 启动时局部重扫：mtime 晚于 last_scan 的目录（异常退出后的一致性校验）。
fn consistency_check(sh: &Arc<Shared>) {
    let last = sh.last_scan.load(Ordering::Relaxed);
    if last == 0 {
        return;
    }
    let stale: Vec<String> = {
        let db = sh.db.lock().unwrap();
        let mut out = Vec::new();
        if let Ok(mut stmt) = db.prepare("SELECT path FROM dirs WHERE mtime > ?1 LIMIT 5000") {
            if let Ok(rows) = stmt.query_map([last], |r| r.get::<_, String>(0)) {
                for row in rows.flatten() {
                    out.push(row);
                }
            }
        }
        out
    };
    if stale.is_empty() {
        return;
    }
    eprintln!("[lse] 一致性校验: {} 个目录在上次扫描后被修改，局部重扫", stale.len());
    let mut ops = Vec::new();
    for d in &stale {
        let sub = lse_core::watch::rescan_subtree(d, &sh.idx, &sh.scanner);
        ops.extend(sub);
    }
    // 比对后应用（diff 由 matches 完成）
    let ops: Vec<Op> = {
        let g = sh.idx.read().unwrap();
        ops.into_iter()
            .filter(|op| match op {
                Op::Upsert { path, size, mtime, is_dir } => !g.matches(path, *size, *mtime, *is_dir),
                _ => true,
            })
            .collect()
    };
    if !ops.is_empty() {
        apply_ops(sh, ops);
    }
}
