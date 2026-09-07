//! CSS 解析器（no_std + alloc）：样式表 / 内联样式 / 选择器 / 值解析。
//!
//! 支持的选择器：标签、`.class`、`#id`、后代组合（空格）、选择器列表（逗号）。
//! 支持的属性：display / color / background(-color) / font-size / font-weight /
//! line-height / text-align / text-decoration / margin(-*) / padding(-*) /
//! border(-*) / width / height / position / left / top / list-style。
//! 未知属性忽略（宽容解析）。

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Debug)]
pub struct SimpleSel {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum Selector {
    Simple(SimpleSel),
    /// 后代组合：ancestors 中的任一连续祖先匹配后，simple 匹配自身。
    Descendant { ancestors: Vec<SimpleSel>, simple: SimpleSel },
}

impl Selector {
    /// 特异性 (id, class, type)。
    pub fn specificity(&self) -> (u32, u32, u32) {
        match self {
            Selector::Simple(s) => spec_of(s),
            Selector::Descendant { ancestors, simple } => {
                let mut sp = spec_of(simple);
                for a in ancestors {
                    let s = spec_of(a);
                    sp.0 += s.0;
                    sp.1 += s.1;
                    sp.2 += s.2;
                }
                sp
            }
        }
    }
}

fn spec_of(s: &SimpleSel) -> (u32, u32, u32) {
    (
        if s.id.is_some() { 1 } else { 0 },
        s.classes.len() as u32,
        if s.tag.is_some() { 1 } else { 0 },
    )
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

impl Stylesheet {
    pub fn new() -> Stylesheet {
        Stylesheet { rules: Vec::new() }
    }
    /// 解析 `<style>` 内容（一个样式表）。
    pub fn parse(css: &str) -> Stylesheet {
        let mut ss = Stylesheet::new();
        let b = css.as_bytes();
        let mut i = 0usize;
        let n = b.len();
        while i < n {
            // 找 '{'，跳过注释
            let (sel_start, sel_end) = (i, find_brace(b, i, b'{'));
            if sel_end >= n {
                break;
            }
            let sel_text = &css[sel_start..sel_end];
            // 找匹配的 '}'
            let mut j = sel_end + 1;
            let mut depth = 1;
            while j < n && depth > 0 {
                match b[j] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let decl_text = {
                // ★ 未闭合的 `{`（如 `<style>x{</style>`，内容以 '{' 结尾）时
                // j == sel_end + 1，j-1 == sel_end：切片 [sel_end+1, sel_end]
                // start > end 直接 panic——远程/本地页面内容可杀死浏览器。
                let end = if depth == 0 { j - 1 } else { j };
                &css[sel_end + 1..end.max(sel_end + 1)]
            };
            let selectors = parse_selector_list(sel_text);
            let decls = parse_declarations(decl_text);
            if !selectors.is_empty() && !decls.is_empty() {
                ss.rules.push(Rule { selectors, decls });
            }
            i = j;
        }
        ss
    }
}

fn find_brace(b: &[u8], start: usize, ch: u8) -> usize {
    let mut i = start;
    let n = b.len();
    while i < n {
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b[i] == ch {
            return i;
        }
        i += 1;
    }
    n
}

fn parse_selector_list(s: &str) -> Vec<Selector> {
    let mut out = Vec::new();
    for part in s.split(',') {
        if let Some(sel) = parse_selector(part.trim()) {
            out.push(sel);
        }
    }
    out
}

fn parse_selector(s: &str) -> Option<Selector> {
    if s.is_empty() || s.contains('{') || s.contains('}') {
        return None;
    }
    // 后代组合
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        return Some(Selector::Simple(parse_simple(parts[0])?));
    }
    let mut ancestors = Vec::new();
    for p in &parts[..parts.len() - 1] {
        ancestors.push(parse_simple(p)?);
    }
    Some(Selector::Descendant {
        ancestors,
        simple: parse_simple(parts[parts.len() - 1])?,
    })
}

fn parse_simple(s: &str) -> Option<SimpleSel> {
    let mut sel = SimpleSel { tag: None, id: None, classes: Vec::new() };
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '#' | '.' => {
                push_compound(&mut sel, &mut cur);
                cur.push(c);
            }
            _ => cur.push(c),
        }
    }
    push_compound(&mut sel, &mut cur);
    if sel.tag.is_none() && sel.id.is_none() && sel.classes.is_empty() {
        None
    } else {
        Some(sel)
    }
}

