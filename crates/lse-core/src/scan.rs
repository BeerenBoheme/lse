//! 多线程并行遍历：只用 readdir 的 d_type 判断类型，不逐文件 stat。
//! size/mtime 通过一次 `statx`（路径级）按需采集——仅在“首次建索引”与“增量事件”时发生，
//! 查询路径永远不遍历文件系统。
//!
//! 实现要点：
//! - `std::fs::read_dir` 在 Linux 上底层是 `readdir(3)`，`DirEntry::file_type()`
//!   直接读取 `dirent.d_type`（DT_UNKNOWN 时才回退 lstat——主流 fs 均提供 d_type）；
//! - 用 Mutex<VecDeque> 工作队列 + 固定线程池（默认 min(核数, 8)），避免线程爆炸；
//! - 采集四元组 (path, size, mtime, is_dir)，size/mtime 来自 `statx` 系调用
//!   （symlink 用 lstat 语义不跟随，防止环）。

use crate::config::{ExcludeRules, Config};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct Scanned {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub dirs: u64,
    pub files: u64,
    pub skipped: u64,
    pub errors: u64,
    pub elapsed_ms: u64,
}

/// 建库进度（daemon 暴露给 GUI/CLI）。
#[derive(Default)]
pub struct Progress {
    pub scanned_dirs: AtomicU64,
    pub scanned_files: AtomicU64,
    pub total_estimate: AtomicU64,
    pub phase: AtomicU8, // 0=idle 1=counting 2=scanning 3=done
    pub done: AtomicU64, // 已完成的扫描条目数（阶段2）
    pub started_unix_ms: AtomicU64,
}

