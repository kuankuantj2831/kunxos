# KunxOS 顶层构建脚本：内核 + init + 系统服务 + rootfs（ramdisk）+ QEMU 启动。
#
#   make kernel   编译内核（内嵌 ramdisk 镜像）
#   make init     编译用户态 init（Rust，kunxinit -> rootfs/init）
#   make networkd 编译网络守护进程（kunxserv/networkd）
#   make wifid    编译 Wi-Fi 守护进程（kunxserv/wifid，扫描/连接/配置存储）
#   make audiod   编译音频守护进程（kunxserv/audiod，焦点+混音）
#   make launcher 编译桌面启动器 + 测试应用（apps/launcher、apps/settings 等）
#   make rootfs   把 init/networkd/launcher/apps/busybox 复制进 rootfs 并打包 ramdisk
#   make run      构建全部并用 QEMU 启动
#   make test-audio  注入 /etc/audio-test 标记跑音频混音/延迟自动化（默认镜像不含，
#                 避免每次开机串行跑 audiomix+audiolat 拖慢启动）
#   make clean    清理所有构建产物
# 控制台安静模式（默认开启）：启动/运行期信息日志不上串口，CLI 只保留错误与
# 交互输出。需要完整日志：make run-gui QEMU_APPEND="verbose"（或运行时 shell
# `verbose on`；结构化日志始终可用 shell `log` / `dmesg` 查看）。
# 运行加速：QEMU_ACCEL 默认 "tcg"。注意：x86_64 宿主上的 qemu-system-riscv64
# 只支持 TCG（KVM 需同架构宿主 + 内核支持）——模拟器内帧率上限由 TCG 决定
# （GUI 动画实测 ~81fps），144Hz 需真机或同架构 KVM 宿主。
# 覆盖示例：make run-gui QEMU_ACCEL=tcg；内核 `-append "perftest"` 可手动复跑
# 启动期动画管线性能基线（默认跳过，见 kunxcore/src/anim.rs）。

TOPDIR    := $(CURDIR)
KUNXCORE  := $(TOPDIR)/kunxcore
KUNXINIT  := $(TOPDIR)/kunxinit
KUNXSERV  := $(TOPDIR)/kunxserv
WIRELESS  := $(TOPDIR)/wireless
NETWORKD  := $(KUNXSERV)/networkd
LOGD      := $(KUNXSERV)/logd
AUDIOD    := $(KUNXSERV)/audiod
CAMERAD   := $(KUNXSERV)/camerad
SENSORD   := $(KUNXSERV)/sensord
POSITIOND := $(KUNXSERV)/positiond
LIFD      := $(KUNXSERV)/lifd
PUSHD     := $(KUNXSERV)/pushd
ALARMD    := $(KUNXSERV)/alarmd
RILD      := $(KUNXSERV)/rild
WIFID     := $(KUNXSERV)/wifid
# 用户态应用共享源码（经 #[path] 引入；任一变化都需重编所有应用）。
SHARED_SRC := $(TOPDIR)/shared/ttf_text.rs $(TOPDIR)/shared/font_render.rs $(TOPDIR)/shared/display.rs \
              $(TOPDIR)/shared/icons.rs \
              $(TOPDIR)/shared/nav.rs $(TOPDIR)/shared/app_kit.rs $(TOPDIR)/shared/error_ui.rs \
              $(TOPDIR)/shared/cursor_overlay.rs $(TOPDIR)/shared/ds.rs \
              $(TOPDIR)/shared/lifecycle.rs $(TOPDIR)/shared/sensor_client.rs \
              $(TOPDIR)/shared/cursor_sprite.rs $(TOPDIR)/shared/cursor.rgba \
              $(TOPDIR)/shared/dialog.rs
APPS      := $(TOPDIR)/apps
LAUNCHER  := $(APPS)/launcher
SETTINGS  := $(APPS)/settings
TERMINAL  := $(APPS)/terminal
CALC      := $(APPS)/calculator
OOBE      := $(APPS)/oobe
STORE     := $(APPS)/store
FILES     := $(APPS)/files
PHONE     := $(APPS)/phone
SMS       := $(APPS)/sms
CONTACTS  := $(APPS)/contacts
BROWSER   := $(APPS)/browser
CLOCK     := $(APPS)/clock
CALENDAR  := $(APPS)/calendar
CAMERA    := $(APPS)/camera
GALLERY   := $(APPS)/gallery
MUSIC     := $(APPS)/music
MAIL      := $(APPS)/mail
MAP       := $(APPS)/map
WEATHER   := $(APPS)/weather
CLOUD     := $(APPS)/cloud
VIDEO     := $(APPS)/video
NOTES     := $(APPS)/notes
LCAPP     := $(APPS)/lcapp
KUNXPKG   := $(TOPDIR)/kunxpkg
ROOTFS    := $(TOPDIR)/rootfs
TOOLS     := $(TOPDIR)/tools
RECOVERY  := $(APPS)/recovery
RECOV_ELF = $(RECOVERY)/target/$(TARGET)/release/recovery

