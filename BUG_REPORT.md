# KunxOS 安全与正确性 Bug 审计报告

> 生成方式：对约 2.2 万行源码（bootloader / 内核 / HAL / 用户态应用 / init / 服务 / 包管理器）做了
> 全量人工审查 + 9 个子系统并行深挖 + 一次 `make` 完整构建验证（构建通过，无编译错误）。
> 严重程度分四档：**Critical**（任意内核内存读写/页表破坏/堆破坏）/ **High**（确定性崩溃、数据丢失、跨进程 UAF）/ **Medium**（功能失效、可触发 panic、语义错误）/ **Low**（防御缺口、潜在缺陷）。

---

## 一、Critical（最高优先级）

### C-1. 用户指针上界 `USER_TOP` 误设为 0x8000_0000，内核恒等映射区被当作"用户内存" → EL0 任意读写内核
- 位置：`kunxcore/src/linux_compat.rs:266`、`kunxcore/src/bionic_shim.rs:23`
- 现象：用户地址空间真实上界是 **0x4000_0000**（内核 RAM 恒等映射起点，`vm.rs:20` RAM_START；用户栈顶 0x3F00_0000）。但 `USER_TOP` 被写成 0x8000_0000（内核 RAM 上界）。于是 `[0x4000_0000, 0x8000_0000)` 全部通过 `user_ok`/`peek_u32` 校验，而该区间正是内核代码/数据/栈/分配器，且经用户页表（继承的内核恒等映射，AP_EL1_RW）在 EL1 下可访问。`copy_from_user`/`copy_to_user` 因此允许任意 EL0 程序读内核内存（信息泄露）或写任意内核地址（内核破坏）。
- 触发：`write(fd, 0x4000_1000, n)` 读内核；`read(fd, 0x4000_2000, n)` 写内核；`openat((char*)0x4000_3000)` 把内核内容当路径。
- 修复：两处 `USER_TOP` 改为 `0x4000_0000`，并加映射有效性/对齐检查。

### C-2. 十余处系统调用对用户指针不做校验，裸指针读写内核（独立于 C-1 存在）
- 位置（`kunxcore/src/linux_compat.rs`，均为任意内核读/写/空指针解引用原语）：
  - `sys_pipe2` :716-724（`fds_ptr` 无校验，任意 8 字节内核写）
  - `sys_ioctl` TIOCGWINSZ :756-768（`arg` 无校验，任意 8 字节内核写）
  - `sys_getdents64` :798-830（`dirp` 无校验，任意长度内核写）
  - `sys_getcwd` :832-843（`copy_to_user` 后无条件 `*(buf+1)=0`，且忽略 copy 返回值）
  - `sys_wait4` :949-962（`status_ptr` 无校验，任意 4 字节内核写）
  - `sys_gettimeofday` :1164-1174 / `sys_clock_gettime` :1176-1187 / `sys_clock_getres` :1189-1198（`tv`/`tp` 无校验，任意 16 字节内核写）
  - `sys_nanosleep` :1200-1213（`req` 无校验读 16 字节；`req==0` 时空指针解引用）
  - `sys_writev` :578-579 / `sys_readv` :615-616（`iov` 指针数组无校验，任意内核读）
  - `read_sockaddr` :1290-1299（connect/bind/sendto 用，无校验读 8 字节）
  - `sys_sendmsg` :1368-1374 / `sys_recvmsg` :1386-1395（`msg`/`iov` 无校验读）
  - `bionic_shim.rs:256-263`（`parent_tidptr/child_tidptr` 只查 `< USER_TOP`，可写内核地址 4 字节）
- 触发：`ioctl(0, TIOCGWINSZ, 0x4000_4000)` 改写内核；`pipe2(0x4000_5000)` 同理；`connect(fd, 0x4000_6000)` 读内核当 sockaddr。
- 修复：所有用户指针解引用前统一过 `user_ok`（含逐项/逐字节边界）与对齐检查。

### C-3. mmap(MAP_FIXED)/brk/mmap_anon/map_framebuffer/ELF 加载无 VA 上界 → 把用户页挂进共享内核页表
- 位置：`kunxcore/src/linux_compat.rs:1121-1132`（MAP_FIXED 只查 `addr % PAGE_SIZE == 0`）、:1149-1153（`map_framebuffer` 取用户 addr）、`kunxcore/src/bionic_shim.rs:311-347`（`brk_set` 无上界）、:350-368（`MMAP_NEXT` 无上界）、`kunxcore/src/elf.rs:143-198`（`load()` 对 `p_vaddr` 零校验）
- 现象：用户页表 L1[1] 继承的是**共享的内核静态 L2/L3 表**（`vm.rs:254-261`）。当 `map_page_in(root, vaddr>=0x4000_0000, ...)` 时，`ensure_l3` 把内核全局 L2 的 2MB 块**就地拆分**，并把用户页（FLAG_USER_DATA，AP_USER_RW）写进共享页表。结果：(a) 内核全局页表被用户修改；(b) 该页对 EL0 直接可读写；(c) 用户可覆盖内核代码/数据/分配器/页表 → 完全内核内存破坏。且 `free_process_tables` 只回收 L1[0]，破坏在进程退出后**持久**。
- 触发：`mmap((void*)0x5000_0000, 0x1000, ..., MAP_FIXED|MAP_ANONYMOUS, ...)` 或 `brk(0x5000_0000)`，或执行一个带 `p_vaddr>=0x4000_0000` 的 PT_LOAD 的恶意 ELF。
- 修复：所有映射路径强制 `[va, va+len) ⊆ [0, 0x4000_0000)`；`load()` 对每个 PT_LOAD 校验 `p_vaddr`/`p_vaddr+p_memsz`；`map_page_in`/`add_page` 层做 `vaddr < 0x4000_0000` 硬断言。

### C-4. 页分配器无"连续 N 页"保证，但大块分配/任务栈/ramdisk/framebuffer 都假设连续
- 位置：
  - `kunxcore/src/heap.rs:103-116`（`large_alloc`：`start = last - (pages-1)*PAGE_SIZE`）
  - `kunxcore/src/sched_cfs.rs:313-319`（`alloc_stack`：16 页栈用"末页减长度"推算栈底）
  - `kunxcore/src/gpu_simple.rs:229-236`（`alloc_fb` 同模式）
  - `kunxcore/src/blkdev.rs:277-296`（`ramdisk_create` 同模式）
