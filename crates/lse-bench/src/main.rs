//! lse-bench：建索引耗时 / 查询延迟 / 内存占用 / 索引体积测量。
//! 子命令：
//!   lse-bench gen  <dir> <files> [dirs]   生成测试文件树
//!   lse-bench run  --root <dir> [--db <path>] [--queries a,b,c] [--load-only]
//!   lse-bench disk [--db <path>]           全盘（默认 roots）建索引测量
//! 输出 KEY=VALUE 便于 bench.sh 解析。

use lse_core::config::{Config, ExcludeRules};
use lse_core::db::{self, Rebuilder};
use lse_core::index::{Index, Query};
use lse_core::scan::{walk_to_channel, Progress, Scanner};
use std::sync::Arc;
use std::time::Instant;

fn rss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn hwm_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn gen(dir: &str, files: usize, dirs: usize) {
    let base = std::path::Path::new(dir);
    let _ = std::fs::remove_dir_all(base);
    std::fs::create_dir_all(base).expect("create bench dir");
    let exts = ["pdf", "txt", "rs", "png", "md", "mp3"];
    let words = [
        "report", "budget", "notes", "photo", "music", "source", "summary", "draft",
    ];
    let per_dir = files.div_ceil(dirs.max(1));
    let mut created = 0usize;
    for d in 0..dirs.max(1) {
        let sub = base.join(format!(
            "dir_{d:03}/{}",
            if d % 3 == 0 {
                "nested/deep".to_string()
            } else {
                format!("category_{}", d % 7)
            })
        );
        std::fs::create_dir_all(&sub).expect("create subdir");
        for i in 0..per_dir {
            if created >= files {
                break;
            }
            let w = words[(i + d) % words.len()];
            let e = exts[(i * 3 + d) % exts.len()];
            let name = format!("{w}_{d}_{i}.{e}");
            let p = sub.join(name);
            if let Err(e) = std::fs::File::create(&p) {
                eprintln!("gen error: {e}");
                break;
            }
            created += 1;
        }
    }
    println!("GEN files={created} dirs={dirs}");
}

struct ScanOut {
    stats: lse_core::scan::ScanStats,
    _idx: Index,
    raw_path_bytes: u64,
    db_ms: u128,
}

