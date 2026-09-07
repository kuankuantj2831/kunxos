//! 电源管理策略引擎（平台无关）。
//!
//! 本模块是 KunxOS 电源与热管理的核心：CPU DVFS（动态调频）、热节流、
//! Idle 状态机、电池模型与后台冻结（Doze）。硬件差异全部收敛在
//! [`kunxhal`] 的电源抽象（[`kunxhal::power`]），本模块只依赖 trait，
//! 因此同一套策略在 QEMU virt 与 Raspberry Pi 4 上行为一致。
//!
//! # 结构
//!
//! ```text
//!  定时器 tick（10ms，IRQ 上下文）
//!     └─ power::tick()
//!         ├─ 每 100ms：负载采样（每核 idle 时间差）→ governor 决策 → DVFS 切换
//!         ├─ 每 200ms：热采样（HAL 传感器 / QEMU 软件热模型）→ 多级节流
//!         └─ 每 1000ms：电池模型（HAL 电池 / 虚拟电池）→ 充电状态机 + 事件
//! ```
//!
//! # QEMU 可验证性
//!
//! QEMU 不模拟温度传感器与时钟树，因此：
//!   - 温度：HAL 返回 `None` 时启用**软件热模型**（一阶微分方程，由负载
//!     驱动：满载稳态 ≈ 85°C、半载 ≈ 62°C、空闲 ≈ 44°C），也可通过
//!     `power thermal force <C>` 直接注入温度以测试节流路径。
//!   - DVFS：HAL 提供虚拟控制器（频点表真实、切换为记账），governor /
//!     热节流的**决策与日志真实**，寄存器写为空操作。
//!   - 电池：HAL 返回 `None` 时启用**虚拟电池**（可经 `power battery/plug`
//!     注入电量与充电状态），状态机与事件通知真实。
//!
//! # 并发
//!
//! `tick()` 运行在定时器 IRQ 上下文（DAIF.I 置位）。负载 / 唤醒计数用
//! [`core::sync::atomic`]（SMP 下多核 idle 任务同时累加）；其余状态由
//! `tick` 独占写、线程上下文（shell / procfs）读 —— 状态展示允许读到
//! 不一致快照，u64 对齐读写天然原子，无需额外锁。

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use kunxhal::power::{DvfsController, FreqPoint};

// ==================== 常量与类型 ====================

/// 负载采样 + DVFS 决策周期（微秒）。调度 tick 为 10ms，累计 10 次。
pub const SAMPLE_US: u64 = 100_000;
/// 热采样周期（微秒）。
pub const THERMAL_POLL_US: u64 = 200_000;
/// 电池采样周期（微秒）。
pub const BATTERY_POLL_US: u64 = 1_000_000;
/// DVFS 最小切换间隔（微秒）：防止 governor 抖动导致的频繁切换。
pub const MIN_SWITCH_INTERVAL_US: u64 = 100_000;
/// 低电量阈值（%）。
pub const BATTERY_LOW_PCT: u8 = 15;

/// DVFS 调速器。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Governor {
    /// 按需：负载 ≥ 75% 升频、≤ 35% 降频（连续 2 个采样才动作）。
    Ondemand,
    /// 保守：负载 ≥ 90% 才升频、≤ 20% 才降频（优先稳定频率）。
    Conservative,
    /// 省电：恒为最低频点。
    Powersave,
    /// 性能：恒为最高频点。
    Performance,
}

impl Governor {
    pub const fn name(self) -> &'static str {
        match self {
            Governor::Ondemand => "ondemand",
            Governor::Conservative => "conservative",
            Governor::Powersave => "powersave",
            Governor::Performance => "performance",
        }
    }

    pub const fn from_id(id: u32) -> Option<Governor> {
        match id {
            0 => Some(Governor::Ondemand),
            1 => Some(Governor::Conservative),
            2 => Some(Governor::Powersave),
            3 => Some(Governor::Performance),
            _ => None,
        }
    }

    pub const fn id(self) -> u32 {
        match self {
            Governor::Ondemand => 0,
            Governor::Conservative => 1,
            Governor::Powersave => 2,
            Governor::Performance => 3,
        }
    }
}

/// 热级别（由温度阈值分段，见 [`thermal_level_of`]）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ThermalLevel {
    /// < 70°C：正常，不限频。
    Normal,
    /// 70~80°C：偏热，仅记录（为升频预留余量）。
    Warm,
    /// 80~85°C：热，频率上限压到 900MHz。
    Hot,
    /// 85~90°C：临界，频率上限压到最低频点。
    Critical,
    /// ≥ 90°C：紧急，最低频 + 尝试关核（SMP 下）并告警。
    Emergency,
}

impl ThermalLevel {
    pub const fn name(self) -> &'static str {
        match self {
            ThermalLevel::Normal => "normal",
            ThermalLevel::Warm => "warm",
            ThermalLevel::Hot => "hot",
            ThermalLevel::Critical => "critical",
            ThermalLevel::Emergency => "emergency",
        }
    }
}

/// 电池状态（充电状态机）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    Discharging,
    Charging,
    Full,
}

impl BatteryState {
    pub const fn name(self) -> &'static str {
        match self {
            BatteryState::Discharging => "discharging",
            BatteryState::Charging => "charging",
            BatteryState::Full => "full",
        }
    }

    pub const fn id(self) -> u32 {
        match self {
            BatteryState::Discharging => 0,
            BatteryState::Charging => 1,
            BatteryState::Full => 2,
        }
    }
}

/// 热阈值（°C）：Normal→Warm→Hot→Critical→Emergency。
pub const T_WARM_C: u32 = 70;
pub const T_HOT_C: u32 = 80;
pub const T_CRITICAL_C: u32 = 85;
pub const T_EMERGENCY_C: u32 = 90;
/// Hot 级别下的频率上限（Hz）：压到 900MHz（表内最接近且 ≤ 它的频点）。
pub const HOT_CAP_HZ: u32 = 900_000_000;

