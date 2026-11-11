//! 配置加载：`$XDG_CONFIG_HOME/lse/config.toml`（主配置）与 `excludes`（排除规则，独立文件）。

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub fn xdg_config_home() -> PathBuf {
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    PathBuf::from(home).join(".config")
}

pub fn xdg_data_home() -> PathBuf {
    if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    PathBuf::from(home).join(".local/share")
}

pub fn config_dir() -> PathBuf {
    xdg_config_home().join("lse")
}

pub fn data_dir() -> PathBuf {
    xdg_data_home().join("lse")
}

pub fn default_db_path() -> PathBuf {
    // LSE_DB 环境变量可覆盖（测试/便携场景）；否则 $XDG_DATA_HOME/lse/index.db
    if let Some(p) = std::env::var_os("LSE_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    data_dir().join("index.db")
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 遍历根目录
    pub roots: Vec<String>,
    /// 建索引线程数（限制开销，避免机器卡顿）
    pub scan_threads: usize,
    /// 默认返回条数上限
    pub query_limit: usize,
    /// 全局快捷键（GUI 使用，如 "Super+Space"）
    pub hotkey: String,
    /// 【已废弃】搜索窗失焦即隐藏。0.1.5 起该功能整体移除、此键不再有任何效果；
    /// 字段仅为兼容旧 config.toml（结构体开了 deny_unknown_fields）而保留。
    pub hide_on_focus_loss: bool,
    /// inotify 监听优先级（预算不足时按此顺序保证）
    pub watch_priority: Vec<String>,
    /// 未监听子树的重扫基准间隔（秒，指数退避上限 4×）
    pub unwatched_rescan_interval: u64,
    /// 全库兜底重扫间隔（秒，0=关闭）
    pub global_rescan_interval: u64,
    /// watch 预算安全系数（0~1，留余量给其他进程）
    pub watch_budget_ratio: f64,
    /// 事件去抖（毫秒）
    pub debounce_ms: u64,
    /// 不进入的文件系统类型（网络盘等）
    pub skip_fs_types: Vec<String>,
    /// 直接内联的额外排除规则（同 excludes 文件语法）
    pub extra_excludes: Vec<String>,
    /// 索引里出现墓碑（删除标记）超过该比例时后台压缩重建
    pub tombstone_compact_ratio: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: vec!["/".into()],
            scan_threads: default_scan_threads(),
            query_limit: 200,
            hotkey: "Super+Space".into(),
            hide_on_focus_loss: true,
            watch_priority: vec![
                "/home".into(),
                "/root".into(),
                "/opt".into(),
                "/srv".into(),
                "/var".into(),
                "/etc".into(),
                "/usr/local".into(),
            ],
            unwatched_rescan_interval: 300,
            global_rescan_interval: 1800,
            watch_budget_ratio: 0.8,
            debounce_ms: 200,
            skip_fs_types: vec![
                "nfs".into(),
                "nfs4".into(),
                "cifs".into(),
                "smb3".into(),
                "smbfs".into(),
                "sshfs".into(),
                "fuse.sshfs".into(),
                "9p".into(),
                "ceph".into(),
                "coda".into(),
                "davfs".into(),
                "davfs2".into(),
                "afp".into(),
                "afpfs".into(),
                "gpfs".into(),
                "lustre".into(),
                "orangefs".into(),
                "fuse.gvfsd-fd".into(),
                "fuse.portal".into(),
                "fuse.dlna".into(),
            ],
            extra_excludes: vec![],
            tombstone_compact_ratio: 0.25,
        }
    }
}

fn default_scan_threads() -> usize {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    n.clamp(2, 8)
}

impl Config {
    /// 读取配置；不存在则创建默认文件（文件系统只读时忽略失败，直接用默认值）。
    pub fn load() -> Config {
        let dir = config_dir();
        let path = dir.join("config.toml");
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str::<Config>(&s).unwrap_or_else(|e| {
                eprintln!("[lse] config.toml 解析失败({e})，使用默认配置");
                Config::default()
            }),
            Err(_) => {
                let default = Config::default();
                let _ = std::fs::create_dir_all(&dir);
                if let Ok(s) = toml::to_string_pretty(&default) {
                    let _ = std::fs::write(&path, s);
                }
                default
            }
        };
        if cfg.scan_threads == 0 {
            cfg.scan_threads = 1;
        }
        cfg
    }
}

/// 排除规则。语法（每行一条，`#` 起始为注释）：
/// - 以 `/` 开头：整棵子树排除（如 `/proc`）；
/// - 含 `/`：按路径组件序列匹配（如 `.git/objects`）；
/// - 含通配符 `*`/`?`：对文件/目录名匹配（如 `*.pyc`）；
/// - 其他：任一路径组件同名即排除（如 `node_modules`）。
#[derive(Debug, Default, Clone)]
pub struct ExcludeRules {
    abs: Vec<String>,
    seqs: Vec<Vec<String>>,
    globs: Vec<String>,
    comps: Vec<String>,
}

