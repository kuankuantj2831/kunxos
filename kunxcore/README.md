# 最小 Rust 内核（QEMU virt AArch64）

一个可在 QEMU `virt` 平台（ARM Cortex-A72）上直接运行的最小 Rust 裸机内核，
包含 MMU 恒等映射、简单多任务调度与物理页分配器。

## 硬件平台

- QEMU `virt` 机器，CPU 为 Cortex-A72
- PL011 UART：MMIO `0x09000000`，时钟 24 MHz
- GICv2：Distributor `0x08000000`，CPU Interface `0x08010000`
- ARM Generic Timer：`CNTFRQ` = 62.5 MHz
- 内存：起始 `0x40000000`，内核加载于 `0x40080000`
- MMU：4KB 粒度，恒等映射

## 功能

1. `global_asm!` 编写复位向量启动代码
2. 关闭 MMU 后重新建立恒等映射页表并启用 MMU（`SCTLR_EL1.M=1`）
3. 设置 EL1 栈指针 `SP_EL1 = 0x40080000`
4. 初始化 PL011：115200 8N1，使能 FIFO
5. 打印 `Hello from Rust kernel!` 与 `MMU enabled.`
6. MMU 恒等映射：代码段 RX、数据段 RW、设备段 DEVICE（`src/mmu.rs`）
7. 物理页分配器：位图管理 `0x40000000-0x7FFFFFFF`，`alloc_page()`/`free_page()`（`src/allocator.rs`）
8. 简单调度器：两个任务各 64KB 栈，定时器中断触发上下文切换（`src/scheduler.rs`）
9. 配置 GICv2 与虚拟定时器，开启 IRQ
10. 任务 A 循环打印 `A`，任务 B 循环打印 `B`，定时器中断交替切换
11. UART IRQ：读取接收字符并回显
12. `panic_handler` 与同步异常处理器：打印信息后死循环
13. 虚拟内存：`vm::map_page(vaddr, paddr, flags)` / `vm::unmap_page(vaddr)`，支持
    UXN/PXN/AF/SH 标志，按需分配中间级页表并自动拆分块描述符（`src/vm.rs`）
14. 内核堆分配器：slab 算法，大小分级 8..2048，`heap::kmalloc`/`heap::kfree`，
    整页 slab 释放后归还物理页分配器、大块按整页分配，无泄漏（`src/heap.rs`）
15. 用户态地址空间：用户页表 + 1MB 用户栈 + `svc #0` 系统调用，
    下陷 EL0 执行用户程序，退出后转入调度器（`src/syscall.rs`）

## 依赖

- Rust 工具链（含 `aarch64-unknown-none` target）：
  `rustup target add aarch64-unknown-none`
- `llvm-objcopy`（随 LLVM / rustup 的 `llvm-tools` 提供）
- `qemu-system-aarch64`

## 编译

```sh
make
```

## 运行

```sh
make run          # 无图形，串口输出到当前终端
make run-gui      # 带图形窗口，串口输出到当前终端
```

## 预期输出

```text
Hello from Rust kernel!
MMU enabled.
[test] kmalloc/kfree OK (no leak)
[test] map_page OK
[test] unmap_page OK
USR
[syscall] user program exited
A
B
A
B
...
```

在终端中输入任意字符会被立即回显（回显会穿插在 A/B 输出之间）。

## 清理

```sh
make clean
```
