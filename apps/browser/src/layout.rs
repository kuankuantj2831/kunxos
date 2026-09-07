//! 布局引擎：盒树构建 + 流式布局（块 / 行内 / 简单表格 / static+relative）。
//!
//! - 块级：垂直堆叠，margin/padding/border，宽度自适应（auto 填充），
//!   高度由内容决定
//! - 行内：按可用宽度折行；ASCII 按词折行、CJK 逐字折行；
//!   行盒含基线；行内盒（a/span/b…）背景/边框按行合并为矩形
//! - 表格：简单等宽列布局；单元格块级布局
//! - 定位：static / relative（位移不影响兄弟流）
//! - 不支持：float / absolute / flex / grid（flex/grid 按 block 降级）

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::css::{Display, Position};
use crate::html::{Dom, NodeKind};
use crate::kit::ttf_text::font_render;
use crate::style::{ComputedStyle, Size, StyleTree, TextAlign};
use crate::url::{resolve, Url};

pub const VIEWPORT_W: f32 = 540.0;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ReplacedKind {
    Img,
    TextInput,
    Checkbox,
    Radio,
    Button,
    SubmitButton,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BKind {
    Block,
    Inline,
    Table,
    TableRow,
    TableCell,
    Replaced,
}

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

#[derive(Clone, Debug)]
pub struct Bx {
    pub kind: BKind,
    pub node: Option<usize>,
    pub rect: Rect,
    pub style: ComputedStyle,
    pub children: Vec<usize>,
    pub replaced: Option<ReplacedKind>,
    /// img 在 Layout.images 中的下标。
    pub img: Option<usize>,
    /// 控件在 Layout.controls 中的下标。
    pub ctrl: Option<usize>,
    /// 匿名块（包裹行内内容）。
    pub anon: bool,
}

/// 文本运行（行内布局产物，直接供绘制）。
#[derive(Clone, Copy, Debug)]
pub struct Run {
    pub node: usize,
    pub start: usize,
    pub end: usize,
    pub x: f32,
    pub baseline: f32,
    pub px: f32,
    pub color: u32,
    pub bold: bool,
    pub underline: bool,
    /// 所属行内盒（下划线/命中）；None=匿名块直属文本
    pub inline_box: Option<usize>,
}

/// 行内盒在某一行的背景/边框（跨词合并矩形）。
#[derive(Clone, Copy, Debug)]
pub struct InlineBg {
    pub rect: Rect,
    pub color: u32,
    pub border_w: [f32; 4],
    pub border_color: [u32; 4],
}

#[derive(Clone, Debug)]
pub struct Link {
    pub rect: Rect,
    pub url: String,
}

#[derive(Clone, Debug)]
pub enum ControlKind {
    TextInput { name: String, value_idx: usize, size: usize },
    Checkbox { name: String, checked: bool },
    Radio { name: String, checked: bool },
    Button { label: String, name: String, submit: bool },
}

#[derive(Clone, Debug)]
pub struct Control {
    pub rect: Rect,
    pub kind: ControlKind,
    pub node: usize,
    /// 所属 form 元素下标。
    pub form: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ImageRef {
    pub node: usize,
    pub src: String,
    pub w: f32,
    pub h: f32,
}

pub struct Layout {
    pub boxes: Vec<Bx>,
    pub runs: Vec<Run>,
    pub bgs: Vec<InlineBg>,
    pub links: Vec<Link>,
    pub controls: Vec<Control>,
    pub images: Vec<ImageRef>,
    pub width: f32,
    pub height: f32,
}

impl Layout {
    fn new_box(&mut self, kind: BKind, node: Option<usize>, style: ComputedStyle) -> usize {
        let idx = self.boxes.len();
        self.boxes.push(Bx {
            kind,
            node,
            rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            style,
            children: Vec::new(),
            replaced: None,
            img: None,
            ctrl: None,
            anon: false,
        });
        idx
    }
}

// ==================== 文本测量 ====================

fn advance(cp: u32, px: f32) -> f32 {
    font_render::advance(cp, px) as f32
}

fn text_width(s: &[u8], px: f32) -> f32 {
    font_render::text_width(s, px) as f32
}

fn metrics(px: f32) -> (f32, f32) {
    let (a, d) = font_render::line_metrics(px);
    (a as f32, (-d) as f32)
}

// ==================== 布局入口 ====================

/// `base`：当前文档 URL，用于解析相对链接。`img_size`：按 src 查询图片尺寸
/// （shell 已抓取；未抓到返回 None 用默认尺寸）。
pub fn layout(
    dom: &Dom,
    styles: &StyleTree,
    viewport_w: f32,
    base: &Url,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> Layout {
    let mut lt = Layout {
        boxes: Vec::new(),
        runs: Vec::new(),
        bgs: Vec::new(),
        links: Vec::new(),
        controls: Vec::new(),
        images: Vec::new(),
        width: viewport_w,
        height: 0.0,
    };
    let root_style = styles.styles[dom.root].clone();
    let root = lt.new_box(BKind::Block, Some(dom.root), root_style);
    let mut ctx = BlockCtx { x: 0.0, y: 0.0, content_w: viewport_w };
    let h = layout_block_children(dom, styles, &mut lt, root, dom.root, &mut ctx, 0, img_size);
    lt.boxes[root].rect = Rect { x: 0.0, y: 0.0, w: viewport_w, h };
    apply_relative(&mut lt, root);
    resolve_links(dom, &mut lt, base);
    lt.height = h;
    lt
}

fn resolve_links(dom: &Dom, lt: &mut Layout, base: &Url) {
    for i in 0..lt.boxes.len() {
        if lt.boxes[i].kind != BKind::Inline {
            continue;
        }
        let Some(node) = lt.boxes[i].node else { continue };
        if let Some(href) = dom.nodes[node].attr("href") {
            if let Some(abs) = resolve(base, href) {
                lt.links.push(Link { rect: lt.boxes[i].rect, url: abs.display() });
            }
        }
    }
    lt.links.retain(|l| l.rect.w > 0.0 && l.rect.h > 0.0);
}

/// 块级布局上下文：内容区原点与宽度（border+padding 内）。
struct BlockCtx {
    x: f32,
    y: f32,
    content_w: f32,
}

fn layout_block_children(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    parent_box: usize,
    node: usize,
    ctx: &mut BlockCtx,
    depth: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> f32 {
    if depth > MAX_DEPTH {
        return 0.0;
    }
    let start_y = ctx.y;
    let mut y = ctx.y;
    let mut inline_nodes: Vec<usize> = Vec::new();

    let children = dom.nodes[node].children.clone();
    for &c in &children {
        let st = &styles.styles[c];
        if st.display == Display::None {
            continue;
        }
        // 文本节点恒为行内级；元素按计算样式分块/行内
        let is_text = matches!(dom.nodes[c].kind, NodeKind::Text { .. });
        let blockish = !is_text && (st.display == Display::Block || st.display == Display::Table);
        if blockish {
            if !inline_nodes.is_empty() {
                y += layout_inline_segment(
                    dom, styles, lt, parent_box, &inline_nodes, ctx.x, y, ctx.content_w, depth + 1, img_size,
                );
                inline_nodes.clear();
            }
            ctx.y = y; // 推进上下文，使子元素在正确位置布局
            y += layout_block_child(dom, styles, lt, parent_box, c, ctx, depth + 1, img_size);
        } else {
            inline_nodes.push(c);
        }
    }
    if !inline_nodes.is_empty() {
        y += layout_inline_segment(
            dom, styles, lt, parent_box, &inline_nodes, ctx.x, y, ctx.content_w, depth + 1, img_size,
        );
    }
    y - start_y
}

fn layout_block_child(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    parent_box: usize,
    node: usize,
    ctx: &mut BlockCtx,
    depth: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> f32 {
    let st = styles.styles[node].clone();
    let m = st.margin;
    let p = st.padding;
    let b = st.border_w;
    let x = ctx.x + m[3];
    let y = ctx.y + m[0];
    let avail = (ctx.content_w - m[1] - m[3]).max(0.0);
    let border_w = match st.width {
        Size::Px(w) => w.min(avail),
        Size::Percent(per) => (avail * per / 100.0).min(avail),
        Size::Auto => avail,
    };
    let inner_w = (border_w - b[1] - b[3]).max(0.0);
    let want_h = st.height;

    if st.display == Display::Table {
        let idx = lt.new_box(BKind::Table, Some(node), st);
        lt.boxes[parent_box].children.push(idx);
        let mut inner = BlockCtx { x: x + b[3] + p[3], y: y + b[0] + p[0], content_w: inner_w };
        let content_h = layout_table(dom, styles, lt, idx, node, &mut inner, depth + 1, img_size);
        let h = content_h + p[0] + p[2] + b[0] + b[2];
        lt.boxes[idx].rect = Rect { x, y, w: border_w, h };
        return h + m[0] + m[2];
    }

    let idx = lt.new_box(BKind::Block, Some(node), st);
    lt.boxes[parent_box].children.push(idx);
    let mut inner = BlockCtx { x: x + b[3] + p[3], y: y + b[0] + p[0], content_w: inner_w };
    let content_h = layout_block_children(dom, styles, lt, idx, node, &mut inner, depth + 1, img_size);
    let mut h = content_h + p[0] + p[2] + b[0] + b[2];
    match want_h {
        Size::Px(ph) => h = h.max(ph),
        Size::Percent(per) => h = h.max(inner_w * per / 100.0),
        Size::Auto => {}
    }
    lt.boxes[idx].rect = Rect { x, y, w: border_w, h };
    h + m[0] + m[2]
}

// ==================== 表格布局 ====================

fn layout_table(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    table_box: usize,
    node: usize,
    ctx: &mut BlockCtx,
    depth: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> f32 {
    if depth > MAX_DEPTH {
        return 0.0;
    }
    let mut rows: Vec<usize> = Vec::new();
    for &c in &dom.nodes[node].children {
        let st = &styles.styles[c];
        if st.display == Display::None {
            continue;
        }
        if st.display == Display::TableRow || dom.nodes[c].tag() == "tr" {
            rows.push(c);
        }
    }
    if rows.is_empty() {
        return 0.0;
    }
    let mut cols = 0usize;
    for &r in &rows {
        let n = dom.nodes[r]
            .children
            .iter()
            .filter(|&&c| {
                styles.styles[c].display != Display::None
                    && (styles.styles[c].display == Display::TableCell
                        || dom.nodes[c].tag() == "td"
                        || dom.nodes[c].tag() == "th")
            })
            .count();
        cols = cols.max(n);
    }
    let cols = cols.max(1);
    let cell_w = ctx.content_w / cols as f32;
    let mut y = ctx.y;

    for &r in &rows {
        let rbox = lt.new_box(BKind::TableRow, Some(r), styles.styles[r].clone());
        lt.boxes[table_box].children.push(rbox);
        let mut row_h = 0.0f32;
        let mut cell_i = 0usize;
        let cell_nodes = dom.nodes[r].children.clone();
        for &c in &cell_nodes {
            let st = &styles.styles[c];
            if st.display == Display::None {
                continue;
            }
            if st.display != Display::TableCell && dom.nodes[c].tag() != "td" && dom.nodes[c].tag() != "th" {
                continue;
            }
            let cbox = lt.new_box(BKind::TableCell, Some(c), styles.styles[c].clone());
            lt.boxes[rbox].children.push(cbox);
            let cx = ctx.x + cell_i as f32 * cell_w;
            let mut inner = BlockCtx {
                x: cx + st.border_w[3] + st.padding[3],
                y: y + st.border_w[0] + st.padding[0],
                content_w: (cell_w - st.border_w[1] - st.border_w[3] - st.padding[1] - st.padding[3]).max(0.0),
            };
            let ch = layout_block_children(dom, styles, lt, cbox, c, &mut inner, depth + 1, img_size);
            let h = ch + st.padding[0] + st.padding[2] + st.border_w[0] + st.border_w[2];
            lt.boxes[cbox].rect = Rect { x: cx, y, w: cell_w, h };
            row_h = row_h.max(h);
            cell_i += 1;
        }
        let kids = lt.boxes[rbox].children.clone();
        for cb in kids {
            lt.boxes[cb].rect.h = row_h;
        }
        lt.boxes[rbox].rect = Rect { x: ctx.x, y, w: ctx.content_w, h: row_h };
        y += row_h;
    }
    y - ctx.y
}

// ==================== 行内布局 ====================

#[derive(Clone)]
enum Atom {
    /// 文本片段（整词或单 CJK 字）
    Text { node: usize, start: usize, end: usize, w: f32, px: f32, ibox: Option<usize> },
    /// 空白（可折行点）
    Space { w: f32, px: f32, ibox: Option<usize> },
    /// 替换元素
    Replaced { node: usize, w: f32, h: f32, ibox: Option<usize> },
    Br,
}

/// 行内段布局状态。
struct Line {
    /// (原子, 行内 x 位置)
    atoms: Vec<(Atom, f32)>,
    w: f32,
    has: bool,
}

impl Line {
    fn new() -> Line {
        Line { atoms: Vec::new(), w: 0.0, has: false }
    }
    fn clear(&mut self) {
        self.atoms.clear();
        self.w = 0.0;
        self.has = false;
    }
}

/// 收集单个节点的行内原子（替换元素尺寸/控件/图片在此注册）。
/// - 文本节点 → 折行原子
/// - 行内元素（span/a/b…）→ 建行内盒并递归子节点
/// - 其余（img/input/button/inline-block）→ 替换元素原子
fn collect_node_atoms(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    node: usize,
    ibox: Option<usize>,
    out: &mut Vec<Atom>,
    depth: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match &dom.nodes[node].kind {
        NodeKind::Text { text } => {
            let px = styles.styles[node].font_size;
            split_text_atoms(node, text.as_bytes(), px, ibox, out);
        }
        NodeKind::Element { .. } => {
            let st = &styles.styles[node];
            match st.display {
                Display::None => {}
                Display::Inline => {
                    if dom.nodes[node].tag() == "br" {
                        out.push(Atom::Br);
                    } else {
                        let ib = lt.new_box(BKind::Inline, Some(node), st.clone());
                        let parent = ibox.unwrap_or(0);
                        lt.boxes[parent].children.push(ib);
                        let kids = dom.nodes[node].children.clone();
                        for c in kids {
                            collect_node_atoms(dom, styles, lt, c, Some(ib), out, depth + 1, img_size);
                        }
                    }
                }
                _ => {
                    let (w, h) = replaced_size(dom, styles, lt, node, img_size);
                    out.push(Atom::Replaced { node, w, h, ibox });
                }
            }
        }
    }
}

/// 把文本切成折行原子：空白折叠为单个 Space，ASCII 词为整体，CJK 逐字。
fn split_text_atoms(node: usize, text: &[u8], px: f32, ibox: Option<usize>, out: &mut Vec<Atom>) {
    let mut i = 0usize;
    let n = text.len();
    while i < n {
        let (cp, adv) = font_render::utf8_next(text, i);
        if cp == 0 {
            i += 1;
            continue;
        }
        if is_ws(cp) {
            while i < n {
                let (c2, a2) = font_render::utf8_next(text, i);
                if !is_ws(c2) || c2 == 0 {
                    break;
                }
                i += a2;
            }
            out.push(Atom::Space { w: advance(b' ' as u32, px), px, ibox });
            continue;
        }
        if cp < 0x80 && !is_word_break(cp) {
            let start = i;
            let mut w = 0.0f32;
            while i < n {
                let (c2, a2) = font_render::utf8_next(text, i);
                if c2 == 0 || is_ws(c2) || is_word_break(c2) || c2 >= 0x80 {
                    break;
                }
                w += advance(c2, px);
                i += a2;
            }
            if i > start {
                out.push(Atom::Text { node, start, end: i, w, px, ibox });
                continue;
            }
        }
        let w = advance(cp, px);
        out.push(Atom::Text { node, start: i, end: i + adv, w, px, ibox });
        i += adv;
    }
}

fn is_ws(cp: u32) -> bool {
    cp == b' ' as u32 || cp == b'\t' as u32 || cp == b'\n' as u32 || cp == b'\r' as u32 || cp == 0x0C
}

fn is_word_break(cp: u32) -> bool {
    matches!(
        cp,
        0x2E | 0x2C | 0x3B | 0x3A | 0x21 | 0x3F | 0x28 | 0x29 | 0x5B | 0x5D | 0x2F | 0x2D
    )
}

fn layout_inline_segment(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    parent_box: usize,
    nodes: &[usize],
    x0: f32,
    y0: f32,
    content_w: f32,
    depth: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> f32 {
    if depth > MAX_DEPTH {
        return 0.0;
    }
    let parent_style = lt.boxes[parent_box].style.clone();
    let anon = lt.new_box(BKind::Block, None, parent_style);
    lt.boxes[anon].anon = true;
    lt.boxes[parent_box].children.push(anon);

    let mut atoms: Vec<Atom> = Vec::new();
    for &n in nodes {
        collect_node_atoms(dom, styles, lt, n, Some(anon), &mut atoms, depth + 1, img_size);
    }
    if atoms.is_empty() {
        lt.boxes[anon].rect = Rect { x: x0, y: y0, w: content_w, h: 0.0 };
        return 0.0;
    }
    let align = lt.boxes[parent_box].style.text_align;

    let mut y = y0;
    let mut line = Line::new();
    // 行内盒在本行的 x 贡献
    let mut ib_min: Vec<(usize, f32)> = Vec::new();
    let mut ib_max: Vec<(usize, f32)> = Vec::new();

    for a in &atoms {
        match a {
            Atom::Br => {
                flush_line(dom, styles, lt, anon, &mut line, &mut ib_min, &mut ib_max, &mut y, align, x0, content_w);
            }
            Atom::Space { w, px, ibox } => {
                if !line.has {
                    continue;
                }
                if line.w + w > content_w {
                    flush_line(dom, styles, lt, anon, &mut line, &mut ib_min, &mut ib_max, &mut y, align, x0, content_w);
                    continue;
                }
                let x = line.w;
                line.w += w;
                if let Some(ib) = ibox {
                    add_ibox(&mut ib_min, &mut ib_max, *ib, x, x + w);
                }
                line.atoms.push((Atom::Space { w: *w, px: *px, ibox: *ibox }, x));
            }
            Atom::Text { w, .. } => {
                if line.has && line.w + w > content_w {
                    flush_line(dom, styles, lt, anon, &mut line, &mut ib_min, &mut ib_max, &mut y, align, x0, content_w);
                }
                let x = line.w;
                line.w += w;
                line.has = true;
                if let Atom::Text { ibox, .. } = a {
                    if let Some(ib) = ibox {
                        add_ibox(&mut ib_min, &mut ib_max, *ib, x, x + w);
                    }
                }
                line.atoms.push((a.clone(), x));
            }
            Atom::Replaced { w, ibox, .. } => {
                if line.has && line.w + w > content_w {
                    flush_line(dom, styles, lt, anon, &mut line, &mut ib_min, &mut ib_max, &mut y, align, x0, content_w);
                }
                let x = line.w;
                line.w += w;
                line.has = true;
                // 图片计入所在行内盒（<a>）的矩形：此前只由文本原子贡献，
                // <a><img></a> 的命中矩形保持 0×0 被 retain 丢弃（点图片无法跳转）。
                if let Some(ib) = ibox {
                    add_ibox(&mut ib_min, &mut ib_max, *ib, x, x + w);
                }
                line.atoms.push((a.clone(), x));
            }
        }
    }
    flush_line(dom, styles, lt, anon, &mut line, &mut ib_min, &mut ib_max, &mut y, align, x0, content_w);

    let total_h = y - y0;
    lt.boxes[anon].rect = Rect { x: x0, y: y0, w: content_w, h: total_h };
    total_h
}

fn flush_line(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    anon: usize,
    line: &mut Line,
    ib_min: &mut Vec<(usize, f32)>,
    ib_max: &mut Vec<(usize, f32)>,
    y: &mut f32,
    align: TextAlign,
    x0: f32,
    content_w: f32,
) {
    if line.atoms.is_empty() {
        return;
    }
    // 行度量
    let mut ascent = 0.0f32;
    let mut line_h = 0.0f32;
    for (a, _) in &line.atoms {
        let (px, lh) = match a {
            Atom::Text { px, ibox, .. } => (*px, style_of(lt, ibox, *px).line_height),
            Atom::Space { px, ibox, .. } => (*px, style_of(lt, ibox, *px).line_height),
            Atom::Replaced { h, .. } => (*h, *h + 4.0),
            Atom::Br => continue,
        };
        let (a_, _) = metrics(px);
        ascent = ascent.max(a_);
        line_h = line_h.max(lh);
    }
    let baseline = *y + ascent;

    // 对齐偏移
    let off = match align {
        TextAlign::Center => ((content_w - line.w) / 2.0).max(0.0),
        TextAlign::Right => (content_w - line.w).max(0.0),
        TextAlign::Left => 0.0,
    };

    for (a, x) in &line.atoms {
        match a {
            Atom::Text { node, start, end, px, ibox, .. } => {
                let st = styles.styles[*node].clone();
                lt.runs.push(Run {
                    node: *node,
                    start: *start,
                    end: *end,
                    x: x0 + off + x,
                    baseline,
                    px: *px,
                    color: st.color,
                    bold: st.bold,
                    underline: st.underline,
                    inline_box: *ibox,
                });
            }
            Atom::Space { .. } => {}
            Atom::Replaced { node, w, h, .. } => {
                let top = *y + (line_h - h) / 2.0;
                place_replaced(dom, styles, lt, anon, *node, x0 + off + x, top, *w, *h);
            }
            Atom::Br => {}
        }
    }
    // 行内盒背景/边框 + 并集 rect
    for &(ib, minx) in ib_min.iter() {
        if let Some(&(_, maxx)) = ib_max.iter().find(|&&(ib2, _)| ib2 == ib) {
            if maxx > minx {
                let r = Rect { x: x0 + off + minx, y: *y, w: maxx - minx, h: line_h };
                let st = lt.boxes[ib].style.clone();
                let cur = lt.boxes[ib].rect;
                if cur.w <= 0.0 && cur.h <= 0.0 {
                    lt.boxes[ib].rect = r;
                } else {
                    let min_x = cur.x.min(r.x);
                    let max_x = cur.right().max(r.right());
                    let min_y = cur.y.min(r.y);
                    let max_y = cur.bottom().max(r.bottom());
                    lt.boxes[ib].rect = Rect { x: min_x, y: min_y, w: max_x - min_x, h: max_y - min_y };
                }
                if st.background.is_some() || st.border() {
                    lt.bgs.push(InlineBg {
                        rect: r,
                        color: st.background.unwrap_or(0x00_00_00_00),
                        border_w: st.border_w,
                        border_color: st.border_color,
                    });
                }
            }
        }
    }
    *y += line_h;
    line.clear();
    ib_min.clear();
    ib_max.clear();
}

/// 取行内盒样式；无行内盒（匿名块直属文本）用文本字号构造默认行高。
fn style_of(lt: &Layout, ibox: &Option<usize>, px: f32) -> ComputedStyle {
    match ibox {
        Some(ib) => lt.boxes[*ib].style.clone(),
        None => ComputedStyle {
            font_size: px,
            line_height: px * 1.35,
            ..ComputedStyle::default()
        },
    }
}

fn add_ibox(min: &mut Vec<(usize, f32)>, max: &mut Vec<(usize, f32)>, ib: usize, x: f32, x2: f32) {
    if let Some(e) = min.iter_mut().find(|e| e.0 == ib) {
        e.1 = e.1.min(x);
    } else {
        min.push((ib, x));
    }
    if let Some(e) = max.iter_mut().find(|e| e.0 == ib) {
        e.1 = e.1.max(x2);
    } else {
        max.push((ib, x2));
    }
}

/// 替换元素尺寸（img/input/button/checkbox/radio），并注册图片引用/控件。
fn replaced_size(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    node: usize,
    img_size: &dyn Fn(&str) -> Option<(f32, f32)>,
) -> (f32, f32) {
    let st = styles.styles[node].clone();
    let tag = dom.nodes[node].tag();
    let (mut w, mut h): (f32, f32) = match tag {
        "img" => {
            let mut iw = match st.width {
                Size::Px(v) => Some(v),
                Size::Percent(per) => Some(per / 100.0 * 300.0),
                Size::Auto => None,
            };
            let mut ih = match st.height {
                Size::Px(v) => Some(v),
                Size::Percent(per) => Some(per / 100.0 * 200.0),
                Size::Auto => None,
            };
            if iw.is_none() || ih.is_none() {
                let (aw, ah) = img_attr_size(dom, node);
                if iw.is_none() {
                    iw = aw;
                }
                if ih.is_none() {
                    ih = ah;
                }
            }
            let src = dom.nodes[node].attr("src").unwrap_or("");
            let fetched = if src.is_empty() { None } else { img_size(src) };
            let (fw, fh) = match (iw, ih) {
                (Some(a), Some(b)) => (a, b),
                (Some(a), None) => (a, a * 0.75),
                (None, Some(b)) => (b * 4.0 / 3.0, b),
                (None, None) => fetched.unwrap_or((120.0, 90.0)),
            };
            if !src.is_empty() {
                lt.images.push(ImageRef { node, src: src.to_string(), w: fw, h: fh });
            }
            (fw, fh)
        }
        "input" => match dom.nodes[node].attr("type").unwrap_or("text") {
            "checkbox" | "radio" => (18.0, 18.0),
            _ => {
                let size = dom.nodes[node].attr("size").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20);
                ((size as f32 * 9.0 + 12.0).clamp(60.0, 420.0), 30.0)
            }
        },
        "button" => {
            // Node::text() 对元素节点恒返回 ""，按钮文案需聚合子树文本。
            let label = dom.inner_text(node);
            let w = text_width(label.as_bytes(), st.font_size) + st.padding[1] + st.padding[3] + 2.0;
            (w.max(40.0), st.font_size + st.padding[0] + st.padding[2] + 2.0)
        }
        _ => (80.0, 30.0),
    };

    if tag == "input" || tag == "button" {
        let itype = dom.nodes[node]
            .attr("type")
            .unwrap_or(if tag == "button" { "submit" } else { "text" });
        let name = dom.nodes[node].attr("name").unwrap_or("").to_string();
        let form = find_form(dom, node);
        let ctrl_idx = lt.controls.len();
        let kind = match (tag, itype) {
            ("input", "checkbox") => ControlKind::Checkbox {
                name,
                checked: dom.nodes[node].has_attr("checked"),
            },
            ("input", "radio") => ControlKind::Radio {
                name,
                checked: dom.nodes[node].has_attr("checked"),
            },
            ("input", "submit") | ("input", "button") | ("button", _) => {
                let label = if tag == "button" {
                    dom.inner_text(node)
                } else {
                    dom.nodes[node].attr("value").unwrap_or("提交").to_string()
                };
                let bw = text_width(label.as_bytes(), st.font_size) + st.padding[1] + st.padding[3] + 2.0;
                w = bw.max(40.0);
                h = st.font_size + st.padding[0] + st.padding[2] + 2.0;
                ControlKind::Button { label, name, submit: itype != "button" }
            }
            _ => ControlKind::TextInput {
                name,
                value_idx: ctrl_idx,
                size: dom.nodes[node].attr("size").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20),
            },
        };
        lt.controls.push(Control { rect: Rect { x: 0.0, y: 0.0, w, h }, kind, node, form });
    }
    (w, h)
}

fn place_replaced(
    dom: &Dom,
    styles: &StyleTree,
    lt: &mut Layout,
    anon: usize,
    node: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    let st = styles.styles[node].clone();
    let tag = dom.nodes[node].tag();
    let kind = if tag == "img" {
        ReplacedKind::Img
    } else if tag == "input" {
        match dom.nodes[node].attr("type").unwrap_or("text") {
            "checkbox" => ReplacedKind::Checkbox,
            "radio" => ReplacedKind::Radio,
            "submit" | "button" => ReplacedKind::SubmitButton,
            _ => ReplacedKind::TextInput,
        }
    } else {
        ReplacedKind::Button
    };
    let bx = lt.new_box(BKind::Replaced, Some(node), st);
    lt.boxes[bx].replaced = Some(kind);
    lt.boxes[bx].rect = Rect { x, y, w, h };
    lt.boxes[bx].ctrl = lt
        .controls
        .iter()
        .enumerate()
        .rev()
        .find(|(_, c)| c.node == node)
        .map(|(i, _)| i);
    if let Some(ci) = lt.boxes[bx].ctrl {
        lt.controls[ci].rect = Rect { x, y, w, h };
    }
    if kind == ReplacedKind::Img {
        let src = dom.nodes[node].attr("src").unwrap_or("");
        lt.boxes[bx].img = lt.images.iter().position(|im| im.src == src);
    }
    lt.boxes[anon].children.push(bx);
}

fn img_attr_size(dom: &Dom, node: usize) -> (Option<f32>, Option<f32>) {
    let w = dom.nodes[node].attr("width").and_then(|s| s.parse::<f32>().ok());
    let h = dom.nodes[node].attr("height").and_then(|s| s.parse::<f32>().ok());
    (w, h)
}

fn find_form(dom: &Dom, node: usize) -> Option<usize> {
    let mut p = dom.nodes[node].parent;
    while let Some(pi) = p {
        if dom.nodes[pi].tag() == "form" {
            return Some(pi);
        }
        p = dom.nodes[pi].parent;
    }
    None
}

/// 应用相对定位位移（沿子树下移/右移）。
fn apply_relative(lt: &mut Layout, root: usize) {
    fn walk(lt: &mut Layout, i: usize, dx: f32, dy: f32, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let bx = &lt.boxes[i];
        let (mut ndx, mut ndy) = (dx, dy);
        if bx.style.position == Position::Relative {
            ndx += bx.style.rel_left;
            ndy += bx.style.rel_top;
        }
        if ndx != 0.0 || ndy != 0.0 {
            lt.boxes[i].rect.x += ndx;
            lt.boxes[i].rect.y += ndy;
        }
        let children = lt.boxes[i].children.clone();
        for c in children {
            walk(lt, c, ndx, ndy, depth + 1);
        }
    }
    walk(lt, root, 0.0, 0.0, 0);
}

// ==================== 命中测试 ====================

pub enum HitResult {
    None,
    Link(String),
    /// 控件下标
    Control(usize),
}

pub fn hit_test(lt: &Layout, x: f32, y: f32) -> HitResult {
    for (i, c) in lt.controls.iter().enumerate().rev() {
        if c.rect.contains(x, y) {
            return HitResult::Control(i);
        }
    }
    for l in lt.links.iter().rev() {
        if l.rect.contains(x, y) {
            return HitResult::Link(l.url.clone());
        }
    }
    HitResult::None
}
