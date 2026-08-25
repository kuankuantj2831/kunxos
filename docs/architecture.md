# KunxOS 系统架构

本文档用 ASCII / Markdown 表格描述 KunxOS 的整体架构：启动流程、模块依赖、
系统调用流程与 RAM 512MB 内存布局。

> 目标平台：QEMU `virt`（ARM Cortex-A72，AArch64）与 Raspberry Pi 4（BCM2711），
> 由 `kunxhal` 的编译期 feature（`qemu` / `pi4`）在二者间切换。

---

## 1. 启动流程图

```
 ┌───────────────────────────────────────────────┐
 │                   BootROM                     │  硬件固件，加载 bootloader
 └──────────────────────┬────────────────────────┘
                        │ QEMU -device loader 载入 bootloader.img @ 0x40200000
                        ▼
 ┌───────────────────────────────────────────────┐
 │                 Bootloader                    │  kunxboot
 │   EL3 → EL2 → EL1 逐级降权                    │
 │   UART / DDR 初始化                           │
 │   MMC（virtio-blk）初始化                     │
 │   挂载 FAT32 分区                              │
 │   加载 KERNEL.IMG   @ 0x40080000              │
 │   加载 KUNXOS.DTB   @ 0x44000000              │
 │   加载 RAMDISK.IMG  @ 0x45000000              │
 │   DTB fixup（memory 大小修正）                 │
 └──────────────────────┬────────────────────────┘
                        │ jump_to_kernel()
                        ▼
 ┌───────────────────────────────────────────────┐
 │                   Kernel                      │  kunxcore @ 0x40080000
 │   解析 DTB → UART → 物理页分配器               │
 │   MMU 恒等映射 → 自检 → VFS + ramdisk 挂载     │
 │   /data 持久化（virtio-blk + MBR + SFS）       │
 │   输入子系统 / 图形（ramfb → /dev/fb0）         │
 │   GIC / 定时器 / 网络 / 调度器 / IPC / 进程     │
 └──────────────────────┬────────────────────────┘
                        │ elf::spawn_from_path("/init")
                        ▼
 ┌───────────────────────────────────────────────┐
 │                    Init                       │  kunxinit（EL0，PID 0 承载）
 │   解析 /etc/inittab                            │
 │     ::sysinit:/etc/init.d/S01-network         │
 │       → exec /bin/networkd                    │
 │     OOBE 首次开机向导（/data/oobe/.done 缺失） │
 │     ::once:/etc/init.d/S03-launcher           │
 └──────────────────────┬────────────────────────┘
                        │ spawn /bin/launcher（常驻）
                        ▼
 ┌───────────────────────────────────────────────┐
 │                   Desktop                     │  launcher + apps
 │   settings / terminal / calculator / store    │
 └───────────────────────────────────────────────┘
```

启动链摘要：

| 阶段 | 组件 | 关键动作 |
|------|------|----------|
| 1 | BootROM | 硬件固件，加载 bootloader |
| 2 | `kunxboot` | 降权到 EL1，初始化 MMC/FAT32，加载内核 + DTB + ramdisk 后跳转 |
| 3 | `kunxcore` | 内核初始化各子系统后，从 ramdisk 加载 `/init` |
| 4 | `kunxinit` | 解释 `/etc/inittab`，启动系统服务与 OOBE |
| 5 | `launcher` | 常驻桌面进程，拉起各应用 |

---

## 2. 模块依赖图

### 2.1 编译期（Cargo）依赖

```
                        ┌──────────────────┐
                        │     kunxhal      │  硬件抽象层：Platform trait
                        │  qemu / pi4 后端 │
                        └───┬──────────┬───┘
            库依赖          │          │           库依赖
          ┌────────────────┘          └────────────────┐
          ▼                                            ▼
 ┌──────────────────┐                          ┌──────────────────┐
 │     kunxcore     │                          │     kunxserv     │
 │    内核（EL1）    │                          │  networkd（EL0）  │
 └──────────────────┘                          │  displayd（废弃） │
                                                └──────────────────┘

 无外部依赖的独立 crate：
   kunxboot   独立 bootloader（不依赖 kunxhal）
   kunxinit   纯系统调用 EL0 程序（无 libc、无依赖）
   kunxpkg    包管理工具（EL0）
   apps/*     launcher / settings / terminal / calculator / oobe / store
```

