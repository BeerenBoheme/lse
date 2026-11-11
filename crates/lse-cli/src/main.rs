//! `lse` 命令行：搜索 / 状态 / 重建。
//!
//! 优先走 D-Bus（`org.lse.Index`，与 GUI 同一后端）；daemon 未运行时降级为
//! **只读打开本地索引库**应答（仍然只查索引、零文件系统遍历）。
//! `--update`：daemon 在 → 远程触发并跟踪进度；daemon 不在 → 本地全量重建。

use lse_core::config::{Config, ExcludeRules};
use lse_core::db;
use lse_core::index::Query;
use lse_core::scan::Progress;
use lse_core::wire::{HitWire, StatusWire};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
lse —— 全盘文件搜索（Linux Search Everything）

用法:
  lse [选项] [关键字...]          子串搜索（大小写不敏感），路径输出到 stdout
  lse --status                    显示索引条目数、最后更新时间、监听状态
  lse --update                    手动重建索引（后台 daemon 在则远程触发，否则本地重建）
  lse --show                      打开图形界面（已运行则唤到前台；全局快捷键降级绑定的目标）

选项:
  --regex <pattern>    正则匹配（匹配完整路径，大小写不敏感）
  --limit <N>          返回条数上限（默认 200；0 = 不限）
  --type <f|d>         只搜文件(f) / 只搜目录(d)
  --ext <pdf,md>       按扩展名过滤（逗号分隔，可带点）
  --path <subdir>      限定在子目录内搜索
  -l, --long           长格式输出：大小、修改时间、类型、路径（tab 分隔）
  -h, --help           显示帮助
  -V, --version        显示版本

示例:
  lse report.pdf                 按文件名搜索
  lse --ext pdf,md 报告          组合过滤
  lse --type d docs              只搜目录
  lse --regex '\\.mkv$'           正则搜索
  lse --path ~/Documents note    限定子目录
  lse --status; lse --update     状态 / 重建
";

struct Args {
    keywords: Vec<String>,
    regex: String,
    limit: u32,
    kind: String,
    exts: String,
    subdir: String,
    long: bool,
    update: bool,
    status: bool,
    show: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        keywords: Vec::new(),
        regex: String::new(),
        limit: 0,
        kind: String::new(),
        exts: String::new(),
        subdir: String::new(),
        long: false,
        update: false,
        status: false,
        show: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("lse {VERSION}");
                return Ok(None);
            }
            "--update" => a.update = true,
            "--status" => a.status = true,
            "--show" => a.show = true,
            "-l" | "--long" => a.long = true,
            "--regex" => a.regex = next_val(&mut it, "--regex")?,
            "--limit" => {
                let v = next_val(&mut it, "--limit")?;
                a.limit = v
                    .parse()
                    .map_err(|_| format!("--limit 需要非负整数，收到: {v}"))?;
            }
            "--type" => {
                let v = next_val(&mut it, "--type")?;
                if v != "f" && v != "d" {
                    return Err(format!("--type 只接受 f 或 d，收到: {v}"));
                }
                a.kind = v;
            }
            "--ext" => a.exts = next_val(&mut it, "--ext")?,
            "--path" => a.subdir = next_val(&mut it, "--path")?,
            other if other.starts_with('-') => {
                return Err(format!("未知选项: {other}（--help 查看用法）"));
            }
            _ => a.keywords.push(arg),
        }
    }
    Ok(Some(a))
}

fn next_val<I: Iterator<Item = String>>(it: &mut I, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} 缺少参数值"))
}

fn main() {
    let args = match parse_args() {
        Ok(None) => return, // help/version 已打印
        Ok(Some(a)) => a,
        Err(e) => {
            eprintln!("lse: {e}");
            std::process::exit(2);
        }
    };

    if args.show {
        std::process::exit(cmd_show());
    }

    let code = futures_lite::future::block_on(async {
        if args.status {
            return cmd_status().await;
        }
        if args.update {
            return cmd_update().await;
        }
        cmd_search(&args).await
    });
    std::process::exit(code);
}

/// 打开图形界面：优先与本可执行文件同目录的 lse-gui（deb/AppImage 都是同目录布局），
/// 回退 PATH 查找。GUI 是单实例（GApplication）——已运行时新进程会把已有窗口唤到前台
/// 后自行退出，这正是 `lse --show` 期望的行为。
fn cmd_show() -> i32 {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let gui = dir.join("lse-gui");
            match std::process::Command::new(&gui).spawn() {
                Ok(_) => {
                    eprintln!("已启动图形界面: {}", gui.display());
                    return 0;
                }
                Err(e) => eprintln!("lse: 同目录启动失败 {}: {e}，尝试 PATH", gui.display()),
            }
        }
    }
    match std::process::Command::new("lse-gui").spawn() {
        Ok(_) => {
            eprintln!("已启动图形界面: PATH 中的 lse-gui");
            0
        }
        Err(e) => {
            eprintln!("lse: 无法启动图形界面（{e}）；请安装 lse-gui 或将其加入 PATH");
            1
        }
    }
}