fn push_compound(sel: &mut SimpleSel, cur: &mut String) {
    if cur.is_empty() {
        return;
    }
    if cur.starts_with('#') {
        sel.id = Some(cur[1..].to_string());
    } else if cur.starts_with('.') {
        sel.classes.push(cur[1..].to_string());
    } else if sel.tag.is_none() {
        sel.tag = Some(cur.clone());
    }
    cur.clear();
}

/// 解析声明块（`prop: value; ...`；忽略无效项与注释）。
pub fn parse_declarations(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for decl in s.split(';') {
        let d = decl.trim();
        if d.is_empty() {
            continue;
        }
        if let Some(ci) = d.find(':') {
            let prop = d[..ci].trim().to_ascii_lowercase();
            let value = d[ci + 1..].trim();
            if !prop.is_empty() && !value.is_empty() {
                out.push((prop, value.to_string()));
            }
        }
    }
    out
}

/// 解析内联 style 属性。
pub fn parse_inline_style(s: &str) -> Vec<(String, String)> {
    parse_declarations(s)
}

// ==================== 值解析 ====================

/// CSS 颜色。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Color(pub u32); // 0x00RRGGBB

pub const TRANSPARENT: Color = Color(0x00_00_00_00);

/// 命名颜色（常用子集）。
fn named_color(name: &str) -> Option<u32> {
    let v = match name {
        "transparent" => return Some(0),
        "black" => 0x00_00_00_00,
        "white" => 0x00_FF_FF_FF,
        "red" => 0x00_FF_00_00,
        "green" => 0x00_00_80_00,
        "lime" => 0x00_00_FF_00,
        "blue" => 0x00_00_00_FF,
        "yellow" => 0x00_FF_FF_00,
        "cyan" | "aqua" => 0x00_00_FF_FF,
        "magenta" | "fuchsia" => 0x00_FF_00_FF,
        "gray" | "grey" => 0x00_80_80_80,
        "silver" => 0x00_C0_C0_C0,
        "maroon" => 0x00_80_00_00,
        "olive" => 0x00_80_80_00,
        "purple" => 0x00_80_00_80,
        "teal" => 0x00_00_80_80,
        "navy" => 0x00_00_00_80,
        "orange" => 0x00_FF_A5_00,
        "pink" => 0x00_FF_C0_CB,
        "brown" => 0x00_A5_2A_2A,
        "gold" => 0x00_FF_D7_00,
        "darkgray" | "darkgrey" => 0x00_A9_A9_A9,
        "dimgray" | "dimgrey" => 0x00_69_69_69,
        "lightgray" | "lightgrey" => 0x00_D3_D3_D3,
        "slategray" | "slategrey" => 0x00_70_80_90,
        "lightblue" => 0x00_AD_D8_E6,
        "lightgreen" => 0x00_90_EE_90,
        "darkgreen" => 0x00_00_64_00,
        "darkblue" => 0x00_00_00_8B,
        "darkred" => 0x00_8B_00_00,
        "indigo" => 0x00_4B_00_82,
        "violet" => 0x00_EE_82_EE,
        "coral" => 0x00_FF_7F_50,
        "tomato" => 0x00_FF_63_47,
        "khaki" => 0x00_F0_E6_8C,
        "beige" => 0x00_F5_F5_DC,
        "ivory" => 0x00_FF_FF_F0,
        "aliceblue" => 0x00_F0_F8_FF,
        "whitesmoke" => 0x00_F5_F5_F5,
        "gainsboro" => 0x00_DC_DC_DC,
        "skyblue" => 0x00_87_CE_EB,
        "steelblue" => 0x00_46_82_B4,
        "royalblue" => 0x00_41_69_E1,
        "mediumseagreen" => 0x00_3C_B3_71,
        "seagreen" => 0x00_2E_8B_57,
        "firebrick" => 0x00_B2_22_22,
        "chocolate" => 0x00_D2_69_1E,
        "saddlebrown" => 0x00_8B_45_13,
        "crimson" => 0x00_DC_14_3C,
        "deeppink" => 0x00_FF_14_93,
        "hotpink" => 0x00_FF_69_B4,
        "turquoise" => 0x00_40_E0_D0,
        "mediumturquoise" => 0x00_48_D1_CC,
        "darkslategray" | "darkslategrey" => 0x00_2F_4F_4F,
        "lightsteelblue" => 0x00_B0_C4_DE,
        "cornflowerblue" => 0x00_64_95_ED,
        _ => return None,
    };
    Some(v)
}

