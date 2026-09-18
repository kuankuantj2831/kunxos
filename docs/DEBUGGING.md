# KunxOS 调试与开发者工具链指南

本文档覆盖 KunxOS 的开发者工具链：远程调试（GDB）、崩溃报告（tombstone）、
结构化日志（printk / logd / logcat 等价物）、性能统计（overlay / top / perf）、
模拟器加速（QEMU KVM / HVF）与真实硬件（Raspberry Pi 4）串口调试。

## 1. 快速上手

```sh
# 构建（QEMU 默认）
make kernel            # 内核 + rootfs（含 logd）

# 带调试串口的图形模式：串口走 TCP（宿主 socat 转发给 GDB）
make run-debug         # 等价于下方第 2.2 节的 QEMU 命令行

# 进入内核交互 shell（串口控制台）后：
kunxos$ gdb            # 挂起内核，等待宿主 GDB 连接
kunxos$ perf on        # 屏幕右上角显示 FPS/CPU/MEM overlay
kunxos$ top -n 1       # 任务 CPU 用量快照
kunxos$ log            # 结构化日志（logcat 等价物）
kunxos$ crash          # 列出崩溃报告（/data/crash）
```

## 2. 远程调试（GDB）

### 2.1 架构

内核内置 **GDB 远程串行协议（RSP）调试桩**（`kunxcore/src/gdbstub.rs`），
经 UART 与宿主 GDB 通信（`target remote <serial>`）。支持：

| 能力 | 状态 | 说明 |
|------|------|------|
| 软件断点 `Z0`/`z0` | ✅ | 写入 BRK #0，命中摘除 → 硬件单步跳过 → 静默重插 |
| 硬件单步 `s` | ✅ | MDSCR_EL1.SS，步进异常后停回 GDB |
| 寄存器读写 `g`/`G`/`p`/`P` | ✅ | 陷阱帧/内核现场互拷，写后继续生效 |
| 内存读写 `m`/`M` | ✅ | 内核恒等映射直访；用户区经当前页表 |
| 继续 `c` / detach `D` | ✅ | 断点处继续 = 摘除→步进→重插（标准舞蹈） |
| 线程（`Hc`/`Hg`/`qfThreadInfo`） | ⏳ 预留 | 返回空响应（不支持） |
| watchpoint `Z2` | ⏳ 预留 | 返回空响应（不支持） |
| 硬件断点 `Z1` | ⏳ 预留 | 返回空响应（不支持） |

断点/单步在 **EL0（用户进程）** 与 **EL1（内核）** 均可命中：

- EL0 BRK64（ESR.EC=0x3C）与软件步进（0x32）→ `lower_sync` 汇编
  分流到 `gdb_debug_entry`；
- EL1 BRK64（0x3C）与软件步进（0x33）→ `sync_entry` 汇编分流到
  `sync_gdb_entry`，继续时从 PANIC_REGS 恢复寄存器并 `eret`（x11 原始值
  被基址标记占用，属已知限制）。

### 2.2 QEMU 下连接 GDB（推荐：串口 TCP 转发）

QEMU 的 `-serial tcp:...` 与内核调试桩结合：

```sh
# 终端 1：启动 QEMU（串口 = TCP 服务端 127.0.0.1:1234）
qemu-system-aarch64 -M virt -cpu cortex-a72 -m 512M \
    -kernel kunxcore/kernel.img \
    -serial tcp:127.0.0.1:1234,server,nowait \
    -monitor none -display none \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
    -device virtio-rng-device -device AC97 \
    -no-reboot

# 终端 2：进入内核 shell 后执行 `gdb` 命令挂起内核
# （或让内核在断点/崩溃处自动进入调试桩）

# 终端 3：宿主 GDB 连接
aarch64-linux-gnu-gdb -nx kunxcore/target/aarch64-unknown-none/release/rust-kernel
(gdb) target remote localhost:1234
(gdb) info registers
(gdb) x/16gx 0x40080000
(gdb) b *0x40081234          # 内核地址软件断点
(gdb) c
```

> `make run-gdb` 目标封装了上述 QEMU 命令行（串口 TCP + 无显示）。

### 2.3 用户进程调试（断点/单步）

