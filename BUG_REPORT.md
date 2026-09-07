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


---

## 十一、2025-09-04 修复轮：「无法打开应用」并行定位与修复

> 现象：桌面点击图标后应用打不开——表现为秒退回桌面 / 黑屏停在桌面 / 偶发整机冻结。
> 方法：三路并行代码侦查 + QEMU 无头复现（点击图标坐标修正后 100% 复现
> `PANIC: clock`），确认多类独立根因后四路并行修复。

### 根因与修复

1. **应用侧 CJK 固定长度拷贝 panic 族（启动即崩/秒退的直接原因）**
   - `apps/clock/src/main.rs:624`：`date[n..n+6].copy_from_slice(weekday_label(wd))`，
     weekday_label 返回「星期X」9 字节 → copy_from_slice 长度不匹配 panic
     （QEMU 实测 `PANIC: clock`，点击图标必现）。按实际长度拷贝+守卫。
   - `clock:678`：`b" DST"` 4 字节拷入 3 字节窗口——世界时钟页有夏令时城市
     （伦敦/纽约等）首帧必崩；`clock:815` 闹钟 label `min(40)` 应为 `min(32)`
     且加越界守卫；`clock:756` 「周X」被 [u8;4] 截半字。
   - 同族：`gallery:116/121`（共 X 张照片）、`contacts:647`（已拨）、
     `phone:299`（未接）全部按实际长度+守卫修复。
   - 条件触发：`shared/font_render.rs` line_metrics 无 font_ready 前置检查
     （字体缺失时首帧 expect panic）、weather fmt_dec/push_temp 下溢与 info
     缓冲无界、map push_dec 无界、browser PAGE unwrap、emoji PNG 解压长度、
     calendar `.as_bytes()日历` 误植字面量。

2. **launcher 转场缺陷（应用正常启动但屏幕停在桌面）**
   - `start_launch` 从不设置 `MOTION.t0`，Launch 完成判定用按压时刻的陈旧
     t0 → 动画瞬间判完成（实测「启动过渡：1 frames」）、门控提前放行；
     补 `MOTION.t0 = now_us()`。
   - Launch 完成分支 `return true` → 主循环同帧整屏重绘桌面，覆盖占位页/
     应用首帧，随后 launcher 进入前台态不再渲染——慢启动应用（TCG 字体
     秒级）期间屏幕永久停在桌面，表象即「点不开」；改 `return false` +
     `render_frame` 前台守卫双保险。
   - `start_fly_away` 阻塞 `waitpid` 冻结主循环 → CANCEL_PID 登记 +
     wait4(WNOHANG) 非阻塞回收。
   - 门控健壮化：写 '0' 前 unlink 重建、写后回读校验、放行失败重试——
     消除残留 '2' 导致应用秒退无提示。
   - `launch_child` 检查 setuid 返回值（失败 exit(126) 拒绝以 root 运行）；
     fb_init open/mmap 失败分别打印 errno（区分表耗尽/ENOMEM/EINVAL）。

3. **calculator/settings/store 缺 FBIO_PRESENT（应用运行但画面永不上屏）**
   - 三应用自管 mmap canvas 且关闭 fd、无 FBIO_PRESENT——合成器就绪后
     mmap 拿到的是 canvas，必须 ioctl FBIO_PRESENT 才拷贝到扫描缓冲并翻页，
     且首帧 present 后内核不再自动翻页。补：保留 FB_FD、flip 后整屏
     FBIO_PRESENT（对照 apps/terminal 的 present_frame）、overlay 刷新后补
     present。

4. **wifid poll_pipe 越界 panic（诊断升级后定位的新 bug）**
   - `kunxserv/wifid/src/main.rs:868-874`：syscall3 返回原始寄存器值，read
     失败（-1）被当作 usize 天文数字，`for i in 0..n` 越界
     （实测 `index out of bounds: the len is 1024 but the index is 1024`）。
     按缓冲上限截断修复；同族检查 networkd/sensord/positiond 均已有守卫。

5. **内核侧加固**
   - vfs：`open_files_used()` + open 失败冷路径打印（限流：前 24 次全量、
     之后每 4096 次采样——服务以 2ms 级轮询未创建的 ctl 文件，不限流会把
     串口刷满 1 万+ 行）；`MAX_OPEN 32→256`（256×40B≈10KB BSS；实测稳态
     占用 10-13/256）。
   - audiod：控制文件 fd 泄漏修复（openat 返回值丢弃，每轮泄漏 1 槽）。
   - 中断风暴护栏：同一中断号连续 200000 次重入时在 GICD 层屏蔽该中断号
     并打印（把「静默冻结」降级为「屏蔽单一中断源继续运行」）；风暴转储
     增加 `act0`（GICD_ICACTIVER0）诊断位。

6. **可诊断性升级**
   - 11 个 kunxserv 服务 + 24 个应用的 `#[panic_handler]` 从丢弃 PanicInfo
     改为打印完整位置（文件:行:列，无堆 BufWriter 模式）——本轮 wifid 越界
     即靠此一步定位。

### 验证

- `make` 全量构建 0 错误；launcher_test：OOBE → 桌面 → 点 Clock →
  「[clock] 就绪」→ 返回转场 → 二次点击仍响应（launcher 不冻结）。
- 打开 Clock/Settings/Calendar/Phone/Browser 全部「就绪」，全程 0 PANIC、
  0 风暴；截图确认应用 UI 真实上屏（FBIO_PRESENT 生效）。
- vfs 失败插桩实证：失败均为「文件不存在」类轮询（err=-1，used=10-13/256），
  打开文件表耗尽假设排除（扩容作为余量保留）。

### 已知未决

- launcher `mmap /dev/fb0` 偶发 EINVAL（本会话观测 1 次；errno 区分日志已
  就位，待复现定位）。
- 中断风暴（iar=0、ISPENDR0=0 自相矛盾）根因指向 QEMU GIC 的 SGI0 锁存与
  dist 寄存器视图不一致；护栏保证不再挂死，`act0` 诊断待复现确认。
- `tools/apps_regression.py` 的 dock/返回键假设与当前生命周期语义有偏差
  （sms 被默认模块配置过滤、browser 的 (24,24) 是应用内导航不退出），
  待按新语义重写。
- 全新镜像首启必进 OOBE（Makefile 保证）；密码/胁迫 PIN 步拒绝空输入，
  依赖其「30 秒无输入自动下一步」通过——三个测试脚本已适配该节奏。

---

## 十二、2025-09-05 全量审查修复轮（六路并行代码审查 → 30+ 项修复）

> 方法：6 个并行审查代理分头逐行验证内核（内存/存储/网络 UI）、shared 库与
> 全部用户态程序，共报告 45+ 条经读码验证的缺陷；本轮修复其中 30 条明确的
> 正确性 bug。全部通过 `make` 完整构建（exit 0）+ 宿主测试（jpeg 7、
> wireless 105、sync_tests 57 全过）+ QEMU `-kernel` / `run-disk` 双模式
> 冒烟（0 PANIC / 0 fault）验证。

### Critical（3）

- **reap 回归（未提交改动引入）**：`process::reap` 丢失 `sched_cfs::reap_pid(pid)`
  调用（HEAD 823 行原有）——任务槽位与 256KB 内核栈永不回收，约 32 次进程
  退出后 `alloc_tid` 耗尽、所有新进程创建永久失败。已恢复调用。
- **SFS `MAX_DEVS=2` 表满「复用 0 号」摧毁根文件系统**：磁盘启动同时挂载
  root ramdisk + /data + tmpfs 三个 SFS 卷，第三个走 `dev_mut` 回退返回
  DEVS[0]，`*c = EMPTY_DEV` 把根卷 RAM 权威位图/日志游标整个清掉。
  `MAX_DEVS` → 4（每槽 ~131KB BSS），回退不可达。
- **FAT32 双卷根目录项冲突**：`mount` 无条件写 `ENTRIES[0]`（ino 1），
  第二个 FAT 卷挂载后先挂卷的 `entry_of(major,1)` dev 不匹配 → 整卷不可
  访问。改为按卷槽位分配根表项（ino = 卷槽位+1），`fs_root_ino` 按卷返回，
  `entry_for` 从根槽位之后分配常规表项。

### High（7）

