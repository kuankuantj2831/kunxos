# Changelog

本文档记录 KunxOS 各版本的所有重要变更，遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/) 约定。

## [Unreleased]

### GUI 交互修复：OOBE 全步骤可点 + 账号输入框 + 应用占位页 + 应用内鼠标指针（2025-08-25 轮）

- **OOBE 选项可点击**：语言 / 模块勾选 / 地区时区 / 网络跳过 / 协议同意 /
  云服务与家长控制 / 锁屏方式与胁迫动作 / 主题与壁纸全部实现命中处理
  （此前只有「下一步/返回」响应点击，其余控件仅绘制不可交互）。
- **OOBE 账号输入框**：账号步骤改为三个真实输入框（用户名 / 密码掩码 /
  确认密码），点击聚焦（强调色边框 + 光标 `_`），支持 virtio-keyboard 键入
  与退格删除，回车推进焦点；`local_account` / `security_hash` 改用输入的
  用户名与密码哈希。
- **QWERTY 键位修复**：evdev 键码按 QWERTY 物理布局编号（KEY_Q=16、
  KEY_W=17…，非字母表顺序），键码 → 字符必须逐键显式映射（与终端应用
  `key_char` 同一张表）；修复后整排错位（Q→a、W→b…）消除，Shift 大写与
  符号（`shift-1`→`!` 等）实测正确：`qwertyuiop asdfghjkl zxcvbnm 1234567890`。
- **占位应用（stub）**：`/bin/phone|sms|contacts|files|browser|clock` 由
  通用 stub 应用提供（从 argv[0] 取应用名，全屏「敬请期待」+ 返回键），
  点击不再 execve 失败闪退（此前 6/9 可见应用无二进制）。
- **应用内鼠标指针**：calculator / settings / store / stub 均绘制跟随指针的
  光标并在移动时重绘（与 launcher/oobe 一致）；store 补充左上角返回键与
  ESC 退出（此前打开后无法返回桌面），点击改为按下+抬起完整手势判定。
- **ramdisk 扩容 512KB → 768KB**（内核 `RAMDISK_BLOCKS` 128→192 与
  `mkfs_sfs.py` 默认块数同步），容纳 6 个 stub 副本。
- 实测（QEMU 触摸 + virtio-keyboard 注入）：语言胶囊/模块勾选/输入框聚焦与
  键入均生效；9 个可见应用全部可打开可返回；各应用屏幕均可见鼠标指针。

### P1：信号 handler（sigaction / 投递 / sigreturn 蹦床）

- 每进程信号动作表（独立全局表，不膨胀 PCB 拷贝）：`rt_sigaction` 注册
  handler + mask；syscall 入口投递（`handle_pending`）。
- 投递机制：用户栈压 288 字节信号帧（TrapFrame 头 + signum），改写陷阱帧
  进入 handler（x0=signum，x30=蹦床地址）；handler 返回经蹦床
  `mov x8,#139; svc` → `rt_sigreturn` 恢复现场。
- 蹦床页：所有用户空间固定 VA 0x1FF00000 映射共享只读页，**标记永久页**
  （`allocator::mark_permanent`），杜绝共享引用计数归零后被释放复用。
- SIGKILL/SIGSTOP 直接终止；SIGINT/SIGTERM/SIGQUIT/SIGSEGV 默认终止；
  子进程退出时父进程注册了 SIGCHLD handler 才置位。
- 实测：`[signal-handler] SIGTERM caught!`，进程经 sigreturn 继续执行。

### P1：mmap 普通文件映射

- `sys_mmap` 文件分支：除 /dev/fb0 显存直映外，支持任意普通文件
  （MAP_PRIVATE 语义：读文件内容到私有页映射；每进程文件映射游标
  0x2E000000 向上）。
- 实测：mmap /etc/inittab 首字节 0x23（'#'）、写入私有页、munmap 全通过。

### P1：TCP 补全（拥塞控制 + 自适应 RTO + 指数退避）

- TcpSock 增加 cwnd/ssthresh/rto_us/last_send_us/retx_cnt。
- 新 ACK：RTT 采样（指数加权平滑）+ 拥塞窗口推进（慢启动 / 拥塞避免）。
- 超时重传：窗口收缩（ssthresh=cwnd/2, cwnd=1MSS）+ RTO 指数退避（上限 30s）；
  重传成功后退避复位。
- 实测：TCP 客户端经 slirp 连接宿主 echo 服务器，发送/接收回显通过。

### P1：tmpfs 内存文件系统

- 复用 ramdisk/mkfs 机制：`blkdev::ramdisk_create` + `fs_simple::mkfs` +
  `vfs::mount("/tmp")`（256KB）。
- 实测：写入/读回 /tmp 文件通过；`df` 显示 tmpfs 挂载。

### P1：HTTP 客户端（shell `http_get`）+ 既有 http_serve 服务端

- shell 新增 `http_get <ip> <port> <path>`：TCP 连接 → GET 请求 → 解析头部 →
  输出正文。
- 实测：客户机经 slirp 从宿主 HTTP 服务器抓取页面
  `[http_get] body (36 bytes): <h1>Hello from host HTTP server</h1>`。

### 关键 bug 修复：sys_nanosleep 误用墙钟时间（RTC 引入）

- RTC 改造时 `now_usec()` 改为墙钟（epoch µs ~1.78e15），但 sys_nanosleep
  复用该函数计算 deadline，而调度器 tick 用单调时钟比较 → deadline 永远
  达不到 → **所有用户态 sleep 永久阻塞**（networkd 卡死、shell 永不出现）。
- 修复：sys_nanosleep 改用 `timer::now_us()`（单调时钟）。

### P2：GDB 远程调试桩（gdbstub）

- 通过 UART 实现 GDB 远程串行协议（RSP）：`?`/`g`/`G`/`p`/`P`/`m`/`M`/`c`/`s`/`D`/
  `qSupported` 等命令；完整 aarch64 g 包（34 GPR + 32 向量 + fpsr/fpcr，788 字节）。
- shell `gdb` 命令进入调试循环（`gdb_break`：保存现场 → 屏蔽 IRQ → 服务命令）；
  断点期间批量排空 PL011 16 字节硬件 FIFO 到 4096 软件缓冲（防长报文丢字节），
  全程不触碰 DAIF（防定时器抢占切换任务污染会话）。
- 实测：宿主 `aarch64-linux-gnu-gdb` 连接后 `info registers` 显示真实寄存器
  （pc=0x400b2e80 在 sched_cfs::yield_now）、`x/4wx 0x40080000` 读出内核代码。

### P2：剪贴板服务

- 内核全局剪贴板（256 字节）+ 自定义系统调用 `SYS_CLIPBOARD_SET(401)` /
  `SYS_CLIPBOARD_GET(402)`（Linux 未占用号段）。
- 实测：`clipboard set=14 get=14 data=clipboard-data` PASS。

### P3：Raspberry Pi 4 后端补全（mailbox 帧缓冲 / GENET / EMMC2）

- **显示（真实实现）**：`plat_pi4.rs` 新增 VideoCore VI mailbox 属性通道驱动
  （channel 8：写/读寄存器轮询、bus 地址转换 0xC0000000、DC CVAC/IVAC 缓存
  一致性），向固件申请 HDMI 帧缓冲：1080p → 720p → 480p 逐级回退，返回真实
  物理地址 / 分辨率 / 32bpp。
- **网络**：GENET 千兆以太网（0xFD580000）返回真实基址 / 中断号
  （GIC_SPI 157 → 硬件 189）+ 预留 OUI MAC；**存储**：EMMC2 SD 卡（0xFE340000）
  返回真实基址 / 中断号（GIC_SPI 126 → 硬件 158）/ 块大小 512。
- 中断号取自 Linux `bcm2711.dtsi` 的 `GIC_SPI`（硬件号 = SPI + 32）。
- 内核侧 GENET / EMMC2 驱动接线与 gpu_simple 的 Pi4 适配留待后续；`make pi4`
  编译检查通过（内核 + HAL 全量，--features pi4）。
- 启动日志 `[hal] display=` 文案由 QEMU 专属 "ramfb" 改为平台无关 "display"。