用户进程（app）地址在 0x400000 起（ELF 加载地址）。流程：

1. 在 shell 中先 `run /bin/calculator` 等让目标进程常驻（或用 `gdb` 命令
   挂起内核后直接设用户地址断点——注意：经内核 shell 进入时，`m`/`M`
   使用 shell 自己的页表，跨进程读写需先切到目标进程上下文）；
2. `gdb` 命令进入调试循环 → 宿主 `target remote`；
3. `b *<用户代码地址>`（可用 `info files` 对照 ELF 段）；`c` 运行；
4. 命中后 `s` 单步、`info registers`、`x/32gx $sp`、`p *(int*)0x...`；
5. `c` 继续（断点处自动"摘除→步进→重插"，行为与 gdb 一致）。

### 2.4 QEMU 内置 gdbstub（备选）

不经过内核调试桩、直接让 QEMU 暂停整机：

```sh
qemu-system-aarch64 ... -s -S   # -s = gdb 监听 1234；-S = 启动即暂停
(gdb) target remote localhost:1234
(gdb) continue
```

此方式调试的是**整机**（含固件/MMU 初始化），看不到 EL0 语义；
内核调试桩则能在用户进程边界停靠（断点/单步针对具体地址空间）。

### 2.5 Raspberry Pi 4 串口调试

- **硬件串口**：GPIO14（TXD）/ GPIO15（RXD），3.3V 电平，接 USB-TTL
  适配器（如 CP2102/CH340），波特率 115200 8N1。
- Pi 4 的 PL011（`/dev/ttyAMA0`）默认被蓝牙占用，需在 `config.txt` 加
  `dtoverlay=disable-bt`（或 `miniuart-bt`）释放；内核从设备树解析 UART
  基址（`kunxhal/src/plat_pi4.rs`）。
- 宿主侧：`screen /dev/ttyUSB0 115200` 或
  `socat -d -d PTY,link=/tmp/ttyS0,raw TCP-LISTEN:1234` 把串口桥成 TCP，
  再 `gdb target remote localhost:1234`（与 2.2 相同的 RSP 会话）。
- **JTAG**：Pi 4 的 GPIO22/23/24/25 可作 JTAG（需额外硬件，如
  Raspberry Pi Debug Probe / J-Link + SWD/JTAG 转接，且须在 `config.txt`
  启用 `enable_jtag_gpio=1`）。本项目未接入内核 JTAG 支持；调试首选串口
  GDB stub，JTAG 仅在串口不可用（如 UART 驱动崩溃）时作为兜底，需
  外部调试器直接操作 CPU 寄存器与内存。

## 3. 崩溃报告（tombstone 等价物）

`kunxcore/src/crash.rs` 在以下场景生成结构化报告并落盘：

| 触发 | 位置 | 文件 |
|------|------|------|
| EL0 用户故障（SIGSEGV/SIGILL/SIGTRAP/SIGBUS） | `el0_fault_handler` | `/data/crash/tombstone_<ts>_pid<pid>.txt` |
| 内核 panic（`panic!`） | `#[panic_handler]` | 同上（无 /data 时 `/tmp/crash/`） |
| EL1 同步异常（MMU 故障等） | `sync_exception_handler` | 同上 |

报告内容（Android tombstone 风格）：

```
*** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***
KunxOS tombstone: SIGSEGV pid 5, name calculator, signal 11 (SIGSEGV)
ESR:  0x96000004  EC=37  data abort from EL0
registers:  x0=... x1=... ... sp=... pc=... cpsr=...
memory map:  0x400000-0x401fff r-x  0x402000-0x405fff rw- (cow) ...
backtrace:  #0 pc=0x401234  fp=0x3efff8 ...
stack:  0x3efff8: 0x...
kernel log tail:  <最近的结构化日志>
*** end tombstone ***
```

- 保留最近 **8** 份（`crash::rotate`，SFS 目录按创建顺序删除最旧）。
- shell 命令：`crash`（列表）、`crash show <file>`、`crash clear`。
- 落盘为 best-effort：早期启动 panic（VFS 未就绪）仅输出控制台。
- 回溯使用 **x29 帧指针链**（`el0_fault_handler` 原有实现；Rust 用户程序
  默认不生成帧指针时退化为栈扫描——见第 6 节问题一）。

