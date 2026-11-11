//! 搜索窗：实时过滤（每字符一次后端查询、无中间态）、三列结果列表
//! （名称/完整路径/修改时间，可点列头排序）、双击或回车打开、右键菜单、
//! 拖拽导出、失焦隐藏、索引状态栏与首次建索引进度。
//!
//! 本进程是纯 D-Bus 客户端：不持有索引、不遍历文件系统。

use crate::file_item::FileItem;
use crate::ipc;
use crate::row::Row;
use crate::sorters::{self, KeySorter};
use gtk::glib;
use gtk::prelude::*;
use gtk::{gdk, gio};
use libadwaita as adw;
use lse_core::wire::HitWire;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

const UI: &str = include_str!(concat!(env!("OUT_DIR"), "/window.ui"));

#[derive(Clone)]
struct Ctx {
    path: String,
    is_dir: bool,
}

pub struct Win {
    pub window: adw::ApplicationWindow,
    builder: gtk::Builder,
    backend: ipc::Backend,
    store: gio::ListStore,
    ctx: Rc<RefCell<Ctx>>,
    search_gen: Rc<Cell<u64>>,
    hide_gen: Rc<Cell<u32>>,
    /// 唤起宽限（毫秒单调时钟）：present 后 1s 内合成器还没把焦点给到窗口时不许
    /// 失焦隐藏——否则快捷键唤起的窗口"闪一下就没了"，用户观感=唤起未发生
    hide_grace_ms: Rc<Cell<i64>>,
    connected: Rc<Cell<bool>>,
    building: Rc<Cell<bool>>,
    err: Rc<RefCell<String>>,
    /// 上次成功查询的文本（refresh 幂等：GTK SearchEntry 会重复发出 search-changed，
    /// 文本未变则不重复查询/填充——否则每次重复 fill 都会让模型层吃掉几十 MB）
    last_query: Rc<RefCell<String>>,
    /// 上次状态栏文本（变化才 set_text，避免无效重绘）
    last_status: Rc<RefCell<String>>,
    hide_on_focus_loss: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum CellKind {
    Name,
    Path,
    Mtime,
}

impl Win {
    pub fn new(
        app: &impl gtk::prelude::IsA<gtk::Application>,
        backend: ipc::Backend,
        hide_on_focus_loss: bool,
    ) -> Rc<Win> {
        let builder = gtk::Builder::from_string(UI);
        let window: adw::ApplicationWindow = builder.object("win").expect("blp: #win");
        let view: gtk::ColumnView = builder.object("column_view").expect("blp: #column_view");
        let store = gio::ListStore::new::<FileItem>();
        let ctx = Rc::new(RefCell::new(Ctx {
            path: String::new(),
            is_dir: false,
        }));
        // 右键菜单：整个视图共享一个 PopoverMenu。若在 cell 工厂里每格各建
        // 一个，200行×3列=600套 popover+stack+scrolledwindow+scrollbar+按钮，
        // 实测占 +70MB（内存元凶）——必须共享。
        let menu_popover = gtk::PopoverMenu::from_model(Some(&make_menu_model()));
        menu_popover.set_parent(&view);

        let name_col = gtk::ColumnViewColumn::new(
            Some("名称"),
            Some(make_factory(CellKind::Name, &view, &store, &ctx, &menu_popover)),
        );
        if std::env::var_os("LSE_NOSORT").is_none() {
            name_col.set_sorter(Some(&KeySorter::new(sorters::cmp_name)));
        }
        name_col.set_resizable(true);

        let path_col = gtk::ColumnViewColumn::new(
            Some("完整路径"),
            Some(make_factory(CellKind::Path, &view, &store, &ctx, &menu_popover)),
        );
        if std::env::var_os("LSE_NOSORT").is_none() {
            path_col.set_sorter(Some(&KeySorter::new(sorters::cmp_path)));
        }
        path_col.set_resizable(true);
        path_col.set_expand(true);

        let mtime_col = gtk::ColumnViewColumn::new(
            Some("修改时间"),
            Some(make_factory(CellKind::Mtime, &view, &store, &ctx, &menu_popover)),
        );
        if std::env::var_os("LSE_NOSORT").is_none() {
            mtime_col.set_sorter(Some(&KeySorter::new(sorters::cmp_mtime)));
        }
        mtime_col.set_resizable(true);

        if std::env::var("LSE_COL1").is_ok() {
            // 单列对照：视图层成本是否按列数放大
            view.append_column(&name_col);
        } else {
            view.append_column(&name_col);
            view.append_column(&path_col);
            view.append_column(&mtime_col);
        }
        // 排序链：ColumnView 点列头只会更新 view.sorter() 状态，真正排序要靠
        // SortListModel 应用它（直接 MultiSelection(ListStore) 时点列头无任何
        // 反应——用户实测"排序不可用"的根因）。取 item 一律走 view.model()
        // （选择模型=排序后视图坐标），不能用 store.item(视图pos)（错位）。
        let sort_model = gtk::SortListModel::new(Some(store.clone()), view.sorter());
        view.set_model(Some(&gtk::MultiSelection::new(Some(sort_model))));

        // 列头三态循环：升 → 降 → 取消（用户需求）。
        // GTK 4.22 内置点击只有二态——gtk_column_view_sorter_add_column 对同列
        // 仅翻转 inverted（升↔降），永远回不到未排序（取证：
        // .bench/gtk4src/gtk-4.22.4/gtk/gtkcolumnviewsorter.c:294 add_column）。
        // 拦截规则：监听 sorter 的 changed，若同列 primary 由 降→升 翻转
        // （= 用户第三次点击），改调 sort_by_column(None)（C 源码注释：
        // "If column is unset, the view will be unsorted"）取消排序；之后再点
        // 同列回到"首次插入升序"，形成 升→降→取消→升… 循环，与列头箭头
        // （升↑ 降↓ 取消=无箭头）视觉自洽。
        // 实现注：GtkColumnViewSorter 的 Rust 类型被 gtk4 crate 的 v4_10 feature
        // 门控（启用会破坏 Row 子类的 Accessible 层级断言），改走 GObject 属性
        // 按名读取 primary-sort-column / primary-sort-order（运行时属性查询，
        // 与编译期 feature 无关）；闭包持 WeakRef<ColumnView> 防引用循环。
        if let Some(base_sorter) = view.sorter() {
            let weak_view = view.downgrade();
            let last: Rc<RefCell<Option<(gtk::ColumnViewColumn, gtk::SortType)>>> =
                Rc::new(RefCell::new(None));
            let busy: Rc<Cell<bool>> = Rc::new(Cell::new(false));
            let (last2, busy2) = (last.clone(), busy.clone());
            let _ = base_sorter.connect_local("changed", false, move |values| {
                if busy2.get() {
                    return None; // 防重入（clear 自身会再触发 changed）
                }
                busy2.set(true);
                let Ok(s) = values[0].get::<gtk::Sorter>() else {
                    busy2.set(false);
                    return None;
                };
                let col = s.property::<Option<gtk::ColumnViewColumn>>("primary-sort-column");
                let ord = s.property::<gtk::SortType>("primary-sort-order");
                let prev = last2.borrow().clone();
                // 判定：本列 primary 且当前升序，且上一状态是同列降序
                // （GTK 内置点击对同列只在 升↔降 翻转，该序列唯一来源是
                // 用户第三次点击同列）
                let trigger = match (&col, ord, &prev) {
                    (Some(c), gtk::SortType::Ascending, Some((lc, gtk::SortType::Descending))) => {
                        c == lc
                    }
                    _ => false,
                };
                if trigger {
                    if let Some(v) = weak_view.upgrade() {
                        v.sort_by_column(None, gtk::SortType::Ascending); // = 取消排序
                    }
                    *last2.borrow_mut() = None;
                } else {
                    *last2.borrow_mut() = col.map(|c| (c, ord));
                }
                busy2.set(false);
                None
            });
        }

        // 点击列头排序后视口回顶部：GTK 默认保持滚动条像素位置/原焦点项，
        // 换序后用户看到的还是列表中段（用户反馈“追随排序前项目显示”）。
        // Sorter::changed 在列头状态切换（升/降/无序）时发出；SortListModel
        // 的重排是异步的，所以立即置顶一次，再补 100ms 兜底——防 GTK 排完后
        // 的 scroll-to-focus 又把视口拉回原焦点项。
        // 闭包只持有 Adjustment（不持 Win），避免窗口对象引用循环。
        if let (Some(sc), Some(sorter)) = (
            builder.object::<gtk::ScrolledWindow>("results_scroll"),
            view.sorter(),
        ) {
            let vadj = sc.vadjustment();
            let _ = sorter.connect_local("changed", false, move |_| {
                vadj.set_value(0.0);
                let v = vadj.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                    v.set_value(0.0);
                });
                None
            });
        }

