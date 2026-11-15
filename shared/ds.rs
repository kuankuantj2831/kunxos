//! KunxOS 统一设计系统（EL0 用户态，裸 syscall、no_std）。
//!
//! 各应用以 `#[path = "../../../shared/ds.rs"] mod ds;` 引入，再 `use ds::*;`
//! 使用统一设计令牌与组件：
//! - 页面背景：星光珍珠白（应用通过公共背景绘制入口渲染）
//! - 导航栏：56px，背景 #FFFFFF / #1C1C1E，底部 1px 分割线 #E5E5EA / #38383A
//! - 列表单元格：56px 高，左侧留白 16px，右侧箭头
//! - 图标：统一 24x24 系统图标库（`Icon` 枚举 + 几何绘制）
//! - 空状态：居中 64px 图标 + 17px 说明文字（#8E8E93）
//!
//! 所有组件均绘制进 app_kit 的 back buffer，渲染末尾由应用自行 `flip()`。

use crate::kit::*;

// ==================== 设计令牌 ====================

/// 导航栏高度（px）。
pub const NAV_H: i32 = 56;
/// 列表单元格高度（px）。
pub const CELL_H: i32 = 56;
/// 列表单元格左侧留白（px）。
pub const CELL_PAD: i32 = 16;
/// 统一图标尺寸（px）。
pub const ICON_SZ: i32 = 24;
/// 空状态图标尺寸（px）。
pub const EMPTY_ICON_SZ: i32 = 64;
/// 空状态说明文字字号（px）。
pub const EMPTY_TEXT_PX: f32 = 17.0;

pub const C_BG_LIGHT: u32 = 0x00_F2_F5_FF;
pub const C_BG_DARK: u32 = 0x00_00_00_00;
pub const C_NAV_LIGHT: u32 = 0x00_FF_FF_FF;
pub const C_NAV_DARK: u32 = 0x00_1C_1C_1E;
pub const C_DIV_LIGHT: u32 = 0x00_E5_E5_EA;
pub const C_DIV_DARK: u32 = 0x00_38_38_3A;
pub const C_TEXT_LIGHT: u32 = 0x00_00_00_00;
pub const C_TEXT_DARK: u32 = 0x00_FF_FF_FF;
/// 次要文字 / 空状态文字（深浅色通用）。
pub const C_DIM: u32 = 0x00_8E_8E_93;
/// 系统强调色。
pub const C_ACCENT: u32 = glass::starlight::ACCENT;

static mut DS_DARK: bool = false;

/// 初始化深浅色模式：全系统固定浅色（应用白色背景打底，忽略主题文件）。
pub fn ds_init() {
    unsafe { DS_DARK = false }
}

pub fn ds_dark() -> bool {
    unsafe { DS_DARK }
}

/// 页面背景。
pub fn ds_bg() -> u32 {
    if ds_dark() { C_BG_DARK } else { C_BG_LIGHT }
}

/// 导航栏背景。
pub fn ds_nav_bg() -> u32 {
    if ds_dark() { C_NAV_DARK } else { C_NAV_LIGHT }
}

/// 分割线颜色。
pub fn ds_divider() -> u32 {
    if ds_dark() { C_DIV_DARK } else { C_DIV_LIGHT }
}

/// 主文字颜色。
pub fn ds_text() -> u32 {
    if ds_dark() { C_TEXT_DARK } else { C_TEXT_LIGHT }
}

/// 列表单元格背景。
pub fn ds_cell_bg() -> u32 {
    if ds_dark() { C_NAV_DARK } else { C_NAV_LIGHT }
}

// ==================== 任意字号文本 ====================

/// 以指定字号绘制文本（y 为字形顶部）。
pub fn ds_text_px(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        let ch = core::char::from_u32(cp).unwrap_or(' ');
        if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
            let gx = cx + g.xmin;
            let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.data[row * g.width + col];
                    if a != 0 {
                        let px_ = gx + col as i32;
                        let py_ = gy + row as i32;
                        if px_ >= 0 && py_ >= 0 && px_ < display_w() as i32 && py_ < display_h() as i32 {
                            let bg = unsafe { BACK_BUF[py_ as usize * display_w() + px_ as usize] };
                            put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                        }
                    }
                }
            }
            cx += g.advance;
        } else {
            cx += if cp >= 0x80 { px as i32 } else { (px * 0.5) as i32 };
        }
        i += n;
    }
}

