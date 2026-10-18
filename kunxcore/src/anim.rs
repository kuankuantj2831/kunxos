//! 动画引擎：对标 iOS（Core Animation / HIG）与 Android（Material Motion）的运动语言。
//!
//! 本模块是系统运动规范的"单一事实来源"：
//!
//! - **缓动曲线**：iOS 曲线来自 `kCAMediaTimingFunction*`（EaseIn/EaseOut/EaseInEaseOut），
//!   Android 曲线来自 Material Design 的 Standard/Decelerate/Accelerate 与 Material 3
//!   Emphasized（均为三次贝塞尔，见 [`Easing`]）。
//! - **弹簧物理**：阻尼谐振子，半隐式欧拉积分（纯四则运算，无 exp/sqrt 内建依赖，
//!   可在 `no_std` 编译）。支持 iOS 风格（`dampingRatio` + `initialVelocity`）与
//!   AndroidX `SpringAnimation` 风格（`stiffness` + `dampingRatio`）。
//! - **动画管理器**：定长槽位的补间/弹簧动画，作用于合成器窗口的
//!   位置 / 透明度 / 缩放，由定时器驱动的 [`frame_tick`] 逐帧推进。
//!
//! 曲线与参数的具体数值与来源见 `docs/animations.md`。用户态 launcher 内的运动库
//! 按同一规范实现（apps/launcher/src/main.rs "运动/动画" 一节）。

use core::arch::asm;

use crate::compositor;
use crate::timer;

/// 屏蔽 IRQ 进入动画管理器临界区（ANIMS 槽位表）。
///
/// 槽位操作来自进程上下文（gesture / 窗口管理），而 `frame_tick` 由 CNTP
/// IRQ 驱动并发遍历同一数组：槽位写入非原子，IRQ 打在写入中间会读到
/// 半初始化的 `Anim`。保存 DAIF 后屏蔽（与 `compositor::irq_save` 同款），
/// 调用方可能本身就在 IRQ 上下文，恢复式写回避免嵌套中断。
unsafe fn irq_save() -> u64 {
    let d: u64;
    asm!("csrr {}, sstatus", out(reg) d);
    asm!("csrci sstatus, 2");
    d
}

unsafe fn irq_restore(saved: u64) {
    asm!("csrw sstatus, {}", in(reg) saved);
}
// ==================== 缓动曲线 ====================

/// 缓动曲线（三次贝塞尔时序函数）。
///
/// | 变体 | 控制点 (x1,y1,x2,y2) | 来源 |
/// | --- | --- | --- |
/// | `Linear` | (0,0,1,1) | 通用 |
/// | `EaseIn` | (0.42,0,1,1) | iOS `kCAMediaTimingFunctionEaseIn` |
/// | `EaseOut` | (0,0,0.58,1) | iOS `kCAMediaTimingFunctionEaseOut` |
/// | `EaseInEaseOut` | (0.42,0,0.58,1) | iOS `kCAMediaTimingFunctionEaseInEaseOut` |
/// | `FastOutSlowIn` | (0.4,0,0.2,1) | Android 标准（`@android:interpolator/fast_out_slow_in`） |
/// | `LinearOutSlowIn` | (0,0,0.2,1) | Android `linear_out_slow_in`（Decelerate） |
/// | `FastOutLinearIn` | (0.4,0,1,1) | Android `fast_out_linear_in`（Accelerate） |
/// | `Emphasized` | (0.2,0,0,1) | Material 3 强调曲线 |
/// | `ColorOSStandard` | (0.22,1,0.36,1) | ColorOS 标准曲线（强减速回弹手感） |
/// | `ColorOSEmphasized` | (0.3,0,0.1,1) | ColorOS 强调进入曲线（快出缓停） |
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    EaseInEaseOut,
    FastOutSlowIn,
    LinearOutSlowIn,
    FastOutLinearIn,
    Emphasized,
    ColorOSStandard,
    ColorOSEmphasized,
}

impl Easing {
    /// 控制点 `(x1, y1, x2, y2)`。
    pub const fn control_points(self) -> (f32, f32, f32, f32) {
        match self {
            Easing::Linear => (0.0, 0.0, 1.0, 1.0),
            Easing::EaseIn => (0.42, 0.0, 1.0, 1.0),
            Easing::EaseOut => (0.0, 0.0, 0.58, 1.0),
            Easing::EaseInEaseOut => (0.42, 0.0, 0.58, 1.0),
            Easing::FastOutSlowIn => (0.4, 0.0, 0.2, 1.0),
            Easing::LinearOutSlowIn => (0.0, 0.0, 0.2, 1.0),
            Easing::FastOutLinearIn => (0.4, 0.0, 1.0, 1.0),
            Easing::Emphasized => (0.2, 0.0, 0.0, 1.0),
            Easing::ColorOSStandard => (0.22, 1.0, 0.36, 1.0),
            Easing::ColorOSEmphasized => (0.3, 0.0, 0.1, 1.0),
        }
    }

    /// 在进度 `t`（0..=1）处的曲线值（0..=1）。
    pub fn sample(self, t: f32) -> f32 {
        // 钳位输入：越界的 x 会让 solve_x 的牛顿/二分外推（与
        // shared/motion.rs 的 eval 行为保持一致）。
        let (x1, y1, x2, y2) = self.control_points();
        solve_bezier(x1, y1, x2, y2, t.clamp(0.0, 1.0))
    }
}

/// 三次贝塞尔采样：`p(t) = (1-t)^3·P0 + 3(1-t)^2t·P1 + 3(1-t)t^2·P2 + t^3·P3`。
#[inline]
fn bezier_sample(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    let mt2 = mt * mt;
    let mt3 = mt2 * mt;
    let t2 = t * t;
    let t3 = t2 * t;
    a * mt3 + 3.0 * b * mt2 * t + 3.0 * c * mt * t2 + d * t3
}

/// 三次贝塞尔一阶导。
#[inline]
fn bezier_deriv(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    3.0 * mt * mt * (b - a) + 6.0 * mt * t * (c - b) + 3.0 * t * t * (d - c)
}

