//! KunxOS「星辰之光」UI 控件库（EL0 用户态，no_std、无堆）。
//!
//! 实现三个与「沙子 / 光芒」主题一致的控件（[`widgets`]）：
//! - [`widgets::BrightnessSlider`]：亮度滑块——七彩沙粒堆叠 + 底部漏沙蒸发、发光星辰 thumb。
//! - [`widgets::StellarToggle`]：星辰开关——沙子在胶囊内填满 / 泄漏、开启时边缘光晕。
//! - [`widgets::NebulaInput`]：星尘输入框——星光笔迹逐字浮现、退格散落成沙、星云选区。
//!
//! 与任务草案接口的差异（依据 KunxOS 规范：no_std、无堆、Cortex-A72 单核 60fps）：
//! - `Vec<SandParticle>` / `Vec<TextCharAnim>` 改为**定长粒子池**（`[Particle; 500]`）
//!   与定长动画队列，不引入堆分配器，满足「单控件粒子数 ≤ 500」约束。
//! - `render` 为 `&mut self`：粒子运动 / 动画在渲染时就地推进
//!   （与 oobe `Button::draw(&mut self)` 同一风格）。
//! - 沙堆积为「列栈」模型：每列仅维护堆顶高度，O(cols) 更新，堆积 / 漏沙都实时计算。
//!
//! # 使用示例：在设置面板放置一个亮度滑块
//!
//! ```ignore
//! // apps/settings 风格（app_kit 提供 BACK_BUF / 触摸轮询）
//! use kunxui::{Canvas, Rect, TouchEvent, TouchPhase, Orientation, widgets::*};
//! #[path = "../../../shared/app_kit.rs"] mod kit;
//!
//! struct KitCanvas;                                 // 适配 app_kit 到 kunxui::Canvas
//! impl Canvas for KitCanvas {
//!     fn draw_pixel(&mut self, x: i32, y: i32, c: u32) { kit::put_pixel(x, y, c); }
//!     fn backdrop(&self, x: i32, y: i32) -> u32 {
//!         if x < 0 || y < 0 || x >= kit::DISPLAY_W as i32 || y >= kit::DISPLAY_H as i32 { 0 }
//!         else { unsafe { kit::BACK_BUF[y as usize * kit::DISPLAY_W + x as usize] } }
//!     }
//! }
//!
//! static mut SLIDER: BrightnessSlider =
//!     BrightnessSlider::new(Rect::new(150, 400, 240, 28), Orientation::Horizontal);
//!
//! // —— 每帧 ——
//! fn settings_frame(dt: f32, tap: Option<(i32, i32)>) {
//!     let s = unsafe { &mut SLIDER };
//!     if let Some((x, y)) = tap {
//!         s.handle_touch(&TouchEvent::up(x, y, 0));
//!     }
//!     let mut cv = KitCanvas;
//!     s.render(&mut cv, dt);                        // 粒子就地推进 + 绘制
//!     // 写回亮度配置（/data/oobe/brightness）：
//!     // let v = (s.value * 100.0) as i32;
//!     // cfg::cfg_set_int(cfg::CONF_DISPLAY, b"brightness", v);
//! }
//! ```
//!
//! # 性能
//!
//! 单个控件 ≤ 500 粒；沙堆用列顶高度表替代逐像素势场；发光为半径受限的
//! 径向渐变 + additive 混合，无需预渲染精灵图。整帧单控件像素写入约
//! 5k~10k，AArch64 @ 1.5GHz 单核 60fps 预算内。

#![cfg_attr(not(test), no_std)]
#![allow(dead_code)]

pub mod widgets;

// 抗锯齿超椭圆圆角核心（与系统 glass.rs 同一份实现，ColorOS 15 级圆滑）。
#[path = "../../shared/glass.rs"]
pub mod smooth;

use core::f32::consts::PI;

// ==================== 基础类型 ====================

