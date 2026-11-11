//! 内存主索引：路径连续存放（blob）+ 定长元数据数组。
//! 查询全部在内存完成，不触碰文件系统。

use std::collections::HashMap;
use std::sync::RwLock;

/// 单条目元数据（32 字节）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Entry {
    pub off: u32,
    pub len: u32,
    pub depth: u16,
    pub open: u16,
    pub flags: u8, // bit0: is_dir, bit1: tombstone
    pub mtime: i64,
    pub size: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & 1 != 0
    }
    pub fn is_tomb(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// 查询结果（D-Bus 层由 daemon 转换为传输结构）。
#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KindFilter {
    Any,
    File,
    Dir,
}

pub struct Query {
    /// 小写化后的子串（大小写不敏感匹配用）；为空表示无关键字
    pub text: String,
    pub limit: usize,
    pub kind: KindFilter,
    /// 小写扩展名，不含点；空表示不限
    pub exts: Vec<String>,
    /// 限定子目录前缀（小写）
    pub subdir: Option<String>,
    pub regex: Option<regex::Regex>,
}

impl Query {
    pub fn new(text: &str) -> Self {
        Query {
            text: text.to_ascii_lowercase(),
            limit: 200,
            kind: KindFilter::Any,
            exts: Vec::new(),
            subdir: None,
            regex: None,
        }
    }

    /// 从原始参数构造查询（daemon 的 D-Bus 入口与 CLI 共用，保证行为一致）。
    /// - `limit`：0 表示使用 `default_limit`；
    /// - `kind`："" | "f" | "d"；
    /// - `exts`：逗号分隔扩展名（可带点）；
    /// - `subdir`：限定子目录前缀；
    /// - `regex`：正则（空则忽略），大小写不敏感。
    pub fn parse(
        text: &str,
        limit: u32,
        kind: &str,
        exts: &str,
        subdir: &str,
        regex: &str,
        default_limit: usize,
    ) -> Result<Query, String> {
        let mut q = Query::new(text.trim());
        q.limit = if limit == 0 { default_limit } else { limit as usize };
        q.kind = match kind.trim() {
            "f" => KindFilter::File,
            "d" => KindFilter::Dir,
            "" | "a" | "any" => KindFilter::Any,
            other => return Err(format!("无效 --type: {other}（应为 f 或 d）")),
        };
        if !exts.trim().is_empty() {
            q.exts = exts
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if !subdir.trim().is_empty() {
            q.subdir = Some(subdir.trim().trim_end_matches('/').to_ascii_lowercase());
        }
        if !regex.trim().is_empty() {
            q.regex = Some(
                regex::RegexBuilder::new(regex.trim())
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| format!("正则无效: {e}"))?,
            );
        }
        Ok(q)
    }
}

