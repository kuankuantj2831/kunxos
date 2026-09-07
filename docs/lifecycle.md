# KunxOS 应用生命周期与后台管理系统设计

> 本文档定义 KunxOS 的应用生命周期状态机、后台任务策略、内存压力管理（LMK）、
> 推送框架与状态持久化，并回答任务提出的三个关键问题。
> 实现语言：Rust（内核接口 + 用户态系统服务）。与现有 CFS 调度器集成（不替换）。

## 1. 架构总览

```
┌─────────────────────────────────────────────────────────────────┐
│ EL1 内核（kunxcore）                                             │
│  ├─ sched_cfs: CFS 调度器（不替换；新增 TaskState::Stopped）     │
│  ├─ signal:    SIGSTOP=停 / SIGCONT=继续（原 SIGSTOP=杀，已改）   │
│  ├─ mem_pressure: 定时器 tick 计算内存压力等级（Normal/Mod/Crit） │
│  └─ procfs:    /proc/pressure、/proc/processes（含页数）          │
└──────────────┬──────────────────────────────────────────────────┘
               │ syscall（kill/setpriority/wait4+WNOHANG/自定义）
┌──────────────▼──────────────────────────────────────────────────┐
│ EL0 系统服务                                                     │
│  ├─ kunxinit (PID1)  ── 拉起 lifd/pushd/networkd/launcher        │
│  ├─ lifd  ── 应用状态机 / 后台任务审核 / 超时挂起 / LMK           │
│  │    控制面：/data/life/{reg_*, cmd_<pid>, evt_<pid>, launcher_evt}│
│  ├─ pushd ── 本地通知队列 / 远程推送接口（预留 APNS/FCM 等价物）  │
│  └─ launcher ── 应用 fork/exec、帧快照与恢复、前台转场            │
│        └─ 应用（app_kit + lifecycle.rs，注册 on_pause/... 回调）  │
└──────────────────────────────────────────────────────────────────┘
```

职责划分：
- **kunxinit（PID 1）**：只负责拉起系统服务与回收僵尸，**不管理应用生命周期**。
- **launcher**：应用进程的父进程（fork/exec/reap），持有帧缓冲（fb0）的所有权与
  前台/桌面转场动画。非阻塞 wait4(WNOHANG) 轮询 + lifd 事件驱动。
- **lifd（Lifecycle Daemon）**：应用状态机唯一权威（authority），持有
  pid→{状态, 后台任务, LRU} 注册表，向应用/launcher 发事件与信号。
- **pushd（Push Daemon）**：通知队列权威；投递动作（唤醒谁）委托 lifd
  （因为只有 lifd 知道应用是否存活/处于何状态）。

## 2. 应用生命周期状态机

```
                    ┌─────────────────────────────────────────────┐
                    │                                             │
   launcher fork    ▼      Home / 切换应用 / 系统请求             │
  +exec ───────► ┌─────────┐  ───────────────────────────────► ┌──────────┐
                 │ Running │                                   │Background│
                 │ (前台)   │ ◄─────────────────────────────── │ (后台)    │
                 └─────────┘    恢复（SIGCONT + RESUME）        └────┬─────┘
                    │  ▲                                            │
                    │  │ 应用正常退出（Back）                        │ 挂起超时(BG_TIMEOUT)
                    │  │                                            │ 或内存压力
                    ▼  │                                            ▼
                 ┌─────────┐   LMK / 用户终止 / 崩溃         ┌──────────┐
                 │Terminated│ ◄──────────────────────────── │ Suspended │
                 └─────────┘   TERMINATE 事件 → SIGKILL     │ (挂起)    │
                    ▲                                        └──────────┘
                    │                                            │
                    └── launcher wait4 回收僵尸 → 槽位释放 ◄──────┘
                        （Terminated 是终态；恢复只能从
                          Background/Suspended 回到 Running）
```

### 2.1 状态定义（lifd 注册表内）

