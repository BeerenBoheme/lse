//! D-Bus 传输结构（daemon 服务端与 CLI/GUI 客户端共用，保证签名一致）。
//! wire 签名：Search 返回 a(stxb)；Status 为同名字段结构体。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct HitWire {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, zbus::zvariant::Type)]
pub struct StatusWire {
    pub version: String,
    /// loading / counting / scanning / ready / idle
    pub phase: String,
    pub dirs: u64,
    pub files: u64,
    pub done: u64,
    pub total: u64,
    pub percent: u8,
    pub eta_secs: i64,
    pub last_scan: i64,
    pub building: bool,
    pub watching: bool,
    pub watched_dirs: u64,
    pub watch_limit: u64,
    pub watch_budget: u64,
    pub watch_needed: u64,
    pub unwatched_dirs: u64,
    pub watch_warning: String,
    pub db_path: String,
    pub db_bytes: u64,
}
