//! 样式计算：UA 默认样式 + 作者样式表 + 内联样式，按特异性级联，属性继承。
//!
//! 继承属性：color / font-size / font-weight / line-height / text-align /
//! underline（text-decoration，简化按继承处理）。
//! 非继承属性：display / margin / padding / border / width / height /
//! background / position / left / top。

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::css::*;
use crate::html::{Dom, NodeKind};

pub const C_LINK: u32 = 0x00_4A_9F_E8;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    Auto,
    Px(f32),
    Percent(f32),
}

#[derive(Clone, Debug)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: u32,
    pub background: Option<u32>,
    pub font_size: f32,
    pub bold: bool,
    pub text_align: TextAlign,
    pub line_height: f32,
    pub margin: [f32; 4], // top right bottom left
    pub padding: [f32; 4],
    pub border_w: [f32; 4],
    pub border_color: [u32; 4],
    pub width: Size,
    pub height: Size,
    pub position: Position,
    pub rel_left: f32,
    pub rel_top: f32,
    pub underline: bool,
    pub is_list_item: bool,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle {
            display: Display::Block,
            color: 0x00_00_00_00,
            background: None,
            font_size: 16.0,
            bold: false,
            text_align: TextAlign::Left,
            line_height: 19.0,
            margin: [0.0; 4],
            padding: [0.0; 4],
            border_w: [0.0; 4],
            border_color: [0; 4],
            width: Size::Auto,
            height: Size::Auto,
            position: Position::Static,
            rel_left: 0.0,
            rel_top: 0.0,
            underline: false,
            is_list_item: false,
        }
    }
}

impl ComputedStyle {
    pub fn border(&self) -> bool {
        self.border_w[0] > 0.0 || self.border_w[1] > 0.0 || self.border_w[2] > 0.0 || self.border_w[3] > 0.0
    }
}

/// 全部节点计算样式（与 DOM 下标对齐）。
pub struct StyleTree {
    pub styles: Vec<ComputedStyle>,
}

/// 收集页面内 `<style>` 内容，解析为样式表。
pub fn collect_stylesheet(dom: &Dom) -> Stylesheet {
    let mut ss = Stylesheet::new();
    let mut text = String::new();
    let style_nodes = dom.elements_by_tag("style");
    for &s in &style_nodes {
        text.clear();
        collect_text(dom, s, &mut text);
        let part = Stylesheet::parse(&text);
        ss.rules.extend(part.rules);
    }
    ss
}

fn collect_text(dom: &Dom, i: usize, out: &mut String) {
    collect_text_d(dom, i, out, 0);
}
fn collect_text_d(dom: &Dom, i: usize, out: &mut String, d: usize) {
    // 深度守卫（树深已在 html::parse 封顶，此处纵深防御）。
    if d > crate::html::MAX_TREE_DEPTH + 1 {
        return;
    }
    match &dom.nodes[i].kind {
        NodeKind::Text { text } => out.push_str(text),
        _ => {
            for &c in &dom.nodes[i].children {
                collect_text_d(dom, c, out, d + 1);
            }
        }
    }
}

/// 计算全部节点样式。`page_bg`/`page_fg` 为 html 根默认背景/前景。
pub fn compute(dom: &Dom, author: &Stylesheet) -> StyleTree {
    let n = dom.nodes.len();
    let mut styles: Vec<ComputedStyle> = Vec::with_capacity(n);
    for _ in 0..n {
        styles.push(ComputedStyle::default());
    }
    // html 根：白底黑字
    styles[dom.root].display = Display::Block;
    styles[dom.root].background = Some(0x00_FF_FF_FF);
    styles[dom.root].color = 0x00_00_00_00;

    // 先序遍历计算（父样式已知）
    let mut order: Vec<usize> = Vec::with_capacity(n);
    preorder(dom, dom.root, &mut order);
    for &i in &order {
        if i == dom.root {
            continue;
        }
        let parent = dom.nodes[i].parent.unwrap_or(dom.root);
        let pstyle = styles[parent].clone();
        let mut st = ua_style(dom, i, &pstyle);
        // 作者样式表（按特异性升序，后者覆盖）
        let mut matched: Vec<(u32, u32, u32, usize)> = Vec::new();
        for (ri, rule) in author.rules.iter().enumerate() {
            for sel in &rule.selectors {
                if selector_matches(dom, i, sel) {
                    let sp = sel.specificity();
                    matched.push((sp.0, sp.1, sp.2, ri));
                    break;
                }
            }
        }
        matched.sort();
        for (_, _, _, ri) in matched {
            for (prop, value) in &author.rules[ri].decls {
                apply_decl(&mut st, prop, value, &pstyle);
            }
        }
        // 内联样式（最高优先级，忽略 !important 简化）
        if let NodeKind::Element { attrs, .. } = &dom.nodes[i].kind {
            for (k, v) in attrs {
                if k == "style" {
                    for (prop, value) in parse_inline_style(v) {
                        apply_decl(&mut st, &prop, &value, &pstyle);
                    }
                }
            }
        }
        // 元素属性派生样式（HTML 表现属性）
        apply_presentational(dom, i, &mut st);
        styles[i] = st;
    }
    StyleTree { styles }
}