| 状态 | 进程可调度? | 可绘帧? | LMK 候选? | 说明 |
|---|---|---|---|---|
| `Running` | ✅（nice −3） | ✅ 独占 fb0 | ❌ | 前台应用，接收输入 |
| `Background` | ❌（SIGSTOP 停） | ❌ | 弱 | 已暂停执行，内存/状态保留；可能持有后台任务 |
| `Suspended` | ❌（SIGSTOP 停） | ❌ | 强（先杀） | 后台超时/压力后转入；UI 状态已落盘 |
| `Terminated` | — | — | — | 进程已 SIGKILL/退出，等待 launcher 回收 |

> 说明：单 fb0 模型下，后台应用被内核 SIGSTOP 冻结（内存完整保留），
> "Background" 与 "Suspended" 的执行行为相同（均停止），区别在于 lifd 的
> 记账：Suspend 使应用成为 LMK 首选、并保证其 UI 状态已持久化。

### 2.2 转换触发与回调钩子

| 转换 | 触发源 | lifd 动作 | 应用回调 | 备注 |
|---|---|---|---|---|
| New → Running | launcher fork+exec 完成、首帧放行 | 注册表登记；事件 RESUME | `on_resume` | 应用可忽略（首帧直接画） |
| Running → Background | ① 应用内 Home 手势（`lc_home`）② launcher 切走 ③ 系统请求 | 写 `PAUSE`→`SUSPEND` 事件；宽限窗后 SIGSTOP；告知 launcher "BG" | `on_pause` → `on_suspend` | 两个回调都在 SIGSTOP 前的宽限窗（默认 120ms）内执行 |
| Background → Running | launcher 点图标/最近任务卡片（`FG <pid>`） | SIGCONT；写 `RESUME` 事件；nice −3；告知 launcher "FG" | `on_resume` | launcher 先恢复应用帧快照再放行 |
| Background → Suspended | 挂起超时（默认 60s）或内存 Moderate | 标记 Suspended（应用已停止，无需回调） | — | 状态已在对 Background 转换时保存 |
| Background/Suspended → Terminated | LMK（内存 Critical）或系统终止 | 写 `TERMINATE` 事件 → 宽限（默认 80ms）→ SIGKILL → 告知 launcher "KILLED" | `on_terminate` | 回调尽力而为（可能来不及） |
| Running → Terminated | 应用正常退出（Back/ESC） | 从注册表删除；告知 launcher "EXITED" | — | launcher wait4 回收 |

### 2.3 事件文件协议（/data/life/）

lifd 与应用/launcher 之间用**文件信箱**通信（与现有 launch_gate 风格一致，
可 cat 调试、可注入测试）：

| 文件 | 写入方 | 内容 | 说明 |
|---|---|---|---|
| `reg_<name>` | launcher | `pid\n` | 应用启动登记（lifd 建注册表项） |
| `cmd_<pid>` | 应用/launcher | `BG`/`RECENTS`/`FG <pid>`/`BTASK <ty> <ms>`/`BTASK_END`/`ACK`/`EXITED`/`DELIVER <pid> <id> <payload>`/`SET_TIMEOUT <ms>` | 命令信箱，lifd 轮询（50ms）。`DELIVER` 由 pushd 提交（通知投递）；`SET_TIMEOUT` 为测试/运维钩子（覆盖「后台→挂起」超时，加速验证超时挂起路径，默认 60s） |
| `evt_<pid>` | lifd | `PAUSE`/`SUSPEND`/`RESUME`/`TERMINATE`/`BG_TASK_EXPIRED`/`NOTIFY <id> <title>|<body>` | 应用事件信箱（应用每帧轮询） |
| `launcher_evt` | lifd | `BG <name> <pid>`/`FG <name> <pid>`/`KILLED <name> <pid>`/`EXITED <name> <pid>` | launcher 事件信箱 |
| `recents` | lifd | `name pid state\n`… | 最近任务列表（launcher 读取） |

命令/事件为单行 ASCII（≤128B）。事件信箱采用"最新覆盖"语义（后写覆盖先写），
生命周期事件幂等，可容忍丢失。

## 3. 应用侧 API（shared/lifecycle.rs，经 app_kit 暴露）

