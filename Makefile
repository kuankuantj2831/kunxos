# KunxOS 顶层构建脚本：内核 + init + 系统服务 + rootfs（ramdisk）+ QEMU 启动。
#
#   make kernel   编译内核（内嵌 ramdisk 镜像）
#   make init     编译用户态 init（Rust，kunxinit -> rootfs/init）
#   make networkd 编译网络守护进程（kunxserv/networkd）
#   make displayd 编译显示守护进程（已废弃：launcher 直接操作 /dev/fb0）
#   make launcher 编译桌面启动器 + 测试应用（apps/launcher、apps/settings 等）
#   make rootfs   把 init/networkd/displayd/launcher/apps/busybox 复制进 rootfs 并打包 ramdisk
#   make run      构建全部并用 QEMU 启动
#   make boot     构建内核 + bootloader + 磁盘镜像，经 virtio-blk 由 bootloader 加载内核
#   make boot-pi4 编译 Raspberry Pi 4 bootloader 后端（编译检查）
#   make pi4      编译 Raspberry Pi 4 后端（编译检查，--features pi4）
#   make clean    清理所有构建产物

TOPDIR    := $(CURDIR)
KUNXCORE  := $(TOPDIR)/kunxcore
KUNXINIT  := $(TOPDIR)/kunxinit
KUNXSERV  := $(TOPDIR)/kunxserv
NETWORKD  := $(KUNXSERV)/networkd
DISPLAYD  := $(KUNXSERV)/displayd
APPS      := $(TOPDIR)/apps
LAUNCHER  := $(APPS)/launcher
SETTINGS  := $(APPS)/settings
TERMINAL  := $(APPS)/terminal
CALC      := $(APPS)/calculator
OOBE      := $(APPS)/oobe
STORE     := $(APPS)/store
STUB      := $(APPS)/stub
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
KUNXPKG   := $(TOPDIR)/kunxpkg
ROOTFS    := $(TOPDIR)/rootfs
TOOLS     := $(TOPDIR)/tools
KUNXBOOT  := $(TOPDIR)/kunxboot

TARGET     := aarch64-unknown-none
KERNEL_BIN := rust-kernel
KERNEL_ELF := $(KUNXCORE)/target/$(TARGET)/release/$(KERNEL_BIN)
KERNEL_IMG := $(KUNXCORE)/kernel.img
INIT_ELF   := $(KUNXINIT)/target/$(TARGET)/release/kunxinit
NET_ELF    := $(NETWORKD)/target/$(TARGET)/release/networkd
DISP_ELF   := $(DISPLAYD)/target/$(TARGET)/release/displayd
LAUNCHER_ELF := $(LAUNCHER)/target/$(TARGET)/release/launcher
SETTINGS_ELF := $(SETTINGS)/target/$(TARGET)/release/settings
TERMINAL_ELF := $(TERMINAL)/target/$(TARGET)/release/terminal
CALC_ELF     := $(CALC)/target/$(TARGET)/release/calculator
OOBE_ELF     := $(OOBE)/target/$(TARGET)/release/oobe
STORE_ELF    := $(STORE)/target/$(TARGET)/release/store
STUB_ELF     := $(STUB)/target/$(TARGET)/release/stub
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
PKG_ELF      := $(KUNXPKG)/target/$(TARGET)/release/kunxpkg
BUSYBOX    := $(KUNXCORE)/apps/busybox.elf
RAMDISK    := $(KUNXCORE)/ramdisk.img
BOOT_IMG   := $(KUNXBOOT)/bootloader.img
DTB        := $(TOPDIR)/kunxos.dtb
DISK_IMG   := $(TOPDIR)/disk.img

ROOTFS_FILES := \
	$(ROOTFS)/etc/inittab \
	$(ROOTFS)/etc/init.d/S01-network \
	$(ROOTFS)/etc/init.d/S03-launcher \
	$(ROOTFS)/var/lib/pkg/sources.list \
	$(ROOTFS)/var/lib/pkg/repo/main/Packages

CARGO   ?= cargo
OBJCOPY ?= llvm-objcopy
QEMU    ?= qemu-system-aarch64

# 平台选择：make（默认 QEMU），make pi4（Raspberry Pi 4）。
PLATFORM ?= qemu
ifeq ($(PLATFORM),pi4)
  CARGO_FEATURES := --no-default-features --features pi4
else
  CARGO_FEATURES := --features qemu
endif

.PHONY: all kernel pi4 init networkd displayd launcher oobe pkg store rootfs run run-gui run-input boot boot-pi4 disk run-disk clean

all: kernel

# ---- 用户态 ELF（Rust no_std，内核 ELF 加载器直接加载，无需 objcopy） ----
$(INIT_ELF): $(shell find $(KUNXINIT)/src -name '*.rs') $(KUNXINIT)/Cargo.toml $(KUNXINIT)/build.rs $(KUNXINIT)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXINIT)/Cargo.toml