/// 求贝塞尔 x 曲线（控制点 0, x1, x2, 1）在给定 x 处的参数 t：
/// 牛顿迭代 + 二分兜底（WebKit/Chromium 同款算法）。
fn solve_x(x1: f32, x2: f32, x: f32) -> f32 {
    const EPS: f32 = 1e-4;
    let mut t = x;
    let mut converged = false;
    for _ in 0..8 {
        let err = bezier_sample(0.0, x1, x2, 1.0, t) - x;
        if err.abs() < EPS {
            converged = true;
            break;
        }
        let d = bezier_deriv(0.0, x1, x2, 1.0, t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= err / d;
    }
    if converged {
        return t.clamp(0.0, 1.0);
    }
    // 二分细化，消除牛顿迭代的残余误差；用牛顿结果收窄区间。
    let (mut lo, mut hi) = if t > x { (0.0f32, t.clamp(0.0, 1.0)) } else { (t.clamp(0.0, 1.0), 1.0f32) };
    for _ in 0..12 {
        let m = (lo + hi) * 0.5;
        if bezier_sample(0.0, x1, x2, 1.0, m) < x {
            lo = m;
        } else {
            hi = m;
        }
    }
    ((lo + hi) * 0.5).clamp(0.0, 1.0)
}

/// 求解完整三次贝塞尔：给定输入进度 `x`，返回曲线值 `y`。
fn solve_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    bezier_sample(0.0, y1, y2, 1.0, solve_x(x1, x2, x))
}

/// f32 平方根（牛顿迭代，纯四则运算，避免依赖 libm / 不稳定内建）。
fn fsqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut r = x * 0.5;
    for _ in 0..8 {
        r = 0.5 * (r + x / r);
    }
    r
}

// ==================== 弹簧物理 ====================

/// 阻尼谐振子弹簧。
///
/// 运动方程：`m·x'' + c·x' + k·(x - target) = 0`，半隐式欧拉积分（1ms 子步），
/// 无需 `exp`/`sin` 闭式解，数值稳定且完全 `no_std` 可编译。
#[derive(Clone, Copy)]
pub struct Spring {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
    pub target: f32,
    pub pos: f32,
    pub vel: f32,
}

impl Spring {
    /// iOS 风格弹簧：`dampingRatio`（0..1 欠阻尼 / 1 临界 / >1 过阻尼，UIView 常用 0.7~0.9）
    /// + `initial_velocity`（单位/秒，如 iOS `initialSpringVelocity`）。
    pub fn ios(damping_ratio: f32, initial_velocity: f32) -> Spring {
        let stiffness = 280.0; // 与 UIView 弹簧动画常用刚度相当
        let mass = 1.0;
        let crit = 2.0 * fsqrt(stiffness * mass);
        Spring {
            stiffness,
            mass,
            damping: crit * damping_ratio.clamp(0.05, 2.0),
            target: 0.0,
            pos: 0.0,
            vel: initial_velocity,
        }
    }

    /// Android 风格弹簧：`stiffness`（N/m，AndroidX SpringAnimation 默认 210）
    /// + `dampingRatio`（默认 1 = 临界阻尼）。
    pub fn android(stiffness: f32, damping_ratio: f32) -> Spring {
        let mass = 1.0;
        let crit = 2.0 * fsqrt(stiffness * mass);
        Spring {
            stiffness,
            mass,
            damping: crit * damping_ratio.clamp(0.05, 2.0),
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }

    /// 推进 `dt` 秒（内部按 1ms 子步积分）。返回是否已稳定在目标附近。
    ///
    /// 数值稳定性防护：`mass` 被钳制到 ≥ 0.1（除零防护）；`stiffness` 被钳制到
    /// `[0, 20_000]`（半隐式欧拉在 `ωh ≥ 2` 时发散，ωh = √(k/m)·h，h=1ms 下
    /// k ≤ 20_000 保证 √k·0.001 < 2）；若积分中出现非有限值（NaN/∞），直接
    /// 强制收敛到目标，避免动画永不结束并持续驱动整屏渲染。
    pub fn step(&mut self, dt: f32) -> bool {
        // 参数防护（防御外部直接构造 Spring 的情况）
        if !self.mass.is_finite() || self.mass <= 0.0 {
            self.mass = 1.0;
        } else if self.mass < 0.1 {
            self.mass = 0.1;
        }
        if !self.stiffness.is_finite() || self.stiffness < 0.0 {
            self.stiffness = 0.0;
        } else if self.stiffness > 20_000.0 {
            self.stiffness = 20_000.0;
        }
        if !self.damping.is_finite() || self.damping < 0.0 {
            self.damping = 0.0;
        }

        let mut t = dt;
        while t > 0.0 {
            let h = if t > 0.001 { 0.001 } else { t };
            let a =
                (-self.stiffness * (self.pos - self.target) - self.damping * self.vel) / self.mass;
            self.vel += a * h;
            self.pos += self.vel * h;
            t -= h;
            // 发散检测：积分结果非有限 → 强制收敛（防止 NaN 卡死动画管线）
            if !self.pos.is_finite() || !self.vel.is_finite() {
                self.pos = self.target;
                self.vel = 0.0;
                return true;
            }
        }
        (self.pos - self.target).abs() < 0.0005 && self.vel.abs() < 0.001
    }
}

// ==================== 动画管理器 ====================

/// 最大并发动画数。
pub const MAX_ANIMS: usize = 8;

/// 可动画的窗口属性。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnimProp {
    X,
    Y,
    Alpha,
    Scale,
}

/// 单个动画：时间型补间（缓动曲线）或物理型弹簧，二选一。
struct Anim {
    active: bool,
    window: usize,
    prop: AnimProp,
    // 时间型补间
    from: f32,
    to: f32,
    t0: u64,
    dur: u64, // 微秒
    easing: Option<Easing>,
    // 物理型弹簧
    spring: Option<Spring>,
    /// 完成回调（参数为动画所属窗口 id，如链式动画 / 销毁窗口 / 重置 transform）。
    on_finish: Option<fn(usize)>,
}

const EMPTY_ANIM: Anim = Anim {
    active: false,
    window: 0,
    prop: AnimProp::X,
    from: 0.0,
    to: 0.0,
    t0: 0,
    dur: 0,
    easing: None,
    spring: None,
    on_finish: None,
};

