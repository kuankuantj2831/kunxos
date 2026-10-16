//! KunxOS 应用侧共享运动库（no_std、无堆、纯四则运算）。
//!
//! `docs/animations.md` 运动规范的应用侧轻量实现，供 launcher 与各桌面应用
//! 经 `#[path]` 引入（同 nav.rs / glass.rs 模式），每进程一份、零分配。
//!
//! 能力：
//!   - 贝塞尔缓动（牛顿迭代 + 二分兜底，与内核 `kunxcore/src/anim.rs` 同算法）
//!   - 阻尼谐振子弹簧（半隐式欧拉 1ms 子步，含数值稳定防护与 retarget 接续）
//!   - 进度 / 三角波呼吸 / 线性插值等无 `sin` 辅助（no_std 无 libm）

// ==================== 贝塞尔缓动 ====================

/// 三次贝塞尔时序函数曲线（控制点 `(x1, y1, x2, y2)`）。
///
/// 数值与 `docs/animations.md` §1 一致；`Emphasized` 为 Material 3 强调曲线。
#[derive(Clone, Copy, PartialEq)]
pub enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    FastOutSlowIn,
    LinearOutSlowIn,
    FastOutLinearIn,
    Emphasized,
    /// ColorOS 17 标准曲线：快速前冲 + 长尾缓停（ColorOS 动效规范主曲线）。
    ColorOSStandard,
    /// ColorOS 强调进入曲线：起步快、收尾极缓（用于入场/展开类动画）。
    ColorOSEmphasized,
}

impl Easing {
    const fn ctrl(self) -> (f32, f32, f32, f32) {
        match self {
            Easing::Linear => (0.0, 0.0, 1.0, 1.0),
            Easing::EaseIn => (0.42, 0.0, 1.0, 1.0),
            Easing::EaseOut => (0.0, 0.0, 0.58, 1.0),
            Easing::EaseInOut => (0.42, 0.0, 0.58, 1.0),
            Easing::FastOutSlowIn => (0.4, 0.0, 0.2, 1.0),
            Easing::LinearOutSlowIn => (0.0, 0.0, 0.2, 1.0),
            Easing::FastOutLinearIn => (0.4, 0.0, 1.0, 1.0),
            Easing::Emphasized => (0.2, 0.0, 0.0, 1.0),
            Easing::ColorOSStandard => (0.22, 1.0, 0.36, 1.0),
            Easing::ColorOSEmphasized => (0.3, 0.0, 0.1, 1.0),
        }
    }

    /// 求输入进度 `x ∈ [0,1]` 处的曲线值 `y ∈ [0,1]`。
    pub fn eval(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        let (x1, y1, x2, y2) = self.ctrl();
        if x1 == y1 && x2 == y2 {
            return x; // Linear 快路径
        }
        solve_bezier(x1, y1, x2, y2, x)
    }
}

fn bezier_sample(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let u = 1.0 - t;
    u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
}

fn bezier_deriv(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let u = 1.0 - t;
    3.0 * u * u * (b - a) + 6.0 * u * t * (c - b) + 3.0 * t * t * (d - c)
}

/// 求 x 曲线（控制点 0, x1, x2, 1）在给定 x 处的参数 t：
/// 牛顿迭代 + 二分兜底（WebKit/Chromium 同款算法）。
fn solve_x(x1: f32, x2: f32, x: f32) -> f32 {
    let mut t = x;
    for _ in 0..8 {
        let err = bezier_sample(0.0, x1, x2, 1.0, t) - x;
        if err.abs() < 1e-6 {
            return t;
        }
        let d = bezier_deriv(0.0, x1, x2, 1.0, t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= err / d;
        if !(0.0..=1.0).contains(&t) {
            break;
        }
    }
    // 二分兜底
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    t = x.clamp(0.0, 1.0);
    for _ in 0..24 {
        let v = bezier_sample(0.0, x1, x2, 1.0, t);
        if (v - x).abs() < 1e-6 {
            break;
        }
        if v < x {
            lo = t;
        } else {
            hi = t;
        }
        t = 0.5 * (lo + hi);
    }
    t
}

fn solve_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    bezier_sample(0.0, y1, y2, 1.0, solve_x(x1, x2, x)).clamp(0.0, 1.0)
}

// ==================== 弹簧物理 ====================

/// f32 平方根（牛顿迭代，纯四则运算，避免依赖 libm）。
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

