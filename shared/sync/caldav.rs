//! CalDAV 客户端（RFC 4791 + RFC 6578 增量同步子集）。
//!
//! 提供：
//!   - 发现：current-user-principal → calendar-home-set → 日历集合列表
//!     （复用 dav.rs）
//!   - PROPFIND：集合元数据（displayname / getctag / sync-token）
//!   - REPORT：
//!       * sync-collection（RFC 6578）：增量变更（改 / 删）+ 新 sync-token
//!       * calendar-query（RFC 4791 §7.8）：时间范围全量查询（ETag 对比
//!         回退路径，服务端不支持 sync-collection 时使用）
//!       * calendar-multiget：按 href 批量取回完整 iCalendar
//!   - PUT / DELETE：事件 CRUD，If-Match（乐观并发）/ If-None-Match（防覆盖）
//!   - MKCOL：创建日历集合
//!
//! 周期事件（RRULE）与时区（TZID → 固定偏移）在 ical.rs 处理；
//! 本模块只关心资源传输与元数据。

use crate::sync::dav::{self, DavClient, DavItem};
use crate::sync::http::HttpErr;
use crate::sync::model::*;
use crate::sync::util;
use crate::sync::xml::{self, XmlDoc};

pub const CT_ICAL: &[u8] = b"text/calendar; charset=utf-8";
/// 一次 sync-collection REPORT 最多处理的变更条目。
pub const MAX_SYNC_ITEMS: usize = 48;

/// sync-collection 的增量变更结果（href 已拷贝为自有缓冲）。
pub struct SyncDelta {
    pub changed: [[u8; KEY_LEN]; MAX_SYNC_ITEMS],
    pub changed_len: [usize; MAX_SYNC_ITEMS],
    pub n_changed: usize,
    pub deleted: [[u8; KEY_LEN]; MAX_SYNC_ITEMS],
    pub deleted_len: [usize; MAX_SYNC_ITEMS],
    pub n_deleted: usize,
    pub token: [u8; TOKEN_LEN],
    pub token_len: usize,
    /// 服务器是否支持 sync-collection（不支持时引擎回退 calendar-query）。
    pub supported: bool,
}

impl SyncDelta {
    pub fn new() -> Self {
        SyncDelta {
            changed: [[0; KEY_LEN]; MAX_SYNC_ITEMS],
            changed_len: [0; MAX_SYNC_ITEMS],
            n_changed: 0,
            deleted: [[0; KEY_LEN]; MAX_SYNC_ITEMS],
            deleted_len: [0; MAX_SYNC_ITEMS],
            n_deleted: 0,
            token: [0; TOKEN_LEN],
            token_len: 0,
            supported: false,
        }
    }
}

/// 执行 sync-collection REPORT。返回 Ok(()) 表示成功（delta.supported
/// 反映服务器能力）；网络 / 协议错误返回 Err。
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
    let bn = xml::build_sync_report(token, &[b"getetag"], &mut body);
    let mut doc = XmlDoc::new();
    let st = dav.report(path, &body[..bn], b"1", resp, req, xml, &mut doc)?;
    if st == 207 || st == 200 {
        delta.supported = true;
        // 顶层 sync-token
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
        // 服务器不支持 sync-collection → 回退全量查询
        delta.supported = false;
        Ok(())
    } else {
        Err(HttpErr::BadResponse)
    }
}

/// calendar-query：按时间范围返回命中条目的 (href, etag)。
/// 回调签名：`f(href, etag)`，返回 false 停止。
pub fn calendar_query<F>(
    dav: &mut DavClient<'_>,
    path: &[u8],
    start: i64,
    end: i64,
    resp: &mut [u8],
    req: &mut [u8],
    xml: &mut [u8],
    mut f: F,
) -> Result<(), HttpErr>
where
    F: FnMut(&[u8], &[u8]) -> bool,
{
    let mut body = [0u8; 1024];
    let bn = xml::build_cal_query(start, end, &[b"getetag"], &mut body);
    let mut doc = XmlDoc::new();
    let st = dav.report(path, &body[..bn], b"1", resp, req, xml, &mut doc)?;
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

/// calendar-multiget：按 href 批量取回完整 iCalendar。
/// 回调签名：`f(href, data, etag)`，返回 false 停止。
pub fn calendar_multiget<F>(
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
    let bn = xml::build_cal_multiget(&[b"getetag"], hrefs, &mut body);
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

/// 推送事件：创建（If-None-Match: *）或更新（If-Match: etag）。
/// 返回 (HTTP 状态, 新 ETag 长度写入 etag_out)。
pub fn put_event(
    dav: &mut DavClient<'_>,
    path: &[u8],
    ical: &[u8],
    if_match: &[u8],
    if_none_match: &[u8],
    etag_out: &mut [u8],
    resp: &mut [u8],
    req: &mut [u8],
) -> Result<u16, HttpErr> {
    let st = dav.put(path, ical, CT_ICAL, if_match, if_none_match, resp, req)?;
    if st == 200 || st == 201 || st == 204 {
        let et = http_etag(resp);
        let _ = util::copy_into(etag_out, et);
    }
    Ok(st)
}

/// 删除事件。
pub fn delete_event(dav: &mut DavClient<'_>, path: &[u8], resp: &mut [u8], req: &mut [u8]) -> Result<u16, HttpErr> {
    dav.delete(path, resp, req)
}

/// 从响应头取 ETag（"ETag: \"x\""）。
pub fn http_etag(resp: &[u8]) -> &[u8] {
    // 响应缓冲中定位头部区
    let head = match util::find_sub(resp, b"\r\n\r\n") {
        Some(i) => &resp[..i],
        None => resp,
    };
    match http::header_get(head, b"ETag") {
        Some(v) => v,
        None => &[],
    }
}

/// 发现 + 建立日历集合列表：
/// 1) current-user-principal 2) calendar-home-set 3) 列出日历集合。
/// 成功发现集合返回数量（填入 out）。
pub fn discover_calendars(
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
    let hl = dav.discover_home(principal, b"calendar-home-set", &mut home, resp, req, xml)?;
    let home = if hl > 0 { &home[..hl] } else { base_path };
    Ok(dav.list_collections(home, COLL_CALENDAR, out, resp, req, xml))
}

use crate::sync::http;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_builder() {
        let mut d = SyncDelta::new();
        d.changed_len[0] = util::copy_into(&mut d.changed[0], b"/dav/cal/a.ics");
        d.n_changed = 1;
        assert_eq!(&d.changed[0][..d.changed_len[0]], b"/dav/cal/a.ics");
    }
}
