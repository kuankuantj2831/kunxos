# KunxOS wifid 构建脚本：no_std 裸机 Rust ELF（由内核 ELF 加载器加载）。

TARGET := aarch64-unknown-none
CARGO  ?= cargo

.PHONY: all clean

all:
	$(CARGO) build --release --target $(TARGET)

clean:
	$(CARGO) clean