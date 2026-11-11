# 更新日志（CHANGELOG）

格式参照 [Keep a Changelog](https://keepachangelog.com/)。版本号遵循语义化版本，按变更性质升位：
**修复类**升小版本号（0.1.0 → 0.1.1）、**新增功能/重大改进**升中版本号（0.1.x → 0.2.0）、
**不兼容的破坏性变更**升大版本号（0.x → 1.0.0）。
每次迭代：改根 `Cargo.toml` 的 `[workspace.package] version` → 补一节（README 示例路径同步）→ `make release`。

## [0.1.4] — 2026-11-12

### 新增
- **应用图标**：随包提供彩色 SVG 图标 `lse-search`（`packaging/lse-search.svg`，128×128 viewBox：深蓝圆底 + 白色文件 + 橙色放大镜），deb/rpm/AppImage 三条打包线统一装入 `icons/hicolor/scalable/apps/`；`.desktop` 的 `Icon=` 由 `lse-search-symbolic` 改指 `lse-search`（原先启动器/任务栏只有单色符号图标，观感差且在小尺寸下几乎不可辨）
- AppImage 根部按规范同时放 `lse-search.svg` 与 `org.lse.Search.desktop`（根部图标名必须与 desktop 的 `Icon=` 一致，否则 AppImage 元数据/桌面集成取不到图标）
- GUI 启动时若图标主题中存在 `lse-search` 则显式设为窗口图标（`gtk::IconTheme::for_display(...).has_icon()` 守卫）；开发态直跑 `target/release/lse-gui`（图标未安装）时不设置，沿用桌面文件推导的默认图标，不产生 GTK 告警
- 单色 `lse-search-symbolic.svg` 保留并继续随包安装，供面板等需要随主题着色的小尺寸场景使用

### 说明
- 托盘图标仍用主题自带的 `system-search-symbolic`：SNI 的 `IconName` 由宿主进程（plasmashell / GNOME shell）解析，AppImage 下宿主看不到包内图标路径，用主题图标最稳（改动会带来"托盘图标丢失"的回归风险）

## [0.1.3] — 2026-11-11

### 修复
- **内存泄漏与状态漂移（inotify watch 状态表，四处）**：
  - 维护线程的 `miss`/`last_done` 退避表**只插不删**，目录 churn 后孤儿键永久累积（慢泄漏）→ 每轮按当前 unwatched 清单剪枝
  - `try_watch_dir` 成功后清理 unwatched 清单的条件**写反**：刚监听成功的根（r == path）反而被保留 → 永久重复重扫 + `unwatched_dirs` 虚高，而路径是清单项子孙时误删其祖先（非递归 watch 不覆盖祖先的其他子树，丢兜底）→ 改为精确匹配、只移除自身
  - 目录删除只从 `watched` 集合移除、**不回写 `watched_dirs` 计数** → 计数只增不减 → 预算判定失真、过度踢出 victim 放大 unwatched 清单 → 删除后按集合实际长度同步计数
  - 已删除/移走的根**永不出 `unwatched_roots`** → 退避重扫每轮对不存在的路径空转（每次都产出 RemoveSubtree）且键永久累积 → 删除时按前缀一并清理并同步 `unwatched_dirs`
  - 启动时 `watched` 集合用 `sorted.take(watch_ok)` 反推：失败目录夹在中间时会把**未监听成功**的目录放进集合（后续 `try_watch_dir` 提前返回、永远收不到事件）→ 改为记录实际 watch 成功的路径
- 验证：新增动态回归脚本（私有 D-Bus 总线 + `chmod 000` 逼出 unwatched 路径），修复前复现三处漂移（退化清单 2→2、监听计数 7→8 不减），修复后四步全部归位（2→1→0、7→8→6）；`cargo test` 26 项全过

## [0.1.2] — 2026-11-11

### 新增
- **RPM 打包**：新增 `packaging/lse.spec.in` 模板（`@VERSION@` 占位，与 deb-control.in 同惯例）与 `make rpm` 目标；`_topdir`/`_tmppath` 全部收在 `build/rpmbuild/` 下（不污染 ~/rpmbuild、不依赖 /var/tmp，受限沙箱亦可构建）；依赖按发行版分家（rpm: gtk4/libadwaita，`Recommends: xdg-desktop-portal`），systemd user 服务/desktop/符号图标/文档与 deb 布局一一对应
- **源码包独立目标 `make srctar`**：deb/rpm/发布归档共用同一出处，版本号仍唯一来自 Cargo.toml；源码包自带 RPM 配方，任意 Fedora/RHEL 可 `dnf builddep` + `rpmbuild --rebuild` 重建

### 变更
- **AppImage 构建适配 Fedora**：gio 模块与 gdk-pixbuf 加载器兼容 `/usr/lib64` 布局（原硬编码 Debian 的 `/usr/lib/x86_64-linux-gnu`，Fedora 上会静默跳过 → 文件图标缺失）；runtime 头源支持 `LSE_APPIMAGE_RUNTIME` 环境变量覆盖，推荐官方 AppImageKit 的 `appimagetool-x86_64.AppImage`
- **AppImage squashfs 压缩 zstd → xz**：官方 AppImageKit runtime 仅内置 xz/zlib 解压，zstd 产物运行时报 `Squashfs image uses (null) compression, this version supports only xz, zlib.` 起不来；改 xz 后 `--appimage-extract` 冒烟通过（且 37M → 32M）

## [0.1.1] — 2026-11-11

### 修复
- **deb 安装后全局快捷键失效**：桌面文件名 `lse-search.desktop` 与 application_id `org.lse.Search` 不符，portal `Registry.Register` 按 `<app_id>.desktop` 查找失败 → `CreateSession` 拒绝（"An app id is required"）→ 快捷键链路第一环断裂。改名为规范的 `org.lse.Search.desktop`（deb/AppImage 同步）；CLI 新增 `lse --show` 作为全局快捷键降级绑定的目标
- **deb 安装后应用启动器找不到 lse**：打包素材权限为 600，dpkg-deb 原样保留 → 装完后普通用户读不到 `.desktop`（KDE/GNOME 启动器静默跳过条目）。Makefile deb 目标统一 chmod 数据文件 0644/postinst 0755，素材源头同步修正

## [0.1.0] — 2026-11-11

首个完整版本（五个阶段全部交付）。

### 新增
- **索引后端**：并行全盘遍历（readdir d_type，size/mtime 每文件一次 fstatat）→ SQLite 归一化存储（dirs/files WITHOUT ROWID，体积 0.48–0.58× 路径字节）→ 内存主索引分片并行加载；148 万文件建索引 28.7s、冷启动 760ms
- **D-Bus 服务**（org.lse.Index）：Search/Status/Rebuild/ReportOpen/Ping；GUI 与 CLI 均为纯客户端
- **inotify 增量**：目录级监听、变更批量事务落库；watch 预算检测（limit×0.8−已用）、低优先级目录踢出、退避重扫（300→900→3600s）、sysctl 调优建议；退出落盘 + 启动一致性校验（mtime 对比局部重扫）
- **GUI**（GTK4 + libadwaita + Blueprint）：80ms debounce 实时过滤零中间态；多词 AND 搜索（首词定相关度、其余词过滤）；三列结果（名称/完整路径/修改时间）列头三态排序（正序→逆序→取消）+ 排序自动置顶；双击/回车打开、右键菜单（打开/打开所在文件夹/复制路径）、拖拽到文件管理器；托盘 SNI（自实现，不依赖 libappindicator）；状态栏"已索引 N 个文件 · 最后更新 X 前"；HeaderBar 最小化/最大化/关闭；打开频率参与排序
- **全局快捷键 Super+Space**：走 xdg-desktop-portal GlobalShortcuts（Wayland 安全），trigger 按两后端白名单规范转换 `LOGO+space`，KDE/GNOME 通吃
- **排除规则**独立配置 `~/.config/lse/exclude.rules`（默认 /proc /sys /dev /run /tmp、node_modules、.git/objects、__pycache__、*.pyc、*.o）
- **CLI** `lse`：关键字/--regex/--limit/--type/--ext/--path/--update/--status；D-Bus 优先、无 daemon 时降级读本地库
- **打包**：deb（systemd user service + .desktop + 符号图标）、AppImage（离线 runtime 拼接，自带 127 库闭包，任意发行版双击即用且 GUI 自动拉起随附后端）、源码包；`make release` 一键归档 `dist/<版本>/`
- **文档**：README（含 KDE Plasma Wayland 实机截图）、bench.sh 性能基准（10 万文件三项指标）

### 修复
- Fedora 下 AppImage"后端未运行"：GUI 自动从同目录拉起随附 `lse-daemon`（systemd→同目录→PATH 三级回退），失败原因以 toast 展示；AppImage 随包携带 gdk-pixbuf SVG 加载器并运行时重写 cache 路径

### 已知限制
- GNOME 未实测（portal trigger 格式已按其源码验证兼容）
- 全盘口径（148 万文件）daemon 常驻 269MB，超出 200MB 指标；50 万口径推算约 90MB 达标
