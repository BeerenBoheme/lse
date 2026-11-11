//! inotify 增量更新（基于 notify crate，底层 inotify）。
//!
//! 应对 `fs.inotify.max_user_watches` 上限的方案（不允许静默失效）：
//! 1. 启动读取 `/proc/sys/fs/inotify/max_user_watches` 与本用户进程已用量，
//!    预算 = limit × watch_budget_ratio − 已用；
//! 2. **只 watch 目录**（1 watch/目录），文件级变化由目录事件推导，
//!    所需 watch 数 = 索引目录数（不是文件数）；
//! 3. 目录按 `watch_priority`（默认 home 优先）排序分配预算；
//! 4. 预算外的子树登记为 unwatched，退化为定期重扫：
//!    无变化 5 分钟 → 空扫 1 次 15 分钟 → 连续 ≥3 次空扫封顶 60 分钟；
//! 5. 预算不足时 status 携带 sysctl 提示命令，GUI/CLI 显示警告。

use crate::config::Config;
use crate::index::{Index, Op};
use crate::scan::{mtime_of, Scanner};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub fn read_watch_limit() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8192)
}

/// 统计本 uid 进程已占用的 watch 数（仅启动/restart 时执行一次）。
pub fn estimate_used_watches() -> u64 {
    let uid = unsafe { libc::getuid() };
    let self_pid = std::process::id();
    let mut used = 0u64;
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for ent in rd.flatten() {
        let Ok(pid) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // 跳过自身：restart 时旧 watcher 的 fd 拆除是异步的，
        // 立即统计会把自己即将释放的 watch 误计为“他人占用”导致预算骤降。
        if pid == self_pid {
            continue;
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok();
        let mine = status
            .as_deref()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|u| u.parse::<u32>().ok())
            .map(|u| u == uid)
            .unwrap_or(false);
        if !mine {
            continue;
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                let fdi = format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_string_lossy());
                if let Ok(s) = std::fs::read_to_string(&fdi) {
                    used += s.lines().filter(|l| l.starts_with("inotify ")).count() as u64;
                }
            }
        }
    }
    used
}

#[derive(Debug, Clone, Default)]
pub struct WatchStatus {
    pub watching: bool,
    pub watched_dirs: u64,
    pub limit: u64,
    pub budget: u64,
    pub needed: u64,
    pub unwatched_dirs: u64,
    pub warning: String,
}

struct WatchInner {
    watcher: Option<RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    unwatched_roots: Vec<String>,
    status: WatchStatus,
    /// 代际：restart 时递增，旧线程自行退出
    epoch: u64,
}

pub struct WatchManager {
    inner: Arc<Mutex<WatchInner>>,
    cfg: Config,
    running: AtomicBool,
    epoch: AtomicU64,
    idx: Arc<RwLock<Index>>,
    scanner: Arc<Scanner>,
    ops_tx: Sender<Vec<Op>>,
    ops_rx: Mutex<Receiver<Vec<Op>>>,
}

impl WatchManager {
    pub fn new(
        cfg: Config,
        idx: Arc<RwLock<Index>>,
        scanner: Arc<Scanner>,
    ) -> Arc<Self> {
        let (tx, rx) = channel();
        Arc::new(WatchManager {
            inner: Arc::new(Mutex::new(WatchInner {
                watcher: None,
                watched: HashSet::new(),
                unwatched_roots: Vec::new(),
                status: WatchStatus::default(),
                epoch: 0,
            })),
            cfg,
            running: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            idx,
            scanner,
            ops_tx: tx,
            ops_rx: Mutex::new(rx),
        })
    }