$(NET_ELF): $(shell find $(NETWORKD)/src -name '*.rs') $(NETWORKD)/Cargo.toml $(NETWORKD)/build.rs $(NETWORKD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(NETWORKD)/Cargo.toml

$(DISP_ELF): $(shell find $(DISPLAYD)/src -name '*.rs') $(DISPLAYD)/Cargo.toml $(DISPLAYD)/build.rs $(DISPLAYD)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(DISPLAYD)/Cargo.toml

$(LAUNCHER_ELF): $(shell find $(LAUNCHER)/src -name '*.rs') $(LAUNCHER)/Cargo.toml $(LAUNCHER)/build.rs $(LAUNCHER)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(LAUNCHER)/Cargo.toml

$(SETTINGS_ELF): $(shell find $(SETTINGS)/src -name '*.rs') $(SETTINGS)/Cargo.toml $(SETTINGS)/build.rs $(SETTINGS)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(SETTINGS)/Cargo.toml

$(TERMINAL_ELF): $(shell find $(TERMINAL)/src -name '*.rs') $(TERMINAL)/Cargo.toml $(TERMINAL)/build.rs $(TERMINAL)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(TERMINAL)/Cargo.toml

$(CALC_ELF): $(shell find $(CALC)/src -name '*.rs') $(CALC)/Cargo.toml $(CALC)/build.rs $(CALC)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(CALC)/Cargo.toml

$(OOBE_ELF): $(shell find $(OOBE)/src -name '*.rs') $(OOBE)/Cargo.toml $(OOBE)/build.rs $(OOBE)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(OOBE)/Cargo.toml

$(STORE_ELF): $(shell find $(STORE)/src -name '*.rs') $(STORE)/Cargo.toml $(STORE)/build.rs $(STORE)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(STORE)/Cargo.toml

$(STUB_ELF): $(shell find $(STUB)/src -name '*.rs') $(STUB)/Cargo.toml $(STUB)/build.rs $(STUB)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(STUB)/Cargo.toml

$(FILES_ELF): $(shell find $(FILES)/src -name '*.rs') $(FILES)/Cargo.toml $(FILES)/build.rs $(FILES)/linker.ld $(TOPDIR)/shared/cjk_font_files.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(FILES)/Cargo.toml

# 桌面应用（真实实现，共用 shared/app_kit.rs + 各自 CJK 字体子集）。
# 宏：$1 = 应用名。
define app_rule
$(APPS)/$1/target/$(TARGET)/release/$1: $$(shell find $(APPS)/$1/src -name '*.rs') $(APPS)/$1/Cargo.toml $(APPS)/$1/build.rs $(APPS)/$1/linker.ld $(TOPDIR)/shared/app_kit.rs $(TOPDIR)/shared/cjk_font_$1.rs
	$(CARGO) build --release --target $(TARGET) --manifest-path $(APPS)/$1/Cargo.toml
endef
$(foreach a,phone sms contacts browser clock calendar camera gallery music mail map weather,$(eval $(call app_rule,$a)))

$(PKG_ELF): $(shell find $(KUNXPKG)/src -name '*.rs') $(KUNXPKG)/Cargo.toml $(KUNXPKG)/build.rs $(KUNXPKG)/linker.ld
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXPKG)/Cargo.toml

# busybox-lite：手写 C，交叉编译为独立 ELF（内核 ELF 加载器直接加载）。
BUSYBOX_LD := $(KUNXCORE)/apps/hello.ld
$(BUSYBOX): $(KUNXCORE)/apps/busybox.c $(BUSYBOX_LD)
	aarch64-linux-gnu-gcc -nostdlib -ffreestanding -fno-stack-protector -O2 -static \
		-Wl,-T,$(BUSYBOX_LD),-z,max-page-size=4096 -o $@ $<

init: $(INIT_ELF)
networkd: $(NET_ELF)
displayd: $(DISP_ELF)
launcher: $(LAUNCHER_ELF) $(SETTINGS_ELF) $(TERMINAL_ELF) $(CALC_ELF) $(STORE_ELF) $(STUB_ELF) $(FILES_ELF) \
	$(PHONE_ELF) $(SMS_ELF) $(CONTACTS_ELF) $(BROWSER_ELF) $(CLOCK_ELF) $(CALENDAR_ELF) $(CAMERA_ELF) \
	$(GALLERY_ELF) $(MUSIC_ELF) $(MAIL_ELF) $(MAP_ELF) $(WEATHER_ELF)
oobe: $(OOBE_ELF)
pkg: $(PKG_ELF)
store: $(STORE_ELF)

# ---- rootfs / ramdisk ----
# 把 init / networkd / displayd / launcher / 测试应用 / busybox 复制进 rootfs，再打包成 SFS ramdisk 镜像。

