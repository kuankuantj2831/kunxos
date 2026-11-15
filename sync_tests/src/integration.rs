//! 集成测试：内存 mock CalDAV/CardDAV 服务器 + 同步引擎端到端流程。
//!
//! 覆盖：
//!   1. 发现流程（principal → home-set → 集合列表）
//!   2. 本地创建 → 推送 → 服务器可见
//!   3. 服务器侧修改 → 增量拉取（sync-collection + multiget）
//!   4. 删除传播（双向）
//!   5. 冲突：If-Match 412 → 标记冲突，三种策略行为
//!   6. 联系人（CardDAV）流程
//!   7. 离线：网络不可达时队列保留，恢复后自动重试

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::host::{HostFs, MemFs};
use crate::sync::accounts::MetaStore;
use crate::sync::fs::Fs;
use crate::sync::http::Net;
use crate::sync::model::*;
use crate::sync::store::Store;
use crate::sync::sync::{self, SyncEnv};
use crate::sync::model::SyncStats;
use crate::sync::util;

// ==================== Mock DAV 服务器 ====================

#[derive(Clone)]
struct Res {
    etag: String,
    data: Vec<u8>,
    rev: u64,
}

struct MockDav {
    resources: HashMap<String, Res>,
    rev: u64,
    expected_auth: Option<String>,
}

impl MockDav {
    fn new() -> Self {
        MockDav { resources: HashMap::new(), rev: 0, expected_auth: None }
    }

    fn with_auth(mut self, auth: &str) -> Self {
        self.expected_auth = Some(auth.to_string());
        self
    }

    fn bump(&mut self) -> u64 {
        self.rev += 1;
        self.rev
    }

    /// 基于内容生成 ETag（内容变化 → ETag 变化，模拟真实服务器）。
    fn etag_of(data: &[u8]) -> String {
        format!("\"{:x}\"", util::fnv1a(data))
    }

    fn put(&mut self, path: &str, data: &[u8], if_match: &str, if_none_match: &str) -> (u16, String) {
        let exists = self.resources.contains_key(path);
        let cur = self.resources.get(path).map(|r| r.etag.clone());
        if if_none_match == "*" && exists {
            return (412, String::new());
        }
        if !if_match.is_empty() {
            let ok = cur.as_deref().map(|c| c == if_match).unwrap_or(false);
            if !ok {
                return (412, String::new());
            }
        }
        let etag = Self::etag_of(data);
        let rev = self.bump();
        self.resources.insert(path.to_string(), Res { etag: etag.clone(), data: data.to_vec(), rev });
        let status = if exists { 204 } else { 201 };
        (status, etag)
    }

    fn delete(&mut self, path: &str) -> u16 {
        if self.resources.remove(path).is_some() {
            self.bump();
            204
        } else {
            404
        }
    }
}