    pub fn status(&self) -> WatchStatus {
        self.inner.lock().unwrap().status.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn unwatched_roots(&self) -> Vec<String> {
        self.inner.lock().unwrap().unwatched_roots.clone()
    }

    /// daemon 主循环用：取走事件批量操作。
    pub fn try_recv_ops(&self) -> Vec<Op> {
        let rx = self.ops_rx.lock().unwrap();
        let mut all = Vec::new();
        while let Ok(ops) = rx.try_recv() {
            all.extend(ops);
        }
        all
    }

    /// 启动监听：按预算 watch 目录；预算外子树进重扫清单。
    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        let dirs = self.idx.read().unwrap().all_dirs();
        let limit = read_watch_limit();
        let used = estimate_used_watches();
        let budget_f = limit as f64 * self.cfg.watch_budget_ratio.clamp(0.1, 1.0);
        let budget = (budget_f - used as f64).max(0.0) as u64;
        let needed = dirs.len() as u64;

        // 优先级排序：home 等前缀优先
        let prio = &self.cfg.watch_priority;
        let mut sorted = dirs;
        sorted.sort_by(|a, b| {
            let pa = prio.iter().position(|pr| a.starts_with(pr.as_str())).unwrap_or(prio.len());
            let pb = prio.iter().position(|pr| b.starts_with(pr.as_str())).unwrap_or(prio.len());
            pa.cmp(&pb).then_with(|| a.cmp(b))
        });

        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        })
        .map_err(|e| format!("创建 inotify watcher 失败: {e}"))?;

        let mut watch_ok = 0u64;
        // 记录**实际 watch 成功**的路径：不能用 sorted.take(watch_ok) 反推——
        // 失败的目录可能夹在中间，那样会把没监听成功的目录放进 watched 集合
        // （后续 try_watch 提前返回、永远收不到事件），同时漏掉真正监听成功的。
        let mut watched_paths: Vec<String> = Vec::new();
        let mut unwatched_roots: Vec<String> = Vec::new();
        for d in sorted.iter() {
            if watch_ok < budget
                && watcher.watch(Path::new(d), RecursiveMode::NonRecursive).is_ok()
            {
                watch_ok += 1;
                watched_paths.push(d.clone());
                continue;
            }
            unwatched_roots.push(d.clone());
        }

        // 去嵌套：父目录已在重扫清单中时跳过子目录（父的子树重扫天然覆盖），
        // 防止嵌套子树重复扫描造成 ops 风暴。
        {
            let set: HashSet<String> = unwatched_roots.iter().cloned().collect();
            unwatched_roots.retain(|u| {
                match std::path::Path::new(u).parent() {
                    Some(par) if !par.as_os_str().is_empty() && par != std::path::Path::new("/") => {
                        !set.contains(&par.to_string_lossy().to_string())
                    }
                    _ => true,
                }
            });
        }

        let warning = if needed > budget {
            format!(
                "inotify watch 预算不足：需要 {needed}，可用 {budget}（系统上限 {limit}，本用户已用 {used}）。\n\
                 {unw} 个目录无法实时监听，将按 5→15→60 分钟退避定期重扫。\n\
                 建议执行：\n  sudo sysctl -w fs.inotify.max_user_watches={sug}\n\
                 持久化：\n  echo 'fs.inotify.max_user_watches={sug}' | sudo tee /etc/sysctl.d/60-lse.conf",
                needed = needed,
                budget = budget,
                limit = limit,
                used = used,
                unw = unwatched_roots.len(),
                sug = needed.next_power_of_two().max(524288),
            )
        } else {
            String::new()
        };

        let my_epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.watched = watched_paths.iter().map(PathBuf::from).collect();
            inner.unwatched_roots = unwatched_roots;
            inner.watcher = Some(watcher);
            inner.epoch = my_epoch;
            inner.status = WatchStatus {
                watching: true,
                watched_dirs: watch_ok,
                limit,
                budget,
                needed,
                unwatched_dirs: inner.unwatched_roots.len() as u64,
                warning,
            };
        }
        self.running.store(true, Ordering::Relaxed);

        self.spawn_event_thread(my_epoch, rx);
        self.spawn_maintenance(my_epoch);
        Ok(())
    }

    /// 新目录出现时立即尝试补 watch（预算内）。
    fn try_watch_dir(&self, path: &Path) {
        let mut inner = self.inner.lock().unwrap();
        if inner.watched.contains(path) {
            return;
        }
        // 预算已满：踢出优先级最低（watch_priority 排位靠后、同排位字典序靠后）的
        // 已监听目录，为新目录腾出预算——新目录通常是用户正在操作的区域，优先级更高。
        // 被踢目录加入 unwatched 清单，由退避重扫兜底。
        if inner.status.watched_dirs >= inner.status.budget && inner.status.budget > 0 {
            let prio_of = |p: &Path| {
                let s = p.to_string_lossy();
                self.cfg
                    .watch_priority
                    .iter()
                    .position(|pr| s.starts_with(pr.as_str()))
                    .unwrap_or(usize::MAX)
            };
            let victim = inner
                .watched
                .iter()
                .filter(|w| !path.starts_with(w.as_path())) // 不踢自己的祖先
                .cloned()
                .max_by(|a, b| prio_of(a).cmp(&prio_of(b)).then_with(|| a.cmp(b)));
            if let Some(v) = victim {
                if let Some(w) = inner.watcher.as_mut() {
                    let _ = w.unwatch(&v);
                }
                inner.watched.remove(&v);
                inner.status.watched_dirs = inner.status.watched_dirs.saturating_sub(1);
                let vs = v.to_string_lossy().to_string();
                if !inner.unwatched_roots.contains(&vs) {
                    inner.unwatched_roots.push(vs);
                }
                inner.status.unwatched_dirs = inner.unwatched_roots.len() as u64;
                eprintln!(
                    "[lse] inotify 预算已满，踢出低优先级目录 {} 为新目录腾出预算",
                    v.display()
                );
            } else {
                return;
            }
        }
        if let Some(w) = inner.watcher.as_mut() {
            match w.watch(path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    inner.watched.insert(path.to_path_buf());
                    inner.status.watched_dirs += 1;
                    // 从 unwatched 清单里移除**恰好是这个**目录的根：
                    // 只有 r == path 才算被新 watch 覆盖。path 是 r 的子孙时必须保留 r
                    // ——非递归 watch 只覆盖 r 一层，r 的其他子树仍靠退避重扫兜底；
                    // 反过来（旧写法保留 r == path）会让刚监听成功的根永久留在清单里，
                    // 被重复重扫且 status.unwatched_dirs 虚高。
                    inner.unwatched_roots.retain(|r| Path::new(r.as_str()) != path);
                    inner.status.unwatched_dirs = inner.unwatched_roots.len() as u64;
                }
                Err(e) => {
                    eprintln!("[lse] watch 目录失败 {}: {e}", path.display());
                }
            }
        }
    }

    fn spawn_event_thread(self: &Arc<Self>, epoch: u64, rx: Receiver<notify::Result<Event>>) {
        let mgr = Arc::clone(self);
        std::thread::Builder::new()
            .name("lse-watch-events".into())
            .spawn(move || {
                let debounce = Duration::from_millis(mgr.cfg.debounce_ms.clamp(50, 2000));
                let mut batch: Vec<PathBuf> = Vec::new();
                loop {
                    if mgr.epoch.load(Ordering::SeqCst) != epoch {
                        return; // restart 后旧线程退出
                    }
                    match rx.recv_timeout(debounce) {
                        Ok(Ok(ev)) => {
                            // Access 类事件（读文件）不改变索引，直接忽略，避免事件洪水
                            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                                batch.extend(ev.paths);
                            }
                        }
                        Ok(Err(_)) => {}
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    // 倾泻突发事件（如 git checkout / 解压）
                    while let Ok(Ok(ev2)) = rx.try_recv() {
                        batch.extend(ev2.paths);
                    }
                    if batch.is_empty() {
                        continue;
                    }
                    batch.sort();
                    batch.dedup();
                    let ops = process_paths(&batch, &mgr);
                    batch.clear();
                    if !ops.is_empty() {
                        let _ = mgr.ops_tx.send(ops);
                    }
                }
            })
            .expect("spawn watch event thread");
    }

    fn spawn_maintenance(self: &Arc<Self>, epoch: u64) {
        let mgr = Arc::clone(self);
        let base = self.cfg.unwatched_rescan_interval.clamp(60, 3600);
        std::thread::Builder::new()
            .name("lse-watch-maint".into())
            .spawn(move || {
                let mut miss: HashMap<String, u32> = HashMap::new();
                let mut last_done: HashMap<String, Instant> = HashMap::new();
                let mut cursor: usize = 0;
                // 启动时以“刚做过”为基线：首次重扫延迟到退避周期后，避免开机即全量子树风暴
                let mut primed = false;
                // 每轮最多处理的子树数（轮转），避免一次巨量 ops 淹没事件循环
                const PER_ROUND: usize = 32;
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    if mgr.epoch.load(Ordering::SeqCst) != epoch || !mgr.running.load(Ordering::Relaxed) {
                        return;
                    }
                    let roots: Vec<String> = mgr.inner.lock().unwrap().unwatched_roots.clone();
                    // 剪掉已从清单消失的根：miss/last_done 只插不删会让键随目录
                    // churn 无限累积（慢泄漏），且幽灵根的退避状态毫无意义。
                    {
                        let live: HashSet<&String> = roots.iter().collect();
                        miss.retain(|k, _| live.contains(k));
                        last_done.retain(|k, _| live.contains(k));
                    }
                    if roots.is_empty() {
                        continue;
                    }
                    let now = Instant::now();
                    if !primed {
                        for r in &roots {
                            last_done.insert(r.clone(), now);
                        }
                        primed = true;
                        continue;
                    }
                    let n = roots.len();
                    for i in 0..PER_ROUND.min(n) {
                        let idx = (cursor + i) % n;
                        let root = roots[idx].clone();
                        let misses = *miss.get(&root).unwrap_or(&0);
                        // 退避：300s → 900s → 3600s
                        let backoff = match misses {
                            0 => base.min(300),
                            1..=2 => 900,
                            _ => 3600,
                        };
                        if let Some(t) = last_done.get(&root) {
                            if now.duration_since(*t).as_secs() < backoff {
                                continue;
                            }
                        }
                        let ops = rescan_subtree(&root, &mgr.idx, &mgr.scanner);
                        let changed = diff_ops(&ops, &mgr.idx);
                        if changed.is_empty() {
                            *miss.entry(root.clone()).or_insert(0) += 1;
                        } else {
                            miss.insert(root.clone(), 0);
                            let _ = mgr.ops_tx.send(changed);
                        }
                        last_done.insert(root, now);
                    }
                    cursor = (cursor + PER_ROUND) % n;
                }
            })
            .expect("spawn watch maintenance thread");
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let mut inner = self.inner.lock().unwrap();
        // 丢弃 watcher 即停止全部监听
        inner.watcher = None;
        inner.status.watching = false;
    }

    /// 重建索引后刷新监听集合。
    pub fn restart(self: &Arc<Self>) {
        self.stop();
        if let Err(e) = self.start() {
            eprintln!("[lse] 重启 inotify 失败: {e}");
        }
    }
}