/// 指定字号下的文本宽度。
pub fn ds_text_width_px(s: &[u8], px: f32) -> i32 {
    ttf_text::font_render::text_width(s, px)
}

/// 以标题字体（bold.ttf）绘制文本（标题/导航栏标题等）。
pub fn ds_text_px_bold(x: i32, y: i32, s: &[u8], px: f32, color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        let ch = core::char::from_u32(cp).unwrap_or(' ');
        let bold_ok =
            ttf_text::font_render::font_bold_ready() && ttf_text::font_render::has_glyph_bold(cp);
        let g = if bold_ok {
            ttf_text::font_render::rasterize_char_bold(ch, px)
        } else {
            ttf_text::font_render::rasterize_char(ch, px)
        };
        if let Some(g) = g {
            let gx = cx + g.xmin;
            let gy = if bold_ok {
                ttf_text::font_render::glyph_draw_top_bold(y, g.ymin, g.height, px)
            } else {
                ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px)
            };
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.data[row * g.width + col];
                    if a != 0 {
                        let px_ = gx + col as i32;
                        let py_ = gy + row as i32;
                        if px_ >= 0 && py_ >= 0 && px_ < display_w() as i32 && py_ < display_h() as i32 {
                            let bg = unsafe { BACK_BUF[py_ as usize * display_w() + px_ as usize] };
                            put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                        }
                    }
                }
            }
            cx += g.advance;
        } else {
            cx += if cp >= 0x80 { px as i32 } else { (px * 0.5) as i32 };
        }
        i += n;
    }
}

/// 标题字体（bold.ttf）下的文本宽度。
pub fn ds_text_width_px_bold(s: &[u8], px: f32) -> i32 {
    ttf_text::font_render::text_width_bold(s, px)
}

/// 以 17px 标题字体绘制文本（导航栏标题等）。
pub fn ds_title_text(x: i32, y: i32, s: &[u8], color: u32) {
    ds_text_px_bold(x, y, s, 17.0, color);
}

// ==================== 导航栏 ====================

/// 绘制 56px 液态玻璃导航栏：模糊背景 + 可选玻璃返回键 + 居中标题 + 底部高光描边。
pub fn ds_nav_bar(title: &[u8], back: bool) {
    draw_glass(0, 0, display_w() as i32, NAV_H, 0);
    if back {
        draw_glass(8, (NAV_H - ICON_SZ - 8) / 2, ICON_SZ + 8, ICON_SZ + 8, 12);
        ds_icon(Icon::Back, 16, (NAV_H - ICON_SZ) / 2, ICON_SZ, C_ACCENT);
        let tw = ds_text_width_px_bold(title, 17.0);
        let tx = (display_w() as i32 - tw) / 2;
        ds_title_text(tx, (NAV_H - 20) / 2, title, ds_text());
    } else {
        let tw = ds_text_width_px_bold(title, 17.0);
        let tx = (display_w() as i32 - tw) / 2;
        ds_title_text(tx, (NAV_H - 20) / 2, title, ds_text());
    }
}

/// 导航栏返回键命中区域（左上 56x56）。
pub fn ds_back_hit(x: i32, y: i32) -> bool {
    x >= 0 && x < 56 && y >= 0 && y < NAV_H
}

// ==================== 列表单元格 ====================

/// 绘制 56px 液态玻璃列表卡片：8px 水平内缩 + 12px 圆角 + 可选前置图标 +
/// 标题/副标题 + 右侧箭头。`leading` 为 None 时不画图标；`sub` 为空时不画副标题；
/// `chevron` 控制右侧箭头。
pub fn ds_list_cell(
    x: i32,
    y: i32,
    w: i32,
    leading: Option<Icon>,
    title: &[u8],
    sub: &[u8],
    chevron: bool,
) {
    // 玻璃卡片（卡片之间自然留缝，替代旧的全宽单元格 + 分割线）。
    draw_glass(x + 8, y + 3, w - 16, CELL_H - 6, 14);
    let mut tx = x + CELL_PAD + 4;
    if let Some(ic) = leading {
        ds_icon(ic, tx, y + (CELL_H - ICON_SZ) / 2, ICON_SZ, C_ACCENT);
        tx += ICON_SZ + 12;
    }
    if sub.is_empty() {
        ds_text_px(tx, y + (CELL_H - 22) / 2, title, 17.0, ds_text());
    } else {
        ds_text_px(tx, y + 9, title, 17.0, ds_text());
        ds_text_px(tx, y + 33, sub, 13.0, C_DIM);
    }
    if chevron {
        ds_icon(Icon::Chevron, x + w - CELL_PAD - ICON_SZ - 8, y + (CELL_H - ICON_SZ) / 2, ICON_SZ, C_DIM);
    }
}