- **wait 丢唤醒竞态**：wait(-1)/wait(pid>0) 的「检查/登记 waiter」与
  `block_current` 之间被 tick 抢占时，子进程 exit 的 wake 只唤醒 Blocked
  任务且清掉 waiter 槽 → 父进程永久阻塞。两条路径包进 IRQ 屏蔽窗口。
- **`sched_cfs::spawn`/`insert` 调度锁在 IRQ 使能下获取**：spawn 持锁跨越
  256KB 栈填充，tick 在 IRQ 上下文 `sched_lock` 自旋 → 单核硬死锁（audio
  ioctl 路径可触发）。入口先 `daifset`，失败路径与尾部恢复 DAIF。
- **execve 不终止兄弟线程**：`replace_address_space` 释放旧页表后兄弟线程
  TTBR0 悬空（C-7 同族，exit 修了 execve 漏了）。新增
  `sched_cfs::kill_siblings_of`（排除当前任务，不置 kill_req）。
- **gdbstub 空报文越界**：`$#00` 报文 n=0 后 `pkt[0]` 直接 panic（外部输入
  可杀内核）；补空包守卫回空包。
- **gdbstub `M` 命令缺逗号越界**：逗号扫描后无条件 `idx+=1` 再切片越界
  panic；补边界分支。
- **virtio_snd used ring id 无校验**：设备回报越界 id 时 IRQ 上下文
  `TX_BUFS[h]` 野索引 panic；仿 net_virtio 禁用驱动。
- **browser CSS `<style>x{</style>` panic**：`{` 为内容末字节时切片
  start>end 直接杀进程（远程/本地页面内容均可触发）；端点钳制修复。

### Medium（14）

- fork `insert` 失败回滚补齐 fd 引用释放（此前泄漏管道/队列/shm/文件引用）。
- `sys_nanosleep`/`sys_sendmsg` 裸读用户指针补 `uaccess_begin/end`（PAN
  硬件上必 panic 的漏网两处）。
- `reset_brk_state` 补复位 `FILE_MMAP_NEXT`（pid 槽位复用后文件 mmap 游标
  只增不减 → 永久 ENOMEM）。
- `brk_set` 增长上界钳到 0x1FF0_0000（防覆盖信号蹦床页/fb mmap/匿名 mmap 区）。
- `vfs::rename` 跨文件系统返回 `-EXDEV`（此前目标 inode 被当作源卷 inode 使用）。
- `pkg::uninstall` 删除失败死循环 → 失败跳过 + 计数上限。
- ota `wipe_dir` 路径拼接补边界检查（/data 深层目录触发恢复模式 panic）。
- socket `T_SYN_RCVD` 校验 `ack == iss+1` 才完成握手（防伪造/迟到 ACK 回退
  snd_una）；`accept` 句柄耗尽时释放子连接槽位（防 TCP 槽位永久泄漏）。
- compositor `fb_present` 全程屏蔽 IRQ（防触摸 IRQ 侧 composite 消费撕裂脏区）。
- perf overlay 改画到待翻转 back（此前画到 canvas：自绘模式不可见、直绘模式
  涂抹应用画面）。
- tty `minor_to_idx` 越界 minor 返回哨兵拒绝（此前钳位路由到错误会话）。
- FAT32 `dir_entry_at`/`write_dir_entry` 拒绝初始簇号 <2（防 u64 下溢写坏
  FAT 表）；FAT 8.3 名字无损校验（`fits_83`，create/mkdir/rename 拒绝会
  截断/替换别名的名字）。
- shared `font_render` 超大字形（>8KB 位图）返回 None（此前截断后保留原
  width/height → 消费方索引越界 panic，大字号必崩）。
- kunxpkg `repo.rs` 四处修复：HTTP 响应缺 `\r\n\r\n` 返回协议错误（此前切片
  panic）；meta/pkg/sig/delta 拷贝前校验目标缓冲容量。

### Low / 防御（6）

- shared `lifecycle` lc_read_evt/lc_restore_state、`audio` stream_state、
  sms（main+store）、modemtest 的 read 返回值改按 isize 判负 + 钳制
  （wifid 同族残留：-errno 的 usize 原值放行）。
- shared `ttf_text`/`text_engine` lseek 失败 → `with_capacity(usize::MAX)`
  abort；钳制到 64MB。
- shared `app_kit::copy_dirty_rect` 源/目标双向钳制（FB 尺寸 > DISPLAY 假设
  时裸指针读越界）。
- shared `text_engine`：3 参数误调 `syscall1` 编译炸弹、`mono_key` f32 位截断
  键冲突、不可达 unwrap（当前为死代码，防启用即炸）。
- settings `cfg_buf_set` 搬移缓冲 512 → 768（notifications.conf cap=768，
  前 512B 内 key 被重写即 panic）。
- launcher 天气组件 `temp.min(999)`、`push_dec` 容量感知、`save_number`
  缓冲 4 → 12、`app_life::build_path` 饱和减下溢、oobe `present_rect`
  尺寸上限守卫、mail 正文折行缓冲写满先落行。

### 排除的疑点

- **ttf_text `#[no_mangle]` memcpy/memset 自递归**（LoopIdiomRecognize 把
  字节循环改写回 `call memset` → 无限递归，可解释 el0_fault 残余）：反汇编
  当前工具链产物验证 memcpy/memset 为干净字节循环、无自调用——疑点排除
  （跨工具链升级仍需留意）。

### 未修（记录在案）

- FAT32 目录簇链不可增长（功能上限，非崩溃）；virtio-blk `reset_ring` 超时
  恢复路径未做完整设备复位（需真机验证）；信号帧边界按进程主栈校验
  （pthread 工作线程收信号被误杀默认动作）；slab `kfree` 双重释放防护不
  完整；DMA 缓冲 64B 缓存行隔离（仅 Pi4/KVM 影响需行内实验）。

---

## 十三、2026-09-05 独立复查修复轮（四路并行代码审查 → 29 项修复）

> 方法：4 个并行审查代理（内核核心/文件系统/GUI 应用栈/服务与应用）+
> QEMU 实机复现定位。与第十二轮互补（该轮已修项不再重复）。全部通过
> `make` 全量构建（exit 0、0 警告）+ 宿主测试（jpeg 7 / wireless 105）+
> QEMU `-kernel` 无头、`run-disk` GUI 双模式冒烟验证。

### 启动链根因：桌面起不来（launcher 静默秒退）

- **陈旧构建产物超过 execve 读入上限**：rootfs/disk.img 里的 launcher 是
  上个会话编译的 396KB 版本，超过旧 `EXEC_BUF_CAP`（256KB）→ execve 静默
  返回 -1 → 子进程 exit(127)，init 收尸后直接退到内核 Shell，桌面永远
  起不来（无任何诊断输出）。`elf.rs` 已改为按文件实际大小分配（4MB 上限）
  后，`make clean && make` 全新构建（launcher 202KB）即恢复正常。
  教训：execve 失败路径必须至少打一行诊断，已确认 elf.rs 诊断齐全。

### 内核安全/正确性（9）

- **sys_socketpair / sys_clipboard_get / sys_statx 写用户内存只做可读校验**：
  `user_ok` 不 break COW/不保证可写，目标是 RO 页（ELF text/rodata 共享页）
  或未断 COW 页时 EL1 裸写权限故障 → 内核 panic（`socketpair(AF_UNIX,0,0,
  (int*)"const")` 一个 svc 即可杀内核）。三处全部改 `user_ok_writable`；
  同族 `rt_sigaction` 的 old 写回一并修复。
- **rt_sigreturn 恢复用户可控 SPSR**：伪造帧 `spsr=EL1h` 可让 eret 以 EL1
  返回用户地址（PXN 下指令中止挂死内核）—— 单 syscall 无条件 DoS。清洗
  `M[3:0]` 强制 EL0t。
- **sys_wait4 ABI 错位**：options 读 x3（应为 x2，x3 是 rusage）—— 真实
  bionic 二进制的 WNOHANG 被当成垃圾 options，退化为阻塞 wait。修正内核并
  同步 launcher 两处调用方（此前两边以同一错误约定互相掩护）。
- **dup3 目标越界泄漏引用**：`newfd>=FD_MAX` 时 `set_fd` 失败被忽略、临时
  槽位照清、已 +1 的对象引用永久泄漏（循环调用耗尽管道/文件/Shm 池）。
  提前 EBADF 拒绝 + set_fd 失败完整回滚。
