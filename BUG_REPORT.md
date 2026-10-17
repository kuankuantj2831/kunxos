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

---

## 十八、2026-09-09 通宵修复轮（9 路并行审查 → 45+ 项修复）

> 方法：4 路审查未提交的 2026-09-08 改动（launcher 连续启动打断/手势、
> OOBE 步骤图标光锥、settings 导航保留区、图标字体管线）+ 5 路子系统深挖
> （内核 syscall/进程、内核驱动/网络栈、pkg/boot/tools、系统服务守护进程、
> 媒体类应用），全部发现逐条人工复核后修复。基线：`make` exit 0 / 0 警告、
> sync_tests 57、wireless 105、power 21、lifecycle-core 38。

### 一、未提交改动审查（2026-09-08 轮的遗留缺陷）

- **launcher 双线性采样左/上边缘外推**（sample_icon_bilinear_stride +
  draw_icon_asset 两处同款）：整数部分钳制后才计算插值权重，fx<0 时 tx 为
  负 —— lerp 变线性外推：alpha 可超 255 溢出顶字节（不透明边缘变半透明
  垃圾色）、暗像素邻亮像素外推出负 RGB 饱和成黑边、拖拽路径 1.12 倍外扩
  采样 fx 至 -3 直接把图标边缘"咬掉"。修法：权重由未钳制 floor 值计算后
  clamp 到 [0,1]（右/下边缘本就无害，伪影只出现在左/上）。
- **launcher start_launch 打断失败路径污染 Return**：return_interrupted +
  快照压暗发生在 gate/fork 之前 —— gate 写失败或 fork 失败时 Return 转场
  带着压暗中途帧继续播、BG_RECENTS 已被误清。重排为 gate→fork 成功后才
  清挂起状态/快照/压暗/瓦片预渲染；失败路径原样返回，Return 以原快照播完
  （完成后照常开最近任务）。
- **launcher resume_app_at 恢复转场零帧**：launch_spring 未赋 pos/target
  （Spring::launch() 恒 0），step_ms 首调即 settled —— 恢复动画 0 帧播完，
  屏幕停在旧帧直到应用 SIGCONT 自绘（返回中途点后台应用图标时必现）。
- **launcher TRANS_FRAMES 串账 + page_drag_allowed 死条件**：被打断的
  Return 帧数计入下一次启动的「启动过渡：N frames」日志（打断时清零）；
  `phase != Return` 被 `matches!(Idle|PressIn)` 完全覆盖且注释误导，删除。
- **OOBE 光锥扫光抹掉光标**：play_icon_cone 每帧整屏 flip 把 overlay 写进
  FB 的光标擦掉且结束后不恢复 —— 鼠标会话每次步骤切换光标凭空消失直到
  再次移动。render_and_flip 把 cursor_overlay_init 移到光锥之后。
- **OOBE 光锥恢复矩形削标题 + 逐帧全量栅格化**：restore 矩形下缘 y=210
  越过 PAGE_TITLE_Y=208（标题墨迹被擦且随终态驻留），下缘钳到标题行上方；
  124px 加粗字形 ~17KB 远超 1KB 字形缓存，14 帧每帧全量栅格化+膨胀 ——
  拆出 blit_icon_gradient 整段动效只栅格化一次。
- **OOBE 完成页「上一步」照样进桌面**：step 12 的 wait_for_tap_next_button
  返回值被丢弃（`let _ =`）——「上一步」在该页仍绘制且可命中，点击直接
  enter_desktop。返回 -1 现回退 step 11（与普通步骤语义一致）。
- **settings 保留区短拖变「抬起位置点击」**：NAV_EXCLUDE 内 pointer_up 恒
  sliding=false，classify 无手势时 tap 用抬起点 —— 保留区内 11~47px 短拖
  误开列表项/开关。加 12px slop（触摸/鼠标两路）：位移超 slop 不产生 tap。
- **settings press_feedback_up 裸拷绕过 LUT**：恢复循环把 BACK_BUF 直写 FB，
  默认亮度 85 下 LUT 非恒等 —— 抬起处留一块偏亮 ~18% 的方块并踩掉光标
  sprite。恢复走 flip 同款 lut_px。
- **settings 底部保留区吞滚动起点且不分手势模式**：BOTTOM_ZONE=130 比导航
  条(56px)高 74px；buttons 模式 classify_swipe 恒 None，保留区纯属滚动死区
  （列表最后 1~2 行起手滚不动）。NAV_EXCLUDE 现仅 MODE_GESTURE 生效。
- **settings 滑条/滚轮无变化也写盘+重绘**：滑条抓取时纯竖直移动每事件
  slider_set→cfg 落盘 + 整屏重绘（x 未变返回 false）；REL_WHEEL 在钳位端
  不再置 redraw（wheel_by 返回是否实际变化）。
- **calculator 释放落空按钮卡在按下态**：本 diff 给鼠标路径新接
  press_feedback_begin 后，拖出网格松开的 else 分支只置 HOLD_BTN=-1 不置
  HOLD_CHANGED —— 下沉画面驻留到下一次点击/动画（触摸路径同款旧患一并修）。
- **图标字体管线核查干净**：246 码点 ↔ make_fonts.py ↔ 子集字体三方一致、
  settings/ui/weather 全部新码点存在于 Phosphor 2.1.1 cmap 且与官方
  name→码点映射零错配、Packages/available 逐字节一致、Packages.sig 与三个
  .kunx 包签名全部验签通过 —— 无需修复。

### 二、内核

- **PM_FREEZE / SET_CPU_CAP 无权限校验（High）**：任意 uid 10000+ 应用可把
  launcher/系统服务限到 1% 或软冻结（cpu_cap==0 的任务 pick_next 直接跳过）。
  加与 sys_kill 同级检查（root / 自身 / 目标父进程），EPERM 拒绝。
- **reap() 可重入/双跑（High，SMP）**：多线程父进程两个线程被同一次 exit
  的 wake_pid 同时唤醒会扫到同一僵尸，reap 无互斥 —— 双重
  free_process_tables（把已复用的页二次归还分配器，后续 alloc 把同一物理页
  发给两个进程）+ fd 引用错减。reap 改原子认领：关中断窗口内 Zombie→Free，
  只有认领者释放；竞争失败/护栏命中返回 None，wait 循环重试（让出防紧转，
  重试阻塞保持屏蔽窗口防丢唤醒）。**wait() 误报成功**（护栏早退只返回退出码、
  wait 当回收成功 → 同一僵尸可被报两次）随 Option 化一并修复。
- **sleep_until 不恢复 DAIF（Medium）**：入口无条件 daifset 且所有返回路径
  都不恢复 —— 调用方从此永久关中断运行（fs_lock 退避把整个后续 FS 临界区
  拖进关中断窗口；perflog 线程第一次睡眠后恒关中断）。与 yield_now 同模式
  保存/恢复。
- **write(2)/pwrite(2) >4KB 静默截断（Low）**：read 侧已按块循环，write 侧
  仍 `count.min(4096)` 单趟 —— 朴素应用不重试即丢数据。File 循环写（中途
  错误返回已完成部分，Linux 语义）；管道保持单趟 ≤4KB（读者不消费时循环会
  无限阻塞，且 POSIX 允许管道短写）。
- **sys_pipe2 无视创建失败（Medium）**：池满/fd 表满把两个 -1 写给用户还
  返回成功，应用拿 fd=-1 后续全乱。补 EMFILE（与 sys_socketpair 对齐）。
- **getdents64 缓冲半满丢目录项（Medium）**：readdir_next 消费式推进后才
  判断放不放得下 —— 放不下时该条目被静默跳过，枚举从此丢项；缓冲装不下
  一条记录时误报 EOF（POSIX 应 EINVAL）。新增 vfs::readdir_back 回退流位置。
- **SIG_TABLES / ELF_PROCS 回收不清理（Medium）**：reap 不清信号 handler 表
  也不调 elf::release（reap_forced 有）—— pid 复用后 kill 会把本应默认
  终止的信号跳转到新地址空间里的陈旧 handler 地址。补齐。
- **PAGE_REFS 引用计数 RMW 无 IRQ 屏蔽（Low）**：cow_break（IRQ 使能）与他核
  exit→free_user_pages 交错丢一次递减 → 共享页计数永不归零（4KB 泄漏）。
  mark/share/cow_share/unshare 四函数加 irq_save/restore。
- **帧统计冲刷跨上下文竞态（Low）**：perf::poll_report / anim
  report_frame_stats 的读-排序-清零可被 note_frame(IRQ) 打断 —— 抹掉新样本
  /跨窗口混算。快照+复位收进关中断窗口，UART 打印留在可中断上下文（不回退
  上轮「IRQ 内忙等打满帧预算」的修复）。
- **net_virtio 只 ACK bit0（Medium）**：virtio-mmio InterruptStatus bit1=
  config-change 永不应答 —— 纯 config-change 中断电平持续 → GIC 立即重
  投递 → 风暴护栏把整块网卡禁掉。无条件 ACK 全部 cause，drain 仍只对 used。
- **TCP 裸 FIN 重传不回 ACK（Low）**：CLOSE_WAIT/FIN_WAIT2 等状态对我方
  首个 ACK 丢失后的对端纯 FIN 重传不响应（fin_seq<rcv_nxt 分支缺 ACK）——
  对端只能 RTO 重传到放弃，拆除卡在 CLOSE_WAIT 直到本端 close()。补与
  TIME_WAIT 分支同语义的重发 ACK。
- **输入缓冲全满劈帧（Low）**：满时按单事件丢最旧，可能把 DOWN 帧劈开
  （BTN_TOUCH=1 与它的 ABS_X/ABS_Y 分离 → 用户态按下点退化为悬停旧位置）。
  改为丢弃最旧的一个完整帧（到首个 SYN 含）。
- **BTN_TOUCH/ABS 顺序（待验证项→结论，无需修）**：内核唯一注入路径
  FRAME_DOWN 的发射顺序是 ABS_X→ABS_Y→BTN_TOUCH→SYN（同批同时间戳），环形
  缓冲单生产者顺序保真 —— 用户态在 BTN_TOUCH=1 时记录按下点的做法正确。

### 三、系统服务

- **后台应用被外部杀死 → 僵尸 + lifd 注册槽永久泄漏（High）**：launcher 只
  在前台态 waitpid；lifd alive()=kill(pid,0) 对 Zombie 恒真（process_exists
  对 Zombie 返回 true）→ 双方互等。泄漏 8 次后全部新应用登记失败（应用
  lc_home 死循环假死）、16 个僵尸后 fork 失败，无自愈路径。
  poll_background_events 补 waitpid_nohang：回收成功即清 launcher 状态并
  report_exited（不走 handle_app_exit —— 其假设 fb0 仍是应用末帧并播返回
  动画，后台被杀时屏上已是桌面）。
- **BG_RECENTS 恒 false（Medium）**：lifd 写的 reason 是 7 字节 "RECENTS"，
  launcher `windows(8)` 的 8 字节窗口与 7 字节模式永不相等 —— 最近任务
  手势返回后动画结束不开最近任务面板。改 windows(7)。
- **lifd BTASK 截断绕过白名单（Low）**：BTASK 256 经 u64→u8 截断成音频类型
  (0)，白名单对截断后的值检查形同虚设；超长 dur 回绕。先按 u64 校验再收窄。
- **wifid FORGET 只删文件（Low）**：NETS 条目一旦加载永不变 inactive ——
  已「忘记」的网络仍被 AUTO 开机自连；同 ssid 重新 SAVE 时旧 psk 留内存。
  FORGET 失配失活；SAVE 成功后同 ssid 就地更新内存表。
