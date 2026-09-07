# KunxOS 电源与热管理子系统

本文档描述 KunxOS 的电源管理架构：HAL 抽象 → 平台后端 → 策略引擎，
以及 CPU DVFS、热管理、Idle 状态机、电池模型与后台冻结（Doze）的实现
与测试方案。目标平台：QEMU virt（逻辑验证）与 Raspberry Pi 4（真机验证）。

## 1. 架构

```text
┌────────────────────────── 内核策略引擎（平台无关） ──────────────────────────┐
│  kunxcore/src/power.rs                                                        │
│                                                                              │
│  定时器 tick（10ms，IRQ 上下文）                                              │
│    └─ power::tick()                                                          │
│        ├─ 每 100ms  load sample（每核 idle 时间差）                           │
│        │    └─ governor 决策（ondemand/conservative/powersave/performance）   │
│        │        └─ 热节流上限覆盖 ──► DVFS 切换（≥100ms 间隔防抖）             │
│        ├─ 每 200ms  热采样（HAL 传感器 / QEMU 软件热模型）                    │
│        │    └─ 级别判定（normal/warm/hot/critical/emergency）                 │
│        │        └─ Hot+：频率上限压降（force，绕过切换间隔）                  │
│        └─ 每 1000ms 电池模型（HAL 电池 / 虚拟电池）                           │
│             └─ 充电状态机（discharging/charging/full）+ 事件通知              │
│                                                                              │
│  调度器 hook（sched_cfs.rs）：                                               │
│    - idle 任务：WFI 记账（enter_wfi/exit_wfi）→ 负载与唤醒统计                │
│    - CFS：cpu_cap（Doze 限速，vruntime ×100/cap）、pick_next 跳过冻结任务     │
│                                                                              │
│  控制面：shell `power` 命令 / /proc/power、/proc/thermal / syscall 410-413   │
└───────────────┬──────────────────────────────────────────────────────────────┘
                │ 只依赖 trait（kunxhal::power）
┌───────────────▼──────────────────────────────────────────────────────────────┐
│  HAL 抽象层  kunxhal/src/power.rs                                            │
│    PowerControl（关机/重启/休眠）   ThermalSensor（毫摄氏度）                 │
│    DvfsController（频点表/频率，按簇）   Battery（电量/充电）                 │
│    Platform::power_control/thermal_sensor/dvfs_controller/battery/           │
│              core_voltage_mv（默认 None，可覆盖）                             │
└───────────────┬──────────────────────────────────────────────────────────────┘
   ┌────────────┴─────────────┐
┌──▼───────────────┐   ┌──────▼─────────────────┐
│ QEMU virt 后端    │   │ Raspberry Pi 4 后端    │
│ plat_qemu.rs      │   │ plat_pi4.rs            │
│  PSCI hvc 关机/重启│   │  PSCI smc 关机/重启    │
│  虚拟 DVFS（表真实 │   │  mailbox GET_TEMP      │
│  切换为记账）       │   │  mailbox GET/SET_CLOCK │
│  热/电池 = None    │   │  _RATE（ARM 时钟 0x3） │
│  （内核用模拟）     │   │  电池 = None          │
└───────────────────┘   └───────────────────────┘
```

分层原则：策略引擎零硬件知识；平台后端零策略。QEMU 与 Pi4 共用同一套
governor / 节流 / 状态机代码，差异只在 trait 实现。

## 2. CPU DVFS

### 2.1 频点表

Pi4（Cortex-A72）与 QEMU 虚拟表一致，600/900/1200/1500 MHz：

| 频点 | 用途 |
|------|------|
| 600 MHz | 最低频：空闲 / Critical 热级别上限 / powersave |
| 900 MHz | Hot（≥80°C）热级别频率上限 |
| 1200 MHz | 中间档：中等负载 |
| 1500 MHz | 最高频：性能 / 满载 |

`FreqPoint { hz, uv }` 含电压字段，但 v1 **只切频不切压**（Pi4 核心电压
由固件自动管理；`GET_VOLTAGE` 仅用于状态显示）。真机接入时由平台后端
在 `set_hz` 内决定是否联动电压。