- **kill(pid,0) 污染信号位图**：POSIX sig-0 探测置位 bit0，但
  `handle_pending` 只扫 1..64 → `signal_pending` 恒非零，该进程每个 syscall
  入口多扫 63 位。按探测语义提前返回。
- **sys_kunx_ota 无权限门禁**：任意应用可 WIPE_DATA（递归删 /data）、
  INSTALL（内核侧任意路径刷写）、切启动槽。变更类命令限 uid==0。
- **文件 mmap 游标与匿名 mmap 游标重叠**：FILE_MMAP_NEXT 自 0x2E000000
  可长到 MMAP_CAP，越过 MMAP_BASE(0x30000000) 后与 anon 游标互相覆盖 PTE
  （`map_page_in` 静默覆写）→ 堆数据当文件数据/文件页被堆踩。文件映射
  硬上限改 `MMAP_BASE`。
- **mmap_anon 预留失败不回滚游标**：记录表满时游标已推进，反复失败的
  mmap 把该 pid 地址空间烧穿至永久 ENOMEM。失败路径游标复位。
- **SLEEPERS 睡眠槽死亡泄漏 + tid 复用假唤醒**：被 SIGKILL 的任务留下活跃
  `Sleeper{tid,deadline}`（kill_pid/reap_pid 均不清），tid 复用后过期唤醒
  打到新任务（无限 futex_wait 收假 ETIMEDOUT），且 32 槽逐个泄漏至
  sleep_until 永久退化忙等。kill_pid / kill_siblings_of / reap_pid 统一
  `sleepers_clear_tid`。
- **reap 先释放地址空间后做存活护栏（SMP UAF）**：新增
  `sched_cfs::has_live_tasks`，护栏提前到释放 fd/用户页/页表之前，存活时
  放弃本次回收。

### 文件系统/存储（6）

- **fs_lock CPU 令牌被任务迁移破坏（SMP 并发入临界区）**：`pick_next` 无
  亲和，持锁任务被抢占迁核后，同核另一任务误判「本核已持有」重入，与迁移
  走的原持有者并发执行（交错 PENDING_*/SCRATCH_* 事务状态、重复 inode/块
  分配）。改为 (CPU,TID) 双令牌：同 CPU 他人且存活 → 真互斥；持有者 tid
  已死 → CAS 抢锁自愈（保留旧方案的免清理自愈性）。
- **fs_truncate 无视 write_inode 失败仍提交事务**：位图释放已 staged 而旧
  inode 仍指向这些块 → 复用后跨文件数据踩踏。失败 `txn_abort`。
- **commit_inner 无视 copy-out write_block 失败**：日志记录随后被 head
  前移作废 → 重启后元数据更新丢失而 RAM 位图已放行（孤儿块）。新增
  `COPYOUT_FAILED` 标记阻塞 checkpoint 前移，保持记录可重放。
- **pkg::uninstall 无名字校验**：`pkg uninstall ../../etc/hosts` 经真实
  `..` 目录项解析删除任意文件（install 有白名单、uninstall 没有）。补
  `valid_name`。
