//! HTTP/1.1 客户端（基于抽象传输，应用侧接内核 TCP 栈、测试侧接 std）。
//!
//! 约束（KunxOS 网络栈）：无 TLS、阻塞式 recv、无 select。因此：
//!   - 每次请求独立 TCP 连接，请求头带 `Connection: close`；
//!   - 响应按 Content-Length 精确读取；无 Content-Length 时读到对端关闭；
//!   - 不支持分块传输编码（服务端对 DAV 一般使用 Content-Length；
//!     若收到 `Transfer-Encoding: chunked` 则返回 NotSupported）。

use crate::sync::util;

/// 抽象网络传输（应用：app_kit socket 系统调用；测试：std::net）。
pub trait Net {
    /// 解析主机（域名或点分 IP）并连接，返回句柄（>=0）或负值错误码。
    fn connect(&mut self, host: &[u8], port: u16) -> isize;
    /// 发送全部数据。返回发送字节数或负值。
    fn send(&mut self, h: isize, data: &[u8]) -> isize;
    /// 读取到 buf。返回字节数；0=对端关闭；负值=错误。
    fn recv(&mut self, h: isize, buf: &mut [u8]) -> isize;
    /// 关闭连接。
    fn close(&mut self, h: isize);
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum HttpErr {
    Connect,
    Send,
    Recv,
    Timeout,
    BadResponse,
    ChunkedUnsupported,
    BufferTooSmall,
}

impl HttpErr {
    pub fn label(&self) -> &'static [u8] {
        match self {
            HttpErr::Connect => b"connect",
            HttpErr::Send => b"send",
            HttpErr::Recv => b"recv",
            HttpErr::Timeout => b"timeout",
            HttpErr::BadResponse => b"bad_response",
            HttpErr::ChunkedUnsupported => b"chunked",
            HttpErr::BufferTooSmall => b"buffer",
        }
    }
}

/// HTTP 响应：状态码 + 缓冲区内的 body 切片。
pub struct Response<'a> {
    pub status: u16,
    pub body: &'a [u8],
}

/// 从响应中取头部值（大小写不敏感）。headers 需以 \r\n 分隔。
pub fn header_get<'a>(headers: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut rest = headers;
    loop {
        let (line, tail) = util::split2(rest, b'\n');
        let line = util::trim(line);
        if line.is_empty() {
            break;
        }
        let (k, v) = util::split2(line, b':');
        if util::eq_ignore_case(util::trim(k), name) {
            return Some(util::trim(v));
        }
        if tail.is_empty() {
            break;
        }
        rest = tail;
    }
    None
}

/// 基础认证头值：`Basic base64(user:pass)`，写入 out，返回长度。
pub fn basic_auth(user: &[u8], pass: &[u8], out: &mut [u8]) -> usize {
    let mut pair = [0u8; 192];
    let n = util::copy_into(&mut pair, user);
    let n = util::push_str(&mut pair, n, b":");
    let n = util::push_str(&mut pair, n, pass);
    let mut b64 = [0u8; 260];
    let b = util::base64_encode(&pair[..n], &mut b64);
    let mut o = util::copy_into(out, b"Basic ");
    o += util::copy_into(&mut out[o..], &b64[..b]);
    o
}

/// 发送 HTTP 请求并把完整响应读入 `buf`。
/// `buf` 需容纳整个响应（状态行 + 头 + 体）。
pub fn request<'b>(
    net: &mut dyn Net,
    host: &[u8],
    port: u16,
    req: &[u8],
    buf: &'b mut [u8],
) -> Result<Response<'b>, HttpErr> {
    let h = net.connect(host, port);
    if h < 0 {
        return Err(HttpErr::Connect);
    }
    let mut sent = 0usize;
    while sent < req.len() {
        let n = net.send(h, &req[sent..]);
        if n <= 0 {
            net.close(h);
            return Err(HttpErr::Send);
        }
        sent += n as usize;
    }
    // 读取头部（直到 \r\n\r\n）
    let mut total = 0usize;
    loop {
        if let Some(hi) = util::find_sub(&buf[..total], b"\r\n\r\n") {
            let head_end = hi + 4;
            // 拷贝头部到栈缓冲，避免与后续 recv 的 &mut buf 冲突
            let mut head_copy = [0u8; 2048];
            let hc = util::copy_into(&mut head_copy, &buf[..hi]);
            let head = &head_copy[..hc];
            // 状态行
            let (status_line, headers) = match util::find_byte(head, b'\n') {
                Some(li) => (util::trim(&head[..li]), &head[li + 1..]),
                None => (util::trim(head), &[][..]),
            };
            let status = parse_status(status_line).ok_or(HttpErr::BadResponse)?;
            // 分块不支持
            if let Some(te) = header_get(headers, b"Transfer-Encoding") {
                if util::eq_ignore_case(te, b"chunked") {
                    net.close(h);
                    return Err(HttpErr::ChunkedUnsupported);
                }
            }
            // 计算 body 长度
            let body_len = match header_get(headers, b"Content-Length") {
                Some(cl) => util::parse_u32(cl).ok_or(HttpErr::BadResponse)? as usize,
                None => usize::MAX, // 读到关闭
            };
            // 读取 body（可能需要继续 recv）
            while total < head_end + body_len.min(usize::MAX - head_end) {
                if body_len != usize::MAX && total >= head_end + body_len {
                    break;
                }
                if total >= buf.len() {
                    net.close(h);
                    return Err(HttpErr::BufferTooSmall);
                }
                let n = net.recv(h, &mut buf[total..]);
                if n <= 0 {
                    break;
                }
                total += n as usize;
                // 无 Content-Length：持续读到关闭
                if body_len == usize::MAX {
                    continue;
                }
            }
            let body_end = if body_len == usize::MAX {
                total
            } else {
                let want = head_end + body_len;
                if total < want {
                    net.close(h);
                    return Err(HttpErr::Recv);
                }
                want
            };
            net.close(h);
            return Ok(Response {
                status,
                body: &buf[head_end..body_end],
            });
        }
        if total >= buf.len() {
            net.close(h);
            return Err(HttpErr::BufferTooSmall);
        }
        let n = net.recv(h, &mut buf[total..]);
        if n <= 0 {
            net.close(h);
            return Err(HttpErr::Recv);
        }
        total += n as usize;
    }
}