### 2.2 负载采样

- 每核 idle 任务在 `wfi` 前后用 `timer::now_us()` 记账（`exit_wfi` 累加
  `IDLE_TOTAL[cpu]`，原子，SMP 安全）。
- 每 100ms：`load[cpu] = (window − idle_delta) / window`，聚合负载为各核平均。
- 空闲 shell 会让出（`thread::yield_now`）→ idle 任务 → wfi → 负载 ≈ 0；
  忙等任务无 idle 时间 → 负载 ≈ 100%。

### 2.3 切换策略（governor）

| governor | 升频 | 降频 | 说明 |
|----------|------|------|------|
| ondemand（默认） | 负载 ≥ 75% | 负载 ≤ 35% | 连续 2 个采样才动作（迟滞） |
| conservative | 负载 ≥ 90% | 负载 ≤ 20% | 优先稳定频率 |
| powersave | 恒为最低频 | — | 续航优先 |
| performance | 恒为最高频 | — | 性能优先 |

防抖措施：最小切换间隔 100ms；迟滞计数避免临界负载抖动。热节流路径
（Hot+ 降频）**绕过**切换间隔，保证安全降频即时生效。

## 3. 热管理

### 3.1 温度读取

- **Pi4**：mailbox 属性通道 `GET_TEMP`（tag `0x00030006`，temperature_id=0），
  返回毫摄氏度（0.001°C 单位；Linux `raspberrypi-hwmon` / `vcgencmd
  measure_temp` 均按毫摄氏度解析）。完整协议见第 7 节。
- **QEMU**：无温度传感器 → **软件热模型**（一阶微分方程）：

  ```text
  dT/dt = H·load − C·(T − T_ambient)
  H = 0.018 °C/s 每 1% 负载；C = 0.04 /s；T_ambient = 40°C
  稳态：满载 85°C / 半载 62°C / 空闲 44°C；时间常数 25s
  ```

  也支持 `power thermal force <C>` 直接注入温度（测试节流路径）。

### 3.2 多级节流

| 级别 | 阈值 | 动作 |
|------|------|------|
| normal | < 70°C | 不限频 |
| warm | 70~80°C | 仅记录（为升频预留余量） |
| hot | 80~85°C | 频率上限 → 900 MHz |
| critical | 85~90°C | 频率上限 → 最低频（600 MHz） |
| emergency | ≥ 90°C | 最低频 + 告警事件；SMP 下尝试关核（见下） |

级别变化写入事件环（`thermal-critical` / `thermal-recover`）并打印 UART
日志（`[power] thermal 82 C -> hot`）。**关核**：`enter_deep_idle(cpu)` 为
预留接口（Pi4 需固件/CPRMAN 寄存器支持）；当前 SMP 未启用时 Emergency
退化为最低频 + 告警（CPU0 不可关）。

## 4. Idle 状态机

```text
Active ──(调度器无就绪任务)──► Idle ──(idle 任务运行)──► Wfi
  ▲                                                       │
  └──────────(IRQ 唤醒 → resched_if_needed)───────────────┘
```

- `enter_wfi(cpu)` / `exit_wfi(cpu, idle_us)` 由每核 idle 任务调用，
  维护 `IDLE_PHASE`（active/idle/wfi）、`WFI_COUNT`、`WAKE_COUNT`。
- 唤醒路径 = 现有 IRQ 分发：定时器（调度 tick / 动画 CNTP）或设备中断
  → `irq_handler` → `resched_if_needed` → 切换回就绪任务。
- 深空闲（关核时钟）为占位接口，Pi4 需固件支持（v2）。

## 5. 电池模型框架（为真机预留）

- 接口：`Battery { present, capacity_percent, charging, plugged, voltage_mv }`。
- 状态机：`charging_state(pct, plugged)`：
  `discharging → (plugged, pct<100) → charging → (pct==100) → full → (unplugged) → discharging`。
- 电量曲线：`battery_step` 虚拟模型（充 1%/s、放 1%/5s）；真机接入时由
  平台后端实现电压→电量查表曲线。
