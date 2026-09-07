//! 星辰开关（StellarToggle）：圆角胶囊容器。
//! - 关闭时内部空、背景暗淡。
//! - 打开时沙子从中心 / 左侧凭空生成，迅速填满整个开关内部，边缘随开启动画泛起柔和光晕。
//! - 关闭动画时沙子从底部 / 右侧漏出，在半空蒸发，光芒渐暗。
//! - 点击切换并触发触觉反馈（由宿主注入 `vibrate` 回调）。
//!
//! 内部沙子复用 [`crate::widgets::ParticleSystem`]，与亮度滑块视觉一致。

use crate::{Canvas, Color, Rect, Spring, TouchEvent, TouchPhase};
use crate::widgets::particle_system::ParticleSystem;

/// 开启动画弹簧阻尼比（填充进度与光芒共用同一弹簧模型）。
const FILL_SPRING_DAMPING: f32 = 0.72;
/// 关闭漏沙的优先方向：右侧列先漏（「从右侧漏出」，自右向左排空）。
const DRAIN_RIGHT_FIRST: bool = true;

/// 开关动画状态。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ToggleAnimState {
    /// 关闭稳态。
    Off,
    /// 开启中：沙子在填充。
    Filling,
    /// 开启稳态：内部充满沙子、边缘发光。
    On,
    /// 关闭中：沙子从底部 / 右侧漏出。
    Draining,
}

/// 星辰开关。
pub struct StellarToggle {
    pub rect: Rect,
    pub is_on: bool,
    /// 当前填充比例 0..=1（由弹簧推进，供外部读取进度）。
    pub sand_fill: f32,
    /// 开启后的光晕主体色。
    pub glow_color: Color,
    /// 当前动画状态。
    pub anim_state: ToggleAnimState,
    /// 内部沙子粒子系统（草案中的 `sand_particles`）。
    pub sand_particles: ParticleSystem,
    /// 宿主注入的触觉反馈回调（audiod vibrate ioctl 等）。为 None 时不反馈。
    pub vibrate: Option<fn()>,

    // —— 内部状态 ——
    fill_spring: Spring,
    glow_spring: Spring,
    initialized: bool,
    /// 动画时钟（星点闪烁相位）。
    anim_t: f32,
}

impl StellarToggle {
    /// 构造（可静态构造，供 no_std `static mut`）。
    pub const fn new(rect: Rect) -> StellarToggle {
        StellarToggle {
            rect,
            is_on: false,
            sand_fill: 0.0,
            glow_color: Color(0x7B2CBF),
            anim_state: ToggleAnimState::Off,
            sand_particles: ParticleSystem::new(0xA11CE),
            vibrate: None,
            fill_spring: Spring::fixed(),
            glow_spring: Spring::fixed(),
            initialized: false,
            anim_t: 0.0,
        }
    }

    /// 切换开 / 关：翻转状态、启动填充弹簧、触发触觉反馈。
    pub fn toggle(&mut self) {
        self.is_on = !self.is_on;
        if self.is_on {
            self.anim_state = ToggleAnimState::Filling;
            self.fill_spring = Spring::ios(FILL_SPRING_DAMPING, 0.0);
            self.fill_spring.set_target(1.0);
        } else {
            self.anim_state = ToggleAnimState::Draining;
            self.fill_spring = Spring::ios(FILL_SPRING_DAMPING, 0.0);
            self.fill_spring.set_target(0.0);
        }
        if let Some(v) = self.vibrate {
            v();
        }
    }

    /// 直接设置开关状态（编程路径，同样走动画）。
    pub fn set_on(&mut self, on: bool) {
        if on != self.is_on {
            self.toggle();
        }
    }

    /// 命中判定（胶囊整体）。
    pub fn hit(&self, x: i32, y: i32) -> bool {
        self.rect.contains(x, y)
    }

    /// 触摸处理：抬起命中的一瞬间切换。
    pub fn handle_touch(&mut self, event: &TouchEvent) {
        if event.phase == TouchPhase::Up && self.hit(event.x, event.y) {
            self.toggle();
        }
    }

