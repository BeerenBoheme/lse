//! SQLite 持久层。schema 归一化以满足“索引文件 ≤ 原始路径字节 × 1.5”：
//! - `dirs(id, path, mtime)`：每个目录一行（目录本身也是可搜索条目）；
//! - `files(dir_id, name, size, mtime) WITHOUT ROWID`：只存 basename，
//!   全路径 = dirs.path + '/' + name，避免整条路径重复存储；
//! - `opens(path, count)`：打开频率；
//! - `meta(key, value)`：last_scan、schema 版本等。

use crate::index::Index;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 1;

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn open(path: &Path) -> Result<Connection, rusqlite::Error> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // 索引缓存占用保持小（内存预算 200MB 留给内存主索引）
    conn.pragma_update(None, "cache_size", "-8000")?; // 8MB
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS dirs (
            id    INTEGER PRIMARY KEY,
            path  TEXT NOT NULL UNIQUE,
            mtime INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS files (
            dir_id INTEGER NOT NULL,
            name   TEXT NOT NULL,
            size   INTEGER NOT NULL,
            mtime  INTEGER NOT NULL,
            PRIMARY KEY (dir_id, name)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS opens (
            path  TEXT PRIMARY KEY,
            count INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .optional()
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn entry_count(conn: &Connection) -> Result<(u64, u64), rusqlite::Error> {
    let dirs: i64 = conn.query_row("SELECT COUNT(*) FROM dirs", [], |r| r.get(0))?;
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    Ok((dirs.max(0) as u64, files.max(0) as u64))
}

/// 全量重建：清空后批量写入。整个构建在单事务外分批提交（每 5 万条一提交）。
pub struct Rebuilder<'a> {
    conn: &'a Connection,
    batch: u32,
    /// 目录 id 缓存：walk 按目录连续产出文件，避免逐文件 SELECT
    dir_cache: std::collections::HashMap<String, i64>,
}

impl<'a> Rebuilder<'a> {
    pub fn begin(conn: &'a Connection) -> Result<Self, rusqlite::Error> {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             DELETE FROM files; DELETE FROM dirs; DELETE FROM meta WHERE key='last_scan';",
        )?;
        Ok(Rebuilder {
            conn,
            batch: 0,
            dir_cache: std::collections::HashMap::with_capacity(1024),
        })
    }

    pub fn insert_dir(&mut self, path: &str, mtime: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO dirs(path,mtime) VALUES(?1,?2)
             ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime",
            params![path, mtime],
        )?;
        self.dir_cache.remove(path);
        self.flush_if_needed()?;
        Ok(())
    }

    pub fn insert_file(
        &mut self,
        dir_path: &str,
        name: &str,
        size: u64,
        mtime: i64,
    ) -> Result<(), rusqlite::Error> {
        let dir_id = match self.dir_cache.get(dir_path) {
            Some(id) => *id,
            None => {
                let id: Option<i64> = self
                    .conn
                    .query_row("SELECT id FROM dirs WHERE path=?1", [dir_path], |r| r.get(0))
                    .optional()?;
                match id {
                    Some(id) => {
                        self.dir_cache.insert(dir_path.to_string(), id);
                        id
                    }
                    // 父目录尚未入库：跳过（目录事件总会先到）
                    None => return Ok(()),
                }
            }
        };
        self.conn.execute(
            "INSERT INTO files(dir_id,name,size,mtime) VALUES(?1,?2,?3,?4)
             ON CONFLICT(dir_id,name) DO UPDATE SET size=excluded.size, mtime=excluded.mtime",
            params![dir_id, name, size as i64, mtime],
        )?;
        self.flush_if_needed()?;
        Ok(())
    }

    fn flush_if_needed(&mut self) -> Result<(), rusqlite::Error> {
        self.batch += 1;
        if self.batch >= 50_000 {
            self.conn.execute_batch("COMMIT; BEGIN;")?;
            self.batch = 0;
        }
        Ok(())
    }

    pub fn finish(self, last_scan: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute_batch("COMMIT;")?;
        set_meta(self.conn, "last_scan", &last_scan.to_string())?;
        set_meta(self.conn, "schema_version", &SCHEMA_VERSION.to_string())?;
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

/// 分离全路径为 (dir, name)。
#[inline]
pub fn split_path(full: &str) -> (&str, &str) {
    match full.rfind('/') {
        Some(0) => ("/", &full[1..]),
        Some(i) => (&full[..i], &full[i + 1..]),
        None => ("", full),
    }
}

/// 冷启动加载：dirs 单线程 + files 按 dir_id 范围并行分片构建 partial index，
/// 主线程仅做 blob 拼接；lookup 表由调用方按需后台重建（build_lookup）。
pub fn load_index(conn: &Connection, db_path: &std::path::Path) -> Result<Index, rusqlite::Error> {
    let mut idx = Index::new();
    let mut dir_paths: Vec<String> = Vec::new();
    let mut id_map: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut min_id = i64::MAX;
    let mut max_id = i64::MIN;

    {
        let mut stmt = conn.prepare("SELECT id, path, mtime FROM dirs ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (id, path, mtime) = row?;
            id_map.insert(id, dir_paths.len());
            min_id = min_id.min(id);
            max_id = max_id.max(id);
            dir_paths.push(path.clone());
            idx.append(&path, 0, mtime, true);
        }
    }

    if id_map.is_empty() || max_id < min_id {
        return Ok(idx);
    }

    // 4 个只读连接按 dir_id 范围并行读（files PK=(dir_id,name)，前导列范围扫描走索引），
    // worker 直接构造 blob 分片 + Entry 分片，主线程只做拼接（不做逐条 hash 插入）。
    let threads = 4;
    let total = max_id - min_id + 1;
    let span = ((total + threads as i64 - 1) / threads as i64).max(1);
    let id_map = std::sync::Arc::new(id_map);
    let dir_paths = std::sync::Arc::new(dir_paths);

    let parts: Vec<(Vec<u8>, Vec<crate::index::Entry>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let start = min_id + span * t as i64;
                let end = (start + span).min(max_id + 1);
                let db_path = db_path.to_path_buf();
                let id_map = std::sync::Arc::clone(&id_map);
                let dir_paths = std::sync::Arc::clone(&dir_paths);
                s.spawn(
                    move || -> rusqlite::Result<(Vec<u8>, Vec<crate::index::Entry>)> {
                        let mut blob: Vec<u8> = Vec::with_capacity(32 << 20);
                        let mut entries: Vec<crate::index::Entry> =
                            Vec::with_capacity(1 << 19);
                        if start >= end {
                            return Ok((blob, entries));
                        }
                        let rc = Connection::open_with_flags(
                            &db_path,
                            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                        )?;
                        let mut stmt = rc.prepare(
                            "SELECT dir_id, name, size, mtime FROM files WHERE dir_id >= ?1 AND dir_id < ?2",
                        )?;
                        let rows = stmt.query_map(params![start, end], |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, i64>(3)?,
                            ))
                        })?;
                        for row in rows {
                            let (dir_id, name, size, mtime) = row?;
                            let Some(&di) = id_map.get(&dir_id) else {
                                continue;
                            };
                            let dpath = &dir_paths[di];
                            let off = blob.len();
                            blob.extend_from_slice(dpath.as_bytes());
                            if !dpath.ends_with('/') {
                                blob.push(b'/');
                            }
                            blob.extend_from_slice(name.as_bytes());
                            let path_len = blob.len() - off;
                            let depth =
                                blob[off..].iter().filter(|b| **b == b'/').count() as u16;
                            entries.push(crate::index::Entry {
                                off: off as u32,
                                len: path_len as u32,
                                depth,
                                open: 0,
                                flags: 0,
                                mtime,
                                size: size.max(0) as u64,
                            });
                        }
                        Ok((blob, entries))
                    },
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Ok((Vec::new(), Vec::new()))))
            .collect::<rusqlite::Result<Vec<_>>>()
    })?;

    for (blob, entries) in parts {
        idx.merge_partial(blob, entries);
    }
    Ok(idx)
}

