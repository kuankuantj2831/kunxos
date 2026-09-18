//! UI 组件：图标表、颜色、液态玻璃面板、标题栏、列表行、开关、滑块、
//! 单选行、步进行、按钮、渐变背景。

use super::*;
use crate::sys_nav::glass::{GlassStyle, GlassTarget, STYLE_ACCENT, STYLE_DANGER, glass_rect, glass_rect_style};

// ==================== 品牌色（分类图标底色，深浅主题下固定） ====================

pub(crate) const IC_NET: u32 = 0x00_00_B9_6B;
pub(crate) const IC_BT: u32 = 0x00_00_7A_FF;
pub(crate) const IC_DISP: u32 = 0x00_FF_95_00;
pub(crate) const IC_SOUND: u32 = 0x00_FF_3B_30;
pub(crate) const IC_NOTIF: u32 = 0x00_FF_2D_55;
pub(crate) const IC_BATT: u32 = 0x00_FF_CC_00;
pub(crate) const IC_PRIV: u32 = 0x00_58_56_D6;
pub(crate) const IC_ACCT: u32 = 0x00_00_C7_BE;
pub(crate) const IC_SYS: u32 = 0x00_8E_8E_93;
pub(crate) const IC_STOR: u32 = 0x00_AF_52_DE;
pub(crate) const IC_ACC: u32 = 0x00_00_B9_6B;
pub(crate) const IC_UPD: u32 = 0x00_AF_52_DE;
pub(crate) const IC_PHONE: u32 = 0x00_5A_C8_FA;
pub(crate) const IC_CLOCK: u32 = 0x00_5A_C8_FA;
pub(crate) const IC_LANG: u32 = 0x00_AF_52_DE;
pub(crate) const IC_KBD: u32 = 0x00_58_56_D6;
pub(crate) const IC_USER: u32 = 0x00_FF_95_00;
pub(crate) const IC_CLOUD: u32 = 0x00_00_7A_FF;
pub(crate) const IC_SYNC: u32 = 0x00_00_B9_6B;
pub(crate) const IC_PAT: u32 = 0x00_58_56_D6;
pub(crate) const IC_INFO: u32 = 0x00_8E_8E_93;
pub(crate) const IC_RESET: u32 = 0x00_FF_3B_30;
pub(crate) const IC_GRAY: u32 = 0x00_8E_8E_93;

// ==================== 8x8 位图字形 ====================

static FONT_LT: [u8; 8] = [0x08, 0x10, 0x20, 0x40, 0x20, 0x10, 0x08, 0x00];
static FONT_GT: [u8; 8] = [0x20, 0x10, 0x08, 0x04, 0x08, 0x10, 0x20, 0x00];
static FONT_A: [u8; 8] = [0x18, 0x24, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00];
pub(crate) static FONT_X: [u8; 8] = [
    0b10000001, 0b01000010, 0b00100100, 0b00011000,
    0b00011000, 0b00100100, 0b01000010, 0b10000001,
];

