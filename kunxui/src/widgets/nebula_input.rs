//! 星尘输入框（NebulaInput）。
//!
//! - 光标：2px 竖杠，「宇宙色」深蓝→紫垂直渐变，约 1Hz 呼吸（亮度呼吸，非显隐）。
//! - 书写动画：每个新字符由一支「看不见的笔」写出——从透明渐显、从左侧轻微滑入，
//!   带星光笔迹光晕；已输入字符带微弱的沙粒质感 / 星光边缘。
//! - 退格：被删字符像沙子一样散落成粒子，在半空渐隐消失。
//! - 长按选择：选区为星云紫蓝高亮条。

use crate::{
    lerp_color, Canvas, KeyAction, Rect, TouchEvent, TouchPhase, XorShift32, C_SELECT,
    CURSOR_BOTTOM, CURSOR_TOP,
};
use crate::widgets::particle_system::ParticleSystem;
use crate::fsin;

/// 文本最大字节数（UTF-8，含 CJK；超出忽略）。
const MAX_TEXT: usize = 64;
/// 书写动画队列长度（快速连续输入时排队）。
const MAX_ANIMS: usize = 16;
/// 光栅化暂存缓冲（32x32 字形 = 1024 字节）。
const MAX_GLYPH_BYTES: usize = 1024;
/// 单字书写动画时长（秒）。
const CHAR_ANIM_DUR: f32 = 0.18;
/// 长按判定阈值（微秒）。
const LONG_PRESS_US: u64 = 450_000;
/// 文本左右留白。
const TEXT_PAD: i32 = 12;

/// 字形光栅度量。
pub struct GlyphRaster {
    pub width: i32,
    pub height: i32,
    /// 单字推进宽度（屏幕 x 增量）。
    pub advance: i32,
    /// 位图水平偏移。
    pub xmin: i32,
}

/// 字体提供者：宿主把 ttf_text / 位图字体适配为它。
pub trait Font {
    /// 光栅化 `cp` 到 `out`（行主序 alpha 覆盖，直方），返回度量。
    /// `out` 需能容纳 width*height 字节；放不下时返回 None。
    fn rasterize(&self, cp: u32, px: f32, out: &mut [u8]) -> Option<GlyphRaster>;
    /// 推进宽度（测量用；宿主可用 ttf 字体的 advance 度量，不必真的栅格化）。
    fn advance(&self, cp: u32, px: f32) -> i32;
}

/// 书写动画条目（每个正在「被写出」的字符一个）。
#[derive(Clone, Copy)]
pub struct TextCharAnim {
    /// 对应字符在 `text` 中的字节偏移。
    pub char_idx: usize,
    /// 动画已推进时间（秒）。
    pub t: f32,
    /// 持续时长（秒）。
    pub dur: f32,
    /// 队列槽是否占用。
    pub alive: bool,
}

impl TextCharAnim {
    const fn dead() -> TextCharAnim {
        TextCharAnim {
            char_idx: 0,
            t: 0.0,
            dur: 1.0,
            alive: false,
        }
    }
    /// 书写进度（经过 ease-out）。
    fn progress(&self) -> f32 {
        crate::ease_out_cubic((self.t / self.dur).clamp(0.0, 1.0))
    }
}

/// 星尘输入框。`render` 就地推进动画与粒子，因此为 `&mut self`。
pub struct NebulaInput {
    pub rect: Rect,
    pub focused: bool,
    /// UTF-8 字节缓冲（草案的 `String`，改为定长缓冲）。
    pub text: [u8; MAX_TEXT],
    pub text_len: usize,
    /// 光标位置（字节偏移）。
    pub cursor_pos: usize,
    /// 光标是否可见（有焦点才显示）。
    pub cursor_visible: bool,
    /// 光标时钟（秒）；驱动 1Hz 呼吸。
    pub cursor_blink_timer: f32,
    /// 书写动画队列（草案的 `Vec<TextCharAnim>`，改为定长循环槽）。
    pub text_anim_queue: [TextCharAnim; MAX_ANIMS],
    /// 字体（宿主注入；None 时按等宽退避测量）。
    pub font: Option<&'static dyn Font>,
    pub font_px: f32,

