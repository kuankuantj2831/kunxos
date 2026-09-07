//! DAV 客户端核心：请求管道、发现、multistatus 解析。
//!
//! CalDAV（caldav.rs）与 CardDAV（carddav.rs）共享本模块的传输层与
//! 发现逻辑；两者各自的 REPORT 体与资源类型由对应模块封装。
//!
//! 缓冲策略：本模块**不持有**缓冲——每个方法由调用方传入
//! `resp`（响应缓冲）、`req`（请求缓冲）、`xml`（XML 解析缓冲）。
//! 这样解析出的切片生命周期始终在调用方作用域内，无自引用。

use crate::sync::http::{self, HttpErr, Net, Response};
use crate::sync::model::*;
use crate::sync::util;
use crate::sync::xml::{self, XmlDoc};

/// DAV 命名空间。
pub const NS_DAV: &[u8] = b"DAV:";

pub const MAX_ITEMS: usize = 96;

/// 解析 `<host[:port]>`，返回 (host, port)。
pub fn parse_server(srv: &[u8]) -> (&[u8], u16) {
    if let Some(i) = util::find_byte(srv, b':') {
        let host = &srv[..i];
        let port = util::parse_u32(&srv[i + 1..]).filter(|&p| p <= 65535).unwrap_or(80) as u16;
        return (host, port);
    }
    (srv, 80)
}

/// multistatus 中的一个响应项（借用解析缓冲）。
#[derive(Clone, Copy)]
pub struct DavItem<'a> {
    pub href: &'a [u8],
    pub status: u16,
    pub etag: &'a [u8],
    /// calendar-data / address-data 原文。
    pub data: &'a [u8],
    /// 是否为集合（resourcetype 含 collection）。
    pub is_coll: bool,
}