static ICON_WIFI: [u8; 8] = [
    0b00111100, 0b01000010, 0b00011000, 0b00100100,
    0b00100100, 0b00100100, 0b00001000, 0b00010000,
];
static ICON_MOBILE: [u8; 8] = [
    0b00000000, 0b00010000, 0b00010000, 0b00110000,
    0b00110000, 0b01110000, 0b01110000, 0b11110000,
];
static ICON_VPN: [u8; 8] = [
    0b00111000, 0b01001000, 0b01001000, 0b00111000,
    0b00010000, 0b00010000, 0b00110000, 0b00110000,
];
static ICON_BT: [u8; 8] = [
    0b01100000, 0b10010000, 0b10001000, 0b01001000,
    0b00110000, 0b01001000, 0b10001000, 0b10010000,
];
static ICON_SUN: [u8; 8] = [
    0b00010000, 0b00111000, 0b01111100, 0b10111010,
    0b10111010, 0b01111100, 0b00111000, 0b00010000,
];
static ICON_MOON: [u8; 8] = [
    0b00111000, 0b01111100, 0b01110000, 0b01100000,
    0b01100000, 0b01110000, 0b01111100, 0b00111000,
];
static ICON_SPEAK: [u8; 8] = [
    0b00010010, 0b00110100, 0b01111100, 0b01111110,
    0b01111110, 0b01111100, 0b00110100, 0b00010010,
];
static ICON_BELL: [u8; 8] = [
    0b00111000, 0b01111100, 0b10000010, 0b10000010,
    0b10000010, 0b01111100, 0b00010000, 0b00111000,
];
static ICON_BATT: [u8; 8] = [
    0b00000001, 0b01111110, 0b10000001, 0b10111001,
    0b10111001, 0b10000001, 0b01111110, 0b00000000,
];
static ICON_DISK: [u8; 8] = [
    0b00000000, 0b01111110, 0b10000001, 0b01111110,
    0b01111110, 0b10000001, 0b01111110, 0b00000000,
];
static ICON_FINGER: [u8; 8] = [
    0b00111100, 0b01000010, 0b10011001, 0b10100101,
    0b01011010, 0b10011001, 0b01000010, 0b00111100,
];
static ICON_LOCK: [u8; 8] = [
    0b01111110, 0b10000001, 0b10000001, 0b01000010,
    0b00111100, 0b00111100, 0b00111100, 0b00111100,
];
static ICON_UPDATE: [u8; 8] = [
    0b00011100, 0b00100010, 0b01000010, 0b00111110,
    0b11111000, 0b10000010, 0b10000100, 0b00111000,
];
static ICON_PHONE: [u8; 8] = [
    0b01111100, 0b10000010, 0b10000010, 0b10000010,
    0b10000010, 0b10000010, 0b01111100, 0b00010000,
];
static ICON_NAV: [u8; 8] = [
    0b00000000, 0b00000000, 0b00000000, 0b01111110,
    0b01111110, 0b00000000, 0b00000000, 0b00000000,
];
static ICON_APPS: [u8; 8] = [
    0b00000000, 0b01100110, 0b01100110, 0b00000000,
    0b01100110, 0b01100110, 0b00000000, 0b00000000,
];
static ICON_RESET: [u8; 8] = [
    0b00111100, 0b01000010, 0b10000001, 0b10000100,
    0b10001000, 0b00111000, 0b00000000, 0b00000000,
];
static ICON_USER: [u8; 8] = [
    0b00111000, 0b01000100, 0b01000100, 0b00111000,
    0b00010000, 0b01010000, 0b00110000, 0b00001000,
];
static ICON_CLOUD: [u8; 8] = [
    0b00000000, 0b00111100, 0b01111110, 0b11111111,
    0b11111111, 0b01111110, 0b00000000, 0b00000000,
];
static ICON_SYNC: [u8; 8] = [
    0b00011000, 0b00100000, 0b01100000, 0b00011000,
    0b00011000, 0b00000110, 0b00000100, 0b00011000,
];
static ICON_CLOCK: [u8; 8] = [
    0b00111100, 0b01000010, 0b01000010, 0b01011010,
    0b01011010, 0b01000010, 0b01000010, 0b00111100,
];
static ICON_LANG: [u8; 8] = [
    0b01111110, 0b10000001, 0b10000001, 0b00111100,
    0b00100100, 0b00100100, 0b00011000, 0b00000000,
];
static ICON_KBD: [u8; 8] = [
    0b01111110, 0b10000001, 0b10000001, 0b10000001,
    0b10000001, 0b01011010, 0b01111110, 0b00000000,
];
static ICON_PAT: [u8; 8] = [
    0b00010000, 0b00000000, 0b00010000, 0b00000000,
    0b00101000, 0b00000000, 0b00010000, 0b00000000,
];
static ICON_INFO: [u8; 8] = [
    0b01111100, 0b10000010, 0b10000010, 0b10000010,
    0b10010010, 0b10000010, 0b01000100, 0b00111000,
];
static ICON_ACC: [u8; 8] = [
    0b00010000, 0b00101000, 0b00010000, 0b01000100,
    0b01000100, 0b00101000, 0b00010000, 0b00000000,
];
static ICON_EYE: [u8; 8] = [
    0b00000000, 0b00111000, 0b00101000, 0b01101100,
    0b01101100, 0b00101000, 0b00111000, 0b00000000,
];
static ICON_GEAR: [u8; 8] = [
    0b00111000, 0b01010100, 0b10111110, 0b10000010,
    0b10000010, 0b10111110, 0b01010100, 0b00111000,
];
static ICON_SAVE: [u8; 8] = [
    0b01111110, 0b10000001, 0b10000001, 0b10000001,
    0b11111111, 0b10000001, 0b10000001, 0b01111110,
];
static ICON_INPUT: [u8; 8] = [
    0b00011000, 0b00011000, 0b01111110, 0b00100100,
    0b00100100, 0b00011000, 0b00011000, 0b00000000,
];