# 软件源（本地 file://）与示例 Packages 索引。
$(ROOTFS)/var/lib/pkg/sources.list:
	@mkdir -p $(dir $@)
	@echo 'file:///var/lib/pkg/repo/main' > $@

$(ROOTFS)/var/lib/pkg/repo/main/Packages:
	@mkdir -p $(dir $@)
	@{ \
		echo 'Package: calculator'; \
		echo 'Version: 1.0.0'; \
		echo 'Size: 8472'; \
		echo 'Description: Simple calculator app'; \
		echo 'Filename: calculator.pkg'; \
		echo ''; \
		echo 'Package: terminal'; \
		echo 'Version: 1.1.0'; \
		echo 'Size: 12000'; \
		echo 'Description: System terminal emulator'; \
		echo 'Filename: terminal.pkg'; \
		echo ''; \
		echo 'Package: editor'; \
		echo 'Version: 0.9.0'; \
		echo 'Size: 9216'; \
		echo 'Description: Lightweight text editor'; \
		echo 'Filename: editor.pkg'; \
		echo 'Depends: libc, libm'; \
	} > $@

$(RAMDISK): $(INIT_ELF) $(NET_ELF) $(LAUNCHER_ELF) $(SETTINGS_ELF) $(TERMINAL_ELF) $(CALC_ELF) $(OOBE_ELF) $(STORE_ELF) $(STUB_ELF) $(FILES_ELF) $(PHONE_ELF) $(SMS_ELF) $(CONTACTS_ELF) $(BROWSER_ELF) $(CLOCK_ELF) $(CALENDAR_ELF) $(CAMERA_ELF) $(GALLERY_ELF) $(MUSIC_ELF) $(MAIL_ELF) $(MAP_ELF) $(WEATHER_ELF) $(PKG_ELF) $(BUSYBOX) $(ROOTFS_FILES) $(wildcard $(ROOTFS)/etc/oobe_default/*) $(wildcard $(ROOTFS)/var/www/*) $(TOOLS)/mkfs_sfs.py
	@mkdir -p $(ROOTFS)/bin $(ROOTFS)/dev $(ROOTFS)/etc/init.d $(ROOTFS)/proc \
		$(ROOTFS)/tmp $(ROOTFS)/var/run $(ROOTFS)/var/log $(ROOTFS)/data $(ROOTFS)/data/oobe \
		$(ROOTFS)/var/lib/pkg/cache $(ROOTFS)/var/lib/pkg/installed
	# /data/oobe 必须为空目录：不预置 .done 或任何 OOBE 配置，保证首次启动进入向导。
	rm -f $(ROOTFS)/data/oobe/.done
	cp -f $(INIT_ELF) $(ROOTFS)/init
	cp -f $(NET_ELF) $(ROOTFS)/bin/networkd
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
	cp -f $(PKG_ELF) $(ROOTFS)/bin/pkg
	cp -f $(BUSYBOX) $(ROOTFS)/bin/busybox
	python3 $(TOOLS)/mkfs_sfs.py $(ROOTFS) $(RAMDISK)

rootfs: $(RAMDISK)

# ---- 内核（内嵌 ramdisk 镜像，按平台传递 feature） ----
kernel: $(RAMDISK)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXCORE)/Cargo.toml $(CARGO_FEATURES)
	$(OBJCOPY) -O binary $(KERNEL_ELF) $(KERNEL_IMG)

# Raspberry Pi 4 后端编译检查：仅 cargo build，不覆盖 kernel.img。
pi4: $(RAMDISK)
	$(CARGO) build --release --target $(TARGET) --manifest-path $(KUNXCORE)/Cargo.toml \
		--no-default-features --features pi4

# ---- 运行 ----
run: kernel
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device -device AC97

run-gui: kernel
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL_IMG) -serial stdio -device ramfb -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-keyboard-device -device virtio-tablet-device \
		-device virtio-rng-device -device AC97

# 输入注入模式：串口挂到 127.0.0.1:1234（配合 tools/input_inject.py 注入触摸事件）。
# 观察内核日志可另开终端运行 `python3 tools/input_inject.py console`。
run-input: kernel
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL_IMG) -no-reboot \
		-serial tcp:127.0.0.1:1234,server,nowait -monitor none -display none \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0 \
		-device virtio-rng-device -device AC97

# 音频验证模式：AC97 输出到宿主 WAV 文件（启动提示音后检查波形）。
run-audio: kernel
	rm -f /tmp/kunxos-audio.wav
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL_IMG) -nographic -no-reboot \
		-audiodev wav,id=aud0,path=/tmp/kunxos-audio.wav \
		-device AC97,audiodev=aud0 -device virtio-rng-device

# ---- Bootloader ----
# 编译 bootloader，生成磁盘镜像（MBR + FAT32），由 bootloader 从 virtio-blk
# 读取 KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG 并跳转内核。
$(BOOT_IMG): $(shell find $(KUNXBOOT)/src -name '*.rs') $(KUNXBOOT)/Cargo.toml $(KUNXBOOT)/build.rs $(KUNXBOOT)/linker.ld $(KUNXBOOT)/linker_pi4.ld
	$(MAKE) -C $(KUNXBOOT) all

# QEMU 转储机器 DTB（内核设备树来源）。
$(DTB):
	$(QEMU) -M virt,dumpdtb=$@ -cpu cortex-a72 -m 512M -nographic >/dev/null 2>&1 || true

$(DISK_IMG): $(KERNEL_IMG) $(DTB) $(RAMDISK) $(TOOLS)/mkdisk.py $(TOOLS)/mkfs_sfs.py
	python3 $(TOOLS)/mkdisk.py $(KERNEL_IMG) $(DTB) $(RAMDISK) $(ROOTFS) $(DISK_IMG)

# 构建带分区表（MBR + boot FAT32 + rootfs SFS + data SFS）的磁盘镜像。
disk: $(DISK_IMG)

boot: kernel $(BOOT_IMG) $(DISK_IMG)
	$(QEMU) -M virt -cpu cortex-a72 -m 512M \
		-device loader,file=$(BOOT_IMG),addr=0x40200000,cpu-num=0 \
		-drive file=$(DISK_IMG),format=raw,if=none,id=disk0 \
		-device virtio-blk-device,drive=disk0 \
		-device virtio-rng-device -device AC97 \
		-nographic -no-reboot

# 以当前方式（-kernel）直接加载内核，并挂载磁盘镜像验证 /data 持久化。
run-disk: kernel $(DISK_IMG)
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL_IMG) \
		-drive file=$(DISK_IMG),format=raw,if=none,id=disk0 \
		-device virtio-blk-device,drive=disk0 \
		-device virtio-rng-device -device AC97 \
		-nographic -no-reboot

# SMP 多核：QEMU -kernel 直载模式无 PSCI（secondary 核需 EL3 固件唤醒，
# 见 kunxcore/src/smp.rs 说明），此目标用于观察 cpus=N 检测与单核回退；
# 多核调度代码（每核 CURRENT/idle/定时器/自旋锁）在具备 PSCI 的环境生效。
run-smp: kernel
	$(QEMU) -M virt -cpu cortex-a72 -smp 2 -m 512M -kernel $(KERNEL_IMG) \
		-nographic -no-reboot

# Pi 4 bootloader 骨架编译检查。
boot-pi4:
	$(MAKE) -C $(KUNXBOOT) pi4

# ---- 清理 ----
clean:
	$(CARGO) clean --manifest-path $(KUNXCORE)/Cargo.toml
	$(CARGO) clean --manifest-path $(KUNXINIT)/Cargo.toml
	$(CARGO) clean --manifest-path $(NETWORKD)/Cargo.toml
	$(CARGO) clean --manifest-path $(DISPLAYD)/Cargo.toml
	$(CARGO) clean --manifest-path $(LAUNCHER)/Cargo.toml
	$(CARGO) clean --manifest-path $(SETTINGS)/Cargo.toml
	$(CARGO) clean --manifest-path $(TERMINAL)/Cargo.toml
	$(CARGO) clean --manifest-path $(CALC)/Cargo.toml
	$(CARGO) clean --manifest-path $(OOBE)/Cargo.toml
	$(CARGO) clean --manifest-path $(STORE)/Cargo.toml
	$(CARGO) clean --manifest-path $(STUB)/Cargo.toml
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
	$(CARGO) clean --manifest-path $(KUNXBOOT)/Cargo.toml
	rm -f $(RAMDISK) $(KERNEL_IMG) $(ROOTFS)/init $(ROOTFS)/bin/networkd \
		$(ROOTFS)/bin/displayd $(ROOTFS)/bin/launcher $(ROOTFS)/bin/settings \
		$(ROOTFS)/bin/terminal $(ROOTFS)/bin/calculator $(ROOTFS)/bin/busybox \
		$(ROOTFS)/bin/oobe $(ROOTFS)/bin/store $(ROOTFS)/bin/pkg \
		$(ROOTFS)/bin/phone $(ROOTFS)/bin/sms $(ROOTFS)/bin/contacts \
		$(ROOTFS)/bin/files $(ROOTFS)/bin/browser $(ROOTFS)/bin/clock \
		$(ROOTFS)/bin/calendar $(ROOTFS)/bin/camera $(ROOTFS)/bin/gallery \
		$(ROOTFS)/bin/music $(ROOTFS)/bin/mail $(ROOTFS)/bin/map \
		$(ROOTFS)/bin/weather \
		$(BOOT_IMG) $(DTB) $(DISK_IMG)
