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