/// ondemand 调速阈值（%）。
const OND_UP: u8 = 75;
const OND_DOWN: u8 = 35;
/// conservative 调速阈值（%）。
const CONS_UP: u8 = 90;
const CONS_DOWN: u8 = 20;
/// 迟滞：连续 N 个采样满足条件才切换。
const HYST_SAMPLES: u32 = 2;

// 软件热模型参数（QEMU 模拟）。
/// 环境温度（毫摄氏度）。
const SIM_AMBIENT_MC: u32 = 40_000;
/// 温升系数：0.018 °C/s 每 1% 负载（满载 1.8°C/s）。
/// 散热系数：0.04 /s。稳态 T = 环境 + 负载% × 45°C（满载 85°C）。
/// 时间常数 τ = 25s。
const SIM_HEAT_PER_PCT_MS: u64 = 18; // mc/s per % → 每 ms：×dt/1000
const SIM_COOL_PER_MS: u64 = 40; // mc per ms per °C → 每 ms：×Δmc/1e6

/// 事件环形缓冲容量。
const EVENTS_CAP: usize = 16;

/// 电源事件类型（电池通知等）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PowerEventKind {
    BatteryLow,
    BatteryFull,
    Plugged,
    Unplugged,
    ChargingStart,
    ThermalCritical,
    ThermalRecover,
}

impl PowerEventKind {
    const fn name(self) -> &'static str {
        match self {
            PowerEventKind::BatteryLow => "battery-low",
            PowerEventKind::BatteryFull => "battery-full",
            PowerEventKind::Plugged => "plugged",
            PowerEventKind::Unplugged => "unplugged",
            PowerEventKind::ChargingStart => "charging-start",
            PowerEventKind::ThermalCritical => "thermal-critical",
            PowerEventKind::ThermalRecover => "thermal-recover",
        }
    }
}

/// 事件记录（时间戳 + 类型）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct PowerEvent {
    time_s: u64,
    kind: PowerEventKind,
}

// ==================== 状态 ====================

/// 当前调速器。
static mut GOV: Governor = Governor::Ondemand;

/// 上次各采样时刻（微秒）。
static mut LAST_SAMPLE_US: u64 = 0;
static mut LAST_THERMAL_US: u64 = 0;
static mut LAST_BATT_US: u64 = 0;

/// 每核累计 idle（wfi）时间（微秒）——由各核 idle 任务累加（原子）。
static IDLE_TOTAL: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 上次采样时各核 idle 累计值。
static mut LAST_IDLE: [u64; 4] = [0; 4];

/// 每核负载（%）。
static CURR_LOAD: [AtomicU8; 4] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];
/// 聚合负载（%）。
static AGG_LOAD: AtomicU8 = AtomicU8::new(0);

/// governor 迟滞连续计数。
static mut UP_STREAK: u32 = 0;
static mut DOWN_STREAK: u32 = 0;
/// 上次 DVFS 切换时刻（微秒，防抖）。
static mut LAST_SWITCH_US: u64 = 0;
/// 累计切换次数。
static SWITCH_COUNT: AtomicU64 = AtomicU64::new(0);

/// 当前热级别。
static mut THERMAL_LEVEL: ThermalLevel = ThermalLevel::Normal;
/// 最近一次采样到的温度（毫摄氏度）。
static THERMAL_TEMP_MC: AtomicU32 = AtomicU32::new(SIM_AMBIENT_MC);
/// QEMU 模拟：强制温度（毫摄氏度）；`None` 时用软件热模型。
static mut THERMAL_FORCE_MC: Option<u32> = None;
/// 软件热模型当前温度（毫摄氏度）。
static mut SIM_TEMP_MC: u32 = SIM_AMBIENT_MC;
/// 上次热级别（用于事件边沿检测）。
static mut LAST_THERMAL_LEVEL: ThermalLevel = ThermalLevel::Normal;

/// 虚拟电池：电量（%）。
static mut BATT_PCT: u8 = 80;
/// 虚拟电池：是否连接电源。
static mut BATT_PLUGGED: bool = false;
/// 当前电池状态。
static mut BATT_STATE: BatteryState = BatteryState::Discharging;
/// 低电量事件已触发标志（边沿去重，+5% 迟滞复位）。
static mut BATT_LOW_EVENTED: bool = false;
/// 上次充电状态（边沿检测）。
static mut LAST_BATT_STATE: BatteryState = BatteryState::Discharging;

/// 事件环形缓冲。
static mut EVENTS: [PowerEvent; EVENTS_CAP] = [PowerEvent { time_s: 0, kind: PowerEventKind::Plugged }; EVENTS_CAP];
static mut EVENT_HEAD: usize = 0;
static mut EVENT_COUNT: usize = 0;

/// 每核 idle 阶段（IdlePhase）。
static IDLE_PHASE: [AtomicU8; 4] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];
/// 每核 WFI 进入次数。
static WFI_COUNT: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// 每核被中断唤醒次数（WFI 退出次数）。
static WAKE_COUNT: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// 后台冻结（Doze）：进程 → CPU 上限（0=冻结，100=不限）。
/// 内核侧由 sched_cfs 维护（见 `sched_cfs::set_task_cpu_cap`），
/// 本模块仅提供查询渲染。
static mut FROZEN_COUNT: u32 = 0;

const MAX_CPUS: usize = 4;

// ==================== 纯函数（可单测） ====================

/// 频点表中「不超过 `hz` 的最大频点」（下限为表首）。
fn clamp_to_table(table: &[FreqPoint], hz: u32) -> u32 {
    if table.is_empty() {
        return hz;
    }
    let mut best = table[0].hz;
    for p in table {
        if p.hz <= hz {
            best = p.hz;
        } else {
            break; // 表按升序
        }
    }
    best
}