    /// 长按选择的起始锚点（字节）。
    pub sel_anchor: Option<usize>,
    /// 是否存在选区。
    pub sel_active: bool,
    pub sel_start: usize,
    pub sel_end: usize,

    /// 沙子粒子系统（退格散落 / 星点复用）。
    pub ps: ParticleSystem,

    // —— 内部 ——
    glyph_buf: [u8; MAX_GLYPH_BYTES],
    rng: XorShift32,
    anim_t: f32,
    down_armed: bool,
    down_x: i32,
    down_y: i32,
    down_us: u64,
    long_pressed: bool,
    moved_away: bool,
}

impl NebulaInput {
    /// 空构造（供 no_std `static mut` 先建空对象，随后调用 `setup`）。
    pub const fn blank() -> NebulaInput {
        NebulaInput {
            rect: Rect::new(0, 0, 16, 32),
            focused: false,
            text: [0; MAX_TEXT],
            text_len: 0,
            cursor_pos: 0,
            cursor_visible: false,
            cursor_blink_timer: 0.0,
            text_anim_queue: [TextCharAnim::dead(); MAX_ANIMS],
            font: None,
            font_px: 20.0,
            sel_anchor: None,
            sel_active: false,
            sel_start: 0,
            sel_end: 0,
            ps: ParticleSystem::new(0x17A7A5),
            glyph_buf: [0; MAX_GLYPH_BYTES],
            rng: XorShift32::new(0x2E3D),
            anim_t: 0.0,
            down_armed: false,
            down_x: 0,
            down_y: 0,
            down_us: 0,
            long_pressed: false,
            moved_away: false,
        }
    }

    /// 完整构造（草案 `new(rect, font)` 的适配）。
    pub fn new(rect: Rect, font: &'static dyn Font) -> NebulaInput {
        let mut n = NebulaInput::blank();
        n.rect = rect;
        n.font = Some(font);
        n
    }

    /// 设置字体与字号（blank 之后用）。
    pub fn setup(&mut self, rect: Rect, font: &'static dyn Font, px: f32) {
        self.rect = rect;
        self.font = Some(font);
        self.font_px = px;
    }

    /// 当前文本（UTF-8 字节）。
    pub fn text(&self) -> &[u8] {
        &self.text[..self.text_len]
    }

    // ==================== UTF-8 工具 ====================

    fn utf8_next(&self, i: usize) -> (u32, usize) {
        utf8_next_bytes(self.text(), i)
    }

    fn advance_of(&self, cp: u32) -> i32 {
        match self.font {
            Some(f) => f.advance(cp, self.font_px),
            None => {
                // 等宽退避：ASCII 半宽，CJK 全宽
                if cp > 0x7F {
                    self.font_px as i32
                } else {
                    (self.font_px * 0.5) as i32
                }
            }
        }
    }

    /// 从行首到字节边界 `byte_idx` 的宽度。
    fn measure_to(&self, byte_idx: usize) -> i32 {
        let mut x = 0i32;
        let mut i = 0usize;
        while i < byte_idx && i < self.text_len {
            let (cp, n) = self.utf8_next(i);
            x += self.advance_of(cp);
            i += n;
        }
        x
    }

    /// 行首（文本左边界）。
    fn text_left(&self) -> i32 {
        self.rect.x + TEXT_PAD
    }

    /// 行顶（字形占用高度的上沿）。
    fn line_top(&self) -> i32 {
        self.rect.cy() - (self.font_px * 0.75) as i32
    }

    /// 把屏幕 x（触摸 / 鼠标）映射到最近的字符边界（字节偏移）。
    pub fn char_boundary_at(&self, px: i32) -> usize {
        let rel = px - self.text_left();
        // 候选边界：行首 + 每个字符之后；取距离最近（平手取偏右）
        let mut best = 0usize;
        let mut best_d = i32::MAX;
        let d0 = if rel < 0 { -rel } else { rel };
        if d0 < best_d {
            best = 0;
            best_d = d0;
        }
        let mut x = 0i32;
        let mut i = 0usize;
        while i < self.text_len {
            let (cp, n) = self.utf8_next(i);
            x += self.advance_of(cp);
            i += n;
            let d = if x > rel { x - rel } else { rel - x };
            if d <= best_d {
                best = i;
                best_d = d;
            }
        }
        best
    }

