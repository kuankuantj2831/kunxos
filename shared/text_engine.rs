//! KunxOS 共享文本引擎（no_std + alloc，宿主可测）。
//!
//! 渲染管线：UTF-8 → script 分段 → 字体回退选择 → shaping（rustybuzz，
//! Harfbuzz 的 Rust 移植）→ BiDi（UAX #9，unicode-bidi）→ 换行（UAX #14，
//! unicode-linebreak + CJK 禁则）→ 布局 → 光栅化（fontdue 轮廓 + Noto Color
//! Emoji CBDT 位图）。
//!
//! 字体回退链：Noto Sans CJK（cn，拉丁/中日韩）→ Noto Naskh Arabic（ar，
//! 阿拉伯/希伯来等 RTL 文字）→ Noto Color Emoji（emoji；ZWJ/肤色连字经
//! rustybuzz GSUB 处理为单个字形后按 CBDT 位图渲染）。
//!
//! 缓存策略（内存受限设备）：
//!   - 整段布局缓存（16 槽，key=文本哈希+字号+行宽+语言）：每帧不重复
//!     bidi/shaping（最昂贵步骤），满足 120 FPS 不阻塞；
//!   - shaped-run 缓存（32 槽）：同文本片段不重复 shaping；
//!   - 单色字形缓存 384 槽（≤4KB/槽）+ Emoji 位图缓存 48 槽（解码缩放至
//!     ≤32px，≤8KB/槽）。总缓存上限约 700KB，LRU 逐出。

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

// ==================== 系统调用（仅目标平台） ====================

#[cfg(target_os = "none")]
mod sys {
    use core::arch::asm;
    use alloc::vec::Vec;

    pub const SYS_READ: usize = 63;
    pub const SYS_OPENAT: usize = 56;
    pub const SYS_CLOSE: usize = 57;
    pub const SYS_LSEEK: usize = 62;
    pub const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;

    #[inline(always)]
    pub unsafe fn syscall1(n: usize, a1: usize) -> usize {
        let ret: usize;
        asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
        ret
    }
    #[inline(always)]
    pub unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
        let ret: usize;
        asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, in("x1") a2, in("x2") a3, options(nostack));
        ret
    }

    /// 读入整个文件（返回空 Vec 表示失败）。
    pub fn read_file(path: &[u8]) -> Vec<u8> {
        unsafe {
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, path.as_ptr() as usize, 0);
            if (fd as isize) < 0 {
                return Vec::new();
            }
            let size = syscall3(SYS_LSEEK, fd, 0, 2) as isize; // SEEK_END
            syscall3(SYS_LSEEK, fd, 0, 0); // SEEK_SET
            let size = if size <= 0 { 0usize } else { (size as usize).min(64 * 1024 * 1024) };
            let mut data = Vec::with_capacity(size);
            let mut buf = [0u8; 4096];
            loop {
                let n = syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len());
                if n == 0 || (n as isize) < 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            syscall1(SYS_CLOSE, fd);
            data
        }
    }
}

#[path = "emoji.rs"]
pub mod crate_emoji;

// ==================== 字体注册表（回退链） ====================

pub const MAX_FONTS: usize = 4;
pub const FONT_CN: usize = 0;
pub const FONT_AR: usize = 1;
pub const FONT_EMOJI: usize = 2;
/// 标题字体（bold.ttf 子集）槽位。
pub const FONT_BOLD: usize = 3;
/// 默认字号（像素）。
pub const FONT_PX: f32 = 16.0;

/// 字形位置（px，相对所在 run 起点；adv 为推进宽度）。
#[derive(Clone, Copy, Debug)]
pub struct GPos {
    pub gid: u16,
    /// 源文本中的簇（字节偏移，相对整段文本）。
    pub cluster: usize,
    pub x: i32,
    pub y: i32,
    pub adv: i32,
}

/// 一个 shaping 后的 run（同字体、同方向、视觉顺序字形）。
#[derive(Clone, Debug)]
pub struct Run {
    pub font: usize,
    pub rtl: bool,
    pub glyphs: Vec<GPos>,
    pub width: i32,
}

/// 一行：视觉顺序的 runs 序列。
#[derive(Clone, Debug)]
pub struct Line {
    pub runs: Vec<Run>,
    pub width: i32,
    pub ascent: i32,
    pub descent: i32,
    /// 布局字号（px），绘制时按此栅格化。
    pub px: f32,
}

/// 语言（影响 shaping 语言与换行规则）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    Zh,
    En,
    Ar,
}

// ---- 字体槽 ----

struct FontPair {
    /// 供 cmap / 字形查询。
    ttf: rustybuzz::ttf_parser::Face<'static>,
    /// 供 shaping。
    ruzz: rustybuzz::Face<'static>,
}