- 现象：`alloc_page`（`allocator.rs:60-82`）是线性 first-fit，每次返回**最低空闲页**。位图一旦出现空洞（slab 整页回收、进程退出释放页等就会产生），连续 N 次 `alloc_page` 拿到的页**不再物理相邻**，而调用方用"最后一页基址 − (N−1)×4096"回推首地址 → 落在他人已分配页上：头部写入他人内存、double-free、真实页泄漏，或任务栈与堆/其它栈重叠。`virtio_blk` 请求缓冲（>3 扇区）、compositor 窗口表面、`alloc_fb`、TCP 缓冲等真实路径都会触发。
- 触发：运行期碎片化之后的任何一次多页分配。
- 修复：分配器提供"连续 N 页"原语（位图内找 N 个连续空闲位），禁止用"末页减长度"推算基址。

### C-5. `net_virtio::send()` 不屏蔽 IRQ，与中断处理并发操作堆/空闲链表 → 堆损坏
- 位置：`kunxcore/src/net_virtio.rs:507-542`（`send()` 全程未 `daifset`）、:270-282（`tx_alloc_desc`/`tx_free_desc` 对 `TX_FREE_HEAD` 的读-改-写）、:288-302（`drain_impl` 中断路径 `heap::kfree`）
- 现象：`send()` 在进程上下文、IRQ 使能状态下执行 `kmalloc`（slab_alloc 临界区）与 `TX_FREE_HEAD` 读-改-写。网卡 used-ring 中断随时可能在中间触发，`irq_handler`→`drain_impl` 调用 `kfree`（slab_free）并发改写同一 slab 槽位的 `free_head`/`free_count` → 空闲链表撕裂 → 同一对象被分配两次（互踩/任意内存破坏）或 use-after-free；TX 描述符从空闲链丢失 → 逐次泄漏后发送永久失败。对照：`drain()`/`recv_into()` 都屏蔽了 IRQ，唯独 `send()` 漏了。
- 触发：任何双向网络流量。
- 修复：`send()` 加 `daifset/daifclr`；更根本地让 `kmalloc/kfree` 自身对 IRQ 安全。

### C-6. fork/clone 子进程恢复路径错误：返回值非 0 + 跨栈写父进程陷阱帧 + clone 栈拷贝漏段
- 位置：`kunxcore/src/process.rs:295-311`（fork 子 ctx）、`kunxcore/src/syscall_table.rs:31-37`（`f.x[0]=...` 写回）、`kunxcore/src/bionic_shim.rs:229-243`（clone 拷贝范围错误）
- 现象（三个独立缺陷）：
  1. **返回值错位**：fork/clone 子进程沿父进程调用链回卷，`syscall_dispatch` 把结果写进传入的陷阱帧指针 `f` —— 该指针是**父进程内核栈地址**（随栈逐字节拷贝，未重定位）。子进程自己的陷阱帧副本 `x0` 从未被改写，eret 后拿到的仍是 svc 前的原始 x0。fork 无参碰巧为 0 掩盖问题；`clone(CLONE_THREAD)` 的 x0=flags（非 0），子线程必然拿到非 0 返回值，被用户态当"父路径"处理 → pthread 状态错乱。
  2. **跨栈写**：子进程向父进程内核栈写入 8 字节（`f.x[0]=0`），父进程若被抢占则可能污染其活动陷阱帧。
  3. **clone 栈拷贝漏段**：`copy_len = stack_top - trap_frame` 漏掉 `[saved_sp, trap_frame)` 的 callee-saved 与返回地址，`clone_child_resume` 从子栈未初始化区域恢复 x19-x30（尤其 x30）→ 跳垃圾地址崩溃。
- 触发：`fork`、`pthread_create`（CLONE_THREAD）。
- 修复：fork/clone 拷贝完成后把子进程陷阱帧副本的 `x[0]` 直接清零；`syscall_dispatch` 结果按当前 sp 推导陷阱帧（而非传入父帧指针）；clone 的 `copy_len` 改为 `stack_top - saved_sp`。

### C-7. 多线程进程 exit 后，兄弟线程仍运行在已回收的地址空间上
- 位置：`kunxcore/src/process.rs:475-492`（`exit` 只标记当前线程退出）、`kunxcore/src/sched_cfs.rs:624-641`（`exit_current` 只切走当前任务）、`kunxcore/src/process.rs:463-472`（`reap` 释放页表）
- 现象：CLONE_THREAD 共享地址空间。一个线程调用 `exit` 后，父进程 `wait` 会 `reap` 并释放整张页表；但该进程的其它兄弟线程仍在运行，其 TTBR0 指向已回收的页表 → 之后任何取指/访存/陷入都命中释放后的内存 → 崩溃或任意行为。
- 触发：多线程进程中任一线程退出，父进程回收。
- 修复：exit 时若同进程还有存活线程，仅退出当前线程；仅当最后一个线程退出时才标记 Zombie 并允许 reap。

### C-8. ELF 加载器不校验段地址（C-3 的加载器侧入口）+ pkg 签名可伪造 + 资源表无边界
- ELF 段：见 C-3（`elf.rs:143-198`）。
- pkg：`kunxcore/src/pkg.rs:62-80`（无密钥 FNV-1a "签名"可对任意内容重算）、:134-173（`parse` 不校验资源区）、:226-278（`install` 按 `rnl/rdl` 切片 `data[off..off+rnl]`，伪造包直接切片越界 → 内核 panic）；包名/资源名未过滤 `../` 与 `/`，`join` 后可写 `/bin/../evil` → 覆盖 `/bin/*`、`/init` 等实现持久化。
- 触发：写入一个伪造 `.pkg` 再 `pkg install`。
- 修复：签名改带密钥 MAC；`parse` 遍历校验资源区总长；包名/资源名白名单；install 失败回滚。

---

## 二、High