/// 下一个更高频点（无则保持当前）。
fn next_higher(table: &[FreqPoint], cur: u32) -> u32 {
    for p in table {
        if p.hz > cur {
            return p.hz;
        }
    }
    cur
}

/// 下一个更低频点（无则保持当前）。
fn next_lower(table: &[FreqPoint], cur: u32) -> u32 {
    let mut best = cur;
    for p in table {
        if p.hz < cur {
            best = p.hz;
        }
    }
    best
}

/// 由温度（°C）得出热级别。
pub fn thermal_level_of(temp_c: u32) -> ThermalLevel {
    if temp_c >= T_EMERGENCY_C {
        ThermalLevel::Emergency
    } else if temp_c >= T_CRITICAL_C {
        ThermalLevel::Critical
    } else if temp_c >= T_HOT_C {
        ThermalLevel::Hot
    } else if temp_c >= T_WARM_C {
        ThermalLevel::Warm
    } else {
        ThermalLevel::Normal
    }
}

/// 热级别对应的频率上限（Hz）。
fn thermal_cap_hz(table: &[FreqPoint], level: ThermalLevel) -> u32 {
    if table.is_empty() {
        return u32::MAX;
    }
    match level {
        ThermalLevel::Normal | ThermalLevel::Warm => table[table.len() - 1].hz,
        ThermalLevel::Hot => clamp_to_table(table, HOT_CAP_HZ),
        ThermalLevel::Critical | ThermalLevel::Emergency => table[0].hz,
    }
}

/// governor 决策：给定负载与当前频率，返回目标频率。
///
/// 纯函数：迟滞计数显式传入，便于单测。
fn governor_target(
    table: &[FreqPoint],
    gov: Governor,
    load: u8,
    cur: u32,
    up_streak: &mut u32,
    down_streak: &mut u32,
) -> u32 {
    if table.is_empty() {
        return cur;
    }
    let max = table[table.len() - 1].hz;
    let min = table[0].hz;
    match gov {
        Governor::Performance => max,
        Governor::Powersave => min,
        Governor::Ondemand | Governor::Conservative => {
            let (up, down) = match gov {
                Governor::Ondemand => (OND_UP, OND_DOWN),
                _ => (CONS_UP, CONS_DOWN),
            };
            if load >= up {
                *up_streak += 1;
                *down_streak = 0;
                if *up_streak >= HYST_SAMPLES {
                    next_higher(table, cur)
                } else {
                    cur
                }
            } else if load <= down {
                *down_streak += 1;
                *up_streak = 0;
                if *down_streak >= HYST_SAMPLES {
                    next_lower(table, cur)
                } else {
                    cur
                }
            } else {
                *up_streak = 0;
                *down_streak = 0;
                cur
            }
        }
    }
}

/// 软件热模型单步：`temp` 经 `dt_ms` 毫秒在 `load` 负载下的新温度（毫摄氏度）。
///
/// 一阶模型 `dT/dt = H·load − C·(T − T_amb)`（H=0.018°C/s/%，C=0.04/s）：
/// 满载稳态 85°C、半载 62.5°C、空闲 44.5°C，时间常数 25s。
pub fn sim_thermal_step(temp_mc: u32, load: u8, dt_ms: u64) -> u32 {
    let dt = dt_ms.max(1);
    let heat = (load as u64 * SIM_HEAT_PER_PCT_MS * dt) / 1000;
    let delta = (temp_mc as i64 - SIM_AMBIENT_MC as i64).max(0) as u64;
    let cool = (delta * SIM_COOL_PER_MS * dt) / 1_000_000;
    if heat >= cool {
        (temp_mc as u64 + (heat - cool)).min(u32::MAX as u64) as u32
    } else {
        temp_mc.saturating_sub((cool - heat) as u32)
    }
}

/// 充电状态机：由电量与连接状态决定状态。
pub fn charging_state(pct: u8, plugged: bool) -> BatteryState {
    if plugged {
        if pct >= 100 {
            BatteryState::Full
        } else {
            BatteryState::Charging
        }
    } else {
        BatteryState::Discharging
    }
}

/// 电池电量变化（虚拟模型）：充电每 1s +1%，放电每 5s −1%。
fn battery_step(pct: u8, plugged: bool, dt_ms: u64) -> u8 {
    if plugged {
        let ticks = (dt_ms / 1000) as u32;
        pct.saturating_add((ticks as u8).min(100 - pct)).min(100)
    } else {
        let ticks = (dt_ms / 5000) as u32;
        pct.saturating_sub((ticks as u8).min(pct))
    }
}

// ==================== 事件记录 ====================

fn push_event(kind: PowerEventKind) {
    unsafe {
        let idx = (EVENT_HEAD + EVENT_COUNT) % EVENTS_CAP;
        EVENTS[idx] = PowerEvent {
            time_s: crate::timer::now_us() / 1_000_000,
            kind,
        };
        if EVENT_COUNT < EVENTS_CAP {
            EVENT_COUNT += 1;
        } else {
            EVENT_HEAD = (EVENT_HEAD + 1) % EVENTS_CAP;
        }
    }
}

// ==================== 初始化 ====================

/// 初始化电源管理器：注册到调度 tick（10ms）并在启动日志标记能力。
///
/// 必须在调度器启动前调用（boot 上下文，IRQ 屏蔽）。实际采样由
/// [`tick`] 在定时器 IRQ 上下文完成。
pub fn init() {
    timer_register_power_tick();
    let plat = kunxhal::current_platform();
    let thermal = plat.thermal_sensor().is_some();
    let dvfs = plat.dvfs_controller().is_some();
    let battery = plat.battery().is_some();
    crate::kinfo! {
        crate::uart_send_str("[power] 电源管理器初始化");
        crate::uart_send_str(if thermal { " thermal=hw" } else { " thermal=sim" });
        crate::uart_send_str(if dvfs { " dvfs=hw" } else { " dvfs=virt" });
        crate::uart_send_str(if battery { " battery=hw" } else { " battery=virt" });
        crate::uart_send_str("\n");
    }
}