fn preorder(dom: &Dom, i: usize, out: &mut Vec<usize>) {
    preorder_d(dom, i, out, 0);
}
fn preorder_d(dom: &Dom, i: usize, out: &mut Vec<usize>, d: usize) {
    if d > crate::html::MAX_TREE_DEPTH + 1 {
        return;
    }
    out.push(i);
    for &c in &dom.nodes[i].children {
        preorder_d(dom, c, out, d + 1);
    }
}

/// 选择器匹配（标签/类/id/后代）。
pub fn selector_matches(dom: &Dom, node: usize, sel: &Selector) -> bool {
    match sel {
        Selector::Simple(s) => simple_matches(dom, node, s),
        Selector::Descendant { ancestors, simple } => {
            if !simple_matches(dom, node, simple) {
                return false;
            }
            // 从 node 的父开始向上匹配 ancestors（从最近到最远）
            let mut cur = dom.nodes[node].parent;
            for anc in ancestors.iter().rev() {
                let mut found = false;
                while let Some(p) = cur {
                    if simple_matches(dom, p, anc) {
                        found = true;
                        cur = dom.nodes[p].parent;
                        break;
                    }
                    cur = dom.nodes[p].parent;
                }
                if !found {
                    return false;
                }
            }
            true
        }
    }
}

fn simple_matches(dom: &Dom, node: usize, s: &SimpleSel) -> bool {
    let nd = &dom.nodes[node];
    let NodeKind::Element { tag, attrs } = &nd.kind else {
        return false;
    };
    if let Some(t) = &s.tag {
        if t != tag {
            return false;
        }
    }
    if let Some(id) = &s.id {
        match nd.attr("id") {
            Some(v) if v == id => {}
            _ => return false,
        }
    }
    if !s.classes.is_empty() {
        let cls = match nd.attr("class") {
            Some(c) => c,
            None => return false,
        };
        for c in &s.classes {
            if !cls.split_whitespace().any(|x| x == c) {
                return false;
            }
        }
    }
    // 通用选择器 `*`
    if s.tag.is_none() && s.id.is_none() && s.classes.is_empty() {
        return true;
    }
    let _ = attrs;
    true
}