## 4. 结构化日志（dmesg / logcat 等价物）

### 4.1 内核 printk

`kunxcore/src/log.rs` 提供带级别 + 模块标签 + 时间戳的结构化环形缓冲
（256 条 × 144B），并镜像到 UART 控制台：

```rust
printk!(crate::log::LOG_ERR, "blk", "read failed: dev={}", major);
// 控制台: [blk] read failed: dev=8
// 结构化: <3> <ts_us> blk: read failed: dev=8
```

级别：`LOG_EMERG..LOG_DEBUG`（0-7，Linux 语义）。

### 4.2 /dev/kmsg

- `read`：增量读取下一条记录（全局游标，供 logd 轮询）；
- `write`：注入用户态日志 `"<level>[tag] msg"` 或 `"<level> tag: msg"`，
  并镜像到控制台 —— **logcat 等价物的内核侧通道**：应用写一行即同时进入
  结构化缓冲与控制台，`log` 命令即可检索。

### 4.3 logd 守护进程

`kunxserv/logd`（EL0 常驻，init 拉起）读取 `/dev/kmsg` 并持久化到
`/data/log/logcat`（无 /data 时 `/tmp/log/logcat`；文件超 256KB 截断成
环形文件，防 SFS 单文件上限与磁盘膨胀）。

### 4.4 shell 命令

```sh
kunxos$ dmesg          # 原始控制台环形缓冲（含启动全部输出）
kunxos$ log            # 结构化视图（全部）
kunxos$ log -l 4       # 仅级别 ≥4（WARNING 及以上）
kunxos$ log blk        # 仅模块标签含 "blk" 的行
kunxos$ echo '<5>[app] hello from app' > /dev/kmsg   # 应用注入日志
```

## 5. 性能统计（overlay / top / perf）

`kunxcore/src/perf.rs`：

- **FPS**：合成器 `present()`（含应用 `FBIO_PRESENT` 路径）每帧计数，
  1 秒窗口滑动平均。
- **CPU%**：调度器 tick 统计 idle/忙比例（10 秒窗口）。
- **MEM%**：物理页分配器 used/total。
- **Overlay**：`perf on` 后在屏幕右上角绘制 `FPS:xx CPU:xx% MEM:xx%`
  （8x8 点阵放大 2x + 半透明黑底），随每帧合成上屏；开销约单帧 10K
  像素写（<1ms，对 120FPS 的 8ms 帧预算可忽略）。

```sh
kunxos$ perf on        # 屏幕 overlay
kunxos$ perf stats     # 打印统计
kunxos$ top            # 每秒刷新任务 CPU 用量（Ctrl 无法中断→`top -n 1` 单次）
kunxos$ top -n 1
```

`top` 列：PID / TID / 状态（R/S/T/Z）/ 名称 / nice / 累计调度 tick（10ms）/ CPU 上限。

## 6. 关键设计问题与答复

### 问题一：当前是否已有 frame pointer 支持？（栈回溯必需）

- **内核**：无帧指针（-fomit-frame-pointer 是 Rust 默认），panic 回溯目前
  用 **栈扫描**（`[ret-scan]`，在 .text 区间内找返回地址字）。
- **用户态**：Rust 应用同样无帧指针；`el0_fault_handler` 的 `[user-bt]`
  沿 x29 帧指针链回溯，但**仅当调用链恰好保留帧指针**时有效（当前应用
  构建未开启 `-C force-frame-pointers`，多数路径退化）。
- **结论**：崩溃回溯的完整方案是把用户态应用与内核都按
  `RUSTFLAGS="-C force-frame-pointers"` 构建（代价：1 个额外寄存器/函数、
  少量栈空间）；当前以栈扫描 + x29 链双轨兜底。tomestone 中两者都输出。

### 问题二：崩溃报告存储位置？

- 主选 **SFS /data 分区**（`/data/crash/`，随 disk.img 持久化）；
- 无 /data（无 virtio-blk 启动）时回退 **/tmp tmpfs**（`/tmp/crash/`）；
- 两者都不可用（早期启动 panic）仅输出控制台。保留最近 8 份。

### 问题三：性能 overlay 如何与合成器集成？是否影响 120 FPS？

