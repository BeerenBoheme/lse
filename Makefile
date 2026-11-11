# lse — Everything 式 Linux 桌面文件搜索工具
# 用法：
#   make            = release 构建（cargo -j6，兼顾性能与机器负载）
#   make test       = 单元测试（lse-core，26 项）
#   make deb        = 生成 .deb 安装包（build/lse_版本_amd64.deb）
#   make rpm        = 生成 .rpm 安装包（build/rpmbuild/RPMS/…，spec 模板 packaging/lse.spec.in）
#   make appimage   = 生成 AppImage（build/lse-版本-x86_64.AppImage）
#   make release    = 版本迭代入口：测试+三件套归档到 dist/<版本>/（含 SHA256SUMS）
#   make install    = 本机构建后直接安装到 /usr/local（需 root）
#   make clean      = 清理构建产物
#
# 版本迭代流程：改根 Cargo.toml [workspace.package] 的 version（唯一来源），
# 补一节 CHANGELOG.md，然后 make release——deb/AppImage/源码包与归档目录
# 全部自动跟随版本号；dist/ 下每个版本一个目录，历史版本原样保留。

CARGO      := cargo
CARGO_HOME ?= $(CURDIR)/.toolchain/cargo
RUSTUP_HOME ?= $(CURDIR)/.toolchain/rustup
export CARGO_HOME RUSTUP_HOME
PATH       := $(CURDIR)/.toolchain/cargo/bin:$(PATH)
export PATH

# 版本号唯一来源：根 Cargo.toml [workspace.package] version（各 crate workspace 继承）
VERSION    := $(shell sed -n '/^\[workspace\.package\]/,/^version = /s/^version = "\(.*\)"$$/\1/p' Cargo.toml)
ifeq ($(VERSION),)
$(error 无法从 Cargo.toml [workspace.package] 读取 version)
endif
JOBS       := 6
BUILDDIR   := build
TARGET     := target/release
SRCTAR     := $(BUILDDIR)/lse-$(VERSION)-src.tar.gz
RPMTOP     := $(BUILDDIR)/rpmbuild

BINARIES   := lse lse-gui lse-daemon lse-bench

.PHONY: all build test bench deb rpm srctar appimage release install uninstall clean

all: build

build:
	$(CARGO) build --release -j $(JOBS)

test: build
	$(CARGO) test -p lse-core --release

bench: build
	./bench.sh