fn timer_register_power_tick() {
    // 仅注册一次（init 只调用一次，防御性检查）。
    static mut REGISTERED: bool = false;
    unsafe {
        if !REGISTERED {
            crate::timer::timer_register_handler(tick);
            REGISTERED = true;
        }
    }
}

// ==================== 周期采样（定时器 IRQ 上下文） ====================

/// 调度 tick（10ms）回调：按周期执行负载采样 / 热采样 / 电池采样。
pub fn tick() {
    let now = crate::timer::now_us();
    unsafe {
        if LAST_SAMPLE_US == 0 {
            LAST_SAMPLE_US = now;
            LAST_THERMAL_US = now;
            LAST_BATT_US = now;
            return;
        }
    }
    if now.saturating_sub(unsafe { LAST_SAMPLE_US }) >= SAMPLE_US {
        sample_load_and_dvfs(now);
    }
    if now.saturating_sub(unsafe { LAST_THERMAL_US }) >= THERMAL_POLL_US {
        sample_thermal(now);
    }
    if now.saturating_sub(unsafe { LAST_BATT_US }) >= BATTERY_POLL_US {
        sample_battery(now);
    }
}

/// 负载采样 + DVFS 决策 + 切换。
fn sample_load_and_dvfs(now: u64) {
    let window = now.saturating_sub(unsafe { LAST_SAMPLE_US }).max(1);
    unsafe { LAST_SAMPLE_US = now };

    let ncpu = (crate::smp::cpu_count() as usize).min(MAX_CPUS);
    let mut sum: u64 = 0;
    for cpu in 0..MAX_CPUS {
        let idle_now = IDLE_TOTAL[cpu].load(Ordering::Relaxed);
        let idle_delta = idle_now.saturating_sub(unsafe { LAST_IDLE[cpu] });
        unsafe { LAST_IDLE[cpu] = idle_now };
        let busy = window.saturating_sub(idle_delta);
        let load = ((busy * 100) / window) as u8;
        CURR_LOAD[cpu].store(load, Ordering::Relaxed);
        if cpu < ncpu {
            sum += load as u64;
        }
    }
    let agg = if ncpu > 0 { (sum / ncpu as u64) as u8 } else { 0 };
    AGG_LOAD.store(agg, Ordering::Relaxed);

    // governor 决策（簇 0；异构平台扩展点：逐簇循环）
    let Some(dvfs) = kunxhal::current_platform().dvfs_controller() else { return };
    if dvfs.cluster_count() == 0 {
        return;
    }
    let table = dvfs.freq_table(0);
    let cur = dvfs.current_hz(0);
    let target = unsafe {
        governor_target(table, GOV, agg, cur, &mut UP_STREAK, &mut DOWN_STREAK)
    };
    apply_freq(dvfs, table, target, false);
}

/// 执行频率切换：受热节流上限约束与切换间隔保护。
/// `force = true`（热节流路径）时绕过切换间隔，保证安全降频即时生效。
fn apply_freq(dvfs: &dyn DvfsController, table: &[FreqPoint], target: u32, force: bool) {
    if table.is_empty() {
        return;
    }
    let cap = unsafe { thermal_cap_hz(table, THERMAL_LEVEL) };
    let want = target.min(cap);
    let cur = dvfs.current_hz(0);
    if want == cur {
        return;
    }
    let now = crate::timer::now_us();
    if !force && now.saturating_sub(unsafe { LAST_SWITCH_US }) < MIN_SWITCH_INTERVAL_US {
        return; // 防抖：非热节流路径受限
    }
    if let Some(actual) = dvfs.set_hz(0, want) {
        unsafe { LAST_SWITCH_US = now };
        SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        crate::kinfo! {
            crate::uart_send_str("[power] dvfs ");
            crate::print_dec(cur as u64);
            crate::uart_send_str(" -> ");
            crate::print_dec(actual as u64);
            crate::uart_send_str(" Hz (load=");
            crate::print_dec(AGG_LOAD.load(Ordering::Relaxed) as u64);
            crate::uart_send_str("%, governor=");
            crate::uart_send_str(unsafe { GOV.name() });
            if actual != want {
                crate::uart_send_str(", 硬件钳制");
            }
            if cap < target {
                crate::uart_send_str(", 热上限=");
                crate::print_dec(cap as u64);
            }
            crate::uart_send_str(")\n");
        }
    }
}

/// 热采样：读传感器（或软件热模型）→ 级别判定 → 节流动作。
fn sample_thermal(now: u64) {
    let dt_ms = now.saturating_sub(unsafe { LAST_THERMAL_US }) / 1000;
    unsafe { LAST_THERMAL_US = now };

    let temp_mc = match kunxhal::current_platform().thermal_sensor() {
        Some(sensor) => sensor.read_temp_mc(0).unwrap_or(0),
        None => {
            // QEMU 模拟：强制温度优先，否则软件热模型由负载驱动。
            match unsafe { THERMAL_FORCE_MC } {
                Some(f) => f,
                None => {
                    let t = unsafe { SIM_TEMP_MC };
                    let load = AGG_LOAD.load(Ordering::Relaxed);
                    let next = sim_thermal_step(t, load, dt_ms);
                    unsafe { SIM_TEMP_MC = next };
                    next
                }
            }
        }
    };
    THERMAL_TEMP_MC.store(temp_mc, Ordering::Relaxed);

    let level = thermal_level_of(temp_mc / 1000);
    let prev = unsafe { THERMAL_LEVEL };
    // ★ 必须更新 THERMAL_LEVEL（render_status / thermal_cap_hz 读取它）；
    // LAST_THERMAL_LEVEL 仅为边沿检测保留。
    unsafe {
        THERMAL_LEVEL = level;
        LAST_THERMAL_LEVEL = level;
    }

    // 事件与日志：只在级别变化时输出，避免刷屏。
    if level != prev {
        crate::uart_send_str("[power] thermal ");
        crate::print_dec((temp_mc / 1000) as u64);
        crate::uart_send_str(" C -> ");
        crate::uart_send_str(level.name());
        crate::uart_send_str("\n");
        match level {
            ThermalLevel::Critical | ThermalLevel::Emergency => {
                push_event(PowerEventKind::ThermalCritical);
            }
            ThermalLevel::Normal => push_event(PowerEventKind::ThermalRecover),
            _ => {}
        }
    }

    // 热节流：Hot+ 立即把频率压到上限（force 绕过切换间隔）。
    if level != ThermalLevel::Normal {
        if let Some(dvfs) = kunxhal::current_platform().dvfs_controller() {
            let table = dvfs.freq_table(0);
            let cap = thermal_cap_hz(table, level);
            if dvfs.current_hz(0) > cap {
                apply_freq(dvfs, table, cap, true);
            }
        }
    }
}