/// 简单 HTTP 请求解析。
struct Req {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(sock: &mut TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    // 先读头部
    let head_end;
    loop {
        let n = sock.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = find_sub(&buf, b"\r\n\r\n") {
            head_end = i + 4;
            break;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some(ci) = l.find(':') {
            let k = l[..ci].trim().to_ascii_lowercase();
            let v = l[ci + 1..].trim().to_string();
            headers.insert(k, v);
        }
    }
    let cl: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    while buf.len() < head_end + cl {
        let n = sock.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[head_end..head_end + cl].to_vec();
    Some(Req { method, path, headers, body })
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn respond(sock: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut resp = format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n", status, body.len());
    for (k, v) in headers {
        resp.push_str(&format!("{}: {}\r\n", k, v));
    }
    resp.push_str("\r\n");
    let _ = sock.write_all(resp.as_bytes());
    let _ = sock.write_all(body);
}

const NS: &str = "xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\"";

fn prop_response(path: &str, props: &[(&str, &str)]) -> String {
    let mut body = format!("<d:multistatus {NS}><d:response><d:href>{}</d:href><d:propstat><d:prop>", path);
    for (k, v) in props {
        body.push_str(&format!("<d:{}>{}</d:{}>", k, v, k));
    }
    body.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>");
    body
}

fn handle_request(server: &mut MockDav, req: &Req) -> (u16, Vec<u8>, String) {
    // 认证检查
    if let Some(exp) = &server.expected_auth {
        let got = req.headers.get("authorization").cloned().unwrap_or_default();
        if got != *exp {
            return (401, b"<error>auth</error>".to_vec(), String::new());
        }
    }
    let path = req.path.clone();
    match req.method.as_str() {
        "PROPFIND" => handle_propfind(server, &path, req),
        "REPORT" => handle_report(server, &path, &req.body),
        "PUT" => {
            let if_match = req.headers.get("if-match").cloned().unwrap_or_default();
            let if_none = req.headers.get("if-none-match").cloned().unwrap_or_default();
            let (st, etag) = server.put(&path, &req.body, &if_match, &if_none);
            let hdr = if !etag.is_empty() { format!("ETag: {}\r\n", etag) } else { String::new() };
            (st, Vec::new(), hdr)
        }
        "DELETE" => (server.delete(&path), Vec::new(), String::new()),
        _ => (405, Vec::new(), String::new()),
    }
}

fn handle_propfind(server: &MockDav, path: &str, req: &Req) -> (u16, Vec<u8>, String) {
    let depth = req.headers.get("depth").cloned().unwrap_or_else(|| "0".into());
    let body_str = String::from_utf8_lossy(&req.body).to_string();
    let want = |p: &str| body_str.contains(p);

    if path == "/" && want("current-user-principal") {
        return (207, prop_response("/", &[("current-user-principal", "<d:href>/principals/me/</d:href>")]).into_bytes(), String::new());
    }
    if path == "/principals/me/" {
        let mut props = vec![];
        if want("calendar-home-set") {
            props.push(("calendar-home-set", "<d:href>/dav/calendars/</d:href>"));
        }
        if want("addressbook-home-set") {
            props.push(("addressbook-home-set", "<d:href>/dav/addressbooks/</d:href>"));
        }
        return (207, prop_response(path, &props).into_bytes(), String::new());
    }
    if path == "/dav/calendars/" && depth == "1" {
        let mut body = format!("<d:multistatus {NS}>");
        body.push_str(&format!(
            "<d:response><d:href>/dav/calendars/cal1/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Calendar 1</d:displayname></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
        ));
        body.push_str("</d:multistatus>");
        return (207, body.into_bytes(), String::new());
    }
    if path == "/dav/addressbooks/" && depth == "1" {
        let body = format!(
            "<d:multistatus {NS}><d:response><d:href>/dav/addressbooks/book1/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><card:addressbook/></d:resourcetype><d:displayname>Book 1</d:displayname></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"
        );
        return (207, body.into_bytes(), String::new());
    }
    if path == "/dav/calendars/cal1/" && want("getctag") {
        return (207, prop_response(path, &[("displayname", "Calendar 1"), ("getctag", "\"ctag-1\""), ("sync-token", format!("tok-{}", server.rev).as_str())]).into_bytes(), String::new());
    }
    if path == "/dav/addressbooks/book1/" {
        return (207, prop_response(path, &[("displayname", "Book 1"), ("getctag", "\"ctag-b1\"")]).into_bytes(), String::new());
    }
    (207, format!("<d:multistatus {NS}></d:multistatus>").into_bytes(), String::new())
}

fn handle_report(server: &MockDav, path: &str, body: &[u8]) -> (u16, Vec<u8>, String) {
    let body_str = String::from_utf8_lossy(body).to_string();
    // 取 sync-token（若存在）
    let token = extract_tag(&body_str, "sync-token");
    let since: u64 = token
        .map(|t| t.strip_prefix("tok-").and_then(|v| v.parse().ok()).unwrap_or(0))
        .unwrap_or(0);
    let is_cal = path.starts_with("/dav/calendars/");
    let _is_card = path.starts_with("/dav/addressbooks/");
    let is_multiget = body_str.contains("multiget");
    let is_query = body_str.contains("calendar-query") || body_str.contains("addressbook-query");

    if is_multiget {
        // 解析 <d:href> 列表
        let mut body_out = format!("<d:multistatus {NS}>");
        for href in extract_all_tags(body_str.as_bytes(), "href") {
            if let Some(r) = server.resources.get(href.as_str()) {
                let data_tag = if is_cal { "calendar-data" } else { "address-data" };
                body_out.push_str(&format!(
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag><c:{data_tag}>{}</c:{data_tag}></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    href,
                    r.etag,
                    String::from_utf8_lossy(&r.data)
                ));
            } else {
                body_out.push_str(&format!("<d:response><d:href>{}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>", href));
            }
        }
        body_out.push_str("</d:multistatus>");
        return (207, body_out.into_bytes(), String::new());
    }

    if is_query {
        // 全量返回（忽略时间范围）
        let mut body_out = format!("<d:multistatus {NS}>");
        for (p, r) in &server.resources {
            let in_coll = if is_cal { p.starts_with("/dav/calendars/cal1/") } else { p.starts_with("/dav/addressbooks/book1/") };
            if in_coll {
                body_out.push_str(&format!(
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    p, r.etag
                ));
            }
        }
        body_out.push_str("</d:multistatus>");
        return (207, body_out.into_bytes(), String::new());
    }

    // sync-collection
    let mut body_out = format!("<d:multistatus {NS}>");
    for (p, r) in &server.resources {
        let in_coll = if is_cal { p.starts_with("/dav/calendars/cal1/") } else { p.starts_with("/dav/addressbooks/book1/") };
        if in_coll && r.rev > since {
            body_out.push_str(&format!(
                "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                p, r.etag
            ));
        }
    }
    // 服务器无删除记录表：删除通过不存在于 resources 表达（增量删除未模拟）
    body_out.push_str(&format!("<d:sync-token>tok-{}</d:sync-token>", server.rev));
    body_out.push_str("</d:multistatus>");
    (207, body_out.into_bytes(), String::new())
}

fn extract_tag(s: &str, tag: &str) -> Option<String> {
    let open = format!("<d:{}>", tag);
    let close = format!("</d:{}>", tag);
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(s[start..end].to_string())
}

fn extract_all_tags(s: &[u8], tag: &str) -> Vec<String> {
    let s = String::from_utf8_lossy(s).to_string();
    let open = format!("<d:{}>", tag);
    let close = format!("</d:{}>", tag);
    let mut out = vec![];
    let mut start = 0;
    while let Some(a) = s[start..].find(&open) {
        let a = start + a + open.len();
        if let Some(b) = s[a..].find(&close) {
            out.push(s[a..a + b].to_string());
            start = a + b + close.len();
        } else {
            break;
        }
    }
    out
}

fn run_server(server: Arc<Mutex<MockDav>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let srv = server.clone();
            std::thread::spawn(move || {
                while let Some(req) = read_request(&mut sock) {
                    let (st, body, extra) = {
                        let mut s = srv.lock().unwrap();
                        handle_request(&mut s, &req)
                    };
                    let status_line = match st {
                        200 => "200 OK",
                        201 => "201 Created",
                        204 => "204 No Content",
                        207 => "207 Multi-Status",
                        401 => "401 Unauthorized",
                        404 => "404 Not Found",
                        405 => "405 Method Not Allowed",
                        412 => "412 Precondition Failed",
                        _ => "500 Internal Server Error",
                    };
                    let headers: Vec<(&str, &str)> = if extra.is_empty() {
                        vec![]
                    } else {
                        let s: &'static str = Box::leak(extra.into_boxed_str());
                        vec![("ETag", s)]
                    };
                    respond(&mut sock, status_line, &headers, &body);
                }
            });
        }
    });
    port
}

// ==================== 测试脚手架 ====================

struct TestCtx {
    fs: MemFs,
    server: Arc<Mutex<MockDav>>,
    port: u16,
}

fn setup(server: MockDav) -> TestCtx {
    let server = Arc::new(Mutex::new(server));
    let port = run_server(server.clone());
    TestCtx { fs: MemFs::new(), server, port }
}

fn make_env<'a>(fs: &'a mut MemFs, resolve: u8) -> SyncEnv<'a> {
    let net: &'a mut dyn Net = Box::leak(Box::new(crate::host::HostNet));
    SyncEnv {
        fs,
        net,
        dev_key: 0x12345678,
        now_us: 1_700_000_000_000_000,
        resolve,
    }
}