static mut FONT_PAIR: [Option<FontPair>; MAX_FONTS] = [None; MAX_FONTS];
static mut FONT_DUE: [Option<fontdue::Font>; MAX_FONTS] = [None; MAX_FONTS];
static mut EMOJI_FONTS: [Option<crate_emoji::EmojiFont>; MAX_FONTS] = [None; MAX_FONTS];
static mut FONT_NAMES: [Option<&'static str>; MAX_FONTS] = [None; MAX_FONTS];
static mut FONT_READY: [bool; MAX_FONTS] = [false; MAX_FONTS];

/// 注册一个字体字节流到槽位 `idx`（0=cn, 1=ar, 2=emoji）。失败返回 false。
pub fn font_register_bytes(idx: usize, name: &'static str, data: &[u8]) -> bool {
    if idx >= MAX_FONTS {
        return false;
    }
    // Box::leak：字体数据永久驻留（bump 堆本就不回收），Face 借用即 'static。
    let leaked: &'static mut Vec<u8> = Box::leak(Box::new(data.to_vec()));
    let d: &'static [u8] = leaked.as_slice();
    let Ok(ttf) = rustybuzz::ttf_parser::Face::parse(d, 0) else {
        return false;
    };
    let Ok(ttf2) = rustybuzz::ttf_parser::Face::parse(d, 0) else {
        return false;
    };
    let ruzz = rustybuzz::Face::from_face(ttf2);
    let due = fontdue::Font::from_bytes(d, fontdue::FontSettings::default()).ok();
    let emoji_font = crate_emoji::EmojiFont::from_bytes(d);
    let is_emoji = idx == FONT_EMOJI && emoji_font.is_some();
    let glyphs_ok = ttf.number_of_glyphs() > 0;
    unsafe {
        FONT_PAIR[idx] = Some(FontPair { ttf, ruzz });
        FONT_DUE[idx] = due;
        EMOJI_FONTS[idx] = emoji_font;
        FONT_NAMES[idx] = Some(name);
        FONT_READY[idx] = is_emoji || FONT_DUE[idx].is_some() || glyphs_ok;
    }
    true
}

/// 从 rootfs `/fonts/` 加载字体（cn/ar/emoji/bold，bold 为标题字体）。幂等。
#[cfg(target_os = "none")]
pub fn text_init() -> bool {
    if text_ready() {
        return true;
    }
    let mut ok = false;
    let cn = sys::read_file(b"/fonts/cn.ttf\0");
    if !cn.is_empty() && font_register_bytes(FONT_CN, "cn", &cn) {
        ok = true;
    }
    let ar = sys::read_file(b"/fonts/ar.ttf\0");
    if !ar.is_empty() && font_register_bytes(FONT_AR, "ar", &ar) {
        ok = true;
    }
    let emoji_f = sys::read_file(b"/fonts/emoji.ttf\0");
    if !emoji_f.is_empty() && font_register_bytes(FONT_EMOJI, "emoji", &emoji_f) {
        ok = true;
    }
    // 标题字体（bold.ttf 子集）：由 fonts/bold.ttf 经 make_fonts.py 生成。
    let bold_f = sys::read_file(b"/fonts/bold.ttf\0");
    if !bold_f.is_empty() && font_register_bytes(FONT_BOLD, "bold", &bold_f) {
        ok = true;
    }
    ok
}

/// 任何字体可用即视为就绪。
pub fn text_ready() -> bool {
    unsafe { FONT_READY[FONT_CN] || FONT_READY[FONT_AR] || FONT_READY[FONT_EMOJI] }
}

fn face(idx: usize) -> &'static FontPair {
    unsafe { FONT_PAIR[idx].as_ref().expect("text_engine：字体未注册") }
}

fn due(idx: usize) -> Option<&'static fontdue::Font> {
    unsafe { FONT_DUE[idx].as_ref() }
}

fn emoji_font() -> Option<&'static crate_emoji::EmojiFont> {
    unsafe { EMOJI_FONTS[FONT_EMOJI].as_ref() }
}

// ==================== Unicode 辅助 ====================

#[inline]
fn fround(v: f32) -> i32 {
    if v >= 0.0 {
        (v + 0.5) as i32
    } else {
        (v - 0.5) as i32
    }
}

/// 是否为 Emoji 呈现字符（含 ZWJ/VS16/肤色修饰，用于 run 分段）。
pub fn is_emoji_cp(cp: u32) -> bool {
    match cp {
        0x1F000..=0x1FAFF | 0x1FC00..=0x1FFFF => true,
        0x200D | 0xFE0F => true,              // ZWJ / VS16
        0x1F3FB..=0x1F3FF => true,            // 肤色修饰
        0x2600..=0x27BF | 0x2B00..=0x2BFF => true,
        0x1F1E6..=0x1F1FF => true,            // 区域指示符（国旗）
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x2199 | 0x21A9 | 0x21AA
        | 0x231A | 0x231B | 0x2328 | 0x23CF | 0x23E9..=0x23F3 | 0x23F8..=0x23FA | 0x24C2
        | 0x25AA | 0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FE | 0x260E | 0x2611 | 0x2614
        | 0x2615 | 0x2618 | 0x261D | 0x2620 | 0x2622 | 0x2623 | 0x2626 | 0x262A | 0x262E
        | 0x262F | 0x2638..=0x263A | 0x2640 | 0x2642 | 0x2648..=0x2653 | 0x265F | 0x2660
        | 0x2663 | 0x2665 | 0x2666 | 0x2668 | 0x267B | 0x267E | 0x2692..=0x2697 | 0x2699
        | 0x269B | 0x269C | 0x26A0 | 0x26A1 | 0x26A7 | 0x26AA | 0x26AB | 0x26B0 | 0x26B1
        | 0x26BD | 0x26BE | 0x26C4 | 0x26C5 | 0x26C8 | 0x26CE | 0x26CF | 0x26D1 | 0x26D3
        | 0x26D4 | 0x26E9 | 0x26EA | 0x26F0..=0x26F5 | 0x26F7..=0x26FA | 0x26FD | 0x2702
        | 0x2705 | 0x2708..=0x270D | 0x270F | 0x2712 | 0x2714 | 0x2716 | 0x271D | 0x2721
        | 0x2728 | 0x2733 | 0x2734 | 0x2744 | 0x2747 | 0x274C | 0x274E | 0x2753..=0x2755
        | 0x2757 | 0x2763 | 0x2764 | 0x2795..=0x2797 | 0x27A1 | 0x27B0 | 0x27BF | 0x2934
        | 0x2935 | 0x2B05..=0x2B07 | 0x2B1B | 0x2B1C | 0x2B50 | 0x2B55 | 0x3030 | 0x303D
        | 0x3297 | 0x3299 => true,
        _ => false,
    }
}

