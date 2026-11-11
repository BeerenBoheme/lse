//! 调试助手：广播一个类型正确的 portal Activated 信号，
//! 用于在没有真实按键的情况下验证 "信号 → 唤起窗口" 链路。
//! 用法：cargo run --release -p lse-gui --example emit_activated

use std::collections::HashMap;
use zbus::zvariant::{OwnedObjectPath, Value};

fn main() {
    let conn = zbus::blocking::Connection::session().expect("session bus");
    let opts: HashMap<&str, Value<'_>> = HashMap::new();
    conn.emit_signal(
        None::<&str>, // 广播（GUI 端 match rule 不过滤 sender）
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
        "Activated",
        &(
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/lsesess_test")
                .expect("path"),
            "lse-search",
            opts,
        ),
    )
    .expect("emit Activated");
    eprintln!("emitted Activated(lse-search)");
}
