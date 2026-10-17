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

- [x] **M1 MAILBOX 认领轮询超限（第 2 轮修复）**：根因**不是内核回归**——
  sfsrace 认领读者的退出检查（`dones >= WRITERS`）只挂在 rename 失败分支，
  而 4845a51 为修迟到写入窗口加了「认领后重建空管道」，管道恒存在 →
  rename 恒成功 → dones=3 后仍无限认领空管道直到 3 万轮上限（确定性假
  失败；心跳实证 dones=3 @poll 6000 后空转 24000 轮）。数据完整性断言
  从未触发（无丢失/重复/撕裂），内核无辜。修复：取空认领（n==0）+
  哨兵收齐即退出（协议注释「再取空确认」的本意）。**test-mailbox ×3
  全过**；MAILBOX PASS 已纳入 test-sfsrace 判定门（防再漏，M1 教训
  写入 Makefile 注释）。

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

### 第 2 轮（2026-10-18 00:0x，定时轮）

- **M1 修复**：MAILBOX 假失败根因 = sfsrace 测试自身退出逻辑自 4845a51 起
  不可达（认领后重建管道 → rename 恒成功 → dones 检查只在 rename 失败分支）。
  心跳诊断实证：dones=3 @poll 6000，其后空转 24000 轮到上限。修复 = 取空
  认领 + 哨兵收齐即退出。内核无回归（阶段 A/B 完整性断言从未触发）。
- **判定门加固**：test-sfsrace 现要求 SFS-RACE PASS **和** MAILBOX PASS
  （同一二进制两阶段都跑，此前只查前者致信箱假失败漏网一整晚）。
- 保留 sfsrace 心跳诊断打印（每 6000 轮一行 + 超限时 dones 值）。
- 验证：test-mailbox ×3 PASS（含 144 条无丢失/无重复 + 无撕裂断言）；
  加固门 test-sfsrace PASS；kos 全套 11 套件 0 失败；make 0 错误 0 新警告。

### 第 3 轮（2026-10-18 01:0x，定时轮）

- **E1 引擎故障注入测试落地（kos/tests/fault.rs，6 测试）**：FaultDev
  （Cell 内部可变）注入一次性读/写/flush 故障 + 永久死盘，断言全 API 零
  panic、Err 全为 Io、故障后服务连续（恢复即可写）、内容旧或新无脏第三态、
  Full Merkle 重挂载与内存视图一致、死盘拒绝一切写且热恢复回最近完好代、
  gc 故障下存活对象不丢。
- **E1 抓到真引擎缺陷并修复（本轮核心战果）**：读故障矩阵在 lba 级审计
  （FaultDev 记录读序列）下现形——**fast_mount 重写时九处防护的
  `return None;` 全部丢失**（空 if 体；即并行会话记忆里警告过的「python
  补丁锚点静默跳过」事故现场）：hint 过期/位图块数不符/容量不足/索引链
  越界/索引块/头块/位图读失败/first_bit 不符全部被静默吞掉——头块读失败
  后拿**上一个对象的残 buffer** 解析出新对象（症状：HashMismatch content
  持续复现）。平时读全成功所以常规测试全绿；故障注入一跑就现形。九处
  防护体全部恢复，fault 6/6 过。**教训沉淀：重写/大补丁后必须 diff 老的
  防护点清单（grep 'return None;' 计数对不上就是丢了）。**
- 验证：kos 全套 12 套件 0 失败（+fault 6）；make 0 错误；加固门
  test-sfsrace PASS（SFS-RACE + MAILBOX 双绿）。

### 第 4 轮（2026-10-18 02:0x，定时轮）

- **E2 内核 I/O 故障降级落地**：syscall 480（root 专用）切换 KOS 块设备
  故障注入；降级状态机（首个 I/O 失败 health 报 CRITICAL、恢复报 INFO，
  边沿触发不刷屏）内建在 KosBlkDev 三原语里。用户核心诉求「报错了还能
  继续维持运行」的内核侧闭环。
- **kostest 新增 fault-degrade 阶段**：注入 → write_mutable/append/mkdir
  全部精确 -5（EIO）→ 解除 → 旧数据逐字节完好 + 新写成功 + mkdir 成功
  （幂等容 -17 残留）。
- **QEMU 带盘实证**（持久卷 major=8 挂载）：health 先后打出「卷已降级
  （读写返回 EIO，系统继续运行）」与「I/O 恢复，卷重回正常服务」，
  KOSTEST FAULT-DEGRADE PASS + KOSTEST PASS，全程系统存活可关机。
- 验证：kos 全套 12 套件 0 失败；make 0 错误；sfsrace（含新门）PASS；
  动了内核数据面 → mailbox ×3 PASS。

### 第 5 轮（2026-10-18 03:0x，定时轮）

- **C1 无缝复制三向实证 + 固化**：shell `cp` 经第 1 轮的 vfs 路由零改造
  打通 /kos/——QEMU 持久卷上 SFS→KOS / KOS→SFS / KOS→KOS 三向复制
  sha1 全一致（/bin/hello 6829afd8…，wc 16384 字节整）。新增
  `make test-kos-copy` 回归（tools/koscp_check.py 四哈希判定），连续
  两轮 PASS（幂等：固定目标名 + write_mutable 原地换代）。