/// 在 haystack 中查找 needle（haystack 侧 ASCII 折小写）；needle 已预小写。
/// 返回首个匹配字节位置。非 ASCII 字节按原样比较（CJK 无大小写，天然正确）。
#[inline]
pub fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let n = needle.len();
    if hay.len() < n {
        return None;
    }
    let last = hay.len() - n;
    // 首字节快筛（needle 已小写，比较时对 haystack 折小写）
    let f = needle[0];
    let mut i = 0;
    while i <= last {
        if hay[i].to_ascii_lowercase() == f {
            let win = &hay[i..i + n];
            // 已确认首字节，逐字节比较剩余部分
            let mut ok = true;
            for k in 1..n {
                if win[k].to_ascii_lowercase() != needle[k] {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[inline]
fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    if hay.len() < needle.len() {
        return false;
    }
    hay[..needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

#[inline]
fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && starts_with_ci(a, b)
}

#[inline]
fn depth_of(path: &str) -> u16 {
    path.bytes().filter(|b| *b == b'/').count() as u16
}

fn path_hash(path: &str) -> u64 {
    // FNV-1a：对短字符串足够快；碰撞在读取时用路径校验兜底。
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 增量更新操作（由 watch 事件或重扫产生）。
#[derive(Clone, Debug)]
pub enum Op {
    Upsert {
        path: String,
        size: u64,
        mtime: i64,
        is_dir: bool,
    },
    Remove {
        path: String,
    },
    /// 整棵子树删除（目录被删/移动出）
    RemoveSubtree {
        prefix: String,
    },
    Opened {
        path: String,
    },
}

pub struct Index {
    blob: Vec<u8>,
    entries: Vec<Entry>,
    by_hash: HashMap<u64, u32>,
    tombstones: u32,
    /// by_hash 是否与 entries 同步（partial+merge 加载后为 false）
    lookup_ready: bool,
}

impl Index {
    pub fn new() -> Self {
        Index {
            blob: Vec::with_capacity(64 << 20),
            entries: Vec::with_capacity(1 << 20),
            by_hash: HashMap::with_capacity(1 << 20),
            tombstones: 0,
            lookup_ready: false,
        }
    }

    pub fn path(&self, e: &Entry) -> &str {
        // 建库期保证 UTF-8 边界（lossy 写入）
        unsafe { std::str::from_utf8_unchecked(&self.blob[e.off as usize..(e.off + e.len) as usize]) }
    }

    pub fn len(&self) -> usize {
        self.entries.len() - self.tombstones as usize
    }

    pub fn raw_len(&self) -> usize {
        self.entries.len()
    }

    pub fn tombstone_count(&self) -> u32 {
        self.tombstones
    }

    /// 预分配容量（冷启动加载前调用，避免反复 realloc）。
    pub fn reserve(&mut self, entries: usize, blob_bytes: usize) {
        self.entries.reserve(entries);
        self.blob.reserve(blob_bytes);
        self.by_hash.reserve(entries);
    }

    /// 合并一个分片构建的 partial index（off 为分片内相对值）。
    /// 合并后 by_hash 需调用 build_lookup 重建（由调用方按需执行）。
    pub fn merge_partial(&mut self, mut blob: Vec<u8>, mut entries: Vec<Entry>) {
        let base = self.blob.len();
        self.blob.append(&mut blob);
        for e in &mut entries {
            e.off = e.off.wrapping_add(base as u32);
        }
        self.entries.append(&mut entries);
    }

    /// 重建路径哈希查找表（增量更新/查询辅助用）。
    /// 冷启动加载走 partial+merge 路径时不在加载线程做，调用方可放后台执行。
    pub fn build_lookup(&mut self) {
        let mut map = HashMap::with_capacity(self.entries.len());
        for (i, e) in self.entries.iter().enumerate() {
            if e.is_tomb() {
                continue;
            }
            let h = path_hash(self.path(e));
            map.entry(h).or_insert(i as u32);
        }
        self.by_hash = map;
        self.lookup_ready = true;
    }

    /// 由一条目构造分片 Entry（供并行加载 worker 使用）。
    pub fn make_entry(off: u32, path: &str, size: u64, mtime: i64, is_dir: bool) -> Entry {
        Entry {
            off,
            len: path.len() as u32,
            depth: depth_of(path),
            open: 0,
            flags: if is_dir { 1 } else { 0 },
            mtime,
            size,
        }
    }

    fn append_inner(&mut self, path: &str, size: u64, mtime: i64, is_dir: bool) {
        let off = self.blob.len() as u32;
        self.blob.extend_from_slice(path.as_bytes());
        let flags = if is_dir { 1u8 } else { 0u8 };
        self.entries.push(Entry {
            off,
            len: path.len() as u32,
            depth: depth_of(path),
            open: 0,
            flags,
            mtime,
            size,
        });
        let idx = (self.entries.len() - 1) as u32;
        self.by_hash.entry(path_hash(path)).or_insert(idx);
        self.lookup_ready = true;
    }

    /// 构建期追加（假定路径唯一；重复路径会被 upsert 语义覆盖为后者）。
    pub fn append(&mut self, path: &str, size: u64, mtime: i64, is_dir: bool) {
        self.append_inner(path, size, mtime, is_dir);
    }

    fn find(&self, path: &str) -> Option<usize> {
        let h = path_hash(path);
        let idx = *self.by_hash.get(&h)? as usize;
        let e = &self.entries[idx];
        if self.path(e) == path {
            return Some(idx);
        }
        // 极小概率哈希碰撞：线性兜底（正确性优先）
        self.entries.iter().position(|e| !e.is_tomb() && self.path(e) == path)
    }

    pub fn upsert(&mut self, path: &str, size: u64, mtime: i64, is_dir: bool) {
        match self.find(path) {
            Some(idx) => {
                let e = &mut self.entries[idx];
                if e.is_tomb() {
                    e.flags &= !2;
                    self.tombstones -= 1;
                }
                e.size = size;
                e.mtime = mtime;
                e.flags = (e.flags & !1) | if is_dir { 1 } else { 0 };
            }
            None => self.append_inner(path, size, mtime, is_dir),
        }
    }

    pub fn remove(&mut self, path: &str) -> bool {
        if let Some(idx) = self.find(path) {
            let e = &mut self.entries[idx];
            if !e.is_tomb() {
                e.flags |= 2;
                self.tombstones += 1;
                return true;
            }
        }
        false
    }

    /// 删除 `prefix` 自身及其下所有条目（目录删除）。
    pub fn remove_subtree(&mut self, prefix: &str) -> u32 {
        let mut n = 0u32;
        let mut pfx = String::with_capacity(prefix.len() + 1);
        pfx.push_str(prefix);
        pfx.push('/');
        let pfx_b = pfx.as_bytes();
        let pre_b = prefix.as_bytes();
        for e in self.entries.iter_mut() {
            if e.is_tomb() {
                continue;
            }
            let path = &self.blob[e.off as usize..(e.off + e.len) as usize];
            if path == pre_b || path == pfx_b || path.starts_with(pfx_b) {
                e.flags |= 2;
                self.tombstones += 1;
                n += 1;
            }
        }
        n
    }

    pub fn bump_open(&mut self, path: &str) {
        if let Some(idx) = self.find(path) {
            let e = &mut self.entries[idx];
            if !e.is_tomb() && e.open < u16::MAX {
                e.open += 1;
            }
        }
    }

    pub fn set_open(&mut self, path: &str, count: u32) {
        if let Some(idx) = self.find(path) {
            let e = &mut self.entries[idx];
            e.open = count.min(u16::MAX as u32) as u16;
        }
    }

    /// 条目当前是否存在于索引（墓碑不算）。
    pub fn contains(&self, path: &str) -> bool {
        self.find(path).map(|i| !self.entries[i].is_tomb()).unwrap_or(false)
    }

    /// 该路径是否已知且是目录。
    pub fn is_dir_path(&self, path: &str) -> bool {
        self.find(path)
            .map(|i| {
                let e = &self.entries[i];
                e.is_dir() && !e.is_tomb()
            })
            .unwrap_or(false)
    }

    /// 条目当前值是否与给定值一致（用于退避重扫的 diff，避免写放大）。
    pub fn matches(&self, path: &str, size: u64, mtime: i64, is_dir: bool) -> bool {
        self.find(path)
            .map(|i| {
                let e = &self.entries[i];
                !e.is_tomb() && e.size == size && e.mtime == mtime && e.is_dir() == is_dir
            })
            .unwrap_or(false)
    }

    /// 应用一批增量操作。`dirty` 为 Some 时（重建进行中）追加脏路径供重放。
    pub fn apply_ops(&mut self, ops: Vec<Op>) {
        for op in ops {
            match op {
                Op::Upsert { path, size, mtime, is_dir } => self.upsert(&path, size, mtime, is_dir),
                Op::Remove { path } => {
                    self.remove(&path);
                }
                Op::RemoveSubtree { prefix } => {
                    self.remove_subtree(&prefix);
                }
                Op::Opened { path } => self.bump_open(&path),
            }
        }
    }

    /// 压缩墓碑（后台执行；完成后整体换回）。
    pub fn compact(&mut self) {
        let mut kept: Vec<Entry> = Vec::with_capacity(self.entries.len());
        let mut blob = Vec::with_capacity(self.blob.len());
        let mut map: HashMap<u64, u32> = HashMap::with_capacity(self.entries.len());
        for e in self.entries.drain(..) {
            if e.is_tomb() {
                continue;
            }
            let path = &self.blob[e.off as usize..(e.off + e.len) as usize];
            let mut ne = e;
            ne.off = blob.len() as u32;
            ne.flags &= !2;
            blob.extend_from_slice(path);
            map.entry(path_hash(std::str::from_utf8(path).unwrap_or("")))
                .or_insert(kept.len() as u32);
            kept.push(ne);
        }
        self.blob = blob;
        self.entries = kept;
        self.by_hash = map;
        self.tombstones = 0;
        self.lookup_ready = true;
    }

    /// 相关度搜索：完全匹配 > 前缀匹配 > 文件名子串 > 路径子串；
    /// 同分按 打开频率降序 > 路径深度升序 > 字节序升序。Top-K 选择，排序全序稳定。
    /// 条目多时按 4 线程分片并行（限流，避免拖慢桌面），分片 top-K 合并后全局截断
    /// （正确性：全局 top-K 必落在各分片 top-K 的并集内）。
    pub fn search(&self, q: &Query) -> Vec<Hit> {
        let n = self.entries.len();
        let threads = if n >= 300_000 { 4 } else { 1 };

        let mut top = if threads == 1 || n == 0 {
            self.search_shard(q, 0, n)
        } else {
            let per = n.div_ceil(threads);
            let parts: Vec<(usize, usize)> = (0..threads)
                .map(|i| (i * per, ((i + 1) * per).min(n)))
                .filter(|(s, e)| *s < *e)
                .collect();
            let mut merged = std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .iter()
                    .map(|&(st, en)| s.spawn(move || self.search_shard(q, st, en)))
                    .collect();
                let mut all = Vec::new();
                for h in handles {
                    all.extend(h.join().unwrap_or_default());
                }
                all
            });
            if q.limit > 0 && merged.len() > q.limit {
                merged.sort_by(|a, b| self.cmp_cand(a, b));
                merged.truncate(q.limit);
            }
            merged
        };

        top.sort_by(|a, b| self.cmp_cand(a, b));
        if q.limit > 0 && top.len() > q.limit {
            top.truncate(q.limit);
        }

        top.into_iter()
            .map(|(_, i)| {
                let e = &self.entries[i as usize];
                Hit {
                    path: self.path(e).to_string(),
                    size: e.size,
                    mtime: e.mtime,
                    is_dir: e.is_dir(),
                }
            })
            .collect()
    }

    /// 单分片扫描：收集 [start,end) 内候选并做片内 top-K。
    fn search_shard(&self, q: &Query, start: usize, end: usize) -> Vec<(u8, u32)> {
        // 空格分词（Everything 习惯）：多个词 AND——每个词都必须命中路径。
        // 首词参与 rank 分级（完全/前缀/文件名/路径段），其余词只做 AND 过滤。
        // 单词（extra 为空）时行为与原先逐字节整串匹配完全一致。
        let needles: Vec<&[u8]> = q.text.split_whitespace().map(|s| s.as_bytes()).collect();
        let (needle, extra): (&[u8], &[&[u8]]) = match needles.split_first() {
            Some((f, rest)) => (*f, rest),
            None => (&[][..], &[][..]),
        };
        let empty = needles.is_empty() && q.regex.is_none();

        let limit = q.limit;
        let unlimited = limit == 0;
        let mut top: Vec<(u8, u32)> =
            Vec::with_capacity(if unlimited { 1024 } else { limit.min(4096) });
        let mut worst: usize = 0;

        let subdir = q.subdir.as_ref().map(|s| s.to_ascii_lowercase());
        let exts: Vec<&str> = q.exts.iter().map(|s| s.as_str()).collect();

        for i in start..end {
            let e = &self.entries[i];
            if e.is_tomb() {
                continue;
            }
            match q.kind {
                KindFilter::Any => {}
                KindFilter::File if e.is_dir() => continue,
                KindFilter::Dir if !e.is_dir() => continue,
                _ => {}
            }
            let path = &self.blob[e.off as usize..(e.off + e.len) as usize];

            let rank: u8 = if empty {
                0
            } else if let Some(re) = &q.regex {
                // 正则：文件名命中 rank2，路径命中 rank3，均不命中则过滤掉
                let name_start = name_start(path);
                let full = std::str::from_utf8(path).unwrap_or("");
                let name = &full[name_start..];
                if re.is_match(name) {
                    2
                } else if re.is_match(full) {
                    3
                } else {
                    continue;
                }
            } else {
                match find_ci(path, needle) {
                    None => continue,
                    Some(pos) => {
                        // 多词 AND：其余词逐个必须在完整路径中命中
                        if extra.iter().any(|t| find_ci(path, t).is_none()) {
                            continue;
                        }
                        let name_start = name_start(path);
                        let name = &path[name_start..];
                        if eq_ci(name, needle) {
                            0
                        } else if starts_with_ci(name, needle) {
                            1
                        } else if pos >= name_start {
                            2
                        } else if find_ci(name, needle).is_some() {
                            2
                        } else {
                            3
                        }
                    }
                }
            };

            if let Some(sb) = &subdir {
                let n = sb.len();
                if !(path.len() >= n
                    && starts_with_ci(&path[..n], sb.as_bytes())
                    && (path.len() == n || path.get(n) == Some(&b'/')))
                {
                    continue;
                }
            }
            if !exts.is_empty() {
                let name_start = name_start(path);
                let name = &path[name_start..];
                let hit = exts.iter().any(|x| {
                    let mut suf = Vec::with_capacity(x.len() + 1);
                    suf.push(b'.');
                    suf.extend_from_slice(x.as_bytes());
                    name.len() >= suf.len()
                        && eq_ci(&path[name_start + name.len() - suf.len()..], &suf)
                });
                if !hit {
                    continue;
                }
            }

            if unlimited {
                top.push((rank, i as u32));
            } else if top.len() < limit {
                top.push((rank, i as u32));
                worst = self.find_worst(&top);
            } else {
                let cand = (rank, i as u32);
                if self.better(&cand, &top[worst]) {
                    top[worst] = cand;
                    worst = self.find_worst(&top);
                }
            }
        }
        top
    }

    /// top 中“最差”元素下标（用于有界选择）。
    fn find_worst(&self, top: &[(u8, u32)]) -> usize {
        let mut w = 0;
        for k in 1..top.len() {
            if self.better(&top[w], &top[k]) {
                w = k;
            }
        }
        w
    }

    /// a 是否优于 b（rank 升、open 降、depth 升、path 字节序升）
    fn better(&self, a: &(u8, u32), b: &(u8, u32)) -> bool {
        self.cmp_cand(a, b) == std::cmp::Ordering::Less
    }

    fn cmp_cand(&self, a: &(u8, u32), b: &(u8, u32)) -> std::cmp::Ordering {
        a.0.cmp(&b.0)
            .then_with(|| {
                let ea = &self.entries[a.1 as usize];
                let eb = &self.entries[b.1 as usize];
                eb.open.cmp(&ea.open) // 降序
            })
            .then_with(|| self.entries[a.1 as usize].depth.cmp(&self.entries[b.1 as usize].depth))
            .then_with(|| {
                let pa = self.path(&self.entries[a.1 as usize]);
                let pb = self.path(&self.entries[b.1 as usize]);
                pa.as_bytes().cmp(pb.as_bytes())
            })
    }

    /// 返回全部目录路径（用于 inotify 预算规划）。
    pub fn all_dirs(&self) -> Vec<String> {
        let mut v = Vec::new();
        for e in &self.entries {
            if e.is_dir() && !e.is_tomb() {
                v.push(self.path(e).to_string());
            }
        }
        v
    }

    pub fn counts(&self) -> (u64, u64) {
        let (mut dirs, mut files) = (0u64, 0u64);
        for e in &self.entries {
            if e.is_tomb() {
                continue;
            }
            if e.is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
        }
        (dirs, files)
    }

    pub fn blob_bytes(&self) -> usize {
        self.blob.len()
    }
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

/// 文件名起始偏移：最后一个 '/' 之后（根路径 "/" 返回 0）。
#[inline]
fn name_start(path: &[u8]) -> usize {
    path.iter().rposition(|b| *b == b'/').map(|i| i + 1).unwrap_or(0)
}

/// 状态快照（供 daemon 的 D-Bus Status 使用）。
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    pub dirs: u64,
    pub files: u64,
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub eta_secs: i64,
    pub last_update: i64,
}

/// 便于 daemon 持有的索引锁包装。
pub type SharedIndex = RwLock<Index>;

#[cfg(test)]
mod tests {
    use super::*;

    fn build(items: &[(&str, bool, u64)]) -> Index {
        let mut idx = Index::new();
        for (p, d, s) in items {
            idx.append(p, *s, 1_700_000_000, *d);
        }
        idx
    }

    fn paths(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn rank_exact_prefix_name_path() {
        let idx = build(&[
            ("/abc/data", false, 1), // 路径段命中（名字 data 无 abc）→ rank3
            ("/data/xabc", false, 1), // 名字子串 → rank2
            ("/data/abcdef", false, 1), // 名字前缀 → rank1
            ("/data/abc", false, 1),    // 完全匹配 → rank0
        ]);
        let q = Query::new("abc");
        let hits = idx.search(&q);
        assert_eq!(
            paths(&hits),
            vec![
                "/data/abc",
                "/data/abcdef",
                "/data/xabc",
                "/abc/data"
            ]
        );
    }

    #[test]
    fn rank_case_insensitive_exact() {
        let idx = build(&[("/dir/AbC.txt", false, 1)]);
        let q = Query::new("abc.TXT");
        let hits = idx.search(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/dir/AbC.txt");
    }

    #[test]
    fn multi_word_and_filters() {
        let idx = build(&[
            ("/docs/年度报告.pdf", false, 1), // 两词都命中
            ("/docs/年度计划.pdf", false, 1), // 只命中“年度”
            ("/docs/报告模板.docx", false, 1), // 只命中“报告”
        ]);
        // 空格分词 AND：必须同时含两个词（顺序无关）
        let q = Query::new("  年度   报告  "); // 多余空白不影响切分
        assert_eq!(paths(&idx.search(&q)), vec!["/docs/年度报告.pdf"]);
        let q2 = Query::new("报告 年度");
        assert_eq!(paths(&idx.search(&q2)), vec!["/docs/年度报告.pdf"]);
        // 换一个词对 → 另一条唯一命中
        let q3 = Query::new("年度 计划");
        assert_eq!(paths(&idx.search(&q3)), vec!["/docs/年度计划.pdf"]);
        // 单词行为不变（整串子串匹配）
        let q4 = Query::new("报告");
        let hits4 = idx.search(&q4);
        let mut r = paths(&hits4);
        r.sort_unstable();
        assert_eq!(r, vec!["/docs/年度报告.pdf", "/docs/报告模板.docx"]);
    }

    #[test]
    fn multi_word_rank_uses_first_word() {
        let idx = build(&[
            ("/r1/qwer alpha.txt", false, 1), // 首词 qwer = 名字前缀 → rank1
            ("/r2/alpha/qwer-x", false, 1),   // 首词 qwer = 名字子串 → rank2
        ]);
        // 分级只看首词：rank1 的排在 rank2 之前（即使次词在 B 中是目录段）
        let q = Query::new("qwer alpha");
        assert_eq!(
            paths(&idx.search(&q)),
            vec!["/r1/qwer alpha.txt", "/r2/alpha/qwer-x"]
        );
    }

    #[test]
    fn tie_open_frequency_before_depth_and_alpha() {
        let mut idx = build(&[("/m/b.txt", false, 1), ("/n/a.txt", false, 1)]);
        // 同 rank2、同 depth：默认字节序 /m < /n
        let q = Query::new("txt");
        assert_eq!(paths(&idx.search(&q)), vec!["/m/b.txt", "/n/a.txt"]);
        // 打开频率优先于字节序
        idx.set_open("/n/a.txt", 7);
        assert_eq!(paths(&idx.search(&q)), vec!["/n/a.txt", "/m/b.txt"]);
    }

    #[test]
    fn tie_depth_before_alpha() {
        // 浅路径字节序靠后，仍应排前（深度优先于字母序）
        let idx = build(&[("/a/b/c/xq", false, 1), ("/z/xq", false, 1)]);
        let q = Query::new("xq");
        assert_eq!(paths(&idx.search(&q)), vec!["/z/xq", "/a/b/c/xq"]);
    }

    #[test]
    fn tie_alpha_final() {
        let idx = build(&[("/b/xq", false, 1), ("/a/xq", false, 1)]);
        let q = Query::new("xq");
        assert_eq!(paths(&idx.search(&q)), vec!["/a/xq", "/b/xq"]);
    }

    #[test]
    fn empty_query_sorted_by_open_then_depth() {
        let mut idx = build(&[("/deep/dir/f1", false, 1), ("/f2", false, 1)]);
        idx.set_open("/deep/dir/f1", 3);
        let q = Query::new("");
        let hits = idx.search(&q);
        assert_eq!(hits[0].path, "/deep/dir/f1"); // 高频优先于深度
    }

    #[test]
    fn limit_truncates() {
        let idx = build(&[
            ("/a/k1", false, 1),
            ("/a/k2", false, 1),
            ("/a/k3", false, 1),
        ]);
        let mut q = Query::new("k");
        q.limit = 2;
        assert_eq!(idx.search(&q).len(), 2);
        q.limit = 0; // 不限
        assert_eq!(idx.search(&q).len(), 3);
    }

    #[test]
    fn kind_filter_dir_only() {
        let idx = build(&[("/a/doc1", true, 1), ("/a/doc2", false, 1)]);
        let mut q = Query::new("doc");
        q.kind = KindFilter::Dir;
        assert_eq!(paths(&idx.search(&q)), vec!["/a/doc1"]);
        q.kind = KindFilter::File;
        assert_eq!(paths(&idx.search(&q)), vec!["/a/doc2"]);
    }

    #[test]
    fn ext_filter() {
        let idx = build(&[
            ("/d/a.pdf", false, 1),
            ("/d/b.md", false, 1),
            ("/d/c.PDF", false, 1),
        ]);
        let mut q = Query::new("");
        q.exts = vec!["pdf".into()];
        assert_eq!(idx.search(&q).len(), 2); // 大小写不敏感
        q.exts = vec!["pdf".into(), "md".into()];
        assert_eq!(idx.search(&q).len(), 3);
    }

    #[test]
    fn subdir_filter_requires_boundary() {
        let idx = build(&[("/home/user2/note", false, 1), ("/home/user/note", false, 1)]);
        let mut q = Query::new("note");
        q.subdir = Some("/home/user".into());
        assert_eq!(paths(&idx.search(&q)), vec!["/home/user/note"]); // user2 不得误命中
    }

    #[test]
    fn regex_filters_and_ranks() {
        let idx = build(&[("/etc/hosts", false, 1), ("/etc/hosts.bak", false, 1)]);
        let mut q = Query::new("");
        q.regex = Some(regex::RegexBuilder::new("^/etc/hosts$").case_insensitive(true).build().unwrap());
        assert_eq!(paths(&idx.search(&q)), vec!["/etc/hosts"]);
        q.regex = Some(regex::RegexBuilder::new(r"\.bak$").case_insensitive(true).build().unwrap());
        assert_eq!(paths(&idx.search(&q)), vec!["/etc/hosts.bak"]);
    }

    #[test]
    fn remove_subtree_removes_prefix_itself() {
        let mut idx = build(&[
            ("/d", true, 0),
            ("/d/sub", true, 0),
            ("/d/sub/f.txt", false, 1),
            ("/dother", true, 0),
        ]);
        idx.remove_subtree("/d");
        assert!(!idx.contains("/d"));
        assert!(!idx.contains("/d/sub"));
        assert!(!idx.contains("/d/sub/f.txt"));
        assert!(idx.contains("/dother")); // 同前缀兄弟不得误删
        let hits = idx.search(&Query::new("f.txt"));
        assert!(hits.is_empty());
    }

    #[test]
    fn find_ci_ascii() {
        assert_eq!(find_ci(b"Hello World", b"world"), Some(6));
        assert_eq!(find_ci(b"Hello", b"lo"), Some(3)); // needle 需预小写
        assert_eq!(find_ci(b"Hello", b"xyz"), None);
        assert_eq!(find_ci(b"abc", b""), Some(0));
        // 中文（无大小写）原样匹配
        assert_eq!(find_ci("文档/报告.pdf".as_bytes(), "报告".as_bytes()), Some(7)); // 字节偏移：文(3)+档(3)+/(1)
    }

    #[test]
    fn upsert_and_delete_roundtrip() {
        let mut idx = build(&[]);
        idx.upsert("/x/new.txt", 5, 100, false);
        assert!(idx.contains("/x/new.txt"));
        idx.upsert("/x/new.txt", 9, 200, false); // 更新
        let hits = idx.search(&Query::new("new.txt"));
        assert_eq!(hits[0].size, 9);
        assert_eq!(hits[0].mtime, 200);
        idx.remove("/x/new.txt");
        assert!(idx.search(&Query::new("new.txt")).is_empty());
        // 再创建（墓碑复活）
        idx.upsert("/x/new.txt", 1, 300, false);
        assert_eq!(idx.search(&Query::new("new.txt")).len(), 1);
    }

    #[test]
    fn query_parse_validation() {
        assert!(Query::parse("a", 10, "x", "", "", "", 200).is_err()); // 非法 type
        assert!(Query::parse("a", 10, "f", "", "", "[bad", 200).is_err()); // 非法正则
        let q = Query::parse(" Hi ", 0, "d", ".PDF, md", "/x/", "", 42).unwrap();
        assert_eq!(q.text, "hi");
        assert_eq!(q.limit, 42); // limit=0 → default
        assert_eq!(q.kind, KindFilter::Dir);
        assert_eq!(q.exts, vec!["pdf", "md"]);
        assert_eq!(q.subdir.as_deref(), Some("/x"));
        let q = Query::parse("a", 7, "", "", "", "", 42).unwrap();
        assert_eq!(q.limit, 7);
    }
}
