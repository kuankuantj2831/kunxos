//! HTML 解析器：tokenizer + 简化树构建（no_std + alloc）。
//!
//! 支持标签：html/head/body/title/div/p/a/img/span/h1-h6/table/tr/td/th/
//! form/input/button/ul/li/b/strong/em/i/u/br/hr/label/select/option 等。
//! - 自闭合/void 元素：br img hr input meta link col
//! - 属性：单/双引号与无引号
//! - 实体：&amp; &lt; &gt; &quot; &#39; &nbsp; 与 &#NN; / &#xHH;
//! - 注释与 doctype 忽略
//! - 隐式闭合：p（遇块级）、li（遇 li）、td/th（遇 td/th/tr）、tr（遇 tr）
//! - 未闭合标签自动补全（解析器结束 / 父元素不匹配时弹出）
//! - `<script>` 内容按原始文本保留但不执行（JS 预留）

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// 空元素（无闭合标签）。
const VOID: [&str; 12] = [
    "br", "img", "hr", "input", "meta", "link", "col", "area", "base", "embed", "source", "wbr",
];

/// 块级元素（触发 p 的隐式闭合）。
const BLOCKISH: [&str; 24] = [
    "div", "p", "h1", "h2", "h3", "h4", "h5", "h6", "table", "ul", "ol", "li", "form", "hr",
    "blockquote", "pre", "section", "article", "header", "footer", "aside", "nav", "main",
    "address",
];

#[derive(Clone, Debug)]
pub enum NodeKind {
    Element { tag: String, attrs: Vec<(String, String)> },
    Text { text: String },
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: NodeKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

impl Node {
    pub fn is_element(&self) -> bool {
        matches!(self.kind, NodeKind::Element { .. })
    }
    pub fn tag(&self) -> &str {
        match &self.kind {
            NodeKind::Element { tag, .. } => tag,
            NodeKind::Text { .. } => "",
        }
    }
    pub fn text(&self) -> &str {
        match &self.kind {
            NodeKind::Text { text } => text,
            NodeKind::Element { .. } => "",
        }
    }
    /// 取属性值（属性名小写；首个同名属性生效，与 HTML 规范一致）。
    pub fn attr(&self, name: &str) -> Option<&str> {
        if let NodeKind::Element { attrs, .. } = &self.kind {
            for (k, v) in attrs {
                if k == name {
                    return Some(v);
                }
            }
        }
        None
    }
    pub fn has_attr(&self, name: &str) -> bool {
        self.attr(name).is_some()
    }
}

#[derive(Clone, Debug)]
pub struct Dom {
    pub nodes: Vec<Node>,
    /// html 元素下标；解析失败也可能为 0 号（包装根）。
    pub root: usize,
}

impl Dom {
    pub fn node(&self, i: usize) -> &Node {
        &self.nodes[i]
    }
    pub fn node_mut(&mut self, i: usize) -> &mut Node {
        &mut self.nodes[i]
    }
    /// 按标签名收集后代元素下标（含自身）。
    pub fn elements_by_tag(&self, tag: &str) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_tag_d(self.root, tag, &mut out, 0);
        out
    }
    fn collect_tag_d(&self, i: usize, tag: &str, out: &mut Vec<usize>, d: usize) {
        // 深度守卫（纵深防御：树深已在 parse 封顶）。
        if d > MAX_TREE_DEPTH + 1 {
            return;
        }
        if self.nodes[i].tag() == tag {
            out.push(i);
        }
        for &c in &self.nodes[i].children {
            self.collect_tag_d(c, tag, out, d + 1);
        }
    }
    /// 文档标题（title 元素文本，无则空）。
    pub fn title(&self) -> String {
        if let Some(t) = self.elements_by_tag("title").first() {
            let mut s = String::new();
            self.text_of(*t, &mut s);
            return s;
        }
        String::new()
    }
    /// 元素子树聚合文本（`<button>` 标签文案等）。Node::text() 对元素
    /// 节点恒返回 ""，布局/绘制须用本函数取按钮内文本。
    pub fn inner_text(&self, i: usize) -> String {
        let mut s = String::new();
        self.text_of_d(i, &mut s, 0);
        s
    }
    fn text_of(&self, i: usize, out: &mut String) {
        self.text_of_d(i, out, 0);
    }
    fn text_of_d(&self, i: usize, out: &mut String, d: usize) {
        if d > MAX_TREE_DEPTH + 1 {
            return;
        }
        match &self.nodes[i].kind {
            NodeKind::Text { text } => out.push_str(text),
            NodeKind::Element { .. } => {
                for &c in &self.nodes[i].children {
                    self.text_of_d(c, out, d + 1);
                }
            }
        }
    }
}