/// 是否 CJK 字符（竖排方向与字体回退）。
pub fn is_cjk_cp(cp: u32) -> bool {
    matches!(cp,
        0x2E80..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F
        | 0x3000..=0x303F | 0xFF00..=0xFFEF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
    )
}

/// 是否为 RTL 文字码点（阿拉伯/希伯来等；用于字体回退选择）。
fn is_rtl_cp(cp: u32) -> bool {
    matches!(cp,
        0x0590..=0x05FF    // 希伯来
        | 0x0600..=0x06FF  // 阿拉伯
        | 0x0700..=0x074F  // 叙利亚
        | 0x0750..=0x077F  // 阿拉伯补充
        | 0x0780..=0x07BF  // 塔纳
        | 0x07C0..=0x07FF  // NKo
        | 0x0800..=0x083F  // 撒玛利亚
        | 0x0840..=0x085F  // 曼达
        | 0x08A0..=0x08FF  // 阿拉伯扩展-A
        | 0xFB50..=0xFDFF  // 阿拉伯呈现形式-A
        | 0xFE70..=0xFEFF  // 阿拉伯呈现形式-B
        | 0x1E900..=0x1E95F // 阿德拉姆
        | 0x10900..=0x1091F // 腓尼基
    )
}

/// 按字符属性选择字体（script/emoji/CJK）。
pub fn pick_font(cp: u32) -> usize {
    if is_emoji_cp(cp) {
        return FONT_EMOJI;
    }
    if is_rtl_cp(cp) {
        return FONT_AR;
    }
    FONT_CN
}

// ==================== UTF-8 ====================

/// 解码 `s[i..]` 处的下一个 UTF-8 码点，返回 (码点, 字节数)。
#[inline]
pub fn utf8_next(s: &[u8], i: usize) -> (u32, usize) {
    if i >= s.len() {
        return (0, 1);
    }
    let b0 = s[i];
    if b0 < 0x80 {
        return (b0 as u32, 1);
    }
    let need = if b0 >= 0xF0 {
        4
    } else if b0 >= 0xE0 {
        3
    } else if b0 >= 0xC0 {
        2
    } else {
        return (0, 1);
    };
    if i + need > s.len() {
        return (0, 1);
    }
    let mut cp = match need {
        2 => (b0 & 0x1F) as u32,
        3 => (b0 & 0x0F) as u32,
        _ => (b0 & 0x07) as u32,
    };
    for k in 1..need {
        let b = s[i + k];
        if b & 0xC0 != 0x80 {
            return (0, 1);
        }
        cp = (cp << 6) | (b & 0x3F) as u32;
    }
    (cp, need)
}

/// 取 `text` 在 `i`（字节）前的码点。
fn utf8_prev(s: &[u8], i: usize) -> (u32, usize) {
    if i == 0 {
        return (0, 1);
    }
    let mut j = i - 1;
    while j > 0 && s[j] & 0xC0 == 0x80 {
        j -= 1;
    }
    utf8_next(s, j)
}

// ==================== shaping（含 shaped-run 缓存） ====================

const RUN_CACHE_SLOTS: usize = 32;

struct RunCacheSlot {
    hash: u64,
    px_bits: u32,
    rtl: bool,
    runs: Vec<Run>, // 簇相对段起点
}

