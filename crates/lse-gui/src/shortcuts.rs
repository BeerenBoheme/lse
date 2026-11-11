//! 全局快捷键：`org.freedesktop.portal.GlobalShortcuts`（X11/Wayland 通用）。
//! 不硬编码 X11 XGrabKey——Wayland 下必须走 portal；portal 不可用（个别旧桌面）
//! 时降级方案见 README（绑定自定义快捷键调用 `lse --show`）。
//!
//! 流程：CreateSession → 等 Response → BindShortcuts(默认 Super+Space) → 等 Response
//!      → 监听 Activated 信号 → 回调（通过 glib idle 回到 GTK 主循环）。

use futures_lite::stream::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Callback = Arc<dyn Fn() + Send + Sync + 'static>;

pub fn start(preferred_trigger: String, on_activate: Callback) {
    std::thread::Builder::new()
        .name("lse-shortcuts".into())
        .spawn(move || {
            if let Err(e) = futures_lite::future::block_on(run(&preferred_trigger, &on_activate)) {
                eprintln!("[lse-gui] 全局快捷键不可用: {e}");
                eprintln!(
                    "[lse-gui] 降级方案：在桌面设置中绑定自定义快捷键执行 `lse --show`（见 README）"
                );
            }
        })
        .expect("spawn shortcuts thread");
}

