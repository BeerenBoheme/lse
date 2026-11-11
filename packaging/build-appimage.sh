#!/bin/sh
# build-appimage.sh <version> — 离线构建 AppImage（type-2）
#
# 环境约束：本机有 mksquashfs，但没有 appimagetool（离线、无 GitHub 访问）。
# 方案：取官方 AppImageKit 的 ELF runtime 头，拼上我们自己 mksquashfs
# 生成的 squashfs 文件系统，得到标准 type-2 AppImage。
# runtime 源：默认用随包固化的**官方 AppImageKit runtime 头**
# （packaging/appimage-runtime-head.bin，从 0.1.3 产物提取，sha256
#  d3c3f557deb44674039880ccf3258c4ea9b9c27785e6238e0d6424dde6d94f19）。
# 不再借用本机第三方 AppImage（如 Hiddify）的 runtime——那是本地文件，
# 换机器就没有，且非官方版本。如需更新 runtime：
#   下载官方 appimagetool 后取其头部，或直接 LSE_APPIMAGE_RUNTIME 指定：
#   https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage
# 产物：build/lse-<版本>-x86_64.AppImage（自包含 GTK 运行库）。
set -eu

cd "$(dirname "$0")/.."
# 未传参时从 Cargo.toml 读版本（与 Makefile 同源，避免硬编码漂移）
VERSION="${1:-$(sed -n '/^\[workspace\.package\]/,/^version = /s/^version = "\(.*\)"$/\1/p' Cargo.toml)}"
test -n "$VERSION" || { echo "无法从 Cargo.toml 确定版本号" >&2; exit 1; }
BUILD=build
APPDIR="$BUILD/lse.AppDir"
RUNTIME_SRC="${LSE_APPIMAGE_RUNTIME:-packaging/appimage-runtime-head.bin}"
OUT="$BUILD/lse-$VERSION-x86_64.AppImage"

command -v mksquashfs >/dev/null || { echo "缺 mksquashfs（apt install squashfs-tools）"; exit 1; }
[ -f "$RUNTIME_SRC" ] || { echo "找不到 runtime 源: $RUNTIME_SRC"; exit 1; }

mkdir -p "$BUILD"

# ---------- 1) 取得 runtime ELF 头 ----------
# 两种输入都支持：
#   a) 纯 runtime 头（packaging/appimage-runtime-head.bin，无 hsqs 后缀）→ 直接拷贝
#   b) 完整 type-2 AppImage（段表末尾接 hsqs squashfs）→ 切出头
python3 - "$RUNTIME_SRC" "$BUILD/runtime-head.bin" <<'PY'
import sys, struct
src, dst = sys.argv[1], sys.argv[2]
raw = open(src, 'rb').read()
assert raw[:4] == b'\x7fELF', f"{src} 不是 ELF"
e_shoff = struct.unpack_from('<Q', raw, 40)[0]
e_shentsize, e_shnum, _ = struct.unpack_from('<HHH', raw, 58)
end = e_shoff + e_shnum * e_shentsize
# 判断顺序关键：纯 runtime 头里也可能含 "hsqs" 字面串（来自 squashfs 代码里的
# 符号/字符串），绝不能先对全文件 find——先在段表末尾定点判定。
if len(raw) == end:
    cut = len(raw)                 # 纯 runtime 头：段表末尾恰是文件尾，无 squashfs
    print(f"[appimage] 输入是纯 runtime 头（{cut} 字节），直接使用")
elif raw[end:end + 4] == b'hsqs':
    cut = end                      # 完整 AppImage：段表末尾即 squashfs 起点
else:
    cut = raw.find(b'hsqs', end)   # 段表末尾之后找 hsqs（可能有对齐填充）
    assert cut > 0, f"段表末尾 {end} 起找不到 squashfs superblock"
    print(f"[appimage] 注意: squashfs 起点 {cut} 与段表末尾 {end} 有 {cut-end} 字节填充")
open(dst, 'wb').write(raw[:cut])
print(f"[appimage] runtime 头 = {cut} 字节")
PY

# ---------- 2) 组装 AppDir ----------
echo "[appimage] 组装 AppDir ..."
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib/gio/modules"

cp target/release/lse target/release/lse-gui target/release/lse-daemon "$APPDIR/usr/bin/"