/// 矩形（整数像素坐标，右 / 下边界开）。语义与 compositor::Rect 相同。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    /// 点是否在矩形内。
    pub const fn contains(self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    /// 右边界（开）。
    pub const fn right(self) -> i32 {
        self.x + self.w
    }
    /// 下边界（开）。
    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }
    /// 水平中心。
    pub const fn cx(self) -> i32 {
        self.x + self.w / 2
    }
    /// 垂直中心。
    pub const fn cy(self) -> i32 {
        self.y + self.h / 2
    }
    /// 向内部收缩 `n` 像素。
    pub const fn inset(self, n: i32) -> Rect {
        Rect {
            x: self.x + n,
            y: self.y + n,
            w: self.w - 2 * n,
            h: self.h - 2 * n,
        }
    }
}

/// 颜色（`0x00RRGGBB`，与 app_kit 配色常量一致）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }
    pub const fn to_u32(self) -> u32 {
        self.0
    }
    /// 按系数 `f`（0..=1.0）变暗 / 变亮（保持色相）。
    pub fn scale(self, f: f32) -> Color {
        Color(scale_brightness(self.0, f))
    }
}

/// 滑块 / 轨道方向。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

/// 触摸阶段。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
}

/// 触摸事件。宿主从 `/dev/input/event0`（BTN_TOUCH / ABS_X / ABS_Y）组装。
#[derive(Clone, Copy)]
pub struct TouchEvent {
    pub phase: TouchPhase,
    pub x: i32,
    pub y: i32,
    /// 与上一事件在主轴的位移（把漏沙速度与滑动速度关联）。Move 才有意义。
    pub dx: i32,
    pub dy: i32,
    /// 事件墙钟（微秒，长按判定用）。
    pub us: u64,
}

impl TouchEvent {
    pub const fn down(x: i32, y: i32, us: u64) -> TouchEvent {
        TouchEvent {
            phase: TouchPhase::Down,
            x,
            y,
            dx: 0,
            dy: 0,
            us,
        }
    }
    pub const fn up(x: i32, y: i32, us: u64) -> TouchEvent {
        TouchEvent {
            phase: TouchPhase::Up,
            x,
            y,
            dx: 0,
            dy: 0,
            us,
        }
    }
    pub const fn move_(x: i32, y: i32, dx: i32, dy: i32, us: u64) -> TouchEvent {
        TouchEvent {
            phase: TouchPhase::Move,
            x,
            y,
            dx,
            dy,
            us,
        }
    }
}

/// 键盘动作（NebulaInput 用；宿主把 app_kit `KeyAction` 映射为它）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Char(u8),
    Backspace,
}

// ==================== 数学（no_std 纯实现，不依赖 libm） ====================

/// 平方根：位技巧初始估计 + 3 次牛顿迭代（与 kunxhal math::sqrt 一致）。
pub fn fsqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    let mut y = f32::from_bits((bits >> 1) + 0x1FC0_0000);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// 取模（结果与除数同号，等价 rem_euclid）。
fn rem_euclid(a: f32, b: f32) -> f32 {
    let m = a - b * ((a / b) as i32 as f32);
    if m < 0.0 {
        m + b
    } else {
        m
    }
}

/// 把任意角归约到 `[-π, π]`。
fn reduce_pi(x: f32) -> f32 {
    let m = rem_euclid(x, 2.0 * PI);
    if m > PI {
        m - 2.0 * PI
    } else {
        m
    }
}

/// 正弦（归约 + 9 阶泰勒，误差 ~1e-8；动画相位足够）。
pub fn fsin(x: f32) -> f32 {
    let r0 = reduce_pi(x);
    let r1 = if r0 > PI / 2.0 {
        PI - r0
    } else if r0 < -PI / 2.0 {
        -PI - r0
    } else {
        r0
    };
    // 再折叠到 [-π/4, π/4]
    if r1 > PI / 4.0 {
        fcos_small(PI / 2.0 - r1)
    } else if r1 < -PI / 4.0 {
        -fcos_small(r1 + PI / 2.0)
    } else {
        let x2 = r1 * r1;
        r1
            * (1.0
                - x2 * (1.0 / 6.0)
                + x2 * x2 * (1.0 / 120.0)
                - x2 * x2 * x2 * (1.0 / 5040.0)
                + x2 * x2 * x2 * x2 * (1.0 / 362880.0))
    }
}

/// 余弦。
pub fn fcos(x: f32) -> f32 {
    fsin(x + PI / 2.0)
}

