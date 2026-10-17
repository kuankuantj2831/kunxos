# KOS 夜间工单（2026-10-17 深夜起，为期 10 小时，每小时一轮）

> 本文件是 KOS 对象存储专项夜间工单。每轮执行者：读「进度日志」→ 从「任务池」领取
> 未完成任务 → 实现 → 按「验证门槛」全量验证 → 更新进度日志 → 本地 git commit（不 push）。
> 禁止向用户请求验收（用户在睡觉，十小时后醒来读总结）。

## 总目标（用户原话归纳）

1. **错误处理**：kos 文件系统「报错了还能继续维持运行」——任何底层 I/O 错误、
   磁盘损坏、非法输入都不得 panic 内核/引擎，必须转为 Err→errno，系统继续服务。
2. **更多稳定性**：断电一致性、故障注入、边界行为全部有测试固化。
3. **完全接入 KUNXOS**：`/kos/` 无缝读写已通（本轮已做 vfs 路由），继续做到
   无缝复制（shell cp 跨池）、files 应用可见、kostest 回归固化。
4. 连续修 bug 跑满十小时，最后收官总结。

## 环境坑（每轮必读，违者白干）

1. **rustup 代理损坏**：本会话环境里 `cargo`/`rustc` 裸调会报
   `unknown proxy name: 'ZCode-3.15.3-linux-x64'`。**每条构建/测试命令前必须**：
   `export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"`
   （shell 环境不跨命令保留，每个 Bash 调用都要带 export）。
2. **后台 shell 不保留 cwd**：一律 `cd /home/kuankuantj/桌面/kunxos && ...` 绝对路径。
3. **make kernel 前必须 `touch kunxcore/ramdisk.img`**（cargo mtime 误判，否则跑旧内核）。
4. **提交纪律**：commit 前先 `git status --short` + `git diff --stat` 过目。若出现
   本工单之外的文件改动（曾有并行会话在制改动被卷入），单独说明或单独提交，
   不得盲目 `git add -A` 吞掉来历不明的改动。
5. QEMU GUI 验证优先 monitor `sendkey`（鼠标事件间歇性不达 virtio-tablet）。
   UI/内核行为改动必须实证（screendump / 串口输出），编译过≠行为对。
6. kos 宿主测试：`cd kos && export PATH=... && CARGO_TARGET_DIR=$PWD/target cargo test`
   （全套 ~20s；capacity 已从 305s 修到 ~7s）。

## 验证门槛（每轮收尾必做，全过才算完）

- [ ] kos 宿主全套测试 0 失败（见坑 6）
- [ ] `make -j$(nproc)` exit 0，0 新警告
- [ ] `make test-sfsrace` PASS（判定门只看 SFS-RACE PASS）
- [ ] 若动了内核数据面/调度：`make test-mailbox` 跑 3 次（当前已知既有失败，见 M1；
      M1 修复后纳入硬门槛）
- [ ] 更新本文件进度日志 + git commit（conventional message，注明第 N 轮）

## 任务池

### P0 既有回归（本轮基线发现，优先修）

- [ ] **M1 MAILBOX 认领轮询超限**：HEAD 基线（不含任何本工单改动）即 3/3 确定性失败
  （`make test-mailbox`，串口见 `MAILBOX FAIL: 认领轮询超限（疑似死锁/活锁）`）。
  上轮工单收官只跑 test-sfsrace（其判定门不含 MAILBOX PASS），漏网。排查方向：
  `git log --oneline 49877c0~5..def9387` 期间 oobe 转场重写/调度改动；二分构建
  定位首个失败提交；修复后 test-mailbox ×3 全过，并把 MAILBOX PASS 加进
  test-sfsrace 判定门（防再漏）。

### P1 错误处理/容错（用户核心诉求：报错了还能继续运行）

- [ ] **E1 引擎故障注入测试**：宿主侧 FaultDev（read/write/flush 按序号或概率返回
  Io 错误）遍历全部公开 API（put/get/write_mutable/append/truncate/write_range/
  upload_*/gc/rename/unlink）：断言零 panic、全部走 Err；任一点故障后 remount，
  断言回到最近完好代（数据不丢不脏）。固化为 `kos/tests/fault.rs`。
- [ ] **E2 内核 I/O 故障降级**：KosBlkDev 读写失败后 KOS 卷进入 degraded 状态
  （health 上报 + 全部 kos syscall 返回 EIO），内核不 panic 不死锁；提供错误注入
  开关（参照故障遏制 syscall 415 的思路，或 blkdev 层注入 major 号开关）供
  kostest 触发与恢复验证。
- [ ] **E3 upload 会话残留回收**：upload 中途崩溃/放弃的会话（`/path.kosup.*`）残留
  清理——unlink 主路径时联动清会话；会话超时（如 60s 无 push）自动回收。
