# KunxOS Bootloader 构建脚本：默认 QEMU virt，`make pi4` 切换 Raspberry Pi 4（骨架）。

TARGET   := aarch64-unknown-none
NAME     := kunxboot
IMAGE    := bootloader.img
BUILD    := target/$(TARGET)/release

CARGO    ?= cargo
OBJCOPY  ?= llvm-objcopy

PLATFORM ?= qemu
ifeq ($(PLATFORM),pi4)
  CARGO_FEATURES := --no-default-features --features pi4
else
  CARGO_FEATURES := --features qemu
endif

.PHONY: all build pi4 clean

# 默认目标：生成 bootloader.img
all: $(IMAGE)

# 编译（release，按平台传递 feature）
build:
	$(CARGO) build --release --target $(TARGET) $(CARGO_FEATURES)

# Raspberry Pi 4 后端（编译检查）
pi4:
	$(MAKE) build PLATFORM=pi4

# 从 ELF 提取纯二进制 bootloader 镜像
$(IMAGE): build
	$(OBJCOPY) -O binary $(BUILD)/$(NAME) $(IMAGE)

clean:
	$(CARGO) clean
	rm -f $(IMAGE)