/// 小角余弦（|x|≤π/4）。
fn fcos_small(x: f32) -> f32 {
    let x2 = x * x;
    1.0
        - x2 * (1.0 / 2.0)
        + x2 * x2 * (1.0 / 24.0)
        - x2 * x2 * x2 * (1.0 / 720.0)
        + x2 * x2 * x2 * x2 * (1.0 / 40320.0)
}

/// 三次缓出（0→1，用于滑入 / 渐显）。
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let m = 1.0 - t;
    1.0 - m * m * m
}

/// 三次缓入缓出（0→1，对称）。
pub fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let m = -2.0 * t + 2.0;
        1.0 - m * m * m / 2.0
    }
}

// ==================== 弹簧（阻尼谐振子，与 launcher 同规范） ====================

/// 阻尼谐振子弹簧（半隐式欧拉积分）。字段布局与
/// apps/launcher/src/app_launch.rs `Spring` 一致，方便与现有系统交换实现。
pub struct Spring {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
    pub target: f32,
    pub pos: f32,
    pub vel: f32,
}

impl Spring {
    /// 固定位（静态构造，用于 no_std 全局 `static mut`）。
    pub const fn fixed() -> Spring {
        Spring {
            stiffness: 0.0,
            damping: 0.0,
            mass: 1.0,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    /// iOS 风格：`damping_ratio` + 初始速度，stiffness=280（与 launcher 一致）。
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
    /// 设定目标（保留速度，过渡顺滑）。
    pub fn set_target(&mut self, t: f32) {
        self.target = t;
    }
    /// 推进 `ms` 毫秒，返回是否已稳定（|pos-target|<0.5 且 |vel|<2）。
    pub fn step_ms(&mut self, ms: f32) -> bool {
        if self.stiffness <= 0.0 {
            self.pos = self.target;
            self.vel = 0.0;
            return true;
        }
        let mut t = ms / 1000.0;
        while t > 0.0 {
            let h = if t > 0.001 { 0.001 } else { t };
            let a =
                (-self.stiffness * (self.pos - self.target) - self.damping * self.vel) / self.mass;
            self.vel += a * h;
            self.pos += self.vel * h;
            t -= h;
        }
        (self.pos - self.target).abs() < 0.5 && self.vel.abs() < 2.0
    }
}

// ==================== 随机数（xorshift32，无标准库） ====================

/// xorshift32 伪随机数生成器（粒子颜色 / 散落方向 / 闪烁相位）。
pub struct XorShift32 {
    pub state: u32,
}

impl XorShift32 {
    /// 固定种子静态构造（混合散列避免全零状态；xorshift 要求 state≠0）。
    pub const fn new(seed: u32) -> XorShift32 {
        let mut s = seed.wrapping_add(0x9E37_79B9);
        s ^= s >> 16;
        s = s.wrapping_mul(0x21F0_AAAD);
        s ^= s >> 15;
        s = s.wrapping_mul(0x735A_2D97);
        s ^= s >> 15;
        XorShift32 {
            state: if s == 0 { 0x9D4EDD } else { s },
        }
    }
    /// 重新播种（宿主可在启动时用 `now_us()` 播种）。
    pub fn reseed(&mut self, seed: u32) {
        *self = XorShift32::new(seed);
    }
    /// 下一个随机 u32。
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }
    /// `[0, n)` 整数。
    #[inline]
    pub fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            self.next_u32() % n
        }
    }
    /// `[0, 1)` f32。
    #[inline]
    pub fn frac(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / 16_777_216.0
    }
    /// `[-1, 1]` f32。
    #[inline]
    pub fn jitter(&mut self) -> f32 {
        self.frac() * 2.0 - 1.0
    }
    /// 从沙色板取一色。
    #[inline]
    pub fn sand_color(&mut self) -> u32 {
        SAND_PALETTE[self.below(SAND_PALETTE.len() as u32) as usize]
    }
}

// ==================== 颜色工具 ====================