fn account_for(port: u16) -> Account {
    let mut a = Account::new();
    a.id = 1;
    a.name_len = util::copy_into(&mut a.name, b"Mock");
    a.acct_type = ACCT_GENERIC;
    let mut srv = [0u8; 32];
    let sn = util::push_str(&mut srv, 0, b"127.0.0.1:");
    let sn = util::push_dec(&mut srv, sn, port as u64);
    a.server_len = util::copy_into(&mut a.server, &srv[..sn]);
    a.principal_len = util::copy_into(&mut a.principal, b"/");
    a.username_len = util::copy_into(&mut a.username, b"user");
    a.auth = AUTH_BASIC;
    a.enabled = true;
    a
}

fn calendar_coll() -> Collection {
    let mut c = Collection::new();
    c.id = 1;
    c.acct = 1;
    c.kind = COLL_CALENDAR;
    c.path_len = util::copy_into(&mut c.path, b"/dav/calendars/cal1/");
    c
}

fn addrbook_coll() -> Collection {
    let mut c = Collection::new();
    c.id = 1;
    c.acct = 1;
    c.kind = COLL_ADDRBOOK;
    c.path_len = util::copy_into(&mut c.path, b"/dav/addressbooks/book1/");
    c
}

fn meta_with(account: Account, coll: Collection) -> MetaStore {
    let mut m = MetaStore::new(b"/data/sync/meta.db");
    m.load(&mut MemFs::new());
    m.upsert_account(account);
    m.upsert_collection(coll);
    m
}