/// 阻尼谐振子弹簧：`m·x'' + c·x' + k·(x − target) = 0`，半隐式欧拉积分
/// （1ms 子步）。与内核 `kunxcore/src/anim.rs` 同款数值与稳定防护。
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
    /// iOS 风格：`damping_ratio`（0..1 欠阻尼 / 1 临界）+ `initial_velocity`（值/秒）。
    pub fn ios(damping_ratio: f32, initial_velocity: f32) -> Spring {
        let stiffness = 280.0;
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

    /// Android 风格：`stiffness`（N/m，AndroidX 默认 210）+ `damping_ratio`。
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

    /// 重设目标并接续当前值/速度（鸿蒙 springMotion 打断接续的应用侧等价物：
    /// 打断旧动画时把当前值与速度喂进来，新弹簧从运动中无缝接管）。
    pub fn retarget(&mut self, target: f32, pos: f32, vel: f32) {
        self.target = target;
        self.pos = pos;
        self.vel = vel;
    }

    /// 推进 `dt` 秒。返回是否已稳定在目标附近。
    ///
    /// 稳定防护：`mass` ≥ 0.1、`stiffness` ≤ 20_000（半隐式欧拉 ωh < 2）、
    /// 积分出非有限值时强制收敛，防止动画永不结束。
    pub fn step(&mut self, dt: f32) -> bool {
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
            if !self.pos.is_finite() || !self.vel.is_finite() {
                self.pos = self.target;
                self.vel = 0.0;
                return true;
            }
        }
        (self.pos - self.target).abs() < 0.0005 && self.vel.abs() < 0.001
    }
}

// ==================== 下拉刷新 ====================

/// 下拉刷新阶段（docs/animations-roadmap.md P1-6）。
#[derive(Clone, Copy, PartialEq)]
pub enum PrState {
    /// 空闲（无手势、无动画）。
    Idle,
    /// 跟手下拉中。
    Pulling,
    /// 越过阈值松手，正在刷新（三圆点 loader 期）。
    Loading,
    /// 回弹中（弹簧拉回 0）。
    Settling,
}

/// 通用下拉刷新：跟手 1:1 → 越界 0.35x 阻尼（与翻页橡皮筋一致）→
/// 松手弹簧回弹；Loading 期三圆点错峰缩放（周期 900ms，错峰 300ms）。
///
/// 应用侧接线（无分配、每帧几十条指令）：
/// 1. 每帧 `frame(TOUCH_DOWN && 起点在列表区, CUR_Y, TOUCH_PY, now)`；
/// 2. `just_triggered()` 为真的那帧执行数据刷新并 `mark_ready()`；
/// 3. `step(now)` 推进 Loading/Settling（返回是否仍需重绘）；
/// 4. 松手帧先问 `ate_tap()`，为真则吞掉本帧 tap（下拉不是点击）。
#[derive(Clone, Copy)]
pub struct PullRefresh {
    pub state: PrState,
    /// 当前下拉位移（px，>= 0；Loading/Settling 期为动画中的位移）。
    pub pull: f32,
    /// 触发刷新的下拉阈值（px）。
    pub trigger: f32,
    /// Loading 最短展示时长（us）：数据就绪后也要播完 loader。
    pub loading_min_us: u64,
    /// Loading 吸附的展示位移（px）。
    pub hold: f32,
    ready: bool,
    t0: u64,
    last: u64,
    spring: Spring,
    ate: bool,
    triggered: bool,
}

impl PullRefresh {
    /// damping = 2·√(k·m)·ζ = 2·√280·0.9 ≈ 30.12（const fn 不能调 fsqrt，
    /// 预计算常数，与 `Spring::ios(0.9, 0.0)` 等价）。
    pub const fn new() -> Self {
        PullRefresh {
            state: PrState::Idle,
            pull: 0.0,
            trigger: 64.0,
            loading_min_us: 900_000,
            hold: 56.0,
            ready: false,
            t0: 0,
            last: 0,
            spring: Spring {
                stiffness: 280.0,
                damping: 30.12,
                mass: 1.0,
                target: 0.0,
                pos: 0.0,
                vel: 0.0,
            },
            ate: false,
            triggered: false,
        }
    }