- 事件通知（16 槽环形缓冲，/proc/power 可读）：
  `battery-low`（≤15%，+5% 迟滞复位）、`battery-full`、`plugged`、
  `unplugged`、`charging-start`。
- 应用侧：`syscall 410 (kunx_pm_get)` 打包返回 电量/温度/频率/负载/电池状态/
  低电量位，低成本轮询；无需读 /proc。

## 6. 后台冻结（Doze）

双机制（互为补充）：

1. **CPU 上限（cpu_cap 0..100）**：CFS 累加 vruntime 时按 `100/cap` 放大
   —— cap=10% 的任务获得约 10% CPU。设置接口：
   - syscall 412 `kunx_pm_set_cpu_cap(pid, cap)`
   - shell `power cap <pid> <0-100>`
2. **软冻结（cap=0）**：保持 Ready 但 `pick_next` 永不选中；syscall 413
   `kunx_pm_freeze(pid, 0/1)`、shell `power freeze/unfreeze <pid>`。
   （与既有的 SIGSTOP `Stopped` 状态互补：后者还能被 `kill_pid` 清理。）

生命周期接线点：launcher/lifd 在应用进入后台时调用 `kunx_pm_set_cpu_cap`
或 SIGSTOP；当前单前台模型下经 shell 演示（`power cap <pid> 10` 后观察
`/proc/power` 的 doze 行）。

## 7. Pi4 mailbox 协议（关键问题回答）

### 7.1 获取 CPU 温度

- 通道：VideoCore VI mailbox 属性通道（channel 8，基址 `0xFE00B880`）。
- 消息缓冲 16 字节对齐；地址须为 VC bus 地址（`0xC0000000 | 物理地址`）；
  请求前 `DC CVAC` clean，响应后 `DC IVAC` invalidate（现有
  `plat_pi4.rs` 已实现全部机制，`property_query` 泛化复用）。
- 标签：

  ```text
  GET_TEMP  0x00030006
  请求 value: [temperature_id]          （0 = SoC）
  响应 value: [temperature_id, temp]    （毫摄氏度，0.001°C）
  ```

### 7.2 设置/读取 CPU 频率（DVFS）

- 时钟 id：`0x3 = ARM`（Mailbox Clock IDs：0=EMMC, 1=UART, 2=?, 3=ARM…）。

  ```text
  GET_CLOCK_RATE  0x00030002  [clock_id] → [clock_id, rate_hz]
  SET_CLOCK_RATE  0x00038002  [clock_id, rate_hz] → [clock_id, rate_hz]
  GET_MAX_CLOCK_RATE 0x00030004；GET_MIN_CLOCK_RATE 0x00030007
  ```

- 固件会把请求频率 clamp 到支持范围并返回生效值；Pi4 支持 100MHz 步进
  （600–1500 MHz）。
- 电压（仅读取，不切换）：`GET_VOLTAGE 0x00030003 [voltage_id]`，
  voltage_id `0x1 = Core`；返回值为「相对 1.2V 的 0.025V 步进偏移」。

### 7.3 QEMU 是否有温度传感器模拟？

- `-M virt` **没有**热传感器设备，其 DTB 也无 `thermal-zones` 节点；
  QEMU 虽有 i2c 传感器模型（tmp105 等），但 virt 机器默认无 i2c 总线且
  不关联 CPU 热行为。
- 结论：无法经设备树获得真实温度 → 采用**内核侧软件热模型**（第 3.1 节），
  由 CPU 负载驱动，配合 `power thermal force` 注入测试节流路径。
  备选（未采用）：`-dtb` 注入自定义 thermal-zones 节点，但节点无硬件
  支撑，仍需软件驱动，收益有限。

### 7.4 DVFS 切换延迟对 120 FPS 动画的影响？

- Pi4 的 mailbox SET_CLOCK_RATE 往返约 10–100µs（mailbox 延迟 + PLL 重锁），
  120Hz 帧预算 8.33ms —— 单次切换只占 <1.2% 帧预算，**切换本身不掉帧**。