fn store_new<'a>(fs: &mut MemFs, blob: &'a mut [u8], kind: u8) -> Store<'a> {
    let (idx, blb) = if kind == COLL_CALENDAR {
        (b"/data/sync/events.idx".as_slice(), b"/data/sync/events.blob".as_slice())
    } else {
        (b"/data/sync/contacts.idx".as_slice(), b"/data/sync/contacts.blob".as_slice())
    };
    let mut s = Store::new(blob, idx, blb);
    s.load(fs);
    s
}

// ==================== 测试 ====================

#[test]
fn discovery_flow() {
    let mut ctx = setup(MockDav::new());
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    let mut net = crate::host::HostNet;
    let acct = account_for(ctx.port);
    let mut dav = crate::sync::dav::DavClient::new(&mut net, &acct.server[..acct.server_len], b"");
    // principal
    let mut principal = [0u8; PRINCIPAL_LEN];
    let pl = dav.discover_principal(b"/", &mut principal, &mut resp, &mut req, &mut xml).unwrap();
    assert_eq!(&principal[..pl], b"/principals/me/");
    // calendar-home-set
    let mut home = [0u8; PRINCIPAL_LEN];
    let hl = dav.discover_home(b"/principals/me/", b"calendar-home-set", &mut home, &mut resp, &mut req, &mut xml).unwrap();
    assert_eq!(&home[..hl], b"/dav/calendars/");
    // 集合列表
    let mut colls = [Collection::new(); 8];
    let n = dav.list_collections(&home[..hl], COLL_CALENDAR, &mut colls, &mut resp, &mut req, &mut xml);
    eprintln!("DISCOVERY: n={} resp={:?}", n, String::from_utf8_lossy(&xml[..xml.len().min(400)]));
    assert_eq!(n, 1);
    assert_eq!(&colls[0].path[..colls[0].path_len], b"/dav/calendars/cal1/");
}

#[test]
fn push_create_event() {
    let mut ctx = setup(MockDav::new());
    let mut meta = meta_with(account_for(ctx.port), calendar_coll());
    let mut blob = [0u8; 65536];
    let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
    // 本地创建事件
    let mut ev = Event::new();
    ev.summary_len = util::copy_into(&mut ev.summary, b"Sync test");
    ev.dtstart = DateTime::new(2025, 9, 1, 10, 0, 0, 0, true);
    ev.dtend = DateTime::new(2025, 9, 1, 11, 0, 0, 0, true);
    let mut env = make_env(&mut ctx.fs, RESOLVE_MANUAL);
    let ok = sync::create_event_local(&mut env, &mut store, 1, b"/dav/calendars/cal1/", &mut ev, 42);
    assert!(ok);
    assert!(store.query_dirty(1, &mut [0usize; 8]) > 0);
    // 同步推送
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    let mut stats = SyncStats::new();
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats).unwrap();
    assert!(stats.pushed >= 1, "pushed={}", stats.pushed);
    assert!(stats.ok);
    // 服务器已存
    let srv = ctx.server.lock().unwrap();
    let keys: Vec<&String> = srv.resources.keys().collect();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].starts_with("/dav/calendars/cal1/") && keys[0].ends_with(".ics"));
    assert!(srv.resources.values().next().unwrap().data.windows(6).any(|w| w == b"Sync t"));
}