#[inline]
pub fn channel_r(c: u32) -> u32 {
    (c >> 16) & 0xFF
}
#[inline]
pub fn channel_g(c: u32) -> u32 {
    (c >> 8) & 0xFF
}
#[inline]
pub fn channel_b(c: u32) -> u32 {
    c & 0xFF
}
#[inline]
pub fn pack_rgb(r: u32, g: u32, b: u32) -> u32 {
    (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

/// 普通 alpha 混合：`fg` 以透明度 `a`（0..=255）盖到 `bg` 上。
#[inline]
pub fn alpha_into(bg: u32, fg: u32, a: u32) -> u32 {
    let a = a.min(255);
    let ia = 255 - a;
    let r = (channel_r(bg) * ia + channel_r(fg) * a) / 255;
    let g = (channel_g(bg) * ia + channel_g(fg) * a) / 255;
    let b = (channel_b(bg) * ia + channel_b(fg) * a) / 255;
    pack_rgb(r, g, b)
}

/// additive 混合：前景以 `a` 叠加到背景（通道相加、饱和裁剪）——发光核心。
#[inline]
pub fn additive(bg: u32, fg: u32, a: u32) -> u32 {
    let a = a.min(255);
    let r = channel_r(bg) + channel_r(fg) * a / 255;
    let g = channel_g(bg) + channel_g(fg) * a / 255;
    let b = channel_b(bg) + channel_b(fg) * a / 255;
    pack_rgb(r, g, b)
}

/// 颜色整体变亮 / 变暗（乘法，保持色相）。
#[inline]
pub fn scale_brightness(c: u32, f: f32) -> u32 {
    let r = (channel_r(c) as f32 * f).min(255.0) as u32;
    let g = (channel_g(c) as f32 * f).min(255.0) as u32;
    let b = (channel_b(c) as f32 * f).min(255.0) as u32;
    pack_rgb(r, g, b)
}

/// 颜色线性插值（t 0..=1）。
#[inline]
pub fn lerp_color(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let it = 1.0 - t;
    let r = (channel_r(a) as f32 * it + channel_r(b) as f32 * t) as u32;
    let g = (channel_g(a) as f32 * it + channel_g(b) as f32 * t) as u32;
    let bl = (channel_b(a) as f32 * it + channel_b(b) as f32 * t) as u32;
    pack_rgb(r, g, bl)
}

/// 宇宙沙色板（星云紫/青/金/粉，任务规格给定的 8 色）。
pub const SAND_PALETTE: [u32; 8] = [
    0x9D4EDD, 0xC77DFF, 0xE0AAFF, 0x7B2CBF, 0x5A189A, 0xFF9E00, 0x00B4D8, 0x90E0EF,
];

/// 光标「宇宙色」渐变端点：深蓝（顶）→ 紫（底）。
pub const CURSOR_TOP: u32 = 0x1B3A8C;
pub const CURSOR_BOTTOM: u32 = 0x7B2CBF;
/// 选区星云紫蓝（半透明用）。
pub const C_SELECT: u32 = 0x4C6EF5;
/// 发光白（星点 / 笔迹高光）。
pub const C_STAR_WHITE: u32 = 0xFFE9FF;

// ==================== 绘制画布 trait ====================

/// 控件渲染目标。宿主（app_kit / 测试）实现本 trait 后交给控件的 `render`。
///
/// 基础能力与草案一致：`draw_pixel`、`draw_line`、`draw_rect`、`draw_circle`、
/// `blend_pixel`；另有 additive 发光的 `add_pixel`、圆角矩形填充 / 描边
/// （所有控件都用圆角容器）。
pub trait Canvas {
    /// 直接写单个像素（0x00RRGGBB）。
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32);
    /// 读取背景像素（混合与毛玻璃输入源）。
    fn backdrop(&self, x: i32, y: i32) -> u32;

    /// 普通 alpha 混合写入。
    #[inline]
    fn blend_pixel(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        let bg = self.backdrop(x, y);
        self.draw_pixel(x, y, alpha_into(bg, color, alpha));
    }
    /// additive 发光混合写入。
    #[inline]
    fn add_pixel(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        let bg = self.backdrop(x, y);
        self.draw_pixel(x, y, additive(bg, color, alpha));
    }

    /// 填充实心矩形。
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for dy in 0..h {
            for dx in 0..w {
                self.draw_pixel(x + dx, y + dy, color);
            }
        }
    }
    /// 空心矩形描边。
    fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        for dx in 0..w {
            self.draw_pixel(x + dx, y, color);
            self.draw_pixel(x + dx, y + h - 1, color);
        }
        for dy in 0..h {
            self.draw_pixel(x, y + dy, color);
            self.draw_pixel(x + w - 1, y + dy, color);
        }
    }
    /// Bresenham 直线。
    fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        let mut x0 = x0;
        let mut y0 = y0;
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.draw_pixel(x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }
    /// 空心圆（抗锯齿 1px 圆环）。
    fn draw_circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        smooth::smooth_stroke_circle(&mut CanvasSmooth(self), cx, cy, r, color);
    }
    /// 实心圆（抗锯齿：圆周环带超采样混合）。
    fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        smooth::smooth_fill_circle(&mut CanvasSmooth(self), cx, cy, r, color);
    }
    /// 填充圆角矩形（抗锯齿超椭圆连续曲率，ColorOS 15 观感）。
    fn fill_rounded(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        smooth::smooth_fill_rr(&mut CanvasSmooth(self), x, y, w, h, r, color);
    }
    /// 1px 圆角描边（抗锯齿：直边锐利、圆角段随曲线淡出）。
    fn rounded_border(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
        smooth::smooth_stroke_rr(&mut CanvasSmooth(self), x, y, w, h, r, color, 255);
    }
}

