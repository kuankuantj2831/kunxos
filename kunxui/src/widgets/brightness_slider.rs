//! 亮度调节滑块「流沙沙漏」：往右 / 上滑动时从 thumb 上方凭空生成七彩沙粒，
//! 沙粒受重力落入轨槽并堆叠；往左 / 下滑动时沙堆从底部漏沙，沙粒在半空蒸发。
//! thumb 是一颗发光的微型星辰，拖拽时光晕增强。

use crate::{Canvas, Orientation, Rect, Spring, TouchEvent, TouchPhase, XorShift32};
use crate::widgets::particle_system::ParticleSystem;

/// 轨道沙漏容器厚度（水平：容器高；竖直：容器宽）。
const TRACK_H: i32 = 22;
/// 目标粒子预算（轨槽满时 ≤ 480，留 20 给在途/漏沙粒子）。
const GRAIN_BUDGET: usize = 460;

/// 亮度滑块。`render` 就地推进粒子与弹簧，因此为 `&mut self`。
pub struct BrightnessSlider {
    pub rect: Rect,
    /// 亮度 0.0..=1.0。
    pub value: f32,
    pub orientation: Orientation,
    /// 沙子粒子系统（草案中的 `sand_particles`，改为定长池）。
    pub sand_particles: ParticleSystem,
    /// 拖拽 / 发沙时的光晕强度 0..=1（弹簧驱动）。
    pub glow_intensity: f32,

    /// 轨道容器厚度（px）。
    pub track_h: i32,

    // —— 内部状态 ——
    initialized: bool,
    dragging: bool,
    /// 拇指（弹簧平滑）位置（px）。
    thumb_x: i32,
    thumb_y: i32,
    thumb_frac: f32,
    /// 漏沙加速量：本帧滑动主轴位移（px），再按比例折算为 extra 漏沙。
    drag_delta: i32,
    thumb_spring: Spring,
    glow_spring: Spring,
    rng: XorShift32,
    /// 动画时钟（秒）：星点闪烁相位用。
    anim_t: f32,
}

impl BrightnessSlider {
    /// 以固定种子静态构造（可放入 no_std `static mut`）。
    pub const fn new(rect: Rect, orientation: Orientation) -> BrightnessSlider {
        BrightnessSlider {
            rect,
            value: 0.5,
            orientation,
            sand_particles: ParticleSystem::new(0x5A17_9D4E),
            glow_intensity: 0.0,
            track_h: TRACK_H,
            initialized: false,
            dragging: false,
            thumb_x: 0,
            thumb_y: 0,
            thumb_frac: 0.5,
            drag_delta: 0,
            thumb_spring: Spring::fixed(),
            glow_spring: Spring::fixed(),
            rng: XorShift32::new(0x33A1),
            anim_t: 0.0,
        }
    }

    /// 轨道容器矩形（沙漏槽）。
    pub fn trough_rect(&self) -> Rect {
        match self.orientation {
            Orientation::Horizontal => Rect::new(
                self.rect.x,
                self.rect.cy() - self.track_h / 2,
                self.rect.w,
                self.track_h,
            ),
            Orientation::Vertical => Rect::new(
                self.rect.cx() - self.track_h / 2,
                self.rect.y,
                self.track_h,
                self.rect.h,
            ),
        }
    }

    /// 由触点位置计算 0..1 值。
    pub fn value_from_pos(&self, x: i32, y: i32) -> f32 {
        match self.orientation {
            Orientation::Horizontal => {
                ((x - self.rect.x) as f32 / (self.rect.w as f32 - 1.0)).clamp(0.0, 1.0)
            }
            Orientation::Vertical => {
                1.0 - ((y - self.rect.y) as f32 / (self.rect.h as f32 - 1.0)).clamp(0.0, 1.0)
            }
        }
    }

