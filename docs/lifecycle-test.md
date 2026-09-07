# KunxOS 应用生命周期与后台管理系统 — 测试方案

> 本文档对应 `docs/lifecycle.md`（设计）的验收测试：生命周期状态机、后台任务
> 白名单、内存压力（LMK）、推送框架与状态持久化。实现语言 Rust + Python（QEMU
> 回归工具）。自动化工具为 `tests/scripts/lifecycle_test.py`（无头核心阶段）+ 可选 GUI
> 阶段；GUI 手动步骤见下方「阶段 B」。

## 0. 构建与运行

```sh
make kernel          # 编译内核（内嵌 ramdisk：init/lifd/pushd/lcapp/launcher 等）
make lifecycle-test  # = kernel + python3 tests/scripts/lifecycle_test.py /tmp/kx_life.log
```

退出码约定：0 = 全部通过；非 0 = 有失败（详细结果打印在末尾，失败项带 FAIL 前缀）。

## 1. 测试架构与可观察性

系统采用「文件信箱」作为测试控制面（设计即支持注入，见 lifecycle.md §2.3）：

| 路径 | 角色 | 测试用法 |
|---|---|---|
| `/data/life/reg_<name>` | 应用登记（内容 `pid FG/BG`） | `echo <pid> FG > /data/life/reg_appA` |
| `/data/life/cmd_<pid>` | 命令：BG / FG <pid> / BTASK / BTASK_END / SET_TIMEOUT / DELIVER | `echo BG > /data/life/cmd_<pid>` |
| `/data/life/evt_<pid>` | lifd 事件（PAUSE/SUSPEND/RESUME/TERMINATE/NOTIFY/BG_TASK_EXPIRED） | `cat /data/life/evt_<pid>` |
| `/data/life/launcher_evt` | lifd → launcher（BG/FG/KILLED/EXITED） | `cat /data/life/launcher_evt` |
| `/data/life/recents` | 最近任务列表（`name pid R/B/S`） | `cat /data/life/recents` |
| `/proc/pressure` | 内核内存压力（读：`level free_kB total_kB`；写：`force <0|1|2>`/`clear`） | 写注入触发挂起/LMK |
| `/proc/processes` | 每进程 `PID STATE PAGES NAME`（T=SIGSTOP 停止） | 验证进程级停止/恢复 |
| `/data/push/in/<pid>_<seq>` | 本地通知提交（内容 `<title>|<body>`） | 写文件 → pushd 入队 → lifd 投递 |
| `/data/push/pending/<pid>` | 挂起/后台应用的延迟通知 | 恢复后随 RESUME 补投 |
| `/data/state/<name>.bin` | 应用 UI 状态（SFS 持久卷） | 写/读回验证 |

串口日志是主断言通道：`[lifd] ...`、`[pushd] ...`、`[mem] pressure ...`、
`[lcapp] ...`（GUI 阶段）均逐行打印。

### 测试“应用”的选择

无头阶段用**真实常驻守护进程**（audiod/sensord/camerad/rild 等，从 `ps` 取 pid）
作为 lifd 注册表里的受控对象：lifd 只依赖 pid 做信号操作，进程是否生命周期感知
无关紧要。测试通过 SIGSTOP/SIGCONT 翻转其内核调度状态来验证状态机，全程可恢复
（测试结束前统一 FG 回前台）。

GUI 阶段用专用演示应用 `apps/lcapp`（经 launcher 启动），验证**应用侧** API：
lc_set_callbacks 注册的 on_pause/on_resume/on_suspend/on_terminate/on_notify
回调、lc_handle_nav 的 Home/Recents 后台化、lc_request_bg_task、lc_notify_local、
lc_save_state/lc_restore_state。

## 2. 阶段 A（无头核心回归，自动化）

`tests/scripts/lifecycle_test.py` 依次执行并断言（用例编号对应 lifecycle.md 章节）：

