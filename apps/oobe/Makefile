# KunxOS OOBE 构建脚本：no_std 裸机 Rust ELF。

TARGET := aarch64-unknown-none
CARGO  ?= cargo

.PHONY: all clean

all:
	$(CARGO) build --release --target $(TARGET)

clean:
	$(CARGO) clean
