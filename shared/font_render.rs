//! 共享 TTF 平滑字体渲染（基于自研 kunxfont，no_std + alloc）。
//!
//! 字形来自 `fonts/uiputong.ttf`（构建期子集化为 `tools/fonts/cn-subset.ttf`，
//! 随 rootfs 以 `/fonts/cn.ttf` 提供）渲染普通文字；标题使用
//! `fonts/bold.ttf`（子集化为 `tools/fonts/bold-subset.ttf`，随 rootfs 以
//! `/fonts/bold.ttf` 提供）。渲染为抗锯齿灰度位图，由调用方按 alpha 混合到
//! 帧缓冲。内核（`kunxcore/src/ui_framework.rs`）与用户态应用
//! （`shared/app_kit.rs`）共用本模块：各自负责把字体文件读入内存并调用
//! [`font_set`] / [`font_set_bold`] 完成初始化。
//!
//! 另有两槽 Phosphor 图标字体（见 `icons.rs`）：`/fonts/icon-fill.ttf`
//! （Phosphor-Fill 实心，独立图标绘制）与 `/fonts/icon-line.ttf`
//! （Phosphor-Bold 粗线，行内 emoji）。文本路径上 emoji 码点经
//! [`icons::emoji_icon`] 自动映射为图标字形，调用方无感知。

#[path = "icons.rs"]
pub mod icons;

extern crate alloc;

use kunxfont::Font;

/// 默认字号（像素）。ASCII 与 CJK 统一使用该字号，宽度由字体度量决定。
pub const FONT_PX: f32 = 16.0;

static mut FONT: Option<Font> = None;
/// 标题字体（bold.ttf 子集）。未设置时标题渲染回退常规字体。
static mut FONT_BOLD: Option<Font> = None;
/// 图标字体（Phosphor-Fill 实心，应用图标/天气图标等独立绘制）。
static mut FONT_ICON_FILL: Option<Font> = None;
/// 图标字体（Phosphor-Bold 粗线，行内 emoji）。
static mut FONT_ICON_LINE: Option<Font> = None;

/// 设置全局字体（调用方负责从 cn.ttf 读取数据并解析）。
pub fn font_set(font: Font) {
    unsafe {
        FONT = Some(font);
    }
}

/// 设置标题字体（调用方负责从 bold.ttf 读取数据并解析）。
pub fn font_set_bold(font: Font) {
    unsafe {
        FONT_BOLD = Some(font);
    }
}

/// 设置图标字体：`fill=true` 为 Phosphor-Fill（实心），false 为
/// Phosphor-Bold（粗线）。调用方负责从 icon-fill/icon-line.ttf 读取并解析。
pub fn font_set_icon(fill: bool, font: Font) {
    unsafe {
        if fill {
            FONT_ICON_FILL = Some(font);
        } else {
            FONT_ICON_LINE = Some(font);
        }
    }
}

/// 字体是否已就绪（未就绪时文本渲染为空操作）。
pub fn font_ready() -> bool {
    unsafe { FONT.is_some() }
}

/// 标题字体是否已就绪。
pub fn font_bold_ready() -> bool {
    unsafe { FONT_BOLD.is_some() }
}

/// 图标字体是否已就绪（`fill=true` 为实心字体槽）。
pub fn icon_ready(fill: bool) -> bool {
    unsafe {
        if fill {
            FONT_ICON_FILL.is_some()
        } else {
            FONT_ICON_LINE.is_some()
        }
    }
}

/// 图标字体引用（未就绪时 panic；调用方先查 [`icon_ready`]）。
fn font_icon(fill: bool) -> &'static Font {
    unsafe {
        if fill {
            FONT_ICON_FILL.as_ref().expect("font_render：图标字体(fill)未初始化")
        } else {
            FONT_ICON_LINE.as_ref().expect("font_render：图标字体(line)未初始化")
        }
    }
}

