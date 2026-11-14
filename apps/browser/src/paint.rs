//! 渲染管线：绘制列表生成（布局产物 → Cmd 列表）+ CPU 光栅化。
//!
//! 布局完成后一次性生成命令列表（无逐帧分配）；滚动/重绘只重放命令，
//! 按视口裁剪（文档坐标 → 屏幕坐标，减去 scroll_y），仅绘制可见区域。

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::html::Dom;
use crate::kit::{blend_put, put_pixel};
use crate::kit::ttf_text::font_render;
use crate::layout::{BKind, Layout, ReplacedKind};

#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    /// 填充矩形（背景）
    Fill { x: i32, y: i32, w: i32, h: i32, color: u32 },
    /// 边框矩形（每边宽度与颜色）
    Border {
        x: i32, y: i32, w: i32, h: i32,
        t: i32, r: i32, b: i32, l: i32,
        ct: u32, cr: u32, cb: u32, cl: u32,
    },
    /// 文本运行（引用 DOM 文本节点字节区间）
    Text {
        x: i32, baseline: i32, px: f32, color: u32,
        bold: bool, underline: bool,
        node: usize, start: usize, end: usize,
    },
    /// 列表项圆点
    Bullet { x: i32, y: i32, color: u32 },
    /// 图片（img 下标 → 页面图片数据）
    Image { x: i32, y: i32, w: i32, h: i32, img: usize },
    /// 文本输入框（值来自表单状态）
    Input { x: i32, y: i32, w: i32, h: i32, ctrl: usize },
    /// 复选框 / 单选钮（状态取自表单运行时）
    Checkbox { x: i32, y: i32, ctrl: usize },
    Radio { x: i32, y: i32, ctrl: usize },
    /// 按钮（标签取自 DOM）
    Button { x: i32, y: i32, w: i32, h: i32, node: usize },
}

/// 由布局结果生成绘制命令列表（按文档顺序）。
pub fn build_paint_list(_dom: &Dom, lt: &Layout) -> Vec<Cmd> {
    let mut cmds: Vec<Cmd> = Vec::new();
    // 盒树背景/边框/替换元素
    for i in 0..lt.boxes.len() {
        let bx = &lt.boxes[i];
        let st = &bx.style;
        match bx.kind {
            BKind::Block | BKind::Table | BKind::TableRow | BKind::TableCell => {
                if let Some(bg) = st.background {
                    cmds.push(Cmd::Fill {
                        x: bx.rect.x as i32,
                        y: bx.rect.y as i32,
                        w: bx.rect.w.max(0.0) as i32,
                        h: bx.rect.h.max(0.0) as i32,
                        color: bg,
                    });
                }
                if st.border() {
                    cmds.push(border_cmd(bx.rect, st));
                }
            }
            BKind::Replaced => {
                match bx.replaced {
                    Some(ReplacedKind::Img) => {
                        if let Some(img) = bx.img {
                            cmds.push(Cmd::Image {
                                x: bx.rect.x as i32,
                                y: bx.rect.y as i32,
                                w: bx.rect.w as i32,
                                h: bx.rect.h as i32,
                                img,
                            });
                        }
                    }
                    Some(ReplacedKind::TextInput) => {
                        if let Some(c) = bx.ctrl {
                            cmds.push(Cmd::Input {
                                x: bx.rect.x as i32,
                                y: bx.rect.y as i32,
                                w: bx.rect.w as i32,
                                h: bx.rect.h as i32,
                                ctrl: c,
                            });
                        }
                    }
                    Some(ReplacedKind::Checkbox) => {
                        if let Some(c) = bx.ctrl {
                            cmds.push(Cmd::Checkbox { x: bx.rect.x as i32, y: bx.rect.y as i32, ctrl: c });
                        }
                    }
                    Some(ReplacedKind::Radio) => {
                        if let Some(c) = bx.ctrl {
                            cmds.push(Cmd::Radio { x: bx.rect.x as i32, y: bx.rect.y as i32, ctrl: c });
                        }
                    }
                    Some(ReplacedKind::Button) | Some(ReplacedKind::SubmitButton) => {
                        if let Some(node) = bx.node {
                            cmds.push(Cmd::Button {
                                x: bx.rect.x as i32,
                                y: bx.rect.y as i32,
                                w: bx.rect.w as i32,
                                h: bx.rect.h as i32,
                                node,
                            });
                        }
                    }
                    _ => {}
                }
            }
            BKind::Inline => {}
        }
        // 列表项圆点
        if st.is_list_item {
            cmds.push(Cmd::Bullet {
                x: (bx.rect.x - 14.0) as i32,
                y: (bx.rect.y + 8.0) as i32,
                color: st.color,
            });
        }
    }
    // 行内背景/边框（按行合并）
    for bg in &lt.bgs {
        if bg.color != 0 {
            cmds.push(Cmd::Fill {
                x: bg.rect.x as i32,
                y: bg.rect.y as i32,
                w: bg.rect.w as i32,
                h: bg.rect.h as i32,
                color: bg.color,
            });
        }
        if bg.border_w[0] > 0.0 || bg.border_w[1] > 0.0 || bg.border_w[2] > 0.0 || bg.border_w[3] > 0.0 {
            cmds.push(Cmd::Border {
                x: bg.rect.x as i32,
                y: bg.rect.y as i32,
                w: bg.rect.w as i32,
                h: bg.rect.h as i32,
                t: bg.border_w[0] as i32,
                r: bg.border_w[1] as i32,
                b: bg.border_w[2] as i32,
                l: bg.border_w[3] as i32,
                ct: bg.border_color[0],
                cr: bg.border_color[1],
                cb: bg.border_color[2],
                cl: bg.border_color[3],
            });
        }
    }
    // 文本运行
    for r in &lt.runs {
        cmds.push(Cmd::Text {
            x: r.x as i32,
            baseline: r.baseline as i32,
            px: r.px,
            color: r.color,
            bold: r.bold,
            underline: r.underline,
            node: r.node,
            start: r.start,
            end: r.end,
        });
    }
    cmds
}

