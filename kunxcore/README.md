# KunxOS 内核（RISC-V 64 位）

当前内核运行于 QEMU `virt`（rv64），由 OpenSBI 在 S 模式启动，使用 PLIC 中断控制器、SBI 定时器和 16550 UART。

构建依赖与完整命令见 [项目说明](../README.md)。在项目根目录执行：

```sh
rustup target add riscv64gc-unknown-none-elf
make kernel
make run
make run-gui
make run-disk
```

本目录的 Makefile 转发到顶层构建，确保 rootfs、共享代码和运行参数保持一致。Pi 4 HAL 后端仍保留供开发，当前内核不提供 Pi 4 启动目标。