/// 已加载字体是否包含该码点的字形（用于回退判断；缺失字形禁止栅格化为
/// .notdef 乱码方块）。emoji 若映射到 Phosphor 图标且图标字体就绪则为 true。
pub fn has_glyph(cp: u32) -> bool {
    if icons::is_hidden_cp(cp) {
        return false;
    }
    if icons::emoji_icon(cp).is_some() {
        return icon_ready(false);
    }
    if !font_ready() {
        return false;
    }
    match char::from_u32(cp) {
        Some(ch) => {
            if !font().has_glyph(ch) {
                lazy_try_full(false);
                return font().has_glyph(ch);
            }
            true
        }
        None => false,
    }
}

/// 标题字体是否包含该码点的字形（emoji 经图标字体渲染时同 [`has_glyph`]）。
pub fn has_glyph_bold(cp: u32) -> bool {
    if icons::is_hidden_cp(cp) {
        return false;
    }
    if icons::emoji_icon(cp).is_some() {
        return icon_ready(false);
    }
    if !font_bold_ready() {
        return false;
    }
    match char::from_u32(cp) {
        Some(ch) => {
            if !font_bold().has_glyph(ch) {
                lazy_try_full(true);
                return font_bold().has_glyph(ch);
            }
            true
        }
        None => false,
    }
}

/// 图标字体是否包含该码点的字形。
pub fn has_glyph_icon(fill: bool, cp: u32) -> bool {
    if !icon_ready(fill) {
        return false;
    }
    match char::from_u32(cp) {
        Some(ch) => font_icon(fill).has_glyph(ch),
        None => false,
    }
}

fn font() -> &'static Font {
    unsafe { FONT.as_ref().expect("font_render：字体未初始化") }
}

fn font_bold() -> &'static Font {
    unsafe { FONT_BOLD.as_ref().expect("font_render：粗体字体未初始化") }
}

/// 从 cn.ttf 字节解析字体。失败返回 None。
pub fn font_from_bytes(data: &[u8]) -> Option<Font> {
    Font::from_bytes(data).ok()
}

// ==================== 缺字形懒加载兜底（仅用户态） ====================
//
// 构建期子集按「源码字符串字面量」裁剪（tools/make_fonts.py），覆盖全部
// 系统 UI 文案，但运行期拼出来的字符（用户数据：联系人名/笔记内容/文件名）
// 可能不在其中。首次栅格化遇到缺字形时，一次性同步加载全量 /fonts/cn.ttf、
// /fonts/bold.ttf 顶替子集并清空字形缓存 —— 字体路径永不渲染豆腐块，且
// UI 类应用（文案全在子集内）永远不付这次加载。
// 由 ttf_text::font_init 在用户态开启；内核（ui_framework）共用本模块但
// 不开启，懒加载路径（含 svc 文件读取）在内核镜像里永远不会执行。

static mut LAZY_FULL_ON: bool = false;
static mut FULL_TRIED_CN: bool = false;
static mut FULL_TRIED_BOLD: bool = false;

/// 用户态开启缺字形懒加载（内核不调用）。幂等。
pub fn lazy_full_enable() {
    unsafe { LAZY_FULL_ON = true; }
}

/// 懒加载是否已尝试过（cn, bold）——加载成败均只试一次，避免逐帧重试。
fn lazy_tried(bold: bool) -> bool {
    unsafe {
        let tried = if bold { &mut FULL_TRIED_BOLD } else { &mut FULL_TRIED_CN };
        if *tried {
            return true;
        }
        *tried = true;
        false
    }
}