fn border_cmd(r: crate::layout::Rect, st: &crate::style::ComputedStyle) -> Cmd {
    Cmd::Border {
        x: r.x as i32,
        y: r.y as i32,
        w: r.w as i32,
        h: r.h as i32,
        t: st.border_w[0] as i32,
        r: st.border_w[1] as i32,
        b: st.border_w[2] as i32,
        l: st.border_w[3] as i32,
        ct: st.border_color[0],
        cr: st.border_color[1],
        cb: st.border_color[2],
        cl: st.border_color[3],
    }
}

// ==================== 光栅化 ====================

/// 表单运行时状态（shell 维护，绘制只读）。
pub struct FormView<'a> {
    pub values: &'a [Vec<u8>],
    pub checked: &'a [bool],
    pub focus: Option<usize>,
    pub cursor_on: bool,
}

/// 页面图片数据（img 下标 → RGB 字节）。
pub struct ImageStore<'a> {
    pub data: &'a [Option<Vec<u8>>],
}

pub struct PaintCtx<'a> {
    pub dom: &'a Dom,
    pub images: &'a ImageStore<'a>,
    pub form: &'a FormView<'a>,
    /// 滚动偏移（文档 → 屏幕）。
    pub scroll_y: i32,
    /// 视口裁剪（屏幕坐标，含边界）。
    pub clip_x: i32,
    pub clip_y: i32,
    pub clip_w: i32,
    pub clip_h: i32,
}

fn visible(x: i32, y: i32, w: i32, h: i32, ctx: &PaintCtx) -> bool {
    // i64 中间量：坐标/尺寸已达上限时 x+w 的 i32 加法回绕会误判可见性。
    let (x, y) = (x as i64, y as i64);
    let (w, h) = (w as i64, h as i64);
    x + w > ctx.clip_x as i64
        && x < (ctx.clip_x + ctx.clip_w) as i64
        && y + h > ctx.clip_y as i64
        && y < (ctx.clip_y + ctx.clip_h) as i64
}