### P3：磁盘加密（blkdev 透明 XOR 包装）

- **MBR 类型 0x8E 标记加密卷**：mkdisk 的 data 分区（sda3）改为加密类型。
- **`blkdev::wrap_encryption`**：把分区包装成加密块设备（新 major = 原 + 0x40，
  名字后缀 `e`），读 = 父设备读密文 → XOR 解密；写 = XOR 加密 → 写密文。
  上层文件系统（SFS/FAT32）完全无感知；内核启动时探测到 0x8E 自动包装。
- **演示密钥**（16 字节 "KunxOS-Enc-Key13"）+ 流密码：key 按「扇区号 +
  字节偏移」循环异或（同一扇区重复写使用相同流，可逆；不同扇区不同流）。
- 实测（QEMU + 宿主校验）：首次启动检测加密 → mkfs（密文写）→ 挂载 →
  写入 `/data/e.txt`；**重启后解密探测 SFS 成功、文件读回一致**；宿主查看
  disk.img：data 分区块 0 无 `SFS_` 明文魔数（密文乱码），XOR 解密后
  `0x5F534653` 魔数正确，文件内容仅解密可见。FAT32 分区（明文）与加密
  /data 共存正常。
- 说明：演示密钥非安全强度；真实系统应从 TPM / 用户口令派生密钥。

### 其它

- init 不再拉起已废弃的 displayd（其 30fps demo 依赖已禁用的动画定时器，
  会永久阻塞子进程回收）；launcher 直接操作 /dev/fb0 不受影响。

### P2：FAT32 可写（VFS 第二文件系统）

- **VFS 文件系统分派层 `fsops.rs`**：挂载时自动探测类型（SFS / FAT32），
  全部文件系统操作按 `dev → 类型` 分派，VFS 只依赖分派层；新增文件系统
  只需加一个分支。
- **内核 FAT32 读写模块 `fat32.rs`**：BPB 解析、FAT 表（FAT1/FAT2 同步）、
  簇链读写与分配/释放、目录项增删改查、8.3 短名转换（LFN 遍历时跳过、
  写入只生成短名）、mkdir/rm/rename/truncate/readdir 全支持。
- **ino 设计**：FAT32 无 inode，用静态「打开文件上下文表」把
  `(dev, 父目录簇, 目录项槽)` 映射为稳定 ino（根目录固定 1）。
- **性能**：每卷「下一空闲簇」游标（分配接近 O(n)）+ 每 entry 簇位置缓存
  （顺序读写从缓存续走），cp 1.6MB 文件由数十秒降至秒级。
- **mkdisk 修复**：boot FAT32 分区的 `fat_size` 此前只按文件内容计算
  （34 扇区 ≈ 4352 簇），而分区有 13 万簇——大簇号无 FAT 项导致内核
  分配失败；改为按整个分区计算。另补根目录簇 2 的 FAT 项置 EOC。
- **关键 bug 修复**（调试定位）：
  - fs_write 首次分配簇后内存 entry 的 cluster 未同步 → 第二次 write 重新
    分配新簇并覆盖目录项首簇（数据成孤儿）；
  - 缓存 `cache_off/cache_cluster` 错位 1 簇（终点 vs 所在簇）→ 顺序写
    覆盖旧尾簇、簇链少分配 400+ 簇、读提前 EOF；统一 `update_cache` 修复。
- **自动挂载**：启动时探测 MBR 各分区，FAT32 分区挂载到 `/mnt/fat32`
  （标准 `echo/ls/cat/cp/mkdir/rm` 直接可用）。
- 实测（QEMU virtio-blk + 宿主校验）：`echo > /mnt/fat32/hello.txt` 读写回、
  `mkdir` 子目录嵌套、`cp KERNEL.IMG`（1.6MB / 3232 簇）与源逐字节一致、
  `rm` 删除、`echo x >` 截断重写；宿主解析磁盘镜像：目录项、簇链、
  FAT1/FAT2 一致，KERNEL.IMG 跨簇链完整可读。
- 已知限制：shell `wc` 的 `read_all` 用 16384 固定缓冲（>16KB 文件只统计
  前 16KB，属既有工具限制，与 FAT32 无关）；FAT32 写入不生成 LFN。

### 中文界面 + CJK 字库（2025-08-25 轮）

- **共享 CJK 16x16 点阵字库**：`tools/gen_cjk_font.py` 从标准 HZK16 生成
  `shared/cjk_font_*.rs`（GB2312 一级字库全量给内核、按应用源码提取子集给各应用，
  规避 SFS 单文件 96KB 限制）。
- **内核 UI 支持 UTF-8 + 中文**：`ui_framework::draw_text` 解码 UTF-8，CJK 用
  16x16 点阵（ASCII 8x8 不变）；启动画面第二行改为「欢迎使用 KunxOS」。
- **launcher / settings / oobe / store 中文化**：应用名、标题、设置项、向导文案、
  按钮全部支持中文（launcher/oobe 按 `/data/oobe/lang` 双语切换，默认中文）。
- **OOBE 语言页默认简体中文**（`LANG=0` 即 zh_CN，模拟点击路径同样默认中文）。

### RTC 实时时钟（PL031）

- HAL `Platform` 新增 `rtc_base()`（QEMU virt = PL031 @ 0x09010000，Pi4 = None）。
- 内核 `rtc.rs`：启动时读取纪元秒，`gettimeofday`/`clock_gettime` 返回墙钟时间
  （纪元 + 单调时钟，无 RTC 退化为 uptime）；`date` 命令显示真实日历时间
  （civil_from_epoch，Howard Hinnant 算法，与 Python 标准库交叉验证）。
- 实测：`[rtc] PL031 epoch=1787645074 wall=2026-08-25 08:04:34`。

### 新系统调用：socketpair / truncate / statx

- **`socketpair(AF_UNIX, SOCK_STREAM)`**：ipc.rs 新增 PairSock（双环缓冲全双工），
  两个 fd 双向读写；`FdKind::Pair` 接入 read/write/close/dup。实测双向收发通过。
- **`truncate(path, 0)`**：按路径截断（O_WRONLY|O_CREAT|O_TRUNC，可创建文件）。
- **`statx`**：返回 statx 布局的类型/大小/mode 等基础字段（256 字节布局）。
- 修复 **VFS `resolve_parent` 循环不推进 `pos`** 的预先存在 bug：多级路径创建
  （`/tmp/x`、`/a/b/c`）此前全部失效（第二遍在子目录里重找同名分量）。

### procfs 只读虚拟文件系统

- 复用 devfs 字符设备机制（`ChrReadFn` 增加 offset 参数支持分次读取），
  `/proc` 下提供：`version` / `meminfo` / `uptime` / `cpuinfo` / `processes`。
- 实测：`cat /proc/version` → `KunxOS 0.1.0 (aarch64) rust-kernel`，
  `cat /proc/meminfo` → `MemTotal: 524288 kB`。

### 稳定性修复：EL0 PC=0 / 跳垃圾地址（预先存在的 FP 寄存器污染）

- **根因（新增证据）**：内核从不在异常/上下文切换中保存 FP/SIMD 寄存器
  （v0-v31 + fpsr/fpcr）。IRQ 处理中的 `memcpy`（NEON）与 `anim::frame_tick`
  的 f32 计算会静默破坏被中断任务的 v 寄存器；execve 新进程的陷阱帧 FP 区
  是内核栈残留（含 0.01f32 等浮点常量）→ 应用浮点计算错乱 → 跳垃圾地址。
  故障帧中 `x30=0x3c23d70a`（= 0.01f32）即铁证。
- **修复**：`irq_entry`/`lower_sync` 异常入口保存、出口恢复 FP 上下文
  （v0-v31+fpsr/fpcr，帧分别扩到 784/800 字节）；`execve` 清零陷阱帧 FP 区；
  禁用动画定时器 TIMER2（其 frame_tick 的 f32 是主要污染源之一，禁用后
  崩溃从 ~2 次/轮降到 ~0-1 次/轮，多次 0 崩溃）。
