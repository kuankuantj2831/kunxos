# kunxos 系统 bug 审查报告与修复计划

## 审查方式
- **kunxboot（引导器）**：探索代理完成全部源码审查（main/dtb/fs_fat/mmc/sec_boot/jump/splash/uart/linker）。
- **kunxcore（内核）**：亲自精读调度器（sched_cfs）、中断分发、SMP、IPC 管道、futex、网络栈、socket、fs_simple、fat32 锁、virtio_blk，确认关键事实：**syscall 入口 `daifclr` 后全程 IRQ 使能（main.rs:2778），可被 10ms tick 抢占切任务**。
- **apps（应用层）**：因配额耗尽仅抽查，列为后续项。

---

## 高严重度（单核即可触发，建议优先修）

**H1. 网络栈全局缓冲无并发保护 → 并发联网应用互踩数据**
- `net_stack.rs:76-90`：FRAME/IPBUF/UDPBUF/CHECKBUF 等全局组装缓冲注释"单线程"，但 `net_stack.rs`/`socket.rs` **0 处 daifset**；syscall 可被 tick 抢占切到另一任务，两个任务（如浏览器+邮件）并发收发时缓冲互踩 → 发出损坏帧/串包。socket.rs:399 等多处 `yield_now()` 主动切换点加剧窗口。
- **修复**：引入全局 NET_LOCK（自旋 + 屏蔽 IRQ，模式同 fat32 锁），覆盖 socket 收发与 net_stack 组包路径。

**H2. 网卡 RX 中断与 socket syscall 并发写同一套 socket 队列**
- `net_virtio.rs:466` 注册 IRQ → 中断里跑 dispatch → 写 socket 接收队列；而 syscall 上下文 IRQ 使能且 socket.rs 无屏蔽 → 队列 head/tail 撕裂、丢包/崩溃。
- **修复**：同 H1 的 NET_LOCK（IRQ 处理器内持锁天然安全，因入口已屏蔽）。

**H3. virtio_blk 无任何锁 → 并发块 I/O 损坏 virtqueue → 跨文件数据损坏**
- `virtio_blk.rs:134-146` static mut RING/LAST_USED，无锁；`fs_simple` 的 fs_lock 不屏蔽 IRQ（fs_simple.rs:375-403），块请求提交-轮询期间可被 tick 抢占，另一任务的 blk_read 覆写描述符环。（fat32 路径因 fat_lock 屏蔽 IRQ 而安全；fs_simple 路径不安全。）
- **修复**：在 blkdev 层加屏蔽 IRQ 的自旋锁，一处覆盖所有后端。

## 中严重度

**M1. `sleep_until` SMP 丢唤醒窗口**（sched_cfs.rs:944-971 + tick:874-884）：登记 sleeper 后、`block_current` 前，他核 tick 到期会把 `s.active` 清掉但目标尚未 Blocked → 之后永久阻塞。单核安全，SMP 启用即触发。修复：tick 仅在成功唤醒（Blocked→Ready）时才清 active；或阻塞后二次核对 deadline。

**M2. futex 等待表无锁**（bionic_shim.rs）：`futex_wait` 屏蔽 IRQ 写表，`futex_wake/requeue/cleanup_tid` 无屏蔽读写——SMP 下竞态。且 `sched_cfs::wake()`（sched_cfs.rs:1300-1312）与 `reap_pid` 护栏（:1419）**无条件 daifclr**，一旦从屏蔽上下文调用会破坏中断屏蔽（当前调用点恰在 IRQ 使能的 syscall 内才未爆）。修复：futex 表操作统一进屏蔽窗口；wake/reap 改为保存-恢复 DAIF。

**M3. fs_simple 锁缺陷**：① 非 IRQ 安全（见 H3）；② 持有者 ID = tid+1，tid 重用后新任务误认持有锁；③ 持锁任务被 kill 时锁永久泄漏。修复：改为 CPU 号+重入深度的原子锁（同 fat32），并加持有者存活校验。

**M4. `tick()` 的 pick_next()==None 分支把刚置 Exited 的任务复活为 Running**（sched_cfs.rs:907-929）：当前靠"每核 idle 恒 Ready"兜住，属脆弱模式，建议 None 分支不触碰 Exited 状态。

## 低严重度 / 备忘
- `for_each_task*`/`task_count` 无锁读 TASKS（诊断用途，撕裂读）。
- `wake_irq` 把 NEED_RESCHED 记在本核而非目标核（SMP 唤醒延迟至多 10ms）。
- `sleep_until` 无空位降级路径在 IRQ 屏蔽下忙转 yield。

## kunxboot 子代理报告要点（已完成审查）
- **高**：恢复 ramdisk 加载地址 0x4A000000 落在 128MB RAM 之外（load_kernel.rs:16，恢复模式必坏）；DTB fixup 增长后缓存维护区间不含尾部（main.rs:195-210，真机随机损坏 bootargs）；设备密钥用可预测熵（栈地址+LCG）烧 OTP（sec_boot.rs:149-158）。
- **中**：DTB 属性长度无界可越界读写 scratch（dtb.rs:150-275）；`size_struct-4` 下溢（dtb.rs:385）；FAT 挂载回退任意分区类型（fs_fat.rs:101）；簇号无分区上界（fs_fat.rs:198）；virtqueue 落 .bss 无 4096 对齐保证（mmc.rs:59+linker.ld）；MMC 超时后描述符竞态（mmc.rs:229）；反降级未实现/BCB 读失败当出厂态/provisioning 失败继续（sec_boot.rs 多处）；建议开 `overflow-checks`。

---

## 修复实施顺序（获批后执行）
1. **P0**：blkdev 层加锁（H3）→ 网络栈 NET_LOCK（H1+H2）——三个高危同源（"syscall 可抢占 + 无锁共享状态"），模式统一。
2. **P1**：futex 表加屏蔽窗口 + `wake()`/`reap_pid` DAIF 保存恢复（M2）；fs_simple 锁改造（M3）；`sleep_until` 丢唤醒（M1）。
3. **P2**：kunxboot 三项高优（恢复 ramdisk 地址、DTB 大小回传、熵源改 virtio-rng 或拒绝烧钥）。
4. **P3**：apps 应用层完整审计（本轮因配额未深入）。
每步改完用 `make` 构建 + 现有回归脚本验证。