// ==================== 分词器 ====================

#[derive(Debug)]
enum Tok {
    Start { name: String, attrs: Vec<(String, String)> },
    End { name: String },
    Text { data: String },
}

/// 极小 HTML 实体表。
fn decode_entity(s: &[u8], i: usize) -> Option<(u32, usize)> {
    // 数字实体（s[i] == '&'，数字实体为 &#...;）
    if i + 1 < s.len() && s[i + 1] == b'#' {
        let mut j = i + 2;
        let mut num = 0u32;
        let mut any = false;
        let mut hex = false;
        if j < s.len() && (s[j] == b'x' || s[j] == b'X') {
            hex = true;
            j += 1;
        }
        while j < s.len() && j - i <= 9 {
            let d = if hex {
                match s[j] {
                    b'0'..=b'9' => (s[j] - b'0') as u32,
                    b'a'..=b'f' => (s[j] - b'a' + 10) as u32,
                    b'A'..=b'F' => (s[j] - b'A' + 10) as u32,
                    _ => break,
                }
            } else if s[j].is_ascii_digit() {
                (s[j] - b'0') as u32
            } else {
                break;
            };
            num = num.wrapping_mul(if hex { 16 } else { 10 }).wrapping_add(d);
            any = true;
            j += 1;
        }
        if any && j < s.len() && s[j] == b';' {
            return Some((num, j + 1 - i));
        }
        return None;
    }
    const NAMED: [(&[u8], u32); 9] = [
        (b"amp;", b'&' as u32),
        (b"lt;", b'<' as u32),
        (b"gt;", b'>' as u32),
        (b"quot;", b'"' as u32),
        (b"apos;", b'\'' as u32),
        (b"nbsp;", 0xA0),
        (b"copy;", 0xA9),
        (b"reg;", 0xAE),
        (b"hellip;", 0x2026),
    ];
    for (name, cp) in NAMED {
        let n = name.len();
        // 命名实体从 '&' 之后开始（s[i] == '&'）；返回含 '&' 的总长
        if s.len() >= i + 1 + n && &s[i + 1..i + 1 + n] == name {
            return Some((cp, n + 1));
        }
    }
    None
}