- 注意：DEBUG_NOTES 中「用户侧缓冲区溢出」结论被本次调查修正——指纹
  （0x3c23d70a=0.01f32、ESR 0x8a000000、x30 被浮点常量覆盖）指向内核 FP
  上下文缺失；仍有 ~10% 残余崩溃待后续定位。


### 信号投递补强（2025-08-18 凌晨轮）

- **`sys_kill` 唤醒目标进程**：kill 后立即唤醒阻塞中的目标（睡眠/等待中的
  进程下一次进入内核时 dispatch 检查信号位图并终止），SIGKILL 不再延迟到
  睡眠结束。
- **SIGKILL 投递端到端验证**：自 kill 测试应用（getpid → kill(self,9) →
  write）实测在 write 的 syscall 入口被终止（exit code=-1，无 "alive"
  输出）——此前只验证了置位，未验证投递。

### shell 增强（2025-08-18 凌晨轮）

- **`sysinfo` 增加 Uptime 行**：显示自启动以来的秒数（与 `uptime`/`date` 一致）。

### shell 新命令：od / tee（2025-08-18 凌晨轮）

- **`od <file> [-n <bytes>]`**：十六进制转储（每行 16 字节 + ASCII 侧栏），
  用于检查二进制文件（.pkg / ELF / 镜像）。
- **`tee <file>`**：把管道输入同时写入文件并透传到终端（`cmd | tee log`）。

### 信号投递补强（2025-08-18 凌晨轮）

- **`sys_kill` 唤醒目标进程**：kill 后立即唤醒阻塞中的目标（睡眠/等待中的
  进程下一次进入内核时 dispatch 检查信号位图并终止），SIGKILL 不再延迟到
  睡眠结束。
- **SIGKILL 投递端到端验证**：自 kill 测试应用（getpid → kill(self,9) →
  write）实测在 write 的 syscall 入口被终止（exit code=-1，无 "alive"
  输出）——此前只验证了置位，未验证投递。

### shell 增强（2025-08-18 凌晨轮）

- **`run` 显示子进程退出码**：`run <elf>` 等待子进程结束后输出
  `[run] pid N exited code=M`（含崩溃码，如 SIGSEGV = -11）；Sink 新增
  `emit_i32` 支持负数。

### 内核消息缓冲 + dmesg（2025-08-18 凌晨轮）

- **内核日志环形缓冲（16KB）**：`uart_send_char` 输出（含启动日志、panic
  前的内核消息）全部写入环形缓冲（IRQ 安全：DAIF 保存/恢复，可从任意
  上下文调用），覆盖最旧内容。
- **shell `dmesg [-n N]`**：回放内核消息缓冲；`-n N` 只显示末尾 N 行。
  供用户态查看启动日志（此前只能看串口实时输出）。

### shell 新命令：stat / true / false / basename / dirname（2025-08-18 凌晨轮）

- **`stat <path...>`**：显示文件类型（regular/directory/char-device/...）与
  字节大小（SFS 元数据，走 `vfs::stat_path`）。
- **`true` / `false`**：无操作命令（脚本条件占位）。
- **`basename <path>`**：输出路径最后一级分量；**`dirname <path>`**：输出
  目录部分（无分隔符时输出 "."）。

### shell 新命令：sha1sum（2025-08-18 凌晨轮）

- **`sha1sum [file...]`**：SHA-1（RFC 3174）校验和，支持管道输入与多文件
  参数；流式实现（分块 update，任意大小文件不截断）。已用标准测试向量
  与 Python hashlib 交叉验证（空串、含换行 abc/hello、busybox 二进制
  多块文件全部一致）。

### shell 新命令：date / which / uniq（2025-08-18 凌晨轮）

- **`date`**：显示自启动以来的运行时长（天:时:分:秒，基于内核单调时钟；
  内核无 RTC，不显示真实日历时间）。
- **`which <cmd...>`**：判断命令是内置命令还是 PATH（/bin、/usr/bin）
  中的可执行文件。
- **`uniq`**：去除管道输入中相邻的重复行（`ls | sort | uniq` 常用）；
  空输入直接返回。

### 信号投递与 kill/sleep 命令（2025-08-18 凌晨轮）

- **SIGKILL 投递**：`sys_kill` 此前只置位信号位图、从不生效；现在 `dispatch`
  入口检查当前进程待处理信号，SIGKILL（9）置位即终止进程（不可屏蔽）。
- **shell `kill <pid>`**：向进程发送 SIGKILL 并唤醒阻塞中的目标（下一次
  系统调用入口被终止）；拒绝自杀（kill shell 自身）与不存在的 pid。
- **shell `sleep <seconds>`**：基于内核单调时钟阻塞指定秒数
  （`sched_cfs::sleep_until`），可被定时器按剩余时间继续。

### 异常入口加固与 PC=0 纵深防御（2025-08-18 凌晨轮）

- **异常向量入口统一屏蔽 IRQ**：`sync_entry`（EL1 同步异常）与 `irq_entry`
  入口立即 `msr daifset, #2`。此前同步异常入口到 handler 的 daifset 之间
  存在窗口：定时器 IRQ 抢占会覆盖 `elr_el1/spsr_el1`，panic 诊断读到 IRQ
  残留值（故障 PC 报成抢占点、寄存器混入 IRQ 处理器残留），误导故障定位。
- **IRQ 返回 EL0 前校验 eret 目标（防「用户态 PC=0 / 执行内核代码」）**：
  irq epilogue 在弹出用户寄存器**之前**检查 `elr_el1`：返回目标为 0 或
  ≥0x40000000（内核地址）时，把 IRQ 帧转成 272 字节陷阱帧走 `el0_fault`
  干净终止进程，而不是 eret 到非法地址。校验在 pop 之前（x9/x10 随后被
  pop 恢复，不污染用户现场）；EL1 返回（M≠0）跳过校验，无误伤。实测
  25 轮 0 panic。
- **调试教训**：热路径（irq epilogue / syscall return / trampoline）新增的
  任何指令都会移动时序竞态窗口——此前为定位「memcpy(dst=2)」故障在热路径
  插入 last-eret 探针，把故障率从 ~7% 放大到 100% 每轮必现；移除探针后
  75+ 轮 0 PANIC。诊断探针只允许放在冷路径（fault/panic 分支）。
- **`virtio_input` 事件时间戳**：virtio 键盘事件此前 `tv_sec/tv_usec` 恒为
  0（`InputEvent::new` 置零），现用内核单调时钟填充，与 UART 触摸协议帧
  路径一致。
- **networkd 管道缓冲 256 → 1024 字节**：`poll_pipe` 读取命令后 `ftruncate`
  清空文件，>256B 的命令尾部此前直接丢失；1024B 覆盖全部现有命令。

### 新功能：文件操作内置命令与 SFS 重命名（2025-08-18 凌晨轮）

- **`touch <file...>`**：文件不存在时创建空文件（O_CREAT），已存在则空操作。
- **`cp <src> <dst>`**：复制文件；`dst` 为目录时复制为 `dst/basename(src)`；
  自复制（`cp a a`）明确拒绝，避免 O_TRUNC 先清空源再读导致数据全丢。
- **`mv <src> <dst>`**：重命名/移动文件，可跨目录（SFS 迁移同一 inode）；
  目标已存在、源为目录/不存在时明确报错。底层新增内核原语：
  - `fs_simple::fs_rename`：目录项「摘除 → 写入新目录」+ 失败回滚；
    拒绝 "." / ".."、目录与设备节点（避免破坏目录一致性）。
  - `vfs::rename(old, new)`：经 `resolve_parent` 解析两路径。
  - **`SYS_RENAMEAT`（38）实现**（原为 ENOSYS）：单根文件系统忽略
    dirfd，区分 ENOENT（源缺失）与 EEXIST（目标存在）。
- **`tail [-n N] [file]`**：输出末尾 N 行（默认 10）；无文件参数读
  stdin/管道；正确处理末尾无换行的文件（隐式换行），`-n 0` 输出为空。
- **`wc` 支持 `-l` / `-w` / `-c` 标志**（可组合，如 `-lw`；默认三者全出），
  与标准 wc 输出对齐；stdin 与多文件均支持，多文件末尾输出 total。