TARGET     := riscv64gc-unknown-none-elf
KERNEL_BIN := rust-kernel
KERNEL_ELF := $(KUNXCORE)/target/$(TARGET)/release/$(KERNEL_BIN)
KERNEL_IMG := $(KUNXCORE)/kernel.img
INIT_ELF   := $(KUNXINIT)/target/$(TARGET)/release/kunxinit
NET_ELF    := $(NETWORKD)/target/$(TARGET)/release/networkd
WIFID_ELF  := $(WIFID)/target/$(TARGET)/release/wifid
LOGD_ELF   := $(LOGD)/target/$(TARGET)/release/logd
AUDIOD_ELF := $(AUDIOD)/target/$(TARGET)/release/audiod
CAMERAD_ELF := $(CAMERAD)/target/$(TARGET)/release/camerad
SENSORD_ELF := $(SENSORD)/target/$(TARGET)/release/sensord
POSITIOND_ELF := $(POSITIOND)/target/$(TARGET)/release/positiond
LIFD_ELF    := $(LIFD)/target/$(TARGET)/release/lifd
PUSHD_ELF   := $(PUSHD)/target/$(TARGET)/release/pushd
ALARMD_ELF  := $(ALARMD)/target/$(TARGET)/release/alarmd
LAUNCHER_ELF := $(LAUNCHER)/target/$(TARGET)/release/launcher
SETTINGS_ELF := $(SETTINGS)/target/$(TARGET)/release/settings
TERMINAL_ELF := $(TERMINAL)/target/$(TARGET)/release/terminal
CALC_ELF     := $(CALC)/target/$(TARGET)/release/calculator
OOBE_ELF     := $(OOBE)/target/$(TARGET)/release/oobe
STORE_ELF    := $(STORE)/target/$(TARGET)/release/store
FILES_ELF    := $(FILES)/target/$(TARGET)/release/files
PHONE_ELF    := $(PHONE)/target/$(TARGET)/release/phone
SMS_ELF      := $(SMS)/target/$(TARGET)/release/sms
CONTACTS_ELF := $(CONTACTS)/target/$(TARGET)/release/contacts
BROWSER_ELF  := $(BROWSER)/target/$(TARGET)/release/browser
CLOCK_ELF    := $(CLOCK)/target/$(TARGET)/release/clock
CALENDAR_ELF := $(CALENDAR)/target/$(TARGET)/release/calendar
CAMERA_ELF   := $(CAMERA)/target/$(TARGET)/release/camera
GALLERY_ELF  := $(GALLERY)/target/$(TARGET)/release/gallery
MUSIC_ELF    := $(MUSIC)/target/$(TARGET)/release/music
MAIL_ELF     := $(MAIL)/target/$(TARGET)/release/mail
MAP_ELF      := $(MAP)/target/$(TARGET)/release/map
WEATHER_ELF  := $(WEATHER)/target/$(TARGET)/release/weather
CLOUD_ELF    := $(CLOUD)/target/$(TARGET)/release/cloud
VIDEO_ELF    := $(VIDEO)/target/$(TARGET)/release/video
NOTES_ELF    := $(NOTES)/target/$(TARGET)/release/notes
LCAPP_ELF    := $(LCAPP)/target/$(TARGET)/release/lcapp
PKG_ELF      := $(KUNXPKG)/target/$(TARGET)/release/kunxpkg
AUDIOTEST_ELF := $(APPS)/audiotest/target/$(TARGET)/release/audiotest
TLSTEST_ELF  := $(APPS)/tlstest/target/$(TARGET)/release/tlstest
AUDIOMIX_ELF  := $(APPS)/audiomix/target/$(TARGET)/release/audiomix
AUDIOLAT_ELF  := $(APPS)/audiolat/target/$(TARGET)/release/audiolat
RILD_ELF      := $(RILD)/target/$(TARGET)/release/rild
MODEMTEST_ELF := $(APPS)/modemtest/target/$(TARGET)/release/modemtest
SFSRACE_ELF   := $(APPS)/sfsrace/target/$(TARGET)/release/sfsrace
CRASHME_ELF   := $(APPS)/crashme/target/$(TARGET)/release/crashme
KOSTEST_ELF   := $(APPS)/kostest/target/$(TARGET)/release/kostest
KOSDEMO_ELF   := $(APPS)/kosdemo/target/$(TARGET)/release/kosdemo
BUSYBOX    := $(KUNXCORE)/apps/busybox.elf
RAMDISK    := $(KUNXCORE)/ramdisk.img
DTB        := $(TOPDIR)/kunxos.dtb
DISK_IMG   := $(TOPDIR)/disk.img
# 平滑字体子集（tools/make_fonts.py 生成，随 rootfs 提供为 /fonts/*.ttf）：
#   cn-subset.ttf  普通文字：fonts/uiputong.ttf（更纱黑体 UI SC 常规） → /fonts/cn.ttf
#   bold-subset.ttf 标题：fonts/bold.ttf（更纱黑体 UI SC SemiBold）  → /fonts/bold.ttf
#   oobe-cn-subset.ttf / oobe-bold-subset.ttf  向导专属超小子集（仅 OOBE 文案，
#   867 字形 ~378KB，fontdue 解析在 TCG 慢宿主下也只需秒级） → /fonts/oobe-cn.ttf
#   / /fonts/oobe-bold.ttf（OOBE 首帧即平滑，不再有像素兜底）。
FONT_SUBSET := $(TOOLS)/fonts/cn-subset.ttf
FONT_BOLD_SUBSET := $(TOOLS)/fonts/bold-subset.ttf
FONT_OOBE_CN_SUBSET := $(TOOLS)/fonts/oobe-cn-subset.ttf
FONT_OOBE_BOLD_SUBSET := $(TOOLS)/fonts/oobe-bold-subset.ttf
# 桌面启动器专属超小子集（launcher-cn/bold，~947 字形 410KB，仅桌面文案）。
FONT_LAUNCHER_CN_SUBSET := $(TOOLS)/fonts/launcher-cn-subset.ttf
FONT_LAUNCHER_BOLD_SUBSET := $(TOOLS)/fonts/launcher-bold-subset.ttf
# Phosphor 图标子集（fonts/Phosphor-Fill.ttf / Phosphor-Bold.ttf，码点取自
# shared/icons.rs）：icon-fill 实心（应用图标/天气图标）、icon-line 粗线
# （行内 emoji）。→ /fonts/icon-fill.ttf /fonts/icon-line.ttf
FONT_ICON_FILL_SUBSET := $(TOOLS)/fonts/icon-fill-subset.ttf
FONT_ICON_LINE_SUBSET := $(TOOLS)/fonts/icon-line-subset.ttf
# rootfs ramdisk 块数（与 kunxcore/src/main.rs 的 RAMDISK_BLOCKS 一致：4096 × 4KB = 16MB，容纳 cn/bold/emoji 三套字体）
RAMDISK_BLOCKS := 4096

ROOTFS_FILES := \
	$(ROOTFS)/etc/inittab \
	$(ROOTFS)/etc/init.d/S01-network \
	$(ROOTFS)/etc/init.d/S03-launcher \
	$(ROOTFS)/var/lib/pkg/sources.list \
	$(ROOTFS)/var/lib/pkg/repo/main/.kunx-done \
	$(ROOTFS)/var/lib/pkg/repo/bad/Packages.sig \
	$(ROOTFS)/var/lib/pkg/available

CARGO   ?= cargo
OBJCOPY ?= llvm-objcopy
# QEMU riscv64：宿主未安装系统包（无 root），使用用户目录解包版
# （qemu-system-riscv + opensbi 固件，来自 Arch 官方仓库包）。
QEMU_SYSROOT := $(HOME)/qemu-riscv-pkgs/root/usr
QEMU    ?= $(QEMU_SYSROOT)/bin/qemu-system-riscv64
# OpenSBI fw_dynamic（qemu -bios）：内核经其以 S 模式在 0x80200000 启动
OPENBI  := $(QEMU_SYSROOT)/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin
# 加速后端：x86 宿主无 riscv KVM，恒为 TCG（纯软件模拟）。
QEMU_ACCEL ?= tcg
# 内核启动参数（QEMU -append）：如 QEMU_APPEND="verbose" 开启完整启动日志。
QEMU_APPEND ?=

CARGO_FEATURES := --features qemu

.PHONY: all kernel init networkd kosdisk kos-disk test-mailbox test-wireless wifid rild logd camerad audiod lifd pushd alarmd launcher lcapp oobe pkg store rootfs run run-gui run-input run-audio run-phone test-modem test-audio test-pdu test-wifi lifecycle-test lifecycle-core-test disk run-disk clean

all: kernel

# ---- 用户态 ELF（Rust no_std，内核 ELF 加载器直接加载，无需 objcopy） ----
$(INIT_ELF): $(shell find $(KUNXINIT)/src -name '*.rs') $(KUNXINIT)/Cargo.toml $(KUNXINIT)/build.rs $(KUNXINIT)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXINIT)/Cargo.toml

