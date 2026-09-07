//! 极简 UI 框架：声明式布局 + 基础控件（面板/文本/按钮/列表/图片）。
//!
//! 控件以 `WidgetKind` 描述，布局通过 `vbox`/`hbox` 声明式计算控件矩形；
//! `Ui::render` 把控件绘制到窗口表面，`Ui::hit_test`/`tap_at` 完成控件级
//! 命中与点击回调分发（按钮）。
//!
//! 文本使用平滑字体渲染（见 `font_render`）：常规文字从 rootfs 的
//! `/fonts/cn.ttf`（uiputong 子集）加载，标题从 `/fonts/bold.ttf`（bold 子集）
//! 加载；首次绘制前调用 [`font_init`]。

#[path = "../../shared/font_render.rs"]
mod font_render;

use alloc::vec::Vec;

use crate::compositor::{Color, Rect, Surface};

/// 单窗口最大控件数。
pub const MAX_WIDGETS: usize = 64;
/// 字形尺寸（像素）——TTF 渲染下为 16px 字号的行高参考。
pub const GLYPH_W: i32 = 16;
pub const GLYPH_H: i32 = 16;

/// 读取 rootfs 中的整个字体文件。失败返回空 Vec。
fn read_font(path: &str) -> Vec<u8> {
    let fd = crate::vfs::open(path, 0);
    if fd < 0 {
        return Vec::new();
    }
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = crate::vfs::read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
    }
    crate::vfs::close(fd);
    data
}

/// 从 rootfs 加载常规字体 `/fonts/cn.ttf`（uiputong 子集）与标题字体
/// `/fonts/bold.ttf`（bold 子集，幂等）。成功返回 true。
pub fn font_init() -> bool {
    let cn_ok = font_render::font_ready();
    if !cn_ok {
        let data = read_font("/fonts/cn.ttf");
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set(f);
            }
        }
    }
    if !font_render::font_bold_ready() {
        let data = read_font("/fonts/bold.ttf");
        if !data.is_empty() {
            if let Some(f) = font_render::font_from_bytes(&data) {
                font_render::font_set_bold(f);
            }
        }
    }
    // Phosphor 图标字体（icon-fill 实心 / icon-line 粗线 emoji，见
    // shared/icons.rs）；失败不影响内核文字渲染，仅缺图标/emoji 呈现。
    if !font_render::icon_ready(true) {
        if let Some(f) = font_render::font_from_bytes(&read_font("/fonts/icon-fill.ttf")) {
            font_render::font_set_icon(true, f);
        }
    }
    if !font_render::icon_ready(false) {
        if let Some(f) = font_render::font_from_bytes(&read_font("/fonts/icon-line.ttf")) {
            font_render::font_set_icon(false, f);
        }
    }
    font_render::font_ready()
}

/// 控件点击回调：`(窗口 id, 控件 id, 本地 x, 本地 y)`。
pub type TapHandler = fn(window: usize, widget: usize, x: i32, y: i32);

/// 控件类型。
#[derive(Clone, Copy)]
pub enum WidgetKind {
    /// 纯色面板。
    #[allow(dead_code)]
    Panel { color: Color },
    /// 文本标签。
    Label { text: &'static str, color: Color },
    /// 按钮（可点击）。
    Button { text: &'static str, color: Color, text_color: Color, on_tap: TapHandler },
    /// 可滚动列表。
    List { items: &'static [&'static str], color: Color, text_color: Color, scroll: i32 },
    /// 图片（原始像素 `0x00RRGGBB`，1:1 显示）。
    Image { data: *const u32, w: u32, h: u32 },
}

/// 一个控件实例（矩形为窗口本地坐标）。
#[derive(Clone, Copy)]
pub struct Widget {
    pub kind: WidgetKind,
    pub rect: Rect,
}

/// 一个窗口的 UI（控件集合）。
pub struct Ui {
    widgets: [Option<Widget>; MAX_WIDGETS],
    count: usize,
}

impl Ui {
    pub const fn new() -> Ui {
        Ui { widgets: [None; MAX_WIDGETS], count: 0 }
    }

    /// 添加控件，返回控件 id（满返回 usize::MAX）。
    pub fn add(&mut self, kind: WidgetKind, rect: Rect) -> usize {
        if self.count >= MAX_WIDGETS {
            return usize::MAX;
        }
        let id = self.count;
        self.widgets[id] = Some(Widget { kind, rect });
        self.count += 1;
        id
    }