/// 解析 multistatus 文档为响应项数组，返回数量。
pub fn parse_multistatus<'a>(doc: &'a XmlDoc<'a>, items: &mut [DavItem<'a>]) -> usize {
    let mut n = 0usize;
    if doc.n == 0 {
        return 0;
    }
    let root = 0usize;
    for k in 0..doc.elems[root].n_children {
        let ci = doc.elems[root].children[k];
        let e = &doc.elems[ci];
        if !util::eq_ignore_case(e.name, b"response") {
            continue;
        }
        if n >= items.len() {
            break;
        }
        let mut href: &[u8] = &[];
        let mut status: u16 = 0;
        let mut etag: &[u8] = &[];
        let mut data: &[u8] = &[];
        let mut is_coll = false;
        for ck in 0..e.n_children {
            let c = e.children[ck];
            let ce = &doc.elems[c];
            if util::eq_ignore_case(ce.name, b"href") {
                href = ce.text;
            } else if util::eq_ignore_case(ce.name, b"status") {
                status = parse_status_code(ce.text);
            } else if util::eq_ignore_case(ce.name, b"propstat") {
                if status == 0 {
                    if let Some(st) = doc.child(c, b"status", NS_DAV) {
                        status = parse_status_code(doc.elems[st].text);
                    }
                }
                if let Some(prop) = doc.child(c, b"prop", NS_DAV) {
                    let pe = &doc.elems[prop];
                    for pk in 0..pe.n_children {
                        let p = pe.children[pk];
                        let pe2 = &doc.elems[p];
                        if util::eq_ignore_case(pe2.name, b"getetag") {
                            etag = pe2.text;
                        } else if util::eq_ignore_case(pe2.name, b"calendar-data") || util::eq_ignore_case(pe2.name, b"address-data") {
                            data = pe2.text;
                        } else if util::eq_ignore_case(pe2.name, b"resourcetype") {
                            for rk in 0..pe2.n_children {
                                let r = pe2.children[rk];
                                if util::eq_ignore_case(doc.elems[r].name, b"collection") {
                                    is_coll = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        items[n] = DavItem { href, status, etag, data, is_coll };
        n += 1;
    }
    n
}

/// 解析 `HTTP/1.1 207 Multi-Status` 形式的状态行 → 状态码。
pub fn parse_status_code(v: &[u8]) -> u16 {
    let t = util::trim(v);
    // 定位数字段：找第二个空格后的 3 位数字
    if let Some(p1) = util::find_byte(t, b' ') {
        let rest = util::trim(&t[p1 + 1..]);
        if rest.len() >= 3 {
            return util::parse_u32(&rest[..3]).unwrap_or(0) as u16;
        }
    }
    util::parse_u32(t).unwrap_or(0) as u16
}

/// 从文档顶层取 sync-token。
pub fn doc_sync_token<'a>(doc: &'a XmlDoc<'a>) -> &'a [u8] {
    if doc.n == 0 {
        return &[];
    }
    let root = 0usize;
    for k in 0..doc.elems[root].n_children {
        let c = doc.elems[root].children[k];
        if util::eq_ignore_case(doc.elems[c].name, b"sync-token") {
            return doc.elems[c].text;
        }
    }
    &[]
}

/// 从文档（PROPFIND 响应）取 getctag。
pub fn doc_ctag<'a>(doc: &'a XmlDoc<'a>) -> &'a [u8] {
    if doc.n == 0 {
        return &[];
    }
    walk_prop_first(doc, 0, b"getctag")
}

/// 深度优先找第一个名为 name 的 prop 值文本。
pub fn walk_prop_first<'a>(doc: &'a XmlDoc<'a>, idx: usize, name: &[u8]) -> &'a [u8] {
    let e = &doc.elems[idx];
    if util::eq_ignore_case(e.name, name) {
        return e.text;
    }
    for k in 0..e.n_children {
        let r = walk_prop_first(doc, e.children[k], name);
        if !r.is_empty() {
            return r;
        }
    }
    &[]
}

/// 在文档中按属性名找 prop 值（属性元素文本或其 href 子元素文本）。
/// 显式栈遍历（避免闭包递归导致类型实例化膨胀）。
pub fn walk_prop_value<'d>(doc: &'d XmlDoc<'d>, root: usize, prop_name: &[u8], mut f: impl FnMut(&'d [u8])) {
    let mut stack = [0usize; crate::sync::xml::MAX_ELEMS];
    let mut sp = 0usize;
    stack[sp] = root;
    sp += 1;
    while sp > 0 {
        sp -= 1;
        let idx = stack[sp];
        let e = &doc.elems[idx];
        if util::eq_ignore_case(e.name, prop_name) {
            if let Some(v) = doc.child(idx, b"href", NS_DAV) {
                f(doc.elems[v].text);
            } else if e.text.len() > 0 {
                f(e.text);
            }
            continue;
        }
        for k in 0..e.n_children {
            if sp < crate::sync::xml::MAX_ELEMS {
                stack[sp] = e.children[k];
                sp += 1;
            }
        }
    }
}

/// 从 multistatus 文档中确定某 href 对应集合的资源类型。
pub fn collection_kind_from_doc(doc: &XmlDoc<'_>, href: &[u8]) -> Option<u8> {
    let root = 0usize;
    for k in 0..doc.elems[root].n_children {
        let resp = doc.elems[root].children[k];
        let re = &doc.elems[resp];
        if !util::eq_ignore_case(re.name, b"response") {
            continue;
        }
        let mut r_href: &[u8] = &[];
        for ck in 0..re.n_children {
            let c = re.children[ck];
            if util::eq_ignore_case(doc.elems[c].name, b"href") {
                r_href = doc.elems[c].text;
            }
        }
        if r_href != href {
            continue;
        }
        for ck in 0..re.n_children {
            let c = re.children[ck];
            if !util::eq_ignore_case(doc.elems[c].name, b"propstat") {
                continue;
            }
            if let Some(prop) = doc.child(c, b"prop", NS_DAV) {
                let pe = &doc.elems[prop];
                for pk in 0..pe.n_children {
                    let p = pe.children[pk];
                    let pn = doc.elems[p].name;
                    if util::eq_ignore_case(pn, b"calendar") {
                        return Some(COLL_CALENDAR);
                    }
                    if util::eq_ignore_case(pn, b"addressbook") {
                        return Some(COLL_ADDRBOOK);
                    }
                    // resourcetype 的子元素（c:calendar / card:addressbook）
                    if util::eq_ignore_case(pn, b"resourcetype") {
                        let rt = &doc.elems[p];
                        for rk in 0..rt.n_children {
                            let rn = doc.elems[rt.children[rk]].name;
                            if util::eq_ignore_case(rn, b"calendar") {
                                return Some(COLL_CALENDAR);
                            }
                            if util::eq_ignore_case(rn, b"addressbook") {
                                return Some(COLL_ADDRBOOK);
                            }
                        }
                    }
                }
            }
        }
        return None;
    }
    None
}

/// DAV 客户端：连接上下文（不持有缓冲）。
pub struct DavClient<'a> {
    pub net: &'a mut dyn Net,
    pub host: [u8; SERVER_LEN],
    pub host_len: usize,
    pub port: u16,
    /// Authorization 头值（"Basic xxx" 或空）。
    pub auth: [u8; 260],
    pub auth_len: usize,
}

impl<'a> DavClient<'a> {
    pub fn new(net: &'a mut dyn Net, server: &[u8], auth: &[u8]) -> Self {
        let (host, port) = parse_server(server);
        let mut h = [0u8; SERVER_LEN];
        let hl = util::copy_into(&mut h, host);
        let mut a = [0u8; 260];
        let al = util::copy_into(&mut a, auth);
        DavClient { net, host: h, host_len: hl, port, auth: a, auth_len: al }
    }

    /// 发起 HTTP 请求；`extra` 为额外头（以 \r\n 结尾）。
    pub fn request<'b>(
        &mut self,
        method: &[u8],
        path: &[u8],
        extra: &[u8],
        body: &[u8],
        resp: &'b mut [u8],
        req: &mut [u8],
    ) -> Result<Response<'b>, HttpErr> {
        let mut auth_hdr = [0u8; 280];
        let mut ah = 0usize;
        if self.auth_len > 0 {
            ah = util::copy_into(&mut auth_hdr, b"Authorization: ");
            ah += util::copy_into(&mut auth_hdr[ah..], &self.auth[..self.auth_len]);
            ah += util::copy_into(&mut auth_hdr[ah..], b"\r\n");
        }
        let mut hdrs = [0u8; 340];
        let mut hn = util::copy_into(&mut hdrs, &auth_hdr[..ah]);
        hn += util::copy_into(&mut hdrs[hn..], extra);
        let req_len = http::build_request(method, path, &self.host[..self.host_len], &hdrs[..hn], body, req);
        http::request(self.net, &self.host[..self.host_len], self.port, &req[..req_len], resp)
    }

    /// PROPFIND：响应解析为 XML 文档。返回 HTTP 状态。
    pub fn propfind<'x>(
        &mut self,
        path: &[u8],
        depth: &[u8],
        props: &[&[u8]],
        resp: &mut [u8],
        req: &mut [u8],
        xml: &'x mut [u8],
        doc: &mut XmlDoc<'x>,
    ) -> Result<u16, HttpErr> {
        let mut extra = [0u8; 16];
        let en = util::push_str(&mut extra, 0, b"Depth: ");
        let en = util::push_str(&mut extra, en, depth);
        let en = util::push_str(&mut extra, en, b"\r\n");
        let mut body = [0u8; 640];
        let bn = xml::build_propfind(props, &mut body);
        let r = self.request(b"PROPFIND", path, &extra[..en], &body[..bn], resp, req)?;
        if r.status != 207 && r.status != 200 && r.status != 404 {
            return Ok(r.status);
        }
        let bl = util::copy_into(xml, r.body);
        doc.ok = false;
        xml::parse_xml(&xml[..bl], doc);
        Ok(r.status)
    }

    /// REPORT：发送 REPORT 请求体，响应解析为 XML 文档。
    pub fn report<'x>(
        &mut self,
        path: &[u8],
        body: &[u8],
        depth: &[u8],
        resp: &mut [u8],
        req: &mut [u8],
        xml: &'x mut [u8],
        doc: &mut XmlDoc<'x>,
    ) -> Result<u16, HttpErr> {
        let mut extra = [0u8; 16];
        let en = util::push_str(&mut extra, 0, b"Depth: ");
        let en = util::push_str(&mut extra, en, depth);
        let en = util::push_str(&mut extra, en, b"\r\n");
        let r = self.request(b"REPORT", path, &extra[..en], body, resp, req)?;
        if r.status != 207 && r.status != 200 && r.status != 404 {
            return Ok(r.status);
        }
        let bl = util::copy_into(xml, r.body);
        doc.ok = false;
        xml::parse_xml(&xml[..bl], doc);
        Ok(r.status)
    }

    /// PUT：创建 / 更新资源。`if_match` / `if_none_match` 为 ETag 条件或空。
    pub fn put(&mut self, path: &[u8], data: &[u8], content_type: &[u8], if_match: &[u8], if_none_match: &[u8], resp: &mut [u8], req: &mut [u8]) -> Result<u16, HttpErr> {
        let mut extra = [0u8; 320];
        let mut en = util::push_str(&mut extra, 0, b"Content-Type: ");
        en += util::copy_into(&mut extra[en..], content_type);
        en += util::copy_into(&mut extra[en..], b"\r\n");
        if !if_match.is_empty() {
            en += util::copy_into(&mut extra[en..], b"If-Match: ");
            en += util::copy_into(&mut extra[en..], if_match);
            en += util::copy_into(&mut extra[en..], b"\r\n");
        }
        if !if_none_match.is_empty() {
            en += util::copy_into(&mut extra[en..], b"If-None-Match: ");
            en += util::copy_into(&mut extra[en..], if_none_match);
            en += util::copy_into(&mut extra[en..], b"\r\n");
        }
        let r = self.request(b"PUT", path, &extra[..en], data, resp, req)?;
        Ok(r.status)
    }

    pub fn delete(&mut self, path: &[u8], resp: &mut [u8], req: &mut [u8]) -> Result<u16, HttpErr> {
        let r = self.request(b"DELETE", path, b"", b"", resp, req)?;
        Ok(r.status)
    }

    /// MKCOL：创建日历 / 地址簿集合。
    pub fn mkcol(&mut self, path: &[u8], kind: u8, displayname: &[u8], resp: &mut [u8], req: &mut [u8]) -> Result<u16, HttpErr> {
        let mut body = [0u8; 640];
        let bn = xml::build_mkcol(kind, displayname, &mut body);
        let r = self.request(b"MKCOL", path, b"", &body[..bn], resp, req)?;
        Ok(r.status)
    }

    /// 发现当前用户 principal 路径（RFC 5397）。写入 out，返回长度（0=未发现）。
    pub fn discover_principal(&mut self, base_path: &[u8], out: &mut [u8], resp: &mut [u8], req: &mut [u8], xml: &mut [u8]) -> Result<usize, HttpErr> {
        let mut doc = XmlDoc::new();
        let st = self.propfind(base_path, b"0", &[b"current-user-principal"], resp, req, xml, &mut doc)?;
        if st != 207 || doc.n == 0 {
            return Ok(0);
        }
        let mut found = 0usize;
        walk_prop_value(&doc, 0, b"current-user-principal", |v| {
            if found == 0 {
                found = util::copy_into(out, v);
            }
        });
        Ok(found)
    }

    /// 发现 home-set（calendar-home-set / addressbook-home-set）路径。
    pub fn discover_home(&mut self, principal_path: &[u8], prop_name: &[u8], out: &mut [u8], resp: &mut [u8], req: &mut [u8], xml: &mut [u8]) -> Result<usize, HttpErr> {
        let mut doc = XmlDoc::new();
        let st = self.propfind(principal_path, b"0", &[prop_name], resp, req, xml, &mut doc)?;
        if st != 207 || doc.n == 0 {
            return Ok(0);
        }
        let mut found = 0usize;
        walk_prop_value(&doc, 0, prop_name, |v| {
            if found == 0 {
                found = util::copy_into(out, v);
            }
        });
        Ok(found)
    }

    /// 列出 home 下所有集合，填充 out，返回数量（kind 过滤）。
    pub fn list_collections(&mut self, home_path: &[u8], kind: u8, out: &mut [Collection], resp: &mut [u8], req: &mut [u8], xml: &mut [u8]) -> usize {
        let mut doc = XmlDoc::new();
        let props: &[&[u8]] = &[b"resourcetype", b"displayname", b"getctag", b"sync-token"];
        let st = match self.propfind(home_path, b"1", props, resp, req, xml, &mut doc) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        if st != 207 || doc.n == 0 {
            return 0;
        }
        let mut items = [DavItem { href: &[], status: 0, etag: &[], data: &[], is_coll: false }; MAX_ITEMS];
        let n = parse_multistatus(&doc, &mut items);
        let mut k = 0usize;
        for i in 0..n {
            if !items[i].is_coll || k >= out.len() {
                continue;
            }
            let coll_kind = match collection_kind_from_doc(&doc, items[i].href) {
                Some(kd) => kd,
                None => continue,
            };
            if coll_kind != kind {
                continue;
            }
            let mut c = Collection::new();
            c.kind = kind;
            c.path_len = util::copy_into(&mut c.path, items[i].href);
            // displayname / ctag 由 collection_meta 单独补齐
            out[k] = c;
            k += 1;
        }
        k
    }
}