    // ==================== 编辑 ====================

    /// 插入一个字符（草案 `insert_char(ch)`）。带选区时先替换选区。
    pub fn insert_char(&mut self, ch: char) {
        if self.sel_active && self.sel_start < self.sel_end {
            self.delete_range(self.sel_start, self.sel_end);
        }
        let mut buf = [0u8; 4];
        let n = encode_utf8(ch, &mut buf);
        if self.text_len + n > MAX_TEXT {
            return; // 缓冲满，忽略（保持状态稳定）
        }
        // 中插：右移
        let end = self.cursor_pos;
        let mut k = self.text_len;
        while k > end {
            self.text[k + n - 1] = self.text[k - 1];
            k -= 1;
        }
        self.text[end..end + n].copy_from_slice(&buf[..n]);
        self.text_len += n;
        // 动画入队
        self.push_anim(end);
        self.cursor_pos = end + n;
        self.sel_anchor = None;
        self.sel_active = false;
    }

    /// 退格（草案 `backspace()`）：有选区则删选区，否则删光标前字符。
    pub fn backspace(&mut self) {
        if self.sel_active && self.sel_start < self.sel_end {
            self.delete_range(self.sel_start, self.sel_end);
            return;
        }
        if self.cursor_pos == 0 {
            return;
        }
        // 解码前面的字符长度
        let i = self.cursor_pos;
        let n = if is_utf8_cont(self.text[i - 1]) {
            // 跳过后续字节找首字节（反向走）
            let mut lead = i - 1;
            while lead > 0 && is_utf8_cont(self.text[lead]) {
                lead -= 1;
            }
            i - lead
        } else {
            1
        };
        let start = self.cursor_pos - n;
        self.delete_range(start, self.cursor_pos);
    }