# ---- deb 打包（dpkg-deb 手工构造，不依赖 debhelper 版本匹配） ----
deb: build
	rm -rf $(BUILDDIR)/deb
	mkdir -p $(BUILDDIR)/deb/DEBIAN
	mkdir -p $(BUILDDIR)/deb/usr/bin
	mkdir -p $(BUILDDIR)/deb/usr/lib/systemd/user
	mkdir -p $(BUILDDIR)/deb/usr/share/applications
	mkdir -p $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/apps
	mkdir -p $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/symbolic
	mkdir -p $(BUILDDIR)/deb/usr/share/doc/lse
	cp $(TARGET)/lse $(TARGET)/lse-gui $(TARGET)/lse-daemon $(BUILDDIR)/deb/usr/bin/
	cp packaging/lse-daemon.service $(BUILDDIR)/deb/usr/lib/systemd/user/
	cp packaging/org.lse.Search.desktop $(BUILDDIR)/deb/usr/share/applications/
	cp packaging/lse-search.svg $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/apps/
	cp packaging/lse-search-symbolic.svg $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/symbolic/
	cp README.md $(BUILDDIR)/deb/usr/share/doc/lse/ 2>/dev/null || true
	sed -e 's/@VERSION@/$(VERSION)/' packaging/deb-control.in > $(BUILDDIR)/deb/DEBIAN/control
	sed -e 's/@VERSION@/$(VERSION)/' packaging/deb-postinst.in > $(BUILDDIR)/deb/DEBIAN/postinst
	# dpkg-deb 原样保留源文件权限位；素材若为 600 会打进 deb → root 装完普通用户读不到
	# desktop（启动器直接看不见条目）。统一修正为标准权限。
	chmod 0644 $(BUILDDIR)/deb/DEBIAN/control \
	    $(BUILDDIR)/deb/usr/share/applications/*.desktop \
	    $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/apps/*.svg \
	    $(BUILDDIR)/deb/usr/share/icons/hicolor/scalable/symbolic/*.svg \
	    $(BUILDDIR)/deb/usr/lib/systemd/user/*.service \
	    $(BUILDDIR)/deb/usr/share/doc/lse/*
	chmod 0755 $(BUILDDIR)/deb/DEBIAN/postinst
	dpkg-deb --build --root-owner-group $(BUILDDIR)/deb $(BUILDDIR)/lse_$(VERSION)_amd64.deb
	@echo "== $(BUILDDIR)/lse_$(VERSION)_amd64.deb 生成完毕"
	@dpkg-deb -I $(BUILDDIR)/lse_$(VERSION)_amd64.deb | head -20

# ---- AppImage（packaging/build-appimage.sh：离线方案——借本机已有
#      type-2 AppImage 的 runtime 头拼接 mksquashfs 文件系统） ----
appimage: build
	./packaging/build-appimage.sh $(VERSION)

# ---- 源码包：deb/rpm 与发布归档共用的唯一出处（版本号跟随 Cargo.toml） ----
srctar:
	@mkdir -p $(BUILDDIR)
	tar --exclude='./target' --exclude='./.bench' --exclude='./.toolchain' \
	    --exclude='./build' --exclude='./dist' \
	    --transform 's,^\./,lse-$(VERSION)/,' \
	    -czf $(SRCTAR) \
	    ./crates ./packaging ./docs ./Cargo.toml ./Cargo.lock ./Makefile \
	    ./README.md ./CHANGELOG.md ./bench.sh ./.gitignore

# ---- rpm 打包（spec 模板 packaging/lse.spec.in → @VERSION@ 替换；
#      _topdir/_tmppath 全部收在 build/rpmbuild/ 下，不污染 ~/rpmbuild，
#      也不依赖 /var/tmp——在受限沙箱里 /var/tmp 可能只读） ----
rpm: srctar
	rm -rf $(RPMTOP)
	mkdir -p $(RPMTOP)/SOURCES $(RPMTOP)/SPECS $(RPMTOP)/BUILD \
	    $(RPMTOP)/BUILDROOT $(RPMTOP)/RPMS $(RPMTOP)/SRPMS $(RPMTOP)/TMP
	sed -e 's/@VERSION@/$(VERSION)/' packaging/lse.spec.in > $(RPMTOP)/SPECS/lse.spec
	cp $(SRCTAR) $(RPMTOP)/SOURCES/
	rpmbuild -ba --define "_topdir $(CURDIR)/$(RPMTOP)" \
	    --define "_tmppath $(CURDIR)/$(RPMTOP)/TMP" \
	    $(RPMTOP)/SPECS/lse.spec
	@echo "== $(RPMTOP)/RPMS/x86_64/lse-$(VERSION)-*.rpm 生成完毕"
	@ls -lh $(RPMTOP)/RPMS/*/*.rpm $(RPMTOP)/SRPMS/*.rpm

# ---- 发布归档：测试 + 三件套（deb / AppImage / 源码包）→ dist/<版本>/ ----
RELEASEDIR := dist/$(VERSION)

release: test deb appimage srctar
	@mkdir -p $(RELEASEDIR)
	cp -f $(SRCTAR) $(RELEASEDIR)/lse-$(VERSION)-src.tar.gz
	cp -f $(BUILDDIR)/lse_$(VERSION)_amd64.deb $(RELEASEDIR)/
	cp -f $(BUILDDIR)/lse-$(VERSION)-x86_64.AppImage $(RELEASEDIR)/
	cd $(RELEASEDIR) && sha256sum lse* > SHA256SUMS
	@echo "== 发布就绪: $(RELEASEDIR)/ （校验和见 SHA256SUMS）"
	@ls -lh $(RELEASEDIR)

# ---- 本机直接安装（开发用；正式安装用 make deb） ----
install: build
	install -Dm755 $(TARGET)/lse $(TARGET)/lse-gui $(TARGET)/lse-daemon -t /usr/local/bin
	# 开发安装指向 /usr/local/bin（deb 打包保持 /usr/bin 不经过这里）
	sed 's|^ExecStart=/usr/bin/|ExecStart=/usr/local/bin/|' packaging/lse-daemon.service \
		> $(BUILDDIR)/lse-daemon-local.service
	install -Dm644 $(BUILDDIR)/lse-daemon-local.service /usr/local/lib/systemd/user/lse-daemon.service
	@echo "已安装。启用常驻索引：systemctl --user enable --now lse-daemon"

uninstall:
	rm -f /usr/local/bin/lse /usr/local/bin/lse-gui /usr/local/bin/lse-daemon
	rm -f /usr/local/lib/systemd/user/lse-daemon.service

clean:
	$(CARGO) clean
	rm -rf $(BUILDDIR)