    /// 设置亮度。降低亮度时按「滑差 × 轨槽体积」生成漏沙预算
    /// （漏沙速度 ↔ 滑动速度：滑得越快同帧漏得越多）。
    pub fn set_value(&mut self, value: f32) {
        let v = value.clamp(0.0, 1.0);
        let delta = self.value - v;
        self.value = v;
        self.thumb_frac = v;
        if delta > 0.0 {
            // 体积换算：要漏掉的颗数 ≈ 滑差 * 列数 * 行数（轨槽全满时即全部存量）
            let grains = (delta
                * self.sand_particles.cols as f32
                * self.sand_particles.rows as f32)
                .max(1.0) as u32;
            self.sand_particles.add_drain(grains);
        } else if delta < 0.0 {
            self.sand_particles.fill_needed();
        }
    }

    /// 命中判定（轨道 + 拇指附近，扩大触控范围）。
    pub fn hit(&self, x: i32, y: i32) -> bool {
        let r = self.trough_rect();
        let pad = 16;
        x >= r.x - pad && x < r.right() + pad && y >= r.y - pad && y < r.bottom() + pad
    }

    /// 触摸处理：按下进入拖拽，移动跟手并更新值 / 漏沙预算，抬起松手。
    pub fn handle_touch(&mut self, event: &TouchEvent) {
        match event.phase {
            TouchPhase::Down => {
                if self.hit(event.x, event.y) {
                    self.dragging = true;
                    self.drag_delta = 0;
                    self.thumb_spring = Spring::ios(0.85, 0.0);
                    self.glow_spring = Spring::ios(0.55, 0.0);
                    self.glow_spring.set_target(1.0);
                    self.set_value(self.value_from_pos(event.x, event.y));
                }
            }
            TouchPhase::Move => {
                if self.dragging {
                    self.drag_delta = match self.orientation {
                        Orientation::Horizontal => event.dx,
                        Orientation::Vertical => -event.dy,
                    };
                    let before = self.value;
                    self.set_value(self.value_from_pos(event.x, event.y));
                    // 实际变化量作为额外漏沙（保持速度联动）
                    if self.value < before {
                        let dec = (before - self.value) * 100.0;
                        self.sand_particles.add_drain(dec as u32 + 1);
                    }
                    self.drag_delta = 0;
                }
            }
            TouchPhase::Up => {
                if self.dragging {
                    self.dragging = false;
                    self.glow_spring.set_target(0.12);
                }
            }
        }
    }

