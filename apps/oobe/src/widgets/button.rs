// KunxOS OOBE 统一液态玻璃圆角按钮组件（no_std、无堆）。
//
// OOBE 所有按钮一律通过本组件绘制与命中，禁止在各步骤里硬编码按钮样式：
//   - 主操作（下一步 / 完成）：浅色磨砂液态玻璃、近黑文字
//   - 次要（跳过 / 返回）：白色玻璃、近黑文字、浅灰描边
//   - 禁止：整体 40% 透明度，不可点击
// 圆角半径 = min(高度 * 0.30, 16px)（ColorOS 15 连续曲率超椭圆）；高度固定 50px；
// 宽度 = min(内容宽度 + 48px, 屏幕宽度 * 0.85)；无阴影。
// 按压反馈：按下着色变暗 15%，松开后 150ms 过渡回弹（无墙钟，按帧计数）。

/// 按钮固定高度。
pub const BTN_H: i32 = 50;
/// 圆角半径上限。
pub const BTN_RADIUS_MAX: i32 = 16;
/// 宽度内边距（内容宽度两侧各 24px）。
pub const BTN_PAD_W: i32 = 48;
/// 宽度上限占屏幕宽度的百分比（85%）。
pub const BTN_MAX_W_PERCENT: i32 = 85;
/// 主按钮最小宽度（保证底部 CTA 醒目，ColorOS 15 风格）。
pub const BTN_MIN_W: i32 = 176;
/// 按压变暗系数（0.85 * 256）。
pub const BTN_PRESS_DARKEN: u32 = 218;
/// 禁止态透明度（0.4 * 255）。
pub const BTN_DISABLED_ALPHA: u32 = 102;
/// 松开回弹帧数（≈150ms @ 60fps / 16ms 每帧）。
pub const BTN_RELEASE_FRAMES: i32 = 9;

/// 按钮文字色（白底/浅玻璃上的近黑主文字）。
const C_PRIMARY: u32 = 0x00_1A_1C_22;
/// 白色。
const C_WHITE: u32 = 0x00_FF_FF_FF;
/// 主按钮按压反馈色（浅色主按钮按压时向深色过渡 = 轻微压暗）。
const C_PRIMARY_PRESS: u32 = 0x00_2C_2D_33;

// 复用 crate 级共享毛玻璃（sys_nav::glass）与 OOBE 主模块定义的玻璃样式。
use crate::sys_nav::glass::{GlassStyle, GlassTarget, glass_rect_style};
use crate::{STYLE_PRIMARY, STYLE_SECONDARY};

/// 按钮状态。
#[derive(Clone, Copy, PartialEq)]
pub enum ButtonState {
    /// 主操作：浅色磨砂玻璃、近黑文字。
    Primary,
    /// 次要：白色玻璃、近黑文字、浅灰描边。
    Secondary,
    /// 禁止：40% 透明度，不可点击。
    Disabled,
}

/// 按钮绘制画布：调用方提供像素写入、圆角填充与文本绘制。
pub trait ButtonCanvas {
    fn put(&mut self, x: i32, y: i32, color: u32);
    fn fill_rounded(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, color: u32);
    fn text_width(&self, s: &[u8]) -> i32;
    fn draw_text(&mut self, x: i32, y: i32, s: &[u8], color: u32);
    /// 背景像素（玻璃按钮的模糊输入源）；未实现时返回 0（仅着色、不模糊）。
    fn backdrop(&self, _x: i32, _y: i32) -> u32 {
        0
    }
}

/// 把 `dyn ButtonCanvas` 适配为毛玻璃目标（避免与 nav 的 blanket impl 冲突）。
struct CanvasGlass<'a>(&'a mut dyn ButtonCanvas);

impl<'a> GlassTarget for CanvasGlass<'a> {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        self.0.backdrop(x, y)
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        self.0.put(x, y, color)
    }
}

/// 圆角按钮。`draw()` 后 `x/y/w/h` 即为命中区。
pub struct Button {
    pub label: &'static [u8],
    pub state: ButtonState,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pressed: bool,
    release_ticks: i32,
}

impl Button {
    pub const fn new(label: &'static [u8], state: ButtonState) -> Self {
        Self {
            label,
            state,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            pressed: false,
            release_ticks: 0,
        }
    }