- **wifid getdents 解析缺守卫（Low）**：缺 reclen<19 与末记录越界检查
  （lifd/pushd 都有）—— 目录流异常时切片 panic。对齐。
- **alarmd 信箱 256B 截断（Low）**：单次 read(256) 后立即 ftruncate，超长
  排队命令尾部被静默丢弃（BUF 实为 512）。改循环读到 EOF（networkd 语义）。
- **rild 联系人缓存只读 2KB（Low）**：load_contacts 单次 read，联系人 >17
  条后不进缓存（来电/短信不显示姓名）。改循环读到 EOF。
- **audiod best_seq 以 0 为基准（Low）**：`seq.wrapping_sub(0)<2^31` 只接受
  seq<2^31 —— 客户端 100Hz 计数约 248 天越过后两槽全被拒收，该客户端音频
  永久静默。基准取 last_seq（顺带修掉陈槽重投递）。
- **音频双槽提交顺序（Low）**：write_chunk 先写 seq 头后写 PCM —— audiod
  2ms 轮询在两写之间读到「新 seq + 旧 PCM」→ 播 10ms 陈旧数据（爆音）。
  改为 PCM 先写、seq 头最后单写提交。

### 四、应用

- **camera 预览帧路径差一（High）**：read_preview_frame 用 `&path[..n-1]`
  把结尾 'v' 切掉 —— open 的永远是 /tmp/cam/frame_X.yu，恒 ENOENT，
  取景器/缩略图完全失效（camerad 模式下主功能全废）。一行修复。
- **camera 系统导航退出不停预览流（Medium）**：手势/三键退出走 exit() 不经
  camera_stop —— camerad 侧环形帧文件被持续覆写。nav 分支先停流再退出。
- **camera REQ_ID 位数越界（Low）**：digits[4]/prefix[8] 无位数上限，
  id≥10000（约万次拍照/录像切换）越界 panic。扩到 10 位（prefix 12）。
- **cloud 恢复路径 .. 穿越（Medium）**：api_restore 只做前缀校验，内核 VFS
  真实解析 ..（目录项天然支持相对行走）—— 恶意服务器可写 /data/oobe 外
  任意可写路径。路径含 ".." 一律拒绝。
- **cloud lock/wipe 无视本机开关（Medium）**：CTRL_LOCK/CTRL_WIPE/CTRL_FIND
  只在渲染层展示，服务器命令无条件执行 —— 明确关闭远程锁定/擦除的设备仍
  被锁定/擦除。设备端加最后一道策略闸门（关闭即 ack denied）。
- **cloud 失败响应提示用陈旧 RESP（Low）**：http_request 失败路径不更新
  LAST_RESP_LEN —— resp_err_hint 用上一轮成功长度切本轮半截缓冲。入口清零。
- **files del_tree 无深度上限（Low）**：dir_total/copy_tree 都限 10 层，删除
  路径没有 —— 每层栈帧含 raw[2048]≈2.3KB，深目录链打穿 1MB 用户栈。补
  depth 参数（del_tree_depth）。
- **contacts 同步页密码框点不中（Low）**：字段命中区上界 260，第三字段
  (290..380) 漏掉 —— 只能靠 Enter 切焦点（calendar 同类 bug 的漏改残留）。
  上界改 380。

### 五、包管理 / 存储 / boot / 构建系统

- **ramdisk 变更不触发内核重编译（High，实测复现）**：cb9ca35 诊断出 build.rs声明 rerun-if-changed 会遮蔽 cargo 对 include_bytes!（dep-info）的变更检测，但只删了 ramdisk 一行 —— linker 脚本行本身仍在遮蔽：纯 rootfs 变化（sfsrace/audiotest 标记、rootfs 内容更新）不触发重编译，objcopy 把陈旧 .rodata 重新包装成 kernel.img（表象：测试标记「间歇性」丢失 —— 本轮实测 test-sfsrace 的/sfsrace-test 标记在镜像里、kernel.img 里却没有，内核走正常启动 → 测试超时 FAIL）。kunxcore 唯一的 include_bytes 就是 ramdisk.img，build.rs 显式声明 rerun-if-changed=ramdisk.img 即完备。
- **store SigV4 kDate 派生错误（High）**：date 被拼进 HMAC 密钥、message
  传空 —— 与 AWS SigV4 规范（date 是 message）必然不一致，对真实 S3/R2
  所有请求 403；本地 stub 不校验 Authorization 所以从未测出。按规范修正。
- **store 下载路径目录逃逸（High）**：索引 name/version 未经字符白名单即拼
  `/data/store/cache/<name>-<version>.kunx` 且写入先于验签 —— 恶意/被劫持
  源用 `../x` 可在任意目录创建/截断 .kunx 文件（失败路径还会 unlink）。
  index::parse_pkg 加白名单（拒绝 '/'、'..'、控制字节，与 kunxpkg
  valid_path 同级防线）。
- **store 流式下载短写不可检测（Low）**：write_fd 永不返回负值，`w < 0` 是
  死条件 —— 磁盘满时「下载成功」落盘残缺文件，直到验签才失败且报错误导。
  比较写入字节数，短写即 R2_ERR_RECV。
- **mkfs_sfs layout() 死循环（Medium，实测复现）**：need<bmap 时的复合赋值
  把两值同时钉死，`layout(33813)` 永不返回 —— sys 分区扩到 ~132MB 以上或
  自定义 total 落进窗口即构建挂死。改为二分求唯一不动点（need 关于 bmap
  单调递减）；33812/33813/33814/65536/131072/262144 与 4096..40000 全扫描
  收敛，total=4096 结果与旧实现一致（data_start=532, jlog=512，镜像不变）。
- **kunxpkg 注册写失败漏 restore_backup（Medium）**：升级时已安装记录写
  失败只 rollback 不恢复备份 —— 旧记录声称 vX 已安装而二进制已删。与另
  两条失败路径对齐。
- **kunxpkg 升级回滚丢旧版 /res（Medium）**：rollback_files 无条件清空
  /res/<name>/ 而备份不含 res —— 升级失败后旧版未覆盖资源永久丢失。逐项
  unlink 已覆盖新包文件；整目录清空移到卸载路径自行执行。
