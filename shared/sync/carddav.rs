//! CardDAV 客户端（RFC 6352 + RFC 6578 增量同步子集）。
//!
//! 提供：
//!   - 发现：current-user-principal → addressbook-home-set → 地址簿列表
//!   - PROPFIND：地址簿元数据（displayname / getctag / sync-token）
//!   - REPORT：sync-collection（RFC 6578）/ addressbook-query（全量回退）/
//!     addressbook-multiget（批量取回 vCard）
//!   - PUT / DELETE：联系人 CRUD（If-Match / If-None-Match）
//!   - MKCOL：创建地址簿
//!
//! vCard 3.0/4.0 解析与照片 / 分组处理在 vcard.rs。

use crate::sync::dav::{self, DavClient, DavItem};
use crate::sync::http::HttpErr;
use crate::sync::model::*;
use crate::sync::util;
use crate::sync::xml::XmlDoc;

pub use super::caldav::{http_etag, MAX_SYNC_ITEMS, SyncDelta};

pub const CT_VCARD: &[u8] = b"text/vcard; charset=utf-8";

/// 执行 sync-collection REPORT（与 CalDAV 相同语义）。
pub fn sync_collection(
    dav: &mut DavClient<'_>,
    path: &[u8],
    token: &[u8],
    delta: &mut SyncDelta,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
) -> Result<(), HttpErr> {
    let mut body = [0u8; 1024];
    let bn = crate::sync::xml::build_sync_report(token, &[b"getetag"], &mut body);
    let mut doc = XmlDoc::new();
    let st = dav.report(path, &body[..bn], b"1", resp, req, xml, &mut doc)?;
    if st == 207 || st == 200 {
        delta.supported = true;
        let t = dav::doc_sync_token(&doc);
        delta.token_len = util::copy_into(&mut delta.token, t);
        let mut items = [DavItem { href: &[], status: 0, etag: &[], data: &[], is_coll: false }; dav::MAX_ITEMS];
        let n = dav::parse_multistatus(&doc, &mut items);
        for i in 0..n {
            let it = &items[i];
            if it.status == 200 || it.status == 201 {
                if delta.n_changed < MAX_SYNC_ITEMS {
                    delta.changed_len[delta.n_changed] = util::copy_into(&mut delta.changed[delta.n_changed], it.href);
                    delta.n_changed += 1;
                }
            } else if it.status == 404 {
                if delta.n_deleted < MAX_SYNC_ITEMS {
                    delta.deleted_len[delta.n_deleted] = util::copy_into(&mut delta.deleted[delta.n_deleted], it.href);
                    delta.n_deleted += 1;
                }
            }
        }
        Ok(())
    } else if st == 400 || st == 403 || st == 501 || st == 405 {
        delta.supported = false;
        Ok(())
    } else {
        Err(HttpErr::BadResponse)
    }
}

/// addressbook-query：全量列出 (href, etag)（回退路径）。
pub fn addressbook_query<F>(
    dav: &mut DavClient<'_>,
    path: &[u8],
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    mut f: F,
) -> Result<(), HttpErr>
where
    F: FnMut(&[u8], &[u8]) -> bool,
{
    let mut body = [0u8; 1024];
    let mut n = 0usize;
    n = util::push_str(&mut body, n, b"<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    n = util::push_str(&mut body, n, b"<d:addressbook-query xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\">");
    n = util::push_str(&mut body, n, b"<d:prop><d:getetag/></d:prop><card:filter><card:prop-filter name=\"FN\"/></card:filter></d:addressbook-query>");
    let mut doc = XmlDoc::new();
    let st = dav.report(path, &body[..n], b"1", resp, req, xml, &mut doc)?;
    if st != 207 && st != 200 {
        return Err(HttpErr::BadResponse);
    }
    let mut items = [DavItem { href: &[], status: 0, etag: &[], data: &[], is_coll: false }; dav::MAX_ITEMS];
    let n = dav::parse_multistatus(&doc, &mut items);
    for i in 0..n {
        if items[i].status == 200 {
            if !f(items[i].href, items[i].etag) {
                break;
            }
        }
    }
    Ok(())
}

/// addressbook-multiget：批量取回 vCard。
pub fn addressbook_multiget<F>(
    dav: &mut DavClient<'_>,
    path: &[u8],
    hrefs: &[&[u8]],
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    mut f: F,
) -> Result<(), HttpErr>
where
    F: FnMut(&[u8], &[u8], &[u8]) -> bool,
{
    if hrefs.is_empty() {
        return Ok(());
    }
    let mut body = [0u8; 8192];
    let bn = crate::sync::xml::build_multiget(&[b"getetag"], hrefs, &mut body);
    let mut doc = XmlDoc::new();
    let st = dav.report(path, &body[..bn], b"1", resp, req, xml, &mut doc)?;
    if st != 207 && st != 200 {
        return Err(HttpErr::BadResponse);
    }
    let mut items = [DavItem { href: &[], status: 0, etag: &[], data: &[], is_coll: false }; dav::MAX_ITEMS];
    let n = dav::parse_multistatus(&doc, &mut items);
    for i in 0..n {
        if items[i].status == 200 && !items[i].data.is_empty() {
            if !f(items[i].href, items[i].data, items[i].etag) {
                break;
            }
        }
    }
    Ok(())
}

/// 推送联系人（创建或更新）。返回 (状态码, 新 ETag)。
pub fn put_contact(
    dav: &mut DavClient<'_>,
    path: &[u8],
    vcard: &[u8],
    if_match: &[u8],
    if_none_match: &[u8],
    etag_out: &mut [u8],
    resp: &mut [u8],
    req: &mut [u8],
) -> Result<u16, HttpErr> {
    let st = dav.put(path, vcard, CT_VCARD, if_match, if_none_match, resp, req)?;
    if st == 200 || st == 201 || st == 204 {
        let et = http_etag(resp);
        let _ = util::copy_into(etag_out, et);
    }
    Ok(st)
}

/// 删除联系人。
pub fn delete_contact(dav: &mut DavClient<'_>, path: &[u8], resp: &mut [u8], req: &mut [u8]) -> Result<u16, HttpErr> {
    dav.delete(path, resp, req)
}

/// 发现 + 建立地址簿集合列表。
pub fn discover_addressbooks(
    dav: &mut DavClient<'_>,
    base_path: &[u8],
    out: &mut [Collection],
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
) -> Result<usize, HttpErr> {
    let mut principal = [0u8; PRINCIPAL_LEN];
    let pl = dav.discover_principal(base_path, &mut principal, resp, req, xml)?;
    let principal = if pl > 0 { &principal[..pl] } else { base_path };
    let mut home = [0u8; PRINCIPAL_LEN];
    let hl = dav.discover_home(principal, b"addressbook-home-set", &mut home, resp, req, xml)?;
    let home = if hl > 0 { &home[..hl] } else { base_path };
    Ok(dav.list_collections(home, COLL_ADDRBOOK, out, resp, req, xml))
}