/// UA 默认样式（以继承父样式为基础）。
fn ua_style(dom: &Dom, node: usize, p: &ComputedStyle) -> ComputedStyle {
    let mut s = ComputedStyle {
        color: p.color,
        font_size: p.font_size,
        bold: p.bold,
        text_align: p.text_align,
        line_height: p.line_height,
        underline: p.underline,
        ..ComputedStyle::default()
    };
    let tag = dom.nodes[node].tag();
    match tag {
        "body" => {
            s.margin = [8.0, 8.0, 8.0, 8.0];
        }
        "h1" => { s.font_size = 28.0; s.bold = true; s.margin = [0.67 * 28.0, 0.0, 0.67 * 28.0, 0.0]; }
        "h2" => { s.font_size = 24.0; s.bold = true; s.margin = [0.83 * 24.0, 0.0, 0.83 * 24.0, 0.0]; }
        "h3" => { s.font_size = 21.0; s.bold = true; s.margin = [1.0 * 21.0, 0.0, 1.0 * 21.0, 0.0]; }
        "h4" => { s.font_size = 18.0; s.bold = true; s.margin = [1.33 * 18.0, 0.0, 1.33 * 18.0, 0.0]; }
        "h5" => { s.font_size = 16.0; s.bold = true; s.margin = [1.67 * 16.0, 0.0, 1.67 * 16.0, 0.0]; }
        "h6" => { s.font_size = 14.0; s.bold = true; s.margin = [2.33 * 14.0, 0.0, 2.33 * 14.0, 0.0]; }
        "p" => {
            s.margin = [p.font_size, 0.0, p.font_size, 0.0];
        }
        "a" => {
            s.display = Display::Inline;
            s.color = C_LINK;
            s.underline = true;
        }
        "ul" | "ol" => {
            s.margin = [p.font_size, 0.0, p.font_size, 0.0];
            s.padding = [0.0, 0.0, 0.0, 24.0];
        }
        "li" => {
            s.is_list_item = true;
            s.display = Display::Block;
        }
        "table" => {
            s.display = Display::Table;
            s.margin = [p.font_size, 0.0, p.font_size, 0.0];
        }
        "tr" => {
            s.display = Display::TableRow;
        }
        "td" | "th" => {
            s.display = Display::TableCell;
            s.padding = [4.0, 4.0, 4.0, 4.0];
            s.border_w = [1.0, 1.0, 1.0, 1.0];
            s.border_color = [0x00_D0_D4_DE; 4];
            if tag == "th" {
                s.bold = true;
                s.text_align = TextAlign::Center;
            }
        }
        "img" => {
            s.display = Display::InlineBlock;
        }
        "input" => {
            s.display = Display::InlineBlock;
            s.background = Some(0x00_FF_FF_FF);
            s.border_w = [1.0, 1.0, 1.0, 1.0];
            s.border_color = [0x00_99_99_99; 4];
        }
        "button" => {
            s.display = Display::InlineBlock;
            s.background = Some(0x00_E4_E7_EC);
            s.border_w = [1.0, 1.0, 1.0, 1.0];
            s.border_color = [0x00_99_99_99; 4];
            s.padding = [5.0, 12.0, 5.0, 12.0];
        }
        "hr" => {
            s.display = Display::Block;
            s.margin = [8.0, 0.0, 8.0, 0.0];
            s.border_w = [1.0, 0.0, 0.0, 0.0];
            s.border_color = [0x00_CC_CC_CC, 0, 0, 0];
        }
        "form" => { s.display = Display::Block; }
        "label" => {}
        "pre" => {}
        "head" | "title" | "style" | "script" | "meta" | "link" | "base" => {
            s.display = Display::None;
        }
        _ => {
            // div/span/section 等：默认 block/inline 由作者 CSS 决定；
            // span/b/i/strong/em/u 默认行内
            match tag {
                "span" | "b" | "strong" | "em" | "i" | "u" | "sub" | "sup" | "small" => {
                    s.display = Display::Inline;
                }
                _ => {}
            }
            if tag == "b" || tag == "strong" {
                s.bold = true;
            }
            if tag == "u" {
                s.underline = true;
            }
        }
    }
    // line-height 未显式设置时按字号默认
    s.line_height = s.font_size * 1.35;
    s
}