fn parse_status(status_line: &[u8]) -> Option<u16> {
    if !util::starts_with_ci(status_line, b"HTTP/") {
        return None;
    }
    let t = util::trim(status_line);
    // 跳过 "HTTP/x.y"，取第一个空格后的首个数字起的 3 位状态码
    let after = match util::find_byte(t, b' ') {
        Some(i) => &t[i + 1..],
        None => t,
    };
    let start = after.iter().position(|c| c.is_ascii_digit())?;
    if start + 3 > after.len() {
        return None;
    }
    let code = util::parse_u32(&after[start..start + 3])?;
    if code > 999 {
        return None;
    }
    Some(code as u16)
}

/// 组装请求（HTTP/1.1 + Connection: close）。
/// `extra_headers` 为可选的额外头（如 Depth / If-Match），每项以 \r\n 结尾。
/// 返回写入 out 的总长度。
pub fn build_request(
    method: &[u8],
    path: &[u8],
    host: &[u8],
    extra_headers: &[u8],
    body: &[u8],
    out: &mut [u8],
) -> usize {
    let mut n = 0usize;
    n = util::push_str(out, n, method);
    n = util::push_str(out, n, b" ");
    n = util::push_str(out, n, path);
    n = util::push_str(out, n, b" HTTP/1.1\r\n");
    n = util::push_str(out, n, b"Host: ");
    n = util::push_str(out, n, host);
    n = util::push_str(out, n, b"\r\n");
    n = util::push_str(out, n, b"Connection: close\r\n");
    n = util::push_str(out, n, extra_headers);
    n = util::push_str(out, n, b"Content-Length: ");
    n = util::push_dec(out, n, body.len() as u64);
    n = util::push_str(out, n, b"\r\n\r\n");
    n = util::push_str(out, n, body);
    n.min(out.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 测试用 std 传输。
    struct StdNet;
    impl Net for StdNet {
        fn connect(&mut self, host: &[u8], port: u16) -> isize {
            let host = String::from_utf8_lossy(host).to_string();
            match std::net::TcpStream::connect((host.as_str(), port)) {
                Ok(s) => {
                    let h = Box::into_raw(Box::new(s)) as isize;
                    h
                }
                Err(_) => -1,
            }
        }
        fn send(&mut self, h: isize, data: &[u8]) -> isize {
            let s = unsafe { &mut *(h as *mut std::net::TcpStream) };
            s.write_all(data).map(|_| data.len() as isize).unwrap_or(-1)
        }
        fn recv(&mut self, h: isize, buf: &mut [u8]) -> isize {
            let s = unsafe { &mut *(h as *mut std::net::TcpStream) };
            s.read(buf).map(|n| n as isize).unwrap_or(-1)
        }
        fn close(&mut self, h: isize) {
            unsafe { drop(Box::from_raw(h as *mut std::net::TcpStream)); }
        }
    }

    #[test]
    fn basic_auth_value() {
        let mut out = [0u8; 64];
        let n = basic_auth(b"user", b"pass", &mut out);
        assert_eq!(&out[..n], b"Basic dXNlcjpwYXNz");
    }

    #[test]
    fn header_lookup() {
        let h = b"Content-Type: text/calendar\r\nETag: \"x1\"\r\nX-Empty:\r\n";
        assert_eq!(header_get(h, b"etag"), Some(&b"\"x1\""[..]));
        assert_eq!(header_get(h, b"Content-Type"), Some(&b"text/calendar"[..]));
        assert_eq!(header_get(h, b"Missing"), None);
    }

    #[test]
    fn request_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let n = sock.read(&mut req).unwrap();
            let req = String::from_utf8_lossy(&req[..n]).to_string();
            assert!(req.starts_with("GET /dav/ HTTP/1.1"));
            assert!(req.contains("Host: 127.0.0.1"));
            sock.write_all(b"HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: 12\r\n\r\n<multistatus").unwrap();
        });
        let mut reqbuf = [0u8; 2048];
        let rn = build_request(b"GET", b"/dav/", b"127.0.0.1", b"Depth: 0\r\n", b"", &mut reqbuf);
        let mut resp_buf = [0u8; 4096];
        let mut net = StdNet;
        let resp = request(&mut net, b"127.0.0.1", port, &reqbuf[..rn], &mut resp_buf).unwrap();
        assert_eq!(resp.status, 207);
        assert_eq!(resp.body, b"<multistatus");
        handle.join().unwrap();
    }
}