pub fn load_opens(conn: &Connection) -> Result<std::collections::HashMap<String, u32>, rusqlite::Error> {
    let mut map = std::collections::HashMap::new();
    let mut stmt = conn.prepare("SELECT path, count FROM opens")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (p, c) = row?;
        map.insert(p, c.clamp(0, u32::MAX as i64) as u32);
    }
    Ok(map)
}

pub fn bump_open(conn: &Connection, path: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO opens(path,count) VALUES(?1,1)
         ON CONFLICT(path) DO UPDATE SET count=count+1",
        [path],
    )?;
    Ok(())
}

/// 单条增量落库（inotify 事件用）。
pub fn upsert_entry(
    conn: &Connection,
    path: &str,
    size: u64,
    mtime: i64,
    is_dir: bool,
) -> Result<(), rusqlite::Error> {
    let (dir, name) = split_path(path);
    if is_dir {
        conn.execute(
            "INSERT INTO dirs(path,mtime) VALUES(?1,?2)
             ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime",
            params![path, mtime],
        )?;
    } else if dir.is_empty() {
        return Ok(());
    } else {
        let dir_id: Option<i64> = conn
            .query_row("SELECT id FROM dirs WHERE path=?1", [dir], |r| r.get(0))
            .optional()?;
        if let Some(dir_id) = dir_id {
            conn.execute(
                "INSERT INTO files(dir_id,name,size,mtime) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(dir_id,name) DO UPDATE SET size=excluded.size, mtime=excluded.mtime",
                params![dir_id, name, size as i64, mtime],
            )?;
        } else {
            // 父目录不存在：先补目录行
            conn.execute(
                "INSERT INTO dirs(path,mtime) VALUES(?1,?2)
                 ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime",
                params![dir, mtime],
            )?;
            let dir_id: i64 =
                conn.query_row("SELECT id FROM dirs WHERE path=?1", [dir], |r| r.get(0))?;
            conn.execute(
                "INSERT INTO files(dir_id,name,size,mtime) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(dir_id,name) DO UPDATE SET size=excluded.size, mtime=excluded.mtime",
                params![dir_id, name, size as i64, mtime],
            )?;
        }
    }
    Ok(())
}