- **`ls` 支持普通文件参数**：`ls /tmp/f` 直接列出该文件（此前静默无输出），
  `-l` 显示类型/大小；路径不存在时输出 `ls: cannot access:`。
- `help` 文本与 Tab 补全表（BUILTINS）同步新增 touch/cp/mv/tail。

### 正确性 / 健壮性修复（2025-08-18 凌晨轮）

- **修复 fork 子进程用户栈越界写（根因级）**：`kunxinit::spawn_service`
  子分支原先把两个 96B 路径/名字缓冲与 argv 数组放在**栈局部**，编译器把
  清零/填充指令排在 `sp` 上方（`[sp+96..192]` 及 `[sp+192]`）；而 fork 子
  进程从 fork 返回时沿用父进程当时的用户栈指针（init 栈很浅，距
  `USER_STACK_TOP` 仅 ~176 字节余量），子进程会越过栈顶写进未映射的
  EL1-only 块 → 偶发「EL0 写权限故障」（此前误判为内核 TLB/页表问题），
  或覆盖栈顶 argv 数据引发后续 PC=0 / LR 损坏。修复：改用函数内
  `static mut` 缓冲（`P`/`NAME_BUF`/`ARGV`），彻底消除向 sp 上方的写。
- **fork 防御护栏**：`fork_parent` 拷贝子进程陷阱帧后校验
  `elr ∈ (0, USER_VA_TOP)`、`sp_el0 < USER_STACK_TOP`、`spsr & 0xF == 0`
  （EL0t），帧损坏时让本次 fork 失败而非调度一个「eret 到坏地址」的
  必死子进程。
- **UART 接收环形缓冲 256 → 4096 字节**：此前 shell 忙于执行命令时粘贴
  超过 256 字节的命令会静默丢弃尾部输入（表现为「收到半行命令后 shell
  永久等待换行」）。新增溢出丢弃计数 `UART_RX_DROPPED` + `uart_take_dropped`，
  shell 在每次读行前提示 `[shell] warning: N input byte(s) dropped`。
- **故障诊断增强**（`el0_fault_handler`）：新增用户栈回溯
  `[user-bt]`（沿 x29 链打印返回地址）与 `FAR_EL1` 打印，用于定位
  「LR 损坏 → ret 到 0」类故障的调用点。

### 残余 PC=0 故障机制最终确认（2025-08-18 第 17 轮）

- **系统调用 sp_el0 随机错位（systrace 实证）**：新增最近 24 次系统调用
  环形记录（nr/elr/sp_el0），故障运行显示用户 sp 在相邻 syscall 之间出现
  **随机错位**（init 故障中 -16 字节、oobe 故障中 +432 字节）—— 即陷阱帧
  `sp_el0` 槽被错误值污染，syscall 返回把错位后的 sp 恢复给用户，后续
  svc 持续维持错位，尾声最终读到被清零/错位的保存寄存器槽 → x30=0 → PC=0。
- 综合此前轮次结论（金丝雀排除栈溢出、fork 帧合法排除 fork 拷贝损坏、
  布局核对排除帧错位），残余 PC=0 确认为**陷阱帧 sp_el0 槽的罕见随机污染**
  （~5% 复现），由 `el0_fault_handler` 优雅终止，系统持续运行。
- **全链路回归验证**：shell 边界（cd 错误路径/`cd -`/重定向追加/跨目录 mv）
  全部通过；`http_serve` 经 QEMU hostfwd 端到端验证（宿主 curl 取回
  /var/www/index.html）。

### 残余 PC=0 故障机制定位（2025-08-18 第 16 轮）


- **机制确认（16 字节用户栈指针错位）**：对 init 最终打印故障的用户栈转储
  显示 —— k_main 的序言把 `x30=0x400010`（_start 的 bl 返回地址）保存到
  `[FB+0x60]`，但尾声从 `[FB+0x40]` 读取恢复寄存器（两者相差 16 字节），
  读到 0 → `ret` → PC=0。结合最后一次系统调用记录（write，sp=0x3effff40）
  与 fork 帧记录（fork 时 sp=0x3effff20 → k_main 帧基=0x3effff50），确认
  用户 sp 在 k_main 执行期间被**向下错位 16 字节**，后续所有 svc 均捕获并
  维持错位后的 sp，尾声因此读到错误的（清零的）保存槽。
- **新增诊断**：
  - `fork_parent` 记录每次 fork 子进程恢复现场（elr/sp/x30/spsr）—— 16+
    轮复现全部合法，排除 fork 陷阱帧拷贝损坏
  - `last_syscall` 增记用户 sp_el0；`el0_fault_handler` 增打
    `[last-syscall] nr/elr/sp` 与 `[user-stack]`（故障点附近用户栈转储）
- 结论：错位源是 syscall 返回/定时器抢占路径中的一次罕见 16 字节 sp_el0
  错位（每次 5-10% 概率），具体竞态点仍在追查；故障由 `el0_fault_handler`
  优雅终止，系统持续运行。

### 修复（2025-08-18 第 15 轮）


- **`ls` 目录项排序（真实用户可见 bug）**：此前 `ls` 按 SFS 目录项创建顺序
  输出（`/tmp` 显示 z,a,m），标准 ls 应按名称字典序。`cmd_ls` 现先收集
  最多 64 项到 .bss 缓冲，插入排序后输出（普通与 `-l` 模式一致）。
- **fork/clone 原子性修复**：`sched_cfs::insert`/`spawn` 此前无条件
  `daifclr` 恢复 IRQ，在 `fork_parent` 的 IRQ 屏蔽临界区中途重新使能中断 ——
  子任务会在 fork 尚未完成（父进程仍在其尾部）时即被调度。改为进入时读
  DAIF 的 I 位、退出时按调用方原状态恢复，保证 fork 全流程原子。

### 故障诊断与假设排除（2025-08-18 凌晨轮 · 残余 PC=0 追查）


- **任务内核栈金丝雀**：`alloc_stack` 用 0xA5 填充全部栈页，`el0_fault_handler`
  检查栈底 64 字节是否被破坏 —— **16+14 轮复现中 0 例破坏，明确排除内核栈
  溢出假设**（此前怀疑 fork 子进程内核栈越界覆写相邻任务栈顶）。
- **故障处理器增强**：打印 `FAR_EL1`（data abort 的故障访问地址）与故障任务
  内核栈顶 512 字节转储 —— 显示子进程栈顶下方残留 fork 时拷贝的父进程
  IRQ 帧（含用户 spsr/sp_el0 值），佐证残余 PC=0 与 fork/exec 首段执行、
  定时器抢占的寄存器恢复竞态相关。
- **页释放路径审计**：fork/exec/exit/reap 的 `free_page`/`free_user_pages`/
  `free_process_tables`/`reap_pid` 全路径复查，未发现双重释放或提前释放
  （create_process 的 spawn 失败路径仅泄漏页表页，已记录为 Low）。
- 结论：残余 PC=0（约 1/10-1/16 复现）为 fork 相关寄存器恢复竞态，由
  `el0_fault_handler` 优雅终止并重启，不挂死内核；诊断输出已就位待后续定位。

### 正确性 / 安全修复（2025-08-17 深夜轮）

- **bootloader 加固（审计二轮）**：
  - `dtb::walk_node` 增加递归深度限制（MAX_DEPTH=16）——防恶意/损坏 DTB
    构造深嵌套节点导致 bootloader 栈溢出
  - `fs_fat::fat_entry` 校验 FAT 表扇区边界（越界一律视为簇链结束）——防
    损坏簇号使 fat_sector 落到数据区/分区外，读到无关数据作为 FAT 项
- **图形/输入子系统补强（审计二轮）**：
  - `compositor::set_scale` 钳制 `scale ∈ [1, 1024]`（防 `visual_rect` 的
    `w*scale` 回绕产生巨大可视矩形）
  - `ui_framework` Image 控件：null/0 尺寸跳过 + 源尺寸钳制到目标矩形
    （防 `blit` 越界读任意内存）
  - `timer::reload`：freq 或周期为 0 时用最大间隔兜底（此前 tval=0 →
    定时器立即重触发 → IRQ 风暴饿死低优先级中断）
  - `virtio_input::drain`：坏 used id 钳制到队列范围再重投递（防 desc 槽位
    逐步耗尽导致输入队列枯竭）
  - `input::handle_frame`：坐标钳制到设备物理最大值（max_x/max_y）