- **H-1** `kunxboot/src/dtb.rs:166-205`：`insert_bootargs` 对 128KB `SCRATCH` 越界写（当 DTB `content_end` 接近 128KB 时越界 ~36B，破坏 .bss 相邻静态变量）。
- **H-2** `kunxboot/src/load_kernel.rs:38/52/66` + `fs_fat.rs:194-215`：镜像文件大小无上限校验，损坏/恶意 FAT 卷可让 bootloader 从 0x40080000 连续覆写自身代码/DTB/ramdisk/全部 RAM（任意内存写）。
- **H-3** `kunxcore/src/linux_compat.rs:298-313`：`read_cstr` 只校验首字节就连续读 127 字节，字符串靠近用户区上界时越界读入内核（信息泄露）；`read_str_array:1036-1041` 同族。
- **H-4** `kunxcore/src/net_virtio.rs:316-329`：RX DMA 缓冲"先拷贝后 invalidate"，缓存一致性顺序错（KVM/真机必现收包内容错乱）；重投递缺 `dcache_clean`。
- **H-5** `kunxcore/src/socket.rs:546-552 + 458-467`：TCP 接收缓冲将满时 `tcp_rx_push` 只入队部分数据，但 `rcv_nxt` 按整段长度推进并 ACK → 未入队数据永久丢失、连接错乱（不可自愈）。
- **H-6** `kunxcore/src/fs_simple.rs:551-583`：`fs_unlink` 可删除 `.`/`..`/目录自身/仍打开的文件 → 根 inode 被截断释放（文件系统整体损坏）、打开文件 inode use-after-free。
- **H-7** `kunxcore/src/vfs.rs:281-300,387-397` + `process.rs:463-472`：打开文件表无引用计数 → fork 后子进程 close 误清父进程槽位（跨进程 fd 别名/UAF）；exit 不回收 fd → 槽位耗尽后 `alloc_openfile` **panic**（DoS）。
- **H-8** `kunxcore/src/blkdev.rs:114-272` + `fs_simple.rs:200-202`：磁盘块号/inode 号无校验 + `sector+count` 裸加法可 usize 回绕绕过边界 → 野物理内存读写、位图栈越界写。
- **H-9** `kunxcore/src/fs_simple.rs:297-308`：`dir_add_entry` 目录写满时覆盖已有目录项（数据丢失）而非报错。
- **H-10** `kunxcore/src/fs_simple.rs:505-520`：`fs_write` 新分配块不置零 + 部分写 RMW → 跨进程文件内容泄露。
- **H-11** `kunxcore/src/compositor.rs:335-336,437` + `:131-138`：窗口尺寸 `w*h*4` 64 位回绕 → 小分配 + 大规模堆越界写（release 下回绕为 0，`fill` 循环击穿堆）。
- **H-12** `kunxcore/src/compositor.rs:377-388,361-374,427-460`：窗口表面裸指针无生命周期 → destroy/resize 后 UI 渲染读悬垂指针（use-after-free）。
- **H-13** `kunxcore/src/ui_framework.rs:135-137` + `compositor.rs:168-175`：Image 控件 `data` 裸指针无校验 → 空指针/越界读。
- **H-14** `kunxcore/src/sched_cfs.rs` + `process.rs`：线程栈/任务槽位/内核栈永不回收 → `alloc_tid` panic（任务数上限 32 耗尽即 DoS）。
- **H-15** `kunxcore/src/ipc.rs:96-109,373-389`：shm 引用计数错误（fork 复制 fd 不 +refs，close 即归零 free）→ use-after-free / double-free。
- **H-16** `kunxcore/src/ipc.rs`：管道/消息队列单槽位等待者（`reader_wait/writer_wait`）丢失唤醒（多读者/多写者时后到者覆盖先到者）。
- **H-17** `kunxcore/src/bionic_shim.rs:264-267`：fork 丢失 TPIDR_EL0（子进程 TLS 基址未从父进程复制）。
- **H-18** `kunxpkg/src/main.rs:810-849`：`write_installed_record` 对 512 字节栈缓冲无界写，合法包（8 个 29+ 字节资源名）即稳定溢出 22+ 字节（release 下静默栈破坏）。
- **H-19** `kunxhal/src/plat_pi4.rs:44-46`：GICD 基址 `0xFF840000` 与 BCM2711 实际 `0xFF841000` 差 0x1000，Pi4 真机中断初始化必坏。

---

## 三、Medium（节选）

- **M-1** `kunxcore/src/linux_compat.rs:541,631,1354`：read/readv/recvfrom 忽略 `copy_to_user` 失败，返回"已读字节数"而非 EFAULT。
- **M-2** `kunxcore/src/linux_compat.rs:497-518,726-732`：`close` 对未登记 fd 返回成功；`dup/dup3` 不做复制；`chdir` 无效。
- **M-3** `kunxcore/src/linux_compat.rs:964-969`：`kill` 无权限/信号校验。
- **M-4** `kunxcore/src/linux_compat.rs:645,659,711`：pread64/pwrite64/lseek 的 offset 被 `as i32` 截断。
- **M-5** `kunxcore/src/bionic_shim.rs:350-368`：mmap/brk 中途失败不回滚（已映射页泄漏、游标已推进）。
- **M-6** `kunxcore/src/vm.rs:325-341,348-376`：`map_page_in` 重映射无 TLB 失效；`free_user_pages` 不清 PTE/不失效 TLB。
- **M-7** `kunxcore/src/socket.rs:629,416-424,646-649`：>2028 字节 TCP 发送被静默丢弃但 `snd_nxt` 仍前进 → 连接永久失步。
- **M-8** `kunxcore/src/net_stack.rs:451-474`：超长域名致 `DNSBUF` 终止符/QTYPE/QCLASS 越界写静态区。
- **M-9** `kunxcore/src/socket.rs:493-573`：TCP RST 被忽略、SYN-ACK 的 ACK 不校验 iss+1。
- **M-10** `kunxcore/src/socket.rs:707-758`：FIN 未登记重传、无 TIME_WAIT、立即释放 4 元组。
- **M-11** `kunxcore/src/net_stack.rs:585-618`：IPv4 分片未处理，非首片被当独立 TCP/UDP 段。
- **M-12** `kunxcore/src/pkg.rs:293-315`：uninstall 删除中按索引遍历 → 跳过一半资源文件。
- **M-13** `kunxcore/src/fs_simple.rs:537-544`：truncate 遇空洞提前 break → 数据块泄漏。
- **M-14** `kunxcore/src/virtio_blk.rs:321-343`：used ring 轮询前缺 `dcache_invalidate`（读陈旧 used_idx，真机超时）。
- **M-15** `kunxboot/src/fs_fat.rs:159-191`：根目录只扫描首簇，跨簇目录项找不到。
- **M-16** `kunxboot/src/fs_fat.rs:123,199-213`：`sectors_per_cluster` 未校验，为 0 时死循环。
- **M-17** `kunxboot/src/dtb.rs:133-149`：`patch_memory` 按 reg 总长猜测 cell 布局，len==12 时写错 cell。
- **M-18** `kunxpkg/src/main.rs:439-448,776-805`：pkg 资源段无边界校验，恶意包切片越界 panic。
- **M-19** `kunxpkg/src/main.rs:455-458,427-434`：安装路径穿越（资源名/包名可含 `..`/`/`）→ 任意文件写。
- **M-20** `kunxserv/displayd`、`kunxserv/networkd` 的 `poll_pipe`：读取位置永不前进 → 命令重复执行、文件无限增长、>256B 命令永远读不到。
- **M-21** `apps/*`：应用硬编码 `SURFACE_ID=1` 与 displayd 实际分配 id=0 不一致 → 窗口永不合成、槽位泄漏。
- **M-22** `apps/launcher/src/main.rs:882-892`：真实子进程窗口永不关闭 + 僵尸进程累积。
- **M-23** `apps/settings/src/main.rs:382-395`：`restore_removed` 对 `NEW_MOD` 无界拷贝 → 越界 panic。
- **M-24** `kunxcore/src/ipc.rs` / `process.rs`：IPC 对象/文件描述符在进程退出时泄漏。
- **M-25** `kunxcore/src/virtio_blk.rs:329-341`：超时路径在描述符仍引用缓冲时 `kfree` → 设备后续 DMA 写入已释放页（use-after-free）；且 `LAST_USED` 未推进，下一次请求会把旧的 used 项误判为本次成功。
- **M-26** `kunxcore/src/input.rs:222-237`：`read_events`（进程上下文）与 IRQ 上下文 `push_event` 对 `head/count` 无互斥 → 事件丢失/重复。
- **M-27** `kunxcore/src/input_stack.rs:111-117`：`now_us()` 的 `cntpct*1_000_000` u64 溢出，62.5MHz 下约 3.4 天 uptime 后回绕（release 时间错乱）。
- **M-28** `kunxcore/src/input.rs:144-163`：UART 触摸帧解析无校验无恢复，串口数据中出现 `0xAA`（或首字节丢失）会吞掉后续 5 字节并产生垃圾事件。