/// 电池采样：HAL 电池优先，否则虚拟模型；驱动充电状态机与事件。
fn sample_battery(now: u64) {
    let dt_ms = now.saturating_sub(unsafe { LAST_BATT_US }) / 1000;
    unsafe { LAST_BATT_US = now };

    // 电量 / 连接状态来源
    let (pct, plugged) = match kunxhal::current_platform().battery() {
        Some(b) => (b.capacity_percent(), b.plugged()),
        None => {
            let p = unsafe { BATT_PCT };
            let plugged = unsafe { BATT_PLUGGED };
            let next = battery_step(p, plugged, dt_ms);
            unsafe { BATT_PCT = next };
            (next, plugged)
        }
    };

    // 充电状态机
    let state = charging_state(pct, plugged);
    let prev = unsafe { LAST_BATT_STATE };
    unsafe { LAST_BATT_STATE = state };
    unsafe { BATT_STATE = state };

    // 边沿事件
    if state != prev {
        match (prev, state) {
            (_, BatteryState::Charging) => push_event(PowerEventKind::ChargingStart),
            (BatteryState::Charging | BatteryState::Full, BatteryState::Discharging) => {
                push_event(PowerEventKind::Unplugged)
            }
            (BatteryState::Discharging, BatteryState::Full) => push_event(PowerEventKind::Plugged),
            _ => {}
        }
        if state == BatteryState::Full {
            push_event(PowerEventKind::BatteryFull);
        }
        crate::kinfo! {
            crate::uart_send_str("[power] battery ");
            crate::print_dec(pct as u64);
            crate::uart_send_str("% -> ");
            crate::uart_send_str(state.name());
            crate::uart_send_str("\n");
        }
    }

    // 低电量事件（边沿 + 迟滞复位）
    let low_evented = unsafe { BATT_LOW_EVENTED };
    if pct <= BATTERY_LOW_PCT && !low_evented {
        unsafe { BATT_LOW_EVENTED = true };
        push_event(PowerEventKind::BatteryLow);
        crate::uart_send_str("[power] battery-low 告警: ");
        crate::print_dec(pct as u64);
        crate::uart_send_str("%\n");
    } else if pct > BATTERY_LOW_PCT + 5 && low_evented {
        unsafe { BATT_LOW_EVENTED = false }; // +5% 迟滞复位
    }
}

// ==================== Idle 记账与状态机 ====================

/// Idle 阶段（每核）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IdlePhase {
    /// 正在运行任务。
    Active = 0,
    /// idle 任务运行中（尚未进入 WFI）。
    Idle = 1,
    /// 处于 WFI（等待中断唤醒）。
    Wfi = 2,
}

/// 进入 WFI 前由 idle 任务调用（记录 idle 开始）。
pub fn enter_wfi(cpu: u32) {
    let c = (cpu as usize).min(MAX_CPUS - 1);
    IDLE_PHASE[c].store(IdlePhase::Wfi as u8, Ordering::Relaxed);
    WFI_COUNT[c].fetch_add(1, Ordering::Relaxed);
}

/// WFI 返回（被中断唤醒）后由 idle 任务调用：
/// 累加 idle 时长并记录唤醒。
pub fn exit_wfi(cpu: u32, idle_us: u64) {
    let c = (cpu as usize).min(MAX_CPUS - 1);
    IDLE_TOTAL[c].fetch_add(idle_us, Ordering::Relaxed);
    WAKE_COUNT[c].fetch_add(1, Ordering::Relaxed);
    IDLE_PHASE[c].store(IdlePhase::Idle as u8, Ordering::Relaxed);
}

/// 当前核的 idle 阶段名（状态展示用）。
pub fn idle_phase_name(cpu: u32) -> &'static str {
    match IDLE_PHASE[(cpu as usize).min(MAX_CPUS - 1)].load(Ordering::Relaxed) {
        0 => "active",
        1 => "idle",
        _ => "wfi",
    }
}

/// 进入深空闲（关核时钟等）。v1 占位：Pi4 需固件 / 寄存器支持，
/// 当前仅记录；策略上保留接口供后续实现。
pub fn enter_deep_idle(_cpu: u32) {
    // TODO(Pi4): 经 mailbox / CPRMAN 关闭 per-core 时钟并停用其定时器。
}

// ==================== 控制接口（shell / procfs / 系统调用） ====================

/// 设置调速器。
pub fn set_governor(g: Governor) -> bool {
    unsafe {
        GOV = g;
        UP_STREAK = 0;
        DOWN_STREAK = 0;
    }
    crate::kinfo! {
        crate::uart_send_str("[power] governor -> ");
        crate::uart_send_str(g.name());
        crate::uart_send_str("\n");
    }
    true
}

/// 当前调速器。
pub fn governor() -> Governor {
    unsafe { GOV }
}

/// 聚合负载（%）。
pub fn current_load() -> u8 {
    AGG_LOAD.load(Ordering::Relaxed)
}