/// 执行命令列表（文档坐标 → 屏幕坐标）。
pub fn execute(cmds: &[Cmd], ctx: &PaintCtx) {
    let sy = ctx.scroll_y;
    for c in cmds {
        match c {
            Cmd::Fill { x, y, w, h, color } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, *w, *h, ctx) {
                    continue;
                }
                fill_rect_clipped(sx, syy, *w, *h, *color, ctx);
            }
            Cmd::Border { x, y, w, h, t, r, b, l, ct, cr, cb, cl } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, *w, *h, ctx) {
                    continue;
                }
                draw_border(sx, syy, *w, *h, *t, *r, *b, *l, [*ct, *cr, *cb, *cl], ctx);
            }
            Cmd::Text { x, baseline, px, color, bold, underline, node, start, end } => {
                let sb = *baseline - sy;
                // 估算文本高度用于裁剪
                let (a, _d) = font_render::line_metrics(*px);
                if sb + a < ctx.clip_y || sb - a > ctx.clip_y + ctx.clip_h {
                    continue;
                }
                let mut cx = *x;
                let text = ctx.dom.nodes[*node].text().as_bytes();
                let end = (*end).min(text.len());
                let mut i = *start;
                while i < end {
                    let (cp, n) = font_render::utf8_next(text, i);
                    if cp == 0 {
                        i += 1;
                        continue;
                    }
                    let adv = draw_glyph(cx, sb, cp, *px, *color, *bold, ctx);
                    if *underline {
                        let uw = adv.max(1);
                        fill_rect_clipped(cx, sb + 3, uw, 1, *color, ctx);
                    }
                    cx += adv;
                    i += n;
                }
            }
            Cmd::Bullet { x, y, color } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, 8, 8, ctx) {
                    continue;
                }
                draw_disc_clipped(sx, syy, 3, *color, ctx);
            }
            Cmd::Image { x, y, w, h, img } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, *w, *h, ctx) {
                    continue;
                }
                draw_image(sx, syy, *w, *h, *img, ctx);
            }
            Cmd::Input { x, y, w, h, ctrl } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, *w, *h, ctx) {
                    continue;
                }
                draw_input(sx, syy, *w, *h, *ctrl, ctx);
            }
            Cmd::Checkbox { x, y, ctrl } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, 18, 18, ctx) {
                    continue;
                }
                let checked = ctx.form.checked.get(*ctrl).copied().unwrap_or(false);
                draw_box_control(sx, syy, checked, false, ctx);
            }
            Cmd::Radio { x, y, ctrl } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, 18, 18, ctx) {
                    continue;
                }
                let checked = ctx.form.checked.get(*ctrl).copied().unwrap_or(false);
                draw_box_control(sx, syy, checked, true, ctx);
            }
            Cmd::Button { x, y, w, h, node } => {
                let (sx, syy) = (*x, *y - sy);
                if !visible(sx, syy, *w, *h, ctx) {
                    continue;
                }
                draw_button(sx, syy, *w, *h, *node, ctx);
            }
        }
    }
}

/// 裁剪填充。
fn fill_rect_clipped(x: i32, y: i32, w: i32, h: i32, color: u32, ctx: &PaintCtx) {
    let x0 = x.max(ctx.clip_x);
    let y0 = y.max(ctx.clip_y);
    let x1 = (x + w).min(ctx.clip_x + ctx.clip_w);
    let y1 = (y + h).min(ctx.clip_y + ctx.clip_h);
    for py in y0..y1 {
        for px in x0..x1 {
            put_pixel(px, py, color);
        }
    }
}

fn draw_border(
    x: i32, y: i32, w: i32, h: i32,
    t: i32, r: i32, b: i32, l: i32,
    cols: [u32; 4],
    ctx: &PaintCtx,
) {
    if t > 0 {
        fill_rect_clipped(x, y, w, t, cols[0], ctx);
    }
    if b > 0 {
        fill_rect_clipped(x, y + h - b, w, b, cols[2], ctx);
    }
    if l > 0 {
        fill_rect_clipped(x, y + t, l, (h - t - b).max(0), cols[3], ctx);
    }
    if r > 0 {
        fill_rect_clipped(x + w - r, y + t, r, (h - t - b).max(0), cols[1], ctx);
    }
}