#[test]
fn deletion_survives_restart_and_is_acknowledged_once() {
    let mut ctx = setup(MockDav::new());
    let mut meta = meta_with(account_for(ctx.port), calendar_coll());
    let mut blob = [0u8; 65536];
    let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
    let mut ev = Event::new();
    ev.summary_len = util::copy_into(&mut ev.summary, b"Delete after restart");
    ev.dtstart = DateTime::new(2025, 9, 1, 10, 0, 0, 0, true);
    ev.dtend = DateTime::new(2025, 9, 1, 11, 0, 0, 0, true);
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    {
        let mut env = make_env(&mut ctx.fs, RESOLVE_MANUAL);
        assert!(sync::create_event_local(&mut env, &mut store, 1, b"/dav/calendars/cal1/", &mut ev, 42));
        sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR,
            &mut resp, &mut req, &mut xml, &mut SyncStats::new()).unwrap();
    }
    assert_eq!(ctx.server.lock().unwrap().resources.len(), 1);
    store.remove(0);
    assert!(store.save(&mut ctx.fs));
    let mut reloaded_blob = [0u8; 65536];
    let mut reloaded = store_new(&mut ctx.fs, &mut reloaded_blob, COLL_CALENDAR);
    assert_eq!(reloaded.query_dirty(1, &mut [0usize; 8]), 1);
    let mut stats = SyncStats::new();
    {
        let mut env = make_env(&mut ctx.fs, RESOLVE_MANUAL);
        sync::sync_collection(&mut env, &mut meta, 1, &mut reloaded, COLL_CALENDAR,
            &mut resp, &mut req, &mut xml, &mut stats).unwrap();
        assert_eq!(stats.deleted, 1);
        assert_eq!(reloaded.query_dirty(1, &mut [0usize; 8]), 0);
        let mut next = SyncStats::new();
        sync::sync_collection(&mut env, &mut meta, 1, &mut reloaded, COLL_CALENDAR,
            &mut resp, &mut req, &mut xml, &mut next).unwrap();
        assert_eq!(next.deleted, 0);
        assert_eq!(next.pushed, 0);
    }
    assert!(ctx.server.lock().unwrap().resources.is_empty());
    assert_eq!(reloaded.query_coll(1, &mut [0usize; 8]), 0);
    assert!(reloaded.compact(&mut ctx.fs));
    assert_eq!(reloaded.n, 0);
}

#[test]
fn incremental_pull_and_push() {
    let mut ctx = setup(MockDav::new());
    // 服务器预置两个事件
    {
        let mut srv = ctx.server.lock().unwrap();
        let ev1 = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a1\r\nDTSTART:20250901T100000Z\r\nDTEND:20250901T110000Z\r\nSUMMARY:Remote A\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let ev2 = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a2\r\nDTSTART:20250902T100000Z\r\nDTEND:20250902T110000Z\r\nSUMMARY:Remote B\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        srv.put("/dav/calendars/cal1/a1.ics", ev1, "", "").0;
        srv.put("/dav/calendars/cal1/a2.ics", ev2, "", "").0;
    }
    let mut meta = meta_with(account_for(ctx.port), calendar_coll());
    let mut blob = [0u8; 65536];
    let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
    let mut env = make_env(&mut ctx.fs, RESOLVE_SERVER);
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    let mut stats = SyncStats::new();
    // 首次同步：全量拉取
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats).unwrap();
    assert_eq!(stats.pulled, 2);
    assert_eq!(store.n, 2);
    assert!(meta.collection(1).unwrap().sync_token_len > 0);
    // 本地新建 → 推送
    let mut ev = Event::new();
    ev.summary_len = util::copy_into(&mut ev.summary, b"Local C");
    ev.dtstart = DateTime::new(2025, 9, 3, 9, 0, 0, 0, true);
    ev.dtend = DateTime::new(2025, 9, 3, 10, 0, 0, 0, true);
    sync::create_event_local(&mut env, &mut store, 1, b"/dav/calendars/cal1/", &mut ev, 7);
    let mut stats2 = SyncStats::new();
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats2).unwrap();
    assert!(stats2.pushed >= 1);
    assert_eq!(store.query_dirty(1, &mut [0usize; 8]), 0);
    // 服务器新增 → 增量拉取
    {
        let mut srv = ctx.server.lock().unwrap();
        srv.put("/dav/calendars/cal1/a3.ics", b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:a3\r\nDTSTART:20250904T100000Z\r\nDTEND:20250904T110000Z\r\nSUMMARY:Remote D\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", "", "").0;
    }
    let mut stats3 = SyncStats::new();
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats3).unwrap();
    assert_eq!(stats3.pulled, 1);
    assert_eq!(store.n, 4);
}