/// 单元格之间的分割线（玻璃卡片布局下不再需要，保留空实现兼容调用方）。
pub fn ds_cell_sep(_x: i32, _y: i32, _w: i32) {}

// ==================== 空状态 ====================

/// 绘制空状态：居中 64px 图标 + 下方 17px 说明文字（#8E8E93）。
pub fn ds_empty_state(ic: Icon, text: &[u8]) {
    let cx = (display_w() as i32 - EMPTY_ICON_SZ) / 2;
    let cy = (display_h() as i32 - EMPTY_ICON_SZ) / 2 - 40;
    ds_icon(ic, cx, cy, EMPTY_ICON_SZ, C_DIM);
    let tw = ds_text_width_px(text, EMPTY_TEXT_PX);
    ds_text_px(
        (display_w() as i32 - tw) / 2,
        cy + EMPTY_ICON_SZ + 18,
        text,
        EMPTY_TEXT_PX,
        C_DIM,
    );
}

// ==================== 按钮 ====================

/// 绘制液态玻璃按钮：圆角玻璃面板 + 居中标签；accent 时用强调色玻璃白字。
pub fn ds_button(x: i32, y: i32, w: i32, h: i32, label: &[u8], accent: bool) {
    let r = (h / 2).min(14);
    if accent {
        draw_glass_style(x, y, w, h, r, &STYLE_ACCENT);
    } else {
        draw_glass(x, y, w, h, r);
    }
    let color = if accent { C_TEXT_DARK } else { ds_text() };
    let tw = ds_text_width_px(label, 17.0);
    ds_text_px(x + (w - tw) / 2, y + (h - 20) / 2, label, 17.0, color);
}

/// 按钮命中。
pub fn ds_button_hit(x: i32, y: i32, bx: i32, by: i32, bw: i32, bh: i32) -> bool {
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

// ==================== 系统图标库（24x24 基础网格） ====================

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    /// 返回（左箭头）。
    Back,
    /// 右侧箭头（列表指示）。
    Chevron,
    /// 播放。
    Play,
    /// 暂停。
    Pause,
    /// 备忘录。
    Note,
    /// 删除。
    Trash,
    /// 搜索。
    Search,
    /// 新增。
    Plus,
    /// 视频。
    Video,
    /// 文件夹。
    Folder,
    /// 时钟。
    Clock,
    /// 音乐。
    Music,
    /// 邮件。
    Mail,
    /// 地图。
    Map,
    /// 相册。
    Photo,
    /// 电话。
    Phone,
    /// 短信。
    Sms,
    /// 设置。
    Settings,
    /// 天气。
    Weather,
    /// 日历。
    Calendar,
    /// 计算器。
    Calc,
    /// 云服务。
    Cloud,
}