        // 窗口必须关联到 GtkApplication：否则应用在 activate 返回后因
        // “没有属于本应用的窗口”而自动退出（表现为窗口闪现即消失）。
        window.set_application(Some(app));

        // 窗口/任务栏图标：用随包安装的 lse-search（hicolor/scalable/apps）。
        // 先查主题里有没有——开发态直跑 target/release 时图标未安装，
        // 不设可避免 GTK 找不到图标的告警（此时沿用桌面文件推导的默认图标）。
        {
            // window.display() 在 adw::ApplicationWindow 上有 RootExt/WidgetExt 两个
            // 同名候选（E0034），显式限定 WidgetExt 版本。
            let display = WidgetExt::display(&window);
            let theme = gtk::IconTheme::for_display(&display);
            if theme.has_icon("lse-search") {
                window.set_icon_name(Some("lse-search"));
            }
        }

        let win = Rc::new(Win {
            window,
            builder,
            backend,
            store,
            ctx,
            search_gen: Rc::new(Cell::new(0)),
            hide_gen: Rc::new(Cell::new(0)),
            hide_grace_ms: Rc::new(Cell::new(0)),
            connected: Rc::new(Cell::new(false)),
            building: Rc::new(Cell::new(false)),
            err: Rc::new(RefCell::new(String::new())),
            last_query: Rc::new(RefCell::new(String::new())),
            last_status: Rc::new(RefCell::new(String::new())),
            hide_on_focus_loss,
        });
        win.wire();
        win
    }

    fn wire(self: &Rc<Self>) {
        let sl = Rc::clone(self);

        // ---- 实时过滤：每敲一个字符 debounce 80ms 后即时查询（无 spinner/中间态）
        let entry: gtk::SearchEntry = self.builder.object("search_entry").expect("#search_entry");
        {
            let sl = Rc::clone(&sl);
            entry.connect_search_changed(move |_| {
                let g = sl.search_gen.get() + 1;
                sl.search_gen.set(g);
                let sl2 = Rc::clone(&sl);
                glib::timeout_add_local_once(Duration::from_millis(80), move || {
                    if sl2.search_gen.get() == g {
                        sl2.refresh();
                    }
                });
            });
        }
        // 回车：打开选中项（无选中则第一条）
        {
            let sl = Rc::clone(&sl);
            entry.connect_activate(move |_| sl.open_activated());
        }

        // 双击 / 列表内回车激活（pos 是排序后视图坐标 → 走选择模型取 item）
        {
            let sl = Rc::clone(&sl);
            let view: gtk::ColumnView = self.builder.object("column_view").expect("#column_view");
            view.connect_activate(move |view, pos| {
                if let Some(item) = view
                    .model()
                    .and_then(|m| m.item(pos))
                    .and_downcast::<FileItem>()
                {
                    sl.open_path(&item.path(), item.is_dir());
                }
            });
        }

        // ---- 操作按钮：后端在 → 重建索引；后端不在 → 启动后端
        {
            let sl = Rc::clone(&sl);
            let btn: gtk::Button = self.builder.object("rebuild_button").expect("#rebuild_button");
            btn.set_visible(true);
            btn.connect_clicked(move |b| {
                if sl.connected.get() {
                    match sl.backend.rebuild() {
                        Ok(true) => sl.toast("已触发后台重建索引"),
                        Ok(false) => sl.toast("索引正在重建中…"),
                        Err(e) => sl.toast(&format!("触发重建失败: {e}")),
                    }
                } else {
                    // 手动启动：把启动方式/失败原因直接告诉用户
                    match ipc::Backend::start_daemon() {
                        Ok(how) => {
                            b.set_label("启动中…");
                            sl.toast(&format!("正在启动后端（{how}）…"));
                        }
                        Err(e) => sl.toast(&format!("{e}")),
                    }
                }
            });
        }

        // ---- 失焦隐藏（可配置；300ms 延迟避免右键菜单/瞬时失焦误伤）
        {
            let sl = Rc::clone(&sl);
            self.window
                .connect_notify_local(Some("is-active"), move |win, _| {
                    let g = sl.hide_gen.get() + 1;
                    sl.hide_gen.set(g);
                    if win.is_active() {
                        return;
                    }
                    Self::arm_hide_check(Rc::clone(&sl), g, 300);
                });
        }

        // ---- 点 X = 隐藏到后台（托盘/快捷键可再次唤起；真正退出走托盘菜单）
        self.window.connect_close_request(|win| {
            eprintln!("[lse-dbg] close-request -> hide");
            win.hide();
            glib::Propagation::Stop
        });

        // ---- 生命周期诊断
        {
            let win = self.window.clone();
            win.connect_destroy(move |_| eprintln!("[lse-dbg] window DESTROYED"));
        }
        self.window
            .connect_unmap(|w| eprintln!("[lse-dbg] window unmapped (mapped={})", w.is_mapped()));
        self.window.connect_realize(|_| eprintln!("[lse-dbg] window realized"));

        // ---- 动作组（右键菜单目标）
        {
            let group = gio::SimpleActionGroup::new();
            let a_open = gio::SimpleAction::new("open", None);
            {
                let sl = Rc::clone(&sl);
                a_open.connect_activate(move |_, _| {
                    let c = sl.ctx.borrow().clone();
                    if !c.path.is_empty() {
                        sl.open_path(&c.path, c.is_dir);
                    }
                });
            }
            let a_folder = gio::SimpleAction::new("open-folder", None);
            {
                let sl = Rc::clone(&sl);
                a_folder.connect_activate(move |_, _| {
                    let c = sl.ctx.borrow().clone();
                    if c.path.is_empty() {
                        return;
                    }
                    if let Err(e) = ipc::open_folder_containing(&c.path) {
                        sl.toast(&e);
                    }
                });
            }
            let a_copy = gio::SimpleAction::new("copy-path", None);
            {
                let sl = Rc::clone(&sl);
                a_copy.connect_activate(move |_, _| {
                    let c = sl.ctx.borrow().clone();
                    if !c.path.is_empty() {
                        sl.window.clipboard().set_text(&c.path);
                        sl.toast("路径已复制");
                    }
                });
            }
            group.add_action(&a_open);
            group.add_action(&a_folder);
            group.add_action(&a_copy);
            self.window.insert_action_group("win", Some(&group));
        }

        // ---- 状态刷新定时器（建索引中 500ms，平时 2s）
        arm_status_timer(&sl);

        // 初始查询（空查询 → 后端按打开频率/深度返回默认列表）+ 初始状态
        sl.refresh_inner(true);
        sl.refresh_status();

        // 后端未运行时自动拉起：AppImage 场景没有 systemd 服务，GUI 与
        // lse-daemon 相邻（同目录回退）；deb 场景等价 systemctl --user start。
        // 双击 AppImage 即可全自启，无需用户点按钮。失败只记日志——
        // 状态轮询（500ms）会继续尝试连接，按钮仍可手动重试。
        if !sl.connected.get() {
            match ipc::Backend::start_daemon() {
                Ok(how) => {
                    eprintln!("[lse-gui] 后端未运行，自动启动: {how}");
                    sl.set_button_label("启动中…");
                }
                Err(e) => eprintln!("[lse-gui] 自动启动后端失败: {e}"),
            }
        }
    }

    // ------------------------------------------------------------ 查询

    fn refresh(&self) {
        self.refresh_inner(false);
    }

    fn refresh_inner(&self, force: bool) {
        if std::env::var_os("LSE_NO_SEARCH").is_some() {
            return;
        }
        let text = self.entry_text();
        if !force && *self.last_query.borrow() == text {
            return; // 文本未变：GTK 重复信号导致的空刷新直接忽略
        }
        let dbg_limit: u32 = std::env::var("LSE_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match self.backend.search(&text, dbg_limit, "", "", "", "") {
            Ok(hits) => {
                self.connected.set(true);
                self.err.borrow_mut().clear();
                *self.last_query.borrow_mut() = text;
                self.fill(&hits);
            }
            Err(e) => {
                self.connected.set(false);
                *self.err.borrow_mut() = e;
                self.store.remove_all();
            }
        }
    }

    fn fill(&self, hits: &[HitWire]) {
        memlog("fill:begin");
        let items: Vec<FileItem> = hits
            .iter()
            .map(|h| {
                let name = h.path.rsplit('/').next().unwrap_or(h.path.as_str());
                FileItem::new(&h.path, name, h.mtime, &fmt_time(h.mtime), h.is_dir)
            })
            .collect();
        // 一次 splice 完成“清空+批量填充”：ListStore 只发一次 items-changed，
        // ColumnView 只做一次重排/重绑。逐条 append 会让视图层每条响应一次
        // （实测每条 ~70KB 分配，200 条 ≈ 70MB/轮）。
        memlog(&format!("fill:after-items({})", items.len()));
        let n = self.store.n_items();
        self.store.splice(0, n, &items);
        // 注意：换词后**不**强制置顶——用户实测反馈"搜索效果不如原来的好"
        //（打字浏览时视口被反复跳回顶部很打扰）；只保留列头排序置顶。
        if std::env::var("LSE_FILLTEST").ok().as_deref() == Some("7") {
            self.window.hide();
            memlog("fill7:已隐藏窗口");
        }
        memlog(&format!("fill:after-splice({})", items.len()));
        if std::env::var_os("LSE_MEMDUMP").is_some() {
            memdump(&self.window);
        }
        // LSE_FILLLOOP=N：重复 splice N 次，检验内存是否逐次累积（防“每敲一键涨几MB”）
        if let Ok(n) = std::env::var("LSE_FILLLOOP").unwrap_or_default().parse::<u32>() {
            let cnt = self.store.n_items();
            for i in 0..n {
                self.store.splice(0, cnt, &items);
                memlog(&format!("fillloop{i}"));
            }
        }

        if std::env::var("LSE_FILLTEST").ok().as_deref() == Some("8") {
            // 单个 FileItem 持有成本斜率：1个 vs 200个
            let keep1: Vec<FileItem> = hits
                .iter()
                .take(1)
                .map(|h| FileItem::new(&h.path, "x", 0, "-", false))
                .collect();
            memlog("filltest8:keep=1");
            let mut keep: Vec<FileItem> = Vec::new();
            for h in hits.iter().take(199) {
                keep.push(FileItem::new(&h.path, "y", 0, "-", false));
            }
            let _ = (keep1, keep);
            memlog("filltest8:keep=200");
        }
        if std::env::var("LSE_FILLTEST").ok().as_deref() == Some("9") {
            // 游离 ColumnView：同款三列结构但不挂入窗口树，
            // 隔离“ColumnView 实例本身的响应”与“窗口树/渲染集成”的成本。
            let st = gio::ListStore::new::<FileItem>();
            let mk = |key: &'static str| {
                let f = gtk::SignalListItemFactory::new();
                f.connect_setup(|_, o| {
                    let li = o.downcast_ref::<gtk::ListItem>().unwrap();
                    li.set_child(Some(&gtk::Label::new(None)));
                });
                f.connect_bind(move |_, o| {
                    let li = o.downcast_ref::<gtk::ListItem>().unwrap();
                    let it = li.item().and_downcast::<FileItem>().unwrap();
                    let lb = li.child().and_downcast::<gtk::Label>().unwrap();
                    let text = if key == "p" {
                        it.path()
                    } else {
                        it.name()
                    };
                    lb.set_text(&text);
                });
                f
            };
            let cv = gtk::ColumnView::new(Some(gtk::MultiSelection::new(Some(
                st.clone(),
            ))));
            cv.append_column(&gtk::ColumnViewColumn::new(Some("n"), Some(mk("n"))));
            cv.append_column(&gtk::ColumnViewColumn::new(Some("p"), Some(mk("p"))));
            cv.append_column(&gtk::ColumnViewColumn::new(Some("m"), Some(mk("m"))));
            let items2: Vec<FileItem> = hits
                .iter()
                .take(200)
                .map(|h| {
                    let name = h.path.rsplit('/').next().unwrap_or(h.path.as_str());
                    FileItem::new(&h.path, name, h.mtime, &fmt_time(h.mtime), h.is_dir)
                })
                .collect();
            memlog("f9:before");
            st.splice(0, st.n_items(), &items2);
            memlog("f9:after-detached-splice");
            std::mem::forget(cv);
        }
        if std::env::var("LSE_FILLTEST").ok().as_deref() == Some("6") {
            let orphan = gio::ListStore::new::<FileItem>();
            for _ in 0..3 {
                let items2: Vec<FileItem> = hits
                    .iter()
                    .take(200)
                    .map(|h| {
                        let name = h.path.rsplit('/').next().unwrap_or(h.path.as_str());
                        FileItem::new(&h.path, name, h.mtime, "-", h.is_dir)
                    })
                    .collect();
                orphan.splice(0, orphan.n_items(), &items2);
            }
            memlog("filltest6:游离ListStore splice x3(不挂视图)");
        }
    }

    fn refresh_status(&self) {
        if std::env::var_os("LSE_NO_STATUS").is_some() {
            return;
        }
        memlog("status:begin");
        match self.backend.status() {
            Ok(st) => {
                memlog("status:after-call");
                self.connected.set(true);
                self.err.borrow_mut().clear();
                let building =
                    st.phase == "counting" || st.phase == "scanning" || st.phase == "loading";
                self.building.set(building);

                let progress_box: gtk::Box = self.builder.object("progress_box").expect("blp");
                let bar: gtk::ProgressBar = self.builder.object("progress_bar").expect("blp");
                let plabel: gtk::Label = self.builder.object("progress_label").expect("blp");
                if building {
                    progress_box.show();
                    bar.set_fraction((st.percent as f64 / 100.0).clamp(0.0, 1.0));
                    // 扫描期间目录里出现新文件时 done 可能略超预统计 total，
                    // 分子 clamp 防止显示 1605305/1605304 这种倒挂；
                    // done>=total 时直接短路成"即将完成"——就算后端进程
                    // 还在跑带下溢 bug 的旧版本（ETA 算出天文数字）也不会显示出来。
                    let shown_done = st.done.min(st.total);
                    let eta_part = if st.total > 0 && shown_done >= st.total {
                        " · 即将完成".to_string()
                    } else {
                        match st.eta_secs {
                            n if n > 0 => format!(" · 预计剩余 {}", fmt_eta(n)),
                            0 => " · 即将完成".to_string(),
                            _ => String::new(),
                        }
                    };
                    plabel.set_text(&if st.total > 0 && st.phase == "scanning" {
                        format!("正在建索引 {}/{}{}", shown_done, st.total, eta_part)
                    } else {
                        "正在统计文件数量…".to_string()
                    });
                } else {
                    progress_box.hide();
                }

                let label: gtk::Label = self.builder.object("status_label").expect("blp");
                let icon: gtk::Image = self.builder.object("backend_icon").expect("blp");
                let new_text = if st.last_scan > 0 {
                    format!(
                        "已索引 {} 个文件 · 最后更新 {}",
                        st.files,
                        rel_time(st.last_scan, db_now())
                    )
                } else if building {
                    "首次索引进行中…".to_string()
                } else {
                    "尚无索引（等待首次构建）".to_string()
                };
                // 文本未变不 set_text：避免每 2s 一次的无效重绘/重排
                if *self.last_status.borrow() != new_text {
                    *self.last_status.borrow_mut() = new_text.clone();
                    icon.set_icon_name(Some("emblem-ok-symbolic"));
                    label.set_text(&new_text);
                }
                if st.watch_warning.is_empty() {
                    label.set_tooltip_text(None);
                } else {
                    label.set_tooltip_text(Some(&st.watch_warning));
                }
                self.set_button_label("重建索引");
            }
            Err(e) => {
                self.connected.set(false);
                self.building.set(false);
                *self.err.borrow_mut() = e;
                let progress_box: gtk::Box = self.builder.object("progress_box").expect("blp");
                progress_box.hide();
            }
        }
        // 连接错误优先显示（含启动引导）
        let err = self.err.borrow().clone();
        if !err.is_empty() {
            let label: gtk::Label = self.builder.object("status_label").expect("blp");
            let icon: gtk::Image = self.builder.object("backend_icon").expect("blp");
            icon.set_icon_name(Some("dialog-warning-symbolic"));
            label.set_text(&format!("后端未运行（{err}）——点击右侧按钮启动"));
            label.set_tooltip_text(Some(&err));
            self.set_button_label("启动后端");
        }
    }

    fn set_button_label(&self, text: &str) {
        let btn: gtk::Button = self.builder.object("rebuild_button").expect("blp");
        if btn.label().as_deref() != Some(text) {
            btn.set_label(text);
        }
    }

    // ------------------------------------------------------------ 打开

    /// 失焦隐藏检查（generation 校验防误伤）；处于唤起宽限期内时延后重查——
    /// Wayland 下 present 后合成器可能晚几百毫秒才给焦点，不能提前隐藏。
    fn arm_hide_check(sl: Rc<Win>, g: u32, delay_ms: u64) {
        glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
            if !sl.hide_on_focus_loss || sl.hide_gen.get() != g || sl.window.is_active() {
                return;
            }
            let now_ms = glib::monotonic_time() / 1_000_000;
            if now_ms < sl.hide_grace_ms.get() {
                Self::arm_hide_check(sl.clone(), g, 150);
                return;
            }
            eprintln!("[lse-dbg] hide (focus loss)");
            sl.window.hide();
        });
    }

    pub fn present_clean(self: &Rc<Self>) {
        // 唤起宽限：1s 内不因失焦隐藏（防"闪一下就没了"）
        self.hide_grace_ms
            .set(glib::monotonic_time() / 1_000_000 + 1_000);
        let entry: gtk::SearchEntry = self.builder.object("search_entry").expect("blp");
        entry.set_text("");
        self.window.present();
        self.refresh_inner(true);
        entry.grab_focus();
    }

    /// LSE_SELFTEST=1：程序化设置列头排序并校验视图顺序，
    /// 无需人工点鼠标即可验证 SortListModel 排序链。
    pub fn selftest(self: &Rc<Self>) {
        if std::env::var_os("LSE_SELFTEST").is_none() {
            return;
        }
        let sl = Rc::clone(self);
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            // 黑屏诊断：dump 关键容器的分配尺寸（0x0 = 布局问题，非 0 = 渲染问题）
            for name in ["header_bar", "root_box", "toast_overlay", "content_box", "results_scroll", "column_view"] {
                match sl.builder.object::<gtk::Widget>(name) {
                    Some(w) => eprintln!(
                        "[selftest] size {name} = ({}, {})",
                        w.size(gtk::Orientation::Horizontal),
                        w.size(gtk::Orientation::Vertical)
                    ),
                    None => eprintln!("[selftest] size {name} = MISSING"),
                }
            }
            eprintln!(
                "[selftest] size window = ({}, {})",
                sl.window.size(gtk::Orientation::Horizontal),
                sl.window.size(gtk::Orientation::Vertical)
            );
            let view: gtk::ColumnView = sl.builder.object("column_view").expect("#column_view");
            let mut before: Vec<String> = Vec::new();
            for i in 0..sl.store.n_items().min(5) {
                if let Some(it) = sl.store.item(i).and_downcast::<FileItem>() {
                    before.push(it.path());
                }
            }
            eprintln!("[selftest] store 前5（相关度序）: {before:?}");
            // 模拟点列头：ColumnViewColumn.sort-order 属性只读，
            // 程序化排序走 sort_by_column（与用户点击同一内部路径）
            if let Some(col) = view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
                view.sort_by_column(Some(&col), gtk::SortType::Ascending);
                eprintln!("[selftest] 已设置名称列升序");
            }
            let sl2 = Rc::clone(&sl);
            glib::timeout_add_local_once(Duration::from_millis(400), move || {
                let view: gtk::ColumnView =
                    sl2.builder.object("column_view").expect("#column_view");
                let Some(model) = view.model() else {
                    eprintln!("[selftest] FAIL: view.model() 为空");
                    return;
                };
                let mut after: Vec<String> = Vec::new();
                for i in 0..model.n_items().min(8) {
                    if let Some(it) = model.item(i).and_downcast::<FileItem>() {
                        after.push(it.path());
                    }
                }
                let key = |p: &String| {
                    p.rsplit('/')
                        .next()
                        .unwrap_or(p)
                        .to_lowercase()
                };
                let ok = after.windows(2).all(|w| key(&w[0]) <= key(&w[1]));
                eprintln!("[selftest] 视图前8（名称升序后）: {after:?}");
                eprintln!("[selftest] 排序校验: {}", if ok { "PASS" } else { "FAIL" });

                // ---- 三态循环自检：升(已设) → 降 → 第三次"点击"(升)应被拦截为取消。
                // GLib 信号同步派发，每步调用返回时状态已就绪，无需延迟。
                // 状态经 GObject 属性读取（ColumnViewSorter 类型有 v4_10 门控）。
                let Some(base) = view.sorter() else {
                    eprintln!("[selftest] 三态校验: FAIL(无 sorter)");
                    return;
                };
                let state_desc = |s: &gtk::Sorter| -> String {
                    let name = s
                        .property::<Option<gtk::ColumnViewColumn>>("primary-sort-column")
                        .and_then(|x| x.title())
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "<none>".into());
                    let ord: gtk::SortType = s.property("primary-sort-order");
                    format!("{name}/{ord:?}")
                };
                eprintln!("[selftest] 三态① 升: primary={}", state_desc(&base));
                let name_col = view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>();
                if let Some(col) = &name_col {
                    view.sort_by_column(Some(col), gtk::SortType::Descending);
                }
                eprintln!("[selftest] 三态② 降: primary={}", state_desc(&base));
                if let Some(col) = &name_col {
                    view.sort_by_column(Some(col), gtk::SortType::Ascending);
                }
                let cleared = base
                    .property::<Option<gtk::ColumnViewColumn>>("primary-sort-column")
                    .is_none();
                eprintln!(
                    "[selftest] 三态③ 第三次点击: primary={} → {}",
                    state_desc(&base),
                    if cleared { "已取消" } else { "仍排序中" }
                );
                eprintln!(
                    "[selftest] 三态校验: {}",
                    if cleared { "PASS" } else { "FAIL" }
                );
            });
        });
    }

    fn open_activated(&self) {
        if let Some(model) = self.current_selection() {
            let n = model.n_items();
            for i in 0..n {
                if model.is_selected(i) {
                    if let Some(it) = model.item(i).and_downcast::<FileItem>() {
                        self.open_path(&it.path(), it.is_dir());
                        return;
                    }
                }
            }
            if let Some(it) = model.item(0).and_downcast::<FileItem>() {
                self.open_path(&it.path(), it.is_dir());
            }
        }
    }

    fn current_selection(&self) -> Option<gtk::SelectionModel> {
        let view: gtk::ColumnView = self.builder.object("column_view").expect("#column_view");
        view.model()
    }

    fn open_path(&self, path: &str, _is_dir: bool) {
        match ipc::launch_path(path) {
            Ok(()) => {
                // 打开频率记录（供排序权重）
                let _ = self.backend.report_open(path);
            }
            Err(e) => self.toast(&e),
        }
    }

    // ------------------------------------------------------------ 杂项

    fn entry_text(&self) -> String {
        let entry: gtk::SearchEntry = self.builder.object("search_entry").expect("#search_entry");
        entry.text().to_string()
    }

    fn toast(&self, msg: &str) {
        let overlay: adw::ToastOverlay = self.builder.object("toast_overlay").expect("blp");
        overlay.add_toast(adw::Toast::new(msg));
    }
}

