//! 托盘图标：自实现 StatusNotifierItem（`org.kde.StatusNotifierItem`）+
//! 菜单（`com.canonical.dbusmenu`），**不引入已废弃的 libappindicator**。
//!
//! - 左键 Activate → 唤起搜索窗；右键 → 桌面原生菜单（重建索引/索引状态/退出）。
//! - KDE Plasma 有 SNI 宿主 → 正常显示；GNOME 默认无 SNI 宿主 → 托盘不可见
//!   （平台限制，主入口是全局快捷键，见 README）。
//! - 回调在 zbus 线程触发，由 main.rs 包装为 glib::idle_add 回到 GTK 主循环。

use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

pub type Callback = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
pub struct Callbacks {
    pub show: Callback,
    pub rebuild: Callback,
    pub status: Callback,
    pub quit: Callback,
}

pub fn start(cb: Callbacks) -> Result<(), String> {
    std::thread::Builder::new()
        .name("lse-tray".into())
        .spawn(move || {
            if let Err(e) = futures_lite::future::block_on(run(cb)) {
                eprintln!("[lse-gui] 托盘不可用: {e}");
            }
        })
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn run(cb: Callbacks) -> Result<(), String> {
    let sni = StatusNotifierItem {
        cb: cb.clone(),
    };
    let menu = DbuSMenu { cb };
    let conn = zbus::connection::Builder::session()
        .map_err(|e| e.to_string())?
        .serve_at("/StatusNotifierItem", sni)
        .map_err(|e| e.to_string())?
        .serve_at("/Menu", menu)
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| e.to_string())?;

    // 向 StatusNotifierWatcher 注册（用连接唯一名，避免与已有实例争抢 well-known 名）
    let unique = conn
        .unique_name()
        .map(|n| n.to_string())
        .unwrap_or_default();
    match zbus::Proxy::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await
    {
        Ok(watcher) => {
            let res: Result<(), _> = watcher
                .call("RegisterStatusNotifierItem", &(unique.as_str(),))
                .await;
            if let Err(e) = res {
                eprintln!("[lse-gui] 托盘注册失败: {e}");
            }
        }
        Err(_) => {
            eprintln!(
                "[lse-gui] 未找到 StatusNotifierWatcher：当前桌面没有托盘宿主\
                 （GNOME 默认如此），托盘图标不可见；请使用全局快捷键"
            );
        }
    }

    std::future::pending::<()>().await;
    Ok(())
}

/// 值装箱辅助：T → Value → OwnedValue。
fn ov<'v, T>(v: T) -> OwnedValue
where
    T: Into<Value<'v>> + zbus::zvariant::DynamicType,
{
    OwnedValue::try_from(Value::new(v)).expect("zvariant 值转换")
}

// ---------------------------------------------------------------- SNI

struct StatusNotifierItem {
    cb: Callbacks,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }

    #[zbus(property)]
    fn id(&self) -> String {
        "lse".into()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        "lse 文件搜索".into()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        // symbolic 图标：随主题着色
        "system-search-symbolic".into()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }

    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        "/Menu".try_into().expect("静态合法对象路径")
    }

    /// 左键单击 → 唤起搜索窗
    fn activate(&self, _x: i32, _y: i32) {
        (self.cb.show)();
    }

    /// 中键 → 同样唤起
    fn secondary_activate(&self, _x: i32, _y: i32) {
        (self.cb.show)();
    }

    /// 右键（宿主不支持 dbusmenu 时的回退）→ 唤起搜索窗
    fn context_menu(&self, _x: i32, _y: i32) {
        (self.cb.show)();
    }

    fn scroll(&self, _delta: i32, _orientation: &str) {
        // 无需滚轮行为
    }
}

// ---------------------------------------------------------------- dbusmenu

type MenuNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

const MENU_ITEMS: [(i32, &str); 3] = [
    (10, "重新构建索引"),
    (20, "索引状态"),
    (30, "退出"),
];

fn item_props(id: i32) -> Option<HashMap<String, OwnedValue>> {
    MENU_ITEMS.iter().find(|(i, _)| *i == id).map(|(_, label)| {
        let mut m: HashMap<String, OwnedValue> = HashMap::new();
        m.insert("label".into(), ov(String::from(*label)));
        m.insert("enabled".into(), ov(true));
        m
    })
}

fn item_node(id: i32) -> OwnedValue {
    let props = item_props(id).expect("已知菜单项");
    ov::<MenuNode>((id, props, Vec::new()))
}

struct DbuSMenu {
    cb: Callbacks,
}

impl DbuSMenu {
    fn dispatch(&self, id: i32) {
        match id {
            10 => (self.cb.rebuild)(),
            20 => (self.cb.status)(),
            30 => (self.cb.quit)(),
            _ => {}
        }
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl DbuSMenu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, MenuNode) {
        let _ = (parent_id, recursion_depth);
        let children: Vec<OwnedValue> = MENU_ITEMS.iter().map(|(id, _)| item_node(*id)).collect();
        (
            1,
            (0, HashMap::new(), children),
        )
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter()
            .filter_map(|id| item_props(id).map(|p| (id, p)))
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        item_props(id)
            .and_then(|mut m| m.remove(&name))
            .ok_or_else(|| zbus::fdo::Error::UnknownProperty(name))
    }

    fn event(&self, id: i32, name: String, _data: OwnedValue, _timestamp: u32) {
        if name == "clicked" {
            self.dispatch(id);
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, name, _, _) in events {
            if name == "clicked" {
                self.dispatch(id);
            }
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<bool>) {
        (Vec::new(), Vec::new())
    }
}