#[derive(Clone, Copy)]
pub(crate) struct IconDef {
    pub color: u32,
    /// Phosphor-Bold 图标码点（shared/icons.rs `settings` 模块；24px 圆角底上
    /// 按 16px 栅格化，抗锯齿灰度）。
    pub cp: u32,
    /// 8x8 点阵兜底字形（图标字体未就绪时经 draw_bits 放大绘制）。
    pub glyph: &'static [u8; 8],
    pub dark_glyph: bool,
}

// ==================== 图标索引 ====================

use crate::ttf_text::font_render::icons::settings as si;

pub(crate) const I_WIFI: usize = 0;
pub(crate) const I_MOBILE: usize = 1;
pub(crate) const I_VPN: usize = 2;
pub(crate) const I_BT: usize = 3;
pub(crate) const I_SUN: usize = 4;
pub(crate) const I_MOON: usize = 5;
pub(crate) const I_FONT: usize = 6;
pub(crate) const I_SPEAK: usize = 7;
pub(crate) const I_BELL: usize = 8;
pub(crate) const I_BATT: usize = 9;
pub(crate) const I_DISK: usize = 10;
pub(crate) const I_FINGER: usize = 11;
pub(crate) const I_LOCK: usize = 12;
pub(crate) const I_UPDATE: usize = 13;
pub(crate) const I_PHONE: usize = 14;
pub(crate) const I_NAV: usize = 15;
pub(crate) const I_APPS: usize = 16;
pub(crate) const I_RESET: usize = 17;
pub(crate) const I_USER: usize = 18;
pub(crate) const I_CLOUD: usize = 19;
pub(crate) const I_SYNC: usize = 20;
pub(crate) const I_CLOCK: usize = 21;
pub(crate) const I_LANG: usize = 22;
pub(crate) const I_KBD: usize = 23;
pub(crate) const I_PAT: usize = 24;
pub(crate) const I_INFO: usize = 25;
pub(crate) const I_ACC: usize = 26;
pub(crate) const I_EYE: usize = 27;
pub(crate) const I_GEAR: usize = 28;
pub(crate) const I_SAVE: usize = 29;
pub(crate) const I_INPUT: usize = 30;
pub(crate) const I_NOTIF: usize = 31;

