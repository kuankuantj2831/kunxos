//! URL 解析 / 格式化 / 相对链接解析（no_std + alloc）。
//!
//! 支持协议：`http`（HTTP/1.1 客户端）、`https`（TLS 预留，报错）、
//! `file`（读取 rootfs 文件）、`kunxos`（内置演示页）。裸主机名/IP
//! 自动补全为 `http://`。相对链接（`href`）按文档基址解析，
//! 支持 `..` / 目录相对 / 查询串。

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Http,
    Https,
    File,
    Kunxos,
    Unknown,
}

impl Scheme {
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::File => "file",
            Scheme::Kunxos => "kunxos",
            Scheme::Unknown => "?",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Url {
    pub scheme: Scheme,
    /// 主机名（http/https）；file/kunxos 为空。
    pub host: String,
    /// 端口（http 默认 80）。
    pub port: u16,
    /// 路径（http/https 以 '/' 开头；file 为文件系统绝对路径；
    /// kunxos 为页面名，如 "home"）。
    pub path: String,
    /// 查询串（不含 '?'）。
    pub query: String,
}

impl Url {
    pub fn empty() -> Url {
        Url {
            scheme: Scheme::Unknown,
            host: String::new(),
            port: 0,
            path: String::new(),
            query: String::new(),
        }
    }

    /// 展示形式（地址栏显示）。
    pub fn display(&self) -> String {
        let mut s = String::new();
        match self.scheme {
            Scheme::Kunxos => {
                s.push_str("kunxos://");
                s.push_str(&self.path);
                if !self.query.is_empty() {
                    s.push('?');
                    s.push_str(&self.query);
                }
            }
            Scheme::File => {
                s.push_str("file://");
                s.push_str(&self.path);
            }
            Scheme::Http | Scheme::Https => {
                s.push_str(self.scheme.name());
                s.push_str("://");
                s.push_str(&self.host);
                if self.scheme == Scheme::Http && self.port != 0 && self.port != 80
                    || self.scheme == Scheme::Https && self.port != 0 && self.port != 443
                {
                    s.push(':');
                    s.push_str(&self.port.to_string());
                }
                s.push_str(&self.path);
                if !self.query.is_empty() {
                    s.push('?');
                    s.push_str(&self.query);
                }
            }
            Scheme::Unknown => {
                s.push_str(&self.path);
            }
        }
        s
    }

    /// 主机:端口（HTTP Host 头）。
    pub fn host_port(&self) -> String {
        let mut s = self.host.clone();
        if self.scheme == Scheme::Http && self.port != 0 && self.port != 80 {
            s.push(':');
            s.push_str(&self.port.to_string());
        }
        s
    }

    /// 请求路径（含查询）。
    pub fn request_target(&self) -> String {
        let mut s = String::new();
        if self.path.is_empty() {
            s.push('/');
        } else {
            s.push_str(&self.path);
        }
        if !self.query.is_empty() {
            s.push('?');
            s.push_str(&self.query);
        }
        s
    }

    /// 同源判断（scheme + host + port）。
    pub fn same_origin(&self, other: &Url) -> bool {
        self.scheme == other.scheme
            && self.host == other.host
            && (self.port == other.port
                || (self.port == 0 || self.port == 80)
                    && (other.port == 0 || other.port == 80))
    }
}

/// 解析用户输入的地址。空串返回 None。
/// - 带协议的 URL 直接解析
/// - `kunxos://x` / `file:///x` / `http(s)://host/path`
/// - 裸主机名 / IP / 路径（如 `10.0.2.2:8080/demo`）→ http://
pub fn parse(input: &str) -> Option<Url> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // 显式协议
    for (p, scheme) in [
        ("kunxos://", Scheme::Kunxos),
        ("file://", Scheme::File),
        ("http://", Scheme::Http),
        ("https://", Scheme::Https),
    ] {
        if lower.strip_prefix(p).is_some() {
            let rest = &t[p.len()..];
            return match scheme {
                Scheme::Kunxos => {
                    let mut u = Url::empty();
                    u.scheme = Scheme::Kunxos;
                    let (path, query) = match rest.find('?') {
                        Some(i) => (&rest[..i], &rest[i + 1..]),
                        None => (rest, ""),
                    };
                    u.path = path.trim_matches('/').to_string();
                    u.query = query.to_string();
                    Some(u)
                }
                Scheme::File => {
                    let mut u = Url::empty();
                    u.scheme = Scheme::File;
                    u.path = rest.to_string();
                    Some(u)
                }
                Scheme::Http | Scheme::Https => parse_authority(rest, scheme),
                _ => None,
            };
        }
    }
    // 没有显式协议：按 http 处理（裸主机 / IP / 带端口路径）
    parse_authority(t, Scheme::Http)
}