static mut ANIMS: [Anim; MAX_ANIMS] = [EMPTY_ANIM; MAX_ANIMS];
static mut ANY_ACTIVE: bool = false;
static mut LAST_TICK_US: u64 = 0;

/// 是否有动画在运行。
pub fn active() -> bool {
    unsafe { ANY_ACTIVE }
}

/// 启动一个时间型补间动画。`dur_us` 为时长（微秒）。返回是否成功入队。
pub fn tween(
    window: usize,
    prop: AnimProp,
    from: f32,
    to: f32,
    dur_us: u64,
    easing: Easing,
    on_finish: Option<fn(usize)>,
) -> bool {
    unsafe {
        let saved = irq_save();
        for a in ANIMS.iter_mut() {
            if !a.active {
                // 从空闲重启帧时钟时清零上次 tick 时刻：否则首帧 dt 为
                // 空闲时长（被钳到 0.1s），弹簧动画第一步就跳 100ms。
                if !ANY_ACTIVE {
                    LAST_TICK_US = 0;
                }
                *a = Anim {
                    active: true,
                    window,
                    prop,
                    from,
                    to,
                    t0: timer::now_us(),
                    dur: dur_us,
                    easing: Some(easing),
                    spring: None,
                    on_finish,
                };
                // 立即应用起始值（避免首帧跳变）
                apply(window, prop, from);
                ANY_ACTIVE = true;
                // 按需点亮 CNTP：空闲时动画定时器已停（frame_tick 无活动时停），
                // 有新动画立即重新启动 120Hz 帧节拍。
                frame_clock_start();
                irq_restore(saved);
                return true;
            }
        }
        irq_restore(saved);
    }
    false
}

/// 启动一个弹簧动画（物理驱动，无固定时长）。返回是否成功入队。
pub fn spring(
    window: usize,
    prop: AnimProp,
    from: f32,
    to: f32,
    mut sp: Spring,
    on_finish: Option<fn(usize)>,
) -> bool {
    sp.pos = from;
    sp.target = to;
    unsafe {
        let saved = irq_save();
        for a in ANIMS.iter_mut() {
            if !a.active {
                if !ANY_ACTIVE {
                    LAST_TICK_US = 0;
                }
                *a = Anim {
                    active: true,
                    window,
                    prop,
                    from,
                    to,
                    t0: 0,
                    dur: 0,
                    easing: None,
                    spring: Some(sp),
                    on_finish,
                };
                apply(window, prop, from);
                ANY_ACTIVE = true;
                frame_clock_start();
                irq_restore(saved);
                return true;
            }
        }
        irq_restore(saved);
    }
    false
}

/// 把动画值应用到合成器窗口。
fn apply(window: usize, prop: AnimProp, value: f32) {
    match prop {
        AnimProp::X | AnimProp::Y => {
            if let Some((wx, wy, _, _)) = compositor::window_geom(window) {
                let (nx, ny) = if prop == AnimProp::X {
                    (value as i32, wy)
                } else {
                    (wx, value as i32)
                };
                compositor::move_window(window, nx, ny);
            }
        }
        AnimProp::Alpha => {
            let a = value.clamp(0.0, 255.0) as u8;
            compositor::set_alpha(window, a);
        }
        AnimProp::Scale => {
            // 定点缩放：1.0 = 256（与 compositor::set_scale 一致）
            let s = value.clamp(1.0, 1024.0) as u32;
            compositor::set_scale(window, s);
        }
    }
}

/// 本 tick 到期的 on_finish 回调暂存：回调（链式动画/销毁窗口）可能调用
/// tween/cancel_window 改写 ANIMS 槽位表，必须等 frame_tick 的迭代循环
/// 结束后统一执行 —— 迭代中重入会让回调选中的槽位与外层迭代元素交叠。
static mut FINISH_Q: [Option<(usize, fn(usize))>; 8] = [None; 8];
static mut FINISH_Q_COUNT: usize = 0;

fn finish(a: &mut Anim) {
    a.active = false;
    if let Some(cb) = a.on_finish {
        unsafe {
            if FINISH_Q_COUNT < FINISH_Q.len() {
                FINISH_Q[FINISH_Q_COUNT] = Some((a.window, cb));
                FINISH_Q_COUNT += 1;
            } else {
                // 队列满（单 tick 8 个完成即溢出）：保持原语义就地执行。
                cb(a.window);
            }
        }
    }
}

/// 循环外执行本 tick 到期的完成回调。
fn run_finish_queue() {
    unsafe {
        let n = FINISH_Q_COUNT;
        FINISH_Q_COUNT = 0;
        for i in 0..n {
            if let Some((w, cb)) = FINISH_Q[i] {
                FINISH_Q[i] = None;
                cb(w);
            }
        }
    }
}

/// 取消指定窗口的全部动画（供 `destroy_window` 调用）。
///
/// 窗口销毁后其 id 可能被新窗口复用，残留的旧动画会继续驱动新窗口
/// （位置/透明度/缩放被旧目标覆盖）。销毁前必须取消。
pub fn cancel_window(window: usize) {
    unsafe {
        let saved = irq_save();
        let mut any = false;
        for a in ANIMS.iter_mut() {
            if a.active && a.window == window {
                a.active = false;
            } else if a.active {
                any = true;
            }
        }
        ANY_ACTIVE = any;
        if !any {
            // 帧时钟只由本模块启停：取消后无剩余动画时立即停掉 CNTP，
            // 否则 frame_tick 的 `!ANY_ACTIVE` 早退路径永远不会走到停表，
            // 定时器以 120Hz 空转唤醒。清零 LAST_TICK_US 保证下次动画
            // 首帧 dt 从 0 开始。
            LAST_TICK_US = 0;
            crate::timer::timer2_stop();
        }
        irq_restore(saved);
    }
}

// ==================== 页面切换动画（OOBE 等向导型 UI） ====================

/// 页面切换动画时长：淡出 / 淡入各 200ms。
pub const PAGE_FADE_US: u64 = 200_000;

/// 淡入结束后强制重置窗口 transform 为 identity。
fn reset_page_transform(window: usize) {
    compositor::reset_window_transform(window);
}