/// 连接 daemon；None = 服务未运行（可降级）。
async fn try_daemon() -> Option<zbus::Connection> {
    let conn = zbus::Connection::session().await.ok()?;
    let ok = conn
        .call_method(
            Some("org.lse.Index"),
            "/org/lse/Index",
            Some("org.lse.Index"),
            "Ping",
            &(),
        )
        .await
        .is_ok();
    if ok { Some(conn) } else { None }
}

fn is_service_unknown(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::MethodError(n, _, _) if n.contains("ServiceUnknown") || n.contains("NameHasNoOwner"))
}

fn default_limit() -> usize {
    Config::load().query_limit
}

async fn cmd_search(args: &Args) -> i32 {
    let text = args.keywords.join(" ");
    let qtext = text.clone();
    let (limit, kind, exts, subdir, regex) = (
        args.limit,
        args.kind.clone(),
        args.exts.clone(),
        args.subdir.clone(),
        args.regex.clone(),
    );

    // 1) D-Bus 路径
    if let Some(conn) = try_daemon().await {
        let body = (qtext, limit, kind, exts, subdir, regex);
        let res = conn
            .call_method(
                Some("org.lse.Index"),
                "/org/lse/Index",
                Some("org.lse.Index"),
                "Search",
                &body,
            )
            .await;
        match res {
            Ok(msg) => match msg.body().deserialize::<Vec<HitWire>>() {
                Ok(hits) => {
                    print_hits(&hits, args.long);
                    return 0;
                }
                Err(e) => {
                    eprintln!("lse: 解析后端应答失败: {e}");
                    return 1;
                }
            },
            Err(e) if is_service_unknown(&e) => { /* 降级 */ }
            Err(e) => {
                eprintln!("lse: 查询失败: {e}");
                return 1;
            }
        }
    }

    // 2) 降级：只读本地索引（零文件系统遍历）
    match standalone_search(&args) {
        Ok(hits) => {
            print_hits(&hits, args.long);
            0
        }
        Err(e) => {
            eprintln!("lse: {e}");
            eprintln!("提示: 后端未运行时可先执行 `lse --update` 建立索引，或启动 daemon:\n      systemctl --user enable --now lse-daemon");
            1
        }
    }
}

fn build_query(args: &Args) -> Result<Query, String> {
    Query::parse(
        &args.keywords.join(" "),
        args.limit,
        &args.kind,
        &args.exts,
        &args.subdir,
        &args.regex,
        default_limit(),
    )
}

fn standalone_search(args: &Args) -> Result<Vec<HitWire>, String> {
    let db_path = lse_core::config::default_db_path();
    if !db_path.exists() {
        return Err(format!(
            "索引不存在（{}），且 daemon 未运行。",
            db_path.display()
        ));
    }
    let q = build_query(args)?;
    let conn = db::open(&db_path).map_err(|e| format!("打开索引库失败: {e}"))?;
    let mut idx = db::load_index(&conn, &db_path).map_err(|e| format!("加载索引失败: {e}"))?;
    idx.build_lookup();
    if let Ok(opens) = db::load_opens(&conn) {
        for (p, c) in opens {
            idx.set_open(&p, c);
        }
    }
    Ok(idx
        .search(&q)
        .into_iter()
        .map(|h| HitWire {
            path: h.path,
            size: h.size,
            mtime: h.mtime,
            is_dir: h.is_dir,
        })
        .collect())
}

fn print_hits(hits: &[HitWire], long: bool) {
    use std::io::Write;
    let out = std::io::stdout();
    let mut out = out.lock();
    for h in hits {
        if long {
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}",
                h.size,
                fmt_time(h.mtime),
                if h.is_dir { "d" } else { "f" },
                h.path
            );
        } else {
            let _ = writeln!(out, "{}", h.path);
        }
    }
}