fn draw_disc_clipped(cx: i32, cy: i32, rad: i32, color: u32, ctx: &PaintCtx) {
    if rad <= 0 {
        return;
    }
    let r2 = rad as f32 * rad as f32;
    for dy in -rad..=rad {
        for dx in -rad..=rad {
            let (px, py) = (cx + dx, cy + dy);
            if px < ctx.clip_x
                || px >= ctx.clip_x + ctx.clip_w
                || py < ctx.clip_y
                || py >= ctx.clip_y + ctx.clip_h
            {
                continue;
            }
            // 4x4 超采样覆盖度（小半径圆点，代价可忽略；抗锯齿边缘）。
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let ox = dx as f32 - 0.5 + (sx as f32 + 0.5) * 0.25;
                    let oy = dy as f32 - 0.5 + (sy as f32 + 0.5) * 0.25;
                    if ox * ox + oy * oy <= r2 {
                        hits += 1;
                    }
                }
            }
            let a = hits * 255 / 16;
            if a >= 255 {
                put_pixel(px, py, color);
            } else if a > 0 {
                blend_put(px, py, color, a);
            }
        }
    }
}

/// 绘制单个字形（灰度混合，粗体二次绘制）。
fn draw_glyph(x: i32, baseline: i32, cp: u32, px: f32, color: u32, bold: bool, ctx: &PaintCtx) -> i32 {
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if !font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let Some(g) = font_render::rasterize_char(ch, px) else {
        return if cp >= 0x80 { 16 } else { 8 };
    };
    let gx = x + g.xmin;
    // 位图顶行：baseline + ymin - height + 1
    let top = baseline + g.ymin - (g.height as i32) + 1;
    for row in 0..g.height {
        let py = top + row as i32;
        if py < ctx.clip_y || py >= ctx.clip_y + ctx.clip_h {
            continue;
        }
        for col in 0..g.width {
            let a = g.data[row * g.width + col];
            if a == 0 {
                continue;
            }
            let px_ = gx + col as i32;
            if px_ < ctx.clip_x || px_ >= ctx.clip_x + ctx.clip_w {
                continue;
            }
            let bg = unsafe { crate::kit::BACK_BUF[py as usize * crate::kit::display_w() + px_ as usize] };
            put_pixel(px_, py, font_render::blend(bg, color, a));
            if bold && px_ + 1 < ctx.clip_x + ctx.clip_w {
                let bg2 = unsafe { crate::kit::BACK_BUF[py as usize * crate::kit::display_w() + (px_ + 1) as usize] };
                put_pixel(px_ + 1, py, font_render::blend(bg2, color, a));
            }
        }
    }
    g.advance
}

/// 图片绘制（最近邻缩放）。
fn draw_image(x: i32, y: i32, w: i32, h: i32, img: usize, ctx: &PaintCtx) {
    // 占位底
    fill_rect_clipped(x, y, w, h, 0x00_1E_2A_3E, ctx);
    let Some(data) = ctx.images.data.get(img).and_then(|d| d.as_ref()) else {
        // 未加载：显示占位框 + 边框
        draw_border(x, y, w, h, 1, 1, 1, 1, [0x00_4A_5A_7A; 4], ctx);
        return;
    };
    if data.len() < 8 {
        return;
    }
    let iw = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let ih = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
    if iw == 0 || ih == 0 || data.len() < 8 + iw * ih * 3 {
        return;
    }
    let px = &data[8..];
    for dy in 0..h {
        let py = y + dy;
        if py < ctx.clip_y || py >= ctx.clip_y + ctx.clip_h {
            continue;
        }
        let sy = (dy as u64 * ih as u64 / h as u64) as usize;
        for dx in 0..w {
            let px_ = x + dx;
            if px_ < ctx.clip_x || px_ >= ctx.clip_x + ctx.clip_w {
                continue;
            }
            let sx = (dx as u64 * iw as u64 / w as u64) as usize;
            let o = (sy * iw + sx) * 3;
            let r = px[o] as u32;
            let g = px[o + 1] as u32;
            let b = px[o + 2] as u32;
            put_pixel(px_, py, (r << 16) | (g << 8) | b);
        }
    }
}

