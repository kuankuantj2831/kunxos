//! 网络层：URL 获取（kunxos:// 内置页 / file:// 文件 / http:// HTTP/1.1 / https:// TLS 1.2）。
//!
//! - HTTP/1.1 客户端：GET + Host 头 + Connection: close，重定向跟随
//!   （301/302/303/307/308，≤5 跳），Content-Length / EOF 定界，
//!   chunked 传输编码解码
//! - 内存缓存：成功 200 响应按 URL 缓存（≤8 条、单条 ≤256KB）
//! - HTTPS：TLS 1.2 客户端（ECDHE_RSA_AES128_GCM_SHA256），证书链验证、
//!   SNI、主机名校验；证书根库见 tls/roots.rs
//! - 安全基线：无 Cookie、无插件、无脚本执行；图片仅同源加载（shell 侧执行）

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::image;
use crate::tls;
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
        Scheme::Http | Scheme::Https => fetch_http(url, 0),
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
        // https：先做 TLS 握手；失败时统一转为可读错误
        let mut wire = if url.scheme == Scheme::Https {
            match Wire::tls(fd, &url.host) {
                Ok(w) => w,
                Err(e) => {
                    sock_close(fd);
                    return Err(format!("TLS 握手失败：{}", e));
                }
            }
        } else {
            Wire::plain(fd)
        };
        if wire.send_all(&req).is_err() {
            wire.close();
            return Err("发送请求失败".into());
        }
        // 读响应头
        let mut head: Vec<u8> = Vec::with_capacity(4096);
        let mut buf = [0u8; 4096];
        let mut header_end: Option<usize> = None;
        while head.len() < 8192 {
            let n = wire.recv(&mut buf);
            if n < 0 {
                wire.close();
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
            wire.close();
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
        let chunked = header_value(&head[..he], "transfer-encoding")
            .map(|v| v.trim().eq_ignore_ascii_case("chunked"))
            .unwrap_or(false);
        // 读响应体
        let mut body: Vec<u8> = Vec::new();
        let body_start = he + sep;
        let remaining = head.len().saturating_sub(body_start);
        if remaining > 0 {
            body.extend_from_slice(&head[body_start..]);
        }
        if chunked {
            // chunked 传输编码：分块大小为十六进制行前缀。Connection: close
            // 下以 EOF 定界 —— 先把原始分块流读全，再剥掉分块元数据。
            // 原始流上限 = 解码上限 + 每块 ~6B 元数据的余量。
            let raw_limit = (MAX_BODY + 64 * 1024).min(512 * 1024);
            let mut buf = [0u8; 4096];
            while body.len() < raw_limit {
                let n = wire.recv(&mut buf);
                if n <= 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n as usize]);
            }
            wire.close();
            let raw = core::mem::take(&mut body);
            return match decode_chunked(&raw) {
                Some(decoded) => {
                    let mut decoded = decoded;
                    decoded.truncate(MAX_BODY);
                    Ok(Resp { status, location, body: decoded })
                }
                None => Err("chunked 分块格式错误".into()),
            };
        }
        let limit = content_length.unwrap_or(MAX_BODY).min(MAX_BODY);
        while body.len() < limit {
            let want = (limit - body.len()).min(4096);
            let n = wire.recv(&mut buf[..want]);
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
        wire.close();
        Ok(Resp { status, location, body })
    }
}

// ==================== 连接抽象（明文 / TLS） ====================

/// HTTP/HTTPS 统一读写接口：明文直用 socket，HTTPS 套一层 TLS 1.2。
enum Wire {
    Plain(isize),
    /// TLS 连接 + 底层 socket fd：`TlsConn::close` 只发 close_notify 警报，
    /// 不关 fd —— fd 必须单独保存并在 close 时释放（内核 socket 表仅 16
    /// 槽，每个 HTTPS 请求泄漏 1 个 fd，十几次后全系统无法再联网）。
    Tls(tls::TlsConn<FdTls>, isize),
}

struct FdTls {
    fd: isize,
}

impl tls::Transport for FdTls {
    fn send_all(&mut self, data: &[u8]) -> Result<(), &'static str> {
        unsafe {
            let mut sent = 0usize;
            while sent < data.len() {
                let n = sock_send(self.fd, &data[sent..]);
                if n <= 0 {
                    return Err("TLS 传输发送失败");
                }
                sent += n as usize;
            }
        }
        Ok(())
    }
    fn recv_some(&mut self, buf: &mut [u8]) -> Result<usize, &'static str> {
        unsafe {
            let n = sock_recv(self.fd, buf);
            if n < 0 {
                return Err("TLS 传输接收失败");
            }
            Ok(n as usize)
        }
    }
}