fn arm_status_timer(sl: &Rc<Win>) {
    let delay = if sl.building.get() || !sl.connected.get() {
        500
    } else {
        2000
    };
    let sl2 = Rc::clone(sl);
    glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        memlog("timer:entry");
        sl2.refresh_status();
        memlog("timer:after-status");
        arm_status_timer(&sl2);
        memlog("timer:rearmed");
    });
}

// ---------------------------------------------------------------- 列工厂

fn make_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("打开"), Some("win.open"));
    menu.append(Some("打开所在文件夹"), Some("win.open-folder"));
    menu.append(Some("复制路径"), Some("win.copy-path"));
    menu
}

fn make_factory(
    kind: CellKind,
    view: &gtk::ColumnView,
    store: &gio::ListStore,
    ctx: &Rc<RefCell<Ctx>>,
    menu: &gtk::PopoverMenu,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    // ---------------- setup：构建行控件 + 手势/拖拽
    {
        let view = view.clone();
        let ctx = Rc::clone(ctx);
        let popover = menu.clone(); // 共享 popover（见 Win::new 注释），不克隆整棵菜单树
        factory.connect_setup(move |_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let row = Row::new();

            match kind {
                CellKind::Name => {
                    let image = gtk::Image::new();
                    let label = gtk::Label::new(None);
                    label.set_xalign(0.0);
                    label.set_hexpand(true);
                    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                    row.append(&image);
                    row.append(&label);
                }
                CellKind::Path | CellKind::Mtime => {
                    let label = gtk::Label::new(None);
                    label.set_xalign(0.0);
                    label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
                    row.append(&label);
                }
            }

            // 右键菜单（共享 popover，不在此处 per-cell 创建）
            let click = gtk::GestureClick::new();
            // GIR GestureSingle："By default gestures react to both GDK_BUTTON_PRIMARY
            // and touch events" —— 不 set_button(0) 则只听主键，右键(3)永远进不来。
            click.set_button(0);
            {
                let row = row.clone();
                let view = view.clone();
                let ctx = Rc::clone(&ctx);
                let popover = popover.clone();
                click.connect_pressed(move |gesture, n_press, x, y| {
                    if n_press != 1 || gesture.current_button() != 3 {
                        return;
                    }
                    let p = row.pos();
                    if p == u32::MAX {
                        return;
                    }
                    // item 从选择模型取（排序后视图坐标），store.item(pos) 会错位
                    let item = view
                        .model()
                        .and_then(|m| m.item(p))
                        .and_downcast::<FileItem>();
                    if let Some(model) = view.model() {
                        if !model.is_selected(p) {
                            model.unselect_all();
                            model.select_item(p, true);
                        }
                    }
                    if let Some(item) = item {
                        *ctx.borrow_mut() = Ctx {
                            path: item.path(),
                            is_dir: item.is_dir(),
                        };
                    }
                    // popover 的父级是 view（共享），点击坐标相对 row → 换算
                    let (vx, vy) = row
                        .compute_point(&view, &gtk::graphene::Point::new(x as f32, y as f32))
                        .map(|p| (p.x(), p.y()))
                        .unwrap_or((x as f32, y as f32));
                    popover
                        .set_pointing_to(Some(&gdk::Rectangle::new(vx as i32, vy as i32, 1, 1)));
                    popover.popup();
                });
            }
            row.add_controller(click);

            // 名称列拖拽（text/uri-list）
            if kind == CellKind::Name {
                let ds = gtk::DragSource::new();
                ds.set_actions(gdk::DragAction::COPY);
                let row_d = row.clone();
                let view = view.clone();
                ds.connect_prepare(move |_, _, _| {
                    let p = row_d.pos();
                    if p == u32::MAX {
                        return None;
                    }
                    let mut paths: Vec<String> = Vec::new();
                    if let Some(model) = view.model() {
                        let n = model.n_items();
                        if p < n && model.is_selected(p) {
                            for i in 0..n {
                                if model.is_selected(i) {
                                    if let Some(it) = model.item(i).and_downcast::<FileItem>() {
                                        paths.push(it.path());
                                    }
                                }
                            }
                        } else if let Some(it) = model.item(p).and_downcast::<FileItem>() {
                            paths.push(it.path());
                        }
                    }
                    if paths.is_empty() {
                        return None;
                    }
                    let mut bytes: Vec<u8> = Vec::new();
                    for p in &paths {
                        bytes.extend_from_slice(
                            format!("file://{}\r\n", ipc::encode_uri_path(p)).as_bytes(),
                        );
                    }
                    Some(gdk::ContentProvider::for_bytes(
                        "text/uri-list",
                        &glib::Bytes::from_owned(bytes),
                    ))
                });
                row.add_controller(ds);
            }

            li.set_child(Some(&row));
        });
    }

    // ---------------- bind：同步位置与文本
    {
        let store = store.clone();
        factory.connect_bind(move |_, obj| {
            let li = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let Some(row) = li.child().and_downcast::<Row>() else {
                return;
            };
            row.set_pos(li.position());
            let Some(item) = li.item().and_downcast::<FileItem>() else {
                return;
            };
            match kind {
                CellKind::Name => {
                    if let Some(img) = row.first_child().and_downcast::<gtk::Image>() {
                        img.set_icon_name(Some(if item.is_dir() {
                            "folder-symbolic"
                        } else {
                            "text-x-generic-symbolic"
                        }));
                    }
                    if let Some(lab) = row.last_child().and_downcast::<gtk::Label>() {
                        lab.set_text(&item.name());
                        lab.set_tooltip_text(Some(&item.path()));
                    }
                }
                CellKind::Path => {
                    if let Some(lab) = row.first_child().and_downcast::<gtk::Label>() {
                        lab.set_text(&item.path());
                    }
                }
                CellKind::Mtime => {
                    if let Some(lab) = row.first_child().and_downcast::<gtk::Label>() {
                        lab.set_text(&item.mtime_text());
                    }
                }
            }
            let _ = &store;
        });
    }

    factory
}