/// 把一批事件路径转换为索引操作（存在→Upsert/子树补扫；消失→删除）。
fn process_paths(paths: &[PathBuf], mgr: &WatchManager) -> Vec<Op> {
    let mut ops = Vec::new();
    for p in paths {
        let lossy = p.to_string_lossy();
        if mgr.scanner.ex.is_excluded(&lossy) {
            continue;
        }
        match std::fs::symlink_metadata(p) {
            Ok(md) => {
                let is_dir = md.is_dir();
                if lossy.contains('\u{FFFD}') {
                    continue; // 非 UTF-8 名字：无法安全表示，跳过（不误删）
                }
                ops.push(Op::Upsert {
                    path: lossy.to_string(),
                    // 目录 size 统一为 0（与 db/load 一致，避免 diff 永不收敛）
                    size: if is_dir { 0 } else { md.len() },
                    mtime: mtime_of(&md),
                    is_dir,
                });
                if is_dir {
                    mgr.try_watch_dir(p);
                    // 只有**新目录**才做子树补扫；已知目录仅更新自身 mtime
                    // （其子项变化会由各自 watch 的事件覆盖，unwatched 子树由退避重扫兜底）。
                    // 否则一次目录事件触发整棵子树遍历，在事件洪水下会拖垮事件循环。
                    let known = mgr.idx.read().unwrap().is_dir_path(&lossy);
                    if !known {
                        let sub = rescan_subtree(&lossy, &mgr.idx, &mgr.scanner);
                        for op in &sub {
                            if let Op::Upsert { path, is_dir: true, .. } = op {
                                mgr.try_watch_dir(Path::new(path));
                            }
                        }
                        ops.extend(sub);
                    }
                }
            }
            Err(_) => {
                if lossy.contains('\u{FFFD}') {
                    continue;
                }
                let known_dir = mgr.idx.read().unwrap().is_dir_path(&lossy);
                if known_dir {
                    ops.push(Op::RemoveSubtree {
                        prefix: lossy.to_string(),
                    });
                    let mut inner = mgr.inner.lock().unwrap();
                    // inotify 对已删除/移走的 inode 会内核侧自动撤 watch，这里只同步状态：
                    // ① watched_dirs 必须跟随集合（只增不减会让预算判定失真、过度踢
                    //    victim，反过来放大 unwatched_roots）；
                    // ② 已消失的根要从 unwatched_roots 清掉，否则幽灵键永久累积，
                    //    退避重扫每轮对不存在的路径空转（每次都产出 RemoveSubtree）。
                    inner.watched.retain(|w| !w.starts_with(p));
                    inner.status.watched_dirs = inner.watched.len() as u64;
                    // retain 保留“仍然存在”的根：等于 p 或位于 p 之下的都已消失，移除。
                    inner.unwatched_roots.retain(|r| {
                        let rp = Path::new(r.as_str());
                        !(rp == p || rp.starts_with(p))
                    });
                    inner.status.unwatched_dirs = inner.unwatched_roots.len() as u64;
                } else {
                    ops.push(Op::Remove {
                        path: lossy.to_string(),
                    });
                }
            }
        }
    }
    ops
}