impl Wire {
    fn plain(fd: isize) -> Wire {
        Wire::Plain(fd)
    }

    fn tls(fd: isize, host: &str) -> Result<Wire, &'static str> {
        let mut rng = SysRng;
        // 墙钟（秒）用于证书有效期校验；RTC 不可用时 now=0 会判定"尚未生效"
        let now = (unsafe { crate::kit::now_us() } / 1_000_000) as i64;
        let conn = tls::TlsConn::connect(FdTls { fd }, host, &mut rng, now)?;
        Ok(Wire::Tls(conn, fd))
    }

    fn send_all(&mut self, data: &[u8]) -> Result<(), &'static str> {
        match self {
            Wire::Plain(fd) => unsafe {
                let mut sent = 0usize;
                while sent < data.len() {
                    let n = sock_send(*fd, &data[sent..]);
                    if n <= 0 {
                        return Err("发送失败");
                    }
                    sent += n as usize;
                }
            },
            Wire::Tls(c, _) => c.send(data)?,
        }
        Ok(())
    }

    /// 返回 <0 = 错误，0 = 对端关闭，>0 = 字节数（与 sock_recv 语义一致）。
    fn recv(&mut self, buf: &mut [u8]) -> isize {
        match self {
            Wire::Plain(fd) => unsafe { sock_recv(*fd, buf) },
            Wire::Tls(c, _) => match c.recv(buf) {
                Ok(n) => n as isize,
                Err(_) => -1,
            },
        }
    }

    fn close(&mut self) {
        match self {
            Wire::Plain(fd) => unsafe { sock_close(*fd) },
            Wire::Tls(c, fd) => {
                c.close();
                // close_notify 只是告警，socket 必须显式释放（防 fd 泄漏）。
                unsafe { sock_close(*fd) };
            }
        }
    }
}

/// 内核 getrandom（SYS_GETRANDOM=278，virtio-rng 硬件熵源）。
struct SysRng;

impl tls::Rng for SysRng {
    fn fill(&mut self, buf: &mut [u8]) {
        unsafe {
            let n = syscall3(278, buf.as_mut_ptr() as usize, buf.len(), 0);
            if (n as isize) < 0 {
                // 熵源不可用：退化为时间+地址混合（演示系统可接受）
                let t = crate::kit::now_us();
                let mut seed = t.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    ^ (buf.as_ptr() as u64).rotate_left(32);
                for b in buf.iter_mut() {
                    seed = seed
                        .wrapping_mul(6364_1362_2384_6793_005)
                        .wrapping_add(1442_6950_4088_8963_407);
                    *b = (seed >> 33) as u8;
                }
            }
        }
    }
}

/// 解码 chunked 传输编码（RFC 9112 §7.1）：`<hex-size>[;ext]\r\n <data>\r\n`
/// 序列，以 0 块结尾（其后为可选 trailer，直到流结束）。
/// 格式不合法返回 None。`buf` 需为完整原始分块流（EOF 定界）。
fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    loop {
        // 解析一行分块大小（十六进制，忽略 ;ext 扩展）。
        let line_end = find_bytes(&raw[i..], b"\r\n")? + i;
        let size_str_raw = &raw[i..line_end];
        let semi = size_str_raw.iter().position(|&b| b == b';').unwrap_or(size_str_raw.len());
        let size_str = core::str::from_utf8(&size_str_raw[..semi]).ok()?.trim();
        if size_str.is_empty() {
            return None;
        }
        let size = usize::from_str_radix(size_str, 16).ok()?;
        i = line_end + 2;
        if size == 0 {
            // 最后一块：其后是 trailer（直到流结束 / 空行），全部忽略。
            return Some(out);
        }
        // size 来自对端可控的十六进制串："FFFF…" 可解析为 usize::MAX，
        // 裸加法回绕会绕过下面的边界检查、切片终点小于起点直接 panic
        // （no_std 应用即 abort）。先做总量上限与 checked 加法。
        if size > MAX_BODY {
            return None;
        }
        if i.checked_add(size).map_or(true, |e| e > raw.len())
            || out.len().checked_add(size).map_or(true, |e| e > MAX_BODY * 2)
        {
            return None;
        }
        out.extend_from_slice(&raw[i..i + size]);
        i += size;
        // 每块数据后必须跟 CRLF。
        if raw.get(i) == Some(&b'\r') {
            i += 1;
        }
        if raw.get(i) == Some(&b'\n') {
            i += 1;
        } else {
            return None;
        }
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
