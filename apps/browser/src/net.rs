//! 网络层：URL 获取（kunxos:// 内置页 / file:// 文件 / http:// HTTP/1.1）。
//!
//! - HTTP/1.1 客户端：GET + Host 头 + Connection: close，重定向跟随
//!   （301/302/303/307/308，≤5 跳），Content-Length 或 EOF 定界
//! - 内存缓存：成功 200 响应按 URL 缓存（≤8 条、单条 ≤256KB）
//! - HTTPS 预留：报错提示 TLS 未实现
//! - 安全基线：无 Cookie、无插件、无脚本执行；图片仅同源加载（shell 侧执行）

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::image;
use crate::kit::*;
use crate::pages;
use crate::url::{Scheme, Url};

const MAX_BODY: usize = 256 * 1024;
const MAX_REDIRECTS: u32 = 5;
const CACHE_CAP: usize = 8;

pub enum FetchResult {
    Ok { bytes: Vec<u8>, final_url: String },
    Err { msg: String },
}

// ==================== 内存缓存 ====================

struct CacheEntry {
    key: String,
    body: Vec<u8>,
}

static mut CACHE: Vec<CacheEntry> = Vec::new();

fn cache_get(key: &str) -> Option<Vec<u8>> {
    unsafe {
        for e in CACHE.iter() {
            if e.key == key {
                return Some(e.body.clone());
            }
        }
    }
    None
}

fn cache_put(key: &str, body: Vec<u8>) {
    if body.len() > MAX_BODY {
        return;
    }
    unsafe {
        if let Some(e) = CACHE.iter_mut().find(|e| e.key == key) {
            e.body = body;
            return;
        }
        if CACHE.len() >= CACHE_CAP {
            CACHE.remove(0);
        }
        CACHE.push(CacheEntry { key: key.to_string(), body });
    }
}

// ==================== 获取入口 ====================

/// 获取 URL 内容。`img_mode`：图片请求（body 按图片处理，不缓存 HTML）。
pub fn fetch(url: &Url) -> FetchResult {
    match url.scheme {
        Scheme::Kunxos => match pages::load(&url.path) {
            Some(html) => FetchResult::Ok { bytes: html.as_bytes().to_vec(), final_url: url.display() },
            None => FetchResult::Err { msg: format!("未知内置页面：{}", url.path) },
        },
        Scheme::File => match read_file(&url.path) {
            Some(bytes) => FetchResult::Ok { bytes, final_url: url.display() },
            None => FetchResult::Err { msg: format!("无法读取文件：{}", url.path) },
        },
        Scheme::Http => fetch_http(url, 0),
        Scheme::Https => FetchResult::Err { msg: "HTTPS 暂不支持（TLS 预留）".into() },
        Scheme::Unknown => FetchResult::Err { msg: "无法识别的地址".into() },
    }
}

/// 获取内置渐变图片（kunxos://gfx/*）。
pub fn fetch_builtin_image(path: &str) -> Option<Vec<u8>> {
    if let Some(rest) = path.strip_prefix("gfx/") {
        let (w, h) = match rest {
            "banner" => (480, 160),
            "square" => (120, 120),
            _ => (240, 120),
        };
        return Some(image::gradient(w, h));
    }
    None
}

/// 获取图片字节（存储格式）。失败返回 None。
pub fn fetch_image_bytes(url: &Url) -> Option<Vec<u8>> {
    if url.scheme == Scheme::Kunxos {
        return fetch_builtin_image(&url.path);
    }
    let key = format!("img:{}", url.display());
    if let Some(b) = cache_get(&key) {
        return Some(b);
    }
    let bytes = match fetch(url) {
        FetchResult::Ok { bytes, .. } => bytes,
        FetchResult::Err { .. } => return None,
    };
    let store = if image::is_ppm(&bytes) {
        image::decode_ppm(&bytes)
    } else {
        None
    };
    if let Some(s) = store {
        cache_put(&key, s.clone());
        Some(s)
    } else {
        None
    }
}

// ==================== file:// ====================