static mut RUN_CACHE: [Option<RunCacheSlot>; RUN_CACHE_SLOTS] = [const { None }; RUN_CACHE_SLOTS];
static mut RUN_HEAD: usize = 0;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// shaping 单个同字体段。返回视觉顺序字形 run，簇相对段起点。
/// 结果缓存于 shaped-run 缓存。
fn shape_segment(seg: &str, font: usize, px: f32, rtl: bool) -> Option<Run> {
    if !unsafe { FONT_READY[font] } {
        return None;
    }
    let hash = fnv1a(seg.as_bytes());
    let px_bits = px.to_bits();
    // 查缓存
    unsafe {
        for i in 0..RUN_CACHE_SLOTS {
            if let Some(slot) = &RUN_CACHE[i] {
                if slot.hash == hash && slot.px_bits == px_bits && slot.rtl == rtl && slot.runs.len() == 1 {
                    return Some(slot.runs[0].clone());
                }
            }
        }
    }
    let mut f = face(font).ruzz.clone();
    f.set_pixels_per_em(Some((px.max(1.0) as u16, px.max(1.0) as u16)));
    // rustybuzz 位置为字体单位（upem）；缩放到 px。
    let upem = f.units_per_em().max(1) as f32;
    let scale = px / upem;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    buffer.push_str(seg);
    let out = rustybuzz::shape(&f, &[], buffer);
    let infos = out.glyph_infos();
    let positions = out.glyph_positions();
    let mut glyphs = Vec::with_capacity(infos.len());
    let mut width = 0i32;
    for (gi, gp) in infos.iter().zip(positions.iter()) {
        let adv = fround(gp.x_advance as f32 * scale);
        glyphs.push(GPos {
            gid: gi.glyph_id as u16,
            cluster: gi.cluster as usize,
            x: fround(gp.x_offset as f32 * scale),
            y: fround(gp.y_offset as f32 * scale),
            adv,
        });
        width += adv;
    }
    let run = Run { font, rtl, glyphs, width };
    unsafe {
        let idx = RUN_HEAD % RUN_CACHE_SLOTS;
        RUN_HEAD += 1;
        RUN_CACHE[idx] = Some(RunCacheSlot { hash, px_bits, rtl, runs: vec![run.clone()] });
    }
    Some(run)
}

/// 对一段文本（逻辑顺序）按字体分段并 shaping，返回各 run（视觉顺序字形）。
/// `span_base`：这段文本在整段中的起始字节偏移（用于簇映射）。
fn shape_span(text: &str, span_base: usize, px: f32, rtl: bool) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let (cp, n) = utf8_next(bytes, i);
        let font = pick_font(cp);
        let mut j = i + n;
        while j < bytes.len() {
            let (cp2, n2) = utf8_next(bytes, j);
            if pick_font(cp2) != font {
                break;
            }
            j += n2;
        }
        if let Some(mut run) = shape_segment(&text[i..j], font, px, rtl) {
            if span_base != 0 {
                for g in run.glyphs.iter_mut() {
                    g.cluster += span_base + i;
                }
            } else {
                for g in run.glyphs.iter_mut() {
                    g.cluster += i;
                }
            }
            runs.push(run);
        }
        i = j;
    }
    runs
}

// ==================== BiDi（UAX #9） ====================

fn bidi_levels(text: &str) -> (Vec<u8>, u8) {
    if text.is_empty() {
        return (Vec::new(), 0);
    }
    let info = unicode_bidi::BidiInfo::new(text, None);
    let pl = info.paragraphs.first().map(|p| p.level.number()).unwrap_or(0);
    let mut levels = Vec::with_capacity(text.len());
    for (i, l) in info.levels.iter().enumerate() {
        if i >= text.len() {
            break;
        }
        levels.push(l.number());
    }
    while levels.len() < text.len() {
        levels.push(pl);
    }
    (levels, pl)
}

/// 把字符级别序列切成"方向 run"（连续同级别），返回 (start, end, level) 逻辑顺序。
fn directional_runs(levels: &[u8]) -> Vec<(usize, usize, u8)> {
    let mut runs = Vec::new();
    let mut i = 0usize;
    while i < levels.len() {
        let lvl = levels[i];
        let mut j = i + 1;
        while j < levels.len() && levels[j] == lvl {
            j += 1;
        }
        runs.push((i, j, lvl));
        i = j;
    }
    runs
}

/// UAX #9 L2：把逻辑顺序的方向 run 重排为视觉顺序（从左到右）。
fn reorder_visual(runs: &mut Vec<(usize, usize, u8)>) {
    if runs.is_empty() {
        return;
    }
    let max_level = runs.iter().map(|r| r.2).max().unwrap_or(0);
    for level in (1..=max_level).rev() {
        let mut i = 0usize;
        while i < runs.len() {
            if runs[i].2 >= level {
                let mut j = i;
                while j < runs.len() && runs[j].2 >= level {
                    j += 1;
                }
                runs[i..j].reverse();
                i = j;
            } else {
                i += 1;
            }
        }
    }
}

// ==================== 换行（UAX #14 + CJK 禁则） ====================

/// 禁则：行首禁字符（闭引号/闭括号/句读，前不能断行）。
fn is_line_start_forbidden(cp: u32) -> bool {
    matches!(cp,
        0x3001 | 0x3002 | 0xFF0C | 0xFF0E | 0xFF1A | 0xFF1B | 0xFF1F | 0xFF01
        | 0xFF09 | 0xFF3D | 0xFF5D | 0x3009 | 0x300B | 0x300D | 0x300F | 0x3011
        | 0x3015 | 0x3017 | 0x3019 | 0x301B | 0x301D | 0x301F | 0x2019 | 0x201D
        | 0x2026 | 0x2014 | 0x30FC | 0xFE50..=0xFE52 | 0xFE54..=0xFE57 | 0xFF64 | 0x00BB
    )
}