```rust
// 回调注册（默认全为 no-op；on_resume 默认触发重绘标记）
pub struct LcCallbacks {
    pub on_pause:     fn(),             // 暂停动画/音频、轻量清理
    pub on_resume:    fn(),             // 回到前台：重绘/恢复
    pub on_suspend:   fn(),             // 保存 UI 状态到 SFS（lc_save_state）
    pub on_terminate: fn(),             // 终止前最终保存
}

pub fn lc_init() -> bool;                        // 懒初始化（应用名从 argv[0] 推导）
pub fn lc_poll() -> LcEvent;                     // 每帧轮询（已挂入 poll_input）
pub fn lc_home() -> bool;                        // Home 手势：后台化，阻塞至恢复后返回 true
pub fn lc_go_recents() -> bool;                  // 最近任务：后台化，恢复后返回
pub fn lc_handle_nav(a: NavAction);              // Back→exit；Home/Recents→后台化
pub fn lc_request_bg_task(ty: BgTaskType, dur_ms: u32) -> bool; // 申请后台任务
pub fn lc_end_bg_task() -> bool;                 // 提前释放
pub fn lc_save_state(data: &[u8]) -> bool;       // 写 /data/state/<app>.bin
pub fn lc_restore_state(buf: &mut [u8]) -> usize;// 读回（0=无状态）
pub fn lc_notify_local(title: &[u8], body: &[u8]);   // 本地推送（经 pushd）
pub fn lc_needs_redraw() -> bool;                // 恢复后置位（应用可查）
```

应用主循环只需把 `exit(nav_exit_code(ev.nav))` 换成 `lc_handle_nav(ev.nav)`，
即可获得：Home=后台化（不再被杀）、恢复后继续、状态保存回调。
`poll_input()` 内部自动调用 `lc_poll()`，多数应用零改动即可后台化。

## 4. 后台任务白名单（lifd 审核）

| 类型 | 代码 | 典型时长上限 | 说明 |
|---|---|---|---|
| Audio（音频播放） | 0 | 3600s（按播放状态续期） | 后台播放 |
| Location（定位） | 1 | 1800s | 后台定位 |
| Voip（语音通话） | 2 | 7200s | 通话保持 |
| Download（下载） | 3 | 1800s | 大文件下载 |

审核规则（lifd `audit_bg_task`）：
1. 类型必须属于白名单（其余拒绝并回 `BG_TASK_DENIED`）。
2. 每应用同时最多 2 个后台任务。
3. 全系统持有后台任务的应用 ≤ 3 个（超出按 LRU 挤占）。
4. 时长超限 → 任务过期：发 `BG_TASK_EXPIRED` 事件，应用应立即收尾；
   再宽限 10s 后 lifd 强制回收（该应用转为可挂起）。

持有后台任务的应用**不会被挂起超时转入 Suspended**（保留可被唤醒的执行能力
——本实现中即不被 SIGSTOP 之外的动作打扰；SIGSTOP 语义不变，任务到期前
lifd 不将其列入 LMK 高优先级）。

## 5. 内存压力管理与 LMK

### 5.1 内核侧（kunxcore::mem_pressure）

- 定时器 tick（10ms）内每 100ms 计算一次：`free = total_pages − used_pages`。
- 等级：`Normal`（free ≥ 25%）、`Moderate`（10% ≤ free < 25%）、
  `Critical`（free < 10%）。等级变化时打印内核日志（可观测的"内核触发事件"）。
- 暴露：`/proc/pressure`（`level free_kB total_kB`）+ 自定义系统调用
  `SYS_LC_MEMPRESSURE`（x0 返回 0/1/2）。
- 测试注入（QEMU 回归用）：`/proc/pressure` 可写——`force 0|1|2` 强制等级、
  `clear` 恢复按真实空闲页计算（`kunxcore/src/procfs.rs` → `mem_pressure::handle_cmd`）；
  供 lifecycle 测试确定性触发挂起/LMK，而不必真实耗尽 512MB。
- `/proc/processes` 增加每进程用户页数（LMK 评分用）。

### 5.2 lifd 响应策略

| 压力等级 | lifd 动作 |
|---|---|
| Normal | 无动作；恢复被挂起的可恢复项（若有） |
| Moderate | ① 把所有 Background（无后台任务）立即转 Suspended（状态已保存，纯记账）② 停止授予新后台任务 |
| Critical | LMK：按 `score = 状态优先 + LRU + 任务罚分` 升序击杀，直到低于 Critical |

