//! 极简 XML 解析与构建（DAV multistatus / PROPFIND / REPORT 用）。
//!
//! 解析器为递归下降 + 显式栈（无堆）：输出元素数组，元素保存本地名、
//! 命名空间 URI（继承解析）、属性与子元素索引。实体引用（`&amp;` 等）
//! 不在此处解码——调用方经 `decode_entities` 按需解码，避免破坏结构。
//!
//! 容量：128 个元素 / 每元素 8 属性 / 32 子元素 / 16 层嵌套（DAV 响应
//! 的 multistatus → response → propstat → prop → 具体属性 恰好 5 层）。

use crate::sync::util;

pub const MAX_ELEMS: usize = 128;
pub const MAX_ATTRS: usize = 8;
pub const MAX_CHILDREN: usize = 32;
pub const MAX_DEPTH: usize = 16;

#[derive(Clone, Copy)]
pub struct XmlAttr<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

#[derive(Clone, Copy)]
pub struct XmlElem<'a> {
    /// 本地名（去掉前缀）。
    pub name: &'a [u8],
    /// 命名空间 URI（如 `DAV:`）；无命名空间时为 None。
    pub ns: Option<&'a [u8]>,
    pub attrs: [XmlAttr<'a>; MAX_ATTRS],
    pub n_attrs: usize,
    /// 子元素在 doc.elems 中的下标。
    pub children: [usize; MAX_CHILDREN],
    pub n_children: usize,
    /// 直接文本内容（trimmed，未解码实体）。
    pub text: &'a [u8],
    pub parent: i32,
}

pub struct XmlDoc<'a> {
    pub elems: [XmlElem<'a>; MAX_ELEMS],
    pub n: usize,
    pub ok: bool,
}

impl<'a> XmlDoc<'a> {
    pub fn new() -> Self {
        XmlDoc {
            elems: [XmlElem {
                name: &[],
                ns: None,
                attrs: [XmlAttr { name: &[], value: &[] }; MAX_ATTRS],
                n_attrs: 0,
                children: [0; MAX_CHILDREN],
                n_children: 0,
                text: &[],
                parent: -1,
            }; MAX_ELEMS],
            n: 0,
            ok: false,
        }
    }

    /// 按名称与命名空间找子元素。
    pub fn child(&self, i: usize, name: &[u8], ns: &[u8]) -> Option<usize> {
        let e = &self.elems[i];
        for k in 0..e.n_children {
            let c = e.children[k];
            let ce = &self.elems[c];
            if util::eq_ignore_case(ce.name, name) && ce.ns.map_or(false, |n| n == ns) {
                return Some(c);
            }
        }
        None
    }

    /// 按名称找子元素（不限命名空间）。
    pub fn child_any_ns(&self, i: usize, name: &[u8]) -> Option<usize> {
        let e = &self.elems[i];
        for k in 0..e.n_children {
            let c = e.children[k];
            if util::eq_ignore_case(self.elems[c].name, name) {
                return Some(c);
            }
        }
        None
    }

    /// 取属性值。
    pub fn attr(&self, i: usize, name: &[u8]) -> Option<&'a [u8]> {
        let e = &self.elems[i];
        for k in 0..e.n_attrs {
            if util::eq_ignore_case(e.attrs[k].name, name) {
                return Some(e.attrs[k].value);
            }
        }
        None
    }
}