impl Progress {
    pub fn set_phase(&self, p: u8) {
        self.phase.store(p, Ordering::Relaxed);
        if p == 1 || p == 2 {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            self.started_unix_ms.store(now, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> (u8, u64, u64, u64) {
        (
            self.phase.load(Ordering::Relaxed),
            self.done.load(Ordering::Relaxed),
            self.total_estimate.load(Ordering::Relaxed),
            self.started_unix_ms.load(Ordering::Relaxed),
        )
    }

    /// (percent 0..=100, eta_secs；估算失败返回 -1)
    pub fn percent_eta(&self) -> (u8, i64) {
        let (_, done, total, started) = self.snapshot();
        if total == 0 {
            return (0, -1);
        }
        let pct = ((done.min(total) * 100) / total).min(100) as u8;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if done > 0 && now > started {
            let elapsed = now - started;
            let rate = done as u128 * 1000 / elapsed as u128;
            // done >= total（扫描期间目录里出现新文件，实扫可超过预统计）时
            // (total - done) 会 u64 下溢出 ≈1.8e19，除以速率得到天文数字 ETA，
            // 必须先截断。
            if done >= total {
                return (pct, 0);
            }
            if rate > 0 {
                let eta = (total - done) as u128 / rate;
                return (pct, eta as i64);
            }
        }
        (pct, -1)
    }
}

struct QueueState {
    q: VecDeque<String>,
    active: usize,
}

struct Pool {
    st: Mutex<QueueState>,
    cv: Condvar,
}

/// 读取挂载表：跳过网络/特殊 fs（可配置）。返回 mountpoint -> fstype。
pub fn read_mounts() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(s) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return map;
    };
    for line in s.lines() {
        // 格式: id parent_id major:minor root mountpoint options [optional...] - fstype source super
        let Some((_pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let Some(left) = line.get(0..(line.len() - post.len())) else {
            continue;
        };
        let mut it = left.split_whitespace();
        let _id = it.next();
        let _pid = it.next();
        let _dev = it.next();
        let _root = it.next();
        let Some(mp) = it.next() else { continue };
        let fstype = post.split_whitespace().next().unwrap_or("");
        map.insert(unescape_mount(mp), fstype.to_string());
    }
    map
}

fn unescape_mount(s: &str) -> String {
    // mountinfo 用 \040 等八进制转义
    let b = s.as_bytes();
    if !b.contains(&b'\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

pub struct Scanner {
    pub cfg: Config,
    pub ex: Arc<ExcludeRules>,
    mounts: HashMap<String, String>,
    skip_fs: HashSet<String>,
    threads: usize,
}

impl Scanner {
    pub fn new(cfg: Config, ex: Arc<ExcludeRules>) -> Self {
        let mounts = read_mounts();
        let skip_fs = crate::config::skip_fs_set(&cfg);
        let threads = cfg.scan_threads.max(1);
        Scanner {
            cfg,
            ex,
            mounts,
            skip_fs,
            threads,
        }
    }

    /// 快速预扫描：只统计目录与文件数（d_type，不 stat），为进度条提供总数估算。
    pub fn precount(&self, progress: &Progress) -> ScanStats {
        progress.set_phase(1);
        let dirs = Arc::new(AtomicU64::new(0));
        let files = Arc::new(AtomicU64::new(0));
        let skipped = Arc::new(AtomicU64::new(0));

        let pool = Arc::new(Pool {
            st: Mutex::new(QueueState {
                q: self.roots(),
                active: 0,
            }),
            cv: Condvar::new(),
        });

        let ex = self.ex.clone();
        let mounts = self.mounts.clone();
        let skip_fs = self.skip_fs.clone();
        let nthreads = self.threads;
        let start = Instant::now();

        std::thread::scope(|s| {
            for _ in 0..nthreads {
                let pool = Arc::clone(&pool);
                let ex = Arc::clone(&ex);
                let dirs = &dirs;
                let files = &files;
                let skipped = &skipped;
                let mounts = &mounts;
                let skip_fs = &skip_fs;
                s.spawn(move || {
                    loop {
                        let next = {
                            let mut st = pool.st.lock().unwrap();
                            loop {
                                if let Some(d) = st.q.pop_front() {
                                    st.active += 1;
                                    break Some(d);
                                }
                                if st.active == 0 {
                                    break None;
                                }
                                st = pool.cv.wait(st).unwrap();
                            }
                        };
                        let Some(dir) = next else { return };
                        let mut children = Vec::new();
                        let rd = match std::fs::read_dir(&dir) {
                            Ok(rd) => rd,
                            Err(_) => {
                                let mut st = pool.st.lock().unwrap();
                                st.active -= 1;
                                if st.active == 0 && st.q.is_empty() {
                                    pool.cv.notify_all();
                                }
                                if st.q.is_empty() && st.active == 0 {
                                    drop(st);
                                    pool.cv.notify_all();
                                }
                                continue;
                            }
                        };
                        for ent in rd.flatten() {
                            let ft = match ent.file_type() {
                                Ok(t) => t,
                                Err(_) => continue,
                            };
                            let name = ent.file_name();
                            let name_b = name.as_encoded_bytes();
                            if ft.is_dir() {
                                let mut p = String::with_capacity(dir.len() + 1 + name_b.len());
                                p.push_str(&dir);
                                if !dir.ends_with('/') {
                                    p.push('/');
                                }
                                p.push_str(&String::from_utf8_lossy(name_b));
                                if ex.is_excluded(&p) {
                                    skipped.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                if should_skip(&p, mounts, skip_fs) {
                                    skipped.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                dirs.fetch_add(1, Ordering::Relaxed);
                                children.push(p);
                            } else {
                                files.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        {
                            let mut st = pool.st.lock().unwrap();
                            st.q.extend(children);
                            st.active -= 1;
                            if st.q.is_empty() && st.active == 0 {
                                pool.cv.notify_all();
                                break;
                            }
                            pool.cv.notify_all();
                        }
                    }
                });
            }
        });

        let stats = ScanStats {
            dirs: dirs.load(Ordering::Relaxed),
            files: files.load(Ordering::Relaxed),
            skipped: skipped.load(Ordering::Relaxed),
            errors: 0,
            elapsed_ms: start.elapsed().as_millis() as u64,
        };
        progress
            .total_estimate
            .store(stats.dirs + stats.files, Ordering::Relaxed);
        stats
    }

    fn roots(&self) -> VecDeque<String> {
        let mut q = VecDeque::new();
        for r in &self.cfg.roots {
            let rp = r.trim_end_matches('/').to_string();
            let rp = if rp.is_empty() { "/".to_string() } else { rp };
            if !self.ex.is_excluded(&rp) {
                q.push_back(rp);
            }
        }
        q
    }

    /// 正式遍历（从配置 roots 开始）。`emit` 接收每条 (path, size, mtime, is_dir)。
    /// size/mtime 采集：目录与文件各一次 stat 系调用（四元组要求）；
    /// **类型判定只依赖 d_type**，不为此额外 stat。
    pub fn walk<F>(&self, progress: &Progress, emit: F) -> ScanStats
    where
        F: Fn(Scanned) + Send + Sync + 'static,
    {
        self.walk_roots(self.roots(), progress, emit)
    }

    /// 从指定单棵子树开始遍历（增量补扫/unwatched 重扫用）。
    pub fn walk_from<F>(&self, root: &str, progress: &Progress, emit: F) -> ScanStats
    where
        F: Fn(Scanned) + Send + Sync + 'static,
    {
        let mut q = VecDeque::new();
        let r = root.to_string();
        if !self.ex.is_excluded(&r) {
            q.push_back(r);
        }
        self.walk_roots(q, progress, emit)
    }

    pub fn walk_roots<F>(&self, initial: VecDeque<String>, progress: &Progress, emit: F) -> ScanStats
    where
        F: Fn(Scanned) + Send + Sync + 'static,
    {
        progress.set_phase(2);
        let emit = Arc::new(emit);
        let dirs = Arc::new(AtomicU64::new(0));
        let files = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicU64::new(0));

        let pool = Arc::new(Pool {
            st: Mutex::new(QueueState {
                q: initial,
                active: 0,
            }),
            cv: Condvar::new(),
        });

        let ex = self.ex.clone();
        let mounts = self.mounts.clone();
        let skip_fs = self.skip_fs.clone();
        let start = Instant::now();

        std::thread::scope(|s| {
            for _ in 0..self.threads {
                let pool = Arc::clone(&pool);
                let ex = Arc::clone(&ex);
                let emit = Arc::clone(&emit);
                let dirs = &dirs;
                let files = &files;
                let errors = &errors;
                let done = &done;
                let mounts = &mounts;
                let skip_fs = &skip_fs;
                let progress = progress;
                s.spawn(move || {
                    loop {
                        let next = {
                            let mut st = pool.st.lock().unwrap();
                            loop {
                                if let Some(d) = st.q.pop_front() {
                                    st.active += 1;
                                    break Some(d);
                                }
                                if st.active == 0 {
                                    break None;
                                }
                                st = pool.cv.wait(st).unwrap();
                            }
                        };
                        let Some(dir) = next else { return };

                        // 先采集目录自身四元组（目录 size 统一为 0：与 db dirs 表/事件路径保持一致）
                        if let Ok(md) = std::fs::symlink_metadata(&dir) {
                            emit(Scanned {
                                path: dir.clone(),
                                size: 0,
                                mtime: mtime_of(&md),
                                is_dir: md.is_dir(),
                            });
                            dirs.fetch_add(1, Ordering::Relaxed);
                            done.fetch_add(1, Ordering::Relaxed);
                        }

                        let rd = match std::fs::read_dir(&dir) {
                            Ok(rd) => rd,
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                                let mut st = pool.st.lock().unwrap();
                                st.active -= 1;
                                if st.q.is_empty() && st.active == 0 {
                                    pool.cv.notify_all();
                                    return;
                                }
                                pool.cv.notify_all();
                                continue;
                            }
                        };

                        let mut children: Vec<String> = Vec::new();
                        for ent in rd.flatten() {
                            // ★ 类型来自 d_type：不为“判断类型”调用 stat
                            let Ok(ft) = ent.file_type() else {
                                errors.fetch_add(1, Ordering::Relaxed);
                                continue;
                            };
                            let name = ent.file_name();
                            let name_b = name.as_encoded_bytes();
                            let is_dir = ft.is_dir();

                            if is_dir {
                                let mut p = String::with_capacity(dir.len() + 1 + name_b.len());
                                p.push_str(&dir);
                                if !dir.ends_with('/') {
                                    p.push('/');
                                }
                                p.push_str(&String::from_utf8_lossy(name_b));
                                if ex.is_excluded(&p)
                                    || should_skip(&p, mounts, skip_fs)
                                {
                                    continue;
                                }
                                children.push(p);
                            } else {
                                // 文件：类型由 d_type 给出（含 symlink→文件），
                                // 大小/mtime 用一次 symlink_metadata（不跟随符号链接）
                                let md = match ent.metadata() {
                                    Ok(m) => m,
                                    Err(_) => {
                                        errors.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                };
                                let mut p = String::with_capacity(dir.len() + 1 + name_b.len());
                                p.push_str(&dir);
                                if !dir.ends_with('/') {
                                    p.push('/');
                                }
                                p.push_str(&String::from_utf8_lossy(name_b));
                                emit(Scanned {
                                    path: p,
                                    size: md.len(),
                                    mtime: mtime_of(&md),
                                    is_dir: false,
                                });
                                files.fetch_add(1, Ordering::Relaxed);
                                done.fetch_add(1, Ordering::Relaxed);
                            }
                        }

                        {
                            let mut st = pool.st.lock().unwrap();
                            st.q.extend(children);
                            st.active -= 1;
                            if st.q.is_empty() && st.active == 0 {
                                pool.cv.notify_all();
                                break;
                            }
                            pool.cv.notify_all();
                        }
                        if done.load(Ordering::Relaxed) % 4096 < 64 {
                            progress
                                .done
                                .store(done.load(Ordering::Relaxed), Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        progress
            .done
            .store(done.load(Ordering::Relaxed), Ordering::Relaxed);

        ScanStats {
            dirs: dirs.load(Ordering::Relaxed),
            files: files.load(Ordering::Relaxed),
            skipped: 0,
            errors: errors.load(Ordering::Relaxed),
            elapsed_ms: start.elapsed().as_millis() as u64,
        }
    }
}

fn should_skip(path: &str, mounts: &HashMap<String, String>, skip_fs: &HashSet<String>) -> bool {
    if let Some(fs) = mounts.get(path) {
        return skip_fs.contains(fs);
    }
    false
}

pub fn mtime_of(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 发送器辅助：把 walk 结果转成 channel（供 db 写入线程消费）。
pub fn walk_to_channel(
    scanner: Arc<Scanner>,
    progress: Arc<Progress>,
) -> (Receiver<Scanned>, std::thread::JoinHandle<ScanStats>) {
    let (tx, rx): (Sender<Scanned>, _) = channel();
    let handle = std::thread::Builder::new()
        .name("lse-scan".into())
        .spawn(move || {
            let stats = scanner.walk(&progress, move |s| {
                let _ = tx.send(s);
            });
            stats
        })
        .expect("spawn scan thread");
    (rx, handle)
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_is_zero_when_done_exceeds_total() {
        // 扫描期间目录里出现新文件时 done 会略超预统计 total，
        // 旧代码 (total - done) u64 下溢 → ETA 显示 344 亿小时。
        let p = Progress::default();
        p.total_estimate.store(1000, Ordering::Relaxed);
        p.done.store(1001, Ordering::Relaxed);
        p.phase.store(2, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        p.started_unix_ms.store(now.saturating_sub(1000), Ordering::Relaxed);
        let (pct, eta) = p.percent_eta();
        assert_eq!(eta, 0, "done>total 时 ETA 必须为 0，不能下溢");
        assert_eq!(pct, 100);
    }

    #[test]
    fn eta_is_sane_when_done_below_total() {
        let p = Progress::default();
        p.total_estimate.store(1000, Ordering::Relaxed);
        p.done.store(250, Ordering::Relaxed);
        p.phase.store(2, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        p.started_unix_ms.store(now.saturating_sub(1000), Ordering::Relaxed);
        let (pct, eta) = p.percent_eta();
        assert_eq!(pct, 25);
        // 250 个/秒 速率下剩 750 → 约 3 秒；允许几秒浮动但必须是小数值
        assert!(eta >= 0 && eta < 60, "eta={eta} 应在合理量级");
    }
}