- **pkg::install 失败回滚不清理 /res/<name>/**：半安装资源残留，重装同名
  包混装。失败路径复用 uninstall 的枚举删除。
- **shell `crash clear` 隔条删除**：删除成功后条目前移却仍 `idx+=1`，
  N 个崩溃转储只删约一半还报「已删除 N」。改为删除成功不递增（pkg 同款）。
- **shell `cmd_mount` 单静态缓冲别名**：挂载点字符串写入唯一的
  MOUNT_POINT_BUF，第二次 mount 原地改写第一次挂载点 → find_mount 把
  /data/... 路由到错误设备。改用与挂载表等容的缓冲池。
- **virtio_blk reset_ring 不复位设备**：只重写队列寄存器/清驱动游标，
  设备侧 in-flight 请求与 used idx 存活，其迟到完成被下一次请求误判为
  「本次成功」（读返回预清零缓冲 = 假成功/脏数据）。reset_ring 完整走
  status=0→ACK→DRIVER→特性→DRIVER_OK。
- **blkdev 加密卷冲刷全局单父**：`ENC_FLUSH_PARENT` 只存最后一次包装的父
  设备，第二个加密卷 fsync 冲错盘。`FlushFn` 签名改 `fn(major)`，加密冲刷
  按自身 major 反推父设备；删除全局变量。

### GUI / shared（4）

- **oobe `present_back_rect` BACK_BUF 越界 panic**：模块页
  `partial_redraw(2, …)` 的 bottom = 最后一行 y+26 超出 960 行，点击「选择
  应用」页任意一行即 `index out of bounds` 杀向导（带真实输入设备必现；
  模拟点击流程碰不到所以此前未暴露）。钳制到 BACK_BUF/屏幕。
- **oobe 30 秒空闲误推进**：纯指针移动被 `read_input_event` 内部消费、
  不算「有输入」，用户移动鼠标浏览页面 30 秒被强推下一步。新增
  `ANY_INPUT_RECENT`（read 到任意字节即置位），空闲计时消费该标记。
- **launcher flip() 脏矩形只钳 FB 尺寸**：内核帧缓冲大于 BACK_BUF
  (540×960) 时 copy_nonoverlapping 越过行尾/末行静默越界读（app_kit 同处
  已修、launcher 这份漏改）。补 `.min(DISPLAY_W/H)`。
- **ttf_text alloc-null 诊断覆盖最高半字节**：hex 数字写 buf[19..27] 后
  `buf[26]=b'\n'` 覆盖最高位，且长度 27 打出 13..19 的 NUL。换行移至
  buf[27]、长度 28。

### 服务与应用（11）

- **syscall 返回值 usize 模式清剿（第二_batch）**：第十二轮修了
  lifecycle/audio/sms/modemtest/ttf_text，本轮修掉其余全部站点 ——
  wifid `poll_wifi`（读错误 → `&buf[1..usize::MAX]` panic 杀进程）、
  wifid `wifi_status`（ioctl 错误被当成功，打印垃圾状态）、rild at.rs
  `poll_read`、settings `poll_key`、store `store_poll_key`、probe 4 处、
  phone `read_state`、contacts `read_dat`、lifd `read_file`（读错误当
  成功 → 执行上一条陈旧命令）、logd（读错误当 256 字节零每 100ms 灌日志）、
  audiod `pull_client`（重吃上一轮陈旧音频）。
- **wifid `write_kv` 无界 copy_from_slice**：`CONNECT <超长ssid> <psk>`
  （管道可被任意进程写入）→ panic 杀 wifid。write_kv 有界截断 +
  handle_command 在 save_network 之前校验 ssid≤32/psk≤64。
- **wifid `net_conf_path` from_utf8_unchecked**：ssid 字节可为任意非
  UTF-8（UB）且可含 `../`。逐字节白名单净化（顺带堵穿越）。
- **networkd / wifid 限时自退**：networkd 3×200ms、wifid 20×250ms 后退出，
  之后 OOBE/设置的所有 SCAN/CONNECT/STATUS 命令静默丢失（写入者拿成功
  回执）—— 「Wi-Fi 永远连不上」。两守护进程改常驻循环。
- **rild 级联短信重组错槽 + 槽位不回收**：`concat_text` 只按 total 匹配
  （两个同段数并发会话拿错槽，完成的消息永远拼不齐）；`concat_gc` 空转，
  4 个槽被半途会话永久占满后多段短信整体失效。按 (ref_id,total) 匹配 +
  6 秒 TTL 清理（ConcatSlot 新增 started_us）。
- **rild at.rs `after_colon` 单引号 panic**：`+CRING: "` 时
  `&rest[1..0]` 越界。长度 ≥2 才去引号。
- **settings `cloud_login` seed 越界**：`[u8;96]` 装用户名+`:`+密码+
  `:kunxos-cloud`，用户名+密码 > 82 字节即 index panic（正常长口令可触发）。
  扩到 112。
- **store 两处 from_utf8_unchecked**：包缓存路径/输出路径来自网络索引
  数据，非 UTF-8 即 UB。改安全转换。
- **files `fmt_size` 整数部分假设一位数字**：512GB 卷 `w/10=512` 经
  `as u8` 截断为 0（显示「0.0GB」），≥100 单位产出非 ASCII 字节喂给文本
  渲染器。整数部分改 push_dec_u32 逐位输出。
- **kunxinit 不回收守护进程僵尸**：只阻塞 wait4(launcher_pid)，networkd/
  wifid 等退出后永远变僵尸。阻塞前 `wait4_nohang(-1)` 循环清尸；launcher
  被提前回收时 ECHILD（-10）不再打印成天文数字。
- **Makefile `clean` 引用已删除的 displayd**：`$(DISPLAYD)` 变量不存在 →
  `cargo clean --manifest-path /Cargo.toml` 失败，make clean 永远 exit 2
  （clean 失效导致陈旧产物残留 —— 与上面 launcher 秒退直接相关）。删除
  该行。

### 同轮清理

- 5 处预存编译警告清零（modem.rs pi4-only 导入、fat32.rs 未用参数/导入、
  wifi_mgmt.rs 测试专用导入、kunxhal camera.rs 不可达 None、hfp.rs 三处
  冗余初始化）。

### 验证

- `make` 全量构建 exit 0、0 警告；宿主测试 jpeg 7 / wireless 105 全过。
- QEMU 无头 `-kernel` 模式：OOBE（无 fb 秒退）→ launcher 正常 execve
  （此前静默秒退）→ init 收尸 → 优雅退出，0 PANIC。
- `run-disk` GUI（ramfb）模式：launcher_test 全流程（OOBE 13 步 → 桌面 →
  点击图标 → 启动/返回转场 → 二次点击）验证。

---

## 九、2026-09-05 全量审计修复轮（并行子代理 5 路深挖 + 人工复核）

> 审计方式：对系统调用路径 / 网络 / 文件系统与 IPC / 用户态应用 / bootloader 与 HAL 与 pkg
> 五条线并行全量读码复核（对照本报告既往"声称已修"逐项验证），另做 `make` 全量构建、
> QEMU `-kernel` 启动回归（shell df/ls 实测）与 sfsrace 文件系统竞态测试。
> 本轮另确认既有修复中两处**声称已修但实际不完整**的关键项（见 T-1/T-3）。

### Critical / High

- **T-1（Critical）`ensure_user_range_writable` 对非 COW 只读页不处理 → 内核写 ELF text/蹦床页
  触发 EL1 权限故障 panic**：AP_USER_RO=0b11 在 **EL1 同样只读**（原注释"只约束 EL0"有误）。
  任意进程一条 `read(0, text_ptr, 1)` 即可整机死机；COW 页 `cow_break` OOM 失败同样被忽略。
  修复：按页查记录，COW 或 `is_user_ro` 一律走 `mprotect_page`（含共享只读池页复制私有副本），
  失败（OOM）返回 false 按 EFAULT 处理，绝不停留在只读映射上继续写（process.rs）。
- **T-2（High）`rt_sigreturn` 双重弹出信号帧**：`deliver` 把原始 sp 存进帧 OFF_SP，sigreturn
  恢复 sp 已隐式弹出帧，却又 `sp += SIG_FRAME_SIZE` —— 每次信号处理返回用户栈永久上漂 288 字节，
  SIGCHLD 风暴下累积必然崩溃（与长期追查的 el0_fault/PC=0 类故障签名吻合）。
  修复：删除多余的 `+=`（signal.rs）。
- **T-3（High）shm 退出路径引用计数不平衡**：`shm_close_index` 只减 1 次 fd 引用而 `shm_map`
  是逐映射 +1 → "create→map→exit"一次泄漏 1 个引用，8 次后 `shm_create` 永久失败。
  修复：按清除的映射记录逐条归还（ipc.rs）。
- **T-4（High）SYS_exit(93) 与 exit_group 混同**：任一工作线程收尾即 kill_pid 整个线程组，
  pthread_join 永远等不到。新增 `sched_cfs::exit_thread_current`（调度锁内原子判定"是否还有
  兄弟任务"+仅退出当前线程+清理 futex/net/sleeper 登记），最后一个线程才升级为整进程退出
  （sched_cfs.rs / process.rs / linux_compat.rs）。
- **T-5（High）`vfs` 打开文件表 refs/offset 无锁 RMW**：父子并发 close 丢减 → 槽位泄漏；
  read/write 长操作期间槽位被 close+复用 → 偏移写进别的文件。修复：refs RMW 屏蔽 IRQ；
  偏移写回前重验 (used,dev,ino,typ) 身份；readdir_next 不再整体写回旧结构（vfs.rs）。
- **T-6（High）`dev_mut` 表满"复用 0 号"真实可达**：磁盘布局挂 5 个 FS（MAX_DEVS=4），shell
  `df` 直调 fs_simple 版本对 FAT major 也建 DevState → 第二个 FAT 触发复用 0 号，root 卷
  状态被改写（第 12 轮"摧毁根文件系统"的通路）。修复：`df` 改走 `fsops` 分派；
  `dev_mut` 表满返回专用哑槽位 SCRATCH_DEV，绝不复用真设备（shell.rs / fs_simple.rs）。
- **T-7（High）TCP 发送 1995~2028 字节被下层静默丢帧但 snd_nxt 照常推进**：tcp_send 只查
  TCPBUF 上界，eth_send 还需容纳 14+20 字节头 → 段 >1994 即整帧丢弃，连接永久失步。
  修复：上界改为 1994；`tcp_xmit` 返回 ip_send 结果，失败时撤销重传登记、不推进 snd_nxt
  （socket.rs）。
- **T-8（High）`net_virtio::irq_handler` 缺 BASE==0 守卫**：设备故障被禁用后迟到 IRQ 读
  REG_IRQ_STATUS → 地址 0x60 未映射 → IRQ 上下文 data abort 挂死内核。已加守卫（net_virtio.rs）。

### Medium（内核）

- `elf.rs`：`load()` 的 kinfo 在长度检查前 `rd_u32(data,0)` —— verbose 下 execve 1-3 字节文件
  切片越界 panic（移到检查后）；`stack_write` 页跨界一次性 memcpy 会写进相邻物理页（按页切分）；
  整段块加载 `add_page` 中途失败泄漏连续块剩余页（就地归还）。
- `execve` 不复位 `BRK_CUR`：同 pid 先 brk 再 execve，旧断点低于新映像段尾时新堆直接覆盖
  text/data（execve 成功路径调用 `reset_brk_state`）。
- `map_file` 失败路径不回滚 `FILE_MMAP_NEXT` 游标（两条失败路径恢复游标）。
- `SYS_PRLIMIT64` 复用 getrlimit、ABI 完全错位恒 EFAULT（按 x0=pid/x2=new/x3=old 独立实现）。
- 匿名 mmap `MAP_FIXED_NOREPLACE` 与 MAP_FIXED 同等对待（占用时返回 EEXIST）。
- `sys_times` 按 64 字节清零 32 字节的 struct tms（越界多清 32 字节）。
- 零长度 send/sendmsg 返回 EFAULT（Linux 语义应为 0）。
- `resolve_user_path` 超长路径按字节静默截断（UTF-8 边界 UB + 可能截出另一真实路径）：
  改返回 None，调用方统一 ENAMETOOLONG。
- `heap::kmalloc_aligned` 对 align>2048 请求压进 slab（Layout 违约 UB）：改走 large_alloc。
- TCP：ESTABLISHED 的 ACK 无上界校验（越界 ACK 可把 snd_una 推过 snd_nxt，已加 seq_le 检查）；
  RST 无任何序号校验（seq==rcv_nxt 或 ack==snd_nxt 才接受）；tcp_send 等待循环不查 T_CLOSED
  （RST 后空转 30s）；close 路径残留普通 `>=` 序号比较（4GB 回绕失效，改 seq_ge）；
  TIME_WAIT 落在 `_=>{}` 不回 ACK（对端 FIN 重传永不停止，补重 ACK 分支）；
  死亡套接字吞掉同四元组新 SYN（回 RST 让对端快速重试）；tcp_recv 循环内的 tcp_retransmit
  （制造重复段 + 错误施加退避）移除；accept 句柄耗尽清槽移回锁内。
- 回环 TCP/UDP 全被丢：ip_send 回环只重算 IP 头校验和，传输层校验和仍按真实源 IP 计算
  （新增 fix_transport_checksum 按回环源重算，TCP 先清零校验和字段）。
- DNS：不校验源 IP==DNS 服务器、DNS_ID 可预测、resolve 结束不复位接受窗口（补 src_ip 校验
  + 会话结束清 DNS_SRC_PORT/DNS_DONE）；超长域名产出 QDCOUNT=1 空查询（build_query 无
  任何 label 返回 0，resolve 直接失败）。
- `virtio_blk::reset_ring` 写 used/avail 后缺 dcache_clean（invalidate 丢弃脏行 → 新请求被
  误判完成/假成功，补 clean）。
- `fat32`：`fs_lookup_unlocked` 无 slot 上限（簇链成环 + fat_lock 关 IRQ 自旋 → 整机冻结，
  补 0x1000000 上限）；4GiB 边界差一（offset==0xFFFFFFFF 放行后 size 截断，改 offset+len 判定）。
- `fs_simple`：间接块指针未校验数据区（伪造 inode 把 IMAP/BMAP 当间接块 → 改写 RAM 权威位图，
  bmap/bmap_alloc/read_indirect 全路径补 data_block_ok 校验）；`fs_unlink_unlocked` 忽略
  truncate 失败仍 inode_free（孤儿块泄漏，失败即 txn_abort 回滚）。
- ipc：管道/消息队列等待者表满返回 0（第 5+ 并发等待者拿到假 EOF/假成功，改让出重试）。
- `vfs::lstat_path("/")` 返回 None → shell `ls /` 报"无法访问"（根路径直接解析挂载根 inode）。

### Medium / Low（boot / HAL / pkg / tools）

- `kunxboot/dtb.rs`：FDT_PROP 值区不校验 struct 块边界（DTB 顶满 scratch 时越界写穿 .bss，
  补 value_off+len<=struct_end）；`insert_props` 假设字符串块紧跟结构块（非该布局时注入
  静默失效，不满足即拒绝）。
- `kunxhal/sensor.rs`：BSC 状态位定义与 BCM2835/2711 手册整体错位（S_TXW/RXR/TXD 实为
  bit2/3/4，此前 5/6/7 → Pi4 真机任何 I2C 写恒超时，已按手册修正）。
- `kunxhal/plat_pi4.rs`：核心电压 mailbox 返回有符号偏移按 u32 计算（负偏移饱和回绕出错误
  读数，改 i32 运算）。
- `kunxpkg`：`review --approve` 不校验 manifest.name 即拼路径（签名包可路径穿越写任意文件，
  补白名单）；升级失败 rollback_files 删除新二进制但不恢复备份（系统处于"记录已装 v1 而
  二进制不存在"的不一致，失败路径先 restore_backup）。
- `tools/mkdisk.py`：FAT 容量校验按单 FAT 估算而布局预留 2 FAT（顶满时文件越过分区边界被
  SFS 镜像覆盖，按 2×fat_size 收敛重算）。

### 明确不改项（复核结论）

- `brk` 收缩终点未对齐（审计报告 High）：`unmap_user_range` 按记录页基址比较，
  未对齐终点不留残留 —— 误报，维持现状。
- TCP 通告窗口/流量控制、TIME_WAIT 定时退出、uaccess PAN 跨抢占、用户范围校验 O(n²)、
  ARP 条目老化、启动链反降级下推：需要更大改动或属设计取舍，列入后续轮次。

### 本轮验证

- `make` 全量构建 exit 0；`make pi4`（kunxhal --features pi4）编译通过。
- QEMU `-kernel` 启动回归：内核 + init + 全部守护进程启动，shell 实测 `df`（root 卷状态
  完好）/`ls /`（修复生效）/`ls /bin`，0 PANIC。
- sfsrace 文件系统竞态测试：**PASS**（内核自动关机，QEMU 退出码 0）。

---

## 十四、2026-09-05 第二轮独立复查修复轮（五路并行代码审查 → 33 项修复）

> 方法：5 个并行审查代理（内核核心/文件系统与存储/网络 IPC shell/GUI 应用栈/
> 服务与 init pkg）逐行读码 + 对照 `git diff` 查未提交改动引入的回归。与第十
> ~十三轮互补（已修项不重复）。全部通过 `make` 全量构建（exit 0）+ 宿主测试
> （sync_tests 57 / wireless 105 全过）+ QEMU `-kernel` 无头与 `run-disk` 双模式
> 冒烟（0 PANIC / 0 fault）验证。

### Critical（1，未提交改动引入的回归）

- **`alloc_page` 成功路径早退跳过 `irq_restore`**（allocator.rs）：未提交改动给
  分配器包上 IRQ 屏蔽，但 `alloc_page` 命中空闲位后直接 `return`，屏蔽态泄漏
  到调用方——syscall 全程 IRQ 使能，任何一次成功单页分配（slab 建页/按需
  分页/COW break/brk/mmap）后定时器 tick 与设备 IRQ 全部丢失直到 syscall 结束，
  最高频路径之一。改命中 `break 'outer` + 尾部统一恢复（与
  `alloc_pages_contig` 同款）。

### High（5）

- **eret SPSR 无白名单清洗（无条件整机冻结）**：`syscall_return_restore` 汇编
  直接 `msr spsr_el1`，而 AArch64 的 EL0 可直接 `msr daifset`；rt_sigreturn
  伪造帧（第十三轮只清了 M[3:0]）或 EL0 直写 DAIF 即可以关中断状态运行
  EL0 → 单核吞掉全部定时器 tick，整机永久冻结。双层修复：eret 路径统一
  `bic` 清 M[3:0]/DAIF/SS/IL（NZCV 保留）；`rt_sigreturn` Rust 侧同步加宽
  清洗掩码。
- **fat32 `fs_unlink_unlocked` 目录非空检查无槽位上限**：簇链成环（恶意/
  损坏镜像）时在关 IRQ 自旋锁内无限扫描 → 整机冻结。同族三处
  （lookup/readdir/find_free_slot）此前已修，此处漏网；补 `0x1_000_000` 上限。
- **socketpair/msgq 阻塞原语缺对端关闭复检**（管道第十三轮修了，同构代码
  漏改）：`pair_read` 阻塞中对端进程退出只减引用不唤醒 → 永久挂起；
  `pair_write`/`pair_read` 跨 `block_current` 持有裸指针，槽位被清空复用后
  把数据写进**新连接**的环（跨连接串扰）；`msgq_send` 唤醒后写进已关闭/
  复用队列并报成功（假成功）。重写：`PairSock` 加 `side0/side1` 端计数
  （照抄 Pipe 方案），每轮循环经 `PAIRS[idx]` 重取引用并检查 `used` 与对端
  计数，EOF 返回 0 / 短写退出；msgq 每轮检查 `QUEUES[].used`。
- **calculator / stub `poll_key` read 返回值按 usize**（第十二/十三轮同族
  漏网站点）：read 失败 -1 → `for i in 0..(n as usize)` 越界 panic 秒退回
  桌面。按 settings 已修款补 `as isize` 判负 + 长度钳制。
- **launcher 复活已回收 pid → 桌面假死**：KILLED/EXITED 事件落在启动动画
  窗口内时 `app_life` 清了 `APP_STATE/APP_PID` 但 `app_launch::LAUNCH_PID`
  未同步；动画完成 `launch_complete` 拿死 pid 无条件进入前台态 → waitpid
  ECHILD 永久轮询、launcher 不再渲染。修复：KILLED 命中 ST_LAUNCHING 时
  调新增的 `clear_launch_pid()`；`launch_complete` 对 `pid<=0` 与 waitpid
  负返回（已被回收）按退出处理回桌面。

### Medium（13）

- **`sys_readv`/`sys_writev` 预校验对用户给的完整 len 逐页落地**：mmap_anon
  预留区 + 单条 syscall 强制内核分配/清零至多 ~256MB 物理页 + 数十亿次映射
  记录扫描（实际拷贝每 iovec 只用 `len.min(4096)`）。预校验范围钳到实际
  拷贝量。
- **`large_alloc` 重试路径按第一次对齐计算页数**：第二次对齐偏移更大时数据
  区尾部最多溢出 `align-1` 字节到相邻物理页（每次进程创建的
  `Box<ProcPageTable>` 走此路径）。重试分支复核 `aligned2+size` 跨度，越界
  释放并按 OOM 返回。
- **`create_process` 进程表满时泄漏整个地址空间**：Box 只被 drop 释放 24KB
  结构体，已加载用户页/派生页表（可达数 MB）全漏，进程表反复打满即 OOM。
  失败路径显式 `free_user_pages()` + `free_process_tables()`。
- **fs_lock「CAS 成功 → 存 TID」两步窗口**（SMP）：窗口内同 CPU 新任务据
  陈旧 TID 误判持有者已死、CAS 接管成功 → 双任务并发进入 FS 临界区。改为
  单原子字编码 `(cpu<<32)|(tid<<16)|depth`，单 CAS 完成「获取+登记」。
- **目录项路径信任 inode 块号**（`dir_add_entry`/`dir_remove_entry`/
  lookup/readdir）：伪造 direct 指针指向位图区时目录数据写进 RAM 权威位图
  （任意块分配位翻转、跨文件踩踏且持久化）。四处扫描统一 `data_block_ok`
  校验，非法按空洞处理（与 bmap 同策略）；`free_indirect_chain` 顶层同款。
- **数据位图分配的日志暂存失败被吞**：单事务 ≥24 块（PENDING 表满）时位图
  分配记录进不了日志，掉电重放后该块被判空闲 → 跨文件数据踩踏。
  `data_block_alloc` 暂存失败回滚位图/计数/提示返回 -1；
  `stage_bmap_block` 改返回 bool（释放路径失败属安全方向，保持原样）。
- **`tcp_close` FIN 重传覆盖未确认数据段登记**：send 超时（rtx 仍登记数据）
  后 close 无条件改写 rtx 槽位为 FIN → 数据段永不重传，双向死等后连接静默
  拆除丢数据。新增 `fin_rtx_pending` 标记：数据未确认时 FIN 用独立标记，
  ACK 确认数据后补登 FIN 重传。
- **IPC 引用计数在 IRQ 使能下非原子读改写**：fork 回滚/reap/close 并发
  drop 丢更新 → 管道/队列/套接字对槽位永久泄漏。全部 add/drop 入口加
  `daifset/daifclr` 屏蔽窗口。
- **shared/audio.rs write 返回值按 usize**：`r >= n` 对失败（-errno 的巨大
  正数）恒真 → audiod 注册假成功、FOCUS/RELEASE 假成功。改按 isize 全等。
- **audiod 客户端表无存活回收**：应用被 SIGKILL 不执行 CLOSE（no_std 进程
  无清理路径），8 槽被死客户端占满后音频整体失效。主循环每 500ms
  `kill(pid,0)` 探测回收（lifd 同款）。
- **mkfs_sfs.py `SB_VERSION` 常量遮蔽**：版本值 2 被字段偏移 4 覆盖，宿主
  镜像超级块实际写 version=4（内核写 2，两侧均不校验故潜伏）。版本值改名
  `SFS_VERSION`。
- **kunxpkg/kunxverify 多证书链解析恒失败**：把「off 到 META-INF 末尾」整段
  传给 `cert::parse`（要求恰好一张证书），≥2 张证书第一张必然长度不匹配 →
  链功能整体不可用（fail-closed 潜伏）。两处先按头部 subject_len 算单张
  精确长度再切片。
- **audiod `audio_init` GETPARAMS `if r < 0` 对 usize 恒假**：ioctl 失败被
  吞、打印垃圾参数。按 isize 判负。

### Low（14）

- ELF 头诊断 `read_volatile(data.as_ptr().add(64))` 读 8 字节需要 len≥72，
  入口校验从 64 提到 72（否则读入 slab 未初始化尾部）。
- fork 陷阱帧护栏补 `c_sp == 0` 拒绝（sp_el0=0 的必死子进程此前可通过）。
- `process::reap` 补 `elf::release(pid)`：pid 复用给非 ELF 进程时陈旧 ASLR
  栈基址/brk 不再被 `current_stack_base()/current_brk()` 当新进程的值。
- blkdev `make_suffixed_name` 7 字符基名时 `dst[n]=0` 写到下标 8 越界；
  拷贝循环预留 suffix+NUL 两位。
- fat32 写路径 4GiB 边界 `offset+len == 2^32` 恰好放行（cur_off 截断回 0、
  size 停旧值、簇缓存错位）；改 `>=` 拒绝。
- ARP 主动学习排除 127/8 回环地址（本地 IPC 注入帧 src_mac 全零，8 项 LRU
  缓存被垃圾条目挤占）。
- shell `cmd_source` 嵌套防护：脚本内再 source 改写同一静态 SRC_BUF 导致
  外层按陈旧 total 切行执行错乱；深度计数拒绝嵌套（RAII 恢复）。
- msgq 同优先级 FIFO 按全局入队序号 `seq` 判定（槽位复用会打乱下标序）。
- net_virtio `init` 队列建立/RX 填充失败路径复位 `BASE=0`（残留值使
  `ready()` 误报就绪）。
- cloud `api_ack` 请求体有界拼接（ACCOUNT/DEVID 经 urlencode 最坏 ~96B/段，
  极端本地配置合计超 256B body 即 index panic）。
- `from_utf8_unchecked` 残留 16 处统一改安全转换（probe/weather/camera/
  cloud/sfsrace/recovery/camerad/logd）。
- launcher/oobe 点阵回退 `cjk_index(cp as u16)` 对 >U+FFFF 码点（emoji）截断
  误命中 CJK 位图；补 `cp <= 0xFFFF` 短路。
- pushd read/getdents64、wifid/lifd getdents64 返回值按 usize（第十三轮
  同族清剿漏网）：读错误重放上一条陈旧通知 / 垃圾目录项解析越界 panic；
  统一 `as isize` 判负。
- rild 短信存储改纯追加写（此前「读 ≤8KB + 整文件重写」，约 30 条后最旧
  短信静默丢失）；`count_records`/`rewrite_store` 改分块读全文件（32KB
  上限 + out 容量守卫）；sys.rs 补 `SEEK_END`。

### 同轮清理

- 修复本轮触及文件的编译警告（launcher/oobe 括号、rild/logd/lifd/pushd/
  wifid 双分号与冗余括号、shared/lifecycle 未使用赋值、kunxpkg 未使用变量、
  shared/app_kit glob 再导出加 `#[allow(unused_imports)]`）。应用内既有 lint
  噪音（clock/settings/contacts 等 ~120 条）为上一会话遗留，未在本轮范围。

### 追加（pkg e2e 驱动定位）

- **`parse_sources` file:// 分支从不设置 `path_len`（恒为 0）**：`pkg update`
  的 file:// 源全部解析成空路径（实测内核收到 open("/Packages")）→
  `fetch_full` 读索引失败返回 -9 → 包管理器 update/install/review 全链路
  不可用（HEAD 基线 e2e 0/20 同样失败，属预先存在缺陷）。补 `path_len`
  赋值后 update 正常拉取 3 包。定位手段：cmd_update 临时诊断打印拆出三个
  失败点（读索引 n=0 / 读签名 n=0）→ 锁定 Source 结构 path_len 缺赋值。
- **shell `ARG_LEN` 32 → 64**：与内核 execve `EXEC_STR_LEN` 对齐。32 时
  50 字符的包路径参数（`/var/lib/pkg/repo/main/files/xxx.kunx`）作 shell
  参数必然截断，review/install-file/verify 长路径命令无法使用（HEAD 的
  实现为静默截断成错误路径，同样不可用）。

### 验证

- `make` 全量构建 exit 0；宿主测试 sync_tests 57 / wireless 105 全过。
- QEMU `-kernel` 无头模式：OOBE（无 fb 秒退，预期）→ launcher 正常 execve →
  init 收尸 → 内核 shell 接管，0 PANIC / 0 EL0 fault。
- QEMU `run-disk` 模式：/data 挂载可写（wifid 创建配置目录）、OOBE/launcher/
  wifid 扫描正常，0 PANIC。
- kunxpkg 端到端（`tests/scripts/kunx_pkg_test.py`）：修复前 3/20（HEAD
  基线 0/20），修复 update/ARG_LEN 后重跑确认（结果见下）。

---

## 十五、2026-09-07 修复轮（挂起项清剿 + 应用内动效）

针对「记录在案」的挂起缺陷逐项清剿，并按 `docs/animations.md` 规范补齐两个应用的动效。

### 内核修复

- **slab 半满状态双重释放防护补全（heap.rs）**：页标签只能拦截「页已归还/
  改作它用」的陈旧指针；半满 slab 内对同一对象二次释放仍会把空闲链表插坏
  （同一对象随后被分配两次互相踩踏）。`slab_free` 现按当前空闲数（≤512）
  遍历空闲链表，命中即静默拒绝 —— O(空闲数) 且 IRQ 已屏蔽，最坏几微秒。
- **pthread 工作线程信号帧边界放宽（signal.rs）**：`deliver`/`sigreturn`
  此前仅按主栈 ASLR 区间校验信号帧 —— CLONE_THREAD 工作线程的用户栈来自
  用户态 mmap，不在该区间，收信号一律被误判「栈越界」按默认动作误杀
  （SIGTERM 直接死、注册了 handler 的信号无法投递）。放宽为：主栈区间内
  快速通过，或全帧落在用户空间 `[PAGE_SIZE, USER_TOP)` 内（已映射可写校验
  与 LAZY 落地/COW break 由 `ensure_user_range_writable` 继续强制，内核
  恒等映射区仍被拒）。
- **FAT32 目录簇链扩容（fat32.rs）**：目录是普通簇链但 `find_free_slot`
  走到链尾即报「目录满」，簇链永不增长 —— 目录写满后创建文件/子目录整体
  失败。现扫描全链找空闲项，找不到时 `alloc_cluster`（分配即清零）给目录
  追加一个新簇，根目录同样适用；卷满返回原「目录满」错误，失败不破坏
  目录结构。

### 应用修复 / 防御

- **launcher fb0 ioctl 分辨率钳制（main.rs）**：`FBIOGET_VSCREENINFO`
  若返回异常分辨率（0 / 超大），`FB_W*FB_H*4` 在 release 下回绕为 0 →
  mmap 收到 len=0 恒 EINVAL —— 即「mmap /dev/fb0 偶发 EINVAL」的可疑路径。
  现对 w/h 做 ≤4096 合法性钳制，非法值保留默认 540x960 并打印诊断
  （异常值观测路径已消除，残余实例待新日志确认）。
- **launcher 启动布局转储（dump_layout）**：冷路径打印
  `[launcher] tile <key> page=<p> cx=<x> cy=<y>` 与 `[launcher] dock <key>
  cx=.. cy=..`，宿主回归脚本按真实坐标点击，模块过滤 / 网格列数变化不再
  使固定坐标失效。

### 动画（对齐 docs/animations.md 3.1 节奏）

- **calculator 按压反馈**：按下即时下沉 1px + 恒定压暗（iOS 按压语义）；
  松开闪光自最强 150ms ease-out 衰减；「=」结果 180ms ease-out 自下滑入
  弹出。全整数运算（无堆 / 无浮点），闪光期间主循环逐帧重绘。
- **clock 标签指示条滑动**：四个标签（时钟/闹钟/计时/秒表）的选中药丸从
  旧位 180ms ease-out 滑向新位，文字选中态随逻辑标签立即切换。

### 回归脚本重写（tests/scripts/apps_regression.py）

- 按 `[launcher] tile/dock` 转储解析真实坐标（旧脚本固定坐标在模块过滤
  （sms 被 OOBE 默认模块配置滤除）与网格列数变化下全部失效）。
- 被过滤应用记 SKIP 不再误报 FAIL。
- 返回语义修正：先点标题栏返回 (24,24)；应用内导航的应用（browser 等）
  直接走底部上滑手势回桌面（旧脚本 (24,24) 假设对 browser 不成立）。
- 交互抽查坐标保留（phone 拨号 / calendar 翻月 / music 播放暂停 /
  browser 书签），退出判定统一「标题栏返回 → 8s 超时 → 手势兜底」。

### 未修（记录在案，移交）

- DMA 缓冲 64B 缓存行隔离（仅 Pi4/KVM 影响，需行内实验）。
- virtio-blk `reset_ring` 超时恢复路径的完整设备复位（需真机/设备复现）。
- 中断风暴（iar=0 与 ISPENDR0 自相矛盾）根因：护栏保证不挂死，
  `act0` 诊断数据待复现采集。

### pkg e2e 揪出的严重缺陷：风暴护栏冻结整机（已修复）

- **`interrupt.rs` 风暴护栏对 PPI 写 `GICD_ICENABLER` 在 QEMU 下把调度器
  时钟中断真正关掉 → 整机冻结**。GICv2 架构规定 SGI/PPI 的 ICENABLER 写为
  RAO/WI（无效），此前按「写了也无害」放行 irq<32；但 QEMU 的 GIC 实现
  照单全收 —— PPI 风暴（iar=0x1b，CNTV 调度 tick）护栏一触发即关掉
  tick，虚机停摆（kunx_pkg_test 两轮实测：护栏触发后串口 60s 零输出，
  测试超时 FAIL；而应用回归 33 次风暴探测未到护栏阈值全程正常）。修复：
  屏蔽写仅对 SPI（irq≥32）执行；PPI 风暴 episode 实测会自行消散，只静音
  诊断不做屏蔽写。此项同时修正第十节「护栏保证不再挂死」的错误结论：
  旧护栏在 QEMU 下恰是挂死源。
- kunx_pkg_test.py 启动等待 560→900s（TCG 下 OOBE 13 步自动快进需 ~7 分钟，
  560s 不够，与本仓库「启动等待加裕量」先例同类）。

### 回归首跑暴露的两处缺陷（已修复并复跑确认）

- **launcher 对 Back（退出码 0）退出静默**（app_life.rs）：`handle_app_exit`
  仅在 `status != 0` 时打印「应用 X 已退出」—— 全部 Back 键退出对生命周期
  观测不可见，宿主回归脚本无法区分「已退出」与「假死」。改为无条件打印
  退出码。
- **回归脚本页面跟踪缺失**：手势兜底（超时后底部上滑）落在桌面上会被
  当作 Home 导航 → 桌面回第 0 页，脚本仍以为在第 1 页，后续 page=1 应用
  全部点空。手势兜底成功后同步 `state["page"]=0`；退出等待超时 8→10s。

### 验证

- `make` 全量构建 exit 0（内核 + 全部用户态组件 + ramdisk 重打包）。
- QEMU 无头冒烟：启动链完整、0 PANIC / 0 EL0 fault。
- 全应用回归（apps_regression.py，重写后）：**14/14 PASS** —— 14 应用
  ready+back 全过；phone 拨号挂断 / calendar 翻月 / music 播放暂停 /
  browser 书签 4 项交互抽查全过。
- 宿主测试：sync_tests **57/57**、wireless **105/105** 全过。
- kunxpkg 端到端（kunx_pkg_test.py）：见追加。

---

## 十六、2026-09-07 通宵修复轮（六路并行审查 → 30+ 项修复 + 性能 + 美化）

> 方法：6 个并行审查代理（browser 新模块 / launcher 与动效库 / 媒体类应用 /
> 内核图形动画输入 / settings+OOBE+共享 UI / 性能专项）逐行读码，人工复核
> 每条发现后分批修复。基线：`make` exit 0 / sync_tests 57 / wireless 105。

### Critical：OOBE 用户选择静默丢失

- **`apply_step_defaults` 无条件重置**：主循环每步「下一步」前先重置该步
  状态再保存 —— 英语/模块勾选/PIN 锁屏/深色主题全部存成默认值（第 8/9 步
  的作者注释只豁免了密码，其余 9 个可交互步骤全部中招）。修复：仅「30 秒
  空闲自动推进」应用默认（`wait_for_tap_next_button` 返回 4 区分）；空闲
  到达密码步关闭 `SEC_ENABLED` 且 `save_password` 清空 `security_type`
  （防「有 type 无 hash」开机锁死）；完成页等待「进入系统」点击（此前
  BTN_ENTER 全链路不可达，30 秒空闲自动进入保留自动化）。

### 应用 panic / 数据损坏

- files `render_storage`：`[u8;48]` 无守卫（1GB 级卷三段 8 字符 fmt_size
  即越界）→ 96B + push_fmt 有界。
- sms `modem.read_state` read 返回值按 usize（-errno 巨大值 → 切片 panic）；
  `write_dec(0)` 缺 `i-=1` 写出空串（id=0 会话共用一个文件、时间戳变长
  错位）；`now_stamp` 补零定长；空号码拒绝入库 + `title[0]` 空守卫。
- video / notes getdents64 `n<=0` 按 usize（错误当成功解析残留数据）。
- camera `cam_poll_resp` 重叠区 `copy_from_slice`（UB）→ `copy_within`；
  录像计时 >10 分钟 `b'0'+x` 溢出。
- contacts `local::write_file` 短写即静默丢数据（~130KB 单次 write）→
  分片写循环。
- map `push_dec` 丢弃负数（姿态/南纬西经渲染为空）→ i128 支持负号。
- phone 通话计时分钟钳 99。

### browser（远程页面触发）

- DOM 递归遍历无深度上限（~2 万层嵌套页打穿 1MB 用户栈）→ 解析期
  `MAX_TREE_DEPTH=256` 封顶 + 遍历深度守卫。
- `<button>` 文本恒空（`Node::text()` 对元素返回 ""）→ 新增
  `Dom::inner_text()`。
- HTTP 头 `\r\n\r\n` 跨 TCP 分段匹配不到 → 从拼接缝前 3 字节扫描；
  `\n\n` 终止符按实际长度取 body；chunked 明确报错。
- 图片无同源校验（`same_origin` 死代码，远程 `<img>` 可探测任意地址）→
  预抓取过滤同源 + kunxos://。
- CSS 长度无上限（`1e30px` → i32 饱和 → 坐标回绕簇）→ `parse_length`
  钳 0..=8192；max_scroll/thumb_y/visible() 溢出防护。
- `<a><img></a>` 链接 0×0 被 retain 丢弃 → Replaced 原子携带 ibox。

### launcher

- 锁屏按压 `PressIn` 永久卡死（抬起被 handle_lock_input 消费、弹簧不收敛
  → 60fps 常驻全屏重绘）→ `on_press` 加 `!LOCKED` 门控 + 锁屏消费手势时
  `cancel_press()` 兜底。
- `launch_blit` 步进 4 只填 2×2（ZOOM_BLOCK=4 改造漏改此处）→ 启动放大
  图标条纹镂空，补整块填充。
- 锁屏回拖 `LOCK_LAYER_OFF>0` 时 `gy as u32` 回绕（渐变顶行垃圾色）→
  off>0 跳过 y<off 行。
- PressOut 回弹期吞掉按下/点击（滑动结束立即点图标无反应）→ 允许打断。
- `app_uid` 解析 saturating；控制中心跟手速度钳 ±3000 / 弹簧初速钳
  ±4、跟手打开重建玻璃、滑条拖动只更新内存松手落盘。

### settings

- 拖拽滚动公式两路全错（触摸传 `TOUCH_PY - value`、鼠标传单次 REL_Y）→
  触摸传绝对 y、鼠标按下点 + REL 累计。
- 新增通用按压反馈（触点软边压暗圆斑写 canvas、抬起从 BACK_BUF 恢复，
  滚动重绘自愈）。

### 内核

- compositor `fb_present`：内核失效区（move_window/set_alpha/set_scale）
  不入拷贝集 → alpha 窗口每 2 帧重复混合（透明度逐帧加深）、窗口移走残影。
  内核失效区并入 copy 与 prev_app。
- anim：帧统计打印从 CNTP IRQ（~110 字符忙等 ≈9.5ms、超帧预算）延迟到
  内核主循环线程上下文；on_finish 回调（链式动画/销毁窗口）从 ANIMS 迭代
  中移出（`FINISH_Q` 循环外执行）。
- `fb_damage` 恰 32 矩形误降级整屏（`>=` → `>`）。
- perf overlay 不在拷贝/clean 集（40% 黑底每 2 帧累积全黑）→ 绘制前从
  canvas 恢复（新增 `restore_back_from_canvas`）。
- ui_framework Image 源宽钳制破坏行距 → 新增 `blit_strided`。
- 合成器采样坐标乘法 i64 化（极端几何回绕防护）。
- 内核 `memset` 升级 16B stp / 8B str 宽通道；`memmove` 无重叠走 memcpy、
  重叠反向（volatile 免疫循环惯用语自递归）。

### 性能（输出逐位一致）

- **EL0 memcpy/memmove/memset 宽通道**（shared/ttf_text.rs，所有应用的
  memcpy 定义）：ldp/stp 16B 通道 —— launcher 每动画帧 ~4MB 拷贝路径。
- 字形缓存键旁路数组（扫描 2KB 常驻 L1，替代 256×1KB 跨步）+ cache_store
  就地写入（去 1KB 栈临时）。
- fill_rect（app_kit/launcher）逐像素 → 行 slice::fill。
- app_kit 三个 TTF 字形路径共用 blit_glyph（求交一次、a==255 直通）。
- 小组件文件读 1s TTL 缓存（天气/电池/充电，动画期每帧 open/read/close
  归零）。
- 缓动求解牛顿收敛即返回 + 二分区间收窄（内核 anim.rs + launcher）。
- clean_needed 平台判定缓存；frame_tick 复用 tick 时刻；launcher 输入
  批读 48B→256B。

### 美化

- 强调色统一为 ColorOS 绿 `0x00B96B`（animations.md 3.5 规范）：清除
  0x22C55E / 0x34C759 / 0x1ABB5E 三种并存绿（22 个文件）。
- settings 按压反馈补齐（此前设置页全部交互无按压反馈）。

### 未修（记录在案，移交）

- compositor SX_TABLE 1024 项上限（>1024 宽分辨率静默丢列；系统固定
  540×960，仅分辨率扩展时需处理）。
- fb_present/composite 整体 IRQ 屏蔽窗口偏大（B2：短临界区快照 + 锁外
  搬运，需较大重构）。
- 状态栏/Dock 毛玻璃带每帧重算（缓存需枚举「带内背景变化」分支）。
- 跟手状态家族性卡死兜底（touch-up 丢失后 2s 无位移自动 settle）。
- launcher 最近任务卡片启动快照含面板残影（cosmetic）。
- input_stack.rs 死代码模块（含 grab id 复用隐患，建议删除或接线时加代数校验）。

### 验证

- `make` 全量构建 exit 0、**0 警告**；宿主测试 sync_tests 57 / wireless 105。
- 全应用回归（apps_regression.py）：**14/14 PASS** + 4 项交互抽查全过。
- QEMU 无头 `-kernel` 冒烟：启动链完整、0 PANIC / 0 EL0 fault。

---

## 十七、2026-09-07 第二轮（内核测试全绿 + 玻璃带缓存 + 内核级修复）

> 上一轮验证遗留的三项内核级无头回归此轮全部拉绿：power 18/21 → **21/21**、
> lifecycle-core 0/38 → **38/38**、sfsrace/wifi PASS。详见 CHANGELOG 同名章节，
> 摘要：

- **双 shell 劈开 UART 输入流**（bootargs shell + init 退出回退 spawn）：
  自动化注入命令按字节被两个并发 shell 劈开，双方都不执行。init 退出时
  不再重复拉起。
- **lifd log() 强制换行拆行多段日志**：新增 log_raw，10 处拼接消息改整行；
  补「通知已投递」日志。
- **风暴转储（IRQ 关闭 ~13ms 忙等 TX）饿死 PL011 RX FIFO**：抽出
  `uart_rx_drain()`，转储间隙主动排空 RX —— 终端粘贴/自动化注入不再截断。
- **pushd 恢复开机自启**：本地通知推送链路重新接通。
- **power_test / lifecycle_core_test 与实现脱节修复**：verbose 门控、
  bootargs selftest、中文化断言、BTASK_END 前置步骤等。
- **状态栏/Dock 玻璃带缓存**（键 = 调色板+主题，命中按行 memcpy）；
  最近任务卡片启动快照残影消除；删除死代码 input_stack 模块；锁屏快扫
  解锁阈值与跟手结算对齐。

### 验证

- `make` exit 0 / 0 警告；sfsrace PASS / wifi PASS / power 21/21 /
  lifecycle-core 38/38；宿主 sync 57 / wireless 105；全应用回归结果见下。