    /// 渲染并推进一帧。粒子、弹簧、闪光全部实时计算。
    pub fn render(&mut self, canvas: &mut dyn Canvas, dt: f32) {
        self.anim_t += dt;
        // 首次渲染：配置网格 + 清空堆
        if !self.initialized {
            self.initialized = true;
            self.configure_grid();
            self.sand_particles.reset_pile();
        } else {
            self.configure_grid();
        }

        // 弹簧推进
        self.glow_spring.step_ms(dt * 1000.0);
        self.glow_intensity = self.glow_spring.pos.clamp(0.0, 1.0);
        self.thumb_spring.set_target(self.thumb_frac);
        self.thumb_spring.step_ms(dt * 1000.0);

        // 拇指屏幕位置
        match self.orientation {
            Orientation::Horizontal => {
                self.thumb_x = self.rect.x
                    + (self.thumb_spring.pos * (self.rect.w - 1) as f32) as i32;
                self.thumb_y = self.rect.cy();
            }
            Orientation::Vertical => {
                self.thumb_y = self.rect.bottom() - 1
                    - (self.thumb_spring.pos * (self.rect.h - 1) as f32) as i32;
                self.thumb_x = self.rect.cx();
            }
        }

        // 目标堆高 = 亮度 * 轨槽行数（+0.5 四舍五入，避免弹簧尾段 0.999 截断少一行）
        self.sand_particles.target_rows =
            ((self.value * self.sand_particles.rows as f32) + 0.5) as u16;
        // 倒沙嘴：thumb 上方（凭空生成的位置）
        match self.orientation {
            Orientation::Horizontal => {
                self.sand_particles.spout_x = self.thumb_x as f32;
                self.sand_particles.spout_y = (self.trough_rect().y - 5) as f32;
            }
            Orientation::Vertical => {
                self.sand_particles.spout_x = self.rect.cx() as f32;
                self.sand_particles.spout_y = (self.thumb_y - 5) as f32;
            }
        }
        // 体积结算：超量 → 漏沙；缺口 → 倒沙（值上升 / 下降由此驱动成型与排空）
        if self.sand_particles.excess() > 0 {
            self.sand_particles.drain_excess();
        } else {
            self.sand_particles.fill_needed();
        }

        // 粒子推进
        self.sand_particles.update(dt);

        // —— 绘制 ——
        let tr = self.trough_rect();
        let rr = (self.track_h / 2).min(12);

        // 轨道：暗色沙漏容器（深空底座 + 微光描边）
        let base = 0x0F_17_2A;
        canvas.fill_rounded(tr.x, tr.y, tr.w, tr.h, rr, base);
        canvas.rounded_border(tr.x, tr.y, tr.w, tr.h, rr, 0x9A_A7_B5);

        // 沙粒
        self.sand_particles.render(canvas, true);

        // 沙堆之上的星云余晖（additive，强度随拖拽）
        let glow = (self.glow_intensity * 70.0) as u32;
        if glow > 8 {
            crate::draw_glow(
                canvas,
                tr.cx() as f32,
                (self.trough_rect().cy()) as f32,
                (tr.w / 2).min(16),
                0x7B2CBF,
                glow,
            );
        }

        // 倒沙嘴光点（正在生沙时）
        if self.sand_particles.pour_pending > 0 || self.dragging {
            let a = (80.0 + self.glow_intensity * 120.0) as u32;
            crate::draw_glow(
                canvas,
                self.sand_particles.spout_x,
                self.sand_particles.spout_y,
                6,
                0xC77DFF,
                a,
            );
            if self.rng.frac() < 0.4 {
                self.sand_particles
                    .spark(self.sand_particles.spout_x, self.sand_particles.spout_y, 0xFFE9FF);
            }
        }

        // thumb：微型发光星辰（径向光晕 + 四芒星光 + 亮核）
        let twinkle = 0.5 + 0.5 * crate::fsin(self.anim_t * 4.6);
        let halo = (180.0 + twinkle * 75.0 * (0.5 + 0.5 * self.glow_intensity)) as u32;
        crate::draw_glow(
            canvas,
            self.thumb_x as f32,
            self.thumb_y as f32,
            12,
            0xE0AAFF,
            halo,
        );
        crate::draw_glow(
            canvas,
            self.thumb_x as f32,
            self.thumb_y as f32,
            6,
            0xFFE9FF,
            (120.0 + twinkle * 60.0) as u32,
        );
        // 四芒星光
        canvas.draw_line(self.thumb_x - 10, self.thumb_y, self.thumb_x + 10, self.thumb_y, 0xFFE9FF);
        canvas.draw_line(self.thumb_x, self.thumb_y - 10, self.thumb_x, self.thumb_y + 10, 0xFFE9FF);
        canvas.fill_circle(self.thumb_x, self.thumb_y, 4, 0xC77DFF);
        canvas.fill_circle(self.thumb_x, self.thumb_y, 2, 0xFFE9FF);
    }

    /// 依据轨道尺寸选择格宽，使静态粒数 ≤ GRAIN_BUDGET。
    fn configure_grid(&mut self) {
        let t = self.trough_rect();
        let (ax, ay) = match self.orientation {
            Orientation::Horizontal => (t.w, t.h),
            Orientation::Vertical => (t.h, t.w),
        };
        // 先按主要轴选取格宽，保证粒数预算
        let mut cell = 4.0f32;
        loop {
            let cols = ((ax as f32 / cell) as usize).max(1);
            let rows = ((ay as f32 / cell) as usize).max(1);
            if cols * rows <= GRAIN_BUDGET || cell >= 6.0 {
                break;
            }
            cell += 1.0;
        }
        self.sand_particles.setup_grid(
            t.x,
            t.y,
            t.w,
            t.h,
            cell,
            self.orientation == Orientation::Vertical,
        );
    }
}