/// 为集合获取 displayname / getctag / sync-token（单次 PROPFIND）。
pub fn collection_meta(
    dav: &mut DavClient<'_>,
    coll: &mut Collection,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
) -> Result<(), HttpErr> {
    let mut doc = XmlDoc::new();
    let st = dav.propfind(&coll.path[..coll.path_len], b"0", &[b"displayname", b"getctag", b"sync-token"], resp, req, xml, &mut doc)?;
    if st != 207 || doc.n == 0 {
        return Err(HttpErr::BadResponse);
    }
    let dn = walk_prop_first(&doc, 0, b"displayname");
    if !dn.is_empty() {
        coll.name_len = util::copy_into(&mut coll.name, dn);
    }
    let cg = walk_prop_first(&doc, 0, b"getctag");
    if !cg.is_empty() {
        coll.ctag_len = util::copy_into(&mut coll.ctag, cg);
    }
    let stk = walk_prop_first(&doc, 0, b"sync-token");
    if !stk.is_empty() {
        coll.sync_token_len = util::copy_into(&mut coll.sync_token, stk);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_addr() {
        assert_eq!(parse_server(b"example.com:8080"), (&b"example.com"[..], 8080));
        assert_eq!(parse_server(b"10.0.2.2"), (&b"10.0.2.2"[..], 80));
        assert_eq!(parse_server(b"h:99999"), (&b"h"[..], 80));
    }

    #[test]
    fn status_code() {
        assert_eq!(parse_status_code(b"HTTP/1.1 207 Multi-Status"), 207);
        assert_eq!(parse_status_code(b"HTTP/1.1 404 Not Found"), 404);
        assert_eq!(parse_status_code(b"200"), 200);
    }

    #[test]
    fn multistatus_items() {
        let text = br#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/cal/1.ics</d:href>
<d:propstat><d:prop><d:getetag>"e1"</d:getetag></d:prop>
<d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/cal/2.ics</d:href>
<d:status>HTTP/1.1 404 Not Found</d:status></d:response>
<d:sync-token>tok-9</d:sync-token>
</d:multistatus>"#;
        let mut doc = XmlDoc::new();
        assert!(xml::parse_xml(text, &mut doc));
        let mut items = [DavItem { href: &[], status: 0, etag: &[], data: &[], is_coll: false }; 8];
        let n = parse_multistatus(&doc, &mut items);
        assert_eq!(n, 2);
        assert_eq!(items[0].href, b"/cal/1.ics");
        assert_eq!(items[0].status, 200);
        assert_eq!(items[0].etag, b"\"e1\"");
        assert_eq!(items[1].status, 404);
        assert_eq!(doc_sync_token(&doc), b"tok-9");
    }

    #[test]
    fn discovery_walk() {
        let text = br#"<d:multistatus xmlns:d="DAV:"><d:response>
<d:href>/</d:href><d:propstat><d:prop>
<d:current-user-principal><d:href>/principals/me/</d:href></d:current-user-principal>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let mut doc = XmlDoc::new();
        assert!(xml::parse_xml(text, &mut doc));
        let mut out = [0u8; 128];
        let mut found = 0usize;
        walk_prop_value(&doc, 0, b"current-user-principal", |v| {
            if found == 0 {
                found = util::copy_into(&mut out, v);
            }
        });
        assert_eq!(&out[..found], b"/principals/me/");
    }
}
