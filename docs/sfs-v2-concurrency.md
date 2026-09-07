# SFS v2 并发修复：inode/块串线、事务污染与 jlog_place 除零

修复对象：`kunxcore/src/fs_simple.rs`（SFS v2，写前日志实现）。
磁盘格式与布局见 `docs/sfs-v2.md`；本文件仅记录**并发层**的缺陷、根因、
修复与回归验证。掉电一致性层的审计另见 `docs/sfs-hardening-notes.md`。

## 1. 缺陷现象（2026-08-27 实测，docs/camera.md §8.2）

多个守护进程（camerad / audiod / sensord / audiomix 子进程…）并发在
`/var/run` 创建文件时：

- **症状 A —— 文件内容串线**：`camerad.ctl` 读到的是 `audiod.ctl` 等其它
  文件的数据（实测读到音频 PCM / 字库块）。`fs_create` / `dir_add_entry` /
  `data_block_alloc` 在并发创建下未保持「目录项 ↔ inode ↔ 数据块」的原子对应，
  FTRUNCATE 后仍反复出现。
- **症状 B —— 事务提交除零崩溃**：随机守护进程在首个事务写
  （`fs_create`/`fs_write`/`fs_truncate` 的 `txn_commit`）时于 `jlog_place`
  处 `%` 除零 panic。实测 `jlog_place` 收到 `major=0xFFFFFFFF` —— 即事务状态
  （`TXN_DEPTH/TXN_MAJOR/PENDING_*`）被并发交错污染后，提交作用在「未挂载」
  的伪设备上，其 `jlog_blocks==0`，`next_abs % 0` → 除零。

## 2. 根因分析

SFS v2 的事务状态与分配位图全部是**内核全局静态量**（单事务模型），而本内核的
syscall 处理在 EL1 期间**放开 IRQ**（`syscall_dispatch` 入口 `daifclr #2`，
定时器每 10ms 抢占可在任意指令间切换任务）。因此：

1. **事务交错**：任务 A 在 `fs_create` 中途被抢占（已 `txn_begin`，
   `TXN_DEPTH=1, TXN_MAJOR=DEV`）；任务 B 进入同一设备的 `fs_create`，
   `txn_begin` 使 `TXN_DEPTH=2`。A、B 共享「全局待写镜像缓冲 PENDING_*」：
   B 的暂存覆盖 A 的暂存；任一方 `txn_commit` 到深度 0 时把**两人未完成操作的
   混合暂存**一次性写日志并 copy-out —— 目录项、inode、数据块张冠李戴，
   表现为「A 建的文件指向 B 的数据块」等串线。
2. **状态污染**：一方提交把 `TXN_MAJOR` 重置为 `u32::MAX`、`PENDING_N=0`；
   另一方随后调用 `fs_checkpoint` / 交叉设备 `txn_begin` 时，以
   `TXN_MAJOR=u32::MAX` 进入 `commit_inner` → `dev_mut(u32::MAX)` 返回
   空白设备条目（`jlog_blocks==0`）→ `jlog_place` 对 0 取模 → 除零 panic。
3. **分配竞态**：`data_block_alloc` / `inode_alloc` 对 RAM 位图的
   「读-改-写」无原子保护；并发读后都看到同一空闲位 → inode 号/数据块重复
   分配（块被两个文件引用 → 读 A 出 B 的数据）。

## 3. 修复方案

### 3.1 全局递归 SFS 锁（`fs_lock` / `fs_unlock`）

所有对 SFS 共享状态（`DEVS` / `TXN_*` / `PENDING_*` / `SCRATCH_*` / `WEAR` /
RAM 位图）的访问必须持有本锁进行。锁语义：

- **按持有任务递归**：锁值 = 持有者 `current_tid()+1`（0 = 空闲）。同一任务内
  `fs_symlink → fs_create → fs_write` 等嵌套调用只递增深度、不互等 —— 锁天然
  覆盖「顶层操作整段」。