/// 文本框绘制（值 + 光标）。
fn draw_input(x: i32, y: i32, w: i32, h: i32, ctrl: usize, ctx: &PaintCtx) {
    // 背景 + 边框
    fill_rect_clipped(x, y, w, h, 0x00_FF_FF_FF, ctx);
    let focused = ctx.form.focus == Some(ctrl);
    let border = if focused { 0x00_00_B9_6B } else { 0x00_99_99_99 };
    draw_border(x, y, w, h, 1, 1, 1, 1, [border; 4], ctx);
    let value = ctx.form.values.get(ctrl).map(|v| v.as_slice()).unwrap_or(&[]);
    let mut cx = x + 6;
    let base = y + (h + 6) / 2; // 近似基线
    let mut i = 0;
    while i < value.len() {
        let (cp, n) = font_render::utf8_next(value, i);
        if cp == 0 {
            i += 1;
            continue;
        }
        let adv = draw_glyph_c(cx, base, cp, 16.0, 0x00_00_00_00, false, ctx);
        cx += adv;
        i += n;
    }
    if focused && ctx.form.cursor_on && cx < x + w - 4 {
        fill_rect_clipped(cx, y + 5, 2, h - 10, 0x00_00_00_00, ctx);
    }
}

fn draw_glyph_c(x: i32, baseline: i32, cp: u32, px: f32, color: u32, bold: bool, ctx: &PaintCtx) -> i32 {
    draw_glyph(x, baseline, cp, px, color, bold, ctx)
}

/// 复选框 / 单选钮。
fn draw_box_control(x: i32, y: i32, checked: bool, radio: bool, ctx: &PaintCtx) {
    fill_rect_clipped(x, y, 18, 18, 0x00_FF_FF_FF, ctx);
    if radio {
        draw_disc_clipped(x + 9, y + 9, 9, 0x00_99_99_99, ctx);
        draw_disc_clipped(x + 9, y + 9, 7, 0x00_FF_FF_FF, ctx);
        if checked {
            draw_disc_clipped(x + 9, y + 9, 5, 0x00_00_B9_6B, ctx);
        }
    } else {
        draw_border(x, y, 18, 18, 1, 1, 1, 1, [0x00_99_99_99; 4], ctx);
        if checked {
            // 对勾
            for i in 0..6 {
                put_pixel(x + 3 + i, y + 6 + i, 0x00_00_B9_6B);
            }
            for i in 0..6 {
                put_pixel(x + 8 + i, y + 11 - i, 0x00_00_B9_6B);
            }
        }
    }
}

/// 按钮绘制。
fn draw_button(x: i32, y: i32, w: i32, h: i32, node: usize, ctx: &PaintCtx) {
    fill_rect_clipped(x, y, w, h, 0x00_E4_E7_EC, ctx);
    draw_border(x, y, w, h, 1, 1, 1, 1, [0x00_99_99_99; 4], ctx);
    // 标签：button 元素取聚合文本（Node::text() 对元素恒为 ""），
    // input[type=submit] 取 value
    let nd = &ctx.dom.nodes[node];
    let btn_text: Option<String> = if nd.tag() == "button" {
        Some(ctx.dom.inner_text(node))
    } else {
        None
    };
    let label: &[u8] = match &btn_text {
        Some(s) => s.as_bytes(),
        None => nd.attr("value").unwrap_or("提交").as_bytes(),
    };
    let lw = font_render::text_width(label, 16.0);
    let mut cx = x + (w - lw) / 2;
    let base = y + (h + 6) / 2;
    let mut i = 0;
    while i < label.len() {
        let (cp, n) = font_render::utf8_next(label, i);
        if cp == 0 {
            i += 1;
            continue;
        }
        cx += draw_glyph_c(cx, base, cp, 16.0, 0x00_1F_29_37, false, ctx);
        i += n;
    }
}