LMK 击杀顺序（score 小者先杀）：
1. Suspended（按 LRU 最久未用先杀）
2. Background 无后台任务（LRU）
3. Background 有后台任务（按类型：Download < Location < Voip < Audio）
4. 永不杀：Running（前台）、launcher、lifd/pushd/networkd 等系统服务

击杀流程：写 `TERMINATE` 事件 → 宽限 80ms（应用执行 on_terminate）→
SIGKILL → 注册表删除 → 通知 launcher "KILLED <name> <pid>"。

## 6. 推送框架（pushd）

### 6.1 本地推送（本期实现）

```
应用 ──写 /data/push/in/<pid>_<seq>──► pushd（队列权威）
                                        │  分配 id，持久化到 /data/push/queue/
                                        ▼
                                  向 lifd 提交 "DELIVER <app> <title>|<body>"
                                        ▼
   lifd 按应用状态投递：
     Running   → evt_<pid>: NOTIFY（应用下一帧自绘横幅，不打断前台动画）
     Background/Suspended → SIGCONT 唤醒 → evt_<pid>: NOTIFY → 应用处理后
                          → 若原为 Suspended，lifd 按「推送唤醒配额」决定是否
                            立即重新挂起（默认处理完即重新挂起）
     Terminated → 持久化 /data/push/pending/<app>/，下次启动时补投
```

### 6.2 远程推送（接口预留）

`pushd` 暴露远程推送适配接口（当前为日志 + 持久化的占位实现，等价物
APNS/FCM 的通道层留待后续）：

```rust
// pushd 内部（预留，非本期实现）：
pub trait RemotePushChannel {
    fn send(&mut self, token: &[u8], payload: &[u8]) -> Result<(), PushError>;
    fn poll_ack(&mut self) -> Option<PushAck>;
}
// 设备侧注册：应用调用 lc_register_remote_push() → pushd 生成设备 token
// （占位：本地随机 + 持久化），预留与远程服务握手接口。
```

远程推送到达后的投递路径与本地推送完全一致（复用 lifd 投递）。

## 7. 状态持久化（State Restoration）

- 保存时机：`on_suspend`（每次后台化时，SIGSTOP 前宽限窗内）。
- 路径：`/data/state/<app>.bin`（SFS 持久卷，重启保留）。
- 恢复时机：应用启动时（`lc_restore_state`）或 `on_resume` 时。
- 体积约束：单文件 ≤ 4MB（SFS 上限）；推荐 ≤ 256KB。
- 崩溃安全：先写临时文件再 rename（SFS rename 为元数据操作）。

## 8. 与 CFS 调度器集成（不替换）

| 状态 | nice（CFS 权重） | 机制 |
|---|---|---|
| Running（前台） | −3（权重 2000，约 2 倍份额） | execve 默认；lifd 在 FG 时 `setpriority` 维持 |
| Background/Suspended | +2（权重 ~550） | lifd 在 BG 时 `setpriority` 调低（进程已 SIGSTOP，权重仅影响恢复后瞬时） |
| 系统服务 | 0 | 不变 |

实现：`setpriority(pid, prio)` 从 no-op 改为真实实现（root 可改任意 pid 的
所有线程 nice → CFS weight）。后台任务到期前，lifd 可把持有任务的应用
nice 提到 0（仍低于前台）。

## 9. 关键问题回答

### Q1. 应用状态切换的延迟预算？（挂起/恢复是否允许掉帧）

| 路径 | 预算 | 分解 | 允许掉帧? |
|---|---|---|---|
| Home（挂起前台应用） | ≤ 300ms | lifd 轮询 50ms + PAUSE/SUSPEND 事件 + 宽限 120ms + SIGSTOP；与 launcher 返回动画**并行** | 挂起本身无帧可掉（用户在看桌面返回动画）；返回动画期间允许丢帧（现有 250ms 停滞压缩机制兜底） |
| 恢复（Background→Running） | ≤ 100ms 出首帧 | SIGCONT（内核 <1ms）+ 快照恢复（2MB memcpy ≈ 5ms）+ RESUME 事件 ≤50ms | 允许掉帧（用户预期是"应用回来了"）；launcher 的 Resume 转场用快照缩放，掉帧不感知 |
| 推送唤醒 | ≤ 50ms 唤醒 | SIGCONT <1ms + 事件 ≤50ms | 不涉及前台动画（见 Q3） |