fn read_file(path: &str) -> Option<Vec<u8>> {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
            if (n as isize) <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > MAX_BODY {
                break;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}

// ==================== HTTP/1.1 ====================

fn fetch_http(url: &Url, depth: u32) -> FetchResult {
    if depth > MAX_REDIRECTS {
        return FetchResult::Err { msg: "重定向次数过多".into() };
    }
    if let Some(b) = cache_get(&url.display()) {
        return FetchResult::Ok { bytes: b, final_url: url.display() };
    }
    let resp = http_request(url);
    match resp {
        Ok(Resp { status: 200 | 201 | 204, body, .. }) => {
            cache_put(&url.display(), body.clone());
            FetchResult::Ok { bytes: body, final_url: url.display() }
        }
        Ok(Resp { status, location, body }) => {
            if (300..400).contains(&status) {
                if let Some(loc) = location {
                    if let Some(next) = crate::url::resolve(url, &loc) {
                        // 跨协议重定向：https → 报错
                        if next.scheme == Scheme::Https {
                            return FetchResult::Err { msg: "重定向到 HTTPS（TLS 预留）".into() };
                        }
                        return fetch_http(&next, depth + 1);
                    }
                }
                FetchResult::Err { msg: format!("重定向缺少 Location（{}）", status) }
            } else if status == 404 {
                let _ = body;
                FetchResult::Err { msg: format!("404 Not Found：{}", url.path) }
            } else if (400..500).contains(&status) {
                let _ = body;
                FetchResult::Err { msg: format!("HTTP {} 客户端错误", status) }
            } else if status >= 500 {
                let _ = body;
                FetchResult::Err { msg: format!("HTTP {} 服务器错误", status) }
            } else {
                let _ = body;
                FetchResult::Err { msg: format!("HTTP {} 响应异常", status) }
            }
        }
        Err(e) => FetchResult::Err { msg: e },
    }
}

struct Resp {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

fn http_request(url: &Url) -> Result<Resp, String> {
    // 解析主机
    let ip = match ip_parse(url.host.as_bytes()) {
        Some(ip) => ip,
        None => unsafe { dns_resolve(url.host.as_bytes()) },
    };
    if ip == 0 {
        return Err(format!("域名解析失败：{}", url.host));
    }
    // 组装请求（≤2048B）
    let mut req = Vec::with_capacity(256);
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(url.request_target().as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\n");
    req.extend_from_slice(b"Host: ");
    req.extend_from_slice(url.host_port().as_bytes());
    req.extend_from_slice(b"\r\nUser-Agent: KunxOS-Browser/1.0\r\n");
    req.extend_from_slice(b"Accept: text/html,application/xhtml+xml,*/*\r\n");
    req.extend_from_slice(b"Connection: close\r\n\r\n");

    unsafe {
        let fd = sock_open();
        if fd < 0 {
            return Err("socket 创建失败".into());
        }
        if sock_connect(fd, ip, url.port) < 0 {
            sock_close(fd);
            return Err(format!("连接失败：{}:{}", url.host, url.port));
        }
        let mut sent = 0usize;
        while sent < req.len() {
            let n = sock_send(fd, &req[sent..]);
            if n <= 0 {
                sock_close(fd);
                return Err("发送请求失败".into());
            }
            sent += n as usize;
        }
        // 读响应头
        let mut head: Vec<u8> = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        let mut header_end: Option<usize> = None;
        while head.len() < 8192 {
            let n = sock_recv(fd, &mut buf);
            if n < 0 {
                sock_close(fd);
                return Err("接收响应失败".into());
            }
            if n == 0 {
                break;
            }
            let before = head.len();
            head.extend_from_slice(&buf[..n as usize]);
            // 从拼接缝前 3 字节开始搜索：\r\n\r\n / \n\n 可能被 TCP 分段
            // 切成两半（只搜新增数据会永远匹配不到 → 正常页面报
            // 「响应头不完整」）。
            let scan_from = before.saturating_sub(3);
            if let Some(pos) = find_bytes(&head[scan_from..], b"\r\n\r\n") {
                let he = scan_from + pos;
                header_end = Some(he);
                break;
            }
            if let Some(pos) = find_bytes(&head[scan_from..], b"\n\n") {
                let he = scan_from + pos;
                header_end = Some(he);
                break;
            }
        }
        let Some(he) = header_end else {
            sock_close(fd);
            return Err("响应头不完整".into());
        };
        // 终止符长度：\r\n\r\n=4，\n\n=2（\n\n 命中时多吞 2 字节 body）。
        let sep = if head[he..].starts_with(b"\r\n\r\n") { 4usize } else { 2usize };
        // 解析状态行
        let head_str = core::str::from_utf8(&head[..he]).unwrap_or("");
        let status = parse_status(head_str);
        let content_length = header_value(&head[..he], "content-length")
            .and_then(|v| v.trim().parse::<usize>().ok());
        let location = header_value(&head[..he], "location").map(|v| v.trim().to_string());
        // chunked 传输编码：HTTP/1.1 服务器在无 Content-Length 时普遍使用，
        // 不解码会把分块元数据当页面内容呈现 —— 明确报错优于乱码。
        if header_value(&head[..he], "transfer-encoding")
            .map(|v| v.trim().eq_ignore_ascii_case("chunked"))
            .unwrap_or(false)
        {
            sock_close(fd);
            return Err("暂不支持 chunked 传输编码".into());
        }
        // 读响应体
        let mut body: Vec<u8> = Vec::new();
        let body_start = he + sep;
        let remaining = head.len().saturating_sub(body_start);
        if remaining > 0 {
            body.extend_from_slice(&head[body_start..]);
        }
        let limit = content_length.unwrap_or(MAX_BODY).min(MAX_BODY);
        while body.len() < limit {
            let want = (limit - body.len()).min(4096);
            let n = sock_recv(fd, &mut buf[..want]);
            if n < 0 {
                break;
            }
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n as usize]);
            if content_length.is_none() && body.len() >= MAX_BODY {
                break;
            }
        }
        sock_close(fd);
        Ok(Resp { status, location, body })
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

/// 解析状态行 "HTTP/1.x NNN ..."。
fn parse_status(head: &str) -> u16 {
    let mut it = head.split_whitespace();
    let _ = it.next(); // HTTP/1.x
    match it.next() {
        Some(code) => code.parse::<u16>().unwrap_or(0),
        None => 0,
    }
}

/// 小写不敏感取头字段。
fn header_value(head: &[u8], name: &str) -> Option<String> {
    let text = core::str::from_utf8(head).unwrap_or("");
    for line in text.split("\r\n").skip(1) {
        if let Some(ci) = line.find(':') {
            let k = line[..ci].trim().to_ascii_lowercase();
            if k == name {
                return Some(line[ci + 1..].trim().to_string());
            }
        }
    }
    None
}