| # | 场景 | 步骤 | 断言（串口/文件） |
|---|---|---|---|
| A1 | 服务启动 | 等待 serial shell `kunxos$` | `ls /data/life` 出 `launcher_evt`；`ls /data/push` 出 `in` |
| A2 | 登记 | `echo <pidA> FG > reg_appA`；`echo <pidB> BG > reg_appB` | `[lifd] registered appA pid=.. FG`；`recents` 含 `appA <pid> R` |
| A3 | 前台→后台 | `echo BG > cmd_<pidA>` | `[lifd] background appA`；`recents` 含 `appA <pid> B`；`/proc/processes` 含 `<pidA>` 行状态 `T` |
| A4 | 后台→前台 | `echo FG <pidB> > cmd_<pidA>`（lifd 忽略发送方，按目标 pid 找注册表） | `[lifd] foreground appB`；`recents` 含 `appB <pid> R`；`/proc/processes` 含 `<pidB>` 行状态 `R` |
| A5 | 再切换 | `echo FG <pidA> > cmd_<pidA>` | `[lifd] foreground appA` |
| A6 | 后台任务白名单 | ① `BTASK 9 100`（越界类型）② 对 appA 连续 `BTASK 1/2/0`（每应用上限 2） | ① `bg task denied: unknown type` ② 前两个 `bg task granted`，第三个 `bg task denied: per-app limit` |
| A7 | 后台任务超时 | `BTASK 3 300`（下载 300ms）→ 等待 | `bg task expired`；`evt_<pidA>` 含 `BG_TASK_EXPIRED` |
| A8 | 挂起超时 | `BTASK_END`；`SET_TIMEOUT 800`；`BG appA` | ~0.8s 后 `[lifd] suspend appA`；`recents` 含 `appA <pid> S` |
| A9 | 内存压力 Moderate | `echo force 1 > /proc/pressure` | `[mem] pressure force 1`；`pressure Moderate: suspending idle background apps` |
| A10 | 内存压力 Critical/LMK | `echo force 2 > /proc/pressure` | `[mem] pressure force 2`；`pressure Critical: running LMK`；`LMK: killing score=`；先杀最低分 `terminated appA`（Running 永不杀） |
| A11 | 压力恢复 | `echo clear > /proc/pressure` | `[mem] pressure clear`；`pressure Normal` |
| A12 | 推送（未登记→丢弃） | `echo hello world > /data/push/in/<未知pid>_1` | `notify dropped: app not registered` |
| A13 | 推送（后台→延迟补投） | 登记并 `BG appD`；`echo hi there > /data/push/in/<pidD>_2` | `notify deferred`；`/data/push/pending/<pidD>` 含 `NOTIFY` |
| A14 | 推送（恢复补投） | `echo FG <pidD> > cmd_<pidC>` | `[lifd] foreground appD`；`evt_<pidD>` 含 `RESUME NOTIFY` |
| A15 | 状态持久化 | `echo save-test-123 > /data/state/sample.bin` | `cat` 回读 `save-test-123` |

> 无头阶段不启动 GUI 框架（`-display none`），但仍拉起 init/lifd/pushd/各守护
> 进程；shell 由 `-append shell` 起动（内核 shell，见 power_test.py 同款模式）。

## 3. 阶段 B（GUI 端到端，可选 `--gui` / 手动）

真实用户路径：desktop → 打开 lcapp → Home 手势 → 后台化（SIGSTOP）→ 再从
最近任务恢复（SIGCONT + RESUME）→ 验证应用侧回调序列与 UI 状态恢复。

1. `make run-gui`（或 `python3 tests/scripts/lifecycle_test.py --gui`，需 ramfb + 虚拟
   触摸设备）。
2. OOBE 完成后 desktop；点第一屏的 **LCApp** 图标（`apps/launcher` 已把 lcapp
   加入 PAGE0_APPS；位置由 `icon_center` 计算，可用 QEMU screendump 核对）。