/// 子集缺该字形时的一次性全量兜底：读全量字体、替换字体槽、清字形缓存
/// （缺字形时缓存的 notdef 结果必须失效）。非用户态（未开启）立即返回。
fn lazy_try_full(bold: bool) {
    unsafe {
        if !LAZY_FULL_ON || lazy_tried(bold) {
            return;
        }
    }
    let data = lazy_read_font(if bold { b"/fonts/bold.ttf" } else { b"/fonts/cn.ttf" });
    if data.is_empty() {
        return;
    }
    if let Some(f) = font_from_bytes(&data) {
        if bold {
            font_set_bold(f);
        } else {
            font_set(f);
        }
        unsafe {
            // 全量字体字形度量可能与子集完全一致（同源字体），但缓存里可能
            // 已有缺字形时写入的 notdef 槽：整表清零（key 恒非 0，0 = 空槽）。
            for k in GLYPH_KEYS.iter_mut() {
                *k = 0;
            }
        }
    }
}

/// 最小文件读取（svc 直连，仅懒加载路径使用）。文件不存在/读失败返回空。
fn lazy_read_font(path: &[u8]) -> alloc::vec::Vec<u8> {
    const SYS_OPENAT: usize = 56;
    const SYS_READ: usize = 63;
    const SYS_CLOSE: usize = 57;
    const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
    let fd = unsafe {
        let mut p = [0u8; 32];
        let n = path.len().min(p.len() - 1);
        p[..n].copy_from_slice(path);
        syscall3_lazy(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, 0)
    };
    if (fd as isize) < 0 {
        return alloc::vec::Vec::new();
    }
    let mut data = alloc::vec::Vec::new();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = unsafe { syscall3_lazy(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len()) };
        if n == 0 || (n as isize) < 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    unsafe { syscall1_lazy(SYS_CLOSE, fd) };
    data
}