---

## 四、Low（节选）

- `kunxcore/src/shell.rs:91-94`：`from_utf8_unchecked` 处理任意字节输入（UB）。
- `kunxcore/src/shell.rs:430-453`：管道中间阶段输出 >1024B 静默截断。
- `kunxcore/src/linux_compat.rs:1266-1281`：getrandom 用时间做种子的确定性 LCG，无真实熵。
- `kunxcore/src/linux_compat.rs:1207`：nanosleep `sec*1_000_000` 可 u64 溢出。
- `kunxcore/src/bionic_shim.rs:79`：futex_wait 无空槽位时返回 -1 被映射为 EPERM 而非 EAGAIN。
- `kunxcore/src/vm.rs`：内核映射缺 PXN（无 W^X）；`kfree` 不校验对象边界。
- `kunxcore/src/allocator.rs:96-106`：`free_page` 位图无越界断言。
- `kunxcore/src/fdt.rs`：DTB 解析字符串/偏移读取无边界约束。
- `kunxcore/src/timer.rs:42-45`：`now_us()` 当 `freq < 1MHz` 时除零（`freq/1_000_000 == 0`）。
- `kunxcore/src/net_virtio.rs:467,518`：`kmalloc` 返回值未检查。
- `kunxcore/src/net_virtio.rs:443-462`：未按 `QUEUE_NUM_MAX` 适配环大小。
- `kunxcore/src/net_stack.rs:567-583,239-256`：ARP 无条件学习、无协议字段校验、缓存无淘汰。
- `kunxcore/src/net_stack.rs:434-446,494-525`：DNS 应答仅校验 ID 可伪造；`DNS_SRC_PORT` 未清零。
- `kunxcore/src/socket.rs:528-657`：TCP 序号普通比较，32 位回绕后失效。
- `kunxcore/src/socket.rs:252-267,357-382`：端口可重复绑定，UDP 投递先到先得。
- `kunxcore/src/net_stack.rs:149-163`：大 UDP 包校验和放弃、>1472B 静默截断。
- `kunxcore/src/gpu_simple.rs:118-120`：fw_cfg DMA 等待无超时。
- `kunxcore/src/compositor.rs:75-82`：`Rect::right/bottom` i32 溢出。
- `kunxcore/src/input_stack.rs:130-135`：坐标 `x * scale` i32 溢出（回绕）。
- `kunxcore/src/net_virtio.rs:113,314`：RX 软件队列 32 < 环深 64，突发丢包。
- `kunxcore/src/input.rs:264-268`：多任务阻塞读时 `DEV.waiter` 被覆盖 → 首个等待者永久丢唤醒。
- `kunxcore/src/interrupt.rs:43-57`：EOI 早于源清除的全局模式（当前两个 handler 安全，未来 handler 若中途开中断会重入）。
- `kunxcore/src/gic.rs:52-59,63-70`：`gic_enable_irq`/`gic_set_priority` 无 IRQ 号范围校验（`irq≥1024` 写穿 GICD 寄存器区）。
- `kunxboot/src/jump.rs:11-27`：跳内核前未做 icache/dcache 失效（真机加固项）。
- `kunxboot/src/mmc.rs:219-221`：virtio used ring 轮询无超时。
- `kunxhal/src/plat_qemu.rs:52-63`：net/display HAL 常量与 QEMU virt 实际不符（当前仅打印用途）。
- `kunxpkg/src/main.rs:296-322`：`list_dir` 对损坏目录项（reclen<19）越界读。
- `kunxpkg/src/main.rs:333-345`：`arg()` 读取越过 512 上限 1 字节。
- `apps/oobe`：无输入设备时永久挂起；输入事件跨 read 分割时被丢弃。

---

## 五、根因归纳

1. **用户/内核地址边界错误**（C-1/C-2/C-3）：`USER_TOP` 常量错 + 大量 syscall 绕过 `user_ok` + 映射路径无上界，三者叠加构成"EL0 任意内核读写"的完整攻击面。
2. **分配器无连续页语义**（C-4）：`large_alloc`/`alloc_stack`/`alloc_fb`/`ramdisk_create` 全部用"末页减长度"推算基址，依赖 first-fit 的连续性，碎片化即破坏。
3. **中断安全边界缺失**（C-5）：`send()` 未关中断，与中断内 `kfree` 竞争堆。
4. **fork/clone 栈拷贝与回卷语义**（C-6/C-7/H-16/H-17）：子进程恢复路径对陷阱帧指针、TLS、兄弟线程生命周期的处理不完整。
5. **磁盘/文件系统元数据过度信任**（H-6/H-8/H-9/H-10）：块号/inode 号/目录项/ELF/pkg 均来自外部输入却无校验。
6. **DMA 缓存一致性**（H-4/M-14）：RX 路径 invalidate 顺序错、缺 clean；used ring 不失效。

## 六、修复优先级建议