    /// 是否可点击（禁止态不可点击）。
    pub fn enabled(&self) -> bool {
        self.state != ButtonState::Disabled
    }

    /// 按钮宽度：min(内容宽度 + 48, 屏幕宽度 * 0.85)；主按钮有最小宽度下限，
    /// 保证「下一步」等主操作在底部足够醒目（ColorOS 15 CTA 尺寸）。
    pub fn width(&self, c: &dyn ButtonCanvas, screen_w: i32) -> i32 {
        let mut w = c.text_width(self.label) + BTN_PAD_W;
        if self.state == ButtonState::Primary {
            w = w.max(BTN_MIN_W);
        }
        w.min(screen_w * BTN_MAX_W_PERCENT / 100)
    }

    /// 通栏按钮：占满（屏幕宽度 - 2×左右留白），欢迎页「开始使用」大 CTA 用。
    ///
    /// 注意：必须走 `draw_at`（固定宽度）。此前误调 `draw`——它的第 4 参是
    /// `screen_w`，会按 `width()` 重新计算宽度，把通栏覆盖成 `min(文字宽+48,
    /// 最小宽 176)` 的窄按钮，命中区随之缩水，欢迎页右侧点击全部落空。
    pub fn draw_full(&mut self, c: &mut dyn ButtonCanvas, margin: i32, y: i32, screen_w: i32) {
        let x = margin;
        let w = screen_w - 2 * margin;
        self.draw_at(c, x, y, w);
    }