#[inline(always)]
unsafe fn syscall1_lazy(n: usize, a1: usize) -> usize {
    let ret: usize;
    core::arch::asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3_lazy(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    core::arch::asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

/// 解码 `s[i..]` 处的下一个 UTF-8 码点，返回 `(码点, 字节数)`。
/// `i` 越界或非法字节时返回 `(0, 1)`（渲染为空位）。
/// 浮点四舍五入到整数（no_std target 的 core 可能不含 f32::round）。
#[inline]
fn fround(v: f32) -> i32 {
    if v >= 0.0 {
        (v + 0.5) as i32
    } else {
        (v - 0.5) as i32
    }
}

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

/// 行度量 `(ascent, descent)`（像素）：基线以上/以下的高度。ascent 为正，descent 为负。
pub fn line_metrics(px: f32) -> (i32, i32) {
    if !font_ready() {
        // 字体未就绪时退回估算度量，避免 expect panic（首帧早于字体加载的路径）。
        return ((px * 0.8) as i32, -(px * 0.2) as i32);
    }
    match font().horizontal_line_metrics(px) {
        Some(m) => (fround(m.ascent), fround(m.descent)),
        None => ((px * 0.8) as i32, -(px * 0.2) as i32),
    }
}

/// 标题字体的行度量（像素）。标题字体未就绪时退回常规字体度量。
pub fn bold_line_metrics(px: f32) -> (i32, i32) {
    if !font_bold_ready() {
        return line_metrics(px);
    }
    match font_bold().horizontal_line_metrics(px) {
        Some(m) => (fround(m.ascent), fround(m.descent)),
        None => ((px * 0.8) as i32, -(px * 0.2) as i32),
    }
}

/// 把「行顶 y（版式坐标）」换算为某字形位图的绘制顶部。字形的 `ymin` 是
/// **位图底边**相对基线的偏移（负值 = 底边在基线下方），位图从底边向上共
/// `height` 行，因此：顶部 = 行顶 + 常规字体 ascent − (−ymin) − (height−1)
/// = 行顶 + ascent − ymin − height + 1。所有 TTF 字形按此基线统一对齐，
/// ASCII/CJK/标点混排不再错位。
pub fn glyph_draw_top(line_top: i32, ymin: i32, height: usize, px: f32) -> i32 {
    line_top + line_metrics(px).0 - ymin - height as i32 + 1
}

/// 标题字体版：行顶 + 标题字体 ascent − 字形 ymin − height + 1。
pub fn glyph_draw_top_bold(line_top: i32, ymin: i32, height: usize, px: f32) -> i32 {
    line_top + bold_line_metrics(px).0 - ymin - height as i32 + 1
}

/// 图标字体的行度量 `(ascent, descent)`（像素）。图标字体未就绪时退回估算。
pub fn icon_line_metrics(fill: bool, px: f32) -> (i32, i32) {
    if !icon_ready(fill) {
        return ((px * 0.8) as i32, -(px * 0.2) as i32);
    }
    match font_icon(fill).horizontal_line_metrics(px) {
        Some(m) => (fround(m.ascent), fround(m.descent)),
        None => ((px * 0.8) as i32, -(px * 0.2) as i32),
    }
}

/// 图标字体版：行顶 + 图标字体 ascent − 字形 ymin − height + 1。
/// （独立绘制图标时更常用「包围盒居中」，见 [`rasterize_icon`]。）
pub fn glyph_draw_top_icon(fill: bool, line_top: i32, ymin: i32, height: usize, px: f32) -> i32 {
    line_top + icon_line_metrics(fill, px).0 - ymin - height as i32 + 1
}

/// 单个字符的栅格化结果。
///
/// `data` 借用字形缓存槽（或超大字形备用缓冲）中的位图，不持有所有权：
/// 调用方在绘制期间不得再次触发同槽覆盖（实际绘制路径逐个字形顺序消费，
/// 无并发/嵌套栅格化，因此借用安全），且渲染本身不再产生堆分配 ——
/// 此前命中缓存也逐字拷贝 Vec，bump 堆永不回收，反复重绘会把堆撑爆。
pub struct Glyph {
    /// 位图最左列相对基线的像素偏移（可为负）。
    pub xmin: i32,
    /// 位图最底行相对基线的像素偏移（可为负，基线下方为负）。
    pub ymin: i32,
    /// 位图宽/高（像素）。
    pub width: usize,
    pub height: usize,
    /// 推进宽度（像素）。
    pub advance: i32,
    /// 灰度位图（0=透明，255=不透明），行优先。
    pub data: &'static [u8],
}

// ==================== 字形缓存 ====================
//
// 轮廓栅格化涉及解析和填充，而鼠标移动会触发整屏重绘，每帧对
// 每个字形重新栅格化会让光标严重拖影/不跟手。字体在初始化后不可变，用一个
// 固定容量环缓存（key = 码点 + 字号）保存最近用过的字形，命中时直接拷贝
// 位图，把「每帧几十次栅格化」降为「每次 memcpy」。

const GLYPH_CACHE_SLOTS: usize = 256;
const GLYPH_MAX_BYTES: usize = 1024; // 16px 下 CJK 字形 ≤ 17x17=289 字节，余量充足

#[derive(Clone, Copy)]
struct GlyphSlot {
    key: u64, // (px.to_bits() << 32) | cp
    xmin: i32,
    ymin: i32,
    width: u16,
    height: u16,
    advance: i32,
    len: u16,
    data: [u8; GLYPH_MAX_BYTES],
}

const EMPTY_SLOT: GlyphSlot = GlyphSlot {
    key: 0,
    xmin: 0,
    ymin: 0,
    width: 0,
    height: 0,
    advance: 0,
    len: 0,
    data: [0; GLYPH_MAX_BYTES],
};

static mut GLYPH_CACHE: [GlyphSlot; GLYPH_CACHE_SLOTS] = [EMPTY_SLOT; GLYPH_CACHE_SLOTS];
static mut GLYPH_CACHE_HEAD: usize = 0;
/// 键旁路数组：查找扫描落在连续 2KB（常驻 L1）而不是 256 个 1KB 槽位的
/// 跨步访问（~256KB 覆盖，每次查找都是全缓存扫描）。与 GLYPH_CACHE[i].key
/// 同步写入，查找先扫本数组、命中才触碰槽位。
static mut GLYPH_KEYS: [u64; GLYPH_CACHE_SLOTS] = [0; GLYPH_CACHE_SLOTS];

/// 超大字形（> GLYPH_MAX_BYTES）备用缓冲：不写缓存，借用本缓冲返回。
/// 容量需覆盖 OOBE 大图标字号（124px 字形位图 ~17KB，取 64KB 富余）。
const GLYPH_SCRATCH_BYTES: usize = 65536;
static mut GLYPH_SCRATCH: [u8; GLYPH_SCRATCH_BYTES] = [0; GLYPH_SCRATCH_BYTES];
static mut GLYPH_SCRATCH_LEN: usize = 0;

/// 返回缓存槽/备用缓冲中位图的 `'static` 借用（槽内数组在 bump 堆上永不释放，
/// 因此指针生命周期安全；调用方顺序消费，使用期间不会被同槽覆盖）。
unsafe fn glyph_data_borrow(ptr: *const u8, len: usize) -> &'static [u8] {
    core::slice::from_raw_parts(ptr, len)
}

fn cache_key(ch: char, px: f32) -> u64 {
    ((px.to_bits() as u64) << 32) | ch as u32 as u64
}

// 字体选择器（缓存键区分不同字体的字形位图，避免互相污染）。
// 键布局：bit 0..21 码点（char ≤ 0x10FFFF）| bit 21..23 选择器 |
// bit 32..64 字号 f32 bits。选择器必须避开 px_bits 高位——px_bits 占据
// 32..64，把选择器放在 62..64 会与 px 指数位重叠，常规/标题/图标字形
// 在缓存中互撞（同 px 下 SEL_CN 与 SEL_BOLD 键相同）。
const SEL_CN: u64 = 0;
const SEL_BOLD: u64 = 1;
const SEL_ICON_FILL: u64 = 2;
const SEL_ICON_LINE: u64 = 3;

/// 带字体选择的缓存键。
fn cache_key_sel(sel: u64, ch: char, px: f32) -> u64 {
    ((px.to_bits() as u64) << 32) | (sel << 21) | ch as u32 as u64
}

/// 查缓存：命中返回 Some(缓存的 advance)，未命中返回 None。
fn cache_lookup_advance(key: u64) -> Option<i32> {
    unsafe {
        for i in 0..GLYPH_CACHE_SLOTS {
            if GLYPH_KEYS[i] == key {
                return Some(GLYPH_CACHE[i].advance);
            }
        }
    }
    None
}

/// 写入缓存（环覆盖最旧槽位）。
fn cache_store(
    key: u64,
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    advance: i32,
    data: &[u8],
) {
    if data.len() > GLYPH_MAX_BYTES {
        return; // 超大字形（如高倍缩放）不缓存
    }
    unsafe {
        let idx = GLYPH_CACHE_HEAD % GLYPH_CACHE_SLOTS;
        GLYPH_CACHE_HEAD += 1;
        // 就地写入字段（此前在栈上构造 1KB 的零初始化临时槽再整体拷贝）。
        let slot = &mut GLYPH_CACHE[idx] as *mut GlyphSlot;
        (*slot).key = key;
        (*slot).xmin = xmin;
        (*slot).ymin = ymin;
        (*slot).width = width as u16;
        (*slot).height = height as u16;
        (*slot).advance = advance;
        (*slot).len = data.len() as u16;
        // 经 addr_of_mut 取字段裸指针再拷贝（裸指针解引用上的隐式 autoref
        // 在当前 edition 是硬错误）。
        let dptr = core::ptr::addr_of_mut!((*slot).data) as *mut u8;
        core::ptr::copy_nonoverlapping(data.as_ptr(), dptr, data.len());
        GLYPH_KEYS[idx] = key;
    }
}

/// 隐藏字符（VS16/ZWJ/肤色修饰/区域指示符）的零宽空位图：占码位不显示、
/// 不产生推进宽度（"❤️" = ❤ + VS16，VS16 若占位会在心形后留缺口）。
static EMPTY_GLYPH_DATA: [u8; 0] = [];

fn empty_glyph() -> Glyph {
    Glyph {
        xmin: 0,
        ymin: 0,
        width: 0,
        height: 0,
        advance: 0,
        data: &EMPTY_GLYPH_DATA,
    }
}

/// 文本字符的字体解析：emoji 映射 Phosphor 图标（line 槽，粗线适合行内），
/// 其余按常规/标题字体。返回 (缓存选择器, 实际栅格化字符)。
fn resolve_sel(bold: bool, cp: u32) -> (u64, char) {
    if let Some(icp) = icons::emoji_icon(cp) {
        if icon_ready(false) {
            if let Some(c) = char::from_u32(icp) {
                return (SEL_ICON_LINE, c);
            }
        }
    }
    (
        if bold { SEL_BOLD } else { SEL_CN },
        char::from_u32(cp).unwrap_or(' '),
    )
}

/// 栅格化单个字符（灰度），带字形缓存。字体未就绪时返回 None。
///
/// 返回的位图借用缓存（零拷贝、零堆分配）；超大字形（> GLYPH_MAX_BYTES）
/// 借用备用缓冲 GLYPH_SCRATCH（当前渲染字号下不会触发）。
pub fn rasterize_char(ch: char, px: f32) -> Option<Glyph> {
    if icons::is_hidden_cp(ch as u32) {
        return Some(empty_glyph());
    }
    let (sel, ch) = resolve_sel(false, ch as u32);
    rasterize_sel(sel, ch, px)
}

/// 以标题字体（bold.ttf）栅格化单个字符。标题字体未就绪时退回常规字体。
/// emoji 同 [`rasterize_char`]：映射 Phosphor 图标（不受标题字体影响）。
pub fn rasterize_char_bold(ch: char, px: f32) -> Option<Glyph> {
    if icons::is_hidden_cp(ch as u32) {
        return Some(empty_glyph());
    }
    let (sel, ch) = resolve_sel(true, ch as u32);
    if sel == SEL_BOLD && !font_bold_ready() {
        return rasterize_sel(SEL_CN, ch, px);
    }
    rasterize_sel(sel, ch, px)
}

/// 以 Phosphor 图标字体栅格化单个图标码点（`fill=true` 为实心风格，
/// false 为粗线风格）。用于独立图标绘制；行内 emoji 走 [`rasterize_char`]。
pub fn rasterize_icon(fill: bool, cp: u32, px: f32) -> Option<Glyph> {
    let ch = char::from_u32(cp)?;
    rasterize_sel(if fill { SEL_ICON_FILL } else { SEL_ICON_LINE }, ch, px)
}

// ==================== 笔画加粗（小尺寸图标可读性） ====================
//
// 16~24px 的粗线图标笔画只有 ~2px，在彩色圆角底上偏纤细。栅格化后对 alpha
// 做一次受控膨胀（正交邻域 72% / 对角 36% 的半透明环），笔画每侧视觉增粗
// ~0.7px 且边缘仍平滑；几何形状不变。

/// 膨胀结果缓冲（最大支持 ~180x180 字形；OOBE 大图标 124px 栅格化也放得下。
/// 超出时返回 None，调用方回退 [`rasterize_icon`]）。
const THICK_SCRATCH: usize = 65536;
static mut THICK_BUF: [u8; THICK_SCRATCH] = [0; THICK_SCRATCH];

/// 加粗栅格化结果（位图借用 [`THICK_BUF`]，下次调用覆盖——与 GLYPH_SCRATCH
/// 同一借用约定：调用方绘制期间不得再次触发栅格化）。
pub struct ThickGlyph {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance: i32,
    pub data: &'static [u8],
}

/// 以 Phosphor 图标字体栅格化并把笔画加粗约 1px。字形过大放不下缓冲时
/// 返回 None（调用方回退 [`rasterize_icon`]）。
pub fn rasterize_icon_thick(fill: bool, cp: u32, px: f32) -> Option<ThickGlyph> {
    let g = rasterize_icon(fill, cp, px)?;
    let (w, h) = (g.width, g.height);
    if w == 0 || h == 0 || w * h > THICK_SCRATCH {
        return None;
    }
    let src = g.data;
    let dilate = |v: u32| -> u32 { v * 72 / 100 };
    let dilate_diag = |v: u32| -> u32 { v * 36 / 100 };
    unsafe {
        let buf = core::ptr::addr_of_mut!(THICK_BUF) as *mut u8;
        for y in 0..h {
            for x in 0..w {
                let c = src[y * w + x] as u32;
                let mut m = c;
                if x > 0 {
                    m = m.max(dilate(src[y * w + x - 1] as u32));
                }
                if x + 1 < w {
                    m = m.max(dilate(src[y * w + x + 1] as u32));
                }
                if y > 0 {
                    m = m.max(dilate(src[(y - 1) * w + x] as u32));
                }
                if y + 1 < h {
                    m = m.max(dilate(src[(y + 1) * w + x] as u32));
                }
                if x > 0 && y > 0 {
                    m = m.max(dilate_diag(src[(y - 1) * w + x - 1] as u32));
                }
                if x + 1 < w && y > 0 {
                    m = m.max(dilate_diag(src[(y - 1) * w + x + 1] as u32));
                }
                if x > 0 && y + 1 < h {
                    m = m.max(dilate_diag(src[(y + 1) * w + x - 1] as u32));
                }
                if x + 1 < w && y + 1 < h {
                    m = m.max(dilate_diag(src[(y + 1) * w + x + 1] as u32));
                }
                *buf.add(y * w + x) = m as u8;
            }
        }
        Some(ThickGlyph {
            xmin: g.xmin,
            ymin: g.ymin,
            width: w,
            height: h,
            advance: g.advance,
            data: core::slice::from_raw_parts(buf, w * h),
        })
    }
}

fn rasterize_sel(sel: u64, ch: char, px: f32) -> Option<Glyph> {
    if !font_ready() {
        return None;
    }
    let ready = match sel {
        SEL_BOLD => font_bold_ready(),
        SEL_ICON_FILL => icon_ready(true),
        SEL_ICON_LINE => icon_ready(false),
        _ => true,
    };
    if !ready {
        return None;
    }
    let key = cache_key_sel(sel, ch, px);
    // 命中缓存：直接借用槽内位图（不再逐字拷贝 Vec）。
    unsafe {
        for i in 0..GLYPH_CACHE_SLOTS {
            if GLYPH_KEYS[i] == key {
                let slot = &GLYPH_CACHE[i];
                let n = slot.len as usize;
                return Some(Glyph {
                    xmin: slot.xmin,
                    ymin: slot.ymin,
                    width: slot.width as usize,
                    height: slot.height as usize,
                    advance: slot.advance,
                    data: glyph_data_borrow(slot.data.as_ptr(), n),
                });
            }
        }
    }
    // 未命中：缺字形兜底（子集可能不含该字形——用户数据等运行期拼接字符；
    // lazy_try_full 成功后字体槽已替换且缓存已清空，本次栅格化即用全量结果），
    // 然后栅格化并写缓存。
    match sel {
        SEL_BOLD => {
            if !font_bold().has_glyph(ch) {
                lazy_try_full(true);
            }
        }
        SEL_ICON_FILL | SEL_ICON_LINE => {}
        _ => {
            if !font().has_glyph(ch) {
                lazy_try_full(false);
            }
        }
    }
    let (m, data) = match sel {
        SEL_BOLD => font_bold().rasterize(ch, px),
        SEL_ICON_FILL => font_icon(true).rasterize(ch, px),
        SEL_ICON_LINE => font_icon(false).rasterize(ch, px),
        _ => font().rasterize(ch, px),
    }.ok()?;
    let advance = fround(m.advance_width);
    if data.len() <= GLYPH_MAX_BYTES {
        cache_store(key, m.xmin, m.ymin, m.width, m.height, advance, &data);
        // 借用刚写入的槽（GLYPH_CACHE_HEAD 已在 cache_store 中 +1）。
        unsafe {
            let idx = (GLYPH_CACHE_HEAD.wrapping_sub(1)) % GLYPH_CACHE_SLOTS;
            let slot = &GLYPH_CACHE[idx];
            let n = slot.len as usize;
            return Some(Glyph {
                xmin: slot.xmin,
                ymin: slot.ymin,
                width: slot.width as usize,
                height: slot.height as usize,
                advance: slot.advance,
                data: glyph_data_borrow(slot.data.as_ptr(), n),
            });
        }
    }
    // 超大字形：借用一个静态备用缓冲（每次覆盖上一次的内容）。
    unsafe {
        // ★ 超出备用缓冲容量的字形必须放弃（返回 None，调用方走固定 advance
        // 兜底）：截断后若保留原 width/height，消费方 `data[row*width+col]`
        // 索引会越过缓冲 → slice index out of range panic（进程被杀）。
        if data.len() > GLYPH_SCRATCH_BYTES {
            return None;
        }
        let n = data.len().min(GLYPH_SCRATCH_BYTES);
        GLYPH_SCRATCH[..n].copy_from_slice(&data[..n]);
        GLYPH_SCRATCH_LEN = n;
        Some(Glyph {
            xmin: m.xmin,
            ymin: m.ymin,
            width: m.width,
            height: m.height,
            advance,
            data: glyph_data_borrow(GLYPH_SCRATCH.as_ptr(), n),
        })
    }
}

/// 单个码点的推进宽度（像素）。字体未就绪时退回固定宽度（CJK 16 / ASCII 8）。
/// 隐藏字符（VS16/ZWJ 等）为 0；emoji 为其映射图标的推进宽度。
pub fn advance(cp: u32, px: f32) -> i32 {
    if !font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    if icons::is_hidden_cp(cp) {
        return 0;
    }
    let (sel, ch) = resolve_sel(false, cp);
    let key = cache_key_sel(sel, ch, px);
    if let Some(a) = cache_lookup_advance(key) {
        return a;
    }
    if let Some(g) = rasterize_sel(sel, ch, px) {
        return g.advance;
    }
    if cp >= 0x80 {
        16
    } else {
        8
    }
}

/// 标题字体（bold.ttf）下单个码点的推进宽度（像素）。标题字体未就绪时退回常规。
pub fn advance_bold(cp: u32, px: f32) -> i32 {
    if !font_bold_ready() {
        return advance(cp, px);
    }
    if !font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    if icons::is_hidden_cp(cp) {
        return 0;
    }
    let (sel, ch) = resolve_sel(true, cp);
    let key = cache_key_sel(sel, ch, px);
    if let Some(a) = cache_lookup_advance(key) {
        return a;
    }
    if let Some(g) = rasterize_sel(sel, ch, px) {
        return g.advance;
    }
    advance(cp, px)
}

/// 文本宽度（像素）。字体未就绪时退回旧固定宽度布局。
pub fn text_width(s: &[u8], px: f32) -> i32 {
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = utf8_next(s, i);
        w += advance(cp, px);
        i += n;
    }
    w
}

/// 标题字体（bold.ttf）下的文本宽度（像素）。
pub fn text_width_bold(s: &[u8], px: f32) -> i32 {
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = utf8_next(s, i);
        w += advance_bold(cp, px);
        i += n;
    }
    w
}

/// 把前景色 `fg` 按灰度 `a` 混合到背景色 `bg`（颜色为 0x00RRGGBB）。
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