pub fn remove_entry(conn: &Connection, path: &str, is_dir_known: bool) -> Result<(), rusqlite::Error> {
    if is_dir_known {
        conn.execute("DELETE FROM dirs WHERE path=?1", [path])?;
        // 级联删除其下文件（子目录行会被下面 subtree 清理）
        conn.execute(
            "DELETE FROM files WHERE dir_id IN (SELECT id FROM dirs WHERE path = ?1 || '/' || '')
             AND 0", // 占位，实际用 subtree 函数
            [path],
        )?;
    } else {
        let (dir, name) = split_path(path);
        let dir_id: Option<i64> = conn
            .query_row("SELECT id FROM dirs WHERE path=?1", [dir], |r| r.get(0))
            .optional()?;
        if let Some(dir_id) = dir_id {
            conn.execute(
                "DELETE FROM files WHERE dir_id=?1 AND name=?2",
                params![dir_id, name],
            )?;
        }
    }
    Ok(())
}

/// 目录子树删除（含目录行与所有文件行）。
pub fn remove_subtree(conn: &Connection, prefix: &str) -> Result<(), rusqlite::Error> {
    let like = format!("{}%", escape_like(prefix));
    conn.execute(
        "DELETE FROM files WHERE dir_id IN (
             SELECT id FROM dirs WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'
         )",
        params![prefix, like],
    )?;
    conn.execute(
        "DELETE FROM dirs WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
        params![prefix, like],
    )?;
    Ok(())
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn db_file_size(path: &Path) -> u64 {
    let mut total = 0;
    for ext in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(ext);
        if let Ok(md) = std::fs::metadata(std::path::Path::new(&p)) {
            total += md.len();
        }
    }
    total
}