    /// 删除字节区间 `[start, end)`（文本 + 动画 + 光标 + 选区均维护），
    /// 每个被删字符在其字形位置散落沙粒。
    fn delete_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.text_len {
            self.sel_active = false;
            self.sel_anchor = None;
            return;
        }
        // 逐字符散沙（按字形中心）
        let mut i = start;
        while i < end {
            let (cp, n) = self.utf8_next(i);
            let cx = self.measure_to(i) + self.text_left() + self.advance_of(cp) / 2;
            let cy = self.rect.cy();
            self.ps.scatter(cx as f32, cy as f32, 6 + (cp % 5) + 6, 90.0);
            self.ps.spark(cx as f32, cy as f32, 0xFFE9FF);
            i += n;
        }
        // 文本左移
        let len = end - start;
        for k in end..self.text_len {
            self.text[k - len] = self.text[k];
        }
        self.text_len -= len;
        // 动画队列：删除落在区间内的条目，其余条目偏移前移
        self.retain_anims(start, end, len);
        self.cursor_pos = start.min(self.text_len);
        self.sel_active = false;
        self.sel_anchor = None;
        self.sel_start = 0;
        self.sel_end = 0;
    }

    /// 动画队列维护：移除 `[start,end)` 内的条目，其余条目若偏移 ≥ end 则前移 `len`。
    fn retain_anims(&mut self, start: usize, end: usize, len: usize) {
        for a in self.text_anim_queue.iter_mut() {
            if !a.alive {
                continue;
            }
            if a.char_idx >= start && a.char_idx < end {
                a.alive = false;
            } else if a.char_idx >= end {
                a.char_idx -= len;
            }
        }
        // 紧致队列
        let mut w = 0usize;
        for r in 0..MAX_ANIMS {
            if self.text_anim_queue[r].alive {
                if w != r {
                    self.text_anim_queue[w] = self.text_anim_queue[r];
                }
                w += 1;
            }
        }
        for r in w..MAX_ANIMS {
            self.text_anim_queue[r] = TextCharAnim::dead();
        }
    }

    /// 书写动画入队（槽满则丢弃最老的动画、该字符直接定格）。
    fn push_anim(&mut self, char_idx: usize) {
        for i in 0..MAX_ANIMS {
            if !self.text_anim_queue[i].alive {
                self.text_anim_queue[i] = TextCharAnim {
                    char_idx,
                    t: 0.0,
                    dur: CHAR_ANIM_DUR,
                    alive: true,
                };
                return;
            }
        }
        // 槽满：把最旧的定格掉（瞬时完成，字符立即显示）
        let mut oldest = 0;
        for i in 1..MAX_ANIMS {
            if self.text_anim_queue[i].t < self.text_anim_queue[oldest].t {
                oldest = i;
            }
        }
        self.text_anim_queue[oldest] = TextCharAnim {
            char_idx,
            t: 0.0,
            dur: CHAR_ANIM_DUR,
            alive: true,
        };
    }

    // ==================== 键盘 ====================

    /// 键盘事件（宿主把 app_kit 的 `KeyAction` 映射过来）。
    pub fn handle_key(&mut self, key: KeyAction) {
        if !self.focused {
            return;
        }
        match key {
            KeyAction::Char(c) => {
                self.insert_char(c as char);
            }
            KeyAction::Backspace => self.backspace(),
        }
    }

    // ==================== 触摸 ====================

    /// 触摸：按下落光标、长按/拖动拉出选区、抬起结束（保留选区供删除/复制）。
    pub fn handle_touch(&mut self, e: &TouchEvent) {
        match e.phase {
            TouchPhase::Down => {
                if !self.rect.contains(e.x, e.y) {
                    self.focused = false;
                    self.sel_active = false;
                    self.down_armed = false;
                    return;
                }
                self.focused = true;
                self.cursor_pos = self.char_boundary_at(e.x);
                self.sel_anchor = Some(self.cursor_pos);
                self.down_armed = true;
                self.down_x = e.x;
                self.down_y = e.y;
                self.down_us = e.us;
                self.long_pressed = false;
                self.moved_away = false;
            }
            TouchPhase::Move => {
                if !self.down_armed {
                    return;
                }
                let moved = (e.x - self.down_x).abs() + (e.y - self.down_y).abs() > 6;
                if moved {
                    self.moved_away = true;
                }
                let held = e.us.saturating_sub(self.down_us) >= LONG_PRESS_US;
                if moved && held {
                    self.long_pressed = true;
                }
                // 更新光标（拖动时同步拉长选区）
                if self.rect.contains(e.x, e.y) {
                    self.cursor_pos = self.char_boundary_at(e.x);
                    if self.long_pressed {
                        if let Some(a) = self.sel_anchor {
                            self.sel_active = true;
                            self.sel_start = a.min(self.cursor_pos);
                            self.sel_end = a.max(self.cursor_pos);
                        }
                    }
                }
            }
            TouchPhase::Up => {
                if !self.down_armed {
                    return;
                }
                self.down_armed = false;
                if self.long_pressed {
                    // 保留选区（供删除 / 复制）
                    if let Some(a) = self.sel_anchor {
                        self.sel_active = true;
                        self.sel_start = a.min(self.cursor_pos);
                        self.sel_end = a.max(self.cursor_pos);
                    }
                } else {
                    // 普通点按：只落光标
                    self.sel_active = false;
                    self.sel_start = 0;
                    self.sel_end = 0;
                }
                self.sel_anchor = None;
            }
        }
    }

    // ==================== 渲染 ====================

    /// 渲染并推进一帧。
    pub fn render(&mut self, canvas: &mut dyn Canvas, dt: f32) {
        self.anim_t += dt; // 全局时钟（光标呼吸 / 星点相位）
        self.cursor_blink_timer += dt;

        // 书写动画推进：完成的条目出队（字符定格）
        let mut w = 0usize;
        for r in 0..MAX_ANIMS {
            if !self.text_anim_queue[r].alive {
                continue;
            }
            self.text_anim_queue[r].t += dt;
            if self.text_anim_queue[r].t >= self.text_anim_queue[r].dur {
                continue; // 完成，回收
            }
            if w != r {
                self.text_anim_queue[w] = self.text_anim_queue[r];
            }
            w += 1;
        }
        for r in w..MAX_ANIMS {
            self.text_anim_queue[r] = TextCharAnim::dead();
        }

        // 粒子推进（退格散沙 / 星点）
        self.ps.update(dt);

        // —— 底座 ——
        let r = (self.rect.h / 2).min(12);
        let base = if self.focused { 0x23_36_50 } else { 0x1C_1C_1E };
        canvas.fill_rounded(self.rect.x, self.rect.y, self.rect.w, self.rect.h, r, base);
        let border = if self.focused {
            lerp_color(0x38_38_3A, 0x7B2CBF, 0.5)
        } else {
            0x38_38_3A
        };
        canvas.rounded_border(self.rect.x, self.rect.y, self.rect.w, self.rect.h, r, border);

        // —— 选区高亮（星云紫蓝半透明）——
        if self.sel_active && self.sel_start < self.sel_end {
            let x0 = self.text_left() + self.measure_to(self.sel_start);
            let x1 = self.text_left() + self.measure_to(self.sel_end);
            let top = self.rect.cy() - (self.font_px * 0.55) as i32;
            let h = (self.font_px * 1.05) as i32;
            for dy in 0..h {
                for dx in x0..x1 {
                    canvas.blend_pixel(dx, top + dy, C_SELECT, 90);
                }
            }
        }

        // —— 字符 ——
        let x_start = self.text_left();
        let y_top = self.line_top();
        let right_limit = self.rect.right() - 2;
        let mut x = x_start;
        let mut i = 0usize;
        let mut seq = 0usize;
        while i < self.text_len && x < right_limit {
            let (cp, n) = self.utf8_next(i);
            // 是否正在书写动画中？
            let anim_p = {
                let a = self.text_anim_queue.iter().find(|a| a.alive && a.char_idx == i);
                a.map(|a| a.progress())
            };
            match anim_p {
                Some(p) => {
                    let alpha = (p * 250.0) as u32 + 5;
                    let slide = ((1.0 - p) * 10.0) as i32; // 从左侧滑入
                    // 星光笔迹：写在「笔尖」位置的光晕
                    let cx = x + slide + self.advance_of(cp) / 2;
                    crate::draw_glow(canvas, cx as f32, self.rect.cy() as f32, 6, 0xC77DFF, (p * 120.0 + 30.0) as u32);
                    self.draw_glyph(canvas, cp, x + slide, y_top, 0xFF_FF_FF, alpha);
                }
                None => {
                    // 已定格字符：本体 + 星光边缘（确定性 1~2 个高亮点）
                    self.draw_glyph(canvas, cp, x, y_top, 0xFF_FF_FF, 255);
                    self.sparkle_glyph(canvas, cp, x, y_top, seq);
                }
            }
            x += self.advance_of(cp);
            i += n;
            seq += 1;
        }

        // 散落的沙粒 / 星点
        self.ps.render(canvas, false);

        // —— 光标：宇宙色竖杠 + 1Hz 呼吸 ——
        if self.focused {
            let cx = self.text_left() + self.measure_to(self.cursor_pos);
            let cy_top = self.rect.cy() - (self.font_px * 0.6) as i32;
            let cy_h = (self.font_px * 1.1) as i32;
            // 呼吸亮度 0.25..1.0（亮度呼吸，不是显隐）
            let breath = 0.5 + 0.5 * fsin(2.0 * core::f32::consts::PI * self.cursor_blink_timer);
            let alpha = (0.25 + 0.75 * breath) * 255.0;
            let cy_h = cy_h.max(1);
            for k in 0..cy_h {
                let col = lerp_color(CURSOR_TOP, CURSOR_BOTTOM, k as f32 / cy_h as f32);
                canvas.blend_pixel(cx, cy_top + k, col, alpha as u32);
                canvas.blend_pixel(cx + 1, cy_top + k, col, alpha as u32);
            }
            if breath > 0.6 {
                crate::draw_glow(canvas, cx as f32, self.rect.cy() as f32, 4, 0x00B4D8, ((breath - 0.6) * 220.0) as u32);
            }
        }
    }

    /// 把 `cp` 以透明度 `alpha`（0..=255）绘制到 `(x, y_top)`。
    fn draw_glyph(&mut self, canvas: &mut dyn Canvas, cp: u32, x: i32, y_top: i32, color: u32, alpha: u32) {
        let Some(f) = self.font else {
            return;
        };
        let Some(g) = f.rasterize(cp, self.font_px, &mut self.glyph_buf) else {
            return;
        };
        if g.width <= 0 || g.height <= 0 {
            return;
        }
        let gx = x + g.xmin;
        for row in 0..g.height {
            for col in 0..g.width {
                let cov = self.glyph_buf[(row * g.width + col) as usize];
                if cov == 0 {
                    continue;
                }
                let a = (cov as u32 * alpha / 255).min(255);
                if a > 0 {
                    canvas.blend_pixel(gx + col, y_top + row, color, a);
                }
            }
        }
    }

    /// 星尘边缘：在字形笔画的确定性位置补 1 粒发亮星点（沙粒质感 / 星光边缘）。
    fn sparkle_glyph(&mut self, canvas: &mut dyn Canvas, cp: u32, x: i32, y_top: i32, seq: usize) {
        let Some(f) = self.font else {
            return;
        };
        let Some(g) = f.rasterize(cp, self.font_px, &mut self.glyph_buf) else {
            return;
        };
        if g.width <= 0 || g.height <= 0 {
            return;
        }
        let n = (g.width * g.height) as usize;
        if n == 0 {
            return;
        }
        // 黄金角步进扫描笔画（确定性、不闪烁）
        let mut k = (seq.wrapping_mul(37)) % n;
        let mut tries = 0;
        while tries < n {
            if self.glyph_buf[k] > 180 {
                let col = (k % g.width as usize) as i32;
                let row = (k / g.width as usize) as i32;
                canvas.blend_pixel(x + g.xmin + col, y_top + row, 0xFFE9FF, 250);
                break;
            }
            k = (k + 13) % n;
            tries += 1;
        }
    }
}