- [ ] **E4 边界复核**：kos_vfs 的 sys_read/sys_list 用户缓冲区拷贝长度钳制；
  UploadIncomplete errno 编码（高 32 位打包）在 vfs 桥 i32 转换处的语义不破坏；
  posix_lseek SEEK_END 失败时的返回语义。

### P2 稳定性

- [ ] **S1 capacity flake 监控**：第 1 轮曾观测一次 fast=false 未复现（可能为并行
  在制态）。每轮跑 `cargo test --test capacity`，若复现：保留镜像 + 记录参数，
  按 dbg2.rs 方法分级定位。
- [ ] **S2 断电矩阵扩展**：crash.rs 基础上加「每代提交后 + 随机偏移」双维度断电点，
  覆盖 persist_alloc_state 三段写序（位图→索引链→hint+超级块）的每个窗口。
- [ ] **S3 NoSpace 行为**：小卷写满：部分写失败不留脏状态（对象索引/位图回滚一致），
  remount 后 free 计数正确、无孤儿占位；回收后可继续写。

### P3 完全接入 KUNXOS（无缝复制等一系列功能）

- [ ] **C1 shell cp 无缝复制 QEMU 实证**：`make kos-disk` 后带 KOS_DRV 启动
  （参考 Makefile 539 行附近 run 目标），串口 shell 里：
  `cp /etc/xxx /kos/data/test.txt` → `kos cat /data/test.txt` → 反向
  `cp /kos/data/test.txt /tmp/back.txt` → `cmp`。失败则修 vfs 路由/posix 桥。
  把该链路写成 kostest 或自动化脚本固化。
- [ ] **C2 shell `kos cp`**：引擎级复制——同池 = 内容寻址天然去重（同内容引用
  复用，秒回）；跨池 = 内核内字节搬运（不经过用户态两跳）。附 `kos df`（卷
  空间）。
- [ ] **C3 files 应用浏览 /kos/**：files 应用走 POSIX open/readdir——验证 readdir
  路径对 /kos 的支持（若缺则补：vfs readdir/ls 路由或 kos 专用 list），截图实证。
- [ ] **C4 kostest 扩展**：POSIX 桥回归（open/read/write/lseek/close + O_APPEND +
  O_TRUNC）、fd 泄漏检查（连开 100 个不关后仍能开）、close_pid_fds 生效验证
  （fork 子进程开 /kos fd 后退出，父进程确认表不涨）。
- [ ] **C5 语义完善**：vfs::stat_path 路由 /kos（cp 到 /kos 目录自动拼 basename）；
  shell ls/cat 对 /kos 目录直通（现需 kos ls，统一体验）。
- [ ] **C6 文档 docs/kos.md**：架构图（双池/manifest/持久索引）、用法（/kos 路径、
  syscalls 号段、kosctl、make kos-disk）、故障模型（degraded/回退扫描/断电恢复）、
  与 SFS 并存关系。

### P4 收官（第 10 轮必做）

- [ ] `make clean && make -j$(nproc)` 全量重建 0 错误；kos 全套 + sfsrace + mailbox
  （若已修）全绿
- [ ] GUI 冒烟带 kos-disk（OOBE→launcher 0 PANIC，`kos stat` 有对象）
- [ ] 写「最终交付总结」小节：修了什么（根因一句话一个）、测了什么、遗留什么
- [ ] 用 CronList 找到本工单自动化（标题含「KOS 十小时夜间工单」）并 CronDelete
  停止后续轮次；然后本轮结束

## 进度日志（每轮追加，勿删旧记录）

### 第 1 轮（2026-10-17 深夜，主会话开工，a871f8c）

- **capacity 2TB 失败根因修复**：`persist_alloc_state` 用懒截断后的 `free.len()`
  当池大小 → 位图持久化成截断版 + hint 块数错 → 下次挂载永久回退 5 亿块全池
  扫描；且 bitmap_dirty 被 resize 短后高位分配裸下标写越界 panic。改取
  `sb.obj_pool_blocks` + mark_bitmap_dirty bounds 助手（4 处）。**305s→7s，全套
  92 测试全绿**。（注：并行会话同晚做了懒加载位图改造，def9387 混入双方改动；
  其自动化已删，避免与今晚工单互踩。）
- **内核 POSIX fd 表加固**：按 pid 绑定 + fs_lock 纪律 + 每进程 64 上限 + 回绕
  安全；reap() 兜底回收死者 KOS fd（exit 顶部方案会破坏 mailbox 时序——实测
  撤回，reap 点就够：pid 复用前必经 reap）。
- **vfs 路由 /kos/**：open/read/write/close/lseek 前置分发到 posix 桥——内核侧
  shell cp/cat 等零改造接入（C1 的地基）。
- **gc unwrap 清剿**：标记循环 objects.get().unwrap() → 宽松跳过。
- 发现并交接：M1 mailbox 既有回归（基线 3/3 失败，非本轮引入）。
- 验证：kos 全套 0 失败；make 0 错误 0 kos 新警告；test-sfsrace PASS；
  mailbox 3/3 FAIL（既有，转 M1）。