/// 解析颜色值。失败返回 None。
pub fn parse_color(v: &str) -> Option<Color> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 3 {
            let r = hex_nybble(hex.as_bytes()[0])?;
            let g = hex_nybble(hex.as_bytes()[1])?;
            let b = hex_nybble(hex.as_bytes()[2])?;
            return Some(Color((r * 17) << 16 | (g * 17) << 8 | (b * 17)));
        }
        if hex.len() == 6 {
            let r = hex_byte(&hex.as_bytes()[0..2])?;
            let g = hex_byte(&hex.as_bytes()[2..4])?;
            let b = hex_byte(&hex.as_bytes()[4..6])?;
            return Some(Color(r << 16 | g << 8 | b));
        }
        return None;
    }
    if let Some(rest) = v.strip_prefix("rgb(").or_else(|| v.strip_prefix("rgba(")) {
        if let Some(end) = rest.find(')') {
            let inner = &rest[..end];
            let mut parts = [0u32; 4];
            let mut cnt = 0;
            for p in inner.split(',') {
                if cnt >= 4 {
                    break;
                }
                let p = p.trim();
                let p = p.strip_suffix('%').map(|x| x).unwrap_or(p);
                if let Ok(num) = p.parse::<f32>() {
                    let num = if p.contains('%') { num * 255.0 / 100.0 } else { num };
                    parts[cnt] = num.clamp(0.0, 255.0) as u32;
                    cnt += 1;
                } else {
                    return None;
                }
            }
            let (r, g, b) = (parts[0], parts[1], parts[2]);
            let a = if cnt >= 4 { parts[3] } else { 255 };
            if a == 0 {
                return Some(TRANSPARENT);
            }
            // 预乘 alpha 到背景上会失真；这里把带 alpha 的颜色按 alpha 混合到
            // 纯白上近似（不支持真 alpha 合成，见 style.rs 说明）。
            let ia = 255 - a;
            let r = (r * a + 255 * ia) / 255;
            let g = (g * a + 255 * ia) / 255;
            let b = (b * a + 255 * ia) / 255;
            return Some(Color(r << 16 | g << 8 | b));
        }
        return None;
    }
    if v.starts_with("hsl(") {
        return Some(Color(0x00_9A_A7_B5)); // 简化：不支持 HSL，回退灰
    }
    named_color(v).map(Color)
}

fn hex_nybble(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'a'..=b'f' => Some((c - b'a' + 10) as u32),
        b'A'..=b'F' => Some((c - b'A' + 10) as u32),
        _ => None,
    }
}

fn hex_byte(s: &[u8]) -> Option<u32> {
    Some(hex_nybble(s[0])? * 16 + hex_nybble(s[1])?)
}

/// 长度：带单位。px / pt / em / rem / % / 无单位（按 px）。
pub enum Length {
    Px(f32),
    Em(f32),
    Percent(f32),
    Auto,
}