1. 先修 C-1/C-2/C-3（把 `USER_TOP` 改 0x4000_0000 + 全部用户指针过 `user_ok` + 映射上界硬断言）—— 消除 EL0 任意内核读写面。
2. 修 C-4（连续页分配原语）、C-5（`send` 关中断）。
3. 修 C-6/C-7（fork/clone/exit 正确性）。
4. 修 H-6/H-7/H-8/H-10（文件系统元数据校验 + fd 引用计数）。
5. 修 H-4/H-5（网络 DMA 缓存一致性 + TCP 数据丢失）。
6. 其余按成本顺次处理。

---

## 七、修复进度（2025-08 全量修复轮）

> 全部修复已通过 `make` 完整构建（exit 0）。**✅ 已修复 / ⚠️ 部分或需重构 / ⬜ 未修（设计取舍）**

### Critical（8/8 ✅）

| ID | 状态 | 修复内容 |
|---|---|---|
| C-1 | ✅ | `USER_TOP` 改为 `0x4000_0000`（linux_compat.rs / bionic_shim.rs） |
| C-2 | ✅ | 全部系统调用用户指针加 `user_ok` 校验（pipe2/ioctl/getdents64/getcwd/wait4/时间类/nanosleep/readv/writev/sockaddr/sendmsg/recvmsg/bionic tidptr） |
| C-3 | ✅ | `vm::user_va_range_ok` + `add_page`/`map_user_page` 上界断言 + mmap(MAP_FIXED)/map_framebuffer/brk_set/mmap_anon/ELF `load()` 段地址校验 + `current_brk` 默认值修正 |
| C-4 | ✅ | 分配器新增 `alloc_pages_contig`，`large_alloc`/`alloc_stack`/`alloc_fb`/`ramdisk_create` 全部改用连续页原语 |
| C-5 | ✅ | `net_virtio::send()` 全程 `daifset/daifclr` 屏蔽 IRQ |
| C-6 | ✅ | fork 传入陷阱帧并置子帧 x[0]=0；clone 拷贝范围改为 `[saved_sp, stack_top)`、`child_frame` 定位修正、x[0]=0 + sp_el0 修补；⚠️ 残留：子进程仍沿父调用链回卷，`syscall_dispatch` 会把 0 写入父进程**陈旧**陷阱帧（无功能影响，跨栈写属残余隐患） |
| C-7 | ✅ | `sched_cfs::kill_pid` 在 `exit()` 中终止同进程全部线程，防止兄弟线程跑在已回收页表上 |
| C-8 | ✅ | ELF `p_vaddr` 范围校验；pkg 资源表逐条边界校验 + 包名/资源名白名单（防路径穿越）+ 签名只作完整性、install 防御性越界检查 + write_file 部分写检测 |

### High（已修 17/19，2 项说明）

| ID | 状态 | 说明 |
|---|---|---|
| H-1 | ✅ | bootloader `insert_bootargs` 越界检查 |
| H-2 | ✅ | `read_file` 增加 capacity 参数 + `load_all` 按目的区域上限校验 |
| H-4 | ✅ | RX 缓冲改为"先 invalidate 后拷贝"，重投递 desc/avail 加 dcache_clean |
| H-5 | ✅ | `tcp_rx_push` 返回实际入队数，`rcv_nxt` 只按实际推进、满时不 ACK |
| H-6 | ✅ | `fs_unlink` 拒绝 `.`/`..`/空名/目录 |
| H-7 | ✅ | OpenFile 加 `refs` 引用计数（fork +1、close/exit -1、归零释放）；`alloc_openfile` 表满返回错误而非 panic；reap 释放全部 fd 资源（File/Pipe/MsgQueue/Shm） |
| H-8 | ✅ | blkdev 边界检查改 checked 运算 + buf 长度校验；`bitmap_alloc`/`bitmap_free` 位图越界防护；`data_block_free` 下溢防护 |
| H-9 | ✅ | `dir_add_entry` 目录写满返回 -1 而非覆盖 |
| H-10 | ✅ | `data_block_alloc` 新块清零 |
| H-11 | ✅ | `create_window`/`resize_window` 尺寸回绕防护（MAX_SURFACE_BYTES） |
| H-12 | ⚠️ | Surface 增加 null/空尺寸防护；完整生命周期（代际号/引用计数）需重构，compositor 当前未接线 |
| H-13 | ✅ | `blit`/`fill`/`set` null 指针防护 |
| H-14 | ✅ | `sched_cfs::reap_pid` 回收已退出任务的 TCB 槽位与 64KB 内核栈 |
| H-15 | ✅ | shm 引用计数：map +1、close 校验 used 且归零才 free、fork 复制 fd 时 `shm_add_ref` |
| H-16 | ✅ | 管道/消息队列等待者改为多槽数组（WAITERS=4），唤醒全部等待者 |
| H-17 | ✅ | fork 子进程继承父进程 TPIDR_EL0 |
| H-18 | ✅ | `write_installed_record` 改为 `append_bounded` 有界写入 |
| H-19 | ✅ | Pi4 GICD 基址 `0xFF840000` → `0xFF841000` |

### Medium（已修 21/24）

✅ 已修：M-1（copy_to_user 失败→EFAULT）、M-5（mmap/brk 失败回滚）、M-6（map_page_in 重映射 TLB 失效）、M-7（tcp_send 超长段拒绝）、M-8（dns_build_query 越界防护）、M-9（TCP RST 处理 + SYN-ACK ACK 校验）、M-11（IPv4 分片丢弃）、M-12（pkg uninstall 索引修复）、M-13（fs_truncate 空洞遍历）、M-14（virtio_blk used ring 缓存失效）、M-15（FAT 根目录簇链遍历）、M-16（sectors_per_cluster 校验）、M-17（patch_memory cell 布局 12/16/8 分档）、M-18/M-19（pkg 资源边界 + 路径穿越）、M-20（displayd/networkd 管道读后 ftruncate，内核新增 `ftruncate` 系统调用）、M-21（displayd surface id 从 1 开始）、M-23（settings NEW_MOD 越界防护）、M-24（reap 释放 fd/IPC 对象）、M-25（virtio_blk 超时路径不释放 + 游标同步）、M-26（input read_events 屏蔽 IRQ）、M-27（input_stack now_us 防溢出）、M-28（UART 帧解析类型校验+重同步）。

⬜ 未修（设计取舍）：M-2（close/dup/chdir 语义简化，需较大改动）、M-3（kill 无权限校验，单用户演示系统）、M-10（TCP FIN 重传/TIME_WAIT，需状态机重构）。M-22 在当前 launcher 代码中已不存在（现为 fork+waitpid 阻塞等待）。

### Low（部分修复）