// ==================== UTF-8 工具函数 ====================

/// 把字符编码为 UTF-8，返回字节数（写入 `out[..4]`）。
fn encode_utf8(ch: char, out: &mut [u8; 4]) -> usize {
    let u = ch as u32;
    if u < 0x80 {
        out[0] = u as u8;
        1
    } else if u < 0x800 {
        out[0] = 0xC0 | ((u >> 6) as u8);
        out[1] = 0x80 | ((u & 0x3F) as u8);
        2
    } else if u < 0x1_0000 {
        out[0] = 0xE0 | ((u >> 12) as u8);
        out[1] = 0x80 | (((u >> 6) & 0x3F) as u8);
        out[2] = 0x80 | ((u & 0x3F) as u8);
        3
    } else {
        out[0] = 0xF0 | ((u >> 18) as u8);
        out[1] = 0x80 | (((u >> 12) & 0x3F) as u8);
        out[2] = 0x80 | (((u >> 6) & 0x3F) as u8);
        out[3] = 0x80 | ((u & 0x3F) as u8);
        4
    }
}

fn is_utf8_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// 从切片读下一个字符（返回 cp 与字节长）。
fn utf8_next_bytes(s: &[u8], i: usize) -> (u32, usize) {
    let b0 = s[i] as u32;
    if b0 < 0x80 {
        (b0, 1)
    } else if b0 & 0xE0 == 0xC0 {
        let b1 = (s[i + 1] & 0x3F) as u32;
        (((b0 & 0x1F) << 6) | b1, 2)
    } else if b0 & 0xF0 == 0xE0 {
        let b1 = (s[i + 1] & 0x3F) as u32;
        let b2 = (s[i + 2] & 0x3F) as u32;
        (((b0 & 0x0F) << 12) | (b1 << 6) | b2, 3)
    } else {
        let b1 = (s[i + 1] & 0x3F) as u32;
        let b2 = (s[i + 2] & 0x3F) as u32;
        let b3 = (s[i + 3] & 0x3F) as u32;
        (((b0 & 0x07) << 18) | (b1 << 12) | (b2 << 6) | b3, 4)
    }
}