    /// 渲染并推进一帧。
    pub fn render(&mut self, canvas: &mut dyn Canvas, dt: f32) {
        self.anim_t += dt;
        // 首次渲染：配置内部网格
        if !self.initialized {
            self.initialized = true;
            self.configure_grid();
            self.sand_particles.reset_pile();
        } else {
            self.configure_grid();
        }

        // 弹簧推进（填充进度 / 光芒强度）
        self.fill_spring.step_ms(dt * 1000.0);
        self.sand_fill = self.fill_spring.pos.clamp(0.0, 1.0);
        self.glow_spring = Spring::ios(0.6, 0.0);
        self.glow_spring.set_target(if self.is_on { 1.0 } else { 0.0 });
        self.glow_spring.step_ms(dt * 1000.0);

        // 状态判定
        self.anim_state = if self.is_on {
            if self.fill_spring.pos > 0.999 && self.fill_spring.vel.abs() < 0.05 {
                ToggleAnimState::On
            } else {
                ToggleAnimState::Filling
            }
        } else if self.fill_spring.pos < 0.001 && self.fill_spring.vel.abs() < 0.05 {
            ToggleAnimState::Off
        } else {
            ToggleAnimState::Draining
        };

        // 目标堆高 = 填充比例 * 行数；漏沙方向为右侧优先
        self.sand_particles.target_rows =
            ((self.sand_fill * self.sand_particles.rows as f32) + 0.5) as u16;
        self.sand_particles.drain_right_first = DRAIN_RIGHT_FIRST;
        // 倒沙嘴：胶囊内部中心偏左（「从中心 / 左侧凭空生成」）
        self.sand_particles.spout_x = (self.rect.x + self.rect.w * 3 / 8) as f32;
        self.sand_particles.spout_y = (self.rect.y + 4) as f32;

        if self.is_on {
            self.sand_particles.fill_needed();
        } else {
            self.sand_particles.drain_excess();
        }
        self.sand_particles.update(dt);

        // —— 绘制 ——
        let r = (self.rect.h / 2).min(self.rect.w / 2).min(24);
        let glow = self.glow_spring.pos;
        let gc = self.glow_color.0;

        // 胶囊底座（开启时泛紫的深空蓝，关闭时暗淡）
        let base = if self.is_on { 0x1A_2E_4A } else { 0x1C_1C_1E };
        canvas.fill_rounded(self.rect.x, self.rect.y, self.rect.w, self.rect.h, r, base);

        // 沙子
        self.sand_particles.render(canvas, true);

        // 边缘光晕（开启时渐入；additive）
        if glow > 0.01 {
            // 外圈余晖
            crate::draw_glow(
                canvas,
                self.rect.cx() as f32,
                self.rect.cy() as f32,
                (self.rect.w / 2 + 6).min(16),
                gc,
                (glow * 90.0) as u32,
            );
            // 内缘描边：沿胶囊轮廓淡紫发光
            let edge = crate::lerp_color(base, gc, glow * 0.55);
            canvas.rounded_border(self.rect.x, self.rect.y, self.rect.w, self.rect.h, r, edge);
        } else {
            canvas.rounded_border(self.rect.x, self.rect.y, self.rect.w, self.rect.h, r, 0x38_38_3A);
        }

        // 填充 / 漏沙转折时的星点氛围
        if self.anim_state == ToggleAnimState::Filling
            || self.anim_state == ToggleAnimState::Draining
        {
            for _ in 0..2 {
                let cx = self.rect.cx() as f32 + self.sand_particles.rng.jitter() * 20.0;
                let cy = self.rect.cy() as f32 + self.sand_particles.rng.jitter() * 10.0;
                self.sand_particles.spark(cx, cy, 0xFFE9FF);
            }
        }
    }

    /// 配置内部网格：格宽 3px（粒数受 MAX_PARTICLES 隐式约束）。
    fn configure_grid(&mut self) {
        let inner = self.rect.inset(1);
        self.sand_particles
            .setup_grid(inner.x, inner.y, inner.w, inner.h, 3.0, false);
    }
}

// ==================== 单元测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemCanvas;

    #[test]
    fn toggle_flips_and_vibrates() {
        use core::sync::atomic::{AtomicUsize, Ordering};
        static VIBES: AtomicUsize = AtomicUsize::new(0);
        VIBES.store(0, Ordering::Relaxed);
        let mut t = StellarToggle::new(Rect::new(200, 300, 56, 32));
        t.vibrate = Some(|| {
            VIBES.fetch_add(1, Ordering::Relaxed);
        });
        t.handle_touch(&TouchEvent::up(228, 316, 1_000));
        assert!(t.is_on);
        assert_eq!(VIBES.load(Ordering::Relaxed), 1);
        t.handle_touch(&TouchEvent::up(200, 300, 2_000));
        assert!(!t.is_on);
        assert_eq!(VIBES.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn fills_and_drains() {
        let mut buf = [0u32; 540 * 120];
        let mut t = StellarToggle::new(Rect::new(240, 40, 60, 32));
        let mut canvas = MemCanvas {
            w: 540,
            h: 120,
            buf: &mut buf,
        };
        t.set_on(true);
        for _ in 0..300 {
            t.render(&mut canvas, 0.016);
        }
        assert_eq!(t.anim_state, ToggleAnimState::On);
        assert_eq!(t.sand_particles.excess(), 0);
        let on_particles = t.sand_particles.alive();
        assert!(on_particles > 60, "on_particles={on_particles}");
        t.set_on(false);
        for _ in 0..400 {
            t.render(&mut canvas, 0.016);
        }
        assert_eq!(t.anim_state, ToggleAnimState::Off);
        assert_eq!(t.sand_particles.excess(), 0);
        for c in 0..t.sand_particles.cols {
            assert_eq!(t.sand_particles.top[c], 0);
        }
    }
}