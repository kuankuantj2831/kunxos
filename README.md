从零开始编写的 ARM64 操作系统，使用 Rust 与少量 C/汇编实现，覆盖 Bootloader、内核、系统服务、HAL、OOBE、桌面启动器与包管理器的完整系统栈。默认目标平台为 QEMU `virt`（Cortex-A72），并提供 Raspberry Pi 4 后端骨架。

## 功能特性

- [x] ARM64 Bootloader（`kunxboot`）：DDR 初始化、MMC/SD 读取、FAT32 解析、DTB 加载与内核跳转
- [x] 内核（`kunxcore`）：MMU 分页、物理页分配器、slab 堆、CFS 调度器、进程/线程、系统调用
- [x] 内核安全硬化：COW fork、PAN（EL1 用户内存隔离）、W^X、栈/堆 ASLR、per-app UID 沙箱（`setuid` + kill 权限检查）
- [x] 虚拟文件系统：VFS、devfs、SFS（Simple FS）、ELF 加载器、内核 IPC 与 socket
- [x] 设备驱动：virtio-blk、virtio-net、GICv2、PL011 UART、通用定时器、GPU/合成器（`ramfb`）
- [x] 硬件抽象层（`kunxhal`）：display / input / audio / net / storage / power 平台抽象
- [x] 系统服务（`kunxserv`）：`networkd` 网络守护进程
- [x] 用户态 init（`kunxinit`）：PID 1，解析 `/etc/inittab` 并拉起系统服务
- [x] OOBE 首次启动向导：账户、语言、主题、时区、安全与隐私设置
- [x] 桌面启动器（`apps/launcher`）与内置应用：设置、终端、计算器、应用商店
- [x] 动画系统对标 iOS / Android：Core Animation 与 Material Motion 缓动曲线、弹簧物理、
      合成器窗口动画（位置/透明度/缩放）、图标按压反馈与应用启动/返回缩放转场（见 `docs/animations.md`）
- [x] 系统导航：全面屏手势（底部上滑=桌面、上滑悬停=最近任务、边缘内滑=返回）与
      三大虚拟按键（返回/桌面/最近任务），OOBE 首启选择、设置中随时切换（`shared/nav.rs`）
- [x] 系统锁屏：PIN / 3x3 图案 / 密码 / 滑动四种解锁（与 OOBE 安全设置联动）、
      快捷面板一键锁定、云服务远程锁定、胁迫 PIN（fake_system / wipe / alert）
- [x] 鼠标/触摸流畅度：全部应用采用 overlay 光标快速路径（移动鼠标只更新光标
      所在小区域，不整屏重绘），OOBE 与 18 个应用全覆盖
- [x] 包管理器（`kunxpkg`）：本地软件源、`Packages` 索引、安装与依赖解析
- [x] 磁盘镜像与持久化：MBR + FAT32 + SFS 分区，`/data` 持久化存储
- [ ] Raspberry Pi 4（BCM2711）真机启动（HAL 与 Bootloader 后端已就绪，待完善）

## 构建

### 依赖

- Rust 工具链，含 `aarch64-unknown-none` target：`rustup target add aarch64-unknown-none`
- `llvm-objcopy`（随 LLVM 或 `rustup` 的 `llvm-tools` 提供）
- `qemu-system-aarch64`
- Python 3（构建磁盘镜像与 ramdisk）

### 常用命令

```sh
make            # 编译全部（内核 + init + 服务 + 应用 + rootfs）
make run        # 无图形，串口输出到当前终端（QEMU virt）
make run-gui    # 带图形窗口（ramfb），串口输出到当前终端
make boot       # 经 Bootloader 从 virtio-blk 磁盘镜像加载内核
make clean      # 清理所有构建产物
```

其他目标：

| 命令 | 说明 |
| --- | --- |
| `make kernel` | 仅编译内核（内嵌 ramdisk 镜像） |
| `make init` / `make networkd` / `make launcher` | 编译单个用户态组件 |
| `make rootfs` | 打包 SFS ramdisk 镜像 |
| `make disk` | 生成磁盘镜像（MBR + FAT32 + SFS） |
| `make run-input` | 输入注入模式，配合 `tools/input_inject.py` |
| `make pi4` / `make boot-pi4` | Raspberry Pi 4 后端编译检查 |

## 项目结构

```text
kunxos/
├── Makefile              # 顶层构建脚本
├── kunxboot/             # Bootloader（Rust）
│   └── src/              #   ddr / mmc / fs_fat / dtb / load_kernel / jump ...
├── kunxcore/             # 内核 rust-kernel
│   ├── src/              #   mmu / vm / allocator / heap / sched_cfs / process /
│   │                     #   syscall / vfs / devfs / elf / ipc / socket / net_stack /
│   │                     #   virtio_blk / net_virtio / gic / timer / input / compositor /
│   │                     #   gpu_simple / ui_framework / fs_simple / pkg / shell ...
│   └── apps/             # busybox 等内核态演示程序
├── kunxhal/              # 硬件抽象层（qemu / pi4 后端）
├── kunxinit/             # 用户态 init（PID 1）
├── kunxserv/             # 系统服务
│   ├── networkd/         #   网络守护进程
│   └── displayd/         #   显示守护进程（已废弃）
├── apps/                 # 桌面应用
│   ├── launcher/         #   桌面启动器
│   ├── settings/         #   设置
│   ├── terminal/         #   终端
│   ├── calculator/       #   计算器
│   ├── oobe/             #   首次启动向导
│   ├── store/            #   应用商店
│   └── stub/             #   占位应用模板（各应用均有独立实现，不再共用）
├── kunxpkg/              # 包管理器
├── rootfs/               # 根文件系统（ramdisk 内容）
├── tools/                # 构建工具（mkdisk / mkfs_sfs / input_inject）
├── tests/                # 测试：scripts/ 为 QEMU 端到端与回归脚本，
│                         # text_host / weather_json 为宿主单元测试 crate
├── sync_tests/           # 同步模块集成测试（Rust）
├── disk.img              # 磁盘镜像（构建产物）
└── kunxos.dtb            # 设备树（构建产物）
```

## 硬件支持

| 平台 | CPU / 板卡 | 状态 |
| --- | --- | --- |
| QEMU `virt` | ARM Cortex-A72 | ✅ 完整支持（串口 / 网络 / virtio-blk / ramfb） |
| Raspberry Pi 4 | BCM2711（Cortex-A72） | 🚧 后端骨架已就绪，真机启动待完善 |

## 快速开始

```sh
# 1. 安装交叉编译 target 与 QEMU
rustup target add aarch64-unknown-none

# 2. 编译并运行（串口）
make run

# 3. 编译并运行（图形界面）
make run-gui

# 4. 经 Bootloader 从磁盘镜像启动
make boot
```

首次启动会进入 OOBE 向导，按提示完成账户与系统设置后进入桌面启动器。

## 调试与开发者工具链

内核内置 GDB 远程调试桩（软件断点/硬件单步/寄存器与内存读写）、崩溃报告
（tombstone 落盘 `/data/crash/`）、结构化日志（printk + `/dev/kmsg` +
`logd`）、性能 overlay（FPS/CPU/MEM）与调试 shell 工具（`gdb`/`top`/
`mount`/`ifconfig`/`log`/`crash`/`perf`）。详见 [`docs/DEBUGGING.md`](docs/DEBUGGING.md)。

```sh
# 调试模式：串口走 TCP（宿主 gdb `target remote localhost:1234`）
make run-debug
```

## 贡献者

- [kuankuantj](https://github.com/kuankuantj)

## 许可证

本项目采用 [MIT License](LICENSE) 开源。