/// 局部重扫：从 `root` 单棵子树遍历（不从全局 roots 开始）。
pub fn rescan_subtree(root: &str, idx: &Arc<RwLock<Index>>, scanner: &Scanner) -> Vec<Op> {
    let mut ops = Vec::new();
    let Ok(md) = std::fs::symlink_metadata(root) else {
        return vec![Op::RemoveSubtree {
            prefix: root.to_string(),
        }];
    };
    let root_is_dir = md.is_dir();
    ops.push(Op::Upsert {
        path: root.to_string(),
        size: if root_is_dir { 0 } else { md.len() },
        mtime: mtime_of(&md),
        is_dir: root_is_dir,
    });
    if !root_is_dir {
        return ops;
    }
    let collected: Arc<Mutex<Vec<Op>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = Arc::clone(&collected);
    let prog = crate::scan::Progress::default();
    scanner.walk_from(root, &prog, move |s| {
        c2.lock().unwrap().push(Op::Upsert {
            path: s.path,
            size: s.size,
            mtime: s.mtime,
            is_dir: s.is_dir,
        });
    });
    ops.extend(collected.lock().unwrap().drain(..));
    let _ = idx;
    ops
}

/// 与当前索引比对，只保留真正发生变化的操作（供退避重扫减少写放大）。
fn diff_ops(ops: &[Op], idx: &Arc<RwLock<Index>>) -> Vec<Op> {
    let g = idx.read().unwrap();
    let mut out = Vec::new();
    for op in ops {
        match op {
            Op::Upsert { path, size, mtime, is_dir } => {
                if !g.matches(path, *size, *mtime, *is_dir) {
                    out.push(op.clone());
                }
            }
            other => out.push(other.clone()),
        }
    }
    out
}