### 2.2 运行时加载链

```
                ┌─────────────┐
                │  kunxcore   │  内核：加载并调度 EL0 进程
                └──────┬──────┘
                       │ 加载 /init
                       ▼
                ┌─────────────┐
                │  kunxinit   │  解析 inittab，启动服务
                └──────┬──────┘
                       │ fork/exec（回退为内核接管）
                       ▼
                ┌─────────────┐
                │  kunxserv   │  networkd（displayd 已废弃）
                └──────┬──────┘
                       │
                       ▼
                ┌─────────────┐
                │    apps     │  launcher → settings/terminal/calculator/store
                └─────────────┘
```

### 2.3 依赖方向总览

```
   kunxhal  ──(库依赖)──►  kunxcore
   kunxhal  ──(库依赖)──►  kunxserv/networkd

   kunxcore ──(加载/调度)──► kunxinit ──(加载服务)──► kunxserv ──► apps
   kunxcore ──(直接 spawn)──► /bin/launcher ──► 各 apps
```

---

## 3. 系统调用流程

```
 ┌──────────┐
 │   User   │  EL0：应用 / init / networkd 调用 svc #0
 └────┬─────┘
      │ svc #0（x8 = 系统调用号，x0..x5 = 参数）
      ▼
 ┌──────────────────────────────────────────────────────────┐
 │                        EL1 异常入口                       │
 │  VBAR_EL1 → lower_sync（0x400 同步异常向量）              │
 │  保存完整寄存器现场到内核栈 TrapFrame（272 字节）          │
 └────┬─────────────────────────────────────────────────────┘
      │ syscall_dispatch(frame)
      ▼
 ┌──────────────────────────────────────────────────────────┐
 │              syscall_table::syscall_dispatch             │
 │  解析 x8 系统调用号，转发到 linux_compat::dispatch        │
 └────┬─────────────────────────────────────────────────────┘
      │ Linux AArch64（asm-generic）系统调用号 → 内核服务
      ▼
 ┌──────────────────────────────────────────────────────────┐
 │              linux_compat::dispatch（翻译层）            │
 │  语义差异处理：openat/getdents64/stat/mmap/clone/futex   │
 │  按系统调用号分发到下列子系统                             │
 └──┬──────────┬──────────┬──────────┬──────────┬──────────┘
    │          │          │          │          │
    ▼          ▼          ▼          ▼          ▼
 ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────────┐
 │ vfs  │ │ ipc  │ │process│ │socket│ │bionic_shim│
 │文件系统│ │消息  │ │进程/  │ │网络  │ │brk/mmap/ │
 │      │ │传递  │ │调度  │ │协议栈 │ │线程/futex │
 └──┬───┘ └──────┘ └──┬───┘ └──┬───┘ └──────────┘
    │                  │        │
    ▼                  ▼        ▼
 ┌─────────────────────────────────────────────┐
 │                设备驱动层                    │
 │  devfs（/dev/null、/dev/tty、/dev/fb0）     │
 │  virtio_blk（块设备）  net_virtio（网卡）    │
 │  gpu_simple（ramfb）   input（触摸）         │
 │  blkdev（ramdisk + MBR 分区）                │
 └─────────────────────────────────────────────┘
```

系统调用完整路径：`User(EL0) → svc #0 → lower_sync(EL1) → syscall_dispatch → linux_compat::dispatch → 内核服务（VFS/IPC/进程/socket）→ 设备驱动`。

---

## 4. 内存布局图（RAM 512MB）