- **用户指针页表漫游校验（防 EL0 DoS）**：`user_ok`/`copy_from_user`/`copy_to_user`/
  `peek_u32`/clone tid 写回全部增加「页已映射」验证（`vm::user_range_mapped`）。
  此前只验范围不验映射，用户传未映射地址会在 EL1 触发 data abort，同步异常
  处理器死循环挂死整个内核（任何用户进程可借此 DoS）
- **EL0 用户程序故障隔离**：`lower_sync` 检查 ESR 区分 SVC 与其它同步异常
  （data/instruction abort、PC 未对齐等）；非 SVC 走 `el0_fault_handler`
  终止当前进程（SIGSEGV 语义），由 init 回收重启 —— 用户程序崩溃不再
  拖垮整个内核（oobe 无帧缓冲时曾稳定触发内核挂死）
- **任务栈 128KB → 256KB + `create_child_process` 引用传参**：ELF 加载器在
  栈上持有 `ProcPageTable`（1536 条记录 = 24KB）且 fork/execve 调用链深，
  128KB 栈在 oobe（2MB BSS）execve 时溢出、写穿栈底破坏物理页位图，
  导致后续栈分配重叠、内核崩溃 —— 256KB 栈 + 避免页表按值传参彻底消除
- **`MAX_USER_PAGES` 512 → 1536**：launcher 含两个 2MB 帧缓冲（约 1017 页）
  + 1MB 栈（256 页），512 项记录打满导致「只建映射不记录」的泄漏与
  fork 父子地址空间不一致；记录表满现在**拒绝映射并回滚**（`add_page` 返回
  bool，全部调用方处理失败），而不是静默丢记录
- **ELF 加载失败路径统一清理**：`load()` 所有错误路径释放已分配页与页表
  （此前恶意/损坏 ELF 反复加载可耗尽物理内存）；`e_entry` 校验（入口必须
  落在用户地址空间）；`setup_stack` 失败回滚
- **`fs_simple` 加固**：inode 号边界校验（防 ino≥64 读写数据块区）、
  `fs_write` 检查 read/write_block 返回值（防 RMW 读失败清零整块）、
  `fs_create`/`fs_mkdir` 失败回滚（防 inode 泄漏与半成品目录）
- **`blkdev` checked 运算**：`block*8`、`start_lba+sector`、分区 `start+count`
  全部 checked（防 usize 回绕读写错误扇区）
- **`allocator::init` 钳制**：`kernel_pages` 截断到 `TOTAL_PAGES`（防链接
  符号异常时位图越界写）
- **`vm::add_page` 记录表满拒绝映射**：不再静默丢记录（泄漏 + fork 不一致 +
  回滚释放错页），全部调用方（fork/brk/mmap/ELF）处理失败并回滚
- **input 等待者多槽化**：`waiter` 单槽 → 4 槽数组（多任务阻塞读不再互相
  覆盖丢失唤醒）；非阻塞 `poller` 登记屏蔽 IRQ
- **`pkg install` 资源路径缓冲 96 → 128**（最长 99 字节路径不再截断丢 NUL）
  并检查 `write_file` 返回值
- **`write_stat` 失败传播**：stat 指针非法返回 `EFAULT`（此前静默成功）
- **`sys_getrandom` 增强**：熵混入 tid + 拷贝失败返回 `EFAULT`；
  `sys_recvmsg` 拷贝失败返回 `EFAULT`
- **`sys_close` 语义修正**：未登记的 fd 返回 `EBADF`（此前无条件成功）；
  补 `MsgQueue` 关闭分支；socket 句柄仅在确为 socket 时关闭
- **`sys_kill` 校验（M-3）**：信号号 ≥64 返回 `EINVAL`；目标进程不存在
  返回 `ESRCH`
- **displayd/networkd ftruncate 显式 length=0（M-20 修复补全）**：此前只传
  一个参数，x1 残留值导致内核按 `length!=0` 拒绝，命令管道从不被清空
  （命令重放 / 文件无限增长 / >256B 后新命令读不到）
- **displayd `SURF_META` 下标修正（M-21 修复补全）**：1 基 id 回退 1 再作
  数组下标（此前 w/h 写错槽位、第 8 个 surface 越界写坏 SURF_BUF）
- **合成器 IRQ 同步（H-12 残留）**：`create/destroy/resize_window` 全程屏蔽
  IRQ 且「先摘除窗口再释放表面」—— 杜绝定时器 IRQ 侧 `composite()` 读取
  已 `kfree` 的表面（use-after-free）
- **动画弹簧数值稳定性**：`Spring::step` 钳制 `mass`/`stiffness`（防除零与
  半隐式欧拉发散）并在积分出 NaN 时强制收敛 —— 防止动画永不结束持续
  驱动整屏渲染
- **bootloader mmc used-ring 轮询超时**：设备挂起时不再死循环
- **`dup`/`dup3` 目标槽位关闭 + 引用计数**：dup3 覆盖已占用槽位时按 close
  语义释放（引用 -1），File/Shm 复制时引用 +1，防对象提前释放

- **fork/clone 子进程恢复路径重构（C-6 残留）**：子进程/子线程首次调度直接进入
  `syscall_return_restore`（sp 指向其拷贝的陷阱帧、x0 已置 0），不再沿父调用链
  回卷 —— 消除「子进程把返回值写回父进程陷阱帧」的跨栈写，也不再有陈旧帧写
- **管道 / 消息队列引用计数（H-7 延伸）**：`refs` 计数（创建/复制 +1，close/exit
  -1，归零才清槽）—— fork 子进程退出不再清空父进程仍在使用的管道/队列
- **真实 `dup`/`dup2`/`dup3` 语义**：复制 fd 表项并共享底层对象（文件共享偏移，
  引用计数同步），替换原先「返回原 fd」的假实现
- **`pread64`/`pwrite64`/`lseek` 64 位偏移（M-4）**：不再 `as i32` 截断
- **per-pid brk / mmap 游标**：`BRK_CUR`/`MMAP_NEXT` 从全局单值改为按进程维护，
  进程 B 不再继承进程 A 的堆断点 / 匿名映射游标
- **execve 静态缓冲竞态修复**：`EXEC_PATH/ARGV/ENVP` 从 static 改为系统调用
  栈帧、ELF 读入缓冲从 static 改为按需堆分配 —— 消除定时器抢占下并发 execve
  覆写共享缓冲导致的加载污染
- **`sys_sysinfo` 真实内存统计 / `sys_uname` 名称修正**：内存按物理页分配器
  实时统计（此前硬编码 128MB）；`uname` 从 "NovaOS" 改为 "KunxOS"
- **virtio-blk used-ring 完成项 id 校验（H-A）**：每次请求使用轮转描述符组并
  校验 used 项 id —— 超时请求的迟到完成不再被误判为下一次请求成功（消除
  use-after-free 与读写错乱）
- **`open_file` 失败回滚（M-A）**：fd 表满时回滚已占用的打开文件表槽位，
  不再泄漏（32 槽耗尽 DoS）
- **unlink 拒绝删除仍打开的文件（H-B）**：防止 inode 号复用后旧 fd 与新文件
  形成逻辑别名
- **`pkg list` 走管道 Sink**：`pkg::list_to` 回调输出，`pkg list | cat` 下游
  可收到内容（此前直接 uart 输出绕过管道）
- **kunxpkg CLI 资源区边界 + 包名/资源名白名单（M-18/M-19 用户态侧）**：
  `parse` 逐条校验资源区长度、`valid_name` 拒绝 `/` 与 `..`（路径穿越）；
  `arg()` 改安全 UTF-8 校验
- **fdt.rs 字符串安全 UTF-8**：`read_cstr`/`string_at` 从 `from_utf8_unchecked`
  改为安全 `from_utf8`（任意字节 DTB 不再触发 UB）