fn build_index(cfg: &Config, ex: &Arc<ExcludeRules>, db_path: &std::path::Path, progress: &Arc<Progress>) -> ScanOut {
    let scanner = Arc::new(Scanner::new(cfg.clone(), Arc::clone(ex)));
    let pre = scanner.precount(progress);
    eprintln!(
        "precount dirs={} files={} ms={}",
        pre.dirs, pre.files, pre.elapsed_ms
    );
    let (rx, handle) = walk_to_channel(Arc::clone(&scanner), Arc::clone(progress));

    let t_db = Instant::now();
    let conn = db::open(db_path).expect("open db");
    let mut reb = Rebuilder::begin(&conn).expect("begin rebuild");
    let mut idx = Index::new();
    let mut raw_path_bytes = 0u64;
    for s in rx {
        raw_path_bytes += s.path.len() as u64;
        idx.append(&s.path, s.size, s.mtime, s.is_dir);
        if s.is_dir {
            let _ = reb.insert_dir(&s.path, s.mtime);
        } else {
            let (dir, name) = db::split_path(&s.path);
            let _ = reb.insert_file(dir, name, s.size, s.mtime);
        }
    }
    let stats = handle.join().unwrap_or_default();
    let now = db::now_unix();
    reb.finish(now).expect("finish db");
    let db_ms = t_db.elapsed().as_millis();
    drop(conn);
    ScanOut {
        stats,
        _idx: idx,
        raw_path_bytes,
        db_ms,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: lse-bench gen|run|disk ...");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "gen" => {
            let dir = args.get(1).cloned().unwrap_or_default();
            let files: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);
            let dirs: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
            gen(&dir, files, dirs);
        }
        "run" | "disk" => {
            let mut root: Option<String> = None;
            let mut db_path = std::env::var("LSE_DB")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/lse-bench.db"));
            let mut queries: Vec<String> = vec![
                "report".into(),
                ".pdf".into(),
                "budget_1".into(),
                "photo".into(),
                "z".into(),
            ];
            let mut load_only = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--root" => {
                        root = args.get(i + 1).cloned();
                        i += 2;
                    }
                    "--db" => {
                        db_path = std::path::PathBuf::from(args.get(i + 1).cloned().unwrap_or_default());
                        i += 2;
                    }
                    "--queries" => {
                        if let Some(q) = args.get(i + 1) {
                            queries = q.split(',').map(|s| s.to_string()).collect();
                        }
                        i += 2;
                    }
                    "--load-only" => {
                        load_only = true;
                        i += 1;
                    }
                    other => {
                        eprintln!("unknown arg {other}");
                        i += 1;
                    }
                }
            }

            let mut cfg = Config::load();
            if let Some(r) = &root {
                cfg.roots = vec![r.clone()];
            }
            let ex = Arc::new(ExcludeRules::load(&cfg.extra_excludes));

            if !load_only && db_path.exists() {
                let _ = std::fs::remove_file(&db_path);
                let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
                let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
            }

            let progress = Arc::new(Progress::default());
            let mut has_build = false;
            let mut raw_bytes = 0u64;
            if !load_only {
                let t0 = Instant::now();
                let res = build_index(&cfg, &ex, &db_path, &progress);
                let build_ms = t0.elapsed().as_millis();
                let st = res.stats;
                has_build = true;
                raw_bytes = res.raw_path_bytes;
                let db_ms = res.db_ms;
                println!("BUILD_TOTAL_MS={build_ms}");
                println!("BUILD_SCAN_MS={}", st.elapsed_ms);
                println!("BUILD_DB_MS={db_ms}");
                println!("DIRS={}", st.dirs);
                println!("FILES={}", st.files);
                println!("BUILD_ERRORS={}", st.errors);
                println!("RAW_PATH_BYTES={raw_bytes}");
            }

            // 冷启动加载测量
            let t1 = Instant::now();
            let conn = db::open(&db_path).expect("open db for load");
            let mut idx = db::load_index(&conn, &db_path).expect("load index");
            let load_ms = t1.elapsed().as_millis();
            idx.build_lookup();
            let opens = db::load_opens(&conn).unwrap_or_default();
            for (p, c) in opens {
                idx.set_open(&p, c);
            }
            let (dirs, files) = idx.counts();
            println!("LOAD_MS={load_ms}");
            println!("IDX_DIRS={dirs}");
            println!("IDX_FILES={files}");

            // 查询延迟
            for q in &queries {
                let mut query = Query::new(q);
                query.limit = 200;
                // 预热
                let _ = idx.search(&query);
                let n = 30;
                let mut times = Vec::with_capacity(n);
                let mut hits = 0usize;
                for _ in 0..n {
                    let t = Instant::now();
                    let r = idx.search(&query);
                    times.push(t.elapsed().as_micros());
                    hits = r.len();
                }
                times.sort();
                let p50 = times[n / 2];
                let p95 = times[((n - 1) * 95) / 100];
                let max = times[n - 1];
                let key: String = q
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
                    .collect();
                println!(
                    "QUERY_{key}_HITS={hits} QUERY_{key}_P50_US={p50} QUERY_{key}_P95_US={p95} QUERY_{key}_MAX_US={max}"
                );
            }

            let db_bytes = db::db_file_size(&db_path);
            println!("DB_BYTES={db_bytes}");
            if raw_bytes == 0 && !has_build {
                // load-only 模式：用当前索引 blob 估算原始路径字节
                raw_bytes = idx.blob_bytes() as u64;
                println!("RAW_PATH_BYTES={raw_bytes}");
            }
            if raw_bytes > 0 {
                let ratio = db_bytes as f64 / raw_bytes as f64;
                println!("DB_RATIO={ratio:.3}");
            }
            println!("RSS_KB={}", rss_kb());
            println!("HWM_KB={}", hwm_kb());
        }
        other => {
            eprintln!("unknown subcommand {other}");
            std::process::exit(2);
        }
    }
}