// ==================== 单元测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemCanvas;

    /// 测试字体：8x8 ASCII / 16x16 CJK 棋盘格（每点都有覆盖率）。& 用 `'static`。
    static MOCK_FONT: MockFont = MockFont;

    struct MockFont;

    impl Font for MockFont {
        fn rasterize(&self, cp: u32, _px: f32, out: &mut [u8]) -> Option<GlyphRaster> {
            if cp > 0x7F {
                let (w, h) = (16, 16);
                if out.len() < (w * h) as usize {
                    return None;
                }
                for row in 0..h {
                    for col in 0..w {
                        out[(row * w + col) as usize] = if (row + col) % 3 == 0 {
                            255
                        } else {
                            60
                        };
                    }
                }
                Some(GlyphRaster {
                    width: w,
                    height: h,
                    advance: 16,
                    xmin: 0,
                })
            } else {
                let (w, h) = (8, 8);
                if out.len() < (w * h) as usize {
                    return None;
                }
                for row in 0..h {
                    for col in 0..w {
                        out[(row * w + col) as usize] = if (row + col) % 2 == 0 { 235 } else { 40 };
                    }
                }
                Some(GlyphRaster {
                    width: w,
                    height: h,
                    advance: 8,
                    xmin: 0,
                })
            }
        }
        fn advance(&self, cp: u32, _px: f32) -> i32 {
            if cp > 0x7F {
                16
            } else {
                8
            }
        }
    }

    #[test]
    fn insert_advances_cursor_and_cjk() {
        let mut n = NebulaInput::new(Rect::new(40, 300, 260, 40), &MOCK_FONT);
        n.font_px = 20.0;
        n.insert_char('星');
        n.insert_char('辰');
        assert_eq!(n.cursor_pos, 6);
        assert_eq!(n.text(), "星辰".as_bytes());
        // 中插
        n.cursor_pos = 3;
        n.insert_char('A');
        assert_eq!(n.text(), "星A辰".as_bytes());
        assert_eq!(n.cursor_pos, 4);
    }

    #[test]
    fn backspace_scatters_and_moves_cursor() {
        let mut n = NebulaInput::new(Rect::new(40, 300, 260, 40), &MOCK_FONT);
        n.font_px = 20.0;
        n.insert_char('星');
        n.insert_char('辰');
        n.backspace(); // 删「辰」（3 字节）
        assert_eq!(n.text(), "星".as_bytes());
        assert_eq!(n.cursor_pos, 3);
        assert!(n.ps.alive() > 0, "退格应散出沙粒");
    }

    #[test]
    fn long_press_selects_and_deletes() {
        let mut n = NebulaInput::new(Rect::new(40, 300, 260, 40), &MOCK_FONT);
        n.font_px = 20.0;
        n.insert_char('a');
        n.insert_char('b');
        n.insert_char('c');
        // 长按 'a'，拖到 'c' 结束
        n.handle_touch(&TouchEvent::down(48, 320, 1_000));
        n.handle_touch(&TouchEvent::move_(48, 320, 0, 0, 500_000));
        n.handle_touch(&TouchEvent::move_(72, 320, 24, 0, 600_000));
        n.handle_touch(&TouchEvent::up(72, 320, 700_000));
        assert!(n.sel_active);
        assert_eq!((n.sel_start, n.sel_end), (0, 3));
        // 删除选区
        n.backspace();
        assert_eq!(n.text_len, 0);
    }

    #[test]
    fn char_boundary_mapping() {
        let mut n = NebulaInput::new(Rect::new(40, 300, 260, 40), &MOCK_FONT);
        n.font_px = 20.0;
        n.insert_char('a');
        n.insert_char('b');
        n.insert_char('c');
        // a(0..8) b(8..16) c(16..24)；行首 = 40+12 = 52
        // 点击 'b' 中间 (52+12=64) → 最邻近边界 16 → 索引 2
        assert_eq!(n.char_boundary_at(64), 2);
        // 点击行首 (52) → 0
        assert_eq!(n.char_boundary_at(52), 0);
        // 点击 'a' 右半 (59) → 8 → 索引 1
        assert_eq!(n.char_boundary_at(59), 1);
    }

    #[test]
    fn render_does_not_crash() {
        let mut n = NebulaInput::new(Rect::new(40, 300, 260, 40), &MOCK_FONT);
        n.font_px = 20.0;
        for ch in "KunxOS 星辰".chars() {
            n.insert_char(ch);
        }
        n.focused = true;
        let mut buf = [0u32; 540 * 120];
        let mut canvas = MemCanvas {
            w: 540,
            h: 120,
            buf: &mut buf,
        };
        for _ in 0..60 {
            n.render(&mut canvas, 0.016);
        }
        assert!(n.text_len > 0);
    }
}