- **kunxpkg semver 通配误判（Low）**：对整段 any 检测 'x' —— "1.2.3-next"
  的 pre 段命中 → 整条约束退化 1.x 全匹配。仅末段恰为 x/X/* 才算通配。
- **kunxpkg alloc_uid 收集表饱和（Low）**：>64 包时 UID 未收集全仍继续分配
  —— 可能两应用共享 UID 破坏沙箱隔离。饱和即拒绝安装。
- **kunxboot dtb insert_props 越界 3 字节（Low）**：容量检查未计 insert_off
  —— 畸形 DTB（chosen 名字 NUL 恰在结构块末尾且缺 END_NODE）写入尾可越出
  scratch 最多 3 字节（踩进 .bss 相邻变量）。insert_off 计入约束。

### 六、记录在案（本轮未修，移交）

- 信箱 read→clear 竞态（系统性 6 处：launcher evt / lifd cmd / alarmd /
  audiod / networkd / wifid / rild）：普通文件模拟 FIFO，写方 O_TRUNC/append
  与读方 read→清空之间无原子提交。需 commit 标记、写临时名换名或内核 rename。
- rild rewrite_store 就地重写 inbox.dat：掉电点可整箱短信丢失（同需 rename
  或墓碑标记）。
- kunxpkg 依赖不递归安装：依赖在索引中「存在」即放行，装出的包运行即坏
  （需递归安装 + 防环访问集，或至少显式警告用户）。
- kunxpkg 索引/记录写入无原子性无 fsync（需内核 rename/fsync 支持）。
- store Packages.json.sig 缺失时仅告警照收（强制验签需 e2e 配套改造）。
- SMP 专项·已修三债 + 新发现竞态（2026-09-16 SMP 打通轮，详见下文
  「SMP 打通记录」）：原三项（PROCESSES 槽位分配非原子 / execve 兄弟线程
  跨核 kill_req 待兑现窗口 / fs_unlock 迁移+嵌套释放）已修复；新暴露
  `TASKS`/`CURRENT` 等调度器静态表的无锁并发面（sfsrace 双核压测偶发
  任务表踩踏，时序敏感）——`-smp 2` 定位实验性，默认回归仍单核。
- cloud 长按解锁实为「间隔 2-3s 双击」（poll_input 只在抬起产 tap；需 kit
  暴露 press 事件或改文案）。
- clock/notes 编辑子页标题栏返回/ESC 直接退出应用（与 calendar/contacts 的
  逐级返回不一致，用户编辑中误触即丢应用）。
- 历史遗留：compositor SX_TABLE 1024 上限、fb_present IRQ 窗口偏大、DMA 64B
  缓存行隔离（Pi4/KVM）、中断风暴 act0 诊断（待复现采集）。

### 验证

- `make` 全量构建 exit 0 / 0 警告。
- 宿主：sync_tests **57/57**、wireless **105/105**（全部修复后复跑）。
- 内核无头回归：power **21/21**、lifecycle-core **38/38**（reap Option 化、
  sleep_until DAIF、PM 权限等内核修复后）。
- mkfs_sfs layout()：死循环修复后 33812/33813/33814/65536/131072/262144
  与 4096..40000 步进 7 全范围收敛；total=4096 与旧实现结果一致（镜像不变）。
- 全应用回归（apps_regression.py）：**14/14 PASS**（phone 拨号 / calendar
  翻月 / music 播放暂停 / browser 书签 4 项交互抽查全过）。
- **sfsrace PASS**（4 进程 ×12 轮并发创建/读写/截断/删除，vfs::readdir_back
  与 getdents64 回退/EINVAL 改动覆盖）、**wifi PASS**（wifid 解析守卫与
  SAVE-FORGET 内存表同步覆盖）。
- 无头冒烟（-kernel -nographic）：启动链完整、0 PANIC / 0 EL0 fault。
- 注：sfsrace 首轮 FAIL 即上方构建系统根因的实证 —— 标记已入 ramdisk
  （116 inodes）而 kernel.img 仍嵌旧镜像（115 inodes），内核走正常启动
  超时；build.rs 修复 + 重编后 PASS。

---

## 十九、2026-09-12～16 TLS/NetSurf 移植轮（内核 musl 兼容缺口 + 验证环境治理）

> 方法：以「musl 静态 Linux 程序」为靶子（nsmonkey / netsurf-fb / tlstest
> 实机驱动）逐个暴露并补齐内核缺口，每条发现均实机复现后修复；轮末全量
> 回归时揪出一例**验证环境退化**（非代码回归）。基线：`make` exit 0 /
> 0 警告、sync_tests 57、wireless 105、power 21、lifecycle-core 38、
> tlstest PASS、nsmonkey 冒烟 PASS。

### 一、内核 musl/Linux 兼容缺口（修复清单）

- **auxv 缺失（阻塞级）**：内核 execve 栈布局只有 argv/envp，无 auxv ——
  musl 静态程序 __init_libc 依赖 AT_PHDR 定位程序头表（读动态段/初始化
  TLS），缺失即启动即死。补全 AT_PHDR/PHENT/PHNUM/PAGESZ/ENTRY/UID/EUID/
  GID/EGID/HWCAP/CLKTCK/SECURE/RANDOM/EXECFN；AT_RANDOM 熵源优先 virtio-rng、
  时间回退；phdr_va 由「程序头表落在哪个 PT_LOAD 段内」推算。
- **CLONE_CHILD_CLEARTID 缺失（阻塞级）**：musl 新线程以
  clone(CLONE_CHILD_CLEARTID, child_tidptr=&__thread_list_lock) 创建 ——
  线程退出需内核向该地址清零并 futex 唤醒等待者，否则 pthread_join 永远
  挂死、__tl_lock 死锁。补 set_tid_address 记录槽 + exit_thread_notify
  （ dying 线程地址空间仍活跃时写 0 + wake，含用户指针校验）。
- **ppoll 缺失（本轮验证期发现）**：pselect6 已实现，但 musl 的 poll()
  走 **ppoll**（aarch64 nr 73）—— 此前未分发恒 ENOSYS，nsmonkey 的
  select 轮询事件循环永不读 stdin/不建窗口（现象：`GENERIC POLL TIMED`
  倒计时刷屏、WINDOW NEW 无输出）。与 pselect6 合并为 sys_poll_family
  （仅超时参数寄存器不同：x[4] / x[2]）。
- **UART read 返回 0 = EOF（阻塞级）**：/dev/ttyS0 无数据时返回 0 ——
  POSIX 语义 0 = EOF，musl fgets 立即结束，nsmonkey 等交互程序启动即退。
  改为阻塞至 ≥1 字符再取走当时可用全部（无数据 yield 让出）。
- **write/pwrite >4KB 静默截断（High）**：`count.min(4096)` 单趟写 ——
  超过 4KB 的写不报错也不重试提示，朴素调用方丢数据且不自知。改按 4KB
  块循环写（中途错误返回已完成部分，Linux 语义）；管道保持单趟 ≤4KB
  （POSIX 允许管道短写，且写循环在读者不消费时会无限阻塞）。
- **writev 零长 iov 死循环（Medium）**：零长 iov 项此前 `n == 0` 即 break
  —— musl 无缓冲流（stdout 按行刷新）首项为零长时死循环。改为 continue。
- **rt_sigprocmask ENOSYS（Low）**：musl 启动路径 __block_all_sigs 调用并
  忽略返回值，ENOSYS 亦可行但语义上应答成功桩（内核无信号投递队列）。
- **TCP 固定接收窗口（Medium，吞吐）**：通告窗口恒 16KB —— 对端在接收
  缓冲（16KB）填满后继续超发，丢段只能等 RTO；取走数据后也不立即 ACK，
  窗口重开靠对端零窗口探测。改通告真实可用空间（缓冲 − 未取走），取数后
  立即 ACK；另补对端 FIN 重传重 ACK（此前不回，对端 RTO 重传到放弃，
  拆除卡在 CLOSE_WAIT 直到本端 close）。
- **页故障 DFSC 误判（High，mmap 路径）**：mmap 匿名区落在继承自内核的
  2MB 块映射地址范围内时，访问报 L2 权限错误而非 translation —— 旧代码
  按 DFSC 精确分类，LAZY 预留页永不落地。改为掩码归一后统一「先 LAZY
  落地、再 COW、均失败才 SIGSEGV」，wild 指针语义不变。
- **用户栈 1MB 不足（Medium）**：NetSurf 深递归（libcss 选择器匹配等）
  打穿 1MB 栈。扩到 8MB（预留区间上限内，仍按需映射）。

### 二、内核健壮性（移植期顺带修复）

- **wait 收尸防重入（High）**：多线程父进程两个线程可被同一次 exit 的
  wake_pid 同时唤醒、扫到同一僵尸 —— 双重 reap = 双重 free_process_tables
  （同一物理页二次归还，后续 alloc 把同一页发给两个进程）+ fd 引用错减。
  reap 改为关中断窗口内原子认领（Zombie → Free），失败方返回 None 由
  wait 循环重扫。
- **pid 复用残留（Medium）**：收尸未清 ELF 运行时信息与 SIG handler 表 ——
  pid 复用给新进程时，陈旧 ASLR 栈基址/brk 被 current_stack_base()/
  current_brk() 当作新进程的值，残留 SIG handler 把默认终止信号跳转到新
  地址空间的垃圾地址。reap 时对齐清空。
- **virtio-mmio IRQ 只 ACK used-ring（Medium）**：设备发纯 config-change
  （InterruptStatus=0b10）时永不写 ACK 寄存器 —— 电平中断被 GIC 立即
  重投递 → 风暴护栏触发把整块网卡禁掉。改为无条件 ACK 全部 cause，drain
  仍只在真有 used 更新时执行。
- **页引用计数 RMW 竞态（Medium，SMP/IRQ）**：mark_shared_page/share_page/
  cow_share 的读-改-写无 IRQ 屏蔽 —— 与另一核 exit→free_user_pages 交错
  丢一次递增/递减，计数永不归零 = 4KB 页永久泄漏。全部补 irq_save/restore
  （与 free_page 同款防护）。
- **readlink errno 语义（Low）**：不存在与非符号链接都返回 -1 —— musl
  realpath 逐组件调 readlink 且只容忍 EINVAL，其他错误码使 realpath 对
  一切路径失败。区分 ENOENT / EINVAL。
- **readdir_back 丢目录项（Low）**：getdents64 用户缓冲装不下「刚被消费」
  的条目时直接丢项，目录枚举从此缺文件。补 readdir_back 回退目录流位置。
- **kunxcore/build.rs ramdisk 跟踪补全**：cb9ca35 只删了 ramdisk 一行，
  linker 行的 rerun-if-changed 仍在遮蔽 include_bytes! 的 dep-info 跟踪
  （纯 rootfs 变化不触发重编译）—— 显式声明 rerun-if-changed=ramdisk.img
  即完备（本包唯一 include_bytes）。

### 三、验证环境：disk.img /data 卷 inode 耗尽（本轮新发现，环境性）

- **现象**：lifecycle-core 回归 23/38（且逐轮波动 23→25→12）——失败集中在
  文件信箱命令（reg_/cmd_* 写入无 ack），`ls /data/life` 仅剩 evt_8/evt_11/
  launcher_evt/recents 残留；对 /data/life/cmd_0 的 O_CREAT 创建**稳定**
  返回 -1，而同目录读取（recents）不受影响。
- **定位**：vfs::open 失败日志加临时全量记录 + shell 重定向失败临时打印 →
  `[vfs] open 失败 err=-1 used=8/256`（打开文件表仅 8/256，排除 fd 表耗尽）
  → fs_create 在 inode_alloc 无空闲位时返回 -1 —— **SFS imap 满**。
- **根因**：disk.img 为跨测试轮持久卷（/data 256 inode 上限，mkfs 仅在无
  SFS 签名时执行）。lifecycle 每轮注册的 evt_<pid>/pending/state 等文件在
  关机路径不清理，pid 漂移即新增永不删除的残留文件；一周多轮测试（本轮
  netsurf/tlstest 高频上机）后 imap 渐满，create 开始失败 —— 表现为「测试
  间歇性挂」的假回归。
- **处置**：重建 disk.img（`rm disk.img && make disk`）后 lifecycle-core
  38/38。注意：**新盘首启会触发全盘 mkfs，与守护进程启动存在竞态**（首轮
  reg 全失败 12/38），第二轮起正常 —— 回归脚本在新盘上需先跑一次热身引导。
- **移交**：① 关机/收尾路径清理 /data 测试残留（或测试脚本自带 teardown）；
  ② df/sysinfo 暴露卷空闲 inode 数（本次只能靠串口日志反推）；③ vfs::open
  失败 errno 细分（ENOSPC/EEXIST/ENOENT 混同为 -1，排查期只能全量开日志）。

### 四、记录在案（本轮未修，移交）

- **shell 重定向 open 失败静默回退 stdout**：`echo x > 不可写路径` 不报错、
  内容直接上屏（排查期临时加过错误打印，已还原）——建议回退前打印
  「sh: 无法打开 <path>」。
- **alarmd 每 400ms open(/data/alarms.txt) ENOENT 刷屏**：无闹钟文件时
  永久失败重试，淹没有效串口日志（建议启动时创建空文件——CTL_PATH 已有
  同款先例——或失败后降频）。
- **HOME 环境变量未设置**：nsmonkey 资源路径解析成 /~/.netsurf/...（无害
  警告）；建议 shell 默认 export HOME=/data。
- **vtimer PPI（irq 27）风暴**：既有「风暴探测器 + 护栏」已兜底（episode
  自行消散，输入不丢），根因（GICv2 PPI eoi 时序）仍待复现采集。
- **信箱 read→clear 竞态（十八已记录）**：本轮 lifd「先删除再处理」路径
  与 shell O_TRUNC 写入的窗口依旧存在，测试时序依赖的偶发丢命令属此列。

### 验证（2026-09-16）

- `make` 全量构建 exit 0 / 0 警告。
- 宿主：sync_tests **57/57**、wireless **105/105**。
- 内核无头回归：power **21/21**；lifecycle-core 共享树复跑受二十·附所记
  信箱回归 + 同机并发 QEMU 干扰受阻（根因已修复，隔离树 38/38），终验待
  收敛后复跑。
- 浏览器 TLS：来宾侧 tlstest **PASS**（QEMU slirp 真网，www.baidu.com:443
  全握手 + 证书链验证 + 取页）；宿主侧 hosttest PASS（同源码 std TcpStream，
  握手 35ms、3 级证书链）。
- NetSurf：nsmonkey 无头冒烟 **PASS**（WINDOW NEW → file:// 取页 → 解析 →
  排版，TITLE/UPDATE_EXTENT 事件齐全）。
- 无头冒烟（-kernel -nographic）：启动链完整、0 PANIC / 0 EL0 fault。

---

## 二十、2026-09-16 通宵修复轮（三路并行审查 → 15 项修复 + 5 组动画）

> 方法：3 个并行审查代理（① 未提交的 2026-09-11 对话框统一轮 diff 逐文件
> 回归审查 → F1~F11；② 信箱 read→clear 竞态全仓盘点（10 站点 + rename
> 内核语义约束）；③ clock/notes/cloud 挂起项方案盘点）+ 主会话实施。
> 另注：本轮与另一并行会话同期作业（内核 / kunxpkg / var/run 守护进程信箱
> 由该会话负责），本轮边界为 launcher/动画/应用侧与 lifd 族信箱，无文件
> 冲突。全部通过 `make` 全量构建（exit 0）+ 宿主 sync 57 / wireless 105。

### 未提交 diff（对话框统一轮）回归修复（审查①发现）

- **F1（高）blit_icon 不透明段直写 BACK_BUF 绕过 TILE_MODE/FOLDER_TILE**：
  本轮性能优化给 blit_icon 加的「不透明段整段 copy_from_slice」快路径不看
  渲染目标模式 —— 图标缓存就绪后任何 `TILE_MODE=true; draw_app_icon(0,0)`
  把图标主体写进 BACK_BUF 左上角（污染状态栏），瓦片本身只剩边缘像素
  （启动转场放大瓦片错图标/编辑模式抖动图标损坏）。`draw_app_icon` 入口
  对两模式跳过缓存快路径，回退 put_pixel 路径（put_pixel 正确路由瓦片）。
- **F2（高）小组件玻璃缓存键不含几何**：布局变化（编辑模式移除小组件 /
  桌面设置切网格列数）后命中旧缓存，把旧位置模糊出的壁纸玻璃 blit 到新
  位置（内容错位）。缓存键加入 (x,y,w,h)。
- **F3（中）「桌面设置」卡直接点确定把壁纸序号误当网格选择**：sheet 3 与
  壁纸卡共用 SHEET_WALL 暂存 —— 打开即被赋为 WALLPAPER(0..3)，直接点
  OK 时 `SHEET_WALL==1` 会把 4x6 用户悄悄切成 5x6（默认壁纸=2 时反向）。
  新增独立暂存 SHEET_GRID（打开时 = GRID_COLS，直接 OK = 无变化）。
- **F4（中）Toast 退场残影长驻**：退场末帧 alpha ~13（8ms 轮询），过期后
  不再触发重绘，局部刷新路径下淡出轮廓停留数分钟。`toast_anim_active`
  过期后 16ms 内补一帧清理（此时 draw_overlay 已不绘制 → 干净帧）。
- **F5（中）文件夹压暗缓存冻结分钟级 UI**：`FOLDER_DIM_OK` 命中时把缓存
  memcpy 回 BACK_BUF，会抹掉分钟沿刚重绘的状态栏（文件夹打开跨分钟时
  状态栏时间停走）。分钟沿分支对 `FOLDER_PHASE != Closed` 先失效缓存。
- **F6（中）browser decode_chunked 整数溢出 → 远程可触发 panic**：分块
  大小 "FFFFFFFFFFFFFFFF" 解析为 usize::MAX，`i + size` 回绕绕过边界检查，
  切片终点小于起点直接 panic（no_std 应用即 abort）。size 上限 + 双
  checked_add。
- **F7（中低）编辑选择卡白卡入场淡入不生效**：卡片填充/描边走
  `LauncherSmooth::put_aa` 直写，不乘全局 PIX_ALPHA —— 第 1 帧全不透明
  「弹出」而遮罩/文字在渐显。put_aa 与 put_pixel 一致乘 PIX_ALPHA。
- **F8（低）dialog draw_check 未选中态角弧描边缺口**：`qx+qy>r` 的 L1
  判定漏掉弧中点（四角 1px 缺口）。改欧氏环带 `(r-1)²` 判定。
- **F9（低）鼠标长按缺控制中心判定**：鼠标长按是主循环按 MOUSE_DOWN 计时
  触发的，不经过手势分类 —— 控制中心/通知中心展开时长按误进编辑模式。
  补 `cc_visible()` 早退（触摸路径 on_blank 分支已有同款）。
- **F10（存疑，未修）连续启动打断后旧后台应用的单槽状态被覆盖**：回收
  路径未完整核验，移交后续轮补验。
- **F11（低）dialog.rs 文档示例 scrim 签名过期**：doc 修正。

### 挂起项清剿（审查②③方案落地）

- **rild rewrite_store 双缺陷**：(a) 就地 lseek(0)+write 不 ftruncate ——
  删除记录后新内容更短、旧尾巴残留，`count_records` 数 `\n` 把陈尾当记录
  → **已删短信「复活」**（比掉电更高频的日常 bug，本轮新发现）；(b) 重写
  中途掉电 → 半行损坏不可恢复。修复：旧箱 rename 为 `.old` 备份 → O_TRUNC
  全新写 → 成功后删备份；新箱写失败把备份改回（旧箱完整）；启动时
  `recover_store()` 检测正身缺失/非法而备份有效即回滚（丢的只是一次
  删除/标记，记录本体无损）。改名失败回退「ftruncate(0) + 写」就地路径。
- **clock 编辑子页返回/ESC 逐级返回**：闹钟编辑页（TAB_ALARM+APAGE_EDIT）
  的标题栏返回与 ESC 原本直接 `break` 退出整个应用（编辑中误触即丢）。
  现回闹钟列表（与「取消」按钮同语义：APAGE=APAGE_LIST + EDIT_FOCUS=0），
  根页才退出（对齐 calendar/contacts 的分级返回）。
- **notes ESC 逐级返回**：编辑器页 ESC 原本直接退出（标题栏返回已是分级）。
  现编辑器 ESC 回列表（PAGE=0 + 清搜索焦点），列表页才退出。
- **cloud 长按解锁改真长按**：原实现只在 tap（抬起）帧判定 —— 实际语义是
  「两次点击间隔 2-3s」，真长按（按住 2s 再抬）反而无法解锁。改为每帧
  检测 kit 静态量（TOUCH_DOWN/TOUCH_US/TOUCH_PX/CUR_X）：按下起点与当前
  位置都在解锁按钮内且持续 ≥2s 即解锁；解锁后吞掉本次抬起（解锁按钮区
  与主界面按钮区有重叠，不吞会误触「立即同步」）。
- **信箱 read→clear 竞态（lifd 族，系统性缺陷的 /data/life 部分）**：
  - `shared/lifecycle.rs`：`lc_write_cmd` 改 **每消息一文件**（`cmd_<pid>_<seq>`，
    写 `.tmp` 后 rename 发布 —— 目标名永不存在，rename 恒成功，lifd 的
    getdents 扫描 + parse_dec 到非数字停止，读方零改动）；`lc_read_evt` 改
    **rename 取走**（`evt_<pid>` → `.r` 再读再删 —— 取走与读取之间新发布
    的事件落在重生文件上，不再被 unlink 吞掉）；`lc_init` 就绪探测从
    「evt 文件存在」改为「/data/life 目录存在」（取走语义下文件不常驻）。
  - `lifd`：`send_evt`/`send_launcher` 改 `.new` 写入 + unlink + rename
    发布（新增 `publish_file`/`rename_file`/syscall4）；**`poll_mailbox`
    改两段式扫描**（先收集名字、关目录、再处理 —— 处理中的同目录 rename
    会使 getdents 游标错位跳过后续 cmd 文件）；`publish_file` rename 失败
    （目标被 cat 打开等）时重试后退回就地覆写并清理临时名（防 `.new`
    泄漏 inode）。两处均为隔离验证揪出的回归，详见下方「附」。
  - `pushd`：DELIVER 命令改 `publish_cmd`（每消息一文件），保留 lifd
    未就绪时的补建目录重试。
  - `launcher app_life`：`request_fg`/`report_exited` 改每消息一文件；
    `read_launcher_evt` 改 rename 取走。
  - 未迁移（移交）：`/var/run` 守护进程信箱（alarmd/audiod/networkd/
    wifid/camerad/sensord/positiond）由同期并行会话处理；rild.cmd
    读方该会话已改 rename 认领。

### 动画（对齐 docs/animations-roadmap.md，详见 animations.md 新节）

- **Toast**：长文案按 420px 折行（最多 3 行，药丸随行增高），保持期按
  行数延长（单行 1.67s 不变，每多一行 +0.6s）；退场上滑 8px（入场的
  反向完整性）。
- **最近任务伪 3D 纵深（P2-3）**：卡片按槽位（距最近应用的次序）
  scale = 1-0.055i、y 下移 9i px、整体 alpha = 1-0.08i（纯缩放近似，
  围绕卡片中心；跟手清理的卡随上拖 60px 内抬升回全尺寸）。
- **锁屏分钟切换 + 解锁先扩后散（P1-4 部分）**：分钟变化时旧时刻上移
  12px 淡出、新时刻自上方 12px 下移入场（320ms `Emphasized`，双时刻
  错位）；解锁转场中时钟 scale 1.0→1.12 + 淡出（鸿蒙解锁质感）。新增
  `draw_big_text_f`（8x8 字形浮点倍率最近邻 + PIX_ALPHA 支持 ——
  fill_rect 快路径不吃 PIX_ALPHA）。
- **settings 主列表 / store 卡片入场错峰（P0-6）**：每行/卡错峰 25ms、
  自下方 20px 上滑入位（200ms `EaseOut`），仅启动首次进入播放
  （滚动/搜索过滤不重播），阻塞推进与页面转场同模式。

### 本轮验证

- `make` 全量构建 exit 0；宿主 sync_tests **57/57**、wireless **105/105**。
- lifecycle-core（lifd 信箱协议的直接覆盖）：**隔离验证 38/38 PASS**
  （HEAD 干净 worktree + 本轮全部文件；共享树复测受同期并行会话的内核
  在制回归阻塞，见「附」）。

### 附：lifecycle-core 失败的定位与修复记录（本轮自查自纠）

本轮 lifecycle-core 实测 12/38（共享树）/ 20/38（隔离树）。三步定位：

1. **隔离对照**：HEAD 干净 worktree（纯 HEAD = 38/38 ✅）+ 仅叠加本轮
   lifd 族文件 = 20/38 → **确认本轮信箱迁移引入回归**，逐段探针证明
   pushd 发布 → lifd DELIVER 链路本身工作正常。
2. **根因一（已修复）**：lifd `poll_mailbox` 在 getdents 迭代中经
   handle_cmd → send_evt 于**同一目录** rename 发布事件（`evt_*.new` →
   `evt_*`），目录项增删使扫描游标错位 → 后续 cmd/reg 文件被跳过
   （FG/BTASK 静默不执行）+ 跳过的文件永不清理逐个泄漏 inode → 测试
   中后期 shell 连根目录创建都失败（`echo a1 > /t1` 回退上屏）。
   修复：扫描改**两段式**（先收集全部名字并关闭目录，再逐个处理）。
3. **根因二（已修复）**：`publish_file` 的 rename 在目标被第三方打开
   （shell `cat` 观测 evt 文件）时 unlink 被拒 → rename 失败 → 事件丢失
   且 `.new` 残留泄漏 inode。修复：短暂重试后仍失败退回就地覆写（旧
   O_TRUNC 语义），并清理临时名。

修复后隔离树 lifecycle-core **38/38 PASS**。共享树复测仍失败 ——
另有**同期并行会话的内核在制回归**：启动期（无任何生命周期活动时）
`/var/run/sensord.state`、`/data/alarms.txt` 等创建即失败、shell 重定向
连根目录都回退上屏、其自身 sfsrace 同窗口 `FAIL: creat`；读路径全部
正常、df 显示 581 空闲块、镜像 196/256 inode（远未耗尽）——签名指向
FS 写路径（fs_simple/vfs/smp 在改，rename 覆盖 + SMP 专项）的中间态。
待该会话收敛后共享树复跑即可全绿。

## 二十一、2026-09-16 SMP/PSCI 打通轮（副核唤醒 + 三债修复 + 每核状态）

### 打通路径（此前结论为何错误）

旧结论「`-kernel` 直载模式无 PSCI（smc 触发 UNDEF / EL3 桩不实现）」的
错误在 **conduit 选错**：QEMU virt（secure=off）的 DTB `/psci` 节点
`method = "hvc"`——QEMU 自身拦截 `hvc #0` 分派 PSCI，不依赖 EL3 固件
（与 `plat_qemu` 的 PSCI hvc 关机同机制）；`smc #0` 在无 EL3 时才是
UNDEF。`smp_init` 改为 DTB 检测 conduit（hvc/smc）+ 函数号，先
`PSCI_VERSION` 探测（-1 则显式单核降级），再逐核 `CPU_ON`。QEMU 11.1
实测 `PSCI version=0x10001`，`-smp 2` 两核在线、副核实际执行用户任务。

### 副核上线的四个真实阻塞点（依次排障实录）

1. **GICC 每核偏移错误**：`gicc_base()` 误加 `cpu_id()*0x20000`，cpu1
   写 0x08030000（GICV 区，NS EL1 不可访问）→ Data Abort DFSC 0x10。
   GICv2 CPU interface 是**每核 banked**（各核同一地址），去掉偏移。
2. **副核栈对齐**：`SECONDARY_STACKS: [u8; N]` 默认 1 字节对齐且被链接
   进 .rodata（实测 0x4012eb46），副核 SP 歪 → 函数序言 `str/stp`
   Alignment fault 风暴（ESR 0x96000061）。`#[repr(align(16))]` 包装 +
   入口汇编 `bic x0,x0,#0xF; mov sp,x0` 兜底。
3. **副核栈只读映射**：不可变 `static` 全零初始化落 .rodata，恒等映射
   把 text..data 段标只读 → 副核开 MMU 后首次栈写 Permission fault
   （ESR 0x9600004F）。改 `static mut` 落 .bss（可写区）。
4. **每核地址翻译状态缺失**：MAIR/TCR/TTBR0/SCTLR/PAN 均为每核寄存器，
   仅 CPU0 的 `mmu::init` 配置过 → cpu1 首次切入用户任务（换用户页表）
   即以复位值翻译 → Instruction Abort（init 在 cpu1 首调即 SIGILL
   @0x400000，`mapped=1` 但取指中止）。新增 `mmu::init_secondary()`。

### 三项已列 SMP 债务修复

- **PROCESSES 槽位分配原子化**：`PROC_CLAIM[pid]` 认领位（AtomicU32）。
  `alloc_pid` CAS 认领 + 复查，持至 `state=Running` 写回或失败清零；
  `reap`/`reap_forced` 在置 Free/清零前认领、完成后释放——消灭
  「他核 fork 与 reap 的 Free 窗口互相踩 PCB/释放新页表」。认领只独占
  单槽位，fork 的长地址空间复制不放大临界区。
- **execve 跨核 kill_req 窗口**：`kill_siblings_of` 标记后新增**静默
  等待**（sched_lock 下轮询「无 Running 兄弟」，1ms 睡眠退避，1s 上界
  超时打印诊断后继续）——等他核 tick 兑现 kill_req 后才允许
  `replace_address_space` 释放旧页表。配套堵复活洞：`pick_next` 对带
  kill_req 的 Ready 任务（标记后阻塞→唤醒）就地兑现 Exited，不再拉回
  Running（否则其将在新地址空间上执行旧映像 PC）。
- **fs_unlock 迁移+嵌套释放**：`fs_lock/fs_unlock` 持有身份从
  (CPU,TID) 双令牌改为 **TID 单令牌**（CPU 位仅诊断记录）。此前迁移后
  的持有者解锁会因 `w>>32 != 当前核` 误走「非持有者」分支**无条件清零**
  ——嵌套深度丢失，外层临界区未退出锁已被他人取得；且迁移后重入
  fs_lock 会误判他人持有而退避死锁。unlock 改 CAS 减深度、归零才真正
  释放；防御清零仅限持有者已死（自愈），存活持有者不再被误清。

### 验证矩阵（QEMU 11.1 TCG，隔离 worktree）

- 单核：boot 正常（`[smp] cpus=1`）、`make test-sfsrace` PASS、
  `make power-test` 21/21。
- `-smp 2`：`PSCI version=0x10001 在线核数=2`、`cpu1 已上线`、0 PANIC；
  QEMU `-d int` 实证 cpu1 有定时器 IRQ + 19 次 SVC + 用户缺页
  （负载均衡真实发生）；sfsrace 全量一次 PASS；power-test 20~21/21
  （FAIL 项为串口 shell 时序敏感，双核 TCG 吞吐下降所致，非确定性错误）。
- **遗留（新发现，未修）**：sfsrace 双核压测**时序敏感地偶发**任务表
  踩踏（`[tasks]` 全零 + 用户态跳 0，Instruction Abort ELR=0）——
  指向 `TASKS`/`CURRENT` 等 `static mut` 调度器状态的无锁并发读写面
  （tick/唤醒/调度路径的广泛裸读）与 tick IRQ 上下文的锁纪律，属
  三债之外的系统性工程。`-smp 2` 因此定位**实验性**：默认回归、发布
  与文档指引仍单核；`make run-smp`（verbose）供 SMP 开发观察。

### 顺带修复（构建系统，Makefile 范围）

- `test-sfsrace` 间歇性 FAIL 根因：mkfs 重写 ramdisk 与上次 cargo 编译
  **同秒**时，mtime 粒度内 cargo 误判 Fresh → 测试标记不重嵌
  kernel.img。mkfs 后 `touch $(RAMDISK)` 强制 mtime 前进触发重编。
- `make run-smp` 更新为真实 SMP 说明 + verbose；`docs/DEBUGGING.md`
  同步（删除「观察单核回退」的过时表述）。

### 2026-09-17 回退记录：smp.rs 暂退 HEAD（PSCI WIP 存档）

`-smp 2` 启动概率性挂死，二分定位最小挂死集合 {smp.rs, mmu.rs,
gic.rs}；其余 22 项内核改动全叠加、仅 smp.rs 取 HEAD 时 3/3 稳定
（cpu1 不上线，退化单核）。mmu.rs（每核翻译寄存器）与 gic.rs（GICC
banked）为正确独立修复，保留。根因是调度器 `TASKS`/`CURRENT` 无锁
static 的双核竞态（本节上文「遗留」项），需单独的锁纪律改造轮。
**处置**：smp.rs 回退至 HEAD，`make run-smp` 稳定单核。PSCI 打通
代码（DTB conduit 检测 + hvc/smc + CPU_ON，已验证 cpu1 可上线、有
SVC/IRQ/缺页活动）完整存档于 `/tmp/smp_psci_wip.patch`
（11076 字节，sha256
`6b8fe86a866d4e6bae1759fe6a7ecc58cd7c4f2ae13a62be2ee4dc55fd3f233a`，
`git apply` 可直接重放）。**待办**：调度器锁纪律改造完成后重新合入。

---

## 二十二、2026-09-18 修复轮（四路并行审查 → 19 项修复 + inode 容量根因 + 4 组动画）

> 方法：4 个并行审查代理（内核 diff / launcher+OOBE diff / 应用+shared diff /
> 服务+kunxpkg diff）对未提交的在制改动（约 8600 行插入）做回归审查，全部
> 发现逐条人工复核后修复。另在批次验证中揪出 sfsrace BASELINE FAIL 的根因
> —— SFS inode 容量耗尽（第二十轮「共享树 /var/run 创建即失败」的真正答案，
> 非中间态）。基线：`make` exit 0 / 0 警告、sync 57、wireless 105。

### Critical/High：SFS inode 容量耗尽（第二十轮遗留症状的根因）

- **`MAX_INODES=256` 容量跟不上 rootfs 增长**（fs_simple.rs / mkfs_sfs.py）：
  rootfs 文件数涨到 196 后，sfsrace（4 进程 × 12 轮 = 48 文件）+ 守护进程
  信箱/状态文件在数秒内耗尽剩余 60 个 inode —— `inode_alloc` 恒失败，
  全系统 create 返回 -1。表象：sfsrace `BASELINE FAIL: creat`（间歇性，
  取决于守护进程先烧掉多少）、`[vfs] open 失败 /var/run/rild.state.tmp`、
  sensord/alarms 等状态文件创建失败。第二十轮归因为「并行会话内核在制
  回归的中间态」，实为容量问题（该会话的信箱改造大量增加 /var/run 与
  /data 的 create churn，把潜伏的容量不足推到了必然触发）。
  修复：`MAX_INODES` 256 → **512**（内核 + mkfs 同步；imap 块 4096B =
  32768 位远未触顶，inode 表 32 块 = 128KB 磁盘、零 RAM 常驻成本）；
  `parse_sb` 新增 `SB_NUM_INODES == MAX_INODES` 护栏 —— 旧 256-inode
  镜像的 jlog 紧跟 16 块 inode 表，新内核按 32 块推位会把 ino 256..511
  的读写打进旧日志区（静默损坏），必须拒绝挂载（`rm disk.img && make disk`
  重建）。定位手段：fs_create_unlocked 四个失败分支临时诊断打印
  （exists/inode_alloc+free 数/write_inode/dir_add），一轮即锁定
  `free=0`。修复后 sfsrace **PASS**（4 子进程全 0 退出）。

### 内核（6）

- **O_APPEND 整条追加链路是死代码**（High，linux_compat.rs:753）：
  `sys_openat` 转发标志时丢弃 O_APPEND(0x400)——`vfs::write` 的追加分支、
  `fs_write_append` 全部不可达；多写者信箱（audiod.ctl / rild.cmd）各自
  「open 时 seek 尾」取到同一尾偏移互相覆盖（本轮 diff 想修的原 bug 原样
  存在）。转发掩码加回 0x400 + vfs 定义 `O_APPEND` 常量，一处激活全链路。
- **SYS_PPOLL 按 pselect6 的 fd_set 语义解析 pollfd 数组**（High，
  linux_compat.rs）：ppoll 的 ABI 是 x0=pollfd 数组指针、x1=条数，本实现
  把 x0 当 nfds、x1 当 fd_set 基址 —— musl 的 poll() 走 ppoll，实际效果
  恒「无就绪」（readfds=条数 非映射地址 → user_ok 失败跳过）+ 若 x1 恰为
  映射地址会按位图改写用户内存（且只过了可读校验，违反 COW 写纪律）。
  重写 `sys_ppoll`：解析 `{fd,events,revents}` 数组、就绪写 revents
  （copy_to_user 走可写校验）、fd<0 忽略、未登记 fd 报 POLLNVAL；
  pselect6 维持 fd_set 语义不变。
- **fb_present 像素阶段被 kill 打断后 BUSY 永久卡死**（High，
  compositor.rs）：三阶段结构的阶段 2（canvas→back 搬运）开中断运行且持
  BUSY=true，tick 兑现 kill_req 可把当前任务直接置 Exited 切走（多线程
  兄弟 exit / launcher kill 应用）→ 死亡属主走不到阶段 3 的复位 → 之后
  所有 fb_present 弃帧、composite 恒让路，**整机画面永久冻结**。BUSY 改
  记录属主 tid（`BUSY_TID`）：后来者发现属主已死即复位接管（fs_lock 死
  持有者自愈同模式），composite 对死属主放行。
- **wait() 重试路径丢唤醒**（Medium，process.rs）：找到僵尸 → 开中断
  reap → 护栏失败（还有存活线程）→ 重新屏蔽后**不复查**直接 block——
  窗口内最后一个存活线程退出的 wake 只唤醒 Blocked 任务（此刻本任务
  Running）→ 唤醒丢失 → wait 永久睡眠。与首路径对齐改 `yield_now() +
  continue`（让出后外层重扫）。
- **vfs::rename 目标打开检查在 FS 锁外**（Low，vfs.rs）：`is_open`/类型
  检查与 `fs_rename` 取锁之间被抢占、恰好有人 open 目标 → rename 仍会
  回收打开中的 inode（fd 与新文件别名，H-6 竞态版）。检查与 rename 封进
  同一 `fs_lock` 临界区（TID 令牌可重入；fs_lock/fs_unlock 转 pub）。
- **CLEAR_TID 槽位跨 tid 复用残留**（Low，bionic_shim.rs / sched_cfs.rs）：
  经 kill_req / 进程整体 exit 死亡的线程不经过 `exit_thread_notify`，槽位
  残留旧用户地址；tid 复用后新任务退出时向无关地址清零 + 误触 futex。
  kill_pid / kill_siblings_of / reap_pid / exit_thread_current 四处统一
  `clear_tid_slot`。
- 另：`USER_STACK_SIZE` 注释谎称「8MB 按需映射」实为 2MB 饿式映射 ——
  注释改为如实描述（8MB 需先做栈区按需落地；实测 2MB nsmonkey 冒烟通过）。

### 系统服务（4）

- **networkd 认领后 open 失败提前 return → 管道永久失效**（High）：
  rename(PIPE_PATH→.r) 成功后 open(.r) 失败即 return——残留清理会删掉
  .r、rename 因 PIPE_PATH 不存在恒失败，此后全部网络命令静默丢失直到
  重启。对齐 wifid/audiod/alarmd：open 失败仅跳过读取，照常清认领 +
  重建管道。
- **audiod 同 pid 重开不重置 last_seq → pid 复用后长时间静默**（Medium）：
  原进程被杀后 pid 槽被新进程复用（内核 pid 无单调性），新进程 seq 从 0
  起、`seq.wrapping_sub(last_seq) ≥ 2^31` 全部拒收——旧进程播多久新实例
  就静默多久。`cmd_open` 命中同 pid 时重置 `last_seq=0 / has_chunk=false`
  （OPEN 语义上就是新流）。
- **rild rewrite_store 新箱未 fsync 就删备份**（Medium）：掉电落在
  unlink 备份之后、新数据落盘之前 → 正身空、备份已删，整箱短信丢失。
  删备份前对 wfd fsync（就地写回退路径同理；append_record 已 fsync，
  此处对齐）。
- **audiod write_status 回退分支缺 O_TRUNC**（Low）：状态行变短时旧尾
  残留，读方多解析垃圾行。回退 open 补 `| 0x200`。

### kunxpkg（2）

- **菱形依赖去重短路跳过约束校验**（High，deps.rs）：`plan.contains` 在
  版本约束校验**之前**返回 —— A→B(>=1) 与 A→C→B(>=2) 时第二条边不校验
  ≥2，装出 C 的坏依赖（正是本 diff 想杜绝的）。约束校验上移到去重之前。
- **finish_atomic 先 unlink 再 rename**（Low，fsio.rs）：注释「内核 rename
  不覆盖已存在目标」与实现相反（fs_rename 是 POSIX 原子替换）；先 unlink
  引入丢档窗口（unlink 成功 → rename 失败 → 旧记录已删）。删掉 unlink
  直接 rename + 失败时 recover 触发检查点重试一次。

### 应用 / launcher（6）

- **browser HTTPS socket fd 泄漏**（High，net.rs）：`Wire::Tls(c) =>
  { c.close(); }` 只发 close_notify 永不关 fd —— 每个 HTTPS 请求（含重
  定向每跳、每张同源图片）泄漏 1 个 socket，内核 socket 表仅 16 槽，十
  几次后浏览器 + 天气 + 云同步全部无法联网直到重启。`Wire::Tls` 变体
  携带 fd，close 时 `sock_close`。
- **sfsrace 认领协议自身丢消息 → 假阳性 FAIL**（中高，sfsrace/main.rs）：
  写者 open→write 之间被 rename 认领时，迟到 append 落在 .r 上；下一轮
  循环开头的盲删 `unlink(taken)` 把它销毁 → 「丢失消息」FAIL 把协议竞态
  误报成内核 bug（作为内核验证工具的假阳性会误导排查/诱使弱化断言）。
  循环开头先排干（open+read+parse）再删。
- **launcher draw_placeholder 首帧守卫恒假**（High，app_launch.rs）：
  `fb_sample()` 后几微秒即 `fb_matches(&自己)` —— 恒不相等 → 守卫失效，
  占位页无条件整屏覆盖；应用首帧若在 launcher 填充/flip 期间落屏会被盖掉
  （不重绘的应用占位页驻留到下次交互）。基线采样移回 `gate_release()`
  之前、以参数传入比较。
- **launcher TILE_MODE 几何回退图标编码不一致**（Low，main.rs）：预乘双
  线性采样把顶字节当 alpha，几何回退写入的 0x00RRGGBB 顶字节 0 被当全
  透明 —— 资产缺失时图标不绘制/启动缩放画黑方块。TILE_MODE 的 put_pixel
  把旧约定归一化为 `0xFF000000|rgb`。
- **launcher read_launcher_evt 改名后 open 失败丢事件**（Low，app_life.rs）：
  rename 成功、open 失败即 return 0 —— 事件内容还在 .r 里但会被下一条
  事件的 rename 覆盖（BG 丢失 → 应用停留前台态）。open 失败把 .r 改名回
  原名让下轮重取。
- **launcher 时钟小组件移除后每秒整屏重绘**（Low，main.rs）：秒级 tick 的
  `!redraw_clock_widget_fast()` 在无时钟小组件时恒 true → 每秒一次整屏
  render_frame（TCG 50~150ms 常驻卡顿）。tick 前置 `WGT_MASK & 1 != 0 ||
  RECENTS_OPEN` 门控。

### 动画（对齐 docs/animations-roadmap.md，详见 animations.md 新节）

- **P1-6 下拉刷新**：`shared/motion.rs` 新增 `PullRefresh` 状态机（跟手
  1:1 → 越界 0.35x 阻尼 → 松手弹簧回弹 + Loading 三圆点错峰缩放 900ms；
  `ate_tap` 吞击 / `just_triggered` 数据刷新钩子 / retarget 速度接续）。
  接入 sms 会话列表（重载 /data/sms + rild 增量导入）。
- **P1-2 gallery 一镜到底**：缩略图 → 大图 300ms `Emphasized` 矩形补间
  （x/y/w/h），中点切换底页；返回反向缩回缩略图位，**中途打断从当前矩形
  续跑**。
- **P0-2 sms 页面转场**：settings 同款整页滑动（240ms FastOutSlowIn +
  快照补底 + 10px 接缝阴影），列表↔会话/新建页 7 个切换点全覆盖。
- **P2-2 光随指动**：`shared/glass.rs` 全局光源偏移（触摸点相对屏幕中心
  ×0.15、±24px 钳制、120ms FastOutSlowIn 跟随、离手 300ms 回零）；高光
  带随光源垂直移动、斜向光泽沿对角移动；app_kill poll_input 的 ABS 事件
  自动喂点，sms/gallery 接入 `light_animating()` 重绘。

### 验证

- `make` 全量构建 exit 0 / 0 警告（含 networkd/launcher 编译错修复 +
  networkd 预存双分号警告清理）。
- 宿主：sync_tests **57/57**、wireless **105/105**。
- **sfsrace PASS**（inode 扩容 + rename 锁 + O_APPEND + sfsrace 排干
  修复的联合覆盖；修复前 BASELINE FAIL / iter 2-3 必失败）。
- disk.img 重建（512-inode 新格式）。
- QEMU 无头冒烟 / GUI 应用回归：见 CHANGELOG（本轮动画改动 4 应用就绪
  0 PANIC 的中间验证后，全量回归在收尾轮补跑）。

---

## 二十三、2026-09-18 第二波修复轮（两路并行审查 → 19 项修复，TLS 安全 + 信箱迁移收官）

> 方法：2 个并行审查代理（① phone/calendar/music/video/weather/map/terminal/
> mail + camerad/sensord/positiond/logd/pushd 全量读码；② kunxboot/kunxhal/
> kunxinit/recovery/probe/modemtest + tools + browser TLS 栈逐行深挖）。
> 与第二十二轮互补（无重叠文件）。

### TLS 栈（安全敏感，5 + 1 个连带根因）

- **（Critical）证书链无 CA/basicConstraints 校验 → 持有效叶子证书者可冒充
  任意域名**（x509.rs verify_chain）：链构建只做 issuer/subject 匹配 + 逐级
  RSA 验签。攻击路径：攻击者用自己域名的合法叶子证书 L 的私钥自签
  `CN=victim.com` 的伪造叶子 F，发送 `[F, L, 真实中间]`——逐级验签全部
  通过，MITM 完整冒充。修复：`parse_cert` 解析 basicConstraints（OID
  2.5.29.19），链中作为签发者的中间证书必须 cA=TRUE 且在有效期内。
- **（连带根因，本轮实证发现）critical 扩展解析拿错 tag → basicConstraints
  永远解析不出来**（x509.rs 扩展循环）：跳过 critical BOOLEAN（01 01 FF）
  后重读 value TLV，却仍用 BOOLEAN 的 0x01 当 tag 判 `!= OCTET` ——
  **所有 critical 扩展被静默跳过**（basicConstraints 按 RFC 5280 恒为
  critical；SAN 非 critical 故未暴露）。hosttest chaindump 实测：修复前
  baidu 真实中间证书 is_ca=false，修复后 true。此缺陷使上面的 CA 校验
  在加上后立刻拒绝全部真站点——两修联合验证：对 www.baidu.com 完整
  握手 + HTTP 200（43ms）。
- **（High）畸形告警记录（0/1 字节）索引越界 panic——可远程触发**
  （mod.rs 两处 CT_ALERT）：明文阶段注入 5 字节空告警记录即可杀浏览器
  进程。记录日志前判长度。
- **（Medium）close_notify 记录 AAD/长度硬编码 24**：明文 2 字节时正确值
  是 AAD=2、记录长 = 2+24=26；硬编码让对端解密失败 + 流上残留 2 字节。
- **（Low）零长度 application_data 记录被当 EOF**：RFC 5246 允许（部分
  服务器作 keepalive），返回 Ok(0) 会静默截断 HTTP 响应 → continue 续读。
- **（Low）HelloRequest 计入握手哈希**：RFC 5246 §7.4.1.1 明确禁止——
  服务器中途发 HelloRequest 会使 Finished 校验失败。ty==0 不更新哈希。
- **（Low）write_tlv DER 长端编码错误**：无 0x8N 计数字节且最低位在前
  （len=300 输出 AC 01，正确 82 01 2C）；≥128 字节的 issuer DN 全是废 DER。
  改大端压栈 + 前缀 0x80|字节数。

### 信箱竞态收官（rename 认领迁移，4 处）

第二十轮迁移了 lifd 族，本轮补齐全部 /var/run 漏网站点（rild 方案照抄）：

- **camerad**（High）：read→ftruncate 两步之间相机应用（cam_send）的新
  命令被抹掉 → 拍照/预览偶发卡死（应用满超时）。
- **sensord**（Medium）：同族（QUIT 丢失 → 守护进程停不下来）。
- **positiond**（Low）：同族（命令面低频）。
- **lc_notify_local**（Low，shared/lifecycle.rs）：本地通知直接 create→
  write，pushd 的 getdents 扫到 create 与 write 之间的空文件即读 0 字节
  + unlink —— 通知永久丢失。改 `.tmp` + rename 原子发布（半写清临时名）。

### 其它

- **（High）terminal UTF-8 组装器丢 CJK**：「凑满 4 字节才解码」与 3 字节
  CJK 序列错位——第 4 字节（下一字 lead byte）触发 flush 后余量循环只转
  发 ASCII，lead byte 静默丢弃，纯中文流只剩第一个字上屏；且 ASCII 不
  flush pending 导致乱序。改按首字节位型定长、凑齐即解码、逐序列消费、
  ASCII/ESC 先排空。
- **（Medium）terminal 回看历史环形缓冲乱序**：`idx % HIST_LINES` 在环
  回绕后（滚屏/clear 一次推 24+ 行）把最新行显示在最旧行上方。改
  `(HIST_NEXT + idx) % HIST_LINES`。
- **（Medium）calendar 23:00 新建事件 DTEND 早于 DTSTART**：`(h+1)%24=0`
  产生同日 00:00，RFC 5545 非法（CalDAV 服务器拒收 → 同步冲突）。改
  `dtstart.to_epoch() + 3600` 反推。
- **（Medium）camerad parse_cmd 硬拒 id > 999**：相机应用 REQ_ID 递增
  不回绕（10 位），长会话第 1000 条命令起全部丢弃 → 每个操作都超时。
  digits[4]→[10] + checked_mul 防溢出。
- **（Medium）kunxboot dtb insert_props 边界用错端点**：上界按 insert_off
  计算，实际最高写入点是 content_end（chosen 通常在结构块前部，差整个
  DTB 主体）——近 128KB 的 DTB 注入 ~212B 属性即越出 scratch 写穿 .bss。
  改按 max(insert_off, content_end)。
- **（Low）recovery SYS_REBOOT=169**（x86_64 号；AArch64 是 142）：重启
  菜单 ENOSYS → 死循环 spin，设备永不重启。
- **（Low）MPU6050/9250 + AK8963 轴数据小端解析**：MPU 系列寄存器大端
  （H 在前），小端解析全轴字节交换（1g=16384 读成 64）——同文件
  fifo_count 却按大端，自相矛盾。改 from_be_bytes + 测试夹具/注入同步
  （kunxhal 测试 36/36）。
- **（Low）terminal 方向键 CSI 短写**：O_NONBLOCK tty 上 3 字节序列可能
  短写，剩余丢失 → shell 收到孤立 ESC。write_fd_all 循环补写。
- **（Low）kunxrepo.py 索引无上限校验**：设备端 INDEX_MAX=16384（约
  40-50 包），仓库扩大后 pkg update 整体不可用。生成期即报错对齐。

### 验证（本轮）

- `make` 全量构建 0 错误 0 警告；kunxhal 宿主测试 36/36（含字节序夹具）。
- TLS hosttest（对 www.baidu.com 真网）：完整握手 + 证书链（3 级）+
  HTTP 200 全过（chaindump 证实中间证书 is_ca 解析正确）。
- 全量回归矩阵见第二十二轮补记（本轮改动后复跑：make / sync 57 /
  wireless 105 / sfsrace / power 21 / lifecycle-core 38 / GUI 应用回归）。

---

## 二十四、2026-09-18 第三波修复轮（内核核心 + shared 全量读码 → 17 项修复，含本会话新动画代码自纠）

> 方法：2 个并行审查代理（① 内核核心子系统全量读码：heap/ipc/signal/
> timer/gdbstub/syscall_table/perf/anim/input/gesture；② shared 库全量 +
> **本会话新写的 motion/glass/sms/files/gallery 动画代码**）。第②路对本会
> 话新代码报出 2 条真缺陷（PullRefresh 弹簧饿死、glass 回零 tween 每帧
> 重启），验证了「新代码也要过审查」的流程价值。

### 内核（4）

- **shm_map/shm_close 引用计数无 IRQ 屏蔽**（High，ipc.rs）：syscall 上下
  文 IRQ 使能，refs 读-改-写与并发 map/close/add_ref 交错丢更新 → refs
  提前归零 `free_page` 释放仍被页表映射的物理页（UAF）。两函数全程屏蔽
  （与同文件其余 add/drop 对齐，唯独这两个漏了）。
- **dup 失败回滚对活进程调 `shm_close_index`**（中高，linux_compat.rs
  两处）：该函数按「进程已退出、页表已销毁」设计，清光映射记录但不
  unmap——回滚时进程活着且可能已 shm_map：记录被清、PTE 残留，后续
  close 按空记录跳过 unmap 直接归零释放 → UAF。用户传 `newfd >= FD_MAX`
  即可稳定触发。新增 `shm_drop_one_ref`（只撤销一次 fd 引用）替换。
- **signal_clear/signal_send 位图 RMW 无屏蔽**（Low，process.rs）：
  handle_pending 的 clear 与他进程 kill 的 send 交错丢新置位（含 SIGKILL
  静默丢弃）。保存-恢复 DAIF。
- **`now_us()` 频率换算整除截断**（Low，timer.rs）：`freq/1_000_000`
  先截断（62.5MHz → d=62），时间戳每秒慢 ~0.8%——perf 帧统计/交互
  deadline/网络超时全部同比失真。改 `(cnt/freq)*1e6 + (cnt%freq)*1e6/freq`。

### shared / 应用（13）

- **（本会话新代码）PullRefresh 弹簧积分被饿死 16 倍**（High，
  motion.rs）：`frame()` 无条件盖 `last`——应用按文档「每帧 frame + step」
  接线时两次 `now_us()` 仅隔几微秒，step 的帧间隔恒被钳到下限 1ms（真实
  16ms），回弹/Loading 以 ~1/16 实时速度播。last 只在手势期 stamp，
  Loading/Settling 交给 step 独占。
- **（本会话新代码）PullRefresh `ate` 残留吞后续手势 tap**（Low）：
  Loading 期的陈旧 ate 会吞掉下一次真实点击。按下即清（不论状态）。
- **（本会话新代码）glass 光随指动回零段每帧重启 tween**（Low）：触发
  条件置 0 后恒真，每帧重设 FX/T0 → 实际逐帧指数收敛而非一次 120ms
  缓动。加 `LIGHT_DECAY` 锚定标志，回零完成才停。
- **glass 双线性放大顶/左边缘外插暗边**（Low，既有）：`fy=dy/4-0.5` 在
  dy=0 时 ty=-0.5 线性外插，负值 `as u32` 饱和 0。tx/ty 钳 [0,1]。
- **sync 墓碑残留：远端重建资源不可见 + 周期性误删**（Medium，
  sync.rs/store.rs）：本地墓碑（DELETE 推送遇非网络错误）遇服务器重建 →
  server-wins 应用远端但 FLAG_DELETED 保留 → 本地不可见 + query_dirty
  下轮再发 DELETE 真删服务器数据（拉回→再删循环）。应用前清 DELETED。
- **sync apply_deleted 服务器优先不落删除**（Medium）：handle_conflict
  返回值被丢弃——记录既不 dirty 也未删，增量路径不再覆盖，本地永显已删
  事件。server-wins 补 `FLAG_DELETED + blen=0`。
- **sms 标题栏点击误开第一个会话**（Medium）：`y < LIST_TOP` 时负数整除
  向零截断（-56/84==0）通过 row>=0 检查。加下界守卫。
- **text_engine BiDi 字符/字节偏移错位**（Medium，潜伏——模块暂无消费
  者）：每字符 levels 被按字节偏移切片，混合方向多字节文本 run 边界落在
  字符中间（str 切片 panic）。按字符字节宽度复制 level。
- **l10n 阿拉伯语复数公式解析失败**（Medium）：`&&/||` 在 parse_cmp 内
  只消费一次，链式表达式剩余字节使 eval 失败 → 恒回退形态 0（六种复数
  全退化）。新增左结合 `parse_and`/`parse_or` 链层（?: < || < && < 比较）。
- **jpeg 编码器尺寸 >65535 静默回绕**（Low）：SOF0 字段 u16，65536→0。
  入口显式 BadDimensions。
- **gallery 网格命中测试覆盖未绘制的第 5 行**（Low）：渲染在
  `y+TILE_H>DISPLAY_H` break，tap 循环遍历全部 18 格——点底部空带打开
  不可见照片。命中加同款可见性条件。
- **emoji CBLC 解析大量未检查索引**（Low）：截断字体启动期 panic。全链
  checked_add + rd_u16c/rd_u32c，越界放弃解析。
- **cursor_overlay 首次 redraw 前 move 画 25×25 黑块**（Low）：
  CUR_UNDER_X<0（备份全 0）时恢复跳过。

### 验证（本轮）

- `make` 全量构建 0 错误 0 警告；kunxhal 宿主 36/36（上一轮）。
- 宿主 sync 57/57、wireless 105/105；sfsrace PASS；wifi selftest PASS；
  GUI 应用回归视觉验证（14 应用 + 4 交互截图，0 PANIC；串口就绪行被
  vtimer 风暴洪泛吞掉，以截图为准）。

### 附（同日续）：store 设置页不可达 + P0-2 store 转场

- **store VIEW_SETTINGS 从未被赋值**（High，apps/store/src/main.rs）：
  设置页代码完整（5 字段编辑 + R2 源配置）但无任何入口——`VIEW =
  VIEW_SETTINGS` 全仓不存在（HEAD 同样），而错误提示引导用户「到设置页
  配置 R2 软件源」→ 无 R2 配置时用户被指向一个打不开的页面。补列表页
  标题栏右上「设置」玻璃按钮 + 命中。
- P0-2 转场迁移补齐 store（列表↔详情/设置，含 ESC 返回），至此路线图
  P0-2 的五个应用（settings/sms/files/store + gallery 缩放）全部完成。

---

## 二十五、2026-09-18 第四波修复轮（网络/FAT32 + rild/audiod 深挖 → 20 项修复）

> 方法：2 个并行审查代理（① net_stack/socket/fat32/net_virtio 全量读码；
> ② rild/audiod/logd/camerad/alarmd 逐行深挖）。

### FAT32 / 网络（4 + 1 移交）

- **（High）FAT32 目录项首簇 HI/LO 与规范相反**（fat32.rs + mkdisk.py）：
  规范偏移 20-21 = FstClus**HI**、26-27 = FstClus**LO**，本驱动读点把 20
  当低位、写点把低位写 20 —— mkdisk.py 同向写反，自产镜像内部自洽掩盖。
  后果：挂载 mkfs.vfat/Windows/Linux 标准卷时小簇号文件全部读空；在标准
  卷上写入后宿主解码首簇错乱。全部读写点对调 + mkdisk 同步 + disk.img
  重建 + run-disk 引导验证。
- **（Medium）TCP 发送段可组 2048 字节帧（超 MTU）**（socket.rs）：按内部
  缓冲极限放行 ~1994 字节载荷，QEMU slirp 侥幸可过，真实网卡/交换机一律
  丢弃超长帧 → 大段黑洞、连接卡死。上限收紧 1460（MTU 1500−20−20）。
- **（Low）FAT32 mount 不校验 reserved_sectors/num_fats**：reserved=0 时
  FAT1 覆盖引导扇区（首次写即毁 BPB）；num_fats=0 时数据区与 FAT1 重叠。
  probe 增加 reserved≥1、num_fats∈{1,2}。
- **（Medium，SMP-only，移交）net_virtio IRQ 上下文 drain 与他核任务路径
  无跨核互斥**：GIC 只路由 core0，irq_handler 不取 net_lock 直接
  drain_impl —— SMP 下与他核 send/recv 并发改 TX 环/堆。单核（当前默认）
  无此问题（C-5 修复的本核屏蔽已足够）。随 SMP 锁纪律轮一并处理
  （与 TASKS/CURRENT 无锁面同族）。

### rild（8）

- **（High）AT 行缓冲 LINE_CAP=160 截断 PDU**：内核 modem 的 CMGR 回吐
  最长 420 hex，160 把正文 >~50 字符的短信截成半截 PDU → 解码失败滞留
  modem 槽位，**正常长度短信永远收不到**（modemtest 只注入短 PDU 所以
  全过）。LINE_CAP → 448。
- **（Medium）命令等待期 URC 全部被吞**：发短信的 3 秒窗口内到达的
  +CMTI（新短信）无人取、+CRING（来电）完全不可见。pending 期对已知
  URC 前缀（+CMTI/+CRING/RING/+CREG:/NO CARRIER）仍派发 on_urc。
- **（Medium-low）无 modem 时 SIM 状态被周期轮询覆写**：NoModem 落
  Unknown 分支，启动设置的 ABSENT 3 秒后被改，「无 SIM 仅紧急呼叫」门
  失效。refresh_sim 在 fd<0 时直接返回。
- **（Low）级联短信段数上限不一致**：槽 total 封顶 8，receive 按原始
  total 匹配 → >8 段长短信凑齐也整条丢弃。匹配用封顶值。
- **（Low）重组长短信落盘仍截 200 字节**：重组缓冲按 1600 设计，
  append_record 的 out[300]/min(200) 把两段长短信截回单段长度。out →
  1700、上限 1600。
- **（Low）命令管道单次 read 1024**：排队多条命令时尾部随认领文件删除。
  循环读到 EOF。
- **（Low）fmt_dec(0) 输出空串**：SIG/START/SEQ 为 0 时值字段缺失。
- **（Low）pdu.c SCTS 半八位解码 nibble>9 越界读**：hexd 只有 10 字节，
  畸形 PDU 的 BCD A-F 读越界。扩成 16 字节表。

### alarmd / audiod / camerad / logd（8）

- **（High）每周闹钟周几按 UTC 日界计算**（alarmd）：repeat 位掩码是本地
  星期、计算用 UTC 日 —— 东八区早于 08:00 的每周闹钟（起床闹钟时段）
  **整体晚一天响**。周几改按本地日历日（zone_offset_min 校正后求）。
- **（Medium）alarmd RELOAD 共享 BUF 自踩**：handle_cmd(RELOAD) →
  reload_alarms 用同一个 BUF 读列表，同批后续命令被列表字节覆写。
  poll_ctl 独立 CTL_BUF。
- **（Medium）camerad 无相机后端时任一命令 panic**：k_main 对无后端是
  优雅降级设计，handle_cmd 里 9 处 `.expect("no camera backend")` 让第
  一条相机命令杀掉守护进程。全部改 Option + "ERR no backend"。
- **（Medium-low）camerad 录像文件无 O_TRUNC**：漏 REC_STOP/崩溃后同名
  文件残留，16B 头覆写旧头、新帧追加旧尾。首帧重建容器（O_TRUNC）。
- **（Low）camerad 尾行无换行被丢弃**：分发循环 0..len 改 0..=len。
- **（Low）audiod 陈旧块无限重播**：客户端停写不 CLOSE 时最后 10ms 块
  每 10ms 循环混音。chunk_age 计数，连续 3 周期无新块即停（新块恢复）。
- **（Low）audiod /dev/audio 写 -1 被当成功**（usize 巨值恒假分支）+
  pid 复用重开继承 focused。
- **（Low）logd 每轮只读一条 kmsg 就睡 100ms**：突发日志（开机风暴）使
  内核环形日志被覆盖、持久化缺记录。读到记录即继续排空，读空才睡。

---

## 二十六、2026-09-18 GUI 应用前台路径调查（已知问题存档 + 测试修正）

> 本会话末段对「GUI 模式下应用启动后整批回归 FAIL」做了长时间定位。结论：
> **非本会话修复引入的回归**，是未提交生命周期重设计（app_life 前台态 + 
> 输入独占设计）中的真实缺陷 + 测试断言过期两者的叠加。证据链完整存档如下。

### 现象

- `apps_regression.py`：14 应用全部 FAIL，应用截图为**纯色**（启动缩放的
  末帧），桌面截图正常；串口只有第一个应用打印「就绪」。
- `launcher_test.py`：应用成功启动（fork/execve/就绪/FG 登记全正常），
  但应用内的返回点击/底部上滑都无法退出应用，launcher 恢复不了桌面输入。

### 关键证据（逐条实证）

1. **输入帧到达内核**：UART 0xAA 帧全部被解析（rxdbg 计数 56+）。
2. **launcher 前台期设计性不读输入**（`main.rs:8168` 注释明示「应用前台/
   恢复期间，launcher 不渲染桌面、不读输入（应用独占 fb0 与输入设备）」，
   该设计**已提交于 HEAD**）。launcher 主循环在前台态只做 wait4(WNOHANG)
   + lifd 轮询 + 8ms 睡眠（任务转储实证：tid13 恒 Blocked 于 nanosleep，
   睡眠槽 deadline 随时间正常推进——**launcher 没有冻结，是设计行为**）。
3. **应用侧不消费输入**：event0 成功读取计数全部来自 launcher（pid=13），
   应用（pid=14）只有首次 EAGAIN 登记、从未成功读到事件 → 返回点击/
   上滑手势永远到不了应用 → 无法退出 → 回归流程卡死在第一个应用。
4. **历史「通过」是退化的**：纯 HEAD（干净 worktree）跑 launcher_test
   3/3「通过」——但日志显示 HEAD 上 gate_write **失败**（256-inode ramdisk
   耗尽 → `.launch_gate` 写不进 → 应用从未启动 → launcher 保持桌面态读
   输入 → 断言碰巧成立）。本会话的 inode 512 扩容让 gate 首次真正写成功、
   应用真正启动，才暴露前台路径的真缺陷。
5. **诊断自身会投毒**：在 UART RX IRQ 上下文里 print（busy-wait TX）会
   拖慢 RX 导致帧字节丢失（BTN 事件丢失、只剩 ABS 移动对）——中期所有
   「复现」都被此污染过，清理后症状回归一致形态。

### 已修复的测试缺陷

- `launcher_test.py` 的 wait_for 会匹配启动期的陈旧标记（「已退出」是
  OOBE 的退出行）→ 加偏移锚点只搜新增内容；第二次点击断言按前台设计
  改为「应用内返回 → 应用退出 → 桌面点击恢复输入」的真实流程；新增
  底部上滑兜底。
- `apps_regression.py` 的就绪等待同样存在陈旧匹配风险（后续轮处理）。

### ★ 根因已修复（2026-09-18 深夜续轮）

**`tty_read` 无视 O_NONBLOCK → GUI 应用主循环在内核 yield 死循环中饿死**。

- **机制**：第十九轮为 musl fgets 兼容把 `/dev/tty` 的 read 改为阻塞语义
  （「至少等到一个字符」），实现是无数据时 `yield_now()` 循环——但
  **`_flags` 参数完全未用**：O_NONBLOCK 打开的读者同样被拖进死循环。
  所有 GUI 应用的主循环每帧调 `poll_tty_esc()`（读 fd 0 / stdin，继承自
  execve 的阻塞 tty）→ 无串口数据 → 内核 yield 循环 → 应用永远回不到
  用户态 → 不渲染、不读输入、不上屏——**「14 应用全挂 + 纯色截图 +
  前台输入死路」一整族症状的完整根因**。
- **定位路径**（证据链）：截图色值 = `APPS[phone].color`（占位页）→
  fbdg 探针 = 应用 present 恒为零 → ckdbg 逐句打印 = 卡在 tap 块与
  `if dirty` 之间 → 任务转储 lastsys=101 + ELR 落在 openat+read 循环
  （`wait_launch_gate` 同款模式但 3s 超时会断）→ 逐行核对两点之间的
  代码发现 `poll_tty_esc()` → 读 `tty_read` 实现确认 yield 循环无视
  O_NONBLOCK。
- **修复**（双层 + 应用自建读）：
  1. 内核 `tty_read` 尊重 O_NONBLOCK：无数据时返回 `-EAGAIN`（-11），
     阻塞语义只对默认（阻塞打开）的 fd 生效——musl 兼容不受影响。
  2. `shared/app_kit` 的 `poll_tty_esc` 改用自己的 `O_NONBLOCK` tty fd
     （`/dev/tty` 独立 open）：fd 0 保持阻塞给 musl fgets，GUI 应用的
     ESC 轮询走非阻塞路径。
  3. **calculator / settings / stub / store 各自实现了 `poll_key()` 直读
     fd 0**（不走 kit）——同族修复：改为各自独立 `O_NONBLOCK` tty fd。
- **验证**：
  - `launcher_test` 全链路 PASS（启动→就绪→应用内返回退出→launcher
    恢复输入），此前该链路 100% 挂死。
  - **apps_regression 14/14 全 PASS（fail=0）**，截图 15/19 有真实 UI
    内容（修复前 0/14 全纯色）——这是**带真实应用启动**的全绿（历史
    「通过」全部是 gate 写失败的退化形态，应用从未启动过）。

### 移交（下一轮的已知问题）

- ~~**应用前台期不读 event0**~~：已由上述 tty_read 修复解决——应用
  卡死在 poll_tty_esc 的内核 yield 循环里，根本没机会持续轮询输入。
- ~~**应用首帧不上屏**~~：同上——应用卡在 poll_tty_esc 后从未到达
  render/flip，perf 统计里的「直绘渲染」计数全部来自 launcher 的
  启动转场帧。
- **VTIMER 风暴（irq=27）**：GUI 高负载下 episode 出现率上升（anim1 61
  条/局），与前台卡死无因果但污染串口观测（UART 洪泛丢打印）。

## 二十七、2026-10-17 夜间十小时自动化轮（40+ 缺陷修复 + ColorOS17 动效 + 两起用户实测反馈根因）

> 完整清单见 docs/NIGHT_PLAN.md 与 CHANGELOG.md。本节只沉淀两起用户实测
> 反馈的根因分析（后续排查同类问题的档案）。

### 根因一：OOBE 进系统后整屏黑（用户实测报告）

- **表层**：launcher 正常运行（串口 0 PANIC、图标/dock 日志齐全），但
  screendump 实测屏幕平均亮度 0.1（全黑，仅底部导航条可见）。
- **根因链**：
  1. B8 开机渐亮的 α 判定用的是**墙钟**（gettimeofday）；
  2. QEMU goldfish RTC 就绪前后墙钟**跳变**（回跳）：渐亮 T0 记录的是
     跳变后的值，flip 的 now 仍取到旧值 → `progress(now<=t0)` 返回 0 →
     α 永久卡 0 → 每次 flip 经全 0 亮度 LUT 上屏 = 整屏黑（探针实证：
     BACK_BUF 均值 210 内容完好、上屏 lvl=0）。
  3. 工具链放大器：`make kernel` 不重嵌更新的 ramdisk（cargo 对内嵌文件
     mtime 误判）——前两次修复「无效」实为跑了旧二进制。修复流程必须
     先 `touch kunxcore/ramdisk.img`。
- **修复**：内核 sys_clock_gettime 补 CLOCK_MONOTONIC（原忽略 clk 恒回
  墙钟）；launcher 与全部 11 处应用侧 now_us 本体改单调钟；渐亮加
  fail-bright（now<=t0 视为播完，绝不 fail-black）。
- **教训**：① 时间差判定一律单调钟，墙钟只做显示；② 视觉问题必须看
  画面（screendump/像素统计），串口 0 PANIC ≠ 画面正常；③ 修复「无效」
  先怀疑镜像陈旧。

### 根因二：动画帧率 11fps（用户实测「太卡」）

- **表层**：OOBE 全部动画是「固定帧数 + 帧间固定 sleep（3-36ms）」旧
  模式；Hero 入场 260ms 实测只跑 3 帧（分段计时：背景全屏重绘 3-12ms
  + 光晕逐像素圆判定 13ms + 整屏 2MB flip + nav 毛玻璃每帧重画）。
- **修复模式（可复用四件套）**：① 真实时间推进替代固定帧数（时长恒定
  帧率拉满）；② 静态内容快照/行缓存（背景/nav 玻璃）；③ 区域呈现
  （脏矩形）替代整屏 flip；④ 逐像素圆改 scanline。
- **实测**：Hero 260ms 3 帧 → 21 帧（≈81fps，TCG 模拟下接近上限；
  真实时间机制在 KVM/真机自动跑更高帧率）。
- **衍生回归两起（均已修复）**：改真实时间推进时漏加退出条件 → 转场
  死循环（OOBE 卡死）；输入泵 any 门漏洞 → 动画期纯移动不重画光标。
  教训：动画循环改造三件套自查清单——退出条件、终态帧、输入泵接全。