- **shell `kos mkdir`**（C5 顺带）：复制进 /kos 子目录的前提；幂等输出
  （已存在 → 提示不报错）。注：/tmp 在 SFS 根卷不存在，跨池目标用 /bin。
- 验证：kos 12 套件 0 失败；make 0 错误；sfsrace（含新门）PASS。

### 第 6 轮（2026-10-18 04:0x，定时轮）

- **C2 引擎级 `kos cp`**：sys_copy 单次 fs_lock 内「读源→写目标」——可变
  记录分块流式（16KB 缓冲任意大小）、不可变对象整读整写（put 内容寻址
  **天然去重**）；shell `kos cp/df(=stat)/du` 子命令。QEMU 实证：142552
  字节记录复制 sha1 与源一致。
- **C5 stat 路由收口**：vfs::stat_path / lstat_path 均路由 /kos/（`stat
  /kos/data` → 目录 164 字节）；cp 复制进目录的 basename 拼接打通
  （`cp /bin/hello /kos/data/` → /kos/data/hello）。**两个坑**：① 剥
  /kos/ 前缀后须补前导斜杠（resolve 只认 "/data" 形式）；② 尾斜杠须
  归一。期间一度误判修复无效（测试跑在旧内核上）——改 vfs 后必须
  touch ramdisk + 确认 kernel 确实重编。
- **test-kos-copy 扩到六哈希**：三向 cp + 引擎级 kos cp + 目录拼接，
  连续两轮 PASS（幂等）。引擎对「截断目录」正确拒绝（EACCES）——
  防御行为顺带实证。
- 验证：kos 12 套件 0 失败；make 0 错误；sfsrace（含新门）PASS；
  动了内核数据面 → mailbox ×3 PASS。

### 第 7 轮（2026-10-18 05:0x，定时轮）

- **E3 上传会话生命周期落地**：①meta v2 增 last_active_us（push/begin
  刷新；旧格式视为永不超时防误杀）；②unlink 主路径联动清 part/meta，
  **主路径不存在但会话残留 = 取消上传语义**（清会话返回成功）；③gc()
  前置 sweep_stale_uploads（60s 空闲自动回收，迭代遍历防深目录递归）。
  测试 tests/upload_gc.rs：联动清理/超时回收/活跃会话存活 3 场景。
- **E4 顺带**：posix_lseek SEEK_END 取大小失败改报 EIO（原 -1 基准 +
  正偏移可把 pos 拨到 EOF 外垃圾位置）。
- **🔴 P0 新发现（第 8 轮已修复 ✅）**：`upload_finish` 后重挂载丢已发布
  对象。**根因：`KosObject::to_bytes` 先算 CRC 后写 reserved[92..96]，
  而 from_bytes 对全 96 字节校验——reserved[0..4]=mtime 非零的对象头
  重挂载必 CrcMismatch**（load_object 收内存 hdr 从不回读校验，提交期
  无感）。此前所有测试时钟为 0 恰好掩盖；upload_gc 首个 set_clock(非
  零) 测试踩响。修复：reserved 先写再算 CRC（对零 reserved 数据 CRC
  不变，旧盘兼容）；**同款潜伏雷一并清剿**：manifest/namespace/merkle
  三处 to_bytes 同顺序问题（reserved 恒零暂未爆）预防性修正。回归：
  upload_gc 3/3（含 finish+重挂载丢对象场景，已去 #[ignore]）。
- 验证：kos 全套 13 套件 0 失败（+upload_gc）；make 0 错误；sfsrace
  （含新门）PASS；mailbox PASS。

### 第 8 轮（2026-10-18 06:0x，定时轮）

- **P0 修复（本轮主战）**：`upload_finish` 后重挂载丢对象——根因为
  `KosObject::to_bytes` 的 **CRC 计算先于 reserved[92..96] 写入**，而
  from_bytes 全 96 字节校验。reserved[0..4] 存 mtime → set_clock 非零后
  一切可变记录提交的头块重挂载必败；提交期无感的原因：load_object 拿
  内存 hdr，从不回读块上头部。**数据丢失级**（重启丢上传成果/记录换代）。
  此前全测试时钟为 0 恰好掩盖。修复 = 字段写入全部先于 CRC；manifest/
  namespace/merkle 三处同款潜伏顺序（reserved 恒零未爆）一并清剿。
- 回归：upload_gc 3/3（重挂载场景去 ignore）；kos 全套 13 套件 0 失败；
  QEMU 持久卷 KOSTEST PASS（352 对象在卷、fault-degrade 阶段绿）；
  make 0 错误；sfsrace 含新门 PASS；mailbox PASS。
- 坑再证：make kernel 后测试标记若在内嵌 ramdisk 里，须重打包 ramdisk
  再 touch + make kernel（否则干净验证跑的是带标记内核）。