pub(crate) static ICONS: [IconDef; 32] = [
    IconDef { color: IC_NET, cp: si::WIFI, glyph: &ICON_WIFI, dark_glyph: false },
    IconDef { color: IC_NET, cp: si::MOBILE, glyph: &ICON_MOBILE, dark_glyph: false },
    IconDef { color: IC_NET, cp: si::VPN, glyph: &ICON_VPN, dark_glyph: false },
    IconDef { color: IC_BT, cp: si::BT, glyph: &ICON_BT, dark_glyph: false },
    IconDef { color: IC_DISP, cp: si::SUN, glyph: &ICON_SUN, dark_glyph: false },
    IconDef { color: IC_DISP, cp: si::MOON, glyph: &ICON_MOON, dark_glyph: false },
    IconDef { color: IC_DISP, cp: si::FONT, glyph: &FONT_A, dark_glyph: false },
    IconDef { color: IC_SOUND, cp: si::SPEAK, glyph: &ICON_SPEAK, dark_glyph: false },
    IconDef { color: IC_NOTIF, cp: si::BELL, glyph: &ICON_BELL, dark_glyph: false },
    IconDef { color: IC_BATT, cp: si::BATT, glyph: &ICON_BATT, dark_glyph: true },
    IconDef { color: IC_STOR, cp: si::DISK, glyph: &ICON_DISK, dark_glyph: false },
    IconDef { color: IC_PRIV, cp: si::FINGER, glyph: &ICON_FINGER, dark_glyph: false },
    IconDef { color: IC_PRIV, cp: si::LOCK, glyph: &ICON_LOCK, dark_glyph: false },
    IconDef { color: IC_UPD, cp: si::UPDATE, glyph: &ICON_UPDATE, dark_glyph: false },
    IconDef { color: IC_PHONE, cp: si::PHONE, glyph: &ICON_PHONE, dark_glyph: false },
    IconDef { color: IC_GRAY, cp: si::NAV, glyph: &ICON_NAV, dark_glyph: false },
    IconDef { color: IC_SYS, cp: si::APPS, glyph: &ICON_APPS, dark_glyph: false },
    IconDef { color: IC_RESET, cp: si::RESET, glyph: &ICON_RESET, dark_glyph: false },
    IconDef { color: IC_ACCT, cp: si::USER, glyph: &ICON_USER, dark_glyph: false },
    IconDef { color: IC_CLOUD, cp: si::CLOUD, glyph: &ICON_CLOUD, dark_glyph: false },
    IconDef { color: IC_SYNC, cp: si::SYNC, glyph: &ICON_SYNC, dark_glyph: false },
    IconDef { color: IC_CLOCK, cp: si::CLOCK, glyph: &ICON_CLOCK, dark_glyph: false },
    IconDef { color: IC_LANG, cp: si::LANG, glyph: &ICON_LANG, dark_glyph: false },
    IconDef { color: IC_KBD, cp: si::KBD, glyph: &ICON_KBD, dark_glyph: false },
    IconDef { color: IC_PAT, cp: si::PAT, glyph: &ICON_PAT, dark_glyph: false },
    IconDef { color: IC_INFO, cp: si::INFO, glyph: &ICON_INFO, dark_glyph: false },
    IconDef { color: IC_ACC, cp: si::ACC, glyph: &ICON_ACC, dark_glyph: false },
    IconDef { color: IC_DISP, cp: si::EYE, glyph: &ICON_EYE, dark_glyph: false },
    IconDef { color: IC_SYS, cp: si::GEAR, glyph: &ICON_GEAR, dark_glyph: false },
    IconDef { color: IC_ACCT, cp: si::SAVE, glyph: &ICON_SAVE, dark_glyph: false },
    IconDef { color: IC_GRAY, cp: si::INPUT, glyph: &ICON_INPUT, dark_glyph: false },
    IconDef { color: IC_NOTIF, cp: si::BELL, glyph: &ICON_BELL, dark_glyph: false },
];

// ==================== 液态玻璃面板 ====================

struct SettingsGlass;
impl GlassTarget for SettingsGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        } else {
            0
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
}

/// 毛玻璃面板（深浅色随主题）。
pub(crate) fn draw_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(&mut SettingsGlass, x, y, w, h, r, dark_on(), true);
}

/// 自定义样式液态玻璃面板。
pub(crate) fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut SettingsGlass, x, y, w, h, r, st, true);
}

pub(crate) fn style_accent() -> &'static GlassStyle {
    &STYLE_ACCENT
}

pub(crate) fn style_danger() -> &'static GlassStyle {
    &STYLE_DANGER
}

// ==================== 主题配色 ====================

pub(crate) fn c_page() -> u32 {
    if dark_on() { T_PAGE_D } else { T_PAGE_L }
}

pub(crate) fn c_card() -> u32 {
    if dark_on() { T_CARD_D } else { T_CARD_L }
}

pub(crate) fn c_text() -> u32 {
    if dark_on() { T_TEXT_D } else { T_TEXT_L }
}

pub(crate) fn c_div() -> u32 {
    if dark_on() { T_DIV_D } else { T_DIV_L }
}

pub(crate) fn c_field() -> u32 {
    if dark_on() { T_FIELD_D } else { T_FIELD_L }
}