/// 解析 `host[:port][/path][?query]`。
fn parse_authority(s: &str, scheme: Scheme) -> Option<Url> {
    let mut u = Url {
        scheme,
        host: String::new(),
        port: 0,
        path: String::new(),
        query: String::new(),
    };
    // 去掉路径与查询
    let (authority, path_query) = match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let (path_part, query) = match path_query.find('?') {
        Some(i) => (&path_query[..i], &path_query[i + 1..]),
        None => (path_query, ""),
    };
    u.query = percent_decode(query);
    // host[:port]
    let (host, port_str) = match authority.rfind(':') {
        Some(i) => (&authority[..i], Some(&authority[i + 1..])),
        None => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    u.host = host.to_string();
    if let Some(p) = port_str {
        if let Ok(v) = p.parse::<u16>() {
            u.port = v;
        } else if !p.is_empty() {
            return None;
        }
    }
    if u.port == 0 {
        u.port = if scheme == Scheme::Https { 443 } else { 80 };
    }
    // 路径部分
    if path_part.is_empty() {
        u.path = "/".to_string();
    } else {
        u.path = percent_decode(path_part);
        if !u.path.starts_with('/') {
            u.path.insert(0, '/');
        }
    }
    Some(u)
}

/// 简单百分号解码（%XX → 字节；忽略非法序列）。
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 0 {
            if i + 2 < b.len() {
                let h = hex(b[i + 1]);
                let l = hex(b[i + 2]);
                if h >= 0 && l >= 0 {
                    out.push((h << 4 | l) as u8);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> i32 {
    match c {
        b'0'..=b'9' => (c - b'0') as i32,
        b'a'..=b'f' => (c - b'a' + 10) as i32,
        b'A'..=b'F' => (c - b'A' + 10) as i32,
        _ => -1,
    }
}

/// 表单值百分号编码（空格→+，非安全字符→%XX）。
pub fn form_encode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    for &c in b {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(c as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(c >> 4) as usize] as char);
                out.push(HEX[(c & 0xF) as usize] as char);
            }
        }
    }
    out
}

const HEX: [u8; 16] = *b"0123456789ABCDEF";

/// 把相对引用（href/src/action）按基址解析为绝对 URL。
/// 绝对引用（带协议）直接解析；`/path` 相对主机根；
/// 其余按基址目录拼接并折叠 `..`/`.`。
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let h = href.trim();
    if h.is_empty() {
        return Some(base.clone());
    }
    // 片段/纯查询：丢弃片段
    let h = match h.find('#') {
        Some(i) => &h[..i],
        None => h,
    };
    if h.starts_with("kunxos://") || h.starts_with("file://")
        || h.starts_with("http://") || h.starts_with("https://")
    {
        return parse(h);
    }
    // 协议相对 //host/path
    if let Some(rest) = h.strip_prefix("//") {
        return parse_authority(rest, base.scheme);
    }
    if base.scheme == Scheme::Kunxos || base.scheme == Scheme::File {
        // 内置页 / 文件页内的相对链接
        let mut u = base.clone();
        u.query.clear();
        if base.scheme == Scheme::Kunxos {
            // 扁平命名空间
            u.path = h.trim_start_matches('/').to_string();
        } else if let Some(rest) = h.strip_prefix('/') {
            u.path = rest.to_string();
        } else {
            // 目录相对：以最后一个 '/' 之前的目录为基
            if let Some(i) = base.path.rfind('/') {
                u.path = format!("{}/{}", &base.path[..i], h);
            } else {
                u.path = format!("/{}", h);
            }
        }
        return Some(u);
    }
    // http(s)：绝对路径
    let mut u = Url {
        scheme: base.scheme,
        host: base.host.clone(),
        port: base.port,
        path: String::new(),
        query: String::new(),
    };
    if let Some(rest) = h.strip_prefix('/') {
        u.path = format!("/{}", rest);
        return Some(u);
    }
    if h.starts_with('?') {
        u.path = base.path.clone();
        u.query = h[1..].to_string();
        return Some(u);
    }
    // 目录相对 + 折叠 ..
    let mut segs: Vec<&str> = Vec::new();
    for seg in base.path.split('/') {
        if !seg.is_empty() {
            segs.push(seg);
        }
    }
    if segs.last().is_some() && !base.path.ends_with('/') {
        segs.pop(); // 去掉文件名
    }
    for seg in h.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    if segs.is_empty() {
        u.path = "/".to_string();
    } else {
        u.path = format!("/{}", segs.join("/"));
    }
    Some(u)
}