/// 在 (x, y)（左上角）绘制 size x size 的系统图标。
pub fn ds_icon(ic: Icon, x: i32, y: i32, size: i32, color: u32) {
    match ic {
        Icon::Back => {
            dline(x + size * 16 / 24, y + size * 5 / 24, x + size * 8 / 24, y + size * 12 / 24, color);
            dline(x + size * 8 / 24, y + size * 12 / 24, x + size * 16 / 24, y + size * 19 / 24, color);
        }
        Icon::Chevron => {
            dline(x + size * 8 / 24, y + size * 6 / 24, x + size * 17 / 24, y + size * 12 / 24, color);
            dline(x + size * 8 / 24, y + size * 18 / 24, x + size * 17 / 24, y + size * 12 / 24, color);
        }
        Icon::Play => {
            tri(
                x + size * 6 / 24,
                y + size * 5 / 24,
                x + size * 20 / 24,
                y + size * 12 / 24,
                x + size * 6 / 24,
                y + size * 19 / 24,
                color,
            );
        }
        Icon::Pause => {
            fill_rect(x + size * 6 / 24, y + size * 5 / 24, size * 4 / 24, size * 14 / 24, color);
            fill_rect(x + size * 14 / 24, y + size * 5 / 24, size * 4 / 24, size * 14 / 24, color);
        }
        Icon::Note => {
            draw_rect(x + size * 5 / 24, y + size * 3 / 24, size * 14 / 24, size * 18 / 24, color);
            dline(x + size * 8 / 24, y + size * 8 / 24, x + size * 16 / 24, y + size * 8 / 24, color);
            dline(x + size * 8 / 24, y + size * 12 / 24, x + size * 16 / 24, y + size * 12 / 24, color);
            dline(x + size * 8 / 24, y + size * 16 / 24, x + size * 13 / 24, y + size * 16 / 24, color);
        }
        Icon::Trash => {
            dline(x + size * 4 / 24, y + size * 7 / 24, x + size * 20 / 24, y + size * 7 / 24, color);
            dline(x + size * 8 / 24, y + size * 7 / 24, x + size * 8 / 24, y + size * 5 / 24, color);
            dline(x + size * 16 / 24, y + size * 7 / 24, x + size * 16 / 24, y + size * 5 / 24, color);
            dline(x + size * 8 / 24, y + size * 5 / 24, x + size * 16 / 24, y + size * 5 / 24, color);
            draw_rect(x + size * 6 / 24, y + size * 9 / 24, size * 12 / 24, size * 11 / 24, color);
            dline(x + size * 9 / 24, y + size * 11 / 24, x + size * 9 / 24, y + size * 17 / 24, color);
            dline(x + size * 15 / 24, y + size * 11 / 24, x + size * 15 / 24, y + size * 17 / 24, color);
        }
        Icon::Search => {
            draw_circle(x + size * 10 / 24, y + size * 10 / 24, size * 5 / 24, color);
            dline(x + size * 14 / 24, y + size * 14 / 24, x + size * 19 / 24, y + size * 19 / 24, color);
        }
        Icon::Plus => {
            dline(x + size * 12 / 24, y + size * 4 / 24, x + size * 12 / 24, y + size * 20 / 24, color);
            dline(x + size * 4 / 24, y + size * 12 / 24, x + size * 20 / 24, y + size * 12 / 24, color);
        }
        Icon::Video => {
            draw_rect(x + size * 3 / 24, y + size * 5 / 24, size * 13 / 24, size * 14 / 24, color);
            tri(
                x + size * 14 / 24,
                y + size * 8 / 24,
                x + size * 21 / 24,
                y + size * 12 / 24,
                x + size * 14 / 24,
                y + size * 16 / 24,
                color,
            );
        }
        Icon::Folder => {
            dline(x + size * 3 / 24, y + size * 7 / 24, x + size * 3 / 24, y + size * 19 / 24, color);
            dline(x + size * 3 / 24, y + size * 19 / 24, x + size * 21 / 24, y + size * 19 / 24, color);
            dline(x + size * 21 / 24, y + size * 19 / 24, x + size * 21 / 24, y + size * 9 / 24, color);
            dline(x + size * 21 / 24, y + size * 9 / 24, x + size * 12 / 24, y + size * 9 / 24, color);
            dline(x + size * 12 / 24, y + size * 9 / 24, x + size * 9 / 24, y + size * 6 / 24, color);
            dline(x + size * 9 / 24, y + size * 6 / 24, x + size * 3 / 24, y + size * 6 / 24, color);
            dline(x + size * 3 / 24, y + size * 6 / 24, x + size * 3 / 24, y + size * 7 / 24, color);
        }
        Icon::Clock => {
            draw_circle(x + size * 12 / 24, y + size * 12 / 24, size * 8 / 24, color);
            dline(x + size * 12 / 24, y + size * 12 / 24, x + size * 12 / 24, y + size * 7 / 24, color);
            dline(x + size * 12 / 24, y + size * 12 / 24, x + size * 16 / 24, y + size * 13 / 24, color);
        }
        Icon::Music => {
            draw_disc(x + size * 8 / 24, y + size * 17 / 24, size * 3 / 24, color);
            draw_disc(x + size * 16 / 24, y + size * 15 / 24, size * 3 / 24, color);
            dline(x + size * 8 / 24, y + size * 17 / 24, x + size * 8 / 24, y + size * 7 / 24, color);
            dline(x + size * 8 / 24, y + size * 7 / 24, x + size * 16 / 24, y + size * 5 / 24, color);
            dline(x + size * 16 / 24, y + size * 5 / 24, x + size * 16 / 24, y + size * 15 / 24, color);
        }
        Icon::Mail => {
            draw_rect(x + size * 3 / 24, y + size * 6 / 24, size * 18 / 24, size * 12 / 24, color);
            dline(x + size * 3 / 24, y + size * 7 / 24, x + size * 12 / 24, y + size * 14 / 24, color);
            dline(x + size * 21 / 24, y + size * 7 / 24, x + size * 12 / 24, y + size * 14 / 24, color);
        }
        Icon::Map => {
            draw_disc(x + size * 12 / 24, y + size * 9 / 24, size * 4 / 24, color);
            tri(
                x + size * 12 / 24,
                y + size * 13 / 24,
                x + size * 8 / 24,
                y + size * 20 / 24,
                x + size * 16 / 24,
                y + size * 20 / 24,
                color,
            );
        }
        Icon::Photo => {
            draw_rect(x + size * 3 / 24, y + size * 4 / 24, size * 18 / 24, size * 16 / 24, color);
            draw_disc(x + size * 8 / 24, y + size * 9 / 24, size * 3 / 24, color);
            dline(x + size * 4 / 24, y + size * 18 / 24, x + size * 9 / 24, y + size * 12 / 24, color);
            dline(x + size * 9 / 24, y + size * 12 / 24, x + size * 14 / 24, y + size * 16 / 24, color);
            dline(x + size * 14 / 24, y + size * 16 / 24, x + size * 20 / 24, y + size * 10 / 24, color);
        }
        Icon::Phone => {
            draw_rect(x + size * 7 / 24, y + size * 3 / 24, size * 10 / 24, size * 18 / 24, color);
            dline(x + size * 9 / 24, y + size * 6 / 24, x + size * 15 / 24, y + size * 6 / 24, color);
            dline(x + size * 9 / 24, y + size * 9 / 24, x + size * 15 / 24, y + size * 9 / 24, color);
            dline(x + size * 9 / 24, y + size * 15 / 24, x + size * 15 / 24, y + size * 15 / 24, color);
            dline(x + size * 9 / 24, y + size * 18 / 24, x + size * 15 / 24, y + size * 18 / 24, color);
        }
        Icon::Sms => {
            draw_rect(x + size * 3 / 24, y + size * 4 / 24, size * 18 / 24, size * 12 / 24, color);
            tri(
                x + size * 6 / 24,
                y + size * 16 / 24,
                x + size * 6 / 24,
                y + size * 20 / 24,
                x + size * 11 / 24,
                y + size * 16 / 24,
                color,
            );
            draw_disc(x + size * 8 / 24, y + size * 10 / 24, size * 1 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 10 / 24, size * 1 / 24, color);
            draw_disc(x + size * 16 / 24, y + size * 10 / 24, size * 1 / 24, color);
        }
        Icon::Settings => {
            draw_circle(x + size * 12 / 24, y + size * 12 / 24, size * 8 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 12 / 24, size * 3 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 5 / 24, size * 2 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 19 / 24, size * 2 / 24, color);
            draw_disc(x + size * 5 / 24, y + size * 12 / 24, size * 2 / 24, color);
            draw_disc(x + size * 19 / 24, y + size * 12 / 24, size * 2 / 24, color);
        }
        Icon::Weather => {
            draw_disc(x + size * 12 / 24, y + size * 12 / 24, size * 5 / 24, color);
            dline(x + size * 12 / 24, y + size * 2 / 24, x + size * 12 / 24, y + size * 5 / 24, color);
            dline(x + size * 12 / 24, y + size * 19 / 24, x + size * 12 / 24, y + size * 22 / 24, color);
            dline(x + size * 2 / 24, y + size * 12 / 24, x + size * 5 / 24, y + size * 12 / 24, color);
            dline(x + size * 19 / 24, y + size * 12 / 24, x + size * 22 / 24, y + size * 12 / 24, color);
            dline(x + size * 5 / 24, y + size * 5 / 24, x + size * 7 / 24, y + size * 7 / 24, color);
            dline(x + size * 19 / 24, y + size * 5 / 24, x + size * 17 / 24, y + size * 7 / 24, color);
            dline(x + size * 5 / 24, y + size * 19 / 24, x + size * 7 / 24, y + size * 17 / 24, color);
            dline(x + size * 19 / 24, y + size * 19 / 24, x + size * 17 / 24, y + size * 17 / 24, color);
        }
        Icon::Calendar => {
            draw_rect(x + size * 4 / 24, y + size * 4 / 24, size * 16 / 24, size * 16 / 24, color);
            dline(x + size * 4 / 24, y + size * 8 / 24, x + size * 20 / 24, y + size * 8 / 24, color);
            dline(x + size * 8 / 24, y + size * 2 / 24, x + size * 8 / 24, y + size * 6 / 24, color);
            dline(x + size * 16 / 24, y + size * 2 / 24, x + size * 16 / 24, y + size * 6 / 24, color);
            dline(x + size * 8 / 24, y + size * 12 / 24, x + size * 8 / 24, y + size * 12 / 24, color);
            dline(x + size * 12 / 24, y + size * 12 / 24, x + size * 12 / 24, y + size * 12 / 24, color);
            dline(x + size * 16 / 24, y + size * 12 / 24, x + size * 16 / 24, y + size * 12 / 24, color);
            dline(x + size * 8 / 24, y + size * 16 / 24, x + size * 8 / 24, y + size * 16 / 24, color);
            dline(x + size * 12 / 24, y + size * 16 / 24, x + size * 12 / 24, y + size * 16 / 24, color);
            dline(x + size * 16 / 24, y + size * 16 / 24, x + size * 16 / 24, y + size * 16 / 24, color);
        }
        Icon::Calc => {
            draw_rect(x + size * 5 / 24, y + size * 3 / 24, size * 14 / 24, size * 18 / 24, color);
            dline(x + size * 5 / 24, y + size * 8 / 24, x + size * 19 / 24, y + size * 8 / 24, color);
            draw_disc(x + size * 8 / 24, y + size * 12 / 24, size * 1 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 12 / 24, size * 1 / 24, color);
            draw_disc(x + size * 16 / 24, y + size * 12 / 24, size * 1 / 24, color);
            draw_disc(x + size * 8 / 24, y + size * 16 / 24, size * 1 / 24, color);
            draw_disc(x + size * 12 / 24, y + size * 16 / 24, size * 1 / 24, color);
            draw_disc(x + size * 16 / 24, y + size * 16 / 24, size * 1 / 24, color);
        }
        Icon::Cloud => {
            draw_disc(x + size * 8 / 24, y + size * 15 / 24, size * 5 / 24, color);
            draw_disc(x + size * 13 / 24, y + size * 12 / 24, size * 6 / 24, color);
            draw_disc(x + size * 18 / 24, y + size * 15 / 24, size * 4 / 24, color);
            fill_rect(x + size * 7 / 24, y + size * 15 / 24, size * 11 / 24, size * 4 / 24, color);
        }
    }
}