/// 页面切换：旧窗口 200ms 淡出（alpha 255->0）、新窗口 200ms 淡入
/// （alpha 0->255）。曲线对齐 ColorOS 17：淡入用 ColorOSEmphasized
/// （快起缓停，入场手感），淡出用 ColorOSStandard（整体标准曲线），
/// 无位移、无缩放。
///
/// 新窗口位置在切换前强制重置为默认居中（含边界检查），transform 为 identity；
/// 淡入结束后再次强制重置 transform 为 identity。旧窗口淡出至 alpha 0 后由
/// 调用方负责销毁。
pub fn page_transition(old_win: Option<usize>, new_win: Option<usize>) {
    if let Some(o) = old_win {
        cancel_window(o);
        tween(
            o,
            AnimProp::Alpha,
            255.0,
            0.0,
            PAGE_FADE_US,
            Easing::ColorOSStandard,
            None,
        );
    }
    if let Some(n) = new_win {
        cancel_window(n);
        compositor::reset_window_transform(n);
        tween(
            n,
            AnimProp::Alpha,
            0.0,
            255.0,
            PAGE_FADE_US,
            Easing::ColorOSEmphasized,
            Some(reset_page_transform),
        );
    }
}

// ==================== 帧间隔统计（流畅度度量） ====================
//
// 记录每次实际呈现一帧的**真实间隔**（相邻两次 present 之间的 µs），
// 滚动维护 min/avg/p95/max 与 >20ms 尖峰计数，并统计每帧合成+呈现耗时；
// 每 60 帧向串口打印一次（uart）。先有度量，再谈优化。

/// 单次报告窗口的样本容量。
const STAT_WINDOW: usize = 64;
/// 报告周期（帧数）。
const STAT_REPORT_EVERY: usize = 60;
/// 尖峰阈值（µs）：间隔超过 20ms 视为掉帧尖峰。
const SPIKE_US: u64 = 20_000;

struct FrameStats {
    /// 本报告周期内已记录的帧数（首帧无间隔，只记 1 帧无样本）。
    count: usize,
    /// 本周期帧间隔 min/max/sum。
    min: u64,
    max: u64,
    sum: u64,
    /// 本周期帧间隔样本（用于 p95）。
    samples: [u64; STAT_WINDOW],
    /// 本周期 >20ms 尖峰次数。
    spike_count: u64,
    /// 本周期 composite+present 耗时累计与最大值（µs）。
    render_sum: u64,
    render_max: u64,
    /// 生命周期内呈现的总帧数（供 perf_test 看门狗判断定时器是否驱动）。
    total_frames: u64,
    /// 上一帧呈现时刻（µs，0 表示尚无帧）。
    last_frame_us: u64,
}

const EMPTY_FRAME_STATS: FrameStats = FrameStats {
    count: 0,
    min: u64::MAX,
    max: 0,
    sum: 0,
    samples: [0; STAT_WINDOW],
    spike_count: 0,
    render_sum: 0,
    render_max: 0,
    total_frames: 0,
    last_frame_us: 0,
};

static mut FRAME_STATS: FrameStats = EMPTY_FRAME_STATS;

/// 生命周期内已呈现的帧总数（供 perf_test 看门狗使用）。
pub fn frames_presented() -> u64 {
    unsafe { FRAME_STATS.total_frames }
}

/// 记录一帧：`now` 为本次帧推进时刻，`render_us` 为合成+呈现耗时。
fn record_frame(now: u64, render_us: u64) {
    unsafe {
        let s = &mut FRAME_STATS;
        if s.last_frame_us != 0 {
            let iv = now.saturating_sub(s.last_frame_us);
            if s.count < STAT_WINDOW {
                s.samples[s.count] = iv;
            }
            if iv < s.min {
                s.min = iv;
            }
            if iv > s.max {
                s.max = iv;
            }
            s.sum += iv;
            if iv > SPIKE_US {
                s.spike_count += 1;
            }
        }
        s.last_frame_us = now;
        s.count += 1;
        s.total_frames += 1;
        s.render_sum += render_us;
        if render_us > s.render_max {
            s.render_max = render_us;
        }
        // ★ 统计打印延迟到线程上下文：报告窗口约 110 字符的串口输出在
        // 115200 波特下逐字节忙等 ≈9.5ms 且全程 IRQ 关闭（CNTP 中断上下文），
        // 超过 8.33ms 帧预算本身，期间调度 tick 与 UART RX 全部顺延。
        if s.count >= STAT_REPORT_EVERY {
            STATS_PENDING = true;
        }
    }
}

/// 统计报告待打印标记（IRQ 侧置位，线程上下文消费）。
static mut STATS_PENDING: bool = false;

/// 线程上下文冲刷待打印的帧统计（内核主循环每轮调用）。
pub fn poll_deferred_stats() {
    unsafe {
        if STATS_PENDING {
            STATS_PENDING = false;
            report_frame_stats();
        }
    }
}