/// 禁则：行尾禁字符（开引号/开括号，后不能断行）。
fn is_line_end_forbidden(cp: u32) -> bool {
    matches!(cp,
        0x3008 | 0x300A | 0x300C | 0x300E | 0x3010 | 0x3014 | 0x3016 | 0x3018
        | 0x301A | 0xFF08 | 0xFF3B | 0xFF5B | 0x2018 | 0x201C | 0x00AB | 0x2015
    )
}

/// 计算整段文本的候选断点（UAX #14 + 禁则过滤）。返回字节偏移集合。
fn break_opportunities(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut prev_end = 0usize;
    for (i, l) in unicode_linebreak::linebreaks(text) {
        if l == unicode_linebreak::BreakOpportunity::Mandatory || l == unicode_linebreak::BreakOpportunity::Allowed {
            if i == 0 || i >= bytes.len() {
                continue;
            }
            let (cp_after, _) = utf8_next(bytes, i);
            let (cp_before, _) = utf8_prev(bytes, i);
            if is_line_start_forbidden(cp_after) || is_line_end_forbidden(cp_before) {
                continue;
            }
            // 空白后断行：从断点回溯到空白之后
            let mut bi = i;
            while bi > prev_end && bytes[bi - 1] == b' ' {
                bi -= 1;
            }
            out.push(bi);
            prev_end = i;
        }
    }
    out
}

// ==================== 布局（含整段缓存） ====================

const LINE_CACHE_SLOTS: usize = 16;

struct LineCacheSlot {
    hash: u64,
    px_bits: u32,
    max_w: i32,
    lang: u8,
    lines: Vec<Line>,
}

static mut LINE_CACHE: [Option<LineCacheSlot>; LINE_CACHE_SLOTS] = [const { None }; LINE_CACHE_SLOTS];
static mut LINE_HEAD: usize = 0;

/// 行高（px）。
pub fn line_height(px: f32) -> i32 {
    fround(px * 0.9) + fround(px * 0.25)
}

fn lang_code(lang: Lang) -> u8 {
    match lang {
        Lang::Zh => 0,
        Lang::En => 1,
        Lang::Ar => 2,
    }
}

/// 单行布局：整段文本 → 视觉顺序 runs。
pub fn layout_line(text: &str, px: f32, lang: Lang) -> Line {
    let (asc, desc) = (fround(px * 0.9), fround(px * 0.25));
    if text.is_empty() {
        return Line { runs: Vec::new(), width: 0, ascent: asc, descent: desc, px };
    }
    let (levels, _pl) = bidi_levels(text);
    let mut druns = directional_runs(&levels);
    reorder_visual(&mut druns);
    let mut runs: Vec<Run> = Vec::new();
    let mut width = 0i32;
    for (s, e, lvl) in druns {
        let rtl = lvl & 1 == 1;
        for r in shape_span(&text[s..e], s, px, rtl) {
            width += r.width;
            runs.push(r);
        }
    }
    Line { runs, width, ascent: asc, descent: desc, px }
}

/// 段落布局（自动换行）：UAX #14 + 禁则 → 多行。整段结果缓存。
pub fn layout_paragraph(text: &str, px: f32, max_width: i32, lang: Lang) -> Vec<Line> {
    let hash = fnv1a(text.as_bytes());
    let px_bits = px.to_bits();
    let lc = lang_code(lang);
    unsafe {
        for i in 0..LINE_CACHE_SLOTS {
            if let Some(slot) = &LINE_CACHE[i] {
                if slot.hash == hash && slot.px_bits == px_bits && slot.max_w == max_width && slot.lang == lc {
                    return slot.lines.clone();
                }
            }
        }
    }
    let lines = layout_paragraph_uncached(text, px, max_width, lang);
    unsafe {
        let idx = LINE_HEAD % LINE_CACHE_SLOTS;
        LINE_HEAD += 1;
        LINE_CACHE[idx] = Some(LineCacheSlot { hash, px_bits, max_w: max_width, lang: lc, lines: lines.clone() });
    }
    lines
}

fn layout_paragraph_uncached(text: &str, px: f32, max_width: i32, lang: Lang) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    if text.is_empty() {
        lines.push(layout_line("", px, lang));
        return lines;
    }
    for para in text.split('\n') {
        if para.is_empty() {
            lines.push(layout_line("", px, lang));
            continue;
        }
        let full_width: i32 = shape_span(para, 0, px, false).iter().map(|r| r.width).sum();
        if full_width <= max_width || max_width <= 0 {
            lines.push(layout_line(para, px, lang));
            continue;
        }
        let opps = break_opportunities(para);
        let bytes = para.as_bytes();
        let mut start = 0usize;
        let mut cur = start;
        let measure = |s: usize, e: usize| -> i32 {
            shape_span(&para[s..e], 0, px, false).iter().map(|r| r.width).sum()
        };
        loop {
            if cur >= bytes.len() {
                if start >= bytes.len() {
                    break;
                }
                lines.push(layout_line(&para[start..], px, lang));
                break;
            }
            let next_break = opps.iter().find(|&&b| b > cur).copied().unwrap_or(bytes.len());
            let w = measure(start, next_break);
            if w <= max_width {
                cur = next_break;
            } else if cur > start {
                let mut end = cur;
                while end > start && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
                    end -= 1;
                }
                lines.push(layout_line(&para[start..end], px, lang));
                let mut ns = cur;
                while ns < bytes.len() && (bytes[ns] == b' ' || bytes[ns] == b'\t') {
                    ns += 1;
                }
                start = ns;
                cur = ns;
            } else {
                // 单个断点内超宽（长单词/长 CJK 串）：按字符切
                let (_, n) = utf8_next(bytes, start);
                lines.push(layout_line(&para[start..start + n], px, lang));
                start += n;
                cur = start;
            }
        }
    }
    lines
}