// ---------------------------------------------------------------- 时间格式化


fn memlog(tag: &str) {
    if std::env::var_os("LSE_MEMLOG").is_none() {
        return;
    }
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let rss = s
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("?")
        .to_string();
    // 验收按 PSS（按比例分摊共享页），RssAnon/RssFile 拆分归因
    let roll = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let pick = |key: &str| -> String {
        roll.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("?")
            .to_string()
    };
    eprintln!(
        "[lse-mem] {tag}: VmRSS={rss} kB Pss={} kB Anon={} kB File={} kB",
        pick("Pss:"),
        pick("RssAnon:"),
        pick("RssFile:")
    );
}

/// LSE_MEMDUMP=1 取证：递归数窗口 widget/label 数量 + smaps 按映射聚合 top8。
/// 目的：判断 fill 的 +70MB 是"全部行都被 realize 成 widget"还是匿名堆增长。
fn memdump(win: &adw::ApplicationWindow) {
    fn walk(
        w: &gtk::Widget,
        total: &mut usize,
        labels: &mut usize,
        types: &mut std::collections::BTreeMap<String, usize>,
        depth: usize,
        max_depth: &mut usize,
    ) {
        *total += 1;
        *max_depth = (*max_depth).max(depth);
        let t = w.type_().name().to_string();
        *types.entry(t).or_insert(0) += 1;
        if w.downcast_ref::<gtk::Label>().is_some() {
            *labels += 1;
        }
        let ch = w.observe_children();
        let n = ch.n_items();
        for i in 0..n {
            if let Some(c) = ch
                .item(i)
                .and_then(|o| o.downcast::<gtk::Widget>().ok())
            {
                walk(&c, total, labels, types, depth + 1, max_depth);
            }
        }
    }
    let (mut total, mut labels, mut max_depth) = (0usize, 0usize, 0usize);
    let mut types: std::collections::BTreeMap<String, usize> = Default::default();
    walk(
        win.clone().upcast::<gtk::Widget>().as_ref(),
        &mut total,
        &mut labels,
        &mut types,
        0,
        &mut max_depth,
    );

    // glibc 在用量：区分“真持有对象”与“分配churn后未归还OS的碎片”
    // mallinfo2 无 uordblks；在用量 ≈ arena + hblkhd − fordblks
    let mi = unsafe { libc::mallinfo2() };
    eprintln!(
        "[lse-dump] widgets={total} labels={labels} depth={max_depth} \
         mallinfo2 in-use={} kB freed={} kB arena={} kB hblkhd={} kB",
        (mi.arena + mi.hblkhd - mi.fordblks) / 1024,
        mi.fordblks / 1024,
        mi.arena / 1024,
        mi.hblkhd / 1024
    );
    let mut tv: Vec<_> = types.into_iter().collect();
    tv.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, n) in tv.into_iter().take(12) {
        eprintln!("[lse-dump]   {n:>6}  {k}");
    }

    let mut agg: std::collections::BTreeMap<String, u64> = Default::default();
    let mut cur = String::from("<other>");
    if let Ok(txt) = std::fs::read_to_string("/proc/self/smaps") {
        for line in txt.lines() {
            let is_hdr = line
                .chars()
                .next()
                .map(|c| c.is_ascii_hexdigit())
                .unwrap_or(false)
                && line.contains('-')
                && line.split_whitespace().count() >= 5;
            if is_hdr {
                cur = line
                    .splitn(6, ' ')
                    .nth(5)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "<anon>".into());
            } else if let Some(rest) = line.strip_prefix("Rss:") {
                let kb: u64 = rest
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                *agg.entry(cur.clone()).or_insert(0) += kb;
            }
        }
    }
    eprintln!("[lse-dump] widgets={total} labels={labels}");
    let mut v: Vec<_> = agg.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, kb) in v.into_iter().take(8) {
        eprintln!("[lse-dump] {kb:>10} kB  {k}");
    }
}

fn db_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

fn fmt_eta(secs: i64) -> String {
    if secs > 7 * 86400 {
        // 溢出兜底（历史上 total-done 曾 u64 下溢出 344 亿小时）
        return "数天".into();
    }
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
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