### Q2. 挂起应用的数据（如游戏进度）保存到 SFS 的同步开销？

- 小状态（配置/UI 状态，1–10KB）：SFS 单次写入 μs 级，落在 120ms 宽限窗内
  完全无感。
- 中等状态（游戏进度，10–256KB）：写入约 0.1–2ms（SFS 位图分配 + 数据拷贝），
  宽限窗内完成；**保存是同步的（on_suspend 返回即落盘）**，保证 SIGSTOP 后
  状态一致。
- 大状态（>256KB）：建议分块/延迟写入；本期对 `on_suspend` 的写入量做上限
  检查（lifd 的 ACK 超时 = 宽限窗，超时未 ACK 仍强制 SIGSTOP，避免挂起路径
  被磁盘拖死）。
- 结论：同步保存开销可接受；保存动作在 SIGSTOP 之前完成，挂起/终止不丢状态。

### Q3. 推送唤醒与 120 FPS 动画的优先级冲突如何解决？

- **前台应用**：推送到达 → 只写 `evt_<pid>` 通知事件，**不打断**前台应用的
  帧循环；横幅由应用在下一帧自行绘制（非抢占式）。
- **后台/挂起应用**：推送唤醒 = SIGCONT + 事件；被唤醒应用只做轻量处理
  （更新状态/发通知），随后可立即重新挂起。唤醒期间的 CPU 争夺由 CFS 权重
  仲裁：前台 nice −3（权重 2000）≈ 后台 nice +2（权重 ~550）的 3.6 倍份额，
  前台 120Hz 动画帧预算（8ms）不受影响。
- **内核级保证**：SIGCONT/SIGSTOP 是 O(1) 状态翻转，不经过调度队列重排；
  推送唤醒不会触发抢占式切换抢占前台（CFS 按 vruntime 选择，前台因权重高
  而 vruntime 增长慢，天然优先）。

## 10. 目录/文件清单（本期交付）

```
docs/lifecycle.md            ← 本文档
docs/lifecycle-test.md       ← 测试方案（见下）
kunxcore/src/sched_cfs.rs    ← TaskState::Stopped / stop_current / cont_pid / set_pid_nice
kunxcore/src/signal.rs       ← SIGSTOP=停 / SIGCONT=继续
kunxcore/src/linux_compat.rs ← kill(SIGCONT)、wait4 WNOHANG、setpriority、SYS_LC_MEMPRESSURE
kunxcore/src/mem_pressure.rs ← 内核内存压力状态机（新）
kunxcore/src/allocator.rs    ← free_pages()
kunxcore/src/procfs.rs       ← /proc/pressure、/proc/processes 页数列
kunxserv/lifd/src/main.rs    ← 生命周期守护进程（新）
kunxserv/pushd/src/main.rs   ← 推送守护进程（新）
shared/lifecycle.rs          ← 应用侧生命周期 API（新）
shared/app_kit.rs            ← poll_input 挂接 lc_poll
apps/launcher/src/app_life.rs← launcher×lifd 集成（新）
apps/launcher/src/main.rs    ← 非阻塞 wait4 + 帧快照/恢复 + Resume 转场
kunxinit/src/main.rs         ← 拉起 lifd/pushd
Makefile                     ← lifd/pushd 构建与 rootfs 集成
```

## 11. 明确不做（范围约束）

- 不实现 Android ActivityManager / iOS SpringBoard 的窗口栈、任务亲和、
  Activity 实例管理等复杂机制——本系统为"单前台全屏应用 + 后台存活"模型。
- 不实现远程推送的真实通道（APNS/FCM 等价物仅预留接口）。
- 不实现应用内多 Activity/场景恢复（仅整应用 UI 状态文件）。
- 不替换 CFS 调度器（仅新增状态与优先级接口）。