- **等待者短睡眠退避**（`sched_cfs::sleep_until(now+1ms)`，处于 Blocked 态），
  不忙等、不 spin、全程不屏蔽 IRQ：持锁任务被定时器抢占后仍会被再次调度并
  最终释放，无死锁、无活锁；即使实时类任务（audiod 等 Realtime）等待一把由
  普通类任务持有的锁，睡眠退避也不会产生「高优先级等待者绕死低优先级持锁者」
  的饿死，且不阻塞调度器（GUI / 音频任务仍可被调度）。
- **防御释放**：`fs_unlock` 检测到「非持有者调用」（历史污染兜底）时直接释放，
  避免锁被永久持有造成整个文件系统死锁。

效果：**每个顶层文件系统操作成为原子段** —— 事务状态独占，位图分配串行，
`fs_create` 的「分配 inode → 写 inode → 加目录项 → 分配数据块 → 提交」全程
不被交错。inode 号唯一、块不重叠、目录项一一对应。

### 3.2 除零与状态防御（`jlog_place` / `txn_begin` / `commit_inner`）

即使锁已消除正常路径的污染，仍对 `jlog_blocks==0` 做防御，杜绝 `% 0`：

- `jlog_place`：`jlog_blocks==0` 直接返回 -1（调用方按「事务未提交」回滚）；
  另防御 `jlog_next_abs < jlog_head_abs` 的游标倒退。
- `txn_begin` 预检：仅当 `jlog_blocks>0` 才计算环剩余空间（避免 0 取模）；
  不可用时跳过预检，提交路径会拦截并回滚。
- `commit_inner`：日志区未就绪（`jlog_blocks==0`）时立即回滚 RAM 位图/计数到
  事务快照并返回 -1 —— 保持「未提交事务整体丢弃」语义，绝不进入 `jlog_place`。

## 4. 并发模型（修复后）

```
EL0 进程 A ── syscall ──► EL1  syscall 处理（IRQ 可抢占）
 EL0 进程 B ── syscall ──►        │
                                 ▼
                          fs_lock()  ── 互斥：同一时刻至多一个任务执行 FS
                          ┌─────────────────────────────┐
                          │ txn_begin → 操作 → txn_commit │  ← 原子交易段
                          └─────────────────────────────┘
                          fs_unlock()
```

- 全局串行化粒度：**一个文件系统操作**（create/write/unlink/…）为一原子段；
  不同设备的操作也互斥（事务状态本就是单套全局量）。
- 性能取舍：小文件控制消息（/var/run、/tmp）为 μs 级，串行代价可忽略；
  大数据块写入仍在持锁期间执行，但锁不屏蔽 IRQ，调度器照常抢占，系统响应
  不受影响（与修复前的单线程模型同量级）。
- 与 WAL/掉电保护的关系：锁只解决**运行期并发**；WAL 原子性（DATA→COMMIT→
  copy-out→检查点）、双超级块、fsync/FLUSH 语义保持不变，崩溃恢复路径不受影响。

## 5. 回归验证

### 5.1 并发回归测试（多进程，`apps/sfsrace` + `make test-sfsrace`）

`/bin/sfsrace`（EL0，无 libc，clone/exit/wait4 风格与 probe/modemtest 一致）：
fork 6 个子进程；每个子进程在 `/var/run` **同时**反复执行
「create(O_CREAT|O_TRUNC) → 写跨 3 个数据块的唯一字节载荷 → 读回逐字节校验 →
truncate(0) → 验证读空 → unlink」。任何串线（读到其它文件的块、inode 冲突、
截断后残留、删除失败、提交崩溃）都会触发 FAIL 输出并退出非零；全部通过输出
`[sfsrace] SFS-RACE PASS`。

- 触发方式：往 ramdisk 根目录放置 `/sfsrace-test` 标记后 `make kernel`，init
  检测到标记即无头运行 sfsrace，结束自动 `poweroff`（QEMU `-no-reboot` 退出）。