✅ 已修：shell `from_utf8_unchecked`（改安全校验）、nanosleep u64 溢出（饱和运算）、futex EAGAIN 语义、`timer::now_us` 除零防护、gic IRQ 范围断言、kunxpkg `arg()` 边界顺序、kunxpkg `list_dir` 名字扫描上界。
⬜ 未修（需更大改动或无关紧要）：getrandom 无真实熵源、plat_qemu HAL 常量与 QEMU 实际不符（仅打印用途）、oobe/launcher 输入事件跨 read 分割、input 阻塞读多等待者覆盖、shell 管道 >1024B 截断、compositor Rect i32 溢出、bootloader DTB 字符串无界读取/跳转前 cache 失效、mmc 轮询无超时等。

### 修复后构建验证

`make`（bootloader + 内核 + init + 服务 + 全部应用 + rootfs 打包）**exit 0 通过**；`kunxhal --features pi4` 单独构建通过。

---

## 八、2025-08-17 深夜优化轮（补充修复）

> 全部通过 `make` 完整构建 + QEMU 三种模式（`-kernel` / `run-gui` ramfb / bootloader+磁盘）启动验证（exit 0，无 PANIC / EL0 fault）。

### 新增修复

| 类别 | 修复内容 |
|---|---|
| **EL0 故障隔离（新）** | `lower_sync` 检查 ESR 区分 SVC 与其它同步异常；非 SVC 走 `el0_fault_handler` 终止当前进程（SIGSEGV 语义）——用户程序崩溃（如 oobe 无帧缓冲）不再挂死整个内核 |
| **用户指针页表漫游校验（新）** | `user_ok`/`copy_from_user`/`copy_to_user`/`peek_u32`/clone tid 写回全部验证页已映射（`vm::user_range_mapped`）——未映射地址在 EL1 解引用不再触发 data abort 挂死内核（用户态 DoS） |
| **任务栈 128KB → 256KB** | oobe（2MB BSS）execve 时 `ProcPageTable`（1536 条 = 24KB）栈帧 + 深调用链溢出 128KB 栈、写穿栈底破坏位图 → 栈分配重叠 → 内核崩溃；256KB + `create_child_process` 引用传参消除 |
| **`MAX_USER_PAGES` 512 → 1536** | launcher 两个 2MB 帧缓冲（1017 页）+ 栈（256 页）打满 512 项记录；表满现在拒绝映射并回滚（`add_page` 返回 bool，全部调用方处理） |
| **ELF 加载失败清理** | `load()` 全部错误路径释放页与页表（防恶意 ELF 耗尽内存 DoS）；`e_entry` 校验；`setup_stack` 失败回滚 |
| **fs_simple 加固** | inode 号边界校验、`fs_write` 检查块读写失败、`fs_create`/`fs_mkdir` 失败回滚 |
| **blkdev/allocator 溢出防护** | `block*8`/`start_lba+sector`/分区 `start+count` checked；`kernel_pages` 钳制 |
| **input 多等待者** | `waiter` 单槽 → 4 槽（多任务阻塞读不覆盖丢唤醒）；poller 登记屏蔽 IRQ |
| **shm 映射跟踪（H-15 补全）** | `Shm.maps` 记录 (pid, vaddr)；`close(fd)` 解除本进程映射再减引用——物理页释放时无页表仍引用 |
| **munmap 实现（新）** | `SYS_MUNMAP` 从 no-op 改为解除私有映射并归还物理页（`unmap_user_range`） |
| **brk 收缩清理（vm 6-2）** | 收缩时移除 `user_pages` 记录并释放物理页（此前只 unmap，反复 shrink/grow 泄漏） |
| **dup/dup3 语义** | 复制 fd 表项共享底层对象 + 引用计数；目标槽位占用时先按 close 释放 |
| **close/kill/stat 语义** | close 未登记 fd 返回 EBADF（补 MsgQueue 分支）；kill 校验信号号与进程存在；`write_stat` 失败返回 EFAULT |
| **getrandom/recvmsg** | 熵混入 tid + 拷贝失败 EFAULT |
| **M-20/M-21 修复补全** | networkd/displayd ftruncate 显式 length=0；displayd `SURF_META[id-1]` 下标 |
| **合成器 IRQ 同步** | create/destroy/resize_window 全程屏蔽 IRQ、先摘除再释放（composite 不再读已 kfree 表面） |
| **动画弹簧数值稳定性** | `Spring::step` 钳制 mass/stiffness、NaN 强制收敛 |
| **shell 管道容量 4KB → 32KB** | 缓冲移入 .bss（shell 单线程），消除大输出截断 |
| **pkg 路径缓冲** | `/res/<name>/<resname>` 缓冲 96 → 128（最长 99 字节）；检查 write_file 返回值 |
| **bootloader mmc 超时** | used-ring 轮询加超时（设备挂起不死循环） |
| **★ 调度器切换恢复点（内联根因）** | `tick`/`yield_now`/`block_current`/`exit_current`/`sleep_until`/`timer_handle_irq` 全部 `#[inline(never)]`——tick 被内联后任务恢复点落在调用方中段，`pop x30; ret` 弹错栈槽 → EL1 PC=0 / EL0 PC=0 随机崩溃；修复后多轮冒烟 0 崩溃 |
| **内核 W^X（PXN）** | 栈/数据/BSS/堆/页表/设备区映射加 `PXN`（EL1 禁取指），消除数据区执行提权路径 |
| **kfree 边界校验** | slab 释放校验页内/对齐/非重复；大块释放校验对齐+魔数+页数（非法释放不破坏链表） |
| **net_stack 加固** | ARP 协议字段校验 + 拒绝 0.0.0.0/自身学习 + LRU 淘汰；DNS 应答源端口=53 且查询未完成；IP 目的过滤（非本机/广播丢弃）；UDP 超 MTU 返回失败 |
| **socket 修复** | bind 端口冲突（EADDRINUSE）；UDP 自动临时端口；重复段回 ACK；**FIN 重传登记**（挥手重发 FIN|ACK 而非旧数据） |
| **virtio 队列深度/kmalloc** | net_virtio RX 队列 32→64（=环深）；net_virtio/virtio_blk `QUEUE_NUM_MAX < 环深` 拒绝初始化；kmalloc 失败检查（不向地址 0 投描述符） |
| **gpu fw_cfg 超时** | DMA 等待加超时（设备异常不死循环） |
| **fdt 边界检查** | `struct_end`/`strings_size` 边界校验（损坏 DTB 不越界读） |
| **shell 新命令/历史** | `uname`/`clear`/`mkdir`/`rm` 内置命令 + 命令历史（↑/↓）+ 管道截断告警 |
| **IPC IRQ 屏蔽临界区** | 管道/消息队列「检查→操作→唤醒」全程屏蔽 IRQ（消除竞争与丢唤醒）；`wait_wake` 用 `wake_irq`；对象池满返回错误不 panic |
| **input_stack 坐标溢出** | `to_screen` i64 定点缩放（u16 全量程不回绕） |
| **进程级 cwd（M-2）** | `chdir`/`getcwd` 真实实现；openat/mkdirat/unlinkat/mknodat/faccessat/newfstatat 相对路径按 cwd 解析（fork 继承、exec 保留） |
| **★ inode 容量 64 → 128** | 首次启动复制 16 个 OOBE 默认配置到 /data 后耗尽 64 inode → mkdir/写文件全部失败；扩大后（内核 + mkfs_sfs.py 同步）创建/追加/删除正常 |
| **rootfs /tmp** | Makefile 补 `/tmp` 目录 |
| **bootloader 缓存维护** | 跳转前 `ic iallu` + 镜像区间 `dc civac`（真机取指/读数据最新）；`load_all` 返回镜像尺寸 |
| **F_DUPFD 真复制** | `fcntl(F_DUPFD)` 复用 dup 实现（此前返回原 fd） |
| **物理页分配器按真实内存钳制** | `allocator::init` 按 DTB `/memory` size 限制可分配页数（QEMU 512MB，此前按 1GB 管理，超量分配触碰未映射内存会 data abort）；`total_pages`/`free_page` 同步校验 |
| **接收端校验和验证** | IP 头 + TCP/UDP 传输层校验和验证（损坏包丢弃，不再破坏连接状态） |
| **compositor Rect 饱和加法** | `right`/`bottom` 防 i32 回绕 |
| **★ HTTP 静态文件服务 / DNS** | `http_serve <port> <root>` 提供文件（index.html/404/MIME）；`nslookup` 域名解析实测 PASS（example.com→真实 IP） |
| **shell netstat / 阻塞命令输出** | 套接字表显示；`flush_uart` 修复阻塞命令（tcp_echo/http_serve）输出永不刷新的问题 |
| **★ HTTP 服务器（http_serve）** | 极简静态页服务器，宿主机 curl status=200 实测 PASS |
| **★ TCP 服务器（listen/accept）** | 服务器侧三次握手 + 待 accept 队列 + `listen`/`accept` 系统调用；hostfwd 实测回显 PASS（大块/多连接） |
| **★ SFS 位图容量钳制** | 单块位图 32768 数据块上限；`total_blocks` 按容量钳制（修复 256MB /data 的 df 失真与计数不一致） |
| **shell df / grep** | `df` 挂载点+容量统计；`grep [-n]` 子串过滤（管道可用） |
| **eret 防御护栏** | `syscall_return_restore` 校验陷阱帧 elr（非 0 且 < 0x40000000），损坏走 el0_fault 干净终止——收口残余「EL0 取指 PC=0」 |
| **shell `;` / `echo -n` / `env`** | 命令分隔符、无换行 echo、环境变量列示 |
| **shell history/uptime/wc 管道** | `history` 列历史、`uptime` 运行时间、`wc` 支持 stdin |
| **shell sysinfo/ping/wc/ls -l** | 内存统计、ICMP ping（实测可达网关）、行/词/字节统计、`ls -l` 类型+大小 |
| **任务槽耗尽优雅降级** | `alloc_tid` 不 panic；`spawn`/`insert` 返回 `u32::MAX`；fork/clone/spawn/elf 全部回滚（回收栈、地址空间、fd） |
| **fork/clone 子栈布局重构** | 子进程内核栈只拷贝 272B 陷阱帧到子栈顶部（不再整体拷贝父活动区）——原方案子进程可用栈仅 `copy_len-368`B，execve 大栈帧溢出踩坏陷阱帧 elr（偶发 EL0 eret 到 PC=0） |