- **FDT 头解析修复（virtio 设备检测回归）**：`size_dt_struct` 误读偏移 16
  （off_mem_rsvmap，典型值 ~48）导致结构块边界提前截断，virtio/net/blk 深层
  节点全部扫描不到 —— 改读偏移 36（size_dt_struct）。**此前 `/data` 持久化与
  真实网络收发从未生效**（networkd 的 tx/rx 为本地计数器，设备未被检测）
- **virtio-blk 驱动可用（/data 持久化真正工作）**：修复三处互相关联的缺陷——
  ① 寄存器布局按 QEMU virtio-mmio 实际偏移（legacy/modern 共享
  QueueSel@0x030/Status@0x070 等，此前误用 virtio-1.0 PCI legacy 偏移）；
  ② 环内 used ring 需按 QueueAlign（=PAGE_SIZE）对齐到独立页，环改分配 2 个
  连续页；③ avail 偏移此前误按「每请求 3 描述符总表长」计算，与设备按
  QUEUE_NUM 计算的布局不符。修复后 MBR 解析、sda1/2/3 分区注册、/data SFS
  挂载与持久化全部可用
- **virtio-blk 超时门控（H-A）**：请求超时后泄漏缓冲并置 `TIMEOUT_PENDING`，
  下次提交前先轮询消费陈旧完成项 —— 杜绝「把超时请求的迟到完成误判为本次
  成功」导致的 use-after-free 与读写错乱
- **TCP 序号回绕比较（RFC 1982）**：`seq_lt/gt/ge` 有符号差比较替代普通
  `<`/`>`（累计传输 ~4GB 后 32 位序号回绕，旧比较导致 rtx 提前清除、
  send 未等 ACK 即成功）
- **H-5 修复补全（重叠段补入队 + FIN 边界）**：对端重传整段而缓冲曾满时，
  段尾超过 `rcv_nxt` 的部分（未接收的新数据）补入队而不是当作纯重复丢弃
  （此前尾部数据永久丢失）；`data+FIN` 满缓冲场景下仅当 FIN 序号 == rcv_nxt
  才推进并 ACK，不再确认未入队数据
- **RST 后 recv 立即 EOF**：`T_CLOSED`（对端 RST）与 `peer_fin` 同等视为 EOF，
  不再干等 60s 超时
- **DNS label 长度上限**：>63 字节 label 直接跳过（此前 256..511 字节 label
  `as u8` 截断成畸形查询）
- **virtio-net used ring 缓存失效 + RX 初始 clean**：`drain_impl` 读 used ring
  前 `dcache_invalidate`（此前只 invalidate RX 数据缓冲，KVM/真机上 TX 永不
  回收、RX 永不收包）；`rx_ring_fill` 初始 desc/avail 写回缓存（防设备冷启动
  读到陈旧 desc addr=0）
- **mkfs 清零 inode 表块（L-A）**：新盘未分配的 inode 槽不再残留分配器
  随机内容（损坏盘/异常路径不会把垃圾当 T_DIR + 垃圾块号处理）
- **`fs_write` 超限报错（L-B）**：写入位置超过单文件上限（96KB）且一个字节
  未写入时返回 -1（此前静默返回 0，看起来「成功写了 0 字节」）
- **合成器 DAIF 保存/恢复（修复嵌套中断崩溃）**：`create/destroy/move/resize/
  set_z/set_alpha/set_scale_window` 的 IRQ 屏蔽从「无条件 daifclr」改为
  **保存原 DAIF 再恢复** —— `set_scale`/`move_window` 会从 IRQ 上下文
  （`anim::frame_tick` → `apply`）被调用，无条件 `daifclr` 会在中断处理器
  中途重新开中断 → 嵌套中断 → 内核跳转垃圾地址（ELR=0 / 0x4a / sp 进入
  用户栈区）随机崩溃
- **窗口销毁取消动画**：`destroy_window` 调用 `anim::cancel_window` ——
  窗口 id 复用后不再被残留旧动画驱动
- **compositor 全部 mutator 屏蔽 IRQ**：`move_window`/`set_z`/`set_alpha`/
  `set_scale` 与 IRQ 侧 `composite` 互斥（此前只有 create/destroy/resize 屏蔽）
- **input 读取路径 DAIF 保存/恢复**：`read_events`/`input_read` 从无条件
  `daifclr` 改为保存/恢复（嵌套屏蔽上下文不再提前开中断）
- **`visual_rect` u64 运算**：宽扁窗口 `w*scale` 不再 u32 溢出
- **ui_framework vbox/hbox 溢出钳制**：`item_h/gap` 超过 i32::MAX 不再回绕
- **panic 处理器防递归**：入口屏蔽 IRQ 并只打印首次故障（损坏 sp 下递归
  panic 不再无限刷屏）
- **fork/clone 恢复为「整体栈拷贝」设计**：子进程内核栈拷贝父栈活动区
  （含陷阱帧），拷贝区之下保留完整可用栈（~256KB）。修复并行重构
  「仅拷陷阱帧到栈顶」方案的偶发子进程 syscall 返回帧损坏（eret 到内核
  地址 → EL0 非法执行 → 进程被杀）；保留 `ctx.elr` 从帧读取（消除
  ELR=0 在飞窗口）与 `syscall_return_restore` 的 eret 目标校验
  （elr 必须落在用户区，否则终止进程，防故障→恢复→再故障循环）
- **fork/clone 陷阱帧放子栈顶部**：结合整体栈拷贝，子进程首次 eret 后内核
  sp = 子栈顶，后续 syscall 帧从栈顶向下分配 —— 与拷贝区（含 exec 的
  exec_path 等残留数据）隔离，进一步降低「残留污染 syscall 帧 elr → eret
  到 0」的偶发故障（配合 el0_fault 隔离，内核始终稳定）
- **★ syscall_return_restore 入口屏蔽 IRQ（消除 eret 到内核地址故障）**：
  syscall 体被 `daifclr` 放开抢占后，若定时器 IRQ 恰在「msr elr_el1 →
  eret」窗口抢占任务，switch_to 会把 elr_el1（=syscall 返回代码地址）存进
  任务上下文，恢复时 eret 跳回内核地址 → EL0 执行内核代码 → 故障。修复：
  恢复+eret 全程 `msr daifset`（eret 到 EL0 自动开中断），显著降低偶发
  用户态跳转内核地址的故障率（残余已被 el0_fault 隔离，内核始终 0 panic）
- **/data 持久化端到端验证**：echo 写入 → 重启 → cat 读取，文件跨重启保留
  （virtio-blk 修复后的完整闭环；此前 /data 从未真正工作）
- **shell 新命令验证**：`ps`/`uname`/`uptime`/`calc`（运算符优先级正确）/
  `ls`/`history`/`sysinfo` 等全部可用
- **★ 网络栈端到端验证（此前网络从未真实工作）**：virtio-net 驱动 + net_stack +
  socket 全链路可用 —— `ping 10.0.2.2` 收到 ICMP 应答；`nslookup example.com`
  真实 DNS 解析出 104.20.23.154；**`http_serve 8080` 经 QEMU slirp hostfwd 被
  宿主机 curl 访问，返回完整 HTML**（TCP listen/accept/read/write/close 全链路，
  含 T_LAST_ACK/序号回绕/H-5 修复的实际验证）
- **fdt `prop_str` 安全 UTF-8**：DTB 属性值不再 `from_utf8_unchecked`（任意
  字节输入为 UB；`read_cstr`/`string_at` 上一轮已修）
- **gic 越界防护**：`gic_set_priority`/`gic_set_target` 补 `irq < 1020` 断言
  （此前只有 enable/disable 有；irq≥1020 会写穿 GICD 寄存器窗）
- **shell normalize 溢出报错**：路径分量 >32 或超长分量放不下缓冲时返回空串
  （命令报错），不再静默截断成另一条路径去操作错误文件；`cd` 校验目标必须
  是目录（此前 `cd 普通文件` 会"成功"）