- 断言：`make test-sfsrace` 在 QEMU 串口日志中 `grep "SFS-RACE PASS"`。
- 同时运行的真实负载：init 仍先拉起 networkd/camerad/audiod/sensord/…，
  （这些守护进程本身并发写 /var/run），构成与 §1 完全一致的复现场景。

### 5.1b 宿主多线程复现/验证（决定性实验）

`tools/sfs_host_test` 用 **include! 同一份内核 fs_simple 源码 + RAM 磁盘桩**在宿主
做多线程并发实验，毫秒级完成，可在同一份源代码上做「缺陷复现 ⇄ 锁验证」对照：

- **复现态（锁桩被证伪时）**：宿主桩曾给每条线程一个「从同一常量分配的 tid」，
  导致所有线程首次都取到**相同 tid** → `fs_lock` 的递归检测对所有人成立 → 锁
  退化为**无互斥**。`test_lock_serialization` 测得临界区最大并发 = 6（应为 1），
  `test_concurrent_race`（6 线程 × 25 轮 create→写 12KB→读回→截断→读空→删）
  完整复现生产实测症状：write-short、截断后读非空、内容串线（cross-talk）、
  悬空目录项、inode 已分配但无效、孤儿数据块，`sfs_check` 报错 ≥10 项 ——
  与 §1 逐条吻合（含「截断后读非空」这一曾在 QEMU 端压观察到的异常）。
- **验证态（锁真实互斥）**：桩改为**全局单调计数器**给每条线程分配唯一稳定
  tid + 等待者 `yield`（对应内核 sleep_until 语义）后，临界区最大并发 = 1，
  同一 race 测试 **0 失败**，`sfs_check` 全绿；单进程全套（含 3 数据块截断、
  跨块写读、掉电重放、撕裂写恢复、Python 镜像）0 失败。
- **结论**：修复前的缺陷是「无锁串行化下**事务状态 + 位图 + 目录项被并发交错**
  的综合撕裂」；修复后的递归互斥锁把每个顶层 FS 操作串行化为原子段后，同一
  并发负载完全干净。内核侧 `sched_cfs::current_tid()` 本身全局唯一，无桩的
  tid 冲突问题。

### 5.2 回归范围

- 单进程场景：启动自检（符号链接、/tmp 读写、根挂载）、OOBE 流程、
  文件管理 / 相机 / 音频应用 —— 由 `apps_regression.py` / `camera_e2e_test.py`
  等既有脚本回归，确保锁未破坏单线程路径。
- 掉电保护：`sfs-hardening-notes.md` 记录的挂载重放、检查点、fsck 路径
  （`make test-sfsrace` 结束即自动 `sfs_check` 级别的干净关机状态）。

## 6. 改动清单

| 文件 | 改动 |
|------|------|
| `kunxcore/src/fs_simple.rs` | 新增全局递归锁 `fs_lock/fs_unlock`（按任务）；全部 22 个对外接口包锁并拆 `_unlocked` 内部实现；`jlog_place`/`txn_begin`/`commit_inner` 对 `jlog_blocks==0` 防御 |
| `apps/sfsrace/` | 新增多进程并发回归测试（Cargo.toml / build.rs / linker.ld / src/main.rs） |
| `Makefile` | `sfsrace` 构建目标；ramdisk 复制 `/bin/sfsrace`；`test-sfsrace` QEMU 无头测试目标 |
| `kunxinit/src/main.rs` | `/sfsrace-test` 标记触发无头并发测试模式（结束自动关机） |
| `docs/sfs-v2-concurrency.md` | 本文档 |
| `CHANGELOG.md` | 本轮修复记录 |

## 7. 遗留与后续

- 锁为**全局**串行：多核 SMP 下不同设备也无法并行 FS（事务状态仍为单套全局
  量）。若需更高并发，可把事务上下文改为「按设备 + 按进程」隔离，再降级为
  每设备锁 —— 列为后续工作（与 sfs-v2.md §10 的既有条目一致）。
- `data_block_alloc` 的「清零新块直写」在分配位已置位后写盘失败会留下
  已分配但内容未定的块（既有语义，靠 fsck/重放覆盖），不在本轮范围。