/// 打印当前统计窗口并重置（保留 `last_frame_us` 保证跨窗口间隔连续）。
fn report_frame_stats() {
    unsafe {
        // 快照 + 复位在关中断窗口内完成：note_frame 在 IRQ 上下文推进
        // FRAME_STATS，可被抢占的读-清零会把新样本抹掉或跨窗口混算。
        // UART 打印（毫秒级忙等）留在可中断上下文。
        let saved = crate::sched_cfs::read_daif();
        core::arch::asm!("csrci sstatus, 2");
        let s = &mut FRAME_STATS;
        let n = s.count.min(STAT_WINDOW);
        let mut buf = [0u64; STAT_WINDOW];
        buf[..n].copy_from_slice(&s.samples[..n]);
        if n == 0 {
            // STATS_PENDING 跨上下文竞态：IRQ 侧在 report 的 UART 输出期间
            // 再次置位、线程侧消费空窗口 —— n==0 时直接返回，防 n-1 下溢。
            crate::sched_cfs::write_daif(saved);
            return;
        }
        let count = s.count;
        let min = s.min;
        let max = s.max;
        let spike_count = s.spike_count;
        let avg = if count > 0 { s.sum / count as u64 } else { 0 };
        let ravg = if count > 0 { s.render_sum / count as u64 } else { 0 };
        let rmax = s.render_max;
        s.count = 0;
        s.min = u64::MAX;
        s.max = 0;
        s.sum = 0;
        s.samples = [0; STAT_WINDOW];
        s.spike_count = 0;
        s.render_sum = 0;
        s.render_max = 0;
        crate::sched_cfs::write_daif(saved);
        // 插入排序（n ≤ 64，开销可忽略）
        for i in 1..n {
            let key = buf[i];
            let mut j = i;
            while j > 0 && buf[j - 1] > key {
                buf[j] = buf[j - 1];
                j -= 1;
            }
            buf[j] = key;
        }
        let p95 = buf[(n * 95 / 100).min(n - 1)];
        uart(" [anim] 帧统计 n=");
        crate::print_dec(count as u64);
        uart(" 间隔(min=");
        crate::print_dec(min);
        uart(",avg=");
        crate::print_dec(avg);
        uart(",p95=");
        crate::print_dec(p95);
        uart(",max=");
        crate::print_dec(max);
        uart(")us 尖峰(>20ms)=");
        crate::print_dec(spike_count);
        uart(" 渲染(avg=");
        crate::print_dec(ravg);
        uart(",max=");
        crate::print_dec(rmax);
        uart(")us\n");
    }
}

/// 按当前电源/热策略点亮帧时钟（动画启动统一入口）。
///
/// 周期取 `power::anim_frame_interval_us()`：热级别 Hot→90Hz、Critical+→60Hz、
/// 低电封顶 60Hz、常态 120Hz。已有定时器重复调用等价一次平滑 reload。
fn frame_clock_start() {
    crate::timer::timer2_set_interval(crate::power::anim_frame_interval_us());
}

/// 电源/热档位变化后由 power 调用：有活动动画时立即把新帧率生效；
/// 无活动动画时不动作（下次 frame_clock_start 取新策值）。
pub fn refresh_frame_clock() {
    unsafe {
        if ANY_ACTIVE {
            crate::timer::timer2_set_interval(crate::power::anim_frame_interval_us());
        }
    }
}

/// 定时器驱动的帧推进（与调度器同周期，10ms 一次；弹簧内部按实际 dt 积分）。
///
/// 有活动动画且合成器就绪时，在推进后合成并呈现一帧，形成动画渲染管线：
/// `timer IRQ → anim::frame_tick → composite + present`。
pub fn frame_tick() {
    unsafe {
        if !ANY_ACTIVE {
            // 动画可能被 cancel_window（而非自然完成）清空：停掉 CNTP，
            // 避免帧时钟在无动画时空转唤醒。
            crate::timer::timer2_stop();
            return;
        }
        let now = timer::now_us();
        let dt_s = if LAST_TICK_US == 0 {
            0.0
        } else {
            ((now.saturating_sub(LAST_TICK_US)) as f32 / 1_000_000.0).clamp(0.0, 0.1)
        };
        LAST_TICK_US = now;

        let any_active_before = ANY_ACTIVE;
        let mut any = false;
        for a in ANIMS.iter_mut() {
            if !a.active {
                continue;
            }
            any = true;
            if let Some(e) = a.easing {
                let t = now.saturating_sub(a.t0) as f32 / a.dur.max(1) as f32;
                if t >= 1.0 {
                    apply(a.window, a.prop, a.to);
                    finish(a);
                    continue;
                }
                let p = e.sample(t);
                apply(a.window, a.prop, a.from + (a.to - a.from) * p);
            } else if let Some(sp) = a.spring.as_mut() {
                sp.target = a.to;
                if sp.step(dt_s) {
                    apply(a.window, a.prop, sp.pos);
                    finish(a);
                    continue;
                }
                apply(a.window, a.prop, sp.pos);
            }
        }
        ANY_ACTIVE = any;
        // 统一执行完成回调（迭代外，见 FINISH_Q 注释）。
        run_finish_queue();

        // 本 tick 有动画自然结束（任一 active→inactive）：终值 apply 只标记
        // 了脏区，若就此停表返回，屏幕会停留在上一 tick 的中间值（如淡出
        // 窗口的残影 alpha）直到下一次输入 —— 收尾帧立即合成并呈现一次。
        if !ANY_ACTIVE && any_active_before && compositor::ready() && !compositor::fb_active() {
            if compositor::composite() {
                compositor::present();
            }
        }

        // 应用直绘接管后（compositor::fb_active），显示节奏由应用的
        // `fb_present` 决定（应用在帧边界提交脏区并请求翻转），本 tick 只
        // 推进动画并标记脏区域，**不再自行翻转**——否则会在应用绘制中途
        // 翻转，把半帧内容送上扫描输出（撕裂）。内核覆盖窗口（Home
        // Indicator / 变暗层）的动画脏区会在应用下一次 present 时一并合成。
        if ANY_ACTIVE && compositor::ready() && !compositor::fb_active() {
            // 复用本 tick 的 `now` 作为起点（now_us 每次重读 CNTFRQ + u64 除法）。
            let t0 = now;
            if compositor::composite() {
                compositor::present();
                let render_us = timer::now_us().saturating_sub(t0);
                record_frame(now, render_us);
            }
        }
        // 无活动动画：停掉 120Hz CNTP —— 空转时不再被每秒 120 次中断唤醒
        // （内核覆盖窗口动画结束后系统只保留调度 tick，空闲 CPU 可真正 wfi）。
        if !ANY_ACTIVE {
            crate::timer::timer2_stop();
        }
    }
}

// ==================== 启动期动画管线性能自检 ====================

