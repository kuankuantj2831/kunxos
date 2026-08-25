# KunxOS 内核构建脚本：默认 QEMU virt，`make pi4` 切换 Raspberry Pi 4 后端。

TARGET   := aarch64-unknown-none
NAME     := rust-kernel
KERNEL   := kernel.img
BUILD    := target/$(TARGET)/release

CARGO    ?= cargo
OBJCOPY  ?= llvm-objcopy
QEMU     ?= qemu-system-aarch64

# 平台特性：默认 qemu（Cargo.toml default），`make pi4` 切换为 pi4。
PLATFORM ?= qemu
ifeq ($(PLATFORM),pi4)
  CARGO_FEATURES := --no-default-features --features pi4
else
  CARGO_FEATURES := --features qemu
endif

.PHONY: all build pi4 run run-gui run-net clean

# 默认目标：生成 kernel.img
all: $(KERNEL)

# 编译内核（release），按平台传递 feature
build:
	$(CARGO) build --release --target $(TARGET) $(CARGO_FEATURES)

# 编译 Raspberry Pi 4 后端（编译检查）
pi4:
	$(MAKE) build PLATFORM=pi4

# 从 ELF 提取纯二进制内核镜像
$(KERNEL): build
	$(OBJCOPY) -O binary $(BUILD)/$(NAME) $(KERNEL)

# 无图形模式运行（串口输出到当前终端，带 virtio-net 用户网络）
run: $(KERNEL)
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL) -nographic -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0

# 带图形模式运行（串口输出到当前终端，另开 QEMU 窗口）
run-gui: $(KERNEL)
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL) -serial stdio -device ramfb -no-reboot \
		-netdev user,id=n0 -device virtio-net-device,netdev=n0

run-net: $(KERNEL)
	$(QEMU) -M virt -cpu cortex-a72 -m 512M -kernel $(KERNEL) -serial stdio -device ramfb -no-reboot -netdev user,id=net0 -device virtio-net-pci,netdev=net0

# 清理构建产物
clean:
	$(CARGO) clean
	rm -f $(KERNEL)