/// 解析长度值；`parent_px` 用于 em/% 换算。失败返回 None（调用方用默认值）。
pub fn parse_length(v: &str, parent_px: f32, parent_other: f32) -> Option<f32> {
    let v = v.trim();
    if v == "auto" {
        return None;
    }
    // 结果钳制到 0..=8192px：无上限时 `1e30px` → inf → `as i32` 饱和为
    // i32::MAX，布局坐标进入加/乘回绕簇（滚动条/绘制坐标错乱）。
    const LEN_CAP: f32 = 8192.0;
    let clamp = |x: f32| Some(x.clamp(0.0, LEN_CAP));
    if let Some(x) = v.strip_suffix("px") {
        return clamp(x.trim().parse::<f32>().ok()?);
    }
    if let Some(x) = v.strip_suffix("pt") {
        return clamp(x.trim().parse::<f32>().ok()? * 96.0 / 72.0);
    }
    if let Some(x) = v.strip_suffix("em") {
        return clamp(x.trim().parse::<f32>().ok()? * parent_px);
    }
    if let Some(x) = v.strip_suffix("rem") {
        return clamp(x.trim().parse::<f32>().ok()? * 16.0);
    }
    if let Some(x) = v.strip_suffix('%') {
        return clamp(x.trim().parse::<f32>().ok()? * parent_other / 100.0);
    }
    // 无单位（0 或字体大小倍数，按 px 处理）
    clamp(v.parse::<f32>().ok()?)
}

/// 解析 font-size。返回 px。
pub fn parse_font_size(v: &str, parent_px: f32) -> f32 {
    let v = v.trim();
    let kw = match v {
        "xx-small" => Some(9.0),
        "x-small" => Some(11.0),
        "small" => Some(13.0),
        "medium" => Some(16.0),
        "large" => Some(20.0),
        "x-large" => Some(26.0),
        "xx-large" => Some(32.0),
        "smaller" => Some(parent_px * 0.83),
        "larger" => Some(parent_px * 1.2),
        _ => None,
    };
    if let Some(k) = kw {
        return k;
    }
    parse_length(v, parent_px, 100.0).unwrap_or(parent_px)
}

/// 解析 line-height。返回 px（number 按父字号倍数）。
pub fn parse_line_height(v: &str, parent_px: f32, font_px: f32) -> f32 {
    let v = v.trim();
    if v == "normal" {
        return font_px * 1.2;
    }
    if let Ok(num) = v.parse::<f32>() {
        return font_px * num;
    }
    parse_length(v, parent_px, 100.0).unwrap_or(font_px * 1.2)
}

/// 解析 border-width。
pub fn parse_border_width(v: &str) -> f32 {
    let v = v.trim();
    let kw = match v {
        "thin" => Some(1.0),
        "medium" => Some(3.0),
        "thick" => Some(5.0),
        _ => None,
    };
    if let Some(k) = kw {
        return k;
    }
    parse_length(v, 16.0, 100.0).unwrap_or(0.0)
}

/// 解析 margin/padding 1-4 值 → (top, right, bottom, left) px。
pub fn parse_box_edges(v: &str) -> [f32; 4] {
    let vals: Vec<f32> = v
        .split_whitespace()
        .filter_map(|x| parse_length(x, 16.0, 100.0))
        .collect();
    match vals.len() {
        0 => [0.0; 4],
        1 => [vals[0]; 4],
        2 => [vals[0], vals[1], vals[0], vals[1]],
        3 => [vals[0], vals[1], vals[2], vals[1]],
        _ => [vals[0], vals[1], vals[2], vals[3]],
    }
}

/// 解析 position。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Position {
    Static,
    Relative,
}

pub fn parse_position(v: &str) -> Position {
    match v.trim() {
        "relative" => Position::Relative,
        // absolute/fixed/sticky 初始不支持：按 static 处理（文档注明）
        _ => Position::Static,
    }
}

/// 解析 display。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    Block,
    Inline,
    InlineBlock,
    None,
    Table,
    TableRow,
    TableCell,
}

pub fn parse_display(v: &str) -> Option<Display> {
    match v.trim() {
        "block" => Some(Display::Block),
        "inline" => Some(Display::Inline),
        "inline-block" => Some(Display::InlineBlock),
        "none" => Some(Display::None),
        "table" => Some(Display::Table),
        "table-row" => Some(Display::TableRow),
        "table-cell" => Some(Display::TableCell),
        // Flex/Grid 暂不支持：按块级降级渲染（布局引擎文档注明）
        "flex" | "grid" | "inline-flex" | "inline-grid" => Some(Display::Block),
        _ => None,
    }
}