    /// 渲染所有控件到 `s`。
    pub fn render(&self, s: &Surface) {
        for i in 0..self.count {
            if let Some(w) = &self.widgets[i] {
                render_widget(s, w);
            }
        }
    }

    /// 控件命中测试：返回 `(lx, ly)` 处最上层控件 id。
    pub fn hit_test(&self, lx: i32, ly: i32) -> Option<usize> {
        for i in (0..self.count).rev() {
            if let Some(w) = &self.widgets[i] {
                if w.rect.contains(lx, ly) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// 控件矩形（窗口本地坐标）。
    pub fn widget_rect(&self, id: usize) -> Option<Rect> {
        if id >= MAX_WIDGETS {
            return None;
        }
        self.widgets[id].as_ref().map(|w| w.rect)
    }

    /// 处理点击：命中按钮则触发回调，返回是否命中按钮。
    pub fn tap_at(&self, window: usize, lx: i32, ly: i32) -> bool {
        if let Some(id) = self.hit_test(lx, ly) {
            if let Some(w) = &self.widgets[id] {
                if let WidgetKind::Button { on_tap, .. } = w.kind {
                    on_tap(window, id, lx, ly);
                    return true;
                }
            }
        }
        false
    }
}

fn render_widget(s: &Surface, w: &Widget) {
    match &w.kind {
        WidgetKind::Panel { color } => {
            s.fill_rect(w.rect, *color);
        }
        WidgetKind::Label { text, color } => {
            draw_text(s, w.rect.x + 4, w.rect.y + 4, text, *color, 1);
        }
        WidgetKind::Button { text, color, text_color, .. } => {
            s.fill_rect(w.rect, *color);
            s.draw_rect(w.rect, Color::GRAY, 1);
            let tw = font_render::text_width(text.as_bytes(), font_render::FONT_PX);
            let tx = w.rect.x + (w.rect.w as i32 - tw) / 2;
            let ty = w.rect.y + (w.rect.h as i32 - GLYPH_H) / 2;
            draw_text(s, tx, ty, text, *text_color, 1);
        }
        WidgetKind::List { items, color, text_color, scroll } => {
            s.fill_rect(w.rect, *color);
            let mut y = w.rect.y + 4;
            let mut idx = *scroll;
            while y + GLYPH_H <= w.rect.bottom() && (idx as usize) < items.len() {
                let it = items[idx as usize];
                draw_text(s, w.rect.x + 4, y, it, *text_color, 1);
                y += GLYPH_H + 6;
                idx += 1;
            }
        }
        WidgetKind::Image { data, w: iw, h: ih } => {
            // 裸指针 + 尺寸防御：data 为 null 或 w/h 异常（0/巨大）时跳过，
            // 并把源尺寸钳制到目标矩形内，防止 blit 越界读任意内存。
            // 注意不能用 `*data as usize == 0` 判空——那会先解引用空指针；
            // 这里用 is_null() 直接检查指针本身。
            if data.is_null() || *iw == 0 || *ih == 0 {
                return;
            }
            // 源行距保持 iw：目标矩形只裁剪拷贝宽高（此前把源宽钳到
            // rect.w，iw > rect.w 时每行内容整体错位）。
            s.blit_strided(
                w.rect.x,
                w.rect.y,
                *data as *mut u32,
                *iw,
                (*iw).min(w.rect.w),
                (*ih).min(w.rect.h),
            );
        }
    }
}

// ==================== 声明式布局 ====================

/// 在 `container` 内垂直等分排列 `out.len()` 个矩形（含间距）。
#[allow(dead_code)]
pub fn vbox(container: Rect, item_h: u32, gap: u32, out: &mut [Rect]) {
    let n = out.len().max(1) as i32;
    // 防 u32→i32 溢出（item_h/gap 过大时 `as i32` 回绕成负值 → 布局错乱）
    let ih = item_h.min(i32::MAX as u32) as i32;
    let gp = gap.min(i32::MAX as u32) as i32;
    let total = n * ih + (n - 1) * gp;
    let y0 = container.y + (container.h as i32 - total).max(0) / 2;
    for (i, r) in out.iter_mut().enumerate() {
        let y = y0 + i as i32 * (ih + gp);
        *r = Rect::new(container.x, y, container.w, item_h);
    }
}

/// 在 `container` 内水平等分排列 `out.len()` 个矩形（含间距）。
#[allow(dead_code)]
pub fn hbox(container: Rect, item_w: u32, gap: u32, out: &mut [Rect]) {
    let n = out.len().max(1) as i32;
    let iw = item_w.min(i32::MAX as u32) as i32;
    let gp = gap.min(i32::MAX as u32) as i32;
    let total = n * iw + (n - 1) * gp;
    let x0 = container.x + (container.w as i32 - total).max(0) / 2;
    for (i, r) in out.iter_mut().enumerate() {
        let x = x0 + i as i32 * (iw + gp);
        *r = Rect::new(x, container.y, item_w, container.h);
    }
}

// ==================== 文本绘制 ====================

/// 在 `(x, y)` 绘制字符串（`scale` 为字号放大倍数）。支持 UTF-8，
/// 全部字形以 cn.ttf 平滑渲染（宽度按字体度量推进）。
pub fn draw_text(s: &Surface, x: i32, y: i32, text: &str, color: Color, scale: u32) {
    let px = font_render::FONT_PX * scale.max(1) as f32;
    let bytes = text.as_bytes();
    let mut cx = x;
    let mut i = 0;
    while i < bytes.len() {
        let (cp, n) = font_render::utf8_next(bytes, i);
        cx += draw_glyph_ttf(s, cx, y, cp, color, px);
        i += n;
    }
}

/// 文本宽度（像素，cn.ttf 字体度量）。
pub fn text_width(text: &str) -> i32 {
    font_render::text_width(text.as_bytes(), font_render::FONT_PX)
}

/// 文本宽度（像素，bold.ttf 标题字体度量）。
pub fn text_width_bold(text: &str) -> i32 {
    font_render::text_width_bold(text.as_bytes(), font_render::FONT_PX)
}

/// 把单个码点以 cn.ttf 平滑字形绘制到 `(x, y)`（y 为字形顶部），返回推进宽度。
/// 字体未就绪时退回固定宽度（CJK 16 / ASCII 8）且不绘制。
fn draw_glyph_ttf(s: &Surface, x: i32, y: i32, cp: u32, color: Color, px: f32) -> i32 {
    if !font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        let gy = font_render::glyph_draw_top(y, g.ymin, g.height, px);
        let fg = color.pixel();
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    if px_ >= 0 && py_ >= 0 && (px_ as u32) < s.w && (py_ as u32) < s.h {
                        let bg = unsafe {
                            *s.pixels.add(py_ as usize * s.w as usize + px_ as usize)
                        };
                        s.set(px_, py_, font_render::blend(bg, fg, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

/// 以 bold.ttf 标题字体把单个码点绘制到 `(x, y)`，返回推进宽度。
/// 标题字体未就绪或缺少该字形时退回常规字体。
fn draw_glyph_ttf_bold(s: &Surface, x: i32, y: i32, cp: u32, color: Color, px: f32) -> i32 {
    if !font_render::font_bold_ready() || !font_render::has_glyph_bold(cp) {
        return draw_glyph_ttf(s, x, y, cp, color, px);
    }
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = font_render::rasterize_char_bold(ch, px) {
        let gx = x + g.xmin;
        let gy = font_render::glyph_draw_top_bold(y, g.ymin, g.height, px);
        let fg = color.pixel();
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    if px_ >= 0 && py_ >= 0 && (px_ as u32) < s.w && (py_ as u32) < s.h {
                        let bg = unsafe {
                            *s.pixels.add(py_ as usize * s.w as usize + px_ as usize)
                        };
                        s.set(px_, py_, font_render::blend(bg, fg, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

/// 以 bold.ttf 标题字体绘制字符串（标题）。
pub fn draw_text_bold(s: &Surface, x: i32, y: i32, text: &str, color: Color, scale: u32) {
    let px = font_render::FONT_PX * scale.max(1) as f32;
    let bytes = text.as_bytes();
    let mut cx = x;
    let mut i = 0;
    while i < bytes.len() {
        let (cp, n) = font_render::utf8_next(bytes, i);
        cx += draw_glyph_ttf_bold(s, cx, y, cp, color, px);
        i += n;
    }
}

// ==================== 8x8 点阵字体（公有领域，U+0020..U+007E） ====================

static FONT8X8: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00], // !
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x36, 0x36, 0x7F, 0x36, 0x7F, 0x36, 0x36, 0x00], // #
    [0x0C, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x0C, 0x00], // $
    [0x00, 0x63, 0x33, 0x18, 0x0C, 0x66, 0x63, 0x00], // %
    [0x1C, 0x36, 0x1C, 0x6E, 0x3B, 0x33, 0x6E, 0x00], // &
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x18, 0x0C, 0x06, 0x06, 0x06, 0x0C, 0x18, 0x00], // (
    [0x06, 0x0C, 0x18, 0x18, 0x18, 0x0C, 0x06, 0x00], // )
    [0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00], // *
    [0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ,
    [0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x00], // .
    [0x60, 0x30, 0x18, 0x0C, 0x06, 0x03, 0x01, 0x00], // /
    [0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00], // 0
    [0x0C, 0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x3F, 0x00], // 1
    [0x1E, 0x33, 0x30, 0x1C, 0x06, 0x33, 0x3F, 0x00], // 2
    [0x1E, 0x33, 0x30, 0x1C, 0x30, 0x33, 0x1E, 0x00], // 3
    [0x38, 0x3C, 0x36, 0x33, 0x7F, 0x30, 0x78, 0x00], // 4
    [0x3F, 0x03, 0x1F, 0x30, 0x30, 0x33, 0x1E, 0x00], // 5
    [0x1C, 0x06, 0x03, 0x1F, 0x33, 0x33, 0x1E, 0x00], // 6
    [0x3F, 0x33, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x00], // 7
    [0x1E, 0x33, 0x33, 0x1E, 0x33, 0x33, 0x1E, 0x00], // 8
    [0x1E, 0x33, 0x33, 0x3E, 0x30, 0x18, 0x0E, 0x00], // 9
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x00], // :
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ;
    [0x18, 0x0C, 0x06, 0x03, 0x06, 0x0C, 0x18, 0x00], // <
    [0x00, 0x00, 0x3F, 0x00, 0x00, 0x3F, 0x00, 0x00], // =
    [0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00], // >
    [0x1E, 0x33, 0x30, 0x18, 0x0C, 0x00, 0x0C, 0x00], // ?
    [0x3E, 0x63, 0x7B, 0x7B, 0x7B, 0x03, 0x1E, 0x00], // @
    [0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00], // A
    [0x3F, 0x66, 0x66, 0x3E, 0x66, 0x66, 0x3F, 0x00], // B
    [0x3C, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3C, 0x00], // C
    [0x1F, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1F, 0x00], // D
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x46, 0x7F, 0x00], // E
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x06, 0x0F, 0x00], // F
    [0x3C, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7C, 0x00], // G
    [0x33, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x33, 0x00], // H
    [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // I
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E, 0x00], // J
    [0x67, 0x66, 0x36, 0x1E, 0x36, 0x66, 0x67, 0x00], // K
    [0x0F, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7F, 0x00], // L
    [0x63, 0x77, 0x7F, 0x7F, 0x6B, 0x63, 0x63, 0x00], // M
    [0x63, 0x67, 0x6F, 0x7B, 0x73, 0x63, 0x63, 0x00], // N
    [0x1C, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1C, 0x00], // O
    [0x3F, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x0F, 0x00], // P
    [0x1E, 0x33, 0x33, 0x33, 0x3B, 0x1E, 0x38, 0x00], // Q
    [0x3F, 0x66, 0x66, 0x3E, 0x36, 0x66, 0x67, 0x00], // R
    [0x1E, 0x33, 0x07, 0x0E, 0x38, 0x33, 0x1E, 0x00], // S
    [0x3F, 0x2D, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // T
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3F, 0x00], // U
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // V
    [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00], // W
    [0x63, 0x63, 0x36, 0x1C, 0x1C, 0x36, 0x63, 0x00], // X
    [0x33, 0x33, 0x33, 0x1E, 0x0C, 0x0C, 0x1E, 0x00], // Y
    [0x7F, 0x63, 0x31, 0x18, 0x4C, 0x66, 0x7F, 0x00], // Z
    [0x1E, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1E, 0x00], // [
    [0x03, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00], // 反斜杠
    [0x1E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1E, 0x00], // ]
    [0x08, 0x1C, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF], // _
    [0x0C, 0x0C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x1E, 0x30, 0x3E, 0x33, 0x6E, 0x00], // a
    [0x07, 0x06, 0x06, 0x3E, 0x66, 0x66, 0x3B, 0x00], // b
    [0x00, 0x00, 0x1E, 0x33, 0x03, 0x33, 0x1E, 0x00], // c
    [0x38, 0x30, 0x30, 0x3E, 0x33, 0x33, 0x6E, 0x00], // d
    [0x00, 0x00, 0x1E, 0x33, 0x3F, 0x03, 0x1E, 0x00], // e
    [0x1C, 0x36, 0x06, 0x0F, 0x06, 0x06, 0x0F, 0x00], // f
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x1F], // g
    [0x07, 0x06, 0x36, 0x6E, 0x66, 0x66, 0x67, 0x00], // h
    [0x0C, 0x00, 0x0E, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // i
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E], // j
    [0x07, 0x06, 0x66, 0x36, 0x1E, 0x36, 0x67, 0x00], // k
    [0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // l
    [0x00, 0x00, 0x33, 0x7F, 0x7F, 0x6B, 0x63, 0x00], // m
    [0x00, 0x00, 0x1F, 0x33, 0x33, 0x33, 0x33, 0x00], // n
    [0x00, 0x00, 0x1E, 0x33, 0x33, 0x33, 0x1E, 0x00], // o
    [0x00, 0x00, 0x3B, 0x66, 0x66, 0x3E, 0x06, 0x0F], // p
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x78], // q
    [0x00, 0x00, 0x3B, 0x6E, 0x66, 0x06, 0x0F, 0x00], // r
    [0x00, 0x00, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x00], // s
    [0x08, 0x0C, 0x3E, 0x0C, 0x0C, 0x2C, 0x18, 0x00], // t
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6E, 0x00], // u
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // v
    [0x00, 0x00, 0x63, 0x6B, 0x7F, 0x7F, 0x36, 0x00], // w
    [0x00, 0x00, 0x63, 0x36, 0x1C, 0x36, 0x63, 0x00], // x
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3E, 0x30, 0x1F], // y
    [0x00, 0x00, 0x3F, 0x19, 0x0C, 0x26, 0x3F, 0x00], // z
    [0x38, 0x0C, 0x0C, 0x07, 0x0C, 0x0C, 0x38, 0x00], // {
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00], // |
    [0x07, 0x0C, 0x0C, 0x38, 0x0C, 0x0C, 0x07, 0x00], // }
    [0x6E, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ~
];

// ==================== 8x8 点阵文本（无 TTF 依赖，供性能 overlay 等） ====================

/// 以 8x8 点阵字体绘制 ASCII 文本（`FONT8X8`，U+0020..U+007E），
/// `scale` 为整数放大倍数。供内核侧 overlay（性能统计等）在不加载
/// cn.ttf 的情况下绘制文本。
pub fn draw_text_8x8(s: &Surface, x: i32, y: i32, text: &str, color: Color, scale: u32) {
    let sc = scale.max(1) as i32;
    let mut cx = x;
    for &c in text.as_bytes() {
        let idx = (c as usize).wrapping_sub(0x20);
        let glyph = if idx < FONT8X8.len() { &FONT8X8[idx] } else { &FONT8X8[0] };
        for row in 0..8usize {
            for col in 0..8usize {
                if glyph[row] & (1 << (7 - col)) != 0 {
                    for sy in 0..sc {
                        for sx in 0..sc {
                            let px = cx + (col as i32) * sc + sx;
                            let py = y + (row as i32) * sc + sy;
                            if px >= 0 && py >= 0 && (px as u32) < s.w && (py as u32) < s.h {
                                s.set(px, py, color.pixel());
                            }
                        }
                    }
                }
            }
        }
        cx += 8 * sc;
    }
}

/// 8x8 点阵文本宽度（像素）。
pub fn text_width_8x8(text: &str, scale: u32) -> i32 {
    (text.len() as i32) * 8 * (scale.max(1) as i32)
}