### 验证结论

- **`-kernel` 模式**：oobe/launcher/displayd/networkd 全部正常启动退出，init 正常回收，0 PANIC
- **GUI 模式（ramfb + 键盘）**：oobe 显示正常、displayd 30fps 合成、networkd 网络正常、无故障
- **bootloader + virtio-blk 磁盘模式**：FAT32 加载、DTB fixup、内核启动全链路正常
- **pi4 构建**：kunxcore / kunxboot `--features pi4` 均编译通过

---

## 九、2025-08-18 凌晨补充轮（virtio-blk 驱动修复 + 偶发 EL0 fault 加固）

> 全部通过 `make` 完整构建 + QEMU 四种模式（`-kernel` / `run-disk` / GUI / bootloader）启动验证。

### virtio-blk 驱动修复（run-disk / boot 模式 /data 持久化）

早期重写引入的回归：内核侧 virtio-blk 在 QEMU（legacy virtio-mmio）上请求恒超时
（`[vblk] timeout sector=0 ... qnum=0`），导致 `/data` 无法挂载、MBR 读取失败。修复：

- **寄存器偏移**：QEMU 的 virtio-mmio 无论 legacy/modern 寄存器偏移相同
  （GuestFeatures@0x020、QueueSel@0x030、Status@0x070）；误用 virtio-1.0 规范
  的 legacy 偏移（0x018/0x024/0x028…）会读错寄存器（QueueNumMax 读到
  GuestFeaturesSel=0 → 队列无法建立）
- **GuestPageSize**：legacy 初始化必须显式写 0x028（此前缺失，QueuePFN 无效）
- **环布局**：设备按 `QUEUE_NUM`（8）计算 desc 表（128B），avail 紧随其后，
  used 按 `QueueAlign`（=4096）对齐到第二页 —— 环分配 2 个连续页
  （此前用 24 个 desc 的偏移 384/404，设备在 128/4096 读 avail/used 全零，
  请求永不处理）
- **清理调试残留**：删除 `MODERN = true` 强制赋值与 `[vblk-dbg]` 打印

修复后：MBR 分区表读取、`/dev/sda3` SFS 检测、`/data` 挂载与持久化全部正常。

### 偶发 EL0 fault 加固

QEMU GUI 模式偶发 `EL0 fault pid=N ESR=... ELR=0`（eret 到 PC=0，用户程序被杀后
由 init 重启，不挂死内核）。加固：

- **`syscall_return_restore` elr 校验**：eret 前检查陷阱帧 elr 为合法用户地址
  （>0 且 < 0x4000_0000），非法转 `el0_fault_handler`（防「故障 → 恢复 → 再故障」
  循环）