/// 主字号（随「字体大小」缩放，0 小 1 标准 2 大 3 超大）。
pub(crate) fn font_px() -> f32 {
    match cfg::cfg_get_int(cfg::CONF_DISPLAY, b"font_size", 1) {
        0 => 14.0,
        2 => 19.0,
        3 => 22.0,
        _ => 16.0,
    }
}

// ==================== 渐变背景 ====================

pub(crate) fn draw_gradient_bg() {
    let base = c_page();
    let cr = ((base >> 16) & 0xFF) as u32;
    let cg = ((base >> 8) & 0xFF) as u32;
    let cb = (base & 0xFF) as u32;
    let (lift, sink) = if dark_on() { (0, 24) } else { (20, 16) };
    let (tr, tg, tb) = ((cr + lift).min(255), (cg + lift).min(255), (cb + lift).min(255));
    let (br, bg_, bb) = (
        cr.saturating_sub(sink),
        cg.saturating_sub(sink),
        cb.saturating_sub(sink),
    );
    for y in 0..DISPLAY_H as i32 {
        let t = (y as u32 * 255 / DISPLAY_H as u32).min(255);
        let inv = 255 - t;
        let r = (tr * inv + br * t) / 255;
        let g = (tg * inv + bg_ * t) / 255;
        let b = (tb * inv + bb * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        let iy = y as usize * DISPLAY_W;
        unsafe {
            for x in 0..DISPLAY_W {
                BACK_BUF[iy + x] = row;
            }
        }
    }
}

// ==================== 标题栏 ====================

pub(crate) fn draw_title_bar(title: &[u8]) {
    draw_glass(0, 0, DISPLAY_W as i32, TOP_TITLE_H, 0);
    // 返回箭头：Phosphor caret-left 矢量字形（点阵兜底见 draw_bits）。
    if !draw_icon_glyph_centered(
        TOP_BACK_X + TOP_BACK_SZ / 2,
        TOP_BACK_Y + TOP_BACK_SZ / 2,
        si::CARET_LEFT,
        20.0,
        c_text(),
    ) {
        draw_bits(TOP_BACK_X + 11, TOP_BACK_Y + 10, &FONT_LT, 2, c_text());
    }
    let px = 16.0;
    let tw = text_width_px_bold(title, px);
    let tx = (DISPLAY_W as i32 - tw) / 2;
    draw_text_bold_px(tx, text_top(px, TOP_TITLE_H / 2), title, px, c_text());
}

pub(crate) fn back_hit(x: i32, y: i32) -> bool {
    x >= TOP_BACK_X
        && x <= TOP_BACK_X + TOP_BACK_SZ
        && y >= TOP_BACK_Y
        && y <= TOP_BACK_Y + TOP_BACK_SZ
}

// ==================== 开关 ====================

pub(crate) fn draw_switch(x: i32, y: i32, on: bool) {
    const SW_W: i32 = 48;
    const SW_H: i32 = 28;
    let track = if on {
        C_SW_ON
    } else if dark_on() {
        T_SW_OFF_D
    } else {
        T_SW_OFF_L
    };
    fill_round_rect(x, y, SW_W, SW_H, 14, track);
    let kx = if on { x + SW_W - 24 - 2 } else { x + 2 };
    fill_circle(kx + 12, y + SW_H / 2, 12, C_WHITE);
}

pub(crate) fn switch_rect(x: i32, y: i32) -> (i32, i32, i32, i32) {
    (x, y, 48, 28)
}

fn in_rect(px: i32, py: i32, r: (i32, i32, i32, i32)) -> bool {
    px >= r.0 && px < r.0 + r.2 && py >= r.1 && py < r.1 + r.3
}

pub(crate) fn switch_inside(px: i32, py: i32, x: i32, y: i32) -> bool {
    in_rect(px, py, switch_rect(x, y))
}

// ==================== 箭头与分割线 ====================

pub(crate) fn draw_arrow(x: i32, cy: i32) {
    // 列表右箭头：Phosphor caret-right 矢量字形（16px 墨迹盒居中于原 16x16 位图位）。
    if !draw_icon_glyph_centered(x + 8, cy, si::CARET_RIGHT, 16.0, C_SUB) {
        draw_bits(x, cy - 8, &FONT_GT, 2, C_SUB);
    }
}

pub(crate) fn draw_divider(y: i32) {
    fill_rect(60, y + ROW_H - 1, DISPLAY_W as i32 - MARGIN - 60, 1, c_div());
}

pub(crate) fn draw_separator(y: i32) {
    fill_rect(24, y, DISPLAY_W as i32 - 24 - MARGIN, 1, c_div());
}

pub(crate) fn draw_group_header(y: i32, title: &[u8]) {
    draw_text_px(MARGIN, text_top(13.0, y + 8 + 6), title, 13.0, C_SUB);
}

// ==================== 列表行 ====================

/// 图标 + 标题 + 右侧值/箭头/开关 的列表行。
pub(crate) fn draw_row(y: i32, icon: usize, title: &[u8], value: &[u8], arrow: bool, sw: bool) {
    let ic = &ICONS[icon];
    let cy = y + ROW_H / 2;
    fill_round_rect(MARGIN, y + (ROW_H - ICON_SZ) / 2, ICON_SZ, ICON_SZ, ICON_R, ic.color);
    let gcolor = if ic.dark_glyph { C_BLACK } else { C_WHITE };
    // 行图标：Phosphor-Bold 矢量字形按墨迹居中（抗锯齿）；图标字体未就绪时
    // 退回 8x8 点阵 2 倍放大。
    if !draw_icon_glyph_centered(MARGIN + ICON_SZ / 2, cy, ic.cp, 16.0, gcolor) {
        draw_bits(
            MARGIN + (ICON_SZ - 16) / 2,
            y + (ROW_H - ICON_SZ) / 2 + (ICON_SZ - 16) / 2,
            ic.glyph,
            2,
            gcolor,
        );
    }
    let px = font_px();
    draw_text_px(MARGIN + ICON_SZ + 12, text_top(px, y + ROW_H / 2), title, px, c_text());
    if sw {
        // 开关状态由调用方在渲染前确定；此函数仅按 开 绘制。
        draw_switch(DISPLAY_W as i32 - MARGIN - 48, y + (ROW_H - 28) / 2, true);
    } else if arrow {
        let w = text_width_px(value, 14.0);
        let right = DISPLAY_W as i32 - MARGIN - 26;
        if w > 0 {
            draw_text_px(right - w, text_top(14.0, y + ROW_H / 2), value, 14.0, C_SUB);
        }
        draw_arrow(DISPLAY_W as i32 - MARGIN - 16, y + ROW_H / 2);
    } else if !value.is_empty() {
        let w = text_width_px(value, 14.0);
        draw_text_px(
            DISPLAY_W as i32 - MARGIN - w,
            text_top(14.0, y + ROW_H / 2),
            value,
            14.0,
            C_SUB,
        );
    }
}

/// 行（无图标）：标题 + 右侧值/箭头/开关。
pub(crate) fn draw_picker_row(y: i32, title: &[u8], value: &[u8], arrow: bool) {
    let px = font_px();
    draw_text_px(MARGIN + 4, text_top(px, y + ROW_H / 2), title, px, c_text());
    if arrow {
        let w = text_width_px(value, 14.0);
        let right = DISPLAY_W as i32 - MARGIN - 26;
        if w > 0 {
            draw_text_px(right - w, text_top(14.0, y + ROW_H / 2), value, 14.0, C_SUB);
        }
        draw_arrow(DISPLAY_W as i32 - MARGIN - 16, y + ROW_H / 2);
    } else if !value.is_empty() {
        let w = text_width_px(value, 14.0);
        draw_text_px(
            DISPLAY_W as i32 - MARGIN - w,
            text_top(14.0, y + ROW_H / 2),
            value,
            14.0,
            C_SUB,
        );
    }
}

/// 单选行（左侧 title + 右侧选中圈）。
pub(crate) fn draw_radio_row(y: i32, title: &[u8], selected: bool) {
    let px = font_px();
    draw_text_px(24, text_top(px, y + ROW_H / 2), title, px, c_text());
    if selected {
        fill_circle(DISPLAY_W as i32 - MARGIN - 20, y + ROW_H / 2, 11, C_PRIMARY);
        fill_circle(DISPLAY_W as i32 - MARGIN - 20, y + ROW_H / 2, 4, C_WHITE);
    } else {
        fill_circle(DISPLAY_W as i32 - MARGIN - 20, y + ROW_H / 2, 11, c_field());
        fill_circle(DISPLAY_W as i32 - MARGIN - 20, y + ROW_H / 2, 8, c_card());
    }
}

/// 步进行（- / 值 / +）。
pub(crate) fn draw_stepper_row(y: i32, title: &[u8], value: &[u8]) {
    let px = font_px();
    draw_text_px(24, text_top(px, y + ROW_H / 2), title, px, c_text());
    let bx = DISPLAY_W as i32 - MARGIN - 150;
    draw_glass(bx, y + (ROW_H - 40) / 2, 40, 40, 20);
    draw_text_px(bx + 11, text_top(22.0, y + ROW_H / 2), b"-", 22.0, c_text());
    let bx2 = bx + 46;
    draw_glass(bx2, y + (ROW_H - 40) / 2, 40, 40, 20);
    draw_text_px(bx2 + 11, text_top(22.0, y + ROW_H / 2), b"+", 22.0, c_text());
    let vw = text_width_px(value, 15.0);
    let vx = bx2 - 14 - vw;
    draw_text_px(vx, text_top(15.0, y + ROW_H / 2), value, 15.0, c_text());
}

// ==================== 按钮 ====================

pub(crate) fn draw_button(y: i32, label: &[u8], primary: bool) {
    let st = if primary { &STYLE_ACCENT } else { &STYLE_DANGER };
    draw_glass_style(MARGIN, y, CARD_W, BTN_H, BTN_R, st);
    let px = font_px();
    let w = text_width_px(label, px);
    draw_text_px(
        MARGIN + (CARD_W - w) / 2,
        text_top(px, y + BTN_H / 2),
        label,
        px,
        C_WHITE,
    );
}

/// 普通玻璃按钮（返回/取消等）。
pub(crate) fn draw_button_plain(y: i32, label: &[u8]) {
    draw_glass(MARGIN, y, CARD_W, BTN_H, BTN_R);
    let px = font_px();
    let w = text_width_px(label, px);
    draw_text_px(
        MARGIN + (CARD_W - w) / 2,
        text_top(px, y + BTN_H / 2),
        label,
        px,
        c_text(),
    );
}

// ==================== 滑块 ====================

pub(crate) fn slider_x() -> i32 {
    DISPLAY_W as i32 / 2 - 120
}

pub(crate) fn draw_slider(x: i32, y: i32, val: i32) {
    const SW: i32 = 240;
    const SH: i32 = 8;
    let track = if dark_on() { T_SW_OFF_D } else { T_SW_OFF_L };
    fill_round_rect(x, y - SH / 2, SW, SH, SH / 2, track);
    let fill_w = (val.clamp(0, 100) * SW / 100).max(4);
    fill_round_rect(x, y - SH / 2, fill_w, SH, SH / 2, C_PRIMARY);
    let kx = x + (val.clamp(0, 100) * SW / 100);
    fill_circle(kx, y, 14, C_WHITE);
    fill_circle(kx, y, 5, C_PRIMARY);
}

pub(crate) fn slider_inside(px: i32, py: i32, x: i32, y: i32) -> bool {
    px >= x - 4 && px <= x + 244 && py >= y - 18 && py <= y + 18
}

pub(crate) fn slider_value(px: i32, x: i32) -> i32 {
    let t = (px - x).clamp(0, 240);
    t * 100 / 240
}

// ==================== 卡片容器 ====================

pub(crate) fn card_h(rows: usize, btn: bool) -> i32 {
    CARD_PAD + rows as i32 * ROW_H + if btn { CARD_PAD + BTN_H + CARD_PAD } else { CARD_PAD }
}

/// 渐变底色 + 标题栏（每页统一入口）。
pub(crate) fn page_base(title: &[u8]) {
    draw_gradient_bg();
    draw_title_bar(title);
}