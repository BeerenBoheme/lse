//! lse GUI 进程入口。
//!
//! 架构约束（硬性）：GUI 是纯 D-Bus 客户端——不持有索引、不做文件系统遍历。
//! - 单实例（application_id）：第二次启动 `lse-gui`/`lse --show` 会唤起已有窗口；
//! - 托盘与全局快捷键的回调来自各自线程，通过 50ms 主循环轮询通道回到 GTK 线程；
//! - 点 X 仅隐藏到托盘；真正退出走托盘菜单。

mod file_item;
mod ipc;
mod row;
mod shortcuts;
mod sorters;
mod tray;
mod window;

use gtk::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy)]
enum Msg {
    Show,
    Quit,
}

fn main() {
    // 默认用 cairo 渲染器（仅当用户未显式设置 GSK_RENDERER）：
    // 实测同窗口稳态 RSS 91.6MB/PSS 62MB，而默认 ngl 要加载
    // libLLVM(46MB)+nvidia-gpucomp(27MB) 等 GL 驱动，RSS 195.9MB/PSS 108.9MB。
    // cairo 走纯 CPU 绘制，本应用是纯文本列表无性能损失，且无 GPU 环境
    // （沙箱/虚拟机/Flatpak）同样稳定；需要 GPU 渲染的用户可自行设置该变量。
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }

    // 内存归因对照模式：只建同款空窗口（无托盘/快捷键/后端连接），
    // 用于区分“GTK4+libadwaita 平台基线”与“lse 业务逻辑增量”（阶段四内存验收用）。
    if std::env::var_os("LSE_BASELINE").is_some() {
        let app = adw::Application::builder()
            .application_id("org.lse.Baseline")
            .build();
        app.connect_activate(move |app| {
            let builder = gtk::Builder::from_string(include_str!(concat!(env!("OUT_DIR"), "/window.ui")));
            let window: adw::ApplicationWindow = builder.object("win").expect("#win");
            window.set_application(Some(app));
            window.present();
        });
        let code = app.run();
        std::process::exit(code.into());
    }

    let cfg = lse_core::Config::load();

    let app = adw::Application::builder()
        .application_id("org.lse.Search")
        .build();

    let win_cell: Rc<RefCell<Option<Rc<window::Win>>>> = Rc::new(RefCell::new(None));
    let (tx, rx) = mpsc::channel::<Msg>();

    // ---------------- 托盘（StatusNotifierItem）
    {
        let tx_show = tx.clone();
        let tx_status = tx.clone();
        let tx_quit = tx.clone();
        let cb = tray::Callbacks {
            show: cb_fn(move || {
                let _ = tx_show.send(Msg::Show);
            }),
            rebuild: cb_fn(|| {
                if let Err(e) = ipc::Backend::connect().and_then(|b| b.rebuild()) {
                    eprintln!("[lse-gui] 触发重建失败: {e}");
                }
            }),
            status: cb_fn(move || {
                let _ = tx_status.send(Msg::Show);
            }),
            quit: cb_fn(move || {
                let _ = tx_quit.send(Msg::Quit);
            }),
        };
        if std::env::var_os("LSE_NO_TRAY").is_none() {
            if let Err(e) = tray::start(cb) {
                eprintln!("[lse-gui] 托盘启动失败: {e}");
            }
        }
    }

    // ---------------- 全局快捷键（xdg-desktop-portal GlobalShortcuts）
    if std::env::var_os("LSE_NO_SHORTCUTS").is_none() {
        let tx_show = tx.clone();
        shortcuts::start(
            cfg.hotkey.clone(),
            cb_fn(move || {
                let _ = tx_show.send(Msg::Show);
            }),
        );
    }

    // ---------------- 主循环消息轮询（跨线程回调 → GTK 线程）
    {
        let win_cell = Rc::clone(&win_cell);
        let app = app.clone();
        eprintln!("[lse-dbg] message poll armed");
        gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Show => {
                        eprintln!("[lse-dbg] Msg::Show -> present_clean");
                        if let Some(win) = win_cell.borrow().as_ref() {
                            win.present_clean();
                        }
                    }
                    Msg::Quit => app.quit(),
                }
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    // ---------------- 应用激活：建窗或唤起已有窗
    app.connect_activate(move |app| {
        eprintln!("[lse-dbg] activate entered");
        let mut cell = win_cell.borrow_mut();
        if let Some(win) = cell.as_ref() {
            win.present_clean();
            return;
        }
        let backend = match ipc::Backend::connect() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[lse-gui] {e}");
                eprintln!("[lse-gui] 无法连接会话总线，退出。");
                app.quit();
                return;
            }
        };
        eprintln!("[lse-dbg] backend connected, building window...");
        let win = window::Win::new(app, backend);
        eprintln!("[lse-dbg] window built");
        win.present_clean();
        eprintln!("[lse-dbg] window presented");
        win.selftest();
        *cell = Some(win);
    });

    app.connect_shutdown(|_| eprintln!("[lse-dbg] SHUTDOWN signal"));
    let code = app.run();
    eprintln!("[lse-dbg] app.run() returned {code:?}");
    std::process::exit(code.into());
}

/// tray/shortcuts 回调类型（跨线程安全）。
type RcCallbacks = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;

fn cb_fn(f: impl Fn() + Send + Sync + 'static) -> RcCallbacks {
    std::sync::Arc::new(f)
}