/// 文本宽度（shaping 后，px；含 BiDi 方向正确性）。
pub fn text_width(text: &str, px: f32, lang: Lang) -> i32 {
    if text.is_empty() {
        return 0;
    }
    layout_line(text, px, lang).width
}

// ==================== 字形缓存与光栅化 ====================

/// 单色字形（fontdue 灰度）。
pub struct MonoGlyph {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub data: &'static [u8],
}

/// Emoji 彩色字形（RGBA）。
pub struct ColorGlyph {
    pub width: u32,
    pub height: u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
    pub data: &'static [u8],
}

pub enum GlyphImg {
    Mono(MonoGlyph),
    Color(ColorGlyph),
    /// 空白占位（adv 已知，无位图）。
    Empty { adv: i32 },
}

const MONO_CACHE_SLOTS: usize = 384;
const MONO_MAX_BYTES: usize = 4096;
const EMOJI_CACHE_SLOTS: usize = 48;
const EMOJI_MAX_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy)]
struct MonoSlot {
    key: u64,
    xmin: i32,
    ymin: i32,
    width: u16,
    height: u16,
    len: u16,
    data: [u8; MONO_MAX_BYTES],
}

const EMPTY_MONO: MonoSlot = MonoSlot {
    key: 0, xmin: 0, ymin: 0, width: 0, height: 0, len: 0, data: [0; MONO_MAX_BYTES],
};

static mut MONO_CACHE: [MonoSlot; MONO_CACHE_SLOTS] = [EMPTY_MONO; MONO_CACHE_SLOTS];
static mut MONO_HEAD: usize = 0;

#[derive(Clone, Copy)]
struct EmojiSlot {
    key: u64,
    width: u16,
    height: u16,
    bearing_x: i32,
    bearing_y: i32,
    len: u32,
    data: [u8; EMOJI_MAX_BYTES],
}

const EMPTY_EMOJI: EmojiSlot = EmojiSlot {
    key: 0, width: 0, height: 0, bearing_x: 0, bearing_y: 0, len: 0, data: [0; EMOJI_MAX_BYTES],
};

static mut EMOJI_CACHE: [EmojiSlot; EMOJI_CACHE_SLOTS] = [EMPTY_EMOJI; EMOJI_CACHE_SLOTS];
static mut EMOJI_HEAD: usize = 0;

fn mono_key(font: usize, gid: u16, px: f32) -> u64 {
    ((font as u64) << 56) | ((gid as u64) << 40) | (px.to_bits() as u64)
}

fn emoji_key(gid: u16, h: u32) -> u64 {
    ((gid as u64) << 16) | (h as u64 & 0xFFFF)
}

unsafe fn borrow_static<T>(p: *const T, len: usize) -> &'static [T] {
    core::slice::from_raw_parts(p, len)
}

/// 取字形位图（单色/彩色），命中缓存。
pub fn glyph_image(font: usize, gid: u16, px: f32) -> Option<GlyphImg> {
    if font == FONT_EMOJI {
        return emoji_glyph(gid, px);
    }
    mono_glyph(font, gid, px)
}

fn mono_glyph(font: usize, gid: u16, px: f32) -> Option<GlyphImg> {
    if !unsafe { FONT_READY[font] } {
        return None;
    }
    let key = mono_key(font, gid, px);
    unsafe {
        for i in 0..MONO_CACHE_SLOTS {
            let s = &MONO_CACHE[i];
            if s.key == key {
                let n = s.len as usize;
                return Some(GlyphImg::Mono(MonoGlyph {
                    xmin: s.xmin,
                    ymin: s.ymin,
                    width: s.width as usize,
                    height: s.height as usize,
                    data: borrow_static(s.data.as_ptr(), n),
                }));
            }
        }
    }
    let f = due(font)?;
    let (m, data) = f.rasterize_indexed(gid, px);
    if data.len() > MONO_MAX_BYTES {
        return None;
    }
    unsafe {
        let idx = MONO_HEAD % MONO_CACHE_SLOTS;
        MONO_HEAD += 1;
        let mut slot = MonoSlot {
            key,
            xmin: m.xmin,
            ymin: m.ymin,
            width: m.width as u16,
            height: m.height as u16,
            len: data.len() as u16,
            data: [0; MONO_MAX_BYTES],
        };
        slot.data[..data.len()].copy_from_slice(&data);
        MONO_CACHE[idx] = slot;
        let c = &MONO_CACHE[idx];
        let n = c.len as usize;
        Some(GlyphImg::Mono(MonoGlyph {
            xmin: c.xmin,
            ymin: c.ymin,
            width: c.width as usize,
            height: c.height as usize,
            data: borrow_static(c.data.as_ptr(), n),
        }))
    }
}