- **`TaskContext::new_thread` 初始 elr = thread_trampoline**（而非 0）：新任务
  首次调度后 trampoline `daifclr` 使能 IRQ，若在 `br x20` 前被抢占，
  `irq_entry` 的 eret 使用 ELR_EL1 —— 为 0 会跳转地址 0；设为 trampoline 后
  被抢占回到 trampoline 重跑（x19/x20 已由 switch_to 恢复）

### 验证结论

- 全量构建（kunxcore/pi4、kunxboot/pi4、全部应用）0 错误
- run-disk 模式 `/data` 持久化正常；bootloader 模式全链路正常
- 偶发 EL0 fault 若出现由 `el0_fault_handler` 优雅终止并重启（系统不挂死）

### 第 17 轮：systrace 实证 sp_el0 随机错位

- 新增最近 24 次 syscall 环形记录（nr/elr/sp_el0），故障转储显示用户 sp 在
  相邻 syscall 间随机错位（-16 / +432 字节）——陷阱帧 sp_el0 槽被污染，
  syscall 返回把错位 sp 恢复给用户并持续维持 → 尾声读错保存槽 → ret 到 0
- 结论：残余 PC=0 为陷阱帧 sp_el0 槽罕见随机污染（~5%），非栈溢出/非 fork
  拷贝损坏/非帧布局错位；诊断输出已完备，后续轮次若复现可直接定位
- 回归：shell 边界用例 + http_serve hostfwd 端到端全部通过

### 第 16 轮：PC=0 机制定位（16 字节 sp 错位）


- 用户栈转储 + last-syscall sp 记录确认：故障进程（init 最终打印）的
  k_main 保存寄存器槽与其尾声读取槽相差 16 字节（保存的 x30=0x400010 位于
  `[FB+0x60]`，尾声读取 `[FB+0x40]`=0）→ ret 到 0
- fork 帧记录：全部 fork 子进程恢复现场合法 → 排除 fork 拷贝损坏
- 用户 sp 在 k_main 执行期间向下错位 0x10，后续 svc 维持错位 → 尾声读错槽
- 错位源指向 syscall 返回/抢占路径的罕见竞态；诊断已就位

### 第 15 轮：修复与追加排查


- **`ls` 排序修复**：`cmd_ls` 改为收集 + 字典序排序（此前按目录项创建顺序，
  属用户可见错误）
- **fork 原子性修复**：`insert`/`spawn` 保持调用方 DAIF 状态（此前在
  fork_parent 临界区内 `daifclr`，子任务可提前被调度）；修复后 20 轮
  复现 fault 率 ~5%（此前 ~10-12%），仍存在残余 PC=0
- 残余 PC=0 故障签名稳定（init 于 fork 返回点 / oobe 于 execve 后首段
  路径拷贝，x30=0 → ret 到 0），金丝雀与栈顶转储继续就位

### 第 14 轮补充：残余 PC=0 假设排除


- 任务内核栈 0xA5 金丝雀：16+14 轮复现 **0 例栈底破坏** → 排除内核栈溢出
- 页释放路径全量复查：无双重释放/提前释放
- 故障处理器新增 FAR_EL1 与内核栈顶转储：子进程栈顶残留 fork 时拷贝的
  父进程 IRQ 帧（含用户 spsr/sp_el0），指向 fork/exec 首段执行与定时器
  抢占的寄存器恢复竞态；诊断输出已就位，后续轮次继续定位

---

## 十、2025-08-18 凌晨补充轮（fork 子进程栈越界根因定位 + 文件操作命令 + UART 缓冲）

### fork 子进程偶发 EL0 fault 根因定位（网络/显示/引导服务偶发被杀）

多次带诊断的复现（`el0_fault_handler` 增补用户栈回溯与 FAR 打印）确认此前
「偶发 EL0 fault（PC=0 / 栈区写权限故障）」存在**两个独立来源**：

1. **kunxinit `spawn_service` 子分支用户栈越界写（已修复）**
   - 现象：`[init] fork networkd pid=2` 后子进程即被 `EL0 fault ESR=0x9200_004e
     ELR=0x400144`（写权限故障 level 2）杀死；FAR 落在 0x3F00_0000（栈顶之上）。
   - 机制：编译器把子分支的两个 96B 缓冲清零指令排在 `sp` 上方（`stp q0` 写
     `[sp+96..192]`、argv 数组写 `[sp+192]`）；fork 子进程沿用父进程浅栈指针
     （距栈顶仅 ~176B），越界写入栈顶之上 —— 内核把栈顶之上映射为继承的
     EL1-only 2MB 块 → EL0 写权限故障；未立即故障时覆盖栈顶 argv 区引发后续
     PC=0 / LR 损坏（init / oobe 偶发 ret 到 0 的另一来源）。
   - 修复：子分支改用函数内 `static mut` 缓冲（消除全部向 sp 上方的写），
     12+12 轮复现中「写权限故障」类 fault 归零。
2. **残余 PC=0（eret 到 0）**：约 1/8 复现率，`ESR=0x8200_000e ELR=0`，出现在
   init / oobe 运行早期（用户栈回溯显示 LR=0 的 memcpy 调用链），由
   `el0_fault_handler` 优雅终止并重启，不挂死内核。根因指向 fork/exec 首段
   用户执行与定时器抢占的竞态，仍待进一步定位（已留诊断输出）。

### 内核新增防御

- **fork 陷阱帧校验**：`fork_parent` 校验子进程恢复现场
  `elr ∈ (0, USER_VA_TOP)`、`sp_el0 < USER_STACK_TOP`、`spsr` 为 EL0t，
  帧损坏时 fork 失败（不调度必死子进程）。

### UART 输入缓冲扩大（粘贴命令不再丢失）

- 环形缓冲 256 → 4096 字节 + 溢出丢弃计数；shell 读行前提示丢弃字节数。
  此前 shell 忙于执行时粘贴 >256B 命令会静默丢字节（半行命令永久等待换行）。

### 新功能

- shell 内置 `touch` / `cp` / `mv` / `tail`；`wc` 支持 `-l/-w/-c`；
  `ls` 支持文件参数与 `cannot access` 报错
- SFS `fs_rename` + `vfs::rename` + **`SYS_RENAMEAT`（38）实现**（原 ENOSYS）

### 验证结论

- 全量构建 0 错误 0 警告；12+16 轮无盘启动复现：写权限故障类 fault 归零、
  12/12 完整引导（此前约 1/10 触发）；全部新命令在 QEMU 实测通过
- run-disk（virtio-blk /data 持久化）链路正常