/// 点是否位于圆角矩形内（像素中心采样，与抗锯齿填充的超椭圆几何一致）。
pub fn rounded_inside(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32, r: i32) -> bool {
    if w <= 0 || h <= 0 {
        return false;
    }
    let r = r.max(0).min(w / 2).min(h / 2) as f32;
    smooth::rr_sub_inside(
        (px - x) as f32 + 0.5,
        (py - y) as f32 + 0.5,
        w as f32,
        h as f32,
        r,
        smooth::rr_k(r),
    )
}

/// 内存帧缓冲画布（宿主参考实现；测试也用它）。
pub struct MemCanvas<'a> {
    pub w: usize,
    pub h: usize,
    pub buf: &'a mut [u32],
}

impl<'a> Canvas for MemCanvas<'a> {
    #[inline]
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && x < self.w as i32 && y < self.h as i32 {
            self.buf[y as usize * self.w + x as usize] = color;
        }
    }
    #[inline]
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < self.w as i32 && y < self.h as i32 {
            self.buf[y as usize * self.w + x as usize]
        } else {
            0
        }
    }
}

/// 把 [`Canvas`] 适配为抗锯齿绘制目标（经 `blend_pixel` 与背景混合）。
pub struct CanvasSmooth<'a, C: Canvas + ?Sized>(pub &'a mut C);

impl<C: Canvas + ?Sized> smooth::SmoothTarget for CanvasSmooth<'_, C> {
    #[inline]
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        self.0.blend_pixel(x, y, color, alpha);
    }
}

// ==================== 发光绘制 ====================

/// 径向渐变 + additive 发光（一切光晕特效的唯一入口，实时计算非预渲染）。
/// `cx/cy` 光心，`radius`（≤16）光半径，`color` 发光色，`intensity` 0..=255 中心强度。
/// 用四象限对称减少梯度计算。
pub fn draw_glow(c: &mut dyn Canvas, cx: f32, cy: f32, radius: i32, color: u32, intensity: u32) {
    let radius = radius.min(16);
    if radius <= 0 {
        return;
    }
    let r2 = radius * radius;
    let x0 = cx as i32;
    let y0 = cy as i32;
    for dy in 0..=radius {
        for dx in 0..=radius {
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue;
            }
            let a = ((r2 - d2) as u32 * intensity.min(255) / r2 as u32).min(255);
            if dx == 0 && dy == 0 {
                c.add_pixel(x0, y0, color, a);
            } else if dx == 0 {
                c.add_pixel(x0, y0 + dy, color, a);
                c.add_pixel(x0, y0 - dy, color, a);
            } else if dy == 0 {
                c.add_pixel(x0 + dx, y0, color, a);
                c.add_pixel(x0 - dx, y0, color, a);
            } else {
                c.add_pixel(x0 + dx, y0 + dy, color, a);
                c.add_pixel(x0 + dx, y0 - dy, color, a);
                c.add_pixel(x0 - dx, y0 + dy, color, a);
                c.add_pixel(x0 - dx, y0 - dy, color, a);
            }
        }
    }
}