/// 用户配置的热键（如 "Super+Space"、"Ctrl+Alt+F"）→ portal 规范 trigger。
/// 修饰符白名单（freedesktop shortcuts-spec，KDE `XdgShortcut::parse` 与
/// GNOME `portal_trigger_to_settings` 实现一致）：SHIFT/CAPS/CTRL/ALT/NUM/LOGO，
/// 用 "+" 连接；键名是 xkb keysym 名（两端都大小写不敏感）。未知词原样保留。
fn to_portal_trigger(hotkey: &str) -> String {
    hotkey
        .split('+')
        .map(|seg| {
            let seg = seg.trim();
            match seg.to_ascii_uppercase().as_str() {
                "SUPER" | "WIN" | "META" | "LOGO" | "MOD4" => "LOGO".to_string(),
                "CTRL" | "CONTROL" => "CTRL".to_string(),
                "ALT" => "ALT".to_string(),
                "SHIFT" => "SHIFT".to_string(),
                "NUM" => "NUM".to_string(),
                "CAPS" | "CAPSLOCK" => "CAPS".to_string(),
                _ => seg.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

async fn run(trigger: &str, on_activate: &Callback) -> Result<(), String> {
    // 用户配置写人类可读的 "Super+Space"，portal 规范（freedesktop shortcuts-spec，
    // KDE 的 XdgShortcut::parse 与 GNOME 的 portal_trigger_to_settings 两端白名单一致）
    // 要求大写修饰符 SHIFT/CAPS/CTRL/ALT/NUM/LOGO 加 "+" 连接、键名为 xkb keysym 名。
    // "Super" 不在白名单里会被两端静默解析成空键（按了没反应），必须先转换。
    let trigger = to_portal_trigger(trigger);
    let trigger = trigger.as_str();
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| format!("连接会话总线: {e}"))?;

    // 先建立两条信号流（创建时即向总线注册匹配，避免 Response 信号竞态丢失）
    let resp_rule = zbus::MatchRule::builder()
        .interface("org.freedesktop.portal.Request")
        .map_err(|e| e.to_string())?
        .member("Response")
        .map_err(|e| e.to_string())?
        .build();
    let mut resp_stream = zbus::MessageStream::for_match_rule(resp_rule, &conn, Some(8))
        .await
        .map_err(|e| e.to_string())?;

    let act_rule = zbus::MatchRule::builder()
        .interface("org.freedesktop.portal.GlobalShortcuts")
        .map_err(|e| e.to_string())?
        .member("Activated")
        .map_err(|e| e.to_string())?
        .build();
    let mut act_stream = zbus::MessageStream::for_match_rule(act_rule, &conn, Some(8))
        .await
        .map_err(|e| e.to_string())?;

    let portal = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
    )
    .await
    .map_err(|e| e.to_string())?;

    // 0) 通过 Registry 自报 app id（xdg-desktop-portal ≥1.19 对 host 应用的要求：
    //    未注册时 CreateSession 会报 "An app id is required"；
    //    注册与本 D-Bus 连接绑定，必须先于 CreateSession、且在同一连接上）
    {
        let reg = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.host.portal.Registry",
        )
        .await;
        match reg {
            Ok(proxy) => {
                let ropts: HashMap<&str, Value> = HashMap::new();
                let r: Result<(), _> = proxy
                    .call("Register", &("org.lse.Search", &ropts))
                    .await;
                match r {
                    Ok(()) => eprintln!("[lse-gui] portal 应用标识注册成功 (org.lse.Search)"),
                    Err(e) => {
                        eprintln!("[lse-gui] portal 应用标识注册失败（继续尝试）: {e}")
                    }
                }
            }
            Err(e) => eprintln!("[lse-gui] 未找到 Registry 接口（旧版 portal？）: {e}"),
        }
    }

    // 1) CreateSession（必须携带 session_handle_token——xdg-desktop-portal 1.21 在缺失时
    //    会触发 session->token != NULL 断言而崩溃，属上游缺陷，这里严格按规范传 token）
    let pid = std::process::id();
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert(
        "session_handle_token",
        Value::from(format!("lsesess{pid}")),
    );
    opts.insert("handle_token", Value::from(format!("lsecre{pid}")));
    // CreateSession 返回 request 句柄（用于等待 Response）；真正的 session handle
    // 在 Response 的 results["session_handle"]（字符串）中。
    let (req_create,): (OwnedObjectPath,) = portal
        .call("CreateSession", &(&opts,))
        .await
        .map_err(|e| format!("CreateSession: {e}"))?;
    let results = wait_response(&mut resp_stream, req_create.as_str()).await?;
    let session_str = results
        .get("session_handle")
        .ok_or_else(|| "CreateSession Response 缺少 session_handle".to_string())
        .and_then(|v| {
            String::try_from(&**v).map_err(|e| format!("session_handle 解析失败: {e}"))
        })?;
    let session = OwnedObjectPath::try_from(session_str.as_str())
        .map_err(|e| format!("session_handle 非法: {e}"))?;

    // 2) BindShortcuts（preferred_trigger 例："Super+Space" → 转换后 "LOGO+space"）
    let mut sc_opts: HashMap<&str, Value> = HashMap::new();
    sc_opts.insert("description", Value::from("打开 lse 文件搜索"));
    sc_opts.insert("preferred_trigger", Value::from(trigger));
    let shortcuts: Vec<(String, HashMap<&str, Value>)> =
        vec![("lse-search".to_string(), sc_opts)];
    let mut bind_opts: HashMap<&str, Value> = HashMap::new();
    bind_opts.insert("handle_token", Value::from(format!("lsebind{pid}")));
    // BindShortcuts(o session_handle, a(sa{sv}) shortcuts, s parent_window, a{sv} options)
    let parent_window = "";
    let (req,): (OwnedObjectPath,) = portal
        .call("BindShortcuts", &(&session, &shortcuts, parent_window, &bind_opts))
        .await
        .map_err(|e| format!("BindShortcuts: {e}"))?;
    let bind_results = wait_response(&mut resp_stream, req.as_str()).await?;
    // Response results 里的 shortcuts 是后端**实际接受**的绑定（含解析后的键）。
    // 打出来即可一眼判定 trigger 是否被解析成功——空键/缺 trigger 说明格式仍不对。
    // 注意：KDE 后端这里会先弹一个"添加全局快捷键"确认对话框，用户点"允许"才会成功；
    // 拒绝也会返回 Success 但键为空。
    match bind_results.get("shortcuts") {
        Some(v) => eprintln!("[lse-gui] portal 接受的快捷键: {v:?}"),
        None => eprintln!("[lse-gui] BindShortcuts Response 未携带 shortcuts 字段"),
    }
    eprintln!(
        "[lse-gui] portal 全局快捷键绑定成功 (lse-search={trigger})，等待按下…"
    );

    // 3) 监听 Activated（兼容信号可能发在 session 句柄或 portal 根对象两种路径）
    while let Some(msg) = act_stream.next().await {
        let msg = msg.map_err(|e| e.to_string())?;
        // Activated(session_handle o, shortcut_id s, timestamp t, details a{sv})——
        // portal 实发 4 个顶层参数（早期自测示例只发 3 个，曾漏掉 timestamp 的 t）。
        let body: (OwnedObjectPath, String, u64, HashMap<String, OwnedValue>) =
            match msg.body().deserialize() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[lse-gui] Activated 信号解析失败: {e}");
                    continue;
                }
            };
        eprintln!("[lse-gui] 收到 Activated: shortcut_id={:?}", body.1);
        if body.1 == "lse-search" {
            on_activate();
        }
    }
    eprintln!("[lse-gui] Activated 信号流结束（portal 断开）");
    Ok(())
}

/// 等待指定 request 句柄上的 Response 信号；成功时返回 results
/// （CreateSession 的 results 携带真正的 `session_handle`——方法返回值是 request 句柄）。
async fn wait_response(
    stream: &mut zbus::MessageStream,
    handle: &str,
) -> Result<HashMap<String, OwnedValue>, String> {
    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(|e| e.to_string())?;
        let path = msg
            .header()
            .path()
            .map(|p| p.to_string())
            .unwrap_or_default();
        if path != handle {
            continue;
        }
        let (response, results): (u32, HashMap<String, OwnedValue>) = msg
            .body()
            .deserialize()
            .map_err(|e| format!("Response 解析失败: {e}"))?;
        if response == 0 {
            return Ok(results);
        }
        return Err(format!(
            "portal 拒绝了请求（response={response}，可能被用户或策略拒绝）"
        ));
    }
    Err("总线连接关闭，等待 Response 中断".into())
}
