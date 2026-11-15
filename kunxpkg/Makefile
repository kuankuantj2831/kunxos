# KunxOS 包管理器构建脚本：no_std 裸机 Rust ELF（由内核 ELF 加载器加载）。

TARGET := riscv64gc-unknown-none-elf
CARGO  ?= cargo

.PHONY: all clean

all:
	$(CARGO) build --release --target $(TARGET)

clean:
	$(CARGO) clean