- 真正风险是（a）切换过频导致持续开销、（b）负载高时降到低频使帧渲染
  变慢（吞吐而非延迟）。缓解：100ms 采样 + 100ms 最小切换间隔 + 迟滞；
  切换发生在定时器 IRQ（帧间），不会落在帧渲染中途（动画走独立 CNTP
  通道，二者不共享相位）。
- QEMU：虚拟 DVFS 无时序效果，切换决策与日志真实。

### 7.5 big.LITTLE 是否在范围？

- 不在实现范围：Pi4 是四核 A72 同构。但 `DvfsController` 按**簇**抽象
  （`cluster_count` / 每簇频点表与频率），策略引擎按簇下发决策 ——
  未来 big.LITTLE 只需平台后端暴露两簇，调度器侧按簇迁移任务为后续工作。

## 8. 测试方案

### 8.1 QEMU 逻辑验证（`make power-test`）

`tests/scripts/power_test.py` 启动内核后经串口驱动：

1. `power selftest` —— 纯函数自检（频点导航 / governor 迟滞 / 热上限 /
   热模型稳态 / 充电状态机 / 电量变化 / 打包位），期望 `0 failure(s)`。
2. `power status` —— 字段齐全（governor/load/freq/thermal/battery/idle/events）。
3. `power thermal force 95` → 等待热采样 → `power status`：
   level=critical/emergency 且 freq 降到 600 MHz；日志出现 `[power] dvfs`。
4. `power thermal clear` → 恢复 normal。
5. `power governor powersave` → freq 600 MHz。
6. `power battery 10` → LOW 告警；`power plug 1` → charging + 事件。
7. `cat /proc/power` 可读。

手动验证：`make run` 后敲 `power status` / `power thermal force 95` /
`power governor performance` 观察 UART 上的切换日志。

### 8.2 Pi4 真机温度压力测试

1. 构建：`make pi4`（编译检查）+ 真实启动（bootloader → 内核）。
2. 基线：`power status` 记录空载温度（通常 40–55°C）与频率。
3. 压力：多核忙等（如 busybox 计算 + 渲染循环），观察：
   - `[power] dvfs` 升频到 1500 MHz；
   - 温度经 mailbox GET_TEMP 上升，80°C 后 `[power] thermal ... -> hot`，
     频率被压到 900 MHz；继续升温至 85°C → 600 MHz（critical）；
   - 温度回落后恢复（级别 normal → 频率按 governor 回升）。
4. 检查 /proc/power 的事件列表与 doze 状态。
5. 安全验证：被动散热条件下温度不应超过 90°C（emergency 是最后防线；
   若超过需检查散热 / 降低 H 系数）。

> 注意：真机 DVFS 依赖 VideoCore 固件（mailbox），确保 config.txt 未
> 锁定 arm_freq / force_turbo=1（否则 SET_CLOCK_RATE 可能被 clamp）。

## 9. 已实现文件清单

| 文件 | 内容 |
|------|------|
| `kunxhal/src/power.rs` | 电源抽象 trait + FreqPoint |
| `kunxhal/src/plat.rs` | Platform 增加 5 个电源默认方法 |
| `kunxhal/src/plat_qemu.rs` | PSCI hvc 电源 + 虚拟 DVFS 表 |
| `kunxhal/src/plat_pi4.rs` | PSCI smc + mailbox GET_TEMP / SET_CLOCK_RATE / GET_VOLTAGE |
| `kunxcore/src/power.rs` | 策略引擎：governor / 热 / idle / 电池 / Doze / selftest |
| `kunxcore/src/sched_cfs.rs` | cpu_cap 字段 + CFS 限速 + 冻结 + idle 记账 |
| `kunxcore/src/main.rs` | `mod power` + `power::init()` |
| `kunxcore/src/procfs.rs` | `/proc/power`、`/proc/thermal`（读 + 写控制） |
| `kunxcore/src/linux_compat.rs` | syscall 410-413 |
| `kunxcore/src/shell.rs` | `power` 命令 |
| `tests/scripts/power_test.py` | QEMU 回归测试 |
| `Makefile` | `make power-test` |