    /// 命中测试（基于上次 draw 的矩形）。
    pub fn hit(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// 是否正处于按下态（松开前持续显示变暗反馈）。
    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// 按下：立即进入变暗态。
    pub fn press(&mut self) {
        self.pressed = true;
        self.release_ticks = 0;
    }

    /// 松开：开始 150ms 回弹过渡。
    pub fn release(&mut self) {
        self.pressed = false;
        self.release_ticks = BTN_RELEASE_FRAMES;
    }

    /// 立即复位（不触发过渡），切步骤时调用。
    pub fn reset(&mut self) {
        self.pressed = false;
        self.release_ticks = 0;
    }

    /// 每帧推进一次回弹过渡。
    pub fn tick(&mut self) {
        if self.release_ticks > 0 {
            self.release_ticks -= 1;
        }
    }

    /// 是否正处于松开回弹过渡中。
    pub fn animating(&self) -> bool {
        self.release_ticks > 0
    }

    /// 按当前状态绘制按钮，并记录命中区（宽度自适应，见 [`Self::width`]）。
    pub fn draw(&mut self, c: &mut dyn ButtonCanvas, x: i32, y: i32, screen_w: i32) {
        let w = self.width(c, screen_w);
        self.draw_at(c, x, y, w);
    }

    /// 以给定宽度绘制（内部实现：按压缩放 + ColorOS 15 提亮/回弹）。
    fn draw_at(&mut self, c: &mut dyn ButtonCanvas, x: i32, y: i32, w: i32) {
        let h = BTN_H;
        let r = (h * 30 / 100).min(BTN_RADIUS_MAX);
        // 命中区保留未缩放尺寸（点击判定不受缩放影响）。
        self.x = x;
        self.y = y;
        self.w = w;
        self.h = h;

        // 按压反馈：按压缩放 0.96，松开后 150ms 平滑回弹（对标ColorOS 15 CTA）。
        let press_p = self.press_progress(); // 0=未按 1=全按
        let sw = (w as f32 * (1.0 - press_p * 0.04)) as i32;
        let sh = (h as f32 * (1.0 - press_p * 0.04)) as i32;
        let dx = (w - sw) / 2;
        let dy = (h - sh) / 2;
        let rx = (r as f32 * (1.0 - press_p * 0.04)) as i32;
        let gx = x + dx;
        let gy = y + dy;
        let dark = self.darken_factor();
        let tx = gx + (sw - c.text_width(self.label)) / 2;
        let ty = gy + (sh - 17) / 2;
        match self.state {
            ButtonState::Primary => {
                // 主按钮：浅色磨砂玻璃 + 近黑文字（按压时轻微压暗，回落线性回弹）。
                let st = if press_p > 0.0 {
                    style_mix(&STYLE_PRIMARY, C_PRIMARY_PRESS as u32, press_p)
                } else {
                    style_darken(&STYLE_PRIMARY, dark)
                };
                let mut g = CanvasGlass(&mut *c);
                glass_rect_style(&mut g, gx, gy, sw, sh, rx, &st, true);
                c.draw_text(tx, ty, self.label, C_PRIMARY);
            }
            ButtonState::Secondary => {
                // ColorOS 15 次要芯片：白色玻璃 + 中性灰描边（按压变暗 + 缩放）。
                let st = style_darken(&STYLE_SECONDARY, dark);
                let mut g = CanvasGlass(&mut *c);
                glass_rect_style(&mut g, gx, gy, sw, sh, rx, &st, true);
                let col = darken(C_PRIMARY, dark);
                c.draw_text(tx, ty, self.label, col);
            }
            ButtonState::Disabled => {
                // 半透明灰色玻璃（40% 禁止态）。
                let st = GlassStyle {
                    tint: 0x00_B6_BE_C9,
                    alpha: BTN_DISABLED_ALPHA + 60,
                    spec: 0,
                    sheen: 0,
                    border: 0x00_9A_A7_B5,
                    border_a: 60,
                };
                let mut g = CanvasGlass(&mut *c);
                glass_rect_style(&mut g, gx, gy, sw, sh, rx, &st, true);
                c.draw_text(tx, ty, self.label, C_WHITE);
            }
        }
    }

    /// 按压进度：按下=1，松开回弹期间线性回落到 0。
    fn press_progress(&self) -> f32 {
        if self.pressed {
            1.0
        } else if self.release_ticks > 0 {
            (BTN_RELEASE_FRAMES - self.release_ticks) as f32 / BTN_RELEASE_FRAMES as f32
        } else {
            0.0
        }
    }

    /// 当前明暗系数：正常 256，按下 218（变暗 15%），松开后线性回弹。
    fn darken_factor(&self) -> u32 {
        if self.pressed {
            BTN_PRESS_DARKEN
        } else if self.release_ticks > 0 {
            BTN_PRESS_DARKEN
                + (256 - BTN_PRESS_DARKEN) * (BTN_RELEASE_FRAMES - self.release_ticks) as u32
                    / BTN_RELEASE_FRAMES as u32
        } else {
            256
        }
    }
}

/// 按系数 `f`（0..256）变暗颜色（0x00RRGGBB）。
fn darken(c: u32, f: u32) -> u32 {
    let r = (((c >> 16) & 0xFF) * f) / 256;
    let g = (((c >> 8) & 0xFF) * f) / 256;
    let b = ((c & 0xFF) * f) / 256;
    (r << 16) | (g << 8) | b
}

/// 按系数 `f`（0..256）变暗整套玻璃样式（按压反馈）。
fn style_darken(st: &GlassStyle, f: u32) -> GlassStyle {
    let mut s = *st;
    s.tint = darken(s.tint, f);
    s.border = darken(s.border, f);
    s
}

/// 颜色线性插值：a*(255-t)+b*t（t ∈ 0..=255）。
fn lerp_color(a: u32, b: u32, t: u32) -> u32 {
    let inv = 255 - t.min(255);
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * t.min(255)) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * t.min(255)) / 255;
    let bl = ((a & 0xFF) * inv + ((b & 0xFF) * t.min(255))) / 255;
    (r << 16) | (g << 8) | bl
}

/// 把整套玻璃样式的着色与描边向 `tint` 插值（主按钮按压压暗反馈用）。
fn style_mix(st: &GlassStyle, tint: u32, t: f32) -> GlassStyle {
    let mut s = *st;
    let f = (t.clamp(0.0, 1.0) * 255.0) as u32;
    if f > 0 {
        s.tint = lerp_color(s.tint, tint, f);
        s.border = lerp_color(s.border, tint, f);
    }
    s
}

/// 把颜色按透明度 `a`（0..255）淡出到白色背景（浅色主题 40% 禁止态）。
fn fade_white(c: u32, a: u32) -> u32 {
    let ia = 255 - a;
    let r = (((c >> 16) & 0xFF) * a + 255 * ia) / 255;
    let g = (((c >> 8) & 0xFF) * a + 255 * ia) / 255;
    let b = ((c & 0xFF) * a + 255 * ia) / 255;
    (r << 16) | (g << 8) | b
}
