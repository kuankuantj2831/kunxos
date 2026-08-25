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
        }
    }

    /// 在进度 `t`（0..=1）处的曲线值（0..=1）。
    pub fn sample(self, t: f32) -> f32 {
        let (x1, y1, x2, y2) = self.control_points();
        solve_bezier(x1, y1, x2, y2, t)
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
    for _ in 0..8 {
        let err = bezier_sample(0.0, x1, x2, 1.0, t) - x;
        if err.abs() < EPS {
            break;
        }
        let d = bezier_deriv(0.0, x1, x2, 1.0, t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= err / d;
    }
    // 二分细化，消除牛顿迭代的残余误差
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
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
    /// 完成回调（如链式动画 / 销毁窗口）。
    on_finish: Option<fn()>,
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
    on_finish: Option<fn()>,
) -> bool {
    unsafe {
        for a in ANIMS.iter_mut() {
            if !a.active {
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
                return true;
            }
        }
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
    on_finish: Option<fn()>,
) -> bool {
    sp.pos = from;
    sp.target = to;
    unsafe {
        for a in ANIMS.iter_mut() {
            if !a.active {
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
                return true;
            }
        }
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

fn finish(a: &mut Anim) {
    a.active = false;
    if let Some(cb) = a.on_finish {
        cb();
    }
}

/// 取消指定窗口的全部动画（供 `destroy_window` 调用）。
///
/// 窗口销毁后其 id 可能被新窗口复用，残留的旧动画会继续驱动新窗口
/// （位置/透明度/缩放被旧目标覆盖）。销毁前必须取消。
pub fn cancel_window(window: usize) {
    unsafe {
        let mut any = false;
        for a in ANIMS.iter_mut() {
            if a.active && a.window == window {
                a.active = false;
            } else if a.active {
                any = true;
            }
        }
        ANY_ACTIVE = any;
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
        if s.count >= STAT_REPORT_EVERY {
            report_frame_stats();
        }
    }
}

/// 打印当前统计窗口并重置（保留 `last_frame_us` 保证跨窗口间隔连续）。
fn report_frame_stats() {
    unsafe {
        let s = &mut FRAME_STATS;
        let n = s.count.min(STAT_WINDOW);
        let mut buf = [0u64; STAT_WINDOW];
        buf[..n].copy_from_slice(&s.samples[..n]);
        // 插入排序（n ≤ 64，IRQ 上下文开销可忽略）
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
        let avg = if s.count > 0 { s.sum / s.count as u64 } else { 0 };
        let ravg = if s.count > 0 { s.render_sum / s.count as u64 } else { 0 };
        uart(" [anim] frame stats n=");
        crate::print_dec(s.count as u64);
        uart(" ival(min=");
        crate::print_dec(s.min);
        uart(",avg=");
        crate::print_dec(avg);
        uart(",p95=");
        crate::print_dec(p95);
        uart(",max=");
        crate::print_dec(s.max);
        uart(")us spikes(>20ms)=");
        crate::print_dec(s.spike_count);
        uart(" render(avg=");
        crate::print_dec(ravg);
        uart(",max=");
        crate::print_dec(s.render_max);
        uart(")us\n");
        s.count = 0;
        s.min = u64::MAX;
        s.max = 0;
        s.sum = 0;
        s.samples = [0; STAT_WINDOW];
        s.spike_count = 0;
        s.render_sum = 0;
        s.render_max = 0;
    }
}

/// 定时器驱动的帧推进（与调度器同周期，10ms 一次；弹簧内部按实际 dt 积分）。
///
/// 有活动动画且合成器就绪时，在推进后合成并呈现一帧，形成动画渲染管线：
/// `timer IRQ → anim::frame_tick → composite + present`。
pub fn frame_tick() {
    unsafe {
        if !ANY_ACTIVE {
            return;
        }
        let now = timer::now_us();
        let dt_s = if LAST_TICK_US == 0 {
            0.0
        } else {
            ((now.saturating_sub(LAST_TICK_US)) as f32 / 1_000_000.0).clamp(0.0, 0.1)
        };
        LAST_TICK_US = now;

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

        if ANY_ACTIVE && compositor::ready() {
            let t0 = timer::now_us();
            if compositor::composite() {
                compositor::present();
                let render_us = timer::now_us() - t0;
                record_frame(now, render_us);
            }
        }
    }
}

// ==================== 启动期动画管线性能自检 ====================

/// 启动阶段跑一段补间动画，驱动 `frame_tick → composite + present` 全管线，
/// 收集第 0 步的帧间隔统计后恢复 `/dev/fb0` 扫描输出。
///
/// 仅在启动期调用（定时器与 GIC 已就绪、调度器尚未启动）：IRQ 在此期间默认
/// 屏蔽，本函数自行开/关；动画结束后把扫描输出切回 `/dev/fb0` 的缓冲，
/// 保证 launcher/OOBE 的直接绘制继续可见。
pub fn perf_test() {
    uart(" [anim-perf] pipeline perf test begin\n");
    if !compositor::ready() {
        uart(" [anim-perf] compositor not ready, skip\n");
        return;
    }
    let w = compositor::create_window(30, 200, 320, 400, 10, 255);
    if w == usize::MAX {
        uart(" [anim-perf] create_window failed\n");
        return;
    }
    // 窗口表面画一点内容，让合成有真实工作量（非纯色填充）
    if let Some(s) = compositor::window_surface(w) {
        s.fill(compositor::Color::ACCENT);
        let mut y = 0i32;
        while y < s.h as i32 {
            let mut x = 0i32;
            while x < s.w as i32 {
                s.set(x, y, compositor::Color::WHITE.pixel());
                x += 32;
            }
            y += 32;
        }
    }

    // 1.5s 水平补间：100Hz 定时器下约 150 帧，触发 2 次统计报告
    tween(w, AnimProp::X, 30.0, 500.0, 1_500_000, Easing::EaseInEaseOut, None);

    // 开 IRQ，让定时器驱动 frame_tick；自旋等待动画结束（硬截止 3s）。
    // 看门狗：50ms 内无任何帧呈现 → 定时器未驱动，提前退出避免拖慢启动。
    let start = timer::now_us();
    let deadline = start + 3_000_000;
    unsafe {
        asm!("msr daifclr, #2");
    }
    let mut watchdog_checked = false;
    while active() && timer::now_us() < deadline {
        if !watchdog_checked && timer::now_us().saturating_sub(start) > 50_000 {
            watchdog_checked = true;
            if frames_presented() == 0 {
                uart(" [anim-perf] no frames advanced (timer not driving), abort\n");
                break;
            }
        }
        core::hint::spin_loop();
    }
    unsafe {
        asm!("msr daifset, #2");
    }

    compositor::destroy_window(w);
    // 恢复扫描输出到 /dev/fb0 的缓冲，保证后续 GUI 进程直接绘制可见
    let fb = crate::gpu_simple::fb_base();
    if fb != 0 {
        crate::gpu_simple::present(fb);
    }
    uart(" [anim-perf] pipeline perf test end\n");
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