fn fmt_time(ts: i64) -> String {
    if ts <= 0 {
        return "-".into();
    }
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&(ts as libc::time_t), &mut tm).is_null() {
            return ts.to_string();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

fn rel_time(ts: i64, now: i64) -> String {
    let d = now - ts;
    if d < 0 {
        return "刚刚".into();
    }
    if d < 60 {
        format!("{d} 秒前")
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n >= K * K * K {
        format!("{:.1} GB", n / (K * K * K))
    } else if n >= K * K {
        format!("{:.1} MB", n / (K * K))
    } else if n >= K {
        format!("{:.1} KB", n / K)
    } else {
        format!("{n:.0} B")
    }
}

async fn cmd_status() -> i32 {
    let now = db::now_unix();
    // 优先 daemon 实时状态
    if let Some(conn) = try_daemon().await {
        let res = conn
            .call_method(
                Some("org.lse.Index"),
                "/org/lse/Index",
                Some("org.lse.Index"),
                "Status",
                &(),
            )
            .await;
        if let Ok(msg) = res {
            if let Ok(st) = msg.body().deserialize::<StatusWire>() {
                println!("后端:   运行中 (v{})", st.version);
                println!("状态:   {}{}", st.phase, if st.building { "（索引构建中）" } else { "" });
                println!(
                    "索引:   {} 目录 + {} 文件（共 {} 条）",
                    st.dirs,
                    st.files,
                    st.dirs + st.files
                );
                if st.phase == "counting" || st.phase == "scanning" {
                    println!(
                        "进度:   {}%（{}/{}，{}）",
                        st.percent,
                        st.done,
                        st.total,
                        if st.eta_secs >= 0 {
                            format!("预计剩余 {} 秒", st.eta_secs)
                        } else {
                            "计算中".into()
                        }
                    );
                }
                if st.last_scan > 0 {
                    println!(
                        "更新:   {}（{}）",
                        fmt_time(st.last_scan),
                        rel_time(st.last_scan, now)
                    );
                } else {
                    println!("更新:   尚未完成首次索引");
                }
                if st.watching {
                    println!(
                        "监听:   inotify 运行中 —— {}/{} 目录已监听（系统上限 {}，预算 {}）；{} 个目录退化为定期重扫",
                        st.watched_dirs, st.watch_needed, st.watch_limit, st.watch_budget, st.unwatched_dirs
                    );
                } else {
                    println!("监听:   未运行");
                }
                if !st.watch_warning.is_empty() {
                    println!("警告:\n{}", st.watch_warning);
                }
                println!("数据库: {}（{}）", st.db_path, fmt_bytes(st.db_bytes));
                return 0;
            }
        }
    }

    // 降级：读磁盘索引
    let db_path = lse_core::config::default_db_path();
    println!("后端:   未运行（systemctl --user enable --now lse-daemon 可设置开机自启）");
    if !db_path.exists() {
        println!("索引:   尚无索引库（daemon 首次启动会自动建立，或执行 lse --update）");
        return 0;
    }
    match db::open(&db_path) {
        Ok(conn) => {
            let (dirs, files) = db::entry_count(&conn).unwrap_or((0, 0));
            let last = db::get_meta(&conn, "last_scan")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            println!("索引:   {dirs} 目录 + {files} 文件（共 {} 条，来自磁盘）", dirs + files);
            if last > 0 {
                println!(
                    "更新:   {}（{}）",
                    fmt_time(last),
                    rel_time(last, now)
                );
            } else {
                println!("更新:   从未完成（可能上次重建被中断）");
            }
        }
        Err(e) => {
            eprintln!("lse: 打开索引库失败: {e}");
            return 1;
        }
    }
    println!(
        "数据库: {}（{}）",
        db_path.display(),
        fmt_bytes(db::db_file_size(&db_path))
    );
    0
}

async fn cmd_update() -> i32 {
    // 1) daemon 在 → 远程触发并跟踪进度
    if let Some(conn) = try_daemon().await {
        let reply = conn
            .call_method(
                Some("org.lse.Index"),
                "/org/lse/Index",
                Some("org.lse.Index"),
                "Rebuild",
                &(),
            )
            .await;
        let started = match reply {
            Ok(m) => m.body().deserialize::<bool>().unwrap_or(true),
            Err(e) => {
                eprintln!("lse: 触发重建失败: {e}");
                return 1;
            }
        };
        eprintln!(
            "lse: 已{}后台重建…",
            if started { "触发" } else { "加入（本就在重建中）" }
        );
        // 跟踪进度直到完成
        let mut building = true;
        let mut last_print = 0i64;
        while building {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let Ok(m) = conn
                .call_method(
                    Some("org.lse.Index"),
                    "/org/lse/Index",
                    Some("org.lse.Index"),
                    "Status",
                    &(),
                )
                .await
            else {
                break;
            };
            let Ok(st) = m.body().deserialize::<StatusWire>() else {
                break;
            };
            building = st.building
                || st.phase == "counting"
                || st.phase == "scanning"
                || st.phase == "loading";
            if building && st.last_scan != last_print && st.total > 0 {
                last_print = st.last_scan;
                eprintln!(
                    "\r[{}%] {}/{} …（预计剩余 {} 秒）        ",
                    st.percent,
                    st.done,
                    st.total,
                    st.eta_secs.max(0)
                );
            }
        }
        eprintln!("\r重建完成。");
        return cmd_status().await;
    }

    // 2) 降级：本地全量重建
    eprintln!("lse: daemon 未运行，本地全量重建…");
    let cfg = Config::load();
    let ex = Arc::new(ExcludeRules::load(&cfg.extra_excludes));
    let db_path = lse_core::config::default_db_path();
    if let Some(p) = db_path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lse: 打开索引库失败: {e}");
            return 1;
        }
    };
    let scanner = Arc::new(lse_core::scan::Scanner::new(cfg, Arc::clone(&ex)));
    let progress = Arc::new(Progress::default());
    match lse_core::build::full_build(&scanner, &conn, &progress) {
        Ok(out) => {
            println!(
                "重建完成: {} 目录 / {} 文件, 耗时 {}ms, 数据库 {}",
                out.stats.dirs,
                out.stats.files,
                out.stats.elapsed_ms,
                fmt_bytes(db::db_file_size(&db_path))
            );
            0
        }
        Err(e) => {
            eprintln!("lse: {e}");
            1
        }
    }
}