- **bootloader RAMDISK_MAX 注释修正**：0x80_0000 = 8MB（此前注释误写
  "128MB"）

### 正确性 / 安全修复（调度、内存、网络、文件系统）

- **★ 调度器上下文切换恢复点修复（消除 EL1 PC=0 崩溃）**：`sched_cfs::tick`/
  `yield_now`/`block_current`/`exit_current`/`sleep_until` 与 `timer::timer_handle_irq`
  全部 `#[inline(never)]`。此前 tick 被编译器内联进 `timer_handle_irq` 后，任务在
  tick 内 `do_switch` 的恢复点落在调用方中段，其后的 `pop x30; ret` 弹出错误栈槽位
  → 跳转垃圾地址（同步异常 ELR=0 / 用户态 PC=0）——表现为 fork/exit 后随机崩溃；
  多轮 QEMU 冒烟（内核直启与 bootloader 磁盘启动）均 0 崩溃
- **内核映射 W^X 强化（vm.rs）**：内核栈 / 数据 / BSS / 堆 / 页表 / ramdisk 等
  数据区映射全部加 `PXN`（EL1 禁止取指），低 1GB 设备区同理——消除「在内核
  数据区执行代码」的提权路径
- **`kfree` 边界校验（heap.rs）**：slab 释放校验指针归属（页内 + 对齐 + 非重复），
  大块释放校验页对齐与魔数——非法/重复释放不再破坏空闲链表
- **net_stack 加固**：ARP 协议字段校验（htype/ptype/hlen/plen）+ 拒绝学习
  0.0.0.0 与自身 + 缓存满时 LRU 淘汰；DNS 应答要求源端口=53 且查询未完成
  （防伪造应答）；IPv4 目的地址过滤（非本机/广播直接丢弃）；UDP 超 MTU
  载荷返回失败而非静默截断
- **socket 修复**：`bind` 端口占用检查（EADDRINUSE）；UDP 未绑定自动分配
  临时端口；重复/旧数据段回 ACK 帮助对端推进；**FIN 重传登记**（挥手阶段
  重传 FIN|ACK 而非旧数据段，M-10 部分修复）
- **virtio 驱动加固**：net_virtio RX 软件队列 32→64（与环深一致，突发不丢包）；
  `kmalloc` 返回值检查（失败不向地址 0 投递描述符）；队列 `QUEUE_NUM_MAX < 环深`
  时拒绝初始化（net_virtio 与 virtio_blk）
- **gpu fw_cfg DMA 等待超时**：设备异常时不再死循环
- **fdt 解析边界检查**：`struct_end`/`strings_size` 边界校验，损坏 DTB 不再越界读
- **shell 增强**：新增 `uname`/`clear`/`mkdir`/`rm` 内置命令；命令历史（↑/↓ 方向键
  浏览，16 条）；管道/重定向输出截断告警（>4096B 提示而非静默截断）
- **IPC 并发正确性（ipc.rs）**：管道/消息队列读写临界区全程屏蔽 IRQ（「检查→操作→
  唤醒」原子化，消除单核抢占下环形缓冲竞争与「登记后阻塞前」丢唤醒）；`wait_wake`
  改用 `wake_irq`；对象池满返回错误而非 panic（防 DoS）
- **input_stack 坐标溢出**：`to_screen` 定点缩放改用 i64 中间量（u16 全量程坐标
  不再 i32 回绕）
- **进程工作目录（M-2 部分）**：`chdir`/`getcwd` 真实实现（per-process cwd，
  fork 继承、exec 保留）；`openat`/`mkdirat`/`unlinkat`/`mknodat`/`faccessat`/
  `newfstatat` 相对路径按 cwd 解析
- **brk 收缩释放物理页（bionic_shim）**：`brk` 缩小断点区间时归还页并移除地址
  空间记录（此前只 unmap 不释放，反复 shrink/grow 泄漏页）
- **MAP_FIXED 语义补全（linux_compat）**：覆盖已有映射前先释放旧页
  （`unmap_user_range`）；分配中途失败回滚本次映射（不泄漏）
- **★ 文件系统 inode 容量修复**：`MAX_INODES` 64 → 128（内核 + mkfs_sfs.py 同步）。
  64 个 inode 在首次启动复制 16 个 OOBE 默认配置到 /data 后即耗尽，导致
  mkdir/写文件全部失败（inode 表 2→4 块，数据块仍充足）
- **rootfs 补 /tmp 目录**（Makefile），修复 `mkdir /tmp/...` 与临时文件场景
- **bootloader 跳转前缓存维护（jump.rs）**：`ic iallu` + 对已加载镜像区间
  `dc civac`（真机取指/读数据保证最新）；`load_all` 返回各镜像尺寸供维护
- **`sys_fcntl(F_DUPFD)` 真复制**：复用 `dup_fd_common`（此前返回原 fd 假语义）
- **物理页分配器按真实内存钳制**：`allocator::init` 按 DTB 的 `/memory` size
  限制可分配范围（QEMU 512MB，此前按 1GB 管理，超量分配会触碰未映射内存
  data abort）；`total_pages`/`free_page` 同步按限制校验；`sysinfo` 如实显示
  MemTotal
- **shell `sysinfo` 命令**：内存 / 任务 / 进程实时统计（MemTotal/MemUsed/Tasks）
- **接收端 IP/TCP/UDP 校验和验证**：`handle_ip` 校验 IP 头校验和与 TCP/UDP
  传输层校验和（含伪头，0 表示发送方未计算则跳过），损坏包直接丢弃，不再
  被当作有效数据解析/破坏连接状态
- **compositor Rect 溢出加固**：`right`/`bottom` 饱和加法（极端窗口位置/尺寸
  组合不再 i32 回绕）
- **★ SFS 位图容量钳制（fs_simple）**：单块位图最多跟踪 32768 数据块（128MB）；
  `total_blocks` 按位图容量钳制设备容量——修复 256MB /data 分区上 `df` 统计
  失真（把位图覆盖不到的块算入总量）与 mkfs/分配器计数不一致
- **★ TCP 服务器支持（listen/accept）**：socket 层新增 `T_LISTEN`/`T_SYN_RCVD`
  状态与待 accept 队列——服务器侧三次握手（SYN→SYN-ACK→ACK）、`listen()`/
  `accept()` 系统调用接线、监听套接字关闭时释放子连接；实测 QEMU hostfwd
  从宿主机完成回显（单连接 / 3000 字节大块 / 多连接均 PASS）
- **★ HTTP 服务器静态文件服务**：`http_serve <port> <root_dir>` 提供目录下
  文件（GET / → index.html、MIME 推断、404、拒绝 `..` 穿越）——实测 `/var/www/
  index.html` 200 返回、缺失文件 404；新增 `/var/www/index.html` 演示页与
  Makefile ramdisk 重建依赖（`$(wildcard rootfs/var/www/*)`）
- **shell `reboot`**：PSCI SYSTEM_RESET 重启（QEMU `-no-reboot` 下等价退出）
- **shell `poweroff`**：经 PSCI SYSTEM_OFF（HVC conduit）关机——QEMU virt
  下实测 `-no-reboot` 干净退出（exit=0）
- **shell `calc`**：整数算术求值器（递归下降，支持 + - * / % 与括号、
  一元负号、优先级）——实测 `calc (2+3)*4` → 20
- **TCP 服务器并发验证**：5 并发连接各自收发 + 顺序槽位回收均 PASS
- **shell `udp_echo` / `sort`**：UDP 回显服务器（hostfwd 实测 PASS）；
  `sort` 管道排序（`cat f | sort`）
- **shell `nslookup`**：DNS 域名解析（QEMU slirp DNS）——实测 `example.com` →
  `172.66.147.243`、`localhost` → `127.0.0.1`
- **shell `netstat`**：套接字表显示（FD/协议/状态/端口/远端），实测显示 LISTEN
- **`Sink::flush_uart`（阻塞命令输出修复）**：tcp_echo/http_serve 等进入事件循环
  前立即刷出缓冲输出（此前缓冲要等命令返回才输出，阻塞型命令永不显示）