- 集成点：`compositor::present()` 在页翻转前把 overlay 画到待翻转的
  back 缓冲上 —— **每个呈现帧**都带 overlay，与"应用直绘 + 内核双缓冲
  翻转"模型正交，无需额外合成层。
- 影响：overlay 文本约 40 字符 × 16×16 = ~10K 像素写，<1ms（纯 CPU
  合成整屏 2MB memcpy 的 ~0.5%），对 8ms/帧预算可忽略；仅当 `perf on`
  时启用（默认关闭，零开销）。

### 问题四：是否支持 JTAG 调试？

- **未接入内核 JTAG**。Pi 4 的 GPIO22-25 可作 JTAG（`enable_jtag_gpio=1`
  + 外部调试探针），但当前工具链以**串口 GDB stub** 为唯一调试通道；
  内核自身没有 JTAG 绑定代码，也没有为 JTAG 保留任何专用资源。
- 使用 JTAG 需额外硬件（Raspberry Pi Debug Probe / J-Link），且串口驱动
  本身崩溃时才有必要——此时整机级调试器可读寄存器/内存、设硬件断点。

## 7. 模拟器加速（QEMU KVM / HVF）

QEMU 的加速器要求 **宿主与客户机同架构**：

| 宿主 | 加速器 | 命令 |
|------|--------|------|
| x86_64 Linux | 无（KVM 不支持跨架构） | 默认 TCG（`-accel tcg,thread=multi`） |
| ARM64 Linux（如树莓派/ARM 服务器） | KVM | `-accel kvm`（`-cpu host`） |
| macOS Apple Silicon | HVF | `-accel hvf`（`-cpu host` 或 `cortex-a72`） |
| macOS Intel | 无（HVF 不支持 aarch64 客户机） | TCG |

> 说明：任务描述中的"x86 宿主 KVM"在 QEMU 上**不可行**——KVM 只能加速
> 与宿主同架构的客户机，x86 宿主无法 KVM 加速 aarch64 客户机；x86 宿主
> 请用 TCG（默认）。多线程 TCG：`-accel tcg,thread=multi -smp 2`——SMP
> 已打通（PSCI CPU_ON 经 DTB `/psci` 检测的 hvc conduit 唤醒副核，见
> `kunxcore/src/smp.rs` 与 `make run-smp`；verbose 启动可见
> `[smp] cpu1 已上线`）。注意：`-smp 2` 仍为**实验性**配置，sfsrace 全量
> 并发压测在双核下偶发任务表踩踏（已列 SMP 债务之外的深层竞态），默认
> 回归与发布仍以单核为准。

```sh
# x86 Linux 宿主（默认 TCG，多线程加速）
qemu-system-aarch64 -M virt -cpu cortex-a72 -accel tcg,thread=multi -smp 2 ...

# ARM64 Linux 宿主（KVM）
qemu-system-aarch64 -M virt -cpu host -accel kvm -enable-kvm ...

# macOS Apple Silicon（HVF）
qemu-system-aarch64 -M virt -cpu cortex-a72 -accel hvf ...
```

## 8. 模拟器磁盘与数据持久化

```sh
make boot        # bootloader + MBR/FAT32/SFS 磁盘镜像，virtio-blk 加载
make run-disk    # -kernel 直载 + 挂载 disk.img（/data 持久化验证）
```

/rootfs 布局：`/bin/networkd` `/bin/logd` `/bin/launcher` `/bin/*apps*`
`/dev/kmsg` `/data`（SFS）`/tmp`（tmpfs）`/fonts/cn.ttf`。

## 9. 常见调试流程速查

```sh
# 1. 崩溃后查 tombstone
kunxos$ crash
kunxos$ crash show tombstone_123456_pid5.txt

# 2. 看最近内核/应用日志
kunxos$ log
kunxos$ dmesg

# 3. 挂 GDB 查现场（先在 shell 敲 gdb，再宿主 target remote）
(gdb) info registers
(gdb) bt                 # 栈扫描回溯（无帧指针时 gdb 自己扫）
(gdb) x/32gx $sp
(gdb) p/x *(unsigned long*)$sp

# 4. 性能观察
kunxos$ perf on
kunxos$ top -n 1
```