#[test]
fn conflict_server_and_client_wins() {
    // —— 服务器优先：本地改动被丢弃 ——
    {
        let mut ctx = setup(MockDav::new());
        {
            let mut srv = ctx.server.lock().unwrap();
            srv.put("/dav/calendars/cal1/x.ics", b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20250901T100000Z\r\nSUMMARY:v1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", "", "").0;
        }
        let mut meta = meta_with(account_for(ctx.port), calendar_coll());
        let mut blob = [0u8; 65536];
        let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
        let mut env = make_env(&mut ctx.fs, RESOLVE_SERVER);
        let mut resp = [0u8; 65536];
        let mut req = [0u8; 8192];
        let mut xml = [0u8; 65536];
        let mut stats = SyncStats::new();
        let r = sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats);
        if let Err(e) = &r {
            eprintln!("SYNC_ERR1: {:?} msg={:?}", e, String::from_utf8_lossy(&stats.msg[..stats.msg_len]));
            eprintln!("RESP: {:?}", String::from_utf8_lossy(&resp[..resp.len().min(600)]));
        }
        r.unwrap();
        // 本地修改（置脏）
        let i = store.find(b"/dav/calendars/cal1/x.ics").unwrap();
        let mut rec = store.recs[i];
        rec.flags |= FLAG_DIRTY;
        store.recs[i] = rec;
        // 服务器同时修改（ETag 变化）
        {
            let mut srv = ctx.server.lock().unwrap();
            let cur = srv.resources.get("/dav/calendars/cal1/x.ics").unwrap().etag.clone();
            srv.put("/dav/calendars/cal1/x.ics", b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20250901T100000Z\r\nSUMMARY:v2-server\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", &cur, "").0;
        }
        // 重新同步：push 412 → 冲突（server-wins 丢弃本地）
        let mut stats2 = SyncStats::new();
        let r2 = sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats2);
        if let Err(e) = &r2 {
            eprintln!("SYNC_ERR2: {:?} pushed={} pulled={} conflicts={} errors={} msg={:?}", e, stats2.pushed, stats2.pulled, stats2.conflicts, stats2.errors, String::from_utf8_lossy(&stats2.msg[..stats2.msg_len]));
        }
        r2.unwrap();
        // server-wins：冲突被标记后在 pull 阶段被服务器版本覆盖
        let i = store.find(b"/dav/calendars/cal1/x.ics").unwrap();
        let blob_data = store.blob_of(i).unwrap();
        let txt = String::from_utf8_lossy(blob_data);
        assert!(txt.contains("v2-server"), "expect server version, got {:?}", txt);
        assert!(!store.recs[i].is_conflict());
    }
    // —— 客户端优先：本地覆盖服务器 ——
    {
        let mut ctx = setup(MockDav::new());
        {
            let mut srv = ctx.server.lock().unwrap();
            srv.put("/dav/calendars/cal1/y.ics", b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y\r\nDTSTART:20250901T100000Z\r\nSUMMARY:base\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", "", "").0;
        }
        let mut meta = meta_with(account_for(ctx.port), calendar_coll());
        let mut blob = [0u8; 65536];
        let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
        let mut env = make_env(&mut ctx.fs, RESOLVE_CLIENT);
        let mut resp = [0u8; 65536];
        let mut req = [0u8; 8192];
        let mut xml = [0u8; 65536];
        let mut stats = SyncStats::new();
        sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats).unwrap();
        let i = store.find(b"/dav/calendars/cal1/y.ics").unwrap();
        let mut rec = store.recs[i];
        rec.flags |= FLAG_DIRTY;
        store.recs[i] = rec;
        {
            let mut srv = ctx.server.lock().unwrap();
            let cur = srv.resources.get("/dav/calendars/cal1/y.ics").unwrap().etag.clone();
            srv.put("/dav/calendars/cal1/y.ics", b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:y\r\nDTSTART:20250901T100000Z\r\nSUMMARY:server-edit\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", &cur, "").0;
        }
        let mut stats2 = SyncStats::new();
        sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats2).unwrap();
        // 客户端优先：首次同步只记录远端 etag（两阶段收敛），再同步一次完成覆盖
        let mut stats3 = SyncStats::new();
        sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats3).unwrap();
        let srv = ctx.server.lock().unwrap();
        let data = String::from_utf8_lossy(&srv.resources.get("/dav/calendars/cal1/y.ics").unwrap().data).to_string();
        assert!(data.contains("SUMMARY:base"), "client-wins should keep local (base), got {:?}", data);
        assert!(!data.contains("server-edit"), "server edit must be overwritten, got {:?}", data);
    }
}