// ==================== 单元测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemCanvas;

    #[test]
    fn value_from_pos_maps() {
        let s = BrightnessSlider::new(Rect::new(100, 400, 200, 24), Orientation::Horizontal);
        assert!((s.value_from_pos(100, 410) - 0.0).abs() < 1e-3);
        assert!((s.value_from_pos(299, 410) - 1.0).abs() < 1e-3);
        assert!((s.value_from_pos(200, 410) - 0.5).abs() < 0.02);
        let v = BrightnessSlider::new(Rect::new(100, 100, 20, 300), Orientation::Vertical);
        // 上滑加大亮度：y 越小 → value 越大
        assert!((v.value_from_pos(110, 399) - 0.0).abs() < 1e-3);
        assert!((v.value_from_pos(110, 100) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn set_value_clamps_and_drains() {
        let mut s = BrightnessSlider::new(Rect::new(100, 400, 200, 24), Orientation::Horizontal);
        s.initialized = true;
        s.sand_particles.setup_grid(100, 389, 200, 22, 4.0, false);
        s.sand_particles.target_rows = 5;
        s.set_value(1.0);
        assert_eq!(s.value, 1.0);
        s.set_value(2.0);
        assert_eq!(s.value, 1.0);
        // 降低亮度 → 产生漏沙预算
        let p0 = s.sand_particles.drain_pending;
        s.set_value(0.2);
        assert!(s.sand_particles.drain_pending > p0);
    }

    #[test]
    fn touch_drag_updates_value() {
        let mut s = BrightnessSlider::new(Rect::new(150, 400, 240, 26), Orientation::Horizontal);
        s.handle_touch(&TouchEvent::down(150, 413, 0));
        assert!(s.dragging);
        s.handle_touch(&TouchEvent::move_(390, 411, 240, 0, 16_000));
        assert!((s.value - 1.0).abs() < 1e-3);
        s.handle_touch(&TouchEvent::up(390, 411, 32_000));
        assert!(!s.dragging);
    }

    #[test]
    fn render_fills_and_empty() {
        let mut buf = [0u32; 540 * 200];
        let mut s = BrightnessSlider::new(Rect::new(150, 60, 240, 26), Orientation::Horizontal);
        s.set_value(1.0);
        let a1;
        let cnt1;
        {
            let mut canvas = MemCanvas { w: 540, h: 200, buf: &mut buf };
            for _ in 0..120 {
                s.render(&mut canvas, 0.016);
            }
            a1 = s.sand_particles.alive();
            cnt1 = buf.iter().filter(|px| **px != 0).count();
        }
        // 满亮度：应当沉积出大量沙粒
        assert!(a1 > 100, "a1={a1}");
        assert!(cnt1 > 1000, "cnt1={cnt1}");
        // 清空 buffer，降到 0 应该漏光
        for px in buf.iter_mut() {
            *px = 0;
        }
        s.set_value(0.0);
        let cnt2;
        {
            let mut canvas = MemCanvas { w: 540, h: 200, buf: &mut buf };
            for _ in 0..500 {
                s.render(&mut canvas, 0.016);
            }
            cnt2 = buf.iter().filter(|px| **px != 0).count();
        }
        // 沙已漏光：轨道底座 / 描边 / thumb 光晕仍占部分像素（不参与沙子判据），
        // 真正的判据是堆顶清零 + 全部颗粒蒸发殆尽。
        assert_eq!(s.sand_particles.excess(), 0);
        assert_eq!(s.sand_particles.alive(), 0, "漏沙应全部蒸发");
        for c in 0..s.sand_particles.cols {
            assert_eq!(s.sand_particles.top[c], 0, "col {c} 未漏净");
        }
        // 顺带确认容器底 + thumb 仍然可见（保证控件本体没有被清空到看不见）
        assert!(cnt2 >= 3000, "cnt2={cnt2} 连容器底座都不见了");
    }
}