/// 当前频率（簇 0，Hz）。
pub fn current_freq_hz() -> u32 {
    kunxhal::current_platform()
        .dvfs_controller()
        .map(|d| d.current_hz(0))
        .unwrap_or(0)
}

/// 当前温度（毫摄氏度）。
pub fn thermal_temp_mc() -> u32 {
    THERMAL_TEMP_MC.load(Ordering::Relaxed)
}

/// 当前热级别。
pub fn thermal_level() -> ThermalLevel {
    unsafe { THERMAL_LEVEL }
}

/// 电池电量（%）。
pub fn battery_percent() -> u8 {
    unsafe { BATT_PCT }
}

/// 电池是否连接电源（虚拟模型）。
pub fn battery_plugged() -> bool {
    unsafe { BATT_PLUGGED }
}

/// 当前电池状态。
pub fn battery_state() -> BatteryState {
    unsafe { BATT_STATE }
}

/// 低电量告警是否激活。
pub fn battery_low() -> bool {
    unsafe { BATT_LOW_EVENTED }
}

/// 累计 DVFS 切换次数。
pub fn switch_count() -> u64 {
    SWITCH_COUNT.load(Ordering::Relaxed)
}

/// 系统调用打包状态：供应用轮询（避免读 /proc 的解析开销）。
///
/// 位布局：
///   bits 0-7   负载 %
///   bits 8-15  电池电量 %
///   bits 16-23 温度 °C（0..255）
///   bits 24-31 频率 MHz
///   bits 32-34 governor id
///   bits 35-36 电池状态 id
///   bit  37    低电量告警
pub fn sys_pm_get() -> u64 {
    let mut v: u64 = 0;
    v |= current_load() as u64;
    v |= (battery_percent() as u64) << 8;
    v |= ((thermal_temp_mc() / 1000) as u64 & 0xFF) << 16;
    v |= ((current_freq_hz() / 1_000_000) as u64 & 0xFF) << 24;
    v |= (governor().id() as u64) << 32;
    v |= (battery_state().id() as u64) << 35;
    v |= (battery_low() as u64) << 37;
    v
}

/// 处理控制命令（shell `power` 命令与可写 procfs 共用）。
///
/// 支持：
///   `governor <name>`、`thermal force <C>`、`thermal clear`、
///   `battery <pct>`、`plug <0|1>`、`cap <pid> <0-100>`、
///   `freeze <pid>`、`unfreeze <pid>`、`selftest`、`status`
pub fn handle_cmd(args: &[&str]) -> i32 {
    if args.is_empty() {
        return 0;
    }
    match args[0] {
        "governor" if args.len() >= 2 => {
            let g = match args[1] {
                "ondemand" => Governor::Ondemand,
                "conservative" => Governor::Conservative,
                "powersave" => Governor::Powersave,
                "performance" => Governor::Performance,
                _ => return -1,
            };
            set_governor(g);
            0
        }
        "thermal" if args.len() >= 3 && args[1] == "force" => {
            match args[2].parse::<u32>() {
                Ok(c) => {
                    unsafe { THERMAL_FORCE_MC = Some(c * 1000) };
                    0
                }
                Err(_) => -1,
            }
        }
        "thermal" if args.len() >= 2 && args[1] == "clear" => {
            unsafe { THERMAL_FORCE_MC = None };
            0
        }
        "battery" if args.len() >= 2 => match args[1].parse::<u8>() {
            Ok(p) => {
                unsafe { BATT_PCT = p.min(100) };
                0
            }
            Err(_) => -1,
        },
        "plug" if args.len() >= 2 => match args[1].parse::<u8>() {
            Ok(v) => {
                unsafe { BATT_PLUGGED = v != 0 };
                0
            }
            Err(_) => -1,
        },
        "cap" if args.len() >= 3 => {
            let pid = args[1].parse::<u32>().unwrap_or(u32::MAX);
            let cap = args[2].parse::<u16>().unwrap_or(u16::MAX);
            if cap > 100 {
                return -1;
            }
            crate::sched_cfs::set_task_cpu_cap(pid, cap)
        }
        "freeze" if args.len() >= 2 => {
            let pid = args[1].parse::<u32>().unwrap_or(u32::MAX);
            crate::sched_cfs::set_task_cpu_cap(pid, 0)
        }
        "unfreeze" if args.len() >= 2 => {
            let pid = args[1].parse::<u32>().unwrap_or(u32::MAX);
            crate::sched_cfs::set_task_cpu_cap(pid, 100)
        }
        "selftest" => {
            let fails = selftest();
            crate::uart_send_str("[power] 自检完成: ");
            crate::print_dec(fails as u64);
            crate::uart_send_str(" 个失败项\n");
            fails as i32
        }
        _ => -1,
    }
}

// ==================== 状态渲染（/proc/power、/proc/thermal） ====================