    /// 每帧喂触摸状态；返回本帧是否需要重绘。
    /// `down` 由应用按「按下且起点在可下拉区域」给出。
    pub fn frame(&mut self, down: bool, cur_y: i32, press_y: i32, now: u64) -> bool {
        // ★ last 只在手势期（Idle/Pulling）stamp：Loading/Settling 期由
        // step() 独占推进——应用按文档「每帧 frame + step」接线时两次
        // now_us() 仅隔几微秒，若 frame 也盖 last，step 的帧间隔恒被钳到
        // 下限 1ms（真实帧周期 16ms），弹簧以 ~1/16 实时速度积分。
        // 另：按下即清残留吞击标记（不论状态——Loading 期的陈旧 ate 会
        // 吞掉后续独立手势的 tap）。
        if down {
            self.ate = false;
        }
        match self.state {
            PrState::Idle => {
                if down && cur_y - press_y > 12 {
                    self.state = PrState::Pulling;
                    self.last = now;
                    self.pull = (cur_y - press_y) as f32;
                    return true;
                }
                false
            }
            PrState::Pulling => {
                if down {
                    self.last = now;
                    let raw = (cur_y - press_y).max(0) as f32;
                    // 跟手 1:1，越过阈值后 0.35x 阻尼（橡皮筋）。
                    let d = if raw <= self.trigger {
                        raw
                    } else {
                        self.trigger + (raw - self.trigger) * 0.35
                    };
                    if (d - self.pull).abs() >= 0.75 {
                        self.pull = d;
                        return true;
                    }
                    false
                } else {
                    // 松手：越过阈值进 Loading，否则直接回弹。
                    self.ate = self.pull >= 6.0;
                    self.last = now;
                    if self.pull >= self.trigger {
                        self.state = PrState::Loading;
                        self.ready = false;
                        self.triggered = true;
                        self.t0 = now;
                        self.spring.retarget(self.hold, self.pull, 0.0);
                    } else {
                        self.begin_settle(now);
                    }
                    true
                }
            }
            _ => false,
        }
    }

    /// 本帧是否刚触发刷新（应用执行数据重载后调 `mark_ready`）。
    pub fn just_triggered(&mut self) -> bool {
        let t = self.triggered;
        self.triggered = false;
        t
    }

    /// 数据刷新完成（Loading 至少播满 `loading_min_us` 才回弹）。
    pub fn mark_ready(&mut self) {
        if self.state == PrState::Loading {
            self.ready = true;
        }
    }

    /// 松手帧是否应吞掉 tap（下拉激活，不是点击）。读后自清。
    pub fn ate_tap(&mut self) -> bool {
        let a = self.ate;
        self.ate = false;
        a
    }

    fn begin_settle(&mut self, now: u64) {
        self.state = PrState::Settling;
        // 速度接续：从 Loading 吸附位平滑转入回弹（P2-4 同思想）。
        let v = self.spring.vel;
        self.spring.retarget(0.0, self.pull, v);
        self.t0 = now;
    }

    /// 推进 Loading/Settling 动画；返回是否仍需逐帧重绘。
    pub fn step(&mut self, now: u64) -> bool {
        // 帧间隔（钳 1..40ms：停顿后跳变不产生大步长积分）。
        let h = now.saturating_sub(self.last).clamp(1_000, 40_000);
        self.last = now;
        match self.state {
            PrState::Loading => {
                let _ = self.spring.step(h as f32 / 1_000_000.0);
                self.pull = self.spring.pos;
                let elapsed = now.saturating_sub(self.t0);
                if self.ready
                    && elapsed >= self.loading_min_us
                    && (self.spring.pos - self.hold).abs() < 1.0
                {
                    self.begin_settle(now);
                }
                true
            }
            PrState::Settling => {
                if self.spring.step(h as f32 / 1_000_000.0) {
                    self.pull = 0.0;
                    self.state = PrState::Idle;
                } else {
                    self.pull = self.spring.pos;
                }
                true
            }
            _ => false,
        }
    }

    /// Loading 三圆点相位（0..1..0 三角波，`dot` 0..=2 错峰 300ms）。
    pub fn loader_phase(&self, now: u64, dot: usize) -> f32 {
        if self.t0 == 0 {
            return 0.0;
        }
        let ph = now.saturating_sub(self.t0).saturating_add(dot as u64 * 300_000);
        triangle(ph, 900_000)
    }
}

// ==================== 时间辅助 ====================

/// 时间型动画进度：`(now − t0) / dur`，钳制到 [0, 1]。`dur_us` 为 0 时直接返回 1。
pub fn progress(t0: u64, dur_us: u64, now: u64) -> f32 {
    if now <= t0 {
        return 0.0;
    }
    if dur_us == 0 {
        return 1.0;
    }
    let el = now.saturating_sub(t0);
    if el >= dur_us {
        1.0
    } else {
        el as f32 / dur_us as f32
    }
}

/// 三角波呼吸 0..1..0（周期 `period_us`，无 sin 近似，OOBE 呼吸同款）。
pub fn triangle(now: u64, period_us: u64) -> f32 {
    if period_us == 0 {
        return 0.0;
    }
    let t = (now % period_us) as f32 / period_us as f32;
    1.0 - (2.0 * t - 1.0).abs()
}

/// 线性插值。
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}