/// 帧率基线测试：验证 CNTP 120Hz 帧时钟驱动 `frame_tick` 全管线，并建立
/// 应用直绘路径（`fb_damage` → 脏区拷贝 → `composite` → `present`）的帧率
/// 基线。
///
/// 场景（直接驱动，模拟应用在帧边界的 present）：
///   0. 定时器看门狗：补间动画 + 开 IRQ，验证 frame_tick 在推进帧（120Hz）；
///   A. 局部脏区（320×200 移动矩形）：时钟/通知/拖动等局部更新的典型开销；
///   B. 整屏脏区（最坏情况：页面切换/整帧重绘）；540×960 实测后按像素比
///      （×4）外推 1080p 全屏预算（纯 CPU 行拷贝/混合为线性带宽瓶颈）；
///   C. 内核覆盖窗口 alpha 混合（Home Indicator 式 overlay）的合成成本。
/// 每个场景报告帧间隔 min/avg/p95/max、掉帧数（>12ms）与合成+呈现耗时。
///
/// 仅在启动期调用（定时器与 GIC 已就绪、调度器尚未启动）：IRQ 默认屏蔽，
/// 本函数自行开/关；应用直绘（launcher）尚未接管显示，不与用户 UI 冲突。

/// 掉帧判定阈值（µs）：120Hz 帧预算 8.33ms 的宽松上界（任务验收阈值）。
const DROP_US: u64 = 12_000;

/// 单场景统计窗口（样本数）。
const PERF_WINDOW: usize = 128;

struct PerfStats {
    count: usize,
    last_us: u64,
    min_iv: u64,
    max_iv: u64,
    sum_iv: u64,
    render_sum: u64,
    render_max: u64,
    drops: u64,
    iv: [u64; PERF_WINDOW],
    render: [u64; PERF_WINDOW],
}

impl PerfStats {
    const fn new() -> PerfStats {
        PerfStats {
            count: 0,
            last_us: 0,
            min_iv: u64::MAX,
            max_iv: 0,
            sum_iv: 0,
            render_sum: 0,
            render_max: 0,
            drops: 0,
            iv: [0; PERF_WINDOW],
            render: [0; PERF_WINDOW],
        }
    }

    fn record(&mut self, now: u64, render_us: u64) {
        if self.last_us != 0 {
            let iv = now.saturating_sub(self.last_us);
            if self.count < PERF_WINDOW {
                self.iv[self.count] = iv;
            }
            if iv < self.min_iv {
                self.min_iv = iv;
            }
            if iv > self.max_iv {
                self.max_iv = iv;
            }
            self.sum_iv += iv;
            if iv > DROP_US {
                self.drops += 1;
            }
        }
        if self.count < PERF_WINDOW {
            self.render[self.count] = render_us;
        }
        self.render_sum += render_us;
        if render_us > self.render_max {
            self.render_max = render_us;
        }
        self.last_us = now;
        self.count += 1;
    }
}

/// 取前 `n` 个样本的 p95（插入排序，n ≤ 128 开销可忽略）。
fn p95_of(arr: &[u64; PERF_WINDOW], n: usize) -> u64 {
    let mut buf = [0u64; PERF_WINDOW];
    buf[..n].copy_from_slice(&arr[..n]);
    for i in 1..n {
        let key = buf[i];
        let mut j = i;
        while j > 0 && buf[j - 1] > key {
            buf[j] = buf[j - 1];
            j -= 1;
        }
        buf[j] = key;
    }
    buf[(n * 95 / 100).min(n - 1)]
}

fn perf_report(label: &str, s: &PerfStats) {
    let n = s.count;
    if n == 0 {
        uart(" [anim-perf] ");
        uart(label);
        uart(" 无样本\n");
        return;
    }
    let p95_iv = p95_of(&s.iv, n);
    let p95_r = p95_of(&s.render, n);
    let avg_iv = s.sum_iv / n as u64;
    let avg_r = s.render_sum / n as u64;
    let fps = if avg_iv > 0 { 1_000_000 / avg_iv } else { 0 };
    uart(" [anim-perf] ");
    uart(label);
    uart(" n=");
    crate::print_dec(n as u64);
    uart(" fps=");
    crate::print_dec(fps);
    uart(" ival(min=");
    crate::print_dec(s.min_iv);
    uart(",avg=");
    crate::print_dec(avg_iv);
    uart(",p95=");
    crate::print_dec(p95_iv);
    uart(",max=");
    crate::print_dec(s.max_iv);
    uart(")us render(avg=");
    crate::print_dec(avg_r);
    uart(",p95=");
    crate::print_dec(p95_r);
    uart(",max=");
    crate::print_dec(s.render_max);
    uart(")us drops(>12ms)=");
    crate::print_dec(s.drops);
    uart("\n");
}

/// 输出「p95 合成+呈现耗时」相对 120Hz 预算与任务阈值（12ms）的判定。
fn perf_verdict(label: &str, p95_render: u64) {
    uart(" [anim-perf] ");
    uart(label);
    uart(" p95-render=");
    crate::print_dec(p95_render);
    uart("us → ");
    if p95_render < 8_333 {
        uart("120Hz 通过（符合 8.33ms 预算）\n");
    } else if p95_render < 12_000 {
        uart("p95<12ms 通过（超出 120Hz 预算）\n");
    } else {
        uart("失败（超过 12ms — 见 docs/perf-baseline.md 降级计划）\n");
    }
}

/// 画布基底色（模拟应用壁纸）。
const CANVAS_BASE: u32 = 0x00_1C_22_2E;
/// 移动矩形色（模拟应用强调色控件）。
const CANVAS_MOVE: u32 = 0x00_42_85_F4;

/// 在持久画布（fb0 映射目标）铺一层渐变「壁纸」。
fn fill_canvas_base() {
    let dst = compositor::fb_target() as *mut u32;
    let w = compositor::width() as usize;
    let h = compositor::height() as usize;
    for y in 0..h {
        for x in 0..w {
            unsafe {
                *dst.add(y * w + x) =
                    CANVAS_BASE + (((x / 64) + (y / 64)) % 3) as u32 * 0x00_00_12_08;
            }
        }
    }
}

/// 在画布上填 `r` 矩形（越界裁剪）。
fn fill_canvas_rect(r: compositor::Rect, color: u32) {
    let dst = compositor::fb_target() as *mut u32;
    let w = compositor::width() as usize;
    for y in r.y.max(0)..r.bottom() {
        for x in r.x.max(0)..r.right() {
            unsafe {
                *dst.add(y as usize * w + x as usize) = color;
            }
        }
    }
}