# GIO 模块（mime/uri 打开依赖；宿主构建，与同发行版兼容）
# Debian 系在 /usr/lib/x86_64-linux-gnu，Fedora/RHEL 在 /usr/lib64
for _giod in /usr/lib/x86_64-linux-gnu/gio/modules /usr/lib64/gio/modules; do
    if [ -d "$_giod" ]; then
        cp -L "$_giod"/*.so "$APPDIR/usr/lib/gio/modules/" 2>/dev/null || true
        break
    fi
done

# gdk-pixbuf 加载器（png/svg/… 文件图标）：宿主 cache 记的是打包机绝对路径，
# 在别的发行版（如 Fedora 的 /usr/lib64 布局）根本不存在 → 随包携带 loaders，
# cache 由 AppRun 运行时按挂载目录重写（见下方 AppRun）。
GBK=""
for _gbkd in /usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0 /usr/lib64/gdk-pixbuf-2.0/2.10.0; do
    if [ -d "$_gbkd/loaders" ]; then GBK="$_gbkd"; break; fi
done
if [ -n "$GBK" ]; then
    mkdir -p "$APPDIR/usr/lib/gdk-pixbuf/loaders"
    cp -L "$GBK"/loaders/*.so "$APPDIR/usr/lib/gdk-pixbuf/loaders/"
    cp -L "$GBK/loaders.cache" "$APPDIR/usr/lib/gdk-pixbuf/" 2>/dev/null || true
fi

# ldd 闭包：除 glibc 核心家族（系统必有、且绝不能混版本）外全部自带。
# 扫描对象 = 三个二进制 + 刚拷入的 gio/gdk-pixbuf 模块（它们的依赖如
# libpng/libjpeg/librsvg 不在二进制的 ldd 里，漏掉就会在 Fedora 上缺库）。
{
    for bin in lse lse-gui lse-daemon; do
        ldd "target/release/$bin" 2>/dev/null
    done
    for so in "$APPDIR/usr/lib/gio/modules"/*.so "$APPDIR/usr/lib/gdk-pixbuf/loaders"/*.so; do
        [ -f "$so" ] && ldd "$so" 2>/dev/null
    done
} | awk '/=> \// {print $3} /^\// {print $1}' | sort -u | while read -r lib; do
    [ -n "$lib" ] && [ -f "$lib" ] || continue
    case "$(basename "$lib")" in
        libc.so.*|libm.so.*|libpthread*|libdl.so*|librt.so*|ld-linux*|\
        libresolv*|libutil*|libanl*|libnss_*|libgcc_s*|libcrypt*|libnsl*)
            continue ;;  # glibc 家族：用宿主系统
    esac
    cp -L "$lib" "$APPDIR/usr/lib/"
done

# 桌面入口 / 图标 / 文档（AppImage 根部要求：desktop 文件 + 与其 Icon= 同名的图标）
cp packaging/org.lse.Search.desktop "$APPDIR/"
cp packaging/lse-search.svg "$APPDIR/"
cp packaging/lse-search-symbolic.svg "$APPDIR/"
mkdir -p "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
         "$APPDIR/usr/share/icons/hicolor/scalable/symbolic" \
         "$APPDIR/usr/share/doc/lse"
cp packaging/org.lse.Search.desktop "$APPDIR/usr/share/applications/"
cp packaging/lse-search.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
cp packaging/lse-search-symbolic.svg "$APPDIR/usr/share/icons/hicolor/scalable/symbolic/"
[ -f README.md ] && cp README.md "$APPDIR/usr/share/doc/lse/" || true

# AppRun：库/数据路径引导；--cli 子命令透给命令行前端
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export XDG_DATA_DIRS="$HERE/usr/share:/usr/local/share:/usr/share${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
if [ -d "$HERE/usr/lib/gio/modules" ]; then
    export GIO_MODULE_DIR="$HERE/usr/lib/gio/modules${GIO_MODULE_DIR:+:$GIO_MODULE_DIR}"
fi
# gdk-pixbuf：随附 loaders.cache 记录的是打包机绝对路径
# （Ubuntu /usr/lib/x86_64-linux-gnu 或 Fedora /usr/lib64），别的发行版
# 没有该路径 → 按本次挂载目录重写到用户缓存。
GDKDIR="$HERE/usr/lib/gdk-pixbuf"
if [ -d "$GDKDIR/loaders" ] && [ -f "$GDKDIR/loaders.cache" ]; then
    PXC="${XDG_CACHE_HOME:-$HOME/.cache}/lse-gdk-loaders.cache"
    if [ ! -f "$PXC" ] || ! grep -qF "$GDKDIR" "$PXC" 2>/dev/null; then
        mkdir -p "$(dirname "$PXC")" 2>/dev/null
        sed -e "s|/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0|$GDKDIR|g" \
            -e "s|/usr/lib64/gdk-pixbuf-2.0/2.10.0|$GDKDIR|g" \
            "$GDKDIR/loaders.cache" > "$PXC" 2>/dev/null || true
    fi
    [ -f "$PXC" ] && export GDK_PIXBUF_MODULE_FILE="$PXC"
fi
if [ "${1:-}" = "--cli" ]; then
    shift
    exec "$HERE/usr/bin/lse" "$@"
fi
exec "$HERE/usr/bin/lse-gui" "$@"
APPRUN
chmod 0755 "$APPDIR/AppRun"

LIBN=$(ls "$APPDIR/usr/lib"/*.so* 2>/dev/null | wc -l)
echo "[appimage] AppDir 就绪：$(du -sh "$APPDIR" | cut -f1)，自带库 $LIBN 个"

# ---------- 3) squashfs + 拼接 runtime ----------
# 压缩用 xz：官方 AppImageKit runtime 只内置 xz/zlib 解压
# （zstd 会报 "uses (null) compression ... supports only xz, zlib"），
# xz 也是 appimagetool 的默认格式。
echo "[appimage] mksquashfs (xz, level 6 默认) ..."
mksquashfs "$APPDIR" "$BUILD/squashfs.part" -comp xz \
    -b 1M -noappend -all-root -no-xattrs -mkfs-time 0 -quiet

cat "$BUILD/runtime-head.bin" "$BUILD/squashfs.part" > "$OUT"
chmod 0755 "$OUT"
rm -f "$BUILD/squashfs.part"

# ---------- 4) 冒烟：--appimage-extract 不依赖显示服务器 ----------
echo "[appimage] 冒烟测试 --appimage-extract ..."
SMOKE="$BUILD/appimage-smoke"
rm -rf "$SMOKE" "$BUILD/squashfs-root"
mkdir -p "$SMOKE"
if (cd "$BUILD" && "$OLDPWD/$OUT" --appimage-extract >/dev/null 2>&1) && [ -f "$BUILD/squashfs-root/AppRun" ]; then
    mv "$BUILD/squashfs-root" "$SMOKE/root"
    echo "[appimage] extract 冒烟通过（runtime 拼接布局正确）"
else
    echo "[appimage] 警告: extract 冒烟失败，请手动运行 $OUT 检查"
fi

echo "== 产物: $OUT ($(du -h "$OUT" | cut -f1))"