fn push_dec(buf: &mut [u8], pos: usize, mut v: u64) -> usize {
    if v == 0 {
        if pos < buf.len() {
            buf[pos] = b'0';
            return pos + 1;
        }
        return pos;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    if pos + n <= buf.len() {
        buf[pos..pos + n].copy_from_slice(&tmp[i..]);
    }
    pos + n
}

fn push_str(buf: &mut [u8], pos: usize, s: &str) -> usize {
    let b = s.as_bytes();
    let n = b.len().min(buf.len().saturating_sub(pos));
    if n > 0 {
        buf[pos..pos + n].copy_from_slice(&b[..n]);
    }
    pos + n
}

/// 渲染温度（毫摄氏度 → `xx.x C`）。
fn push_temp(buf: &mut [u8], pos: usize, mc: u32) -> usize {
    let mut p = push_dec(buf, pos, (mc / 1000) as u64);
    p = push_str(buf, p, ".");
    let frac = (mc % 1000) / 100;
    p = push_dec(buf, p, frac as u64);
    push_str(buf, p, " C")
}

/// `/proc/power` 内容。
pub fn render_status(buf: &mut [u8]) -> usize {
    let mut p = push_str(buf, 0, "KunxOS power manager\n");
    p = push_str(buf, p, "governor    : ");
    p = push_str(buf, p, governor().name());
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "load        : ");
    p = push_dec(buf, p, current_load() as u64);
    p = push_str(buf, p, "%");
    for cpu in 0..MAX_CPUS {
        p = push_str(buf, p, " cpu");
        p = push_dec(buf, p, cpu as u64);
        p = push_str(buf, p, "=");
        p = push_dec(buf, p, CURR_LOAD[cpu].load(Ordering::Relaxed) as u64);
    }
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "freq        : ");
    p = push_dec(buf, p, (current_freq_hz() / 1_000_000) as u64);
    p = push_str(buf, p, " MHz");
    if let Some(d) = kunxhal::current_platform().dvfs_controller() {
        p = push_str(buf, p, " (table:");
        for (i, fp) in d.freq_table(0).iter().enumerate() {
            if i > 0 {
                p = push_str(buf, p, "/");
            }
            p = push_dec(buf, p, (fp.hz / 1_000_000) as u64);
        }
        p = push_str(buf, p, " MHz)");
    }
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "switches    : ");
    p = push_dec(buf, p, switch_count());
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "thermal     : ");
    p = push_temp(buf, p, thermal_temp_mc());
    p = push_str(buf, p, " level=");
    p = push_str(buf, p, thermal_level().name());
    p = push_str(buf, p, "\n");
    if let Some(mv) = kunxhal::current_platform().core_voltage_mv() {
        p = push_str(buf, p, "voltage     : ");
        p = push_dec(buf, p, mv as u64);
        p = push_str(buf, p, " mV\n");
    }
    p = push_str(buf, p, "battery     : ");
    p = push_dec(buf, p, battery_percent() as u64);
    p = push_str(buf, p, "% ");
    p = push_str(buf, p, battery_state().name());
    p = push_str(buf, p, " plugged=");
    p = push_dec(buf, p, battery_plugged() as u64);
    if battery_low() {
        p = push_str(buf, p, " LOW");
    }
    p = push_str(buf, p, "\n");
    for cpu in 0..MAX_CPUS {
        p = push_str(buf, p, "idle        : cpu");
        p = push_dec(buf, p, cpu as u64);
        p = push_str(buf, p, " phase=");
        p = push_str(buf, p, idle_phase_name(cpu as u32));
        p = push_str(buf, p, " wfi=");
        p = push_dec(buf, p, WFI_COUNT[cpu].load(Ordering::Relaxed));
        p = push_str(buf, p, " wake=");
        p = push_dec(buf, p, WAKE_COUNT[cpu].load(Ordering::Relaxed));
        p = push_str(buf, p, "\n");
    }
    // 后台冻结（Doze）任务清单
    let mut shown = false;
    crate::sched_cfs::for_each_task_cap(|pid, cap, frozen| {
        if cap < 100 || frozen {
            p = push_str(buf, p, "doze        : pid ");
            p = push_dec(buf, p, pid as u64);
            if frozen {
                p = push_str(buf, p, " FROZEN");
            } else {
                p = push_str(buf, p, " cap=");
                p = push_dec(buf, p, cap as u64);
                p = push_str(buf, p, "%");
            }
            p = push_str(buf, p, "\n");
            shown = true;
        }
    });
    if !shown {
        p = push_str(buf, p, "doze        : (none)\n");
    }
    // 事件
    p = push_str(buf, p, "events      :\n");
    unsafe {
        for i in 0..EVENT_COUNT {
            let e = &EVENTS[(EVENT_HEAD + i) % EVENTS_CAP];
            p = push_str(buf, p, "  ");
            p = push_dec(buf, p, e.time_s);
            p = push_str(buf, p, "s ");
            p = push_str(buf, p, e.kind.name());
            p = push_str(buf, p, "\n");
        }
    }
    p
}

/// `/proc/thermal` 内容。
pub fn render_thermal(buf: &mut [u8]) -> usize {
    let mut p = push_str(buf, 0, "KunxOS thermal\n");
    p = push_str(buf, p, "temp        : ");
    p = push_temp(buf, p, thermal_temp_mc());
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "level       : ");
    p = push_str(buf, p, thermal_level().name());
    p = push_str(buf, p, "\n");
    p = push_str(buf, p, "thresholds  : warm=");
    p = push_dec(buf, p, T_WARM_C as u64);
    p = push_str(buf, p, "C hot=");
    p = push_dec(buf, p, T_HOT_C as u64);
    p = push_str(buf, p, "C critical=");
    p = push_dec(buf, p, T_CRITICAL_C as u64);
    p = push_str(buf, p, "C emergency=");
    p = push_dec(buf, p, T_EMERGENCY_C as u64);
    p = push_str(buf, p, "C\n");
    match kunxhal::current_platform().thermal_sensor() {
        Some(_) => p = push_str(buf, p, "source      : hardware sensor\n"),
        None => {
            p = push_str(buf, p, "source      : software model (QEMU sim)\n");
            match unsafe { THERMAL_FORCE_MC } {
                Some(f) => {
                    p = push_str(buf, p, "forced      : ");
                    p = push_temp(buf, p, f);
                    p = push_str(buf, p, "\n");
                }
                None => p = push_str(buf, p, "forced      : (none)\n"),
            }
        }
    }
    p
}

// ==================== 自检 ====================