/// 应用单条声明。
fn apply_decl(st: &mut ComputedStyle, prop: &str, value: &str, p: &ComputedStyle) {
    let v = value.trim();
    match prop {
        "display" => {
            if let Some(d) = parse_display(v) {
                st.display = d;
            }
        }
        "color" => {
            if let Some(c) = parse_color(v) {
                st.color = c.0;
            }
        }
        "background-color" | "background" => {
            if v == "transparent" || v == "none" {
                st.background = None;
            } else if let Some(c) = parse_color(v) {
                st.background = Some(c.0);
            }
        }
        "font-size" => {
            st.font_size = parse_font_size(v, p.font_size);
            st.line_height = st.font_size * 1.35;
        }
        "font-weight" => {
            st.bold = match v {
                "normal" => false,
                "bold" => true,
                _ => v.parse::<u32>().map(|w| w >= 600).unwrap_or(false),
            };
        }
        "line-height" => {
            st.line_height = parse_line_height(v, p.font_size, st.font_size);
        }
        "text-align" => {
            st.text_align = match v {
                "center" => TextAlign::Center,
                "right" => TextAlign::Right,
                _ => TextAlign::Left,
            };
        }
        "text-decoration" => {
            st.underline = v.contains("underline");
        }
        "margin" => {
            st.margin = parse_box_edges(v);
        }
        "margin-top" => st.margin[0] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "margin-right" => st.margin[1] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "margin-bottom" => st.margin[2] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "margin-left" => st.margin[3] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "padding" => {
            st.padding = parse_box_edges(v);
        }
        "padding-top" => st.padding[0] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "padding-right" => st.padding[1] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "padding-bottom" => st.padding[2] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "padding-left" => st.padding[3] = parse_length(v, p.font_size, 0.0).unwrap_or(0.0),
        "border" => {
            // border: <width> <style> <color>
            let mut w = 0.0;
            let mut col = 0x00_00_00_00;
            for tok in v.split_whitespace() {
                if let Some(c) = parse_color(tok) {
                    col = c.0;
                } else {
                    let bw = parse_border_width(tok);
                    if bw > 0.0 && w == 0.0 {
                        w = bw;
                    }
                }
            }
            for i in 0..4 {
                if w > 0.0 {
                    st.border_w[i] = w;
                    st.border_color[i] = col;
                }
            }
        }
        "border-width" => {
            let e = parse_box_edges(v);
            for i in 0..4 {
                st.border_w[i] = e[i];
            }
        }
        "border-color" => {
            let parts: Vec<&str> = v.split_whitespace().collect();
            for (i, part) in parts.iter().enumerate().take(4) {
                if let Some(c) = parse_color(part) {
                    st.border_color[i] = c.0;
                }
            }
        }
        "border-style" => {
            if v == "none" {
                for i in 0..4 {
                    st.border_w[i] = 0.0;
                }
            }
        }
        "border-top" => set_border_side(st, 0, v),
        "border-right" => set_border_side(st, 1, v),
        "border-bottom" => set_border_side(st, 2, v),
        "border-left" => set_border_side(st, 3, v),
        "border-top-width" => st.border_w[0] = parse_border_width(v),
        "border-right-width" => st.border_w[1] = parse_border_width(v),
        "border-bottom-width" => st.border_w[2] = parse_border_width(v),
        "border-left-width" => st.border_w[3] = parse_border_width(v),
        "width" => {
            st.width = parse_size(v, p.font_size);
        }
        "height" => {
            st.height = parse_size(v, p.font_size);
        }
        "position" => {
            st.position = parse_position(v);
        }
        "left" => {
            st.rel_left = parse_length(v, p.font_size, 0.0).unwrap_or(0.0);
        }
        "top" => {
            st.rel_top = parse_length(v, p.font_size, 0.0).unwrap_or(0.0);
        }
        _ => {} // 其他属性（flex 等）忽略
    }
}

fn set_border_side(st: &mut ComputedStyle, side: usize, v: &str) {
    let mut w = 0.0;
    let mut col = 0x00_00_00_00;
    for tok in v.split_whitespace() {
        if let Some(c) = parse_color(tok) {
            col = c.0;
        } else {
            let bw = parse_border_width(tok);
            if bw > 0.0 && w == 0.0 {
                w = bw;
            }
        }
    }
    if w > 0.0 {
        st.border_w[side] = w;
        st.border_color[side] = col;
    }
}

fn parse_size(v: &str, parent_font: f32) -> Size {
    let v = v.trim();
    if v == "auto" {
        return Size::Auto;
    }
    if let Some(x) = v.strip_suffix('%') {
        if let Ok(p) = x.trim().parse::<f32>() {
            return Size::Percent(p);
        }
        return Size::Auto;
    }
    match parse_length(v, parent_font, 0.0) {
        Some(px) => Size::Px(px),
        None => Size::Auto,
    }
}

/// HTML 表现属性（width/height/bgcolor/align/text-align 等）派生样式。
fn apply_presentational(dom: &Dom, node: usize, st: &mut ComputedStyle) {
    let nd = &dom.nodes[node];
    if let Some(w) = nd.attr("width") {
        if st.width == Size::Auto {
            st.width = parse_size(w, st.font_size);
        }
    }
    if let Some(h) = nd.attr("height") {
        if st.height == Size::Auto {
            st.height = parse_size(h, st.font_size);
        }
    }
    if let Some(bg) = nd.attr("bgcolor") {
        if let Some(c) = parse_color(bg) {
            st.background = Some(c.0);
        }
    }
    if let Some(al) = nd.attr("align") {
        st.text_align = match al {
            "center" => TextAlign::Center,
            "right" => TextAlign::Right,
            _ => TextAlign::Left,
        };
    }
}