/// 实心三角形（扫描线填充，按 y 排序后逐行取两条边夹区间）。
fn tri(x0: i32, y0: i32, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
    let mut pts = [(x0, y0), (x1, y1), (x2, y2)];
    pts.sort_by_key(|p| p.1);
    let (ax, ay) = pts[0];
    let (bx, by) = pts[1];
    let (cx, cy) = pts[2];
    let mut y = ay;
    while y <= cy {
        let t_ac = if cy > ay { (y - ay) as f32 / (cy - ay) as f32 } else { 0.0 };
        let x_ac = ax as f32 + (cx - ax) as f32 * t_ac;
        let x_other = if y <= by && by > ay {
            let t_ab = (y - ay) as f32 / (by - ay) as f32;
            ax as f32 + (bx - ax) as f32 * t_ab
        } else if y > by && cy > by {
            let t_bc = (y - by) as f32 / (cy - by) as f32;
            bx as f32 + (cx - bx) as f32 * t_bc
        } else {
            x_ac
        };
        let x_lo = x_ac.min(x_other) as i32;
        let x_hi = x_ac.max(x_other) as i32;
        for px in x_lo..=x_hi {
            put_pixel(px, y, color);
        }
        y += 1;
    }
}

/// 直线（Bresenham）。
fn dline(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let mut x0 = x0;
    let mut y0 = y0;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_pixel(x0, y0, color);
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