/// 整帧换色（模拟整帧重绘的不同内容）。
fn fill_canvas_frame(f: usize) {
    let dst = compositor::fb_target() as *mut u32;
    let w = compositor::width() as usize;
    let h = compositor::height() as usize;
    let c = CANVAS_BASE + ((f % 4) as u32) * 0x00_01_02_03;
    for y in 0..h {
        for x in 0..w {
            unsafe {
                *dst.add(y * w + x) = c;
            }
        }
    }
}

pub fn perf_test() {
    uart(" [anim-perf] 渲染管线性能测试开始\n");
    if !compositor::ready() {
        uart(" [anim-perf] 合成器未就绪，跳过测试\n");
        return;
    }
    let sw = compositor::width();
    let sh = compositor::height();
    uart(" [anim-perf] 原生分辨率 ");
    crate::print_dec(sw as u64);
    uart("x");
    crate::print_dec(sh as u64);
    uart("\n");

    // ---- 0. 定时器看门狗：CNTP 120Hz → frame_tick → composite → present ----
    let w = compositor::create_window(30, 200, 320, 400, 10, 255);
    if w != usize::MAX {
        if let Some(s) = compositor::window_surface(w) {
            s.fill(compositor::Color::ACCENT);
        }
        tween(w, AnimProp::X, 30.0, 300.0, 300_000, Easing::EaseInEaseOut, None);
        let start = timer::now_us();
        let deadline = start + 1_000_000;
        unsafe {
            asm!("csrsi sstatus, 2");
        }
        while active() && timer::now_us() < deadline {
            core::hint::spin_loop();
        }
        unsafe {
            asm!("csrci sstatus, 2");
        }
        let frames = frames_presented();
        uart(" [anim-perf] 300ms 动画内定时器驱动帧数：");
        crate::print_dec(frames);
        uart(if frames >= 20 {
            "（120Hz 帧时钟正常）\n"
        } else {
            "（WARN：定时器未驱动帧节拍！）\n"
        });
        compositor::destroy_window(w);
    } else {
        uart(" [anim-perf] 创建窗口失败，跳过定时器看门狗\n");
    }

    // 基准：持久画布铺壁纸并整屏上屏
    fill_canvas_base();
    compositor::fb_present(&[compositor::fb_full_rect()]);

    // ---- A. 局部脏区（320×200 移动矩形，模拟时钟/通知/拖动） ----
    let mut stats_a = PerfStats::new();
    let mut st_a = (0u64, 0u64, 0u64);
    let mut x = 30i32;
    for _ in 0..120 {
        let old = compositor::Rect::new(x - 3, 220, 320, 200);
        let new = compositor::Rect::new(x, 220, 320, 200);
        // 模拟应用直绘：擦旧画新（写入持久画布，时间不计入合成）
        fill_canvas_rect(old, CANVAS_BASE);
        fill_canvas_rect(new, CANVAS_MOVE);
        let t0 = timer::now_us();
        compositor::fb_present(&[old, new]);
        let t1 = timer::now_us();
        stats_a.record(t1, t1 - t0);
        let (c, k, p) = compositor::last_stage_us();
        st_a.0 += c;
        st_a.1 += k;
        st_a.2 += p;
        x += 3;
        if x as u32 + 320 >= sw {
            x = 30;
        }
    }
    perf_report("A/local-dirty-320x200", &stats_a);
    uart(" [anim-perf] A/stage-us meta+copy=");
    crate::print_dec(st_a.0 / 120);
    uart(" comp=");
    crate::print_dec(st_a.1 / 120);
    uart(" present=");
    crate::print_dec(st_a.2 / 120);
    uart("us/帧\n");

    // ---- B. 整屏脏区（最坏情况：整帧重绘） ----
    let mut stats_b = PerfStats::new();
    let mut st_b = (0u64, 0u64, 0u64);
    for f in 0..60 {
        fill_canvas_frame(f);
        let t0 = timer::now_us();
        compositor::fb_present(&[compositor::fb_full_rect()]);
        let t1 = timer::now_us();
        stats_b.record(t1, t1 - t0);
        let (c, k, p) = compositor::last_stage_us();
        st_b.0 += c;
        st_b.1 += k;
        st_b.2 += p;
    }
    perf_report("B/full-screen", &stats_b);
    uart(" [anim-perf] B/stage-us meta+copy=");
    crate::print_dec(st_b.0 / 60);
    uart(" comp=");
    crate::print_dec(st_b.1 / 60);
    uart(" present=");
    crate::print_dec(st_b.2 / 60);
    uart("us/帧\n");

    // ---- C. 内核覆盖窗口 alpha 混合（Home Indicator 式 overlay） ----
    let ov = compositor::create_window(30, 30, 200, 200, i32::MAX, 128);
    let mut stats_c = PerfStats::new();
    if ov != usize::MAX {
        if let Some(s) = compositor::window_surface(ov) {
            s.fill(compositor::Color::WHITE);
        }
        let mut ox = 30i32;
        for _ in 0..120 {
            let old = compositor::Rect::new(ox, 30, 200, 200);
            compositor::move_window(ov, ox + 3, 30);
            let new = compositor::Rect::new(ox + 3, 30, 200, 200);
            let t0 = timer::now_us();
            compositor::fb_present(&[old, new]);
            let t1 = timer::now_us();
            stats_c.record(t1, t1 - t0);
            ox += 3;
            if ox + 200 >= sw as i32 {
                ox = 30;
            }
        }
        perf_report("C/overlay-alpha-200x200", &stats_c);
        compositor::destroy_window(ov);
    } else {
        uart(" [anim-perf] 叠加窗口创建失败，跳过叠加测试\n");
    }

    // ---- D. 行拷贝对齐基线：同一 320×200 矩形在偶/奇 x0 下的纯拷贝耗时 ----
    // 奇 x0 → 行首 ≡4 (mod 8)：memcpy 前置 4 字节对齐宽通道生效时 D-odd ≈
    // D-even；若退化为字节循环则 D-odd 数倍于 D-even（脏区 x 为奇数像素的
    // 光标/局部更新正是此形态）。
    let d_even = compositor::bench_row_copy(64, 320, 200, 50);
    let d_odd = compositor::bench_row_copy(65, 320, 200, 50);
    uart(" [anim-perf] D/row-copy-320x200 even-x0=");
    crate::print_dec(d_even);
    uart("us odd-x0=");
    crate::print_dec(d_odd);
    uart("us odd/even=");
    crate::print_dec(if d_even == 0 { 0 } else { d_odd * 100 / d_even });
    uart("%\n");
    // D2：跨奇偶（混合对齐形态：src ≡0 / dst ≡4 mod 8 及其反向），
    // 监控 memcpy u32 字循环分支。
    let d2_eo = compositor::bench_row_copy_cross(64, 65, 320, 200, 50);
    let d2_oe = compositor::bench_row_copy_cross(65, 64, 320, 200, 50);
    uart(" [anim-perf] D2/cross-parity-320x200 even→odd=");
    crate::print_dec(d2_eo);
    uart("us odd→even=");
    crate::print_dec(d2_oe);
    uart("us （对照 D 同奇偶=");
    crate::print_dec(d_even);
    uart("us）\n");

    // ---- E. 重叠拷贝基线：memmove 反向路径（滚动/同缓冲平移形态） ----
    // 同缓冲 320×200 块下移 16 字节（dst>src 全程重叠）；附单次语义正确性
    // 抽查（序号模式逐字核对），反向宽通道错序会以「正确性失败」暴露。
    let e = compositor::bench_overlap_copy(200, 30);
    uart(" [anim-perf] E/overlap-copy-320x200+16B ");
    if e == u64::MAX {
        uart("正确性失败!\n");
    } else {
        uart("=");
        crate::print_dec(e);
        uart("us/次\n");
    }

    // ---- 汇总：1080p 外推 + 判定 ----
    let n_b = stats_b.count.max(1);
    let full_avg = stats_b.render_sum / n_b as u64;
    let full_p95 = p95_of(&stats_b.render, stats_b.count);
    // 1080p = 4 × 540×960 像素；纯 CPU 行拷贝/混合为线性带宽瓶颈 → 按像素比外推
    let est_avg = full_avg.saturating_mul(4);
    let est_p95 = full_p95.saturating_mul(4);
    uart(" [anim-perf] 1080p 估算（4 倍像素，线性缩放）：整屏渲染平均=");
    crate::print_dec(est_avg);
    uart("us p95=");
    crate::print_dec(est_p95);
    uart("us\n");
    if sw == 540 && sh == 960 {
        perf_verdict("540x960 full-screen", full_p95);
        perf_verdict("1080p full-screen (estimate)", est_p95);
    } else {
        perf_verdict("native full-screen", full_p95);
    }
    uart(" [anim-perf] 渲染管线性能测试结束\n");
}