/// 实体解码（`&amp;` `&lt;` `&gt;` `&quot;` `&apos;` `&#NN;`）到 out。
pub fn decode_entities(src: &[u8], out: &mut [u8]) -> usize {
    let mut o = 0usize;
    let mut i = 0usize;
    while i < src.len() {
        if src[i] == b'&' {
            if let Some(semi) = util::find_byte_from(src, b';', i + 1) {
                let ent = &src[i + 1..semi];
                let ch: Option<u8> = match ent {
                    b"amp" => Some(b'&'),
                    b"lt" => Some(b'<'),
                    b"gt" => Some(b'>'),
                    b"quot" => Some(b'"'),
                    b"apos" => Some(b'\''),
                    _ => {
                        if ent.len() >= 2 && ent[0] == b'#' {
                            match util::parse_u32(&ent[1..]) {
                                Some(num) if num <= 255 => Some(num as u8),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                };
                if let Some(c) = ch {
                    if o < out.len() {
                        out[o] = c;
                    }
                    o += 1;
                    i = semi + 1;
                    continue;
                }
            }
        }
        if o < out.len() {
            out[o] = src[i];
        }
        o += 1;
        i += 1;
    }
    o.min(out.len())
}

/// 解析 XML 文本为元素树。text 需以 NUL 结尾或用 len 限定。
pub fn parse_xml<'a>(text: &'a [u8], doc: &mut XmlDoc<'a>) -> bool {
    doc.n = 0;
    doc.ok = false;
    // 命名空间栈（每帧：前缀→URI 映射）+ 元素索引栈（父元素追踪）
    let mut stack: [([(&[u8], &[u8]); 8], usize); MAX_DEPTH] = [([(&[], &[]); 8], 0); MAX_DEPTH];
    let mut elem_stack: [i32; MAX_DEPTH] = [-1; MAX_DEPTH];
    let mut depth = 0usize;
    let mut top: i32 = -1;
    let mut i = 0usize;
    let len = text.len();

    while i < len {
        // 跳过 '<' 前的文本
        match util::find_byte_from(text, b'<', i) {
            Some(lt) => {
                let text_content = &text[i..lt];
                if top >= 0 && !util::trim(text_content).is_empty() {
                    let t = top as usize;
                    doc.elems[t].text = util::trim(text_content);
                }
                i = lt;
            }
            None => break,
        }
        // 注释
        if text[i..].starts_with(b"<!--") {
            match find_closing(text, i + 4, b"-->") {
                Some(e) => {
                    i = e + 3;
                    continue;
                }
                None => return false,
            }
        }
        // 处理指令 / DOCTYPE
        if text[i..].starts_with(b"<?") {
            match find_closing(text, i + 2, b"?>") {
                Some(e) => {
                    i = e + 2;
                    continue;
                }
                None => return false,
            }
        }
        if text[i..].starts_with(b"<!") {
            match find_closing(text, i + 2, b">") {
                Some(e) => {
                    i = e + 1;
                    continue;
                }
                None => return false,
            }
        }
        // 闭合标签
        if text[i..].starts_with(b"</") {
            match find_closing(text, i + 2, b">") {
                Some(e) => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                    top = if depth == 0 { -1 } else { elem_stack[depth - 1] };
                    i = e + 1;
                    continue;
                }
                None => return false,
            }
        }
        // 开始标签
        if text[i] == b'<' {
            let (tag, end, self_close) = match parse_open_tag(text, i) {
                Some(x) => x,
                None => return false,
            };
            i = end;
            // 解析标签名与属性
            let (name_with_prefix, attrs_raw) = match util::find_byte(tag, b' ') {
                Some(sp) => (&tag[..sp], &tag[sp + 1..]),
                None => (tag, &[][..]),
            };
            // 提取 xmlns 声明（本元素作用域）
            let mut local_ns: [(&[u8], &[u8]); 8] = [(&[], &[]); 8];
            let mut n_local = 0usize;
            let mut attrs: [XmlAttr<'a>; MAX_ATTRS] = [XmlAttr { name: &[], value: &[] }; MAX_ATTRS];
            let mut n_attrs = 0usize;
            let mut rest = attrs_raw;
            while !rest.is_empty() {
                let (pair, tail) = match util::find_byte(rest, b'=') {
                    Some(eq) => {
                        let key = util::trim(&rest[..eq]);
                        let val_raw = util::trim(&rest[eq + 1..]);
                        let (val, after) = parse_attr_value(val_raw);
                        (Some((key, val)), after)
                    }
                    None => (None, &[][..]),
                };
                if let Some((key, val)) = pair {
                    if util::starts_with_ci(key, b"xmlns:") {
                        if n_local < 8 {
                            local_ns[n_local] = (util::trim(&key[6..]), val);
                            n_local += 1;
                        }
                    } else if util::eq_ignore_case(key, b"xmlns") {
                        if n_local < 8 {
                            local_ns[n_local] = (b"", val);
                            n_local += 1;
                        }
                    } else if n_attrs < MAX_ATTRS {
                        // 属性名去前缀
                        let aname = match util::find_byte(key, b':') {
                            Some(ci) => &key[ci + 1..],
                            None => key,
                        };
                        attrs[n_attrs] = XmlAttr { name: aname, value: val };
                        n_attrs += 1;
                    }
                }
                if tail.is_empty() {
                    break;
                }
                rest = tail;
            }
            // 解析名称（前缀 → ns）
            let (prefix, local) = match util::find_byte(name_with_prefix, b':') {
                Some(ci) => (&name_with_prefix[..ci], &name_with_prefix[ci + 1..]),
                None => (&[][..], name_with_prefix),
            };
            let mut ns: Option<&[u8]> = None;
            // 先查本元素自己的 xmlns 声明，再向上查父帧（根元素可见自身声明）
            for k in 0..n_local {
                if local_ns[k].0 == prefix {
                    ns = Some(local_ns[k].1);
                    break;
                }
            }
            if ns.is_none() {
                for l in (0..depth).rev() {
                    let map = &stack[l].0;
                    let n = stack[l].1;
                    for k in 0..n {
                        if map[k].0 == prefix {
                            ns = Some(map[k].1);
                            break;
                        }
                    }
                    if ns.is_some() {
                        break;
                    }
                }
            }
            // 记录本元素的 xmlns 到栈帧
            if depth < MAX_DEPTH {
                let frame = &mut stack[depth];
                frame.1 = n_local;
                for k in 0..n_local {
                    frame.0[k] = local_ns[k];
                }
            }
            // 创建元素
            if doc.n >= MAX_ELEMS {
                return false;
            }
            let idx = doc.n;
            doc.n += 1;
            if depth < MAX_DEPTH {
                elem_stack[depth] = idx as i32;
            }
            doc.elems[idx] = XmlElem {
                name: local,
                ns,
                attrs,
                n_attrs,
                children: [0; MAX_CHILDREN],
                n_children: 0,
                text: &[],
                parent: top,
            };
            if top >= 0 {
                let p = top as usize;
                if doc.elems[p].n_children < MAX_CHILDREN {
                    doc.elems[p].children[doc.elems[p].n_children] = idx;
                    doc.elems[p].n_children += 1;
                }
            }
            if !self_close {
                if depth + 1 >= MAX_DEPTH {
                    return false;
                }
                depth += 1;
                top = idx as i32;
            }
            continue;
        }
        i += 1;
    }
    doc.ok = true;
    true
}

fn find_closing(text: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
    while i + needle.len() <= text.len() {
        if &text[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 解析 `<...>` 开标签：返回 (标签内容, 结束下标, 是否自闭合)。
fn parse_open_tag(text: &[u8], from: usize) -> Option<(&[u8], usize, bool)> {
    let mut i = from + 1;
    let mut in_q: u8 = 0; // 0 无引号, 1 双引号, 2 单引号
    while i < text.len() {
        let c = text[i];
        match c {
            b'"' if in_q == 0 => in_q = 1,
            b'\'' if in_q == 0 => in_q = 2,
            b'"' if in_q == 1 => in_q = 0,
            b'\'' if in_q == 2 => in_q = 0,
            b'>' if in_q == 0 => {
                let tag = &text[from + 1..i];
                let self_close = tag.len() >= 1 && tag[tag.len() - 1] == b'/';
                let tag = if self_close { &tag[..tag.len() - 1] } else { tag };
                return Some((util::trim(tag), i + 1, self_close));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 解析属性值（`"..."` / `'...'` / 无引号），返回 (值, 剩余)。
fn parse_attr_value(v: &[u8]) -> (&[u8], &[u8]) {
    if v.is_empty() {
        return (v, &[]);
    }
    let quote = v[0];
    if quote == b'"' || quote == b'\'' {
        if let Some(e) = util::find_byte_from(v, quote, 1) {
            return (&v[1..e], util::trim(&v[e + 1..]));
        }
        return (&v[1..], &[][..]);
    }
    // 无引号：到空白为止
    match v.iter().position(|&c| c == b' ' || c == b'\t') {
        Some(sp) => (&v[..sp], util::trim(&v[sp..])),
        None => (v, &[]),
    }
}

// ==================== 构建辅助 ====================

/// 生成 `<d:propfind>` 请求体（Depth 由调用方设置）。
pub fn build_propfind(props: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<d:propfind xmlns:d=\"DAV:\"><d:prop>");
    for p in props {
        n = util::push_str(out, n, b"<d:");
        n = util::push_str(out, n, p);
        n = util::push_str(out, n, b"/>");
    }
    n = util::push_str(out, n, b"</d:prop></d:propfind>");
    n.min(out.len())
}

/// 生成 `<d:sync-collection>` REPORT 体（RFC 6578）。
/// `token` 为空表示初始同步（服务器返回新 token）。
pub fn build_sync_report(token: &[u8], props: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<d:sync-collection xmlns:d=\"DAV:\">");
    if !token.is_empty() {
        n = util::push_str(out, n, b"<d:sync-token>");
        n = util::push_str(out, n, token);
        n = util::push_str(out, n, b"</d:sync-token>");
    }
    n = util::push_str(out, n, b"<d:sync-level>1</d:sync-level>");
    n = util::push_str(out, n, b"<d:prop><d:getetag/></d:prop>");
    for p in props {
        n = util::push_str(out, n, b"<d:");
        n = util::push_str(out, n, p);
        n = util::push_str(out, n, b"/>");
    }
    n = util::push_str(out, n, b"</d:sync-collection>");
    n.min(out.len())
}

/// 生成 calendar-query REPORT 体（时间范围 + expand，RFC 4791 §7.8）。
pub fn build_cal_query(start: i64, end: i64, props: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<c:calendar-query xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">");
    n = util::push_str(out, n, b"<d:prop><d:getetag/>");
    for p in props {
        n = util::push_str(out, n, b"<d:");
        n = util::push_str(out, n, p);
        n = util::push_str(out, n, b"/>");
    }
    n = util::push_str(out, n, b"</d:prop><c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"VEVENT\">");
    n = util::push_str(out, n, b"<c:time-range start=\"");
    n = push_ical_dt(out, n, start);
    n = util::push_str(out, n, b"\" end=\"");
    n = push_ical_dt(out, n, end);
    n = util::push_str(out, n, b"\"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>");
    n.min(out.len())
}

/// 生成 addressbook-query / multiget 体。
pub fn build_multiget(_props: &[&[u8]], hrefs: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<d:addressbook-multiget xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">");
    n = util::push_str(out, n, b"<d:prop><d:getetag/><card:address-data/></d:prop>");
    for h in hrefs {
        n = util::push_str(out, n, b"<d:href>");
        n = util::push_str(out, n, h);
        n = util::push_str(out, n, b"</d:href>");
    }
    n = util::push_str(out, n, b"</d:addressbook-multiget>");
    n.min(out.len())
}

/// 生成 calendar-multiget 体。
pub fn build_cal_multiget(_props: &[&[u8]], hrefs: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<c:calendar-multiget xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">");
    n = util::push_str(out, n, b"<d:prop><d:getetag/><c:calendar-data/></d:prop>");
    for h in hrefs {
        n = util::push_str(out, n, b"<d:href>");
        n = util::push_str(out, n, h);
        n = util::push_str(out, n, b"</d:href>");
    }
    n = util::push_str(out, n, b"</c:calendar-multiget>");
    n.min(out.len())
}

/// 生成 MKCOL 体（创建日历 / 地址簿集合）。
pub fn build_mkcol(kind: u8, displayname: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(out, n, b"<d:mkcol xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">");
    n = util::push_str(out, n, b"<d:set><d:prop><d:resourcetype><d:collection/>");
    if kind == crate::sync::model::COLL_CALENDAR {
        n = util::push_str(out, n, b"<c:calendar/>");
    } else {
        n = util::push_str(out, n, b"<card:addressbook/>");
    }
    n = util::push_str(out, n, b"</d:resourcetype>");
    if !displayname.is_empty() {
        n = util::push_str(out, n, b"<d:displayname>");
        n = util::push_str(out, n, displayname);
        n = util::push_str(out, n, b"</d:displayname>");
    }
    n = util::push_str(out, n, b"</d:prop></d:set></d:mkcol>");
    n.min(out.len())
}

/// 写入 iCalendar UTC 时间戳（YYYYMMDDTHHMMSSZ）。
pub fn push_ical_dt(out: &mut [u8], at: usize, epoch: i64) -> usize {
    let (y, mo, d, h, mi, s) = crate::sync::util::civil_from_epoch(epoch);
    let mut n = at;
    n = crate::sync::util::push_dec_pad(out, n, y as u64, 4);
    n = crate::sync::util::push_dec_pad(out, n, mo as u64, 2);
    n = crate::sync::util::push_dec_pad(out, n, d as u64, 2);
    n = crate::sync::util::push_str(out, n, b"T");
    n = crate::sync::util::push_dec_pad(out, n, h as u64, 2);
    n = crate::sync::util::push_dec_pad(out, n, mi as u64, 2);
    n = crate::sync::util::push_dec_pad(out, n, s as u64, 2);
    n = crate::sync::util::push_str(out, n, b"Z");
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multistatus() {
        let text = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/cal/1.ics</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"abc"</d:getetag>
        <d:displayname>My Calendar</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/cal/2.ics</d:href>
    <d:status>HTTP/1.1 404 Not Found</d:status>
  </d:response>
</d:multistatus>"#;
        let mut doc = XmlDoc::new();
        assert!(parse_xml(text, &mut doc));
        assert!(doc.ok);
        let root = 0usize;
        assert_eq!(doc.elems[root].name, b"multistatus");
        assert_eq!(doc.elems[root].ns, Some(&b"DAV:"[..]));
        // 两个 response 子元素
        let mut resps = [0usize; 8];
        let mut rn = 0usize;
        for k in 0..doc.elems[root].n_children {
            if doc.elems[doc.elems[root].children[k]].name == b"response" {
                resps[rn] = doc.elems[root].children[k];
                rn += 1;
            }
        }
        assert_eq!(rn, 2);
        let r0 = resps[0];
        let href = doc.child(r0, b"href", b"DAV:").unwrap();
        assert_eq!(doc.elems[href].text, b"/dav/cal/1.ics");
        let propstat = doc.child(r0, b"propstat", b"DAV:").unwrap();
        let prop = doc.child(propstat, b"prop", b"DAV:").unwrap();
        let etag = doc.child(prop, b"getetag", b"DAV:").unwrap();
        assert_eq!(doc.elems[etag].text, b"\"abc\"");
    }

    #[test]
    fn parse_ns_inheritance() {
        let text = br#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><C:calendar-data>DATA</C:calendar-data></D:response></D:multistatus>"#;
        let mut doc = XmlDoc::new();
        assert!(parse_xml(text, &mut doc));
        let resp = doc.child(0, b"response", b"DAV:").unwrap();
        let data = doc.child(resp, b"calendar-data", b"urn:ietf:params:xml:ns:caldav").unwrap();
        assert_eq!(doc.elems[data].text, b"DATA");
    }

    #[test]
    fn entity_decode() {
        let mut out = [0u8; 64];
        let n = decode_entities(b"a&amp;b &lt;x&gt; &#65;", &mut out);
        assert_eq!(&out[..n], b"a&b <x> A");
    }

    #[test]
    fn builders() {
        let mut out = [0u8; 512];
        let n = build_propfind(&[b"getetag", b"displayname"], &mut out);
        assert!(out[..n].windows(8).any(|w| w == b"propfind"));
        let mut out2 = [0u8; 512];
        let n2 = build_sync_report(b"token-1", &[b"getetag"], &mut out2);
        assert!(out2[..n2].windows(15).any(|w| w == b"sync-collection"));
        let mut out3 = [0u8; 512];
        let n3 = build_cal_query(1_700_000_000, 1_800_000_000, &[b"getetag"], &mut out3);
        assert!(out3[..n3].windows(14).any(|w| w == b"calendar-query"));
        let mut out4 = [0u8; 512];
        let n4 = build_mkcol(crate::sync::model::COLL_CALENDAR, b"Work", &mut out4);
        assert!(out4[..n4].windows(5).any(|w| w == b"mkcol"));
    }
}