RAM 由 `kunxhal` 平台后端报告：基址 `0x40000000`，大小 `0x20000000`（512MB），
即 `0x40000000 – 0x60000000`。QEMU 以 `-m 512M` 启动。

### 4.1 物理内存分配

```
 地址                  │ 用途
 ──────────────────────┼────────────────────────────────────────────────
 0x00000000            │ 设备区（MMIO，非 RAM）
   ...
 0x08000000            │ GICv2 Distributor
 0x08010000            │ GICv2 CPU Interface
 0x09000000            │ PL011 UART
 0x09020000            │ fw_cfg（ramfb 帧缓冲配置通道）
   ...
 0x40000000 ───────────┼────────── RAM 基址（512MB 起点）──────────
 0x40000000 ─ 0x40080000 │ 内核启动栈（SP_EL1 = 0x40080000，向下增长）
 0x40080000            │ 内核镜像入口（_start / .text）
 0x40080000 ─ _kernel_end │ .text → .rodata → .data → .bss（4KB 对齐）
 _kernel_end ─ 0x44000000 │ 物理页分配器可用内存（4KB 页位图管理）
 0x44000000            │ DTB（bootloader 载入 KUNXOS.DTB）
 0x45000000            │ RAMDISK（bootloader 载入 RAMDISK.IMG，rootfs SFS）
 0x48000000            │ ramfb 帧缓冲（540×960×32bpp ≈ 2MB，/dev/fb0）
 0x48000000 ─ 0x60000000 │ 其余可用内存（页分配器 / 用户地址空间）
 0x60000000 ───────────┼────────── RAM 末尾（512MB 结束）──────────
```

### 4.2 分配表

| 区域 | 起始地址 | 说明 |
|------|----------|------|
| RAM 基址 | `0x40000000` | 512MB 物理内存起点 |
| 内核启动栈 | `0x40000000` 起 | `SP_EL1 = _stack_top = 0x40080000`，向下增长 |
| 内核镜像 | `0x40080000` | QEMU 加载 raw 内核（链接脚本入口），含 `.text/.rodata/.data/.bss` |
| 页分配器 | `_kernel_end` 起 | 位图管理 4KB 物理页（`alloc_page`/`free_page`） |
| DTB | `0x44000000` | bootloader 载入设备树，内核经 `x0` 获取其地址 |
| RAMDISK | `0x45000000` | bootloader 载入 rootfs（SFS），内核内嵌副本挂载为 `/` |
| 帧缓冲 | `0x48000000` | ramfb，540×960×32bpp，`/dev/fb0` 可 `mmap` |
| RAM 末尾 | `0x60000000` | 512MB 结束 |

### 4.3 MMIO 设备区

| 设备 | 地址 | 说明 |
|------|------|------|
| GICv2 Distributor | `0x08000000` | 中断分发 |
| GICv2 CPU Interface | `0x08010000` | CPU 接口 |
| PL011 UART | `0x09000000` | 串口，24MHz 时钟，115200 8N1 |
| fw_cfg | `0x09020000` | QEMU 特有，ramfb 配置通道 |
| virtio-mmio | DTB 解析 | virtio-net / virtio-blk 设备（地址由设备树给出） |

### 4.4 虚拟内存

- MMU：4KB 粒度，`TCR_EL1.T0SZ=25`（39 位 VA），三级页表 L1(1GB) → L2(2MB) → L3(4KB)。
- 内核恒等映射：RAM 区 + 低 1GB 设备区（`vm::build_identity_map`）。
- 用户地址空间：独立根页表，继承内核恒等映射（EL1&0 共用 `TTBR0_EL1`），
  每进程 1MB 用户栈。

> 说明：`allocator.rs` 与 `vm.rs` 中的位图/恒等映射覆盖 `0x40000000 – 0x80000000`
> （1GB），而 `kunxhal` 的 `ram_size` 与 QEMU `-m 512M` 均为 512MB。二者存在
> 不一致，实际可用内存以 512MB 为准，本文档按 512MB 描述。