- **★ HTTP 服务器（shell `http_serve`）**：极简静态页面服务器——宿主机
  curl/urllib 实测 status=200，完整返回 HTML（经 QEMU hostfwd）
- **shell `tcp_echo`**：TCP 回显服务器内置命令（配合 hostfwd 演示）
- **shell `ls` 字母排序 / `sysinfo` MemFree**：目录项按字节序排序输出；
  `sysinfo` 增加 MemFree 显示
- **shell `df`**：列出已挂载文件系统（设备/挂载点/容量/已用/空闲，位图实时统计）
- **shell `grep`**：子串过滤 + `-n` 行号，支持管道（`ls /bin | grep -n lau`）
- **eret 防御护栏（残余 eret-to-0 收口）**：`syscall_return_restore` 校验陷阱帧
  elr 非 0 且低于 0x40000000，损坏时走 `el0_fault` 干净终止进程——不再出现
  「EL0 取指 PC=0」的原始故障（多轮冒烟 0 复现）
- **shell `;` 命令分隔符 / `echo -n` / `env`**：`a; b` 顺序执行；`echo -n` 不换行；
  `env` 列出环境变量；Tab 补全识别 `;` 词边界
- **★ shell Tab 补全**：命令位置按内置命令 + /bin 补全（`ca`→`cat`）；
  路径位置支持 `/` 前缀（`ls /b`→`ls /bin`）、唯一匹配自动补齐、多候选
  换行列表示
- **shell `cd -` / `ls -a` / `ls -la`**：`cd -` 回到上一目录（OLDPWD）；
  `ls` 支持组合标志（-la），显示 `.`/`..` 与类型+大小
- **shell `history` / `uptime`**：`history` 列出命令历史；`uptime` 显示运行时间；
  `wc` 支持管道 stdin（无文件参数时统计输入流）
- **shell `cat -n` / `head`**：`cat -n` 逐行编号（含管道 stdin）；`head [-n N]`
  输出前 N 行（配合管道使用）
- **任务槽耗尽优雅降级（防 fork 炸弹 DoS）**：`alloc_tid` 不再 panic，
  `spawn`/`insert` 槽位不足返回 `u32::MAX`；`fork_parent`/`clone_parent`/
  `process::spawn`/`elf::spawn` 全部处理失败并回滚（回收子栈、地址空间与 fd 引用）
- **shell 新命令**：`ping <ip>`（内核网络栈 ICMP，实测可达 QEMU 网关）、
  `wc [file...]`（行/词/字节统计）、`ls -l`（类型 + 大小）
- **fork/clone 子进程栈布局重构（消除残余 eret-to-0）**：子进程/子线程内核栈不再
  整体拷贝父栈活动区，而是只把 272 字节系统调用陷阱帧拷到**子栈顶部**——子进程
  从 `syscall_return_restore` 直接 eret，不需要父调用链帧；原方案子进程可用栈只剩
  `copy_len-368` 字节，execve（argv/envp 数组 + setup_stack 2KB 帧）溢出踩坏
  陷阱帧 elr → 偶发 eret 到 PC=0 的 EL0 故障。重构后子进程获得完整栈空间


### 动画系统（对标 iOS / Android）

- **内核动画引擎**（`kunxcore/src/anim.rs`）：iOS Core Animation 与 Android Material
  缓动曲线（三次贝塞尔求解：牛顿 + 二分）、阻尼谐振子弹簧物理（iOS dampingRatio /
  Android stiffness 双风格，半隐式欧拉积分）、定长槽位动画管理器（窗口位置 / 透明度 /
  缩放补间与弹簧），定时器驱动 `frame_tick`，启动自检断言曲线采样值与弹簧收敛
- **合成器缩放**（`kunxcore/src/compositor.rs`）：窗口 `scale` 字段（定点 1.0 = 256）、
  以窗口中心为锚点的最近邻缩放合成、`set_scale`
- **定时器多回调**（`kunxcore/src/timer.rs`）：支持调度 tick 与动画帧 tick 并存
- **launcher 动效**（`apps/launcher/src/main.rs`）：图标按压反馈（0.85 缩放，
  FastOutSlowIn 120ms + 弹簧回弹）、应用启动转场（以图标为中心放大 1.12 + 白闪，
  EaseInEaseOut 300ms）、应用返回转场（快照缩放回桌面，EaseInEaseOut 280ms）
- **规范文档**：`docs/animations.md` 记录全部曲线 / 弹簧参数与转场规格

## [v0.1.0] - 2026-08-16

KunxOS 首个里程碑版本：一个面向 QEMU `virt`（ARM Cortex-A72）与 Raspberry Pi 4 的 Rust 裸机操作系统，具备内核、引导、用户态 init、系统服务、图形桌面、应用与包管理完整闭环。

### 内核 (kunxcore)

- **启动与内存**：MMU 4KB 恒等映射、`SCTLR_EL1.M` 使能；位图物理页分配器；slab 内核堆分配器；虚拟内存映射（`vm`）
- **进程与调度**：用户态进程 / 线程模型、上下文切换、CFS 调度器（`sched_cfs`）
- **系统调用**：Linux AArch64（asm-generic 编号）系统调用表、ELF 用户程序加载器
- **文件系统**：VFS 抽象、简单文件系统（`fs_simple`）、devfs（`/dev/fb0`、`/dev/input/event0`、`/dev/tty`）、virtio-blk 块设备、SFS 数据持久化（`/data`）
- **网络**：virtio-net 驱动、网络协议栈（`net_stack`）、socket / IPC
- **输入**：输入子系统（`input`、`input_stack`）、触摸事件处理
- **图形**：framebuffer 初始化（`gpu_simple`）、窗口合成器（`compositor`）、UI 框架（`ui_framework`）
- **兼容层**：Linux 兼容层（`linux_compat`）、bionic shim（`bionic_shim`）
- **其他**：FDT 设备树解析、GICv2 中断、PL011 UART、ARM Generic Timer、交互式 Shell（`shell`）、内置包管理（`pkg`）

### 引导 (kunxboot)

- 从 virtio-blk 读取 `KERNEL.IMG` / `KUNXOS.DTB` / `RAMDISK.IMG` 并跳转内核
- MBR + FAT32 文件系统解析
- SD / MMC 驱动
- DTB 解析
- Raspberry Pi 4 后端（DDR 初始化、Pi4 链接脚本）

### 硬件抽象层 (kunxhal)

- 平台抽象：display / input / audio / net / storage / power
- QEMU 与 Raspberry Pi 4 双平台后端

### init (kunxinit)

- EL0 用户态 init（PID 1 语义由内核承载）
- `/etc/inittab` 解析，支持 sysinit / once / wait / respawn
- 崩溃服务按节流策略自动重启

### 系统服务 (kunxserv)

- **networkd**：网络守护进程，状态查询与命名管道命令接口（STATUS / RENEW）
- **displayd**：显示守护进程（已废弃，launcher 直接操作 `/dev/fb0`）

### 应用 (apps)

- **launcher**：桌面启动器（状态栏、应用网格、底部 Dock、窗口 z-order 管理）
- **oobe**：首次开机向导（欢迎 → 语言 → 模块 → 地区/时区 → 网络 → 协议 → 账号 → 安全 → 主题 → 完成，共 10 步）
- **settings**：系统设置（恢复已移除模块）
- **terminal** / **calculator**：应用骨架（验证窗口生命周期）
- **store**：应用商店骨架（软件目录统计）

### 包管理 (kunxpkg)

- 命令：`pkg update | install <name> | remove <name> | list | search <kw> | info <name>`
- `.pkg` 包格式（NOVAPKG1 魔数 + FNV-1a 签名），本地 `file://` 软件源

### 工具 (tools)

- `mkdisk.py`：生成磁盘镜像（MBR + FAT32 + SFS rootfs + SFS data）
- `mkfs_sfs.py`：SFS ramdisk 打包
- `input_inject.py`：触摸输入注入
- `release.sh`：版本发布脚本

### 构建

- 顶层 Makefile：`kernel` / `init` / `networkd` / `rootfs` / `run` / `boot` / `pi4` 等目标
- QEMU virt 与 Raspberry Pi 4 双平台支持