3. 前台断言：串口出现 `[lcapp] ready`、`[lcapp] restored save_n=..`（或
   `state_restored none`）、`[lifd] registered lcapp pid=.. FG`。
4. 触发 Home 手势（底部上滑）：应用写 `BG` 命令 → 宽限窗内 `on_pause` →
   `on_suspend`（保存状态，`[lcapp] on_suspend saved=..`）→ lifd SIGSTOP →
   `[lifd] background lcapp`。launcher 同步收到 `launcher_evt: BG lcapp` 并
   快照末帧、播放返回动画。
5. 进程级确认：`/proc/processes` 中 lcapp 行状态 `T`；`recents` 含
   `lcapp <pid> B`。
6. 从最近任务卡片恢复：launcher 播放 Resume 缩放动画（lcapp 仍 SIGSTOP）→
   动画完成发 `FG` → lifd SIGCONT + `evt_<pid>: RESUME` → `[lcapp] on_resume`
   + `[lcapp] resumed`（自动重绘）→ `/proc/processes` 状态 `R`。
7. 通知链路：在 lcapp 前台点「发送本地通知」按钮 → `[lcapp] notify_queued` →
   pushd `notify id=... -> pid=... forwarded` → lifd 投递 `evt`：NOTIFY →
   `[lcapp] on_notify id=.. title=..`。若在后台点击「发通知」，则入 pending，
   恢复时随 RESUME 补投（`RESUME NOTIFY` 合并事件同时触发 on_resume+on_notify）。
8. 后台任务：点「请求后台任务」→ `[lcapp] bg_task_requested` + lifd
   `bg task granted type=0`；「结束后台任务」→ `bg task ended`。
9. 状态恢复验证：`on_suspend` 保存后，SIGKILL lcapp（`kill <pid>`）再重新打开：
   启动时 `[lcapp] restored save_n=..`（从 `/data/state/lcapp.bin` 回读）。

## 4. 注入与非注入两路的可靠性说明

- **内存压力**：`/proc/pressure` 写 `force <0|1|2>` 强制等级（`mem_pressure.rs`
  的 `handle_cmd`），`clear` 恢复按真实空闲页计算——测试不依赖真实耗尽 512MB
  内存，结果确定性；真机/压力负载下可用 `force clear` 观察真实等级跃迁日志
  `[mem] pressure L -> L'`。
- **进程停止**：SIGSTOP 由内核 `sched_cfs::stop_current` 置 `TaskState::Stopped`
  并切走；SIGCONT 由 `cont_pid` 恢复为 Ready。`/proc/processes` 的 `T` 标记
  与 `poll_deaths`（`kill(pid,0)`）都以此为据，与 lifd 注册表状态一致。
- **命令注入**：lifd 主循环 50ms 轮询信箱（`poll_mailbox`），`cmd_<pid>` 读后
  删除——测试中每条命令写后加 50ms~2s 等待，断言带超时，不依赖精确时序。

## 5. 回归入口

| 目标 | 内容 | 对应文档 |
|---|---|---|
| `make lifecycle-test` | 阶段 A 自动化（无头；不装 GUI） | 本文档 §2 |
| `python3 tests/scripts/lifecycle_test.py --gui` | 阶段 B（需显示设备） | 本文档 §3 |
| `make power-test` | 电源管理回归（与 Doze/节流协同） | docs/power_management.md |
| `make run-gui` + 手动操作 | 真机行为抽查（lcapp 图标 → Home → 恢复） | 本文档 §3 |

## 6. 已知限制（非本期验收项）

- 挂起/恢复的帧率预算、SFS 写入开销、推送唤醒与 120FPS 优先级冲突的**实测**
  数据需在真机（Pi4）采集；本期交付逻辑正确性（状态机/时限/投递路径）与
  延迟分解见 lifecycle.md §9。
- 远程推送通道（APNS/FCM 等价物）为接口预留（pushd `RemotePushChannel`），
  不在此测试范围。