#[test]
fn contacts_push_pull() {
    let mut ctx = setup(MockDav::new());
    let mut meta = meta_with(account_for(ctx.port), addrbook_coll());
    let mut blob = [0u8; 65536];
    let mut store = store_new(&mut ctx.fs, &mut blob, COLL_ADDRBOOK);
    let mut env = make_env(&mut ctx.fs, RESOLVE_MANUAL);
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    // 本地创建联系人
    let mut c = Contact::new();
    c.fn_len = util::copy_into(&mut c.fn_, b"Test User");
    c.tels[0].value_len = util::copy_into(&mut c.tels[0].value, b"+8613800000001");
    c.tels[0].kind = TEL_CELL;
    c.tels_len = 1;
    eprintln!("CT: create local");
    sync::create_contact_local(&mut env, &mut store, 1, b"/dav/addressbooks/book1/", &mut c, 3);
    eprintln!("CT: sync1");
    let mut stats = SyncStats::new();
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_ADDRBOOK, &mut resp, &mut req, &mut xml, &mut stats).unwrap();
    assert!(stats.pushed >= 1);
    eprintln!("CT: check server");
    let srv = ctx.server.lock().unwrap();
    assert_eq!(srv.resources.len(), 1);
    drop(srv);
    // 服务器新增联系人 → 拉取
    {
        let mut s = ctx.server.lock().unwrap();
        s.put("/dav/addressbooks/book1/r1.vcf", b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Remote Person\r\nTEL;TYPE=CELL:+15550001\r\nEND:VCARD\r\n", "", "").0;
    }
    eprintln!("CT: sync2");
    let mut stats2 = SyncStats::new();
    sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_ADDRBOOK, &mut resp, &mut req, &mut xml, &mut stats2).unwrap();
    eprintln!("CT: sync2 done pulled={}", stats2.pulled);
    assert_eq!(stats2.pulled, 1);
    assert_eq!(store.n, 2);
    let i = store.find(b"/dav/addressbooks/book1/r1.vcf").unwrap();
    let mut ev = Contact::new();
    let mut buf = [0u8; 4096];
    let bl = util::copy_into(&mut buf, store.blob_of(i).unwrap());
    assert!(crate::sync::vcard::parse_vcard(&mut buf, bl, &mut ev));
    assert_eq!(&ev.fn_[..ev.fn_len], b"Remote Person");
}