fn emoji_glyph(gid: u16, px: f32) -> Option<GlyphImg> {
    let font = emoji_font()?;
    let target_h = ((px * 2.0).max(8.0) as u32).min(32);
    let key = emoji_key(gid, target_h);
    unsafe {
        for i in 0..EMOJI_CACHE_SLOTS {
            let s = &EMOJI_CACHE[i];
            if s.key == key {
                let n = s.len as usize;
                return Some(GlyphImg::Color(ColorGlyph {
                    width: s.width as u32,
                    height: s.height as u32,
                    bearing_x: s.bearing_x,
                    bearing_y: s.bearing_y,
                    data: borrow_static(s.data.as_ptr(), n),
                }));
            }
        }
    }
    let bmp = font.glyph(gid, target_h)?;
    if bmp.rgba.len() > EMOJI_MAX_BYTES {
        return None;
    }
    unsafe {
        let idx = EMOJI_HEAD % EMOJI_CACHE_SLOTS;
        EMOJI_HEAD += 1;
        let mut slot = EmojiSlot {
            key,
            width: bmp.width as u16,
            height: bmp.height as u16,
            bearing_x: bmp.bearing_x,
            bearing_y: bmp.bearing_y,
            len: bmp.rgba.len() as u32,
            data: [0; EMOJI_MAX_BYTES],
        };
        slot.data[..bmp.rgba.len()].copy_from_slice(&bmp.rgba);
        EMOJI_CACHE[idx] = slot;
        let c = &EMOJI_CACHE[idx];
        let n = c.len as usize;
        Some(GlyphImg::Color(ColorGlyph {
            width: c.width as u32,
            height: c.height as u32,
            bearing_x: c.bearing_x,
            bearing_y: c.bearing_y,
            data: borrow_static(c.data.as_ptr(), n),
        }))
    }
}

/// 字体是否包含某码点（用于 .notdef 规避）。
pub fn font_has_glyph(font: usize, cp: u32) -> bool {
    if !unsafe { FONT_READY[font] } {
        return false;
    }
    let Some(ch) = char::from_u32(cp) else {
        return false;
    };
    if font == FONT_EMOJI {
        let Some(f) = emoji_font() else {
            return false;
        };
        return face(font).ttf.glyph_index(ch).map(|g| f.has_glyph(g.0)).unwrap_or(false);
    }
    face(font).ttf.glyph_index(ch).is_some()
}

// ==================== 绘制 ====================

/// 把前景色按 alpha 混合到背景色（0x00RRGGBB）。
#[inline]
pub fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    if a == 0 {
        return bg;
    }
    if a == 255 {
        return fg;
    }
    let a = a as u32;
    let ia = 255 - a;
    let r = ((bg >> 16 & 0xFF) * ia + (fg >> 16 & 0xFF) * a) / 255;
    let g = ((bg >> 8 & 0xFF) * ia + (fg >> 8 & 0xFF) * a) / 255;
    let b = ((bg & 0xFF) * ia + (fg & 0xFF) * a) / 255;
    (r << 16) | (g << 8) | b
}

/// 绘制单行（runs 为视觉顺序）。`y` 为基线 y。
/// `bg_at` 返回某像素背景色（抗锯齿混合）；`put` 写最终颜色。
pub fn draw_line(
    x: i32,
    y: i32,
    line: &Line,
    color: u32,
    bg_at: &mut dyn FnMut(i32, i32) -> u32,
    put: &mut dyn FnMut(i32, i32, u32),
) {
    let px = if line.px > 0.0 { line.px } else { FONT_PX };
    let mut cx = x;
    for run in line.runs.iter() {
        for g in run.glyphs.iter() {
            match glyph_image(run.font, g.gid, px) {
                Some(GlyphImg::Mono(m)) => {
                    let gx = cx + m.xmin + g.x;
                    let gy = y + m.ymin + g.y;
                    for row in 0..m.height {
                        for col in 0..m.width {
                            let a = m.data[row * m.width + col];
                            if a != 0 {
                                let bg = bg_at(gx + col as i32, gy + row as i32);
                                put(gx + col as i32, gy + row as i32, blend(bg, color, a));
                            }
                        }
                    }
                }
                Some(GlyphImg::Color(c)) => {
                    let gx = cx + c.bearing_x + g.x;
                    let gy = y + c.bearing_y + g.y;
                    let (w, h) = (c.width as usize, c.height as usize);
                    for row in 0..h {
                        for col in 0..w {
                            let idx = (row * w + col) * 4;
                            let a = c.data[idx + 3];
                            if a != 0 {
                                let r = c.data[idx];
                                let gg = c.data[idx + 1];
                                let b = c.data[idx + 2];
                                put(gx + col as i32, gy + row as i32, (r as u32) << 16 | (gg as u32) << 8 | b as u32);
                            }
                        }
                    }
                }
                Some(GlyphImg::Empty { .. }) | None => {}
            }
            cx += g.adv;
        }
    }
}