/// 对文本做实体解码。
fn decode_text(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'&' {
            if let Some((cp, n)) = decode_entity(s, i) {
                // 按 UTF-8 追加码点
                let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += n;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 把 HTML 字节流切成 token 列表（文本 token 已做实体解码）。
/// `<script>…</script>` 内容按原始文本保留为一个 Text token（JS 预留不执行）。
fn tokenize(input: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut i = 0usize;
    let n = input.len();
    while i < n {
        if input[i] == b'<' {
            // script 原始内容
            if input[i..].len() >= 7 && input[i..i + 7].eq_ignore_ascii_case(b"<script") {
                let name_end = i + 7;
                let after = name_end;
                // 找到 '>'
                let mut gt = after;
                while gt < n && input[gt] != b'>' {
                    gt += 1;
                }
                let mut attrs: Vec<(String, String)> = Vec::new();
                if gt < n {
                    let (a, _) = parse_attrs(&input[after..gt]);
                    attrs = a;
                }
                toks.push(Tok::Start { name: "script".into(), attrs });
                // 找 </script （大小写不敏感）
                let body_start = if gt < n { gt + 1 } else { n };
                let close = find_sub_ci(&input[body_start..], b"</script");
                let body_end = match close {
                    Some(c) => body_start + c,
                    None => n,
                };
                let raw = String::from_utf8_lossy(&input[body_start..body_end]).into_owned();
                toks.push(Tok::Text { data: raw });
                // 跳过闭合标签
                let mut j = body_end;
                while j < n && input[j] != b'>' {
                    j += 1;
                }
                i = if j < n { j + 1 } else { n };
                toks.push(Tok::End { name: "script".into() });
                continue;
            }
            // 注释
            if input[i..].starts_with(b"<!--") {
                if let Some(end) = find_sub(&input[i + 4..], b"-->") {
                    i += 4 + end + 3;
                    continue;
                }
                break;
            }
            // 结束标签
            if i + 1 < n && input[i + 1] == b'/' {
                let mut j = i + 2;
                while j < n && is_name_char(input[j]) {
                    j += 1;
                }
                if j > i + 2 {
                    let name = core::str::from_utf8(&input[i + 2..j]).unwrap_or("").to_ascii_lowercase();
                    toks.push(Tok::End { name });
                }
                // 跳到 '>'
                while j < n && input[j] != b'>' {
                    j += 1;
                }
                i = (j + 1).min(n);
                continue;
            }
            // 开始标签
            let mut j = i + 1;
            while j < n && is_name_char(input[j]) {
                j += 1;
            }
            if j > i + 1 {
                let name = core::str::from_utf8(&input[i + 1..j]).unwrap_or("").to_ascii_lowercase();
                let (attrs, end) = parse_attrs(&input[j..]);
                toks.push(Tok::Start { name, attrs });
                i = j + end;
                continue;
            }
            // 纯 '<'：按文本
            toks.push(Tok::Text { data: "<".to_string() });
            i += 1;
            continue;
        }
        // 文本
        let start = i;
        while i < n && input[i] != b'<' {
            i += 1;
        }
        toks.push(Tok::Text { data: decode_text(&input[start..i]) });
    }
    toks
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b':'
}

/// 解析标签属性，返回 (attrs, 消耗的字节数，含 '>')。
fn parse_attrs(s: &[u8]) -> (Vec<(String, String)>, usize) {
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    let n = s.len();
    loop {
        // 跳过空白
        while i < n && (s[i] == b' ' || s[i] == b'\t' || s[i] == b'\n' || s[i] == b'\r' || s[i] == 0x0C)
        {
            i += 1;
        }
        if i >= n {
            break; // 未闭合标签
        }
        if s[i] == b'>' {
            i += 1;
            break;
        }
        if s[i] == b'/' && i + 1 < n && s[i + 1] == b'>' {
            i += 2;
            break;
        }
        // 属性名
        let aname_start = i;
        while i < n && is_name_char(s[i]) {
            i += 1;
        }
        if i == aname_start {
            // 非法字符，跳过
            i += 1;
            continue;
        }
        let name = core::str::from_utf8(&s[aname_start..i]).unwrap_or("").to_ascii_lowercase();
        // 跳过空白到 '='
        let mut j = i;
        while j < n && (s[j] == b' ' || s[j] == b'\t' || s[j] == b'\n' || s[j] == b'\r') {
            j += 1;
        }
        let mut value = String::new();
        if j < n && s[j] == b'=' {
            j += 1;
            while j < n && (s[j] == b' ' || s[j] == b'\t' || s[j] == b'\n' || s[j] == b'\r') {
                j += 1;
            }
            if j < n && (s[j] == b'"' || s[j] == b'\'') {
                let q = s[j];
                j += 1;
                let vs = j;
                while j < n && s[j] != q {
                    j += 1;
                }
                value = decode_text(&s[vs..j]);
                if j < n {
                    j += 1;
                }
            } else {
                let vs = j;
                while j < n && s[j] != b'>' && s[j] != b' ' && s[j] != b'\t' && s[j] != b'\n'
                    && s[j] != b'\r'
                {
                    j += 1;
                }
                value = decode_text(&s[vs..j]);
            }
            i = j;
        }
        attrs.push((name, value));
    }
    (attrs, i)
}

/// 大小写不敏感的子串查找。
fn find_sub_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if hay[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(i);
        }
    }
    None
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

// ==================== 树构建 ====================

/// DOM 树深上限：远超正常页面（<32 层），同时保证递归遍历（每帧 ~64B）
/// 在 1MB 用户栈内安全（256×64B ≈ 16KB）。
pub const MAX_TREE_DEPTH: usize = 256;

/// 解析 HTML 文档为 DOM。
pub fn parse(input: &[u8]) -> Dom {
    let toks = tokenize(input);
    let mut dom = Dom { nodes: Vec::new(), root: 0 };
    // 包装根（保证所有内容有父节点）
    let root_idx = dom.nodes.len();
    dom.nodes.push(Node {
        kind: NodeKind::Element { tag: "#root".into(), attrs: Vec::new() },
        parent: None,
        children: Vec::new(),
    });
    let mut stack: Vec<usize> = vec![root_idx];

    for t in toks {
        match t {
            Tok::Text { data } => {
                if data.is_empty() {
                    continue;
                }
                // 不在 pre/textarea 里时忽略纯空白文本（防布局产生空行）
                if data.trim().is_empty() && !stack.is_empty() {
                    let top = *stack.last().unwrap();
                    if dom.nodes[top].tag() != "pre" && dom.nodes[top].tag() != "textarea" {
                        continue;
                    }
                }
                let top = *stack.last().unwrap();
                let idx = dom.nodes.len();
                dom.nodes.push(Node {
                    kind: NodeKind::Text { text: data },
                    parent: Some(top),
                    children: Vec::new(),
                });
                dom.nodes[top].children.push(idx);
            }
            Tok::Start { name, attrs } => {
                // 隐式闭合
                maybe_implicit_close(&mut dom, &mut stack, &name);
                let top = *stack.last().unwrap();
                let idx = dom.nodes.len();
                dom.nodes.push(Node {
                    kind: NodeKind::Element { tag: name.clone(), attrs },
                    parent: Some(top),
                    children: Vec::new(),
                });
                dom.nodes[top].children.push(idx);
                // 树深封顶：超限元素按父级兄弟平铺（内容仍渲染，不嵌套）。
                // 此前无上限 —— ~2 万层嵌套页（64KB 即可构造）会让后续
                // 递归遍历（elements_by_tag/text_of/布局）打穿 1MB 用户栈。
                if !VOID.contains(&name.as_str()) && stack.len() < MAX_TREE_DEPTH {
                    stack.push(idx);
                }
            }
            Tok::End { name } => {
                // 从栈顶向下找匹配
                let mut found = None;
                for (k, &i) in stack.iter().enumerate().rev() {
                    if dom.nodes[i].tag() == name {
                        found = Some(k);
                        break;
                    }
                }
                if let Some(k) = found {
                    stack.truncate(k);
                }
            }
        }
    }
    // 解析完成：补全根标签（html/head/body 未写时仍能渲染 body 内容）
    dom.root = root_idx;
    // 找 html 元素
    for (i, nd) in dom.nodes.iter().enumerate() {
        if nd.tag() == "html" {
            dom.root = i;
            break;
        }
    }
    dom
}

/// 简化隐式闭合规则。
fn maybe_implicit_close(dom: &mut Dom, stack: &mut Vec<usize>, name: &str) {
    // p 遇到块级元素闭合
    if BLOCKISH.contains(&name) {
        if let Some(&top) = stack.last() {
            if dom.nodes[top].tag() == "p" {
                stack.pop();
            }
        }
    }
    // li 遇 li/ul/ol
    if name == "li" || name == "ul" || name == "ol" {
        while let Some(&top) = stack.last() {
            let t = dom.nodes[top].tag();
            if t == "li" {
                stack.pop();
            } else {
                break;
            }
        }
    }
    // td/th 遇 td/th/tr
    if name == "td" || name == "th" || name == "tr" {
        while let Some(&top) = stack.last() {
            let t = dom.nodes[top].tag();
            if t == "td" || t == "th" {
                stack.pop();
            } else {
                break;
            }
        }
    }
    // tr 遇 tr
    if name == "tr" {
        while let Some(&top) = stack.last() {
            let t = dom.nodes[top].tag();
            if t == "tr" {
                stack.pop();
            } else {
                break;
            }
        }
    }
}