impl ExcludeRules {
    /// 从规则行构造（单元测试与 load 共用）。
    pub fn from_lines<I, S>(lines: I) -> ExcludeRules
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut r = ExcludeRules::default();
        for line in lines {
            let line = line.as_ref().trim().trim_end_matches('/').to_string();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('/') {
                r.abs.push(line);
            } else if line.contains('/') {
                r.seqs.push(line.split('/').map(|s| s.to_string()).collect());
            } else if line.contains('*') || line.contains('?') {
                r.globs.push(line);
            } else {
                r.comps.push(line);
            }
        }
        r
    }

    pub fn load(extra_inline: &[String]) -> ExcludeRules {
        let mut lines: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
        let path = config_dir().join("excludes");
        if let Ok(s) = std::fs::read_to_string(&path) {
            for line in s.lines() {
                let line = line.trim();
                if !line.is_empty() && !line.starts_with('#') {
                    lines.push(line.to_string());
                }
            }
        } else {
            let _ = std::fs::create_dir_all(config_dir());
            let mut content = String::from(
                "# lse 排除规则（每行一条）。语法：\n\
                 #   /xxx      -> 排除整棵子树\n\
                 #   a/b       -> 路径组件序列匹配\n\
                 #   *.pyc     -> 名字通配匹配\n\
                 #   node_modules -> 同名组件匹配\n",
            );
            for d in DEFAULT_EXCLUDES {
                content.push_str(d);
                content.push('\n');
            }
            let _ = std::fs::write(&path, content);
        }
        for e in extra_inline {
            lines.push(e.clone());
        }
        ExcludeRules::from_lines(lines)
    }

    #[inline]
    pub fn is_excluded(&self, path: &str) -> bool {
        for p in &self.abs {
            if path == p.as_str() {
                return true;
            }
            if path.len() > p.len()
                && path.starts_with(p.as_str())
                && path.as_bytes()[p.len()] == b'/'
            {
                return true;
            }
        }
        // 组件同名 / 通配匹配：逐组件快速检查
        let mut comps_iter = path.split('/').filter(|c| !c.is_empty());
        // 跳过开头的空组件（绝对路径以 / 开头）
        if path.starts_with('/') {
            // split('/') 首项为空字符串，已被 filter 过滤
        }
        let mut components: Vec<&str> = Vec::with_capacity(8);
        while let Some(c) = comps_iter.next() {
            for pat in &self.comps {
                if c == pat {
                    return true;
                }
            }
            for g in &self.globs {
                if glob_match(g.as_bytes(), c.as_bytes()) {
                    return true;
                }
            }
            components.push(c);
        }
        // 组件序列匹配（如 .git/objects）
        for seq in &self.seqs {
            if contains_seq(&components, seq) {
                return true;
            }
        }
        false
    }

    pub fn abs_roots(&self) -> impl Iterator<Item = &String> {
        self.abs.iter()
    }
}

fn contains_seq(hay: &[&str], needle: &[String]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    'outer: for start in 0..=(hay.len() - needle.len()) {
        for (i, n) in needle.iter().enumerate() {
            if hay[start + i] != n {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

/// 极简 glob：仅支持 `*`（任意长度）与 `?`（单字符），针对文件名。
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while n < name.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = n;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// 默认排除：伪文件系统、临时目录、常见缓存/构建产物。
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/tmp",
    "node_modules",
    ".git/objects",
    "__pycache__",
    "*.pyc",
    "*.o",
    ".cache",
    ".npm",
    ".venv",
    "target",
    "*.swp",
    "*.tmp",
    "lost+found",
];

/// 需要跳过的文件系统类型集合（从 Config 构造）。
pub fn skip_fs_set(cfg: &Config) -> HashSet<String> {
    cfg.skip_fs_types.iter().cloned().collect()
}

pub fn ensure_dir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ExcludeRules {
        ExcludeRules::from_lines([
            "/proc",
            "/tmp",
            "node_modules",
            ".git/objects",
            "__pycache__",
            "*.pyc",
            "*.o",
            "# 注释行",
        ])
    }

    #[test]
    fn abs_prefix_excludes_subtree_only_at_boundary() {
        let ex = rules();
        assert!(ex.is_excluded("/proc"));
        assert!(ex.is_excluded("/proc/self"));
        assert!(ex.is_excluded("/proc/self/status"));
        assert!(!ex.is_excluded("/procfoo")); // 不得前缀误伤
        assert!(!ex.is_excluded("/home/user/proc"));
    }

    #[test]
    fn component_match_any_depth() {
        let ex = rules();
        assert!(ex.is_excluded("/home/u/app/node_modules"));
        assert!(ex.is_excluded("/home/u/app/node_modules/lodash/index.js"));
        assert!(ex.is_excluded("/opt/x/__pycache__/a.pyc"));
        assert!(!ex.is_excluded("/home/u/node_modules_backup/x"));
    }

    #[test]
    fn seq_match_dot_git_objects() {
        let ex = rules();
        assert!(ex.is_excluded("/home/u/repo/.git/objects"));
        assert!(ex.is_excluded("/home/u/repo/.git/objects/ab/cdef"));
        assert!(!ex.is_excluded("/home/u/repo/.git/config")); // .git 其余部分不排除
        assert!(!ex.is_excluded("/home/u/.gitx/objects"));
    }

    #[test]
    fn glob_matches_basename() {
        let ex = rules();
        assert!(ex.is_excluded("/build/main.pyc"));
        assert!(ex.is_excluded("/build/main.o"));
        assert!(!ex.is_excluded("/build/main.py"));
        assert!(!ex.is_excluded("/build/a.pycx"));
    }

    #[test]
    fn comments_and_slash_tolerated() {
        let ex = ExcludeRules::from_lines(["#c", "", "/var/cache/"]);
        assert!(ex.is_excluded("/var/cache/apt"));
        assert!(!ex.is_excluded("/var/lib"));
    }

    #[test]
    fn glob_match_engine() {
        assert!(glob_match(b"*.pyc", b"a.pyc"));
        assert!(glob_match(b"*.pyc", b".pyc"));
        assert!(!glob_match(b"*.pyc", b"a.pyc.bak"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"ac"));
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"exact", b"exact"));
        assert!(!glob_match(b"exact", b"exactly"));
    }

    #[test]
    fn config_default_scan_threads_capped() {
        let t = Config::default().scan_threads;
        assert!((1..=8).contains(&t), "scan_threads 应限制在 1..=8，实际 {t}");
    }
}