// ==================== 自检 ====================

/// 曲线与弹簧的数值自检（启动时由 `self_test` 调用，打印到串口）。
///
/// 参考值来源：
/// - `EaseInOut(0.5) ≈ 0.4998`（对称性）
/// - `FastOutSlowIn(0.5) ≈ 0.7754`（Material Design 官方文档值）
/// - `LinearOutSlowIn(0.5) ≈ 0.8392` / `FastOutLinearIn(0.5) ≈ 0.3248`
pub fn self_test() {
    uart(" [anim] easing curves (iOS/Android)\n");
    let cases: [(Easing, f32, f32, &str); 8] = [
        (Easing::Linear, 0.5, 0.5, "Linear(0.5)"),
        (Easing::EaseIn, 0.5, 0.3152, "EaseIn(0.5)"),
        (Easing::EaseOut, 0.5, 0.6848, "EaseOut(0.5)"),
        (Easing::EaseInEaseOut, 0.5, 0.4998, "EaseInEaseOut(0.5)"),
        (Easing::FastOutSlowIn, 0.5, 0.7754, "FastOutSlowIn(0.5)"),
        (Easing::LinearOutSlowIn, 0.5, 0.8392, "LinearOutSlowIn(0.5)"),
        (Easing::FastOutLinearIn, 0.5, 0.3248, "FastOutLinearIn(0.5)"),
        (Easing::Emphasized, 0.5, 0.8778, "Emphasized(0.5)"),
    ];
    let mut ok = true;
    for (e, t, expect, name) in cases {
        let got = e.sample(t);
        let pass = (got - expect).abs() < 0.01;
        ok &= pass;
        uart("   ");
        uart(name);
        uart("=");
        print_fixed(got);
        uart(if pass { " OK\n" } else { " FAIL\n" });
    }

    // 弹簧：iOS 风格（damping 0.8）从 0 弹向 1，应在 ~1s 内稳定
    uart(" [anim] spring physics\n");
    let mut sp = Spring::ios(0.8, 0.0);
    sp.pos = 0.0;
    sp.target = 1.0;
    let mut settled = false;
    let mut t = 0.0f32;
    while t < 2.0 {
        if sp.step(0.016) {
            settled = true;
            break;
        }
        t += 0.016;
    }
    let pass_ios = settled && (sp.pos - 1.0).abs() < 0.01;
    ok &= pass_ios;
    uart("   ios(0.8) settle=");
    print_fixed(sp.pos);
    uart(if pass_ios { " OK\n" } else { " FAIL\n" });

    // 弹簧：Android 风格（stiffness 210, damping 1 临界）从 0 弹向 1
    let mut sp = Spring::android(210.0, 1.0);
    sp.pos = 0.0;
    sp.target = 1.0;
    let mut settled = false;
    let mut t = 0.0f32;
    while t < 2.0 {
        if sp.step(0.016) {
            settled = true;
            break;
        }
        t += 0.016;
    }
    let pass_android = settled && (sp.pos - 1.0).abs() < 0.01;
    ok &= pass_android;
    uart("   android(210,1) settle=");
    print_fixed(sp.pos);
    uart(if pass_android { " OK\n" } else { " FAIL\n" });

    uart(if ok { " [anim] self-test OK\n" } else { " [anim] self-test FAIL\n" });
}

/// 打印 f32 为定点值（×10000，取整）。
fn print_fixed(v: f32) {
    let q = (v * 10_000.0) as u64;
    crate::print_dec(q);
}

fn uart(s: &str) {
    crate::uart_send_str(s);
}