$(NET_ELF): $(shell find $(NETWORKD)/src -name '*.rs') $(NETWORKD)/Cargo.toml $(NETWORKD)/build.rs $(NETWORKD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(NETWORKD)/Cargo.toml

# wifid：Wi-Fi 守护进程（/dev/wifi 命令/事件 + /data/wifi/networks 配置存储）。
$(WIFID_ELF): $(shell find $(WIFID)/src -name '*.rs') $(WIFID)/Cargo.toml $(WIFID)/build.rs $(WIFID)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(WIFID)/Cargo.toml

# rild：RIL 守护进程（build.rs 交叉编译 C PDU 编解码库 libpdu.a 并静态链接）。
$(RILD_ELF): $(shell find $(RILD)/src -name '*.rs') $(RILD)/Cargo.toml $(RILD)/build.rs $(RILD)/linker.ld $(RILD)/src/pdu/pdu.c $(RILD)/src/pdu/pdu.h
	$(CARGO) build --release --target $(TARGET) --manifest-path $(RILD)/Cargo.toml

# modemtest：电话/短信子系统自动化自检（无头，QEMU 测试模式运行）。
$(MODEMTEST_ELF): $(shell find $(APPS)/modemtest/src -name '*.rs') $(APPS)/modemtest/Cargo.toml $(APPS)/modemtest/build.rs $(APPS)/modemtest/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/modemtest/Cargo.toml

# sfsrace：SFS v2 并发回归测试（多进程并发创建/读写/截断/删除 /var/run 文件）。
$(SFSRACE_ELF): $(shell find $(APPS)/sfsrace/src -name '*.rs') $(APPS)/sfsrace/Cargo.toml $(APPS)/sfsrace/build.rs $(APPS)/sfsrace/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/sfsrace/Cargo.toml

$(LIFD_ELF): $(shell find $(LIFD)/src -name '*.rs') $(LIFD)/Cargo.toml $(LIFD)/build.rs $(LIFD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(LIFD)/Cargo.toml

$(ALARMD_ELF): $(shell find $(ALARMD)/src -name '*.rs') $(ALARMD)/Cargo.toml $(ALARMD)/build.rs $(ALARMD)/linker.ld $(TOPDIR)/shared/audio.rs $(TOPDIR)/shared/tz.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(ALARMD)/Cargo.toml

$(PUSHD_ELF): $(shell find $(PUSHD)/src -name '*.rs') $(PUSHD)/Cargo.toml $(PUSHD)/build.rs $(PUSHD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(PUSHD)/Cargo.toml

$(LOGD_ELF): $(shell find $(LOGD)/src -name '*.rs') $(LOGD)/Cargo.toml $(LOGD)/build.rs $(LOGD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(LOGD)/Cargo.toml

$(AUDIOD_ELF): $(shell find $(AUDIOD)/src -name '*.rs') $(AUDIOD)/Cargo.toml $(AUDIOD)/build.rs $(AUDIOD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(AUDIOD)/Cargo.toml

$(CAMERAD_ELF): $(shell find $(CAMERAD)/src -name '*.rs') $(CAMERAD)/Cargo.toml $(CAMERAD)/build.rs $(CAMERAD)/linker.ld $(TOPDIR)/shared/jpeg/src/lib.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(CAMERAD)/Cargo.toml

$(SENSORD_ELF): $(shell find $(SENSORD)/src -name '*.rs') $(SENSORD)/Cargo.toml $(SENSORD)/build.rs $(SENSORD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(SENSORD)/Cargo.toml

$(POSITIOND_ELF): $(shell find $(POSITIOND)/src -name '*.rs') $(POSITIOND)/Cargo.toml $(POSITIOND)/build.rs $(POSITIOND)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(POSITIOND)/Cargo.toml

$(LAUNCHER_ELF): $(shell find $(LAUNCHER)/src -name '*.rs') $(LAUNCHER)/Cargo.toml $(LAUNCHER)/build.rs $(LAUNCHER)/linker.ld $(SHARED_SRC) $(TOPDIR)/shared/cjk_font_launcher.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(LAUNCHER)/Cargo.toml

$(SETTINGS_ELF): $(shell find $(SETTINGS)/src -name '*.rs') $(SETTINGS)/Cargo.toml $(SETTINGS)/build.rs $(SETTINGS)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(SETTINGS)/Cargo.toml

$(TERMINAL_ELF): $(shell find $(TERMINAL)/src -name '*.rs') $(TERMINAL)/Cargo.toml $(TERMINAL)/build.rs $(TERMINAL)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(TERMINAL)/Cargo.toml

$(CALC_ELF): $(shell find $(CALC)/src -name '*.rs') $(CALC)/Cargo.toml $(CALC)/build.rs $(CALC)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(CALC)/Cargo.toml

CRASHME := $(APPS)/crashme
$(CRASHME_ELF): $(shell find $(CRASHME)/src -name '*.rs') $(CRASHME)/Cargo.toml $(CRASHME)/build.rs $(CRASHME)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(CRASHME)/Cargo.toml

KOSTEST := $(APPS)/kostest
KOSDEMO := $(APPS)/kosdemo
$(KOSDEMO_ELF): $(shell find $(KOSDEMO)/src -name '*.rs') $(KOSDEMO)/Cargo.toml $(KOSDEMO)/build.rs $(KOSDEMO)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KOSDEMO)/Cargo.toml
$(KOSTEST_ELF): $(shell find $(KOSTEST)/src -name '*.rs') $(KOSTEST)/Cargo.toml $(KOSTEST)/build.rs $(KOSTEST)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KOSTEST)/Cargo.toml

$(OOBE_ELF): $(shell find $(OOBE)/src -name '*.rs') $(OOBE)/Cargo.toml $(OOBE)/build.rs $(OOBE)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(OOBE)/Cargo.toml

$(STORE_ELF): $(shell find $(STORE)/src -name '*.rs') $(STORE)/Cargo.toml $(STORE)/build.rs $(STORE)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(STORE)/Cargo.toml


$(FILES_ELF): $(shell find $(FILES)/src -name '*.rs') $(FILES)/Cargo.toml $(FILES)/build.rs $(FILES)/linker.ld $(SHARED_SRC) $(TOPDIR)/shared/cjk_font_files.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(FILES)/Cargo.toml

# 桌面应用（真实实现，共用 shared/app_kit.rs + 各自 CJK 字体子集）。
# 宏：$1 = 应用名。
define app_rule
$(APPS)/$1/target/$(TARGET)/release/$1: $$(shell find $(APPS)/$1/src -name '*.rs') $(APPS)/$1/Cargo.toml $(APPS)/$1/build.rs $(APPS)/$1/linker.ld $(SHARED_SRC) $(TOPDIR)/shared/cjk_font_$1.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/$1/Cargo.toml
endef
$(foreach a,phone sms contacts browser clock calendar camera gallery music mail map weather cloud video notes,$(eval $(call app_rule,$a)))

$(AUDIOTEST_ELF): $(shell find $(APPS)/audiotest/src -name '*.rs') $(APPS)/audiotest/Cargo.toml $(APPS)/audiotest/build.rs $(APPS)/audiotest/linker.ld $(TOPDIR)/shared/audio.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/audiotest/Cargo.toml

# 浏览器 TLS 栈来宾侧自测（复用 apps/browser/src/tls/）
$(TLSTEST_ELF): $(shell find $(APPS)/tlstest/src -name '*.rs') $(shell find $(APPS)/browser/src/tls -name '*.rs') $(APPS)/tlstest/Cargo.toml $(APPS)/tlstest/build.rs $(APPS)/tlstest/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/tlstest/Cargo.toml

$(AUDIOMIX_ELF): $(shell find $(APPS)/audiomix/src -name '*.rs') $(APPS)/audiomix/Cargo.toml $(APPS)/audiomix/build.rs $(APPS)/audiomix/linker.ld $(TOPDIR)/shared/audio.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/audiomix/Cargo.toml

$(AUDIOLAT_ELF): $(shell find $(APPS)/audiolat/src -name '*.rs') $(APPS)/audiolat/Cargo.toml $(APPS)/audiolat/build.rs $(APPS)/audiolat/linker.ld $(TOPDIR)/shared/audio.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/audiolat/Cargo.toml

$(PKG_ELF): $(shell find $(KUNXPKG)/src -name '*.rs') $(KUNXPKG)/Cargo.toml $(KUNXPKG)/build.rs $(KUNXPKG)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXPKG)/Cargo.toml

# busybox-lite：手写 C，交叉编译为独立 ELF（内核 ELF 加载器直接加载）。
BUSYBOX_LD := $(KUNXCORE)/apps/hello.ld
$(BUSYBOX): $(KUNXCORE)/apps/busybox.c $(BUSYBOX_LD)
	clang --target=riscv64 -march=rv64gc -mabi=lp64d -mcmodel=medany -nostdlib -ffreestanding -fno-stack-protector -O2 -static \
		-Wl,-T,$(BUSYBOX_LD),-z,max-page-size=4096 -fuse-ld=lld -o $@ $<

init: $(INIT_ELF)
networkd: $(NET_ELF)

# 无线协议栈共享库（wifid / btd / locationd 共用）：宿主单元测试（QEMU 逻辑测试）。
test-wireless:
	cargo test --manifest-path $(WIRELESS)/Cargo.toml

wifid: $(WIFID_ELF)
rild: $(RILD_ELF)
modemtest: $(MODEMTEST_ELF)
sfsrace: $(SFSRACE_ELF)
logd: $(LOGD_ELF)
audiod: $(AUDIOD_ELF)
camerad: $(CAMERAD_ELF)
sensord: $(SENSORD_ELF)
positiond: $(POSITIOND_ELF)
lifd: $(LIFD_ELF)
pushd: $(PUSHD_ELF)
alarmd: $(ALARMD_ELF)
# lcapp：生命周期演示/测试应用（docs/lifecycle-test.md 的 GUI 验证载体）。
$(LCAPP_ELF): $(shell find $(LCAPP)/src -name '*.rs') $(LCAPP)/Cargo.toml $(LCAPP)/build.rs $(LCAPP)/linker.ld $(SHARED_SRC)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(LCAPP)/Cargo.toml
lcapp: $(LCAPP_ELF)

launcher: $(LAUNCHER_ELF) $(SETTINGS_ELF) $(TERMINAL_ELF) $(CALC_ELF) $(STORE_ELF) $(FILES_ELF) \
	$(PHONE_ELF) $(SMS_ELF) $(CONTACTS_ELF) $(BROWSER_ELF) $(CLOCK_ELF) $(CALENDAR_ELF) $(CAMERA_ELF) \
	$(GALLERY_ELF) $(MUSIC_ELF) $(MAIL_ELF) $(MAP_ELF) $(WEATHER_ELF) $(CLOUD_ELF) $(VIDEO_ELF) $(NOTES_ELF) $(LCAPP_ELF)
oobe: $(OOBE_ELF)
audiotools: $(AUDIOTEST_ELF) $(AUDIOMIX_ELF) $(AUDIOLAT_ELF)
pkg: $(PKG_ELF)
store: $(STORE_ELF)

# ---- 恢复 ramdisk（RECOVRD.IMG 载荷） ----
$(RECOV_ELF): $(shell find $(RECOVERY)/src -name '*.rs') $(RECOVERY)/Cargo.toml $(RECOVERY)/build.rs $(RECOVERY)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(RECOVERY)/Cargo.toml

RECOV_RAMDISK := $(KUNXCORE)/recovdisk.img
RECOV_STAGE  := /tmp/kunxos-recov-rootfs
$(RECOV_RAMDISK): $(RECOV_ELF) $(TOOLS)/mkfs_sfs.py
	@rm -rf $(RECOV_STAGE)
	@mkdir -p $(RECOV_STAGE)/bin
	@cp -f $(RECOV_ELF) $(RECOV_STAGE)/bin/recovery
	@python3 $(TOOLS)/mkfs_sfs.py $(RECOV_STAGE) $@ 4096

recovery-ramdisk: $(RECOV_RAMDISK)

# ----  根文件系统 / ramdisk 映像  ----
# 把 init / networkd / launcher / 测试应用 / busybox 复制进 rootfs，再打包成 SFS ramdisk 镜像。

# 软件源（本地 file:// + 示例 https 条目，索引为 Ed25519 签名索引）。
# 另含一个「篡改签名」的坏仓库 repo/bad：pkg update 必须拒绝它（验签强制）。
$(ROOTFS)/var/lib/pkg/sources.list: $(ROOTFS)/var/lib/pkg/repo/bad/Packages.sig
	@mkdir -p $(dir $@)
	@{ \
		echo 'file:///var/lib/pkg/repo/main'; \
		echo 'file:///var/lib/pkg/repo/bad'; \
		echo '# https://repo.kunxos.example/kunx   # 内核无 TLS，https 源当前会被明确拒绝'; \
	} > $@

# 坏仓库：Packages 有效、Packages.sig 为随机 64 字节（验签必须失败）。
$(ROOTFS)/var/lib/pkg/repo/bad/Packages.sig:
	@mkdir -p $(dir $@)
	@{ \
		echo 'Package: evil'; \
		echo 'Version: 9.9.9'; \
		echo 'Filename: evil-9.9.9.kunx'; \
	} > $(dir $@)Packages
	@python3 -c "import sys; sys.stdout.buffer.write(b'X'*64)" > $@

# 签名索引 + 示例 .kunx 包（tools/kunxrepo/*.py，纯 Python 零依赖）。
# 用真实应用 ELF 打包 calculator / terminal / notes，签发开发者证书，生成
# Packages + Packages.sig + Packages.meta（仓库密钥 Ed25519 签名）。
KUNXREPO := $(TOPDIR)/tools/kunxrepo
KUNX_DEMO_REPO := /tmp/kunx-demo-repo
$(ROOTFS)/var/lib/pkg/repo/main/.kunx-done: $(CALC_ELF) $(TERMINAL_ELF) $(NOTES_ELF) $(PKG_ELF) \
		$(KUNXREPO)/build_demo_repo.py $(KUNXREPO)/mkkunx.py $(KUNXREPO)/kunxrepo.py \
		$(KUNXREPO)/genkeys.py $(KUNXREPO)/ed25519.py \
		$(wildcard $(KUNXREPO)/manifests/*.json) $(wildcard $(KUNXREPO)/keys/*)
	@rm -rf $(KUNX_DEMO_REPO)
	@python3 $(KUNXREPO)/build_demo_repo.py $(KUNX_DEMO_REPO) $(TOPDIR)/apps
	@mkdir -p $(ROOTFS)/var/lib/pkg/repo/main
	@cp -r $(KUNX_DEMO_REPO)/* $(ROOTFS)/var/lib/pkg/repo/main/
	@touch $@

# 合并可用索引（pkg / store 的 AVAILABLE 路径）：与 Packages 同步生成。
$(ROOTFS)/var/lib/pkg/available: $(ROOTFS)/var/lib/pkg/repo/main/.kunx-done
	cp -f $(ROOTFS)/var/lib/pkg/repo/main/Packages $@

$(RAMDISK): $(INIT_ELF) $(NET_ELF) $(LOGD_ELF) $(CRASHME_ELF) $(KOSTEST_ELF) $(KOSDEMO_ELF) $(SENSORD_ELF) $(POSITIOND_ELF) $(LAUNCHER_ELF) $(SETTINGS_ELF) $(TERMINAL_ELF) $(CALC_ELF) $(OOBE_ELF) $(STORE_ELF) $(FILES_ELF) $(PHONE_ELF) $(SMS_ELF) $(CONTACTS_ELF) $(BROWSER_ELF) $(CLOCK_ELF) $(CALENDAR_ELF) $(CAMERA_ELF) $(GALLERY_ELF) $(MUSIC_ELF) $(MAIL_ELF) $(MAP_ELF) $(WEATHER_ELF) $(CLOUD_ELF) $(VIDEO_ELF) $(NOTES_ELF) $(LCAPP_ELF) $(PKG_ELF) $(AUDIOD_ELF) $(CAMERAD_ELF) $(LIFD_ELF) $(PUSHD_ELF) $(ALARMD_ELF) $(AUDIOTEST_ELF) $(AUDIOMIX_ELF) $(AUDIOLAT_ELF) $(TLSTEST_ELF) $(RILD_ELF) $(WIFID_ELF) $(MODEMTEST_ELF) $(SFSRACE_ELF) $(BUSYBOX) $(ROOTFS_FILES) $(FONT_SUBSET) $(FONT_BOLD_SUBSET) $(FONT_OOBE_CN_SUBSET) $(FONT_OOBE_BOLD_SUBSET) $(FONT_LAUNCHER_CN_SUBSET) $(FONT_LAUNCHER_BOLD_SUBSET) $(FONT_ICON_FILL_SUBSET) $(FONT_ICON_LINE_SUBSET) $(wildcard $(ROOTFS)/etc/oobe_default/*) $(wildcard $(ROOTFS)/var/www/*) $(wildcard $(ROOTFS)/sfsrace-test) $(wildcard $(ROOTFS)/etc/audio-test) $(TOOLS)/mkfs_sfs.py FORCE
	@mkdir -p $(ROOTFS)/bin $(ROOTFS)/dev $(ROOTFS)/etc/init.d $(ROOTFS)/proc \
		$(ROOTFS)/tmp $(ROOTFS)/var/run $(ROOTFS)/var/log $(ROOTFS)/data $(ROOTFS)/data/oobe \
		$(ROOTFS)/data/photos $(ROOTFS)/data/videos \
		$(ROOTFS)/var/lib/pkg/cache $(ROOTFS)/var/lib/pkg/cache/index \
		$(ROOTFS)/var/lib/pkg/installed $(ROOTFS)/var/lib/pkg/permissions \
		$(ROOTFS)/var/lib/pkg/reviewed $(ROOTFS)/var/lib/pkg/backup \
		$(ROOTFS)/data/app $(ROOTFS)/fonts
	# /data/oobe 必须为空目录：不预置 .done 或任何 OOBE 配置，保证首次启动进入向导。
	rm -f $(ROOTFS)/data/oobe/.done
	cp -f $(INIT_ELF) $(ROOTFS)/init
	cp -f $(NET_ELF) $(ROOTFS)/bin/networkd
	cp -f $(LOGD_ELF) $(ROOTFS)/bin/logd
	cp -f $(CRASHME_ELF) $(ROOTFS)/bin/crashme
	cp -f $(KOSTEST_ELF) $(ROOTFS)/bin/kostest
	cp -f $(KOSDEMO_ELF) $(ROOTFS)/bin/kosdemo
	cp -f $(AUDIOD_ELF) $(ROOTFS)/bin/audiod
	cp -f $(CAMERAD_ELF) $(ROOTFS)/bin/camerad
	cp -f $(SENSORD_ELF) $(ROOTFS)/bin/sensord
	cp -f $(POSITIOND_ELF) $(ROOTFS)/bin/positiond
	cp -f $(LIFD_ELF) $(ROOTFS)/bin/lifd
	cp -f $(PUSHD_ELF) $(ROOTFS)/bin/pushd
	cp -f $(ALARMD_ELF) $(ROOTFS)/bin/alarmd
	cp -f $(LAUNCHER_ELF) $(ROOTFS)/bin/launcher
	cp -f $(SETTINGS_ELF) $(ROOTFS)/bin/settings
	cp -f $(TERMINAL_ELF) $(ROOTFS)/bin/terminal
	cp -f $(CALC_ELF) $(ROOTFS)/bin/calculator
	cp -f $(OOBE_ELF) $(ROOTFS)/bin/oobe
	cp -f $(STORE_ELF) $(ROOTFS)/bin/store
	cp -f $(PHONE_ELF) $(ROOTFS)/bin/phone
	cp -f $(SMS_ELF) $(ROOTFS)/bin/sms
	cp -f $(CONTACTS_ELF) $(ROOTFS)/bin/contacts
	cp -f $(FILES_ELF) $(ROOTFS)/bin/files
	cp -f $(BROWSER_ELF) $(ROOTFS)/bin/browser
	cp -f $(CLOCK_ELF) $(ROOTFS)/bin/clock
	cp -f $(CALENDAR_ELF) $(ROOTFS)/bin/calendar
	cp -f $(CAMERA_ELF) $(ROOTFS)/bin/camera
	cp -f $(GALLERY_ELF) $(ROOTFS)/bin/gallery
	cp -f $(MUSIC_ELF) $(ROOTFS)/bin/music
	cp -f $(MAIL_ELF) $(ROOTFS)/bin/mail
	cp -f $(MAP_ELF) $(ROOTFS)/bin/map
	cp -f $(WEATHER_ELF) $(ROOTFS)/bin/weather
	cp -f $(CLOUD_ELF) $(ROOTFS)/bin/cloud
	cp -f $(VIDEO_ELF) $(ROOTFS)/bin/video
	cp -f $(NOTES_ELF) $(ROOTFS)/bin/notes
	cp -f $(LCAPP_ELF) $(ROOTFS)/bin/lcapp
	cp -f $(PKG_ELF) $(ROOTFS)/bin/pkg
	cp -f $(AUDIOTEST_ELF) $(ROOTFS)/bin/audiotest
	cp -f $(TLSTEST_ELF) $(ROOTFS)/bin/tlstest
	cp -f $(AUDIOMIX_ELF) $(ROOTFS)/bin/audiomix
	cp -f $(AUDIOLAT_ELF) $(ROOTFS)/bin/audiolat
	cp -f $(RILD_ELF) $(ROOTFS)/bin/rild
	cp -f $(WIFID_ELF) $(ROOTFS)/bin/wifid
	cp -f $(MODEMTEST_ELF) $(ROOTFS)/bin/modemtest
	cp -f $(SFSRACE_ELF) $(ROOTFS)/bin/sfsrace
	cp -f $(BUSYBOX) $(ROOTFS)/bin/busybox
	# 平滑字体子集：普通文字经 /fonts/cn.ttf（uiputong 子集）加载渲染；
	# 标题经 /fonts/bold.ttf（bold 子集）加载渲染。
	cp -f $(FONT_SUBSET) $(ROOTFS)/fonts/cn.ttf
	cp -f $(FONT_BOLD_SUBSET) $(ROOTFS)/fonts/bold.ttf
	# OOBE 向导专属超小子集（仅向导文案，~380KB）：首帧即平滑，不再有像素兜底。
	cp -f $(FONT_OOBE_CN_SUBSET) $(ROOTFS)/fonts/oobe-cn.ttf
	cp -f $(FONT_OOBE_BOLD_SUBSET) $(ROOTFS)/fonts/oobe-bold.ttf
	# 桌面启动器专属子集（launcher-cn/bold，~947 字形 410KB，仅桌面文案）：
	# 桌面首帧即平滑，TCG 慢宿主不再卡在「正在进入系统」等全量字体。
	cp -f $(FONT_LAUNCHER_CN_SUBSET) $(ROOTFS)/fonts/launcher-cn.ttf
	cp -f $(FONT_LAUNCHER_BOLD_SUBSET) $(ROOTFS)/fonts/launcher-bold.ttf
	# Phosphor 图标子集（icon-fill 实心 → 应用/天气图标；icon-line 粗线 →
	# 行内 emoji。码点表 shared/icons.rs，make_fonts.py 生成）。
	cp -f $(FONT_ICON_FILL_SUBSET) $(ROOTFS)/fonts/icon-fill.ttf
	cp -f $(FONT_ICON_LINE_SUBSET) $(ROOTFS)/fonts/icon-line.ttf
	# 桌面应用图标贴图（tools/make_icons.py 生成，launcher 启动时读取；
	# 不内嵌 ELF——263KB 会顶过内核 execve 的 EXEC_BUF_CAP）：
	mkdir -p $(ROOTFS)/share
	cp -f $(TOPDIR)/shared/app_icons.rgba $(ROOTFS)/share/app_icons.rgba
	# pkg / store 读取的合并可用索引（与 kunxpkg 的 AVAILABLE 路径一致）。
	cp -f $(ROOTFS)/var/lib/pkg/repo/main/Packages $(ROOTFS)/var/lib/pkg/available
	# 始终重跑 mkfs（镜像内容由 rootfs 树整体决定，无法用 make 依赖捕捉
	# 「标记文件被删除」这一状态变化）。先写 .tmp 再与已存在镜像比较：
	# 内容相同则不覆盖（保留 ramdisk 时间戳 → cargo 不重编），不同才替换
	# （自动处理 /sfsrace-test 标记的新增与删除，杜绝 run-gui 继承测试镜像）。
	python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK).tmp $(RAMDISK_BLOCKS)
	@cmp -s $(RAMDISK).tmp $(RAMDISK) && rm -f $(RAMDISK).tmp \
		|| mv -f $(RAMDISK).tmp $(RAMDISK)

rootfs: $(RAMDISK)

# 系统鼠标指针贴图：tubiao/shubiao.png → 24x24 RGBA8888 裸像素
# （shared/cursor_sprite.rs include_bytes! 内嵌，热点常量也在该模块）。
$(TOPDIR)/shared/cursor.rgba: $(TOPDIR)/tubiao/shubiao.png $(TOOLS)/make_cursor.py
	python3 $(TOOLS)/make_cursor.py

# FORCE：让 $(RAMDISK) 规则每次执行（配合上面的 tmp+cmp 使镜像只在内容变化时更新）。
.PHONY: FORCE
FORCE:

# ---- 内核（内嵌 ramdisk 镜像，按平台传递 feature） ----
# KOS 对象存储卷（内嵌第二 RAM 磁盘，内核启动时挂载到 KOS 引擎）。
KOSCTL    := $(TOPDIR)/kos/target/debug/kosctl
KOSDISK   := $(KUNXCORE)/kosdisk.img

kosctl:
	cd $(TOPDIR)/kos && CARGO_TARGET_DIR=$(TOPDIR)/kos/target cargo build --bin kosctl

# 持久 KOS 卷（QEMU 第二磁盘；重启数据保留）。256MB 稀疏文件。
KOS_DISK_IMG := $(TOPDIR)/kos-disk.img

KOS_SEED := $(TOOLS)/.kos-seed
KOS_TRUST := $(KUNXCORE)/src/kos_trust_pk.rs

kos-disk: kosctl
	@if [ ! -f $(KOS_SEED) ]; then \
		$(KOSCTL) keygen | awk '{print $$2}' > $(KOS_SEED); \
		echo "已生成 KOS 签名种子 $(KOS_SEED)（开发密钥，随仓库走）"; \
	fi
	$(KOSCTL) mkfs $(KOS_DISK_IMG).tmp 65536
	echo "persistent kos volume" > /tmp/kos-seed.txt
	$(KOSCTL) putm $(KOS_DISK_IMG).tmp /seed.txt /tmp/kos-seed.txt
	$(KOSCTL) append $(KOS_DISK_IMG).tmp /seed.txt /tmp/kos-seed.txt
	$(KOSCTL) sign $(KOS_DISK_IMG).tmp $$(cat $(KOS_SEED)) | awk '/trust-pk/{print $$2}' > /tmp/kos-pk.hex
	mv $(KOS_DISK_IMG).tmp $(KOS_DISK_IMG)
	@python3 -c "pk=open('/tmp/kos-pk.hex').read().strip(); \
	seed=open('$(KOS_SEED)').read().strip(); \
	pkb=', '.join('0x'+pk[i:i+2] for i in range(0,64,2)); \
	sdb=', '.join('0x'+seed[i:i+2] for i in range(0,64,2)); \
	open('$(KOS_TRUST)','w').write('// 由 make kos-disk 生成（tools/.kos-seed 派生）。勿手改。\n'+\
	'pub const KOS_TRUST_PK: [u8; 32] = ['+pkb+'];\n'+\
	'pub const KOS_SIGN_SEED: [u8; 32] = ['+sdb+'];\n')"
	@echo "kos-disk.img 已生成（256MB 持久卷，manifest 已签名，信任公钥已写入内核）"

kosdisk: kosctl
	$(KOSCTL) mkfs $(KOSDISK).tmp 1024
	echo "KOS volume seeded by make kosdisk" > /tmp/kos-seed.txt
	$(KOSCTL) put $(KOSDISK).tmp /seed.txt /tmp/kos-seed.txt
	mv $(KOSDISK).tmp $(KOSDISK)
	@echo "kosdisk.img 已生成（1024 块 = 4MB，KOS_MAJOR=254）"

kernel: $(RAMDISK) $(KOSDISK)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXCORE)/Cargo.toml $(CARGO_FEATURES)
	$(OBJCOPY) -O binary $(KERNEL_ELF) $(KERNEL_IMG)

# ---- 运行 ----
KOS_DRV := -drive file=$(KOS_DISK_IMG),format=raw,if=none,id=kosdisk -device virtio-blk-device,drive=kosdisk

run: kernel $(KOS_DISK_IMG)
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -append "$(QEMU_APPEND)" -nographic -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device \
		$(KOS_DRV) \
		-audiodev none,id=aud0 -device virtio-sound-device,audiodev=aud0

run-gui: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -append "$(QEMU_APPEND)" -serial stdio -device ramfb -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-keyboard-device -device virtio-tablet-device \
		-device virtio-rng-device \
		-audiodev none,id=aud0 -device virtio-sound-device,audiodev=aud0

# 调试模式：串口挂到 127.0.0.1:1234（宿主 gdb `target remote localhost:1234`，
# 先在内核 shell 执行 `gdb` 挂起内核，或直接在断点/崩溃处自动进入调试桩）。
run-debug: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -no-reboot \
		-serial tcp:127.0.0.1:1234,server,nowait -monitor none -display none \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device -audiodev none,id=aud0

# 输入注入模式：串口挂到 127.0.0.1:1234（配合 tools/input_inject.py 注入触摸事件）。
# 观察内核日志可另开终端运行 `python3 tools/input_inject.py console`。
run-input: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -no-reboot \
		-serial tcp:127.0.0.1:1234,server,nowait -monitor none -display none \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device \
		-audiodev none,id=aud0 -device virtio-sound-device,audiodev=aud0

# 音频验证模式（virtio-sound 首选）：输出到宿主 WAV 文件，供波形分析。
# 内核音频栈（/dev/audio + IRQ 环形缓冲）走 virtio-sound 低延迟路径。
run-audio: kernel
	rm -f /tmp/kunxos-audio.wav
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-audiodev wav,id=aud0,path=/tmp/kunxos-audio.wav \
		-device virtio-sound-device,audiodev=aud0 -device virtio-rng-device

# 电话/短信演示模式：GUI + virtio-sound（铃声/提示音）+ 虚拟调制解调器。
# 来电/短信经 /dev/modemctl 注入（shell: echo "CALL_IN:13800000001" > /dev/modemctl）。
run-phone: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -serial stdio -device ramfb -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-keyboard-device -device virtio-tablet-device \
		-device virtio-rng-device \
		-audiodev none,id=aud0 -device virtio-sound-device,audiodev=aud0

# 音频验证模式（AC97 遗留回退）：内核回退到 AC97 启动提示音路径。
run-audio-ac97: kernel
	rm -f /tmp/kunxos-audio.wav
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-audiodev wav,id=aud0,path=/tmp/kunxos-audio.wav \
		-device virtio-rng-device

# 音频验证模式（静音后端）：任意 run 目标 + virtio-sound 的便捷组合，
# 用 none 后端丢弃音频（QEMU 仍按模拟时钟消费，测量路径完整）。
run-audio-none: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-audiodev none,id=aud0 \
		-device virtio-sound-device,audiodev=aud0 -device virtio-rng-device

# ---- 电话/短信自动化测试 ----
# 构建带 /data/modemtest.flag 的 ramdisk → QEMU 无头运行 modemtest（rild 全链路
# 自检：拨号/来电/短信/紧急呼叫）→ 断言串口日志含 [MODEMTEST] PASS。
TEST_MODEM_LOG := /tmp/kunxos-modemtest.log

test-modem: $(RILD_ELF) $(MODEMTEST_ELF)
	@touch $(ROOTFS)/data/modemtest.flag
	@python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK) $(RAMDISK_BLOCKS)
	@$(MAKE) kernel
	@rm -f $(TEST_MODEM_LOG)
	@echo "== running modemtest in QEMU (headless) =="
	-timeout 240 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-device virtio-rng-device | tee $(TEST_MODEM_LOG)
	@rm -f $(ROOTFS)/data/modemtest.flag
	@grep -q "\[MODEMTEST\] PASS" $(TEST_MODEM_LOG) && echo "MODEM TEST: PASS" \
		|| (echo "MODEM TEST: FAIL (see $(TEST_MODEM_LOG))"; exit 1)

# ---- SFS v2 并发回归测试 ----
# 构建带 /sfsrace-test 标记的 ramdisk → QEMU 无头运行 /bin/sfsrace（6 进程
# 并发创建/读写/截断/删除 /var/run 文件，校验无 inode/块串线、无除零崩溃）→
# 断言串口日志含 "SFS-RACE PASS"。
TEST_SFSRACE_LOG := /tmp/kunxos-sfsrace.log

# ---- KOS 无缝复制回归（/kos/ POSIX 桥 × shell cp，三向 sha1 比对） ----
# 带持久 KOS 卷启动，shell 里 cp 三个方向（SFS→KOS / KOS→SFS / KOS→KOS），
# tools/koscp_check.py 判定四份 sha1 一致。幂等：目标名固定，write_mutable
# 原地换代，重复运行不受磁盘残留影响。
TEST_KOSCP_LOG := /tmp/kunxos-koscp.log

.PHONY: test-kos-copy
test-kos-copy: kernel $(KOS_DISK_IMG)
	@rm -f $(TEST_KOSCP_LOG)
	@echo "== running kos seamless-copy test in QEMU (headless) =="
	@-(sleep 12; \
		echo "kos mkdir /data"; sleep 1; \
		echo "cp /bin/hello /kos/data/from-sfs.txt"; sleep 2; \
		echo "sha1sum /bin/hello"; sleep 1; \
		echo "sha1sum /kos/data/from-sfs.txt"; sleep 1; \
		echo "cp /kos/data/from-sfs.txt /bin/back.txt"; sleep 2; \
		echo "sha1sum /bin/back.txt"; sleep 1; \
		echo "cp /kos/data/from-sfs.txt /kos/data/copy2.txt"; sleep 2; \
		echo "sha1sum /kos/data/copy2.txt"; sleep 1; \
		echo "kos cp /data/from-sfs.txt /data/kos-cp.txt"; sleep 2; \
		echo "sha1sum /kos/data/kos-cp.txt"; sleep 1; \
		echo "cp /bin/hello /kos/data/"; sleep 2; \
		echo "sha1sum /kos/data/hello"; sleep 1; \
		echo "poweroff"; sleep 4) | timeout 150 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-device virtio-rng-device $(KOS_DRV) | tee $(TEST_KOSCP_LOG)
	@python3 tools/koscp_check.py $(TEST_KOSCP_LOG)

test-sfsrace: $(SFSRACE_ELF)
	@touch $(ROOTFS)/sfsrace-test
	@python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK) $(RAMDISK_BLOCKS)
	# mkfs 与上次 cargo 编译同秒时，mtime 粒度内 cargo 误判 Fresh → 测试标记
	# 不重嵌进 kernel.img（间歇性 FAIL 根因）。强制 mtime 前进触发重编。
	@touch $(RAMDISK)
	@$(MAKE) kernel
	@rm -f $(TEST_SFSRACE_LOG)
	@echo "== running sfsrace in QEMU (headless) =="
	-timeout 240 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-device virtio-rng-device | tee $(TEST_SFSRACE_LOG)
	# 无论 QEMU 退出码（测试失败时内核自动关机使 QEMU 非零退出），都必须移除标记，
	# 否则 rootfs 遗留 /sfsrace-test → 下次 make run-gui 一启动就进测试模式并关机。
	@rm -f $(ROOTFS)/sfsrace-test
	# 判定门必须同时含 SFS-RACE 与 MAILBOX：同一二进制本就两阶段都跑，
	# 只查前者曾让信箱阶段确定性假失败漏网一整晚（M1 教训）。
	@grep -q "SFS-RACE PASS" $(TEST_SFSRACE_LOG) && grep -q "MAILBOX PASS" $(TEST_SFSRACE_LOG) && echo "SFS RACE TEST: PASS" \
		|| (echo "SFS RACE TEST: FAIL (see $(TEST_SFSRACE_LOG))"; exit 1)

# ---- 并发信箱压测（rename 原子提交/认领） ----
# 复用 sfsrace 二进制（内含信箱压测阶段）：阶段A 3 写者 O_APPEND 并发追加 +
# 父进程 rename 认领消费（断言无丢失/无重复/无半行/轮询有界=无死锁）；
# 阶段B 1 写者 tmp+fsync+rename 原子发布 + 1 读者并发整读（断言无撕裂、
# 版本单调、终值可见）。断言串口日志同时含 "SFS-RACE PASS" 与 "MAILBOX PASS"。
TEST_MAILBOX_LOG := /tmp/kunxos-mailbox.log

test-mailbox: $(SFSRACE_ELF)
	@touch $(ROOTFS)/sfsrace-test
	@python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK) $(RAMDISK_BLOCKS)
	@$(MAKE) kernel
	@rm -f $(TEST_MAILBOX_LOG)
	@echo "== running mailbox stress in QEMU (headless) =="
	-timeout 240 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-device virtio-rng-device | tee $(TEST_MAILBOX_LOG)
	@rm -f $(ROOTFS)/sfsrace-test
	@grep -q "SFS-RACE PASS" $(TEST_MAILBOX_LOG) && grep -q "MAILBOX PASS" $(TEST_MAILBOX_LOG) \
		&& echo "MAILBOX TEST: PASS" \
		|| (echo "MAILBOX TEST: FAIL (see $(TEST_MAILBOX_LOG))"; exit 1)

# ---- 音频自动化测试（audiomix 混音/焦点 + audiolat 延迟） ----
# 默认镜像不含 /etc/audio-test 标记（每次开机串行跑 audiomix+audiolat 5 会
# 拖慢启动）；仅在测试时注入→构建 ramdisk→QEMU 无头运行。结束无论成败都必须
# 移除标记，否则下次 make run-gui 一启动就进音频测试模式。
TEST_AUDIO_LOG := /tmp/kunxos-audio-test.log

test-audio: $(AUDIOMIX_ELF) $(AUDIOLAT_ELF)
	@touch $(ROOTFS)/etc/audio-test
	@python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK) $(RAMDISK_BLOCKS)
	@$(MAKE) kernel
	@rm -f $(TEST_AUDIO_LOG)
	@echo "== running audio test in QEMU (headless) =="
	-timeout 360 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-audiodev none,id=aud0 -device virtio-sound-device,audiodev=aud0 \
		-device virtio-rng-device | tee $(TEST_AUDIO_LOG)
	@rm -f $(ROOTFS)/etc/audio-test
	@if grep -q "audiomix.*已退出" $(TEST_AUDIO_LOG) && grep -q "audiolat.*已退出" $(TEST_AUDIO_LOG); \
		then echo "AUDIO TEST: PASS"; \
		else echo "AUDIO TEST: FAIL (see $(TEST_AUDIO_LOG))"; exit 1; fi

# C PDU 编解码库主机单元测试。
test-pdu:
	gcc -O2 -Wall -I $(RILD)/src/pdu -o /tmp/test_pdu tests/scripts/test_pdu.c $(RILD)/src/pdu/pdu.c
	/tmp/test_pdu

# QEMU 转储机器 DTB（内核设备树来源）。
$(DTB):
	$(QEMU) -M virt,accel=$(QEMU_ACCEL),dumpdtb=$@ -cpu rv64 -m 512M -nographic >/dev/null 2>&1 || true

$(DISK_IMG): $(KERNEL_IMG) $(DTB) $(RAMDISK) $(RECOV_RAMDISK) $(TOOLS)/mkdisk.py $(TOOLS)/mkfs_sfs.py
	python3 $(TOOLS)/mkdisk.py $(KERNEL_IMG) $(DTB) $(RAMDISK) $(RECOV_RAMDISK) $(ROOTFS) $(DISK_IMG) --boot-key $(TOOLS)/kunxsec/keys/boot.sk --build 42

# 构建带分区表（MBR + boot FAT32 + rootfs SFS + data SFS）的磁盘镜像。
disk: $(DISK_IMG)

# 以当前方式（-kernel）直接加载内核，并挂载磁盘镜像验证 /data 持久化。
run-disk: kernel $(DISK_IMG)
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -smp 2 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) \
		-drive file=$(DISK_IMG),format=raw,if=none,id=disk0 \
		-device virtio-blk-device,drive=disk0 \
		-device virtio-rng-device -audiodev none,id=aud0 \
		-nographic -no-reboot

# SMP 多核（-smp 2）：RISC-V 上所有 hart 经 OpenSBI 直接进入内核 _start，
# 非 boot hart 在停车场等待 SCHED_READY（无需 PSCI / loader 唤醒）。
# 副核每核 idle/tick + 全局就绪队列负载均衡，verbose 启动可见：
#   [smp] cpus=2（secondary 停车场等待调度就绪） / [smp] cpu1 已上线
run-smp: kernel
	$(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -smp 2 -m 512M -kernel $(KERNEL_IMG) \
		-append "verbose" -nographic -no-reboot

# ---- Wi-Fi 无头自检 ----
# QEMU `-append "wifitest"`：内核跑 wifi::selftest（扫描 → WPA2-PSK 4-way →
# 正确口令连上 + 错误口令被 AP 拒绝），串口断言 [wifi-selftest] PASS。
TEST_WIFI_LOG := /tmp/kunxos-wifi.log

test-wifi: kernel
	@echo "== wifi selftest in QEMU (headless) =="
	@rm -f $(TEST_WIFI_LOG)
	@-timeout 120 $(QEMU) -M virt,accel=$(QEMU_ACCEL) -cpu rv64 -bios $(OPENBI) -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-append "wifitest" -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device | tee $(TEST_WIFI_LOG)
	@grep -q "\[wifi-selftest\] PASS" $(TEST_WIFI_LOG) && echo "WIFI SELFTEST: PASS" \
		|| (echo "WIFI SELFTEST: FAIL (see $(TEST_WIFI_LOG))"; exit 1)

# ---- 电源管理测试 ----
# 构建内核后在 QEMU 上跑 power selftest / 热注入节流 / 调速器 / 电池状态机回归。
power-test: kernel
	python3 tests/scripts/power_test.py /tmp/kx_power.log

# ---- 生命周期测试（docs/lifecycle-test.md） ----
# 无头完整回归 + 命令信箱背靠背连发（cmd_<pid>_<seq> 生产协议不丢命令）。
# 两个都绿才算过：前者覆盖状态机全链路，后者守护 IPC 原子发布语义。
lifecycle-test: kernel
	python3 tests/scripts/lifecycle_test.py /tmp/kx_life.log
	python3 tests/scripts/lifecycle_cmd_burst.py /tmp/kx_life_burst.log

# 无头核心回归（快速、确定性）：仅串口 shell 驱动 /data/life 文件信箱与
# /proc/pressure 注入，直接验证 lifd 状态机（R/B/S/T）、SIGSTOP/SIGCONT、
# 后台任务白名单与超时、通知转发与延迟补投，不经过 GUI。
lifecycle-core-test: kernel
	python3 tests/scripts/lifecycle_core_test.py /tmp/kx_life_core.log

# ---- 清理 ----
clean:
	$(CARGO) clean --manifest-path $(KUNXCORE)/Cargo.toml
	$(CARGO) clean --manifest-path $(KUNXINIT)/Cargo.toml
	$(CARGO) clean --manifest-path $(NETWORKD)/Cargo.toml
	$(CARGO) clean --manifest-path $(WIFID)/Cargo.toml
	$(CARGO) clean --manifest-path $(LOGD)/Cargo.toml
	$(CARGO) clean --manifest-path $(AUDIOD)/Cargo.toml
	$(CARGO) clean --manifest-path $(LAUNCHER)/Cargo.toml
	$(CARGO) clean --manifest-path $(APPS)/audiotest/Cargo.toml
	$(CARGO) clean --manifest-path $(APPS)/audiomix/Cargo.toml
	$(CARGO) clean --manifest-path $(APPS)/audiolat/Cargo.toml
	$(CARGO) clean --manifest-path $(SETTINGS)/Cargo.toml
	$(CARGO) clean --manifest-path $(TERMINAL)/Cargo.toml
	$(CARGO) clean --manifest-path $(CALC)/Cargo.toml
	$(CARGO) clean --manifest-path $(OOBE)/Cargo.toml
	$(CARGO) clean --manifest-path $(STORE)/Cargo.toml
	$(CARGO) clean --manifest-path $(FILES)/Cargo.toml
	$(CARGO) clean --manifest-path $(PHONE)/Cargo.toml
	$(CARGO) clean --manifest-path $(SMS)/Cargo.toml
	$(CARGO) clean --manifest-path $(CONTACTS)/Cargo.toml
	$(CARGO) clean --manifest-path $(BROWSER)/Cargo.toml
	$(CARGO) clean --manifest-path $(CLOCK)/Cargo.toml
	$(CARGO) clean --manifest-path $(CALENDAR)/Cargo.toml
	$(CARGO) clean --manifest-path $(CAMERA)/Cargo.toml
	$(CARGO) clean --manifest-path $(GALLERY)/Cargo.toml
	$(CARGO) clean --manifest-path $(MUSIC)/Cargo.toml
	$(CARGO) clean --manifest-path $(MAIL)/Cargo.toml
	$(CARGO) clean --manifest-path $(MAP)/Cargo.toml
	$(CARGO) clean --manifest-path $(WEATHER)/Cargo.toml
	$(CARGO) clean --manifest-path $(KUNXPKG)/Cargo.toml
	$(CARGO) clean --manifest-path $(ALARMD)/Cargo.toml
	$(CARGO) clean --manifest-path $(LCAPP)/Cargo.toml
	rm -f $(RAMDISK) $(KERNEL_IMG) $(ROOTFS)/init $(ROOTFS)/bin/networkd \
		$(ROOTFS)/bin/lcapp \
		$(ROOTFS)/bin/logd \
		$(ROOTFS)/bin/audiod $(ROOTFS)/bin/audiotest $(ROOTFS)/bin/audiomix $(ROOTFS)/bin/audiolat \
		$(ROOTFS)/bin/launcher $(ROOTFS)/bin/settings \
		$(ROOTFS)/bin/terminal $(ROOTFS)/bin/calculator $(ROOTFS)/bin/busybox \
		$(ROOTFS)/bin/alarmd \
$(ROOTFS)/bin/oobe $(ROOTFS)/bin/store $(ROOTFS)/bin/pkg \
	$(ROOTFS)/bin/wifid $(ROOTFS)/bin/phone $(ROOTFS)/bin/sms $(ROOTFS)/bin/contacts \
		$(ROOTFS)/bin/files $(ROOTFS)/bin/browser $(ROOTFS)/bin/clock \
		$(ROOTFS)/bin/calendar $(ROOTFS)/bin/camera $(ROOTFS)/bin/gallery \
		$(ROOTFS)/bin/music $(ROOTFS)/bin/mail $(ROOTFS)/bin/map \
		$(ROOTFS)/bin/weather \
		$(DTB) $(DISK_IMG)