#[test]
fn offline_queue_retained() {
    // 网络不可达：同步失败但队列保留；修复后（换端口）自动恢复
    let mut ctx = setup(MockDav::new());
    // 先指向一个无监听端口（离线）
    let mut meta = MetaStore::new(b"/data/sync/meta.db");
    meta.load(&mut MemFs::new());
    let mut acct = Account::new();
    acct.id = 1;
    acct.acct_type = ACCT_GENERIC;
    acct.enabled = true;
    acct.server_len = util::copy_into(&mut acct.server, b"127.0.0.1:1"); // 无服务
    meta.upsert_account(acct);
    meta.upsert_collection(calendar_coll());
    let mut blob = [0u8; 65536];
    let mut store = store_new(&mut ctx.fs, &mut blob, COLL_CALENDAR);
    let mut env = make_env(&mut ctx.fs, RESOLVE_MANUAL);
    let mut ev = Event::new();
    ev.summary_len = util::copy_into(&mut ev.summary, b"Offline event");
    ev.dtstart = DateTime::new(2025, 9, 1, 10, 0, 0, 0, true);
    ev.dtend = DateTime::new(2025, 9, 1, 11, 0, 0, 0, true);
    sync::create_event_local(&mut env, &mut store, 1, b"/dav/calendars/cal1/", &mut ev, 1);
    let mut resp = [0u8; 65536];
    let mut req = [0u8; 8192];
    let mut xml = [0u8; 65536];
    let mut stats = SyncStats::new();
    let r = sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats);
    assert!(r.is_err(), "offline sync must fail");
    assert!(!stats.ok);
    assert!(store.query_dirty(1, &mut [0usize; 8]) > 0, "queue must be retained");
    // 恢复：把账户服务器改回可用端口（模拟网络恢复）
    let mut acct2 = meta.account(1).unwrap().clone();
    let mut srv = [0u8; 32];
    let sn = util::push_str(&mut srv, 0, b"127.0.0.1:");
    let sn = util::push_dec(&mut srv, sn, ctx.port as u64);
    acct2.server_len = util::copy_into(&mut acct2.server, &srv[..sn]);
    meta.upsert_account(acct2);
    let mut stats2 = SyncStats::new();
    let r2 = sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats2);
    assert!(r2.is_ok(), "after recovery sync succeeds: {:?}", r2);
    assert_eq!(store.query_dirty(1, &mut [0usize; 8]), 0, "queue drained");
}

#[test]
fn local_only_no_server() {
    // 本地账户（ACCT_LOCAL）：不联网，直接成功
    let mut fs = MemFs::new();
    let mut meta = MetaStore::new(b"/data/sync/meta.db");
    meta.load(&mut fs);
    let mut acct = Account::new();
    acct.id = 1;
    acct.acct_type = ACCT_LOCAL;
    acct.enabled = true;
    meta.upsert_account(acct);
    let mut coll = Collection::new();
    coll.id = 1;
    coll.acct = 1;
    coll.kind = COLL_CALENDAR;
    meta.upsert_collection(coll);
    let mut blob = [0u8; 8192];
    let mut store = Store::new(&mut blob, b"i", b"b");
    store.load(&mut fs);
    let mut net = crate::host::HostNet;
    let mut env = SyncEnv { fs: &mut fs, net: &mut net, dev_key: 0, now_us: 0, resolve: RESOLVE_MANUAL };
    let mut resp = [0u8; 4096];
    let mut req = [0u8; 4096];
    let mut xml = [0u8; 4096];
    let mut stats = SyncStats::new();
    let r = sync::sync_collection(&mut env, &mut meta, 1, &mut store, COLL_CALENDAR, &mut resp, &mut req, &mut xml, &mut stats);
    assert!(r.is_ok());
    assert_eq!(&stats.msg[..stats.msg_len], b"local");
}

#[test]
fn host_fs_roundtrip() {
    let dir = std::env::temp_dir().join(format!("kunxos-sync-test-{}", std::process::id()));
    let mut fs = HostFs::new(dir.clone());
    assert!(fs.write_file(b"/data/sync/test.txt", b"hello"));
    let mut buf = [0u8; 64];
    let n = fs.read_file(b"/data/sync/test.txt", &mut buf);
    assert_eq!(&buf[..n as usize], b"hello");
    assert!(fs.file_exists(b"/data/sync/test.txt"));
    assert!(fs.delete_file(b"/data/sync/test.txt"));
    std::fs::remove_dir_all(&dir).ok();
}