/// 竖直渐变条（光标：深蓝→紫），宽 2px。
pub fn draw_gradient_v(c: &mut dyn Canvas, x: i32, y: i32, h: i32, top: u32, bottom: u32) {
    let h = h.max(1);
    for k in 0..h {
        let col = lerp_color(top, bottom, k as f32 / h as f32);
        c.draw_pixel(x, y + k, col);
        c.draw_pixel(x + 1, y + k, col);
    }
}

// ==================== 单元测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(10, 20));
        assert!(r.contains(39, 59));
        assert!(!r.contains(40, 59));
        assert!(!r.contains(9, 20));
    }

    #[test]
    fn blend_and_add() {
        assert_eq!(alpha_into(0x000000, 0xFFFFFF, 255), 0xFFFFFF);
        assert_eq!(alpha_into(0xFFFFFF, 0x000000, 255), 0x000000);
        assert_eq!(alpha_into(0x000000, 0xFF0000, 128), 0x800000);
        assert_eq!(additive(0x000000, 0xFF0000, 128), 0x800000);
        // add 饱和裁剪
        assert_eq!(additive(0xFF0000, 0xFF0000, 255), 0xFF0000);
        assert_eq!(additive(0x008080, 0xFF0000, 64), 0x408080);
    }

    #[test]
    fn palette_pick() {
        let mut rng = XorShift32::new(7);
        for _ in 0..100 {
            let c = rng.sand_color();
            assert!(SAND_PALETTE.contains(&c));
        }
    }

    #[test]
    fn spring_settles() {
        let mut sp = Spring::ios(0.65, 0.0);
        sp.set_target(1.0);
        for _ in 0..120 {
            if sp.step_ms(16.0) {
                break;
            }
        }
        assert!((sp.pos - 1.0).abs() < 0.5, "pos={}", sp.pos);
        assert!(sp.vel.abs() < 2.0);
    }

    #[test]
    fn sin_accurate_enough() {
        let samples = [-PI, -0.3, 0.0, 0.3, PI / 2.0, 2.2, PI];
        for s in samples {
            let got = fsin(s);
            let exp = match s {
                x if (x - -0.3).abs() < 1e-6 => -0.2955202_f32,
                x if (x - 0.0).abs() < 1e-6 => 0.0,
                x if (x - 0.3).abs() < 1e-6 => 0.2955202,
                x if (x - PI / 2.0).abs() < 1e-6 => 1.0,
                x if (x - 2.2).abs() < 1e-6 => 0.8084964,
                _ => 0.0, // -PI 与 PI 都应为 0
            };
            assert!((got - exp).abs() < 1e-4, "sin({})={} exp={}", s, got, exp);
        }
    }

    #[test]
    fn mem_canvas_bounds() {
        let mut buf = [0u32; 16];
        {
            let mut mc = MemCanvas { w: 4, h: 4, buf: &mut buf };
            mc.draw_pixel(1, 1, 0x123456);
            assert_eq!(mc.backdrop(1, 1), 0x123456);
            mc.draw_pixel(5, 5, 0x999999); // 越界：无操作
            assert_eq!(mc.backdrop(5, 5), 0);
            assert_eq!(mc.backdrop(1, 1), 0x123456);
        }
        assert_eq!(buf[0], 0);
        assert_eq!(buf[5], 0x123456);
        assert_eq!(buf[5], 0x123456);
    }

    #[test]
    fn rounded_geometry() {
        let r = Rect::new(10, 10, 20, 20);
        // 圆角矩形中心必在内部
        assert!(rounded_inside(20, 20, r.x, r.y, r.w, r.h, 6));
        // 左上外角不在内部
        assert!(!rounded_inside(10, 10, r.x, r.y, r.w, r.h, 6));
        // 直角（r 很小）内边
        assert!(rounded_inside(12, 10, r.x, r.y, r.w, r.h, 2));
    }

    #[test]
    fn color_tools() {
        assert_eq!(scale_brightness(0x808080, 0.5), 0x404040);
        assert_eq!(lerp_color(0x000000, 0xFFFFFF, 0.5), 0x7F7F7F);
        assert_eq!(lerp_color(0x000000, 0xFFFFFF, 0.0), 0x000000);
        assert_eq!(lerp_color(0x000000, 0xFFFFFF, 1.0), 0xFFFFFF);
    }
}