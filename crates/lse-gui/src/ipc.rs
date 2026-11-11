//! D-Bus 后端客户端（GUI 是纯客户端：不持有索引、不遍历文件系统）。
//! 采用 zbus 阻塞 API——本机内存索引查询往返 1~5ms，直接在 GTK 主循环调用，
//! 天然满足“每敲一个字符刷新、无中间态”。

use lse_core::wire::{HitWire, StatusWire};
use gtk::prelude::*;
use std::sync::Mutex;

const DST: &str = "org.lse.Index";
const PATH: &str = "/org/lse/Index";
const IFACE: &str = "org.lse.Index";

pub struct Backend {
    conn: Mutex<zbus::blocking::Connection>,
}

impl Backend {
    pub fn connect() -> Result<Backend, String> {
        let conn =
            zbus::blocking::Connection::session().map_err(|e| format!("连接会话总线失败: {e}"))?;
        Ok(Backend {
            conn: Mutex::new(conn),
        })
    }

    fn call<B, T>(&self, method: &str, body: &B) -> Result<T, String>
    where
        B: serde::Serialize + zbus::zvariant::Type,
        T: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        let conn = self.conn.lock().unwrap();
        let msg = conn
            .call_method(Some(DST), PATH, Some(IFACE), method, body)
            .map_err(|e| format!("{method} 调用失败: {e}"))?;
        msg.body()
            .deserialize::<T>()
            .map_err(|e| format!("{method} 应答解析失败: {e}"))
    }

    /// 探活（保留给调试/状态指示灯使用）
    #[allow(dead_code)]
    pub fn ping(&self) -> Result<String, String> {
        self.call("Ping", &())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        text: &str,
        limit: u32,
        kind: &str,
        exts: &str,
        subdir: &str,
        regex: &str,
    ) -> Result<Vec<HitWire>, String> {
        self.call(
            "Search",
            &(
                text.to_string(),
                limit,
                kind.to_string(),
                exts.to_string(),
                subdir.to_string(),
                regex.to_string(),
            ),
        )
    }

    pub fn status(&self) -> Result<StatusWire, String> {
        self.call("Status", &())
    }

    pub fn rebuild(&self) -> Result<bool, String> {
        self.call("Rebuild", &())
    }

    pub fn report_open(&self, path: &str) -> Result<(), String> {
        self.call("ReportOpen", &(path.to_string(),)).map(|_: ()| ())
    }

    /// 启动后端，返回启动方式（供日志/toast 展示）。三级回退：
    /// 1) systemd user service（deb 安装场景，单元存在时）；
    /// 2) 与本 GUI 可执行文件同目录的 lse-daemon——AppImage 挂载目录与
    ///    /usr/local/bin、/usr/bin 都是二进制相邻布局。**裸名走 PATH 在
    ///    AppImage 里必然失败（这正是 Fedora 上后端起不来的根因）**；
    /// 3) PATH 中的裸名（用户手动放进 PATH 的场景）。
    pub fn start_daemon() -> Result<String, String> {
        use std::process::Command;
        let systemctl_ok = Command::new("systemctl")
            .args(["--user", "start", "lse-daemon"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if systemctl_ok {
            return Ok("systemd 服务".to_string());
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let sibling = dir.join("lse-daemon");
                if sibling.is_file() {
                    return match Command::new(&sibling).spawn() {
                        Ok(_) => Ok(format!("随附后端 {}", sibling.display())),
                        Err(e) => Err(format!("启动 {} 失败: {e}", sibling.display())),
                    };
                }
            }
        }
        match Command::new("lse-daemon").spawn() {
            Ok(_) => Ok("PATH 中的 lse-daemon".to_string()),
            Err(e) => Err(format!(
                "启动后端失败: {e}（systemd 服务不存在，同目录与 PATH 均未找到 lse-daemon）"
            )),
        }
    }
}

/// 极简 file:// URI 编码（空格与非 ASCII 需转义）。
pub fn encode_uri_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 8);
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 打开文件/目录（GIO 启动默认应用；这是“打开”，不是搜索遍历，不违反客户端约束）。
/// GIO 对无关联的可执行文件（AppImage、二进制、脚本）会报"无法打开"→ 回退直接执行。
pub fn launch_path(path: &str) -> Result<(), String> {
    let file = gtk::gio::File::for_path(path);
    let uri = file.uri(); // GString, file://...
    let no_ctx: Option<&gtk::gio::AppLaunchContext> = None;
    match gtk::gio::AppInfo::launch_default_for_uri(uri.as_str(), no_ctx) {
        Ok(()) => Ok(()),
        Err(e1) => {
            if let Ok(md) = std::fs::metadata(path) {
                use std::os::unix::fs::PermissionsExt;
                if md.is_file() && md.permissions().mode() & 0o111 != 0 {
                    match std::process::Command::new(path).spawn() {
                        Ok(mut child) => {
                            // 后台收割，避免留僵尸进程
                            std::thread::spawn(move || {
                                let _ = child.wait();
                            });
                            return Ok(());
                        }
                        Err(e2) => return Err(format!("打开失败: {e1}；执行失败: {e2}")),
                    }
                }
            }
            Err(format!("打开失败: {e1}"))
        }
    }
}

/// 打开所在文件夹并选中（org.freedesktop.FileManager1，KDE/GNOME 通用；失败退回打开目录）。
pub fn open_folder_containing(path: &str) -> Result<(), String> {
    let uri = format!("file://{}", encode_uri_path(path));
    let res = std::thread::spawn(move || -> Result<(), String> {
        let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
        conn.call_method(
            Some("org.freedesktop.FileManager1"),
            "/org/freedesktop/FileManager1",
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri], String::new()),
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    })
    .join()
    .unwrap_or_else(|_| Err("线程崩溃".into()));
    match res {
        Ok(()) => Ok(()),
        Err(_) => {
            let parent = std::path::Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".into());
            launch_path(&parent)
        }
    }
}