/// 电源管理逻辑自检：返回失败项数量（0 = 全部通过）。
///
/// 只测试纯函数与状态机，不依赖硬件；QEMU 与 Pi4 均可运行。
pub fn selftest() -> u32 {
    let mut fails = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                crate::uart_send_str("[power-selftest] ");
                crate::uart_send_str($name);
                crate::uart_send_str(" 通过\n");
            } else {
                crate::uart_send_str("[power-selftest] ");
                crate::uart_send_str($name);
                crate::uart_send_str(" 失败\n");
                fails += 1;
            }
        };
    }

    // 测试频点表（MHz → Hz；函数只做比较，无单位耦合）
    const T: [FreqPoint; 4] = [
        freq_point_const(600, 0),
        freq_point_const(900, 0),
        freq_point_const(1200, 0),
        freq_point_const(1500, 0),
    ];
    const H600: u32 = 600_000_000;
    const H900: u32 = 900_000_000;
    const H1200: u32 = 1_200_000_000;
    const H1500: u32 = 1_500_000_000;

    // 1. 频点表导航
    check!("next_higher mid", next_higher(&T, H900) == H1200);
    check!("next_higher top", next_higher(&T, H1500) == H1500);
    check!("next_lower mid", next_lower(&T, H1200) == H900);
    check!("next_lower bottom", next_lower(&T, H600) == H600);
    check!(
        "clamp_to_table",
        clamp_to_table(&T, 850_000_000) == H600 && clamp_to_table(&T, 901_000_000) == H900
    );

    // 2. governor 决策 + 迟滞
    let mut up = 0u32;
    let mut down = 0u32;
    // 连续 2 个高负载采样后升频
    let t1 = governor_target(&T, Governor::Ondemand, 90, H900, &mut up, &mut down);
    check!("ondemand hyst first", t1 == H900 && up == 1);
    let t2 = governor_target(&T, Governor::Ondemand, 90, H900, &mut up, &mut down);
    check!("ondemand hyst second", t2 == H1200 && up == 2);
    // 低负载连续 2 个采样后降频
    let _ = governor_target(&T, Governor::Ondemand, 10, H1200, &mut up, &mut down);
    check!("ondemand down first", down == 1 && up == 0);
    let t3 = governor_target(&T, Governor::Ondemand, 10, H1200, &mut up, &mut down);
    check!("ondemand down second", t3 == H900);
    // 中间负载复位迟滞
    up = 0;
    down = 0;
    let t4 = governor_target(&T, Governor::Ondemand, 50, H900, &mut up, &mut down);
    check!("ondemand mid keeps", t4 == H900 && up == 0 && down == 0);
    // powersave / performance
    let t5 = governor_target(&T, Governor::Powersave, 100, H1500, &mut up, &mut down);
    check!("powersave min", t5 == H600);
    let t6 = governor_target(&T, Governor::Performance, 0, H600, &mut up, &mut down);
    check!("performance max", t6 == H1500);
    // conservative 阈值更高（80% 在中间区 → 不累计；95% 连续 2 次才升频）
    up = 0;
    down = 0;
    let t7 = governor_target(&T, Governor::Conservative, 80, H900, &mut up, &mut down);
    check!("conservative 80 keeps", t7 == H900 && up == 0 && down == 0);
    let t8a = governor_target(&T, Governor::Conservative, 95, H900, &mut up, &mut down);
    check!("conservative 95 first", t8a == H900 && up == 1);
    let t8b = governor_target(&T, Governor::Conservative, 95, H900, &mut up, &mut down);
    check!("conservative 95 up", t8b == H1200);

    // 3. 热节流覆盖 governor
    let cap_hot = thermal_cap_hz(&T, ThermalLevel::Hot);
    let cap_crit = thermal_cap_hz(&T, ThermalLevel::Critical);
    check!("thermal hot cap", cap_hot == H900);
    check!("thermal critical cap", cap_crit == H600);

    // 4. 热级别判定
    check!(
        "thermal levels",
        thermal_level_of(60) == ThermalLevel::Normal
            && thermal_level_of(75) == ThermalLevel::Warm
            && thermal_level_of(82) == ThermalLevel::Hot
            && thermal_level_of(87) == ThermalLevel::Critical
            && thermal_level_of(95) == ThermalLevel::Emergency
    );

    // 5. 软件热模型：满载升温至热区、空载降温
    let mut t = sim_thermal_step(40_000, 100, 1000); // 40°C 满载 1s
    check!("sim warm start", t > 40_000);
    for _ in 0..300 {
        t = sim_thermal_step(t, 100, 1000); // 5 分钟满载
    }
    check!("sim full-load hot", t / 1000 >= 80); // 稳态 ≥ 80°C
    for _ in 0..300 {
        t = sim_thermal_step(t, 5, 1000); // 5 分钟空载
    }
    check!("sim idle cool", t / 1000 < 55);
    // 稳态收敛：1s 步长跑 1 小时（Euler 积分大 dt 会发散，须用小步长）
    let mut t_full = 40_000u32;
    for _ in 0..3600 {
        t_full = sim_thermal_step(t_full, 100, 1000);
    }
    check!("sim steady bounded", (80..=90).contains(&(t_full / 1000)));

    // 6. 充电状态机
    check!(
        "charging state machine",
        charging_state(50, false) == BatteryState::Discharging
            && charging_state(50, true) == BatteryState::Charging
            && charging_state(100, true) == BatteryState::Full
    );

    // 7. 电池电量变化
    check!("battery discharge", battery_step(50, false, 10_000) == 48);
    check!("battery charge", battery_step(50, true, 3_000) == 53);
    check!("battery full clamp", battery_step(99, true, 10_000) == 100);
    check!("battery zero clamp", battery_step(1, false, 100_000) == 0);

    // 8. 打包状态位
    let packed = sys_pm_get();
    check!("pm_get load bits", (packed & 0xFF) <= 100);
    check!("pm_get battery bits", ((packed >> 8) & 0xFF) <= 100);
    check!("pm_get governor bits", ((packed >> 32) & 0x7) <= 3);

    fails
}

/// 自检用频点构造（`freq_point` 的 MHz 值别名，避免长常量）。
const fn freq_point_const(hz_mhz: u32, uv: u32) -> FreqPoint {
    FreqPoint {
        hz: hz_mhz * 1_000_000,
        uv,
    }
}