/// 绘制单行文本，返回行宽。
pub fn draw_text(
    x: i32,
    y: i32,
    text: &str,
    px: f32,
    color: u32,
    bg: u32,
    put: &mut dyn FnMut(i32, i32, u32),
) -> i32 {
    let line = layout_line(text, px, Lang::En);
    draw_line(x, y, &line, color, &mut |_, _| bg, put);
    line.width
}

/// 绘制自动换行文本，返回总高度。`y` 为首行基线。
pub fn draw_text_wrapped(
    x: i32,
    y: i32,
    max_width: i32,
    text: &str,
    px: f32,
    color: u32,
    bg: u32,
    put: &mut dyn FnMut(i32, i32, u32),
) -> i32 {
    let lines = layout_paragraph(text, px, max_width, Lang::En);
    let lh = line_height(px);
    let mut cy = y;
    for l in lines.iter() {
        draw_line(x, cy, l, color, &mut |_, _| bg, put);
        cy += lh;
    }
    cy - y
}

// ==================== 竖排（预留 + 基础实现） ====================

pub struct ColumnItem {
    pub font: usize,
    pub gid: u16,
    pub cp: u32,
    /// 是否旋转（拉丁字符竖排时旋转 90°）。
    pub rotate: bool,
    pub advance: i32,
}

pub struct Column {
    pub items: Vec<ColumnItem>,
    pub width: i32,
    pub height: i32,
}

impl Default for Column {
    fn default() -> Self {
        Column { items: Vec::new(), width: 0, height: 0 }
    }
}

/// 竖排布局（中日韩传统书写方向）。max_height 为列高上限。
pub fn layout_vertical(text: &str, px: f32, max_height: i32) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    let bytes = text.as_bytes();
    let lh = line_height(px);
    let mut i = 0usize;
    let mut cur = Column { items: Vec::new(), width: 0, height: 0 };
    let mut h = 0i32;
    while i < bytes.len() {
        let (cp, n) = utf8_next(bytes, i);
        if cp == b'\n' as u32 {
            if !cur.items.is_empty() {
                columns.push(core::mem::take(&mut cur));
            }
            h = 0;
            i += n;
            continue;
        }
        let font = pick_font(cp);
        let rotate = !is_cjk_cp(cp) && !is_emoji_cp(cp) && font != FONT_EMOJI;
        if h + lh > max_height && !cur.items.is_empty() {
            columns.push(core::mem::take(&mut cur));
            h = 0;
        }
        let gid = if unsafe { FONT_READY[font] } {
            let ch = char::from_u32(cp).unwrap_or(' ');
            face(font).ttf.glyph_index(ch).map(|g| g.0).unwrap_or(0)
        } else {
            0
        };
        cur.items.push(ColumnItem { font, gid, cp, rotate, advance: lh });
        cur.width = fround(px);
        cur.height += lh;
        h += lh;
        i += n;
    }
    if !cur.items.is_empty() {
        columns.push(cur);
    }
    if columns.is_empty() {
        columns.push(Column { items: Vec::new(), width: 0, height: 0 });
    }
    columns
}

/// 绘制竖排列。`x` 为列中心 x，`y` 为列顶。
pub fn draw_vertical(
    x: i32,
    y: i32,
    col: &Column,
    color: u32,
    bg: u32,
    put: &mut dyn FnMut(i32, i32, u32),
) {
    let mut cy = y;
    for item in col.items.iter() {
        match glyph_image(item.font, item.gid, FONT_PX) {
            Some(GlyphImg::Mono(m)) => {
                if item.rotate {
                    let w = m.width as i32;
                    let hh = m.height as i32;
                    for r in 0..hh {
                        for c in 0..w {
                            let a = m.data[(r * w + c) as usize];
                            if a != 0 {
                                let dx = x - hh / 2 + (hh - 1 - r);
                                let dy = cy + c - w / 2 + m.ymin;
                                put(dx, dy, blend(bg, color, a));
                            }
                        }
                    }
                } else {
                    let gx = x - m.width as i32 / 2 + m.xmin;
                    let gy = cy + m.ymin;
                    for r in 0..m.height {
                        for c in 0..m.width {
                            let a = m.data[r * m.width + c];
                            if a != 0 {
                                put(gx + c as i32, gy + r as i32, blend(bg, color, a));
                            }
                        }
                    }
                }
            }
            Some(GlyphImg::Color(c)) => {
                let gx = x - c.width as i32 / 2 + c.bearing_x;
                let gy = cy + c.bearing_y;
                for r in 0..c.height as usize {
                    for cc in 0..c.width as usize {
                        let idx = (r * c.width as usize + cc) * 4;
                        if c.data[idx + 3] != 0 {
                            let px = ((c.data[idx] as u32) << 16) | ((c.data[idx + 1] as u32) << 8) | c.data[idx + 2] as u32;
                            put(gx + cc as i32, gy + r as i32, px);
                        }
                    }
                }
            }
            Some(GlyphImg::Empty { .. }) | None => {}
        }
        cy += item.advance;
    }
}
