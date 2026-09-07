//! 远程软件源客户端：sources.list 解析、索引获取（file/http/https）、
//! 索引 Ed25519 验签、增量索引更新（delta）与缓存策略。
//!
//! 安全模型：
//! - **索引签名**（`Packages.sig` = 仓库密钥对 `Packages` 的 Ed25519 签名）是
//!   真实性与完整性的根本保障，传输层加密（HTTPS/TLS）是纵深防御；
//! - KunxOS 内核当前无 TLS 栈，因此 `https://` 源会被**明确拒绝**（不静默降级），
//!   待内核 TLS 落地后同一代码路径即支持；`http://` 与 `file://` 可用；
//! - 增量补丁 `delta-<base>-<target>` 同样由仓库密钥签名后才可应用。
//!
//! 缓存策略（/var/lib/pkg/cache/index/<n>/）：
//! - `Packages` + `Packages.meta`（含 Revision）+ `Packages.sig` 落盘；
//! - 更新时先取 meta 比较 Revision：未变 → 直接用缓存；变了 → 尝试
//!   `delta-<old>-<new>`（有签名则应用），失败再取全量。

use crate::ed25519;
use crate::trust;

/// 源数量上限。
pub const MAX_SOURCES: usize = 4;
/// 索引缓冲上限（单个源）。
pub const INDEX_MAX: usize = 16384;
/// HTTP 响应缓冲上限。
pub const HTTP_MAX: usize = 32768;

/// 源类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    File,
    Http,
    Https,
}

/// 一个软件源。
#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub kind: SourceKind,
    /// host[:port]（file 源为空）。
    pub host: [u8; 96],
    pub host_len: usize,
    pub port: u16,
    /// URL 路径或本地目录。
    pub path: [u8; 192],
    pub path_len: usize,
}

/// 解析 sources.list：每行一个源；`#` 注释；空行忽略。
/// 返回源数量（非法行跳过并计数 `bad`）。
pub fn parse_sources(src: &[u8], out: &mut [Source; MAX_SOURCES]) -> (usize, usize) {
    let mut n = 0usize;
    let mut bad = 0usize;
    let mut ls = 0usize;
    while ls < src.len() {
        let le = next_line(src, ls);
        let line = trim(&src[ls..le]);
        ls = le;
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        if n >= MAX_SOURCES {
            bad += 1;
            continue;
        }
        let mut s = Source {
            kind: SourceKind::File,
            host: [0; 96],
            host_len: 0,
            port: 0,
            path: [0; 192],
            path_len: 0,
        };
        let ok = if let Some(rest) = line.strip_prefix(b"file://") {
            s.kind = SourceKind::File;
            // ★ file:// 分支此前只拷贝字节、从不设置 path_len（恒为 0）——
            // join_url_path 取 path[..0] 得到空路径，所有 file:// 源解析成
            // "/Packages" 打开失败，pkg update 永远失败（-9）。
            !rest.is_empty() && fill(&mut s.path, rest) && {
                s.path_len = rest.len().min(s.path.len());
                true
            }
        } else if let Some(rest) = line.strip_prefix(b"https://") {
            s.kind = SourceKind::Https;
            parse_url(rest, &mut s)
        } else if let Some(rest) = line.strip_prefix(b"http://") {
            s.kind = SourceKind::Http;
            parse_url(rest, &mut s)
        } else {
            false
        };
        if ok {
            out[n] = s;
            n += 1;
        } else {
            bad += 1;
        }
    }
    (n, bad)
}

/// 解析 "host[:port]/path"。
fn parse_url(rest: &[u8], s: &mut Source) -> bool {
    let (hostport, path) = match rest.iter().position(|&c| c == b'/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, b"/"[..].as_ref()),
    };
    let mut host = hostport;
    let mut port: u16 = if s.kind == SourceKind::Https { 443 } else { 80 };
    if let Some(i) = hostport.iter().position(|&c| c == b':') {
        host = &hostport[..i];
        let p = &hostport[i + 1..];
        if !p.is_empty() {
            let mut v: u32 = 0;
            for &c in p {
                if !c.is_ascii_digit() {
                    return false;
                }
                v = v * 10 + (c - b'0') as u32;
            }
            if v == 0 || v > 65535 {
                return false;
            }
            port = v as u16;
        }
    }
    if host.is_empty() || !fill(&mut s.host, host) || !fill(&mut s.path, path) {
        return false;
    }
    s.host_len = host.len().min(s.host.len());
    s.port = port;
    s.path_len = path.len().min(s.path.len());
    true
}

fn fill(dst: &mut [u8], src: &[u8]) -> bool {
    if src.len() > dst.len() {
        return false;
    }
    dst[..src.len()].copy_from_slice(src);
    true
}

fn next_line(b: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < b.len() && b[i] != b'\n' {
        i += 1;
    }
    if i < b.len() {
        i + 1
    } else {
        i
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t' || b[s] == b'\r') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r') {
        e -= 1;
    }
    &b[s..e]
}

// ==================== 索引元数据 ====================

/// 解析 meta 的 Revision 字段（缺失视为 0）。
pub fn meta_revision(meta: &[u8]) -> u64 {
    let mut ls = 0usize;
    while ls < meta.len() {
        let le = next_line(meta, ls);
        let line = trim(&meta[ls..le]);
        if let Some(rest) = line.strip_prefix(b"Revision:") {
            let mut v: u64 = 0;
            let r = trim(rest);
            if r.is_empty() {
                return 0;
            }
            for &c in r {
                if !c.is_ascii_digit() {
                    return 0;
                }
                v = v.wrapping_mul(10).wrapping_add((c - b'0') as u64);
            }
            return v;
        }
        ls = le;
    }
    0
}

// ==================== HTTP 传输 ====================

/// 对 `file` 发 HTTP GET，响应体写入 `resp`，返回响应体长度；失败返回负值。
/// 响应须为 HTTP/1.x 200；仅解析 Content-Length 或读到连接关闭。
pub fn http_get(src: &Source, file: &[u8], resp: &mut [u8]) -> isize {
    if src.kind == SourceKind::Https {
        // 内核无 TLS：明确拒绝，不静默降级（索引签名仍保证真实性）
        return -5;
    }
    let host = &src.host[..src.host_len];
    let (ip, port, ok) = crate::net::resolve(host, src.port);
    if !ok || ip == 0 {
        return -1;
    }
    // 组装请求
    let mut req = [0u8; 512];
    let mut n = 0usize;
    for &c in b"GET " {
        req[n] = c;
        n += 1;
    }
    let mut path = [0u8; 256];
    let pl = join_url_path(src, file, &mut path);
    for &c in &path[..pl] {
        req[n] = c;
        n += 1;
    }
    for &c in b" HTTP/1.0\r\nHost: " {
        req[n] = c;
        n += 1;
    }
    for &c in host {
        req[n] = c;
        n += 1;
    }
    for &c in b"\r\nConnection: close\r\n\r\n" {
        req[n] = c;
        n += 1;
    }
    unsafe {
        let fd = crate::net::sock_open();
        if fd < 0 {
            return -2;
        }
        if crate::net::sock_connect(fd, ip, port) < 0 {
            crate::net::sock_close(fd);
            return -3;
        }
        let mut sent = 0usize;
        while sent < n {
            let s = crate::net::sock_send(fd, &req[sent..n]);
            if s <= 0 {
                crate::net::sock_close(fd);
                return -4;
            }
            sent += s as usize;
        }
        let mut total = 0usize;
        loop {
            let space = resp.len() - total;
            if space == 0 {
                break;
            }
            let r = crate::net::sock_recv(fd, &mut resp[total..]);
            if r < 0 {
                crate::net::sock_close(fd);
                return -6;
            }
            if r == 0 {
                break;
            }
            total += r as usize;
        }
        crate::net::sock_close(fd);
        // 解析状态行与头部
        if total < 16 {
            return -7;
        }
        // HTTP/1.x 200
        // ★ 头部必须有 \r\n\r\n 终止符：unwrap_or(total) 会让后续切片变成
        // [total+4..total]（start > end）直接 panic（服务器返回裸 \n 或截断
        // 响应即触发）。缺失按协议错误处理。
        let Some(head_end) = find_sub(&resp[..total], b"\r\n\r\n") else {
            return -7;
        };
        let status = &resp[..head_end.min(16)];
        if !(status.len() >= 12 && &status[9..12] == b"200") {
            return -8;
        }
        let body = &resp[head_end + 4..total];
        body.len() as isize
    }
}

/// 拼接 URL 路径：<src.path>/<file>（处理斜杠）。
/// 诊断用：join_url_path 的公开封装。
pub fn join_url_path_pub(src: &Source, file: &[u8], out: &mut [u8]) -> usize {
    join_url_path(src, file, out)
}

fn join_url_path(src: &Source, file: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let p = &src.path[..src.path_len];
    for &c in p {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = c;
        n += 1;
    }
    if n > 0 && out[n - 1] != b'/' && !file.is_empty() && file[0] != b'/' {
        if n < out.len() - 1 {
            out[n] = b'/';
            n += 1;
        }
    }
    for &c in file {
        if n + 1 >= out.len() {
            break;
        }
        out[n] = c;
        n += 1;
    }
    n
}

fn find_sub(data: &[u8], needle: &[u8]) -> Option<usize> {    if needle.is_empty() || data.len() < needle.len() {
        return None;
    }
    let mut i = 0;
    while i + needle.len() <= data.len() {
        if &data[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ==================== 索引获取与验签 ====================

/// 获取一个源的索引（含验签）。成功返回索引内容长度（写入 `idx`）。
/// `cached_meta`/`cached_idx` 为本地缓存（可为空）。
///
/// 返回 (索引长度, revision, 是否走增量, meta 内容长度)。
/// 索引长度 < 0 表示失败。
pub fn fetch_index(
    src: &Source,
    idx: &mut [u8],
    meta_out: &mut [u8],
    cached_idx: &[u8],
    cached_meta: &[u8],
) -> (isize, u64, bool, usize) {
    // 1. 取 meta
    let mut meta_buf = [0u8; 512];
    let meta_len = match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, b"Packages.meta", &mut p);
            crate::fsio::read_path(&p[..pl], &mut meta_buf)
        }
        _ => {
            let mut resp = [0u8; HTTP_MAX];
            let r = http_get(src, b"Packages.meta", &mut resp);
            if r < 0 {
                0
            } else {
                let m = r as usize;
                // 目标缓冲小于 HTTP_MAX：超长响应按协议错误拒绝（截断会破坏验签）
                if m > meta_buf.len() {
                    return (-7, 0, false, 0);
                }
                meta_buf[..m].copy_from_slice(&resp[..m]);
                m
            }
        }
    };
    let new_rev = meta_revision(&meta_buf[..meta_len]);
    let old_rev = meta_revision(cached_meta);
    let mc = meta_len.min(meta_out.len());
    meta_out[..mc].copy_from_slice(&meta_buf[..mc]);

    // 2. 版本未变：直接用缓存
    if new_rev != 0 && new_rev == old_rev && !cached_idx.is_empty() {
        let c = cached_idx.len().min(idx.len());
        idx[..c].copy_from_slice(&cached_idx[..c]);
        return (c as isize, new_rev, false, mc);
    }

    // 3. 尝试增量：delta-<old>-<new>
    if old_rev != 0 && new_rev != 0 && new_rev > old_rev && !cached_idx.is_empty() {
        let mut dname = [0u8; 64];
        let mut dn = 0usize;
        for &c in b"delta-" {
            dname[dn] = c;
            dn += 1;
        }
        dn += fmt_u64(old_rev, &mut dname[dn..]);
        dname[dn] = b'-';
        dn += 1;
        dn += fmt_u64(new_rev, &mut dname[dn..]);
        if let Some(applied) = try_apply_delta(src, &dname[..dn], cached_idx, idx) {
            return (applied as isize, new_rev, true, mc);
        }
    }

    // 4. 全量获取 + 验签
    match fetch_full(src, idx) {
        Some(len) => (len as isize, new_rev, false, mc),
        None => (-9, new_rev, false, mc),
    }
}

/// 从源拉取一个文件（file:// 直接读 / http GET），写入 buf，返回长度；失败 ≤ 0。
pub fn fetch_file(src: &Source, file: &[u8], buf: &mut [u8]) -> isize {
    match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, file, &mut p);
            let n = crate::fsio::read_path(&p[..pl], buf);
            if n == 0 {
                -1
            } else {
                n as isize
            }
        }
        SourceKind::Http => {
            let mut resp = [0u8; HTTP_MAX];
            let r = http_get(src, file, &mut resp);
            if r < 0 {
                return r;
            }
            let m = r as usize;
            if m > buf.len() {
                return -10;
            }
            buf[..m].copy_from_slice(&resp[..m]);
            m as isize
        }
        SourceKind::Https => -5,
    }
}

/// 全量拉取 Packages + Packages.sig 并验签。成功返回索引长度。
pub fn fetch_full(src: &Source, idx: &mut [u8]) -> Option<usize> {
    let mut pkg = [0u8; INDEX_MAX];
    let len = match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, b"Packages", &mut p);
            let n = crate::fsio::read_path(&p[..pl], &mut pkg);
            if n == 0 {
                return None;
            }
            n
        }
        _ => {
            let mut resp = [0u8; HTTP_MAX];
            let r = http_get(src, b"Packages", &mut resp);
            if r < 0 {
                return None;
            }
            let m = r as usize;
            if m > pkg.len() {
                return None;
            }
            pkg[..m].copy_from_slice(&resp[..m]);
            m
        }
    };
    // 验签
    let mut sig_buf = [0u8; 80];
    let sig_len = match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, b"Packages.sig", &mut p);
            crate::fsio::read_path(&p[..pl], &mut sig_buf)
        }
        _ => {
            let mut resp = [0u8; 256];
            let r = http_get(src, b"Packages.sig", &mut resp);
            if r < 0 {
                return None;
            }
            let m = r as usize;
            if m > sig_buf.len() {
                return None;
            }
            sig_buf[..m].copy_from_slice(&resp[..m]);
            m
        }
    };
    if sig_len != ed25519::SIG_LEN {
        return None;
    }
    let sig: [u8; 64] = sig_buf[..64].try_into().unwrap();
    if !ed25519::verify(&trust::REPO_ANCHOR, &pkg[..len], &sig) {
        return None;
    }
    let c = len.min(idx.len());
    idx[..c].copy_from_slice(&pkg[..c]);
    Some(c)
}

/// 尝试拉取并应用增量补丁。成功返回新索引长度；失败 None。
fn try_apply_delta(src: &Source, dname: &[u8], base: &[u8], out: &mut [u8]) -> Option<usize> {
    // 拉取 delta 内容（≤ 8KB）
    let mut d = [0u8; 8192];
    let dlen = match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, dname, &mut p);
            let n = crate::fsio::read_path(&p[..pl], &mut d);
            if n == 0 {
                return None;
            }
            n
        }
        _ => {
            let mut resp = [0u8; HTTP_MAX];
            let r = http_get(src, dname, &mut resp);
            if r < 0 {
                return None;
            }
            let m = r as usize;
            if m > d.len() {
                return None;
            }
            d[..m].copy_from_slice(&resp[..m]);
            m
        }
    };
    // 验签 delta
    let mut sig_buf = [0u8; 80];
    let mut sname = [0u8; 80];
    let mut sn = 0usize;
    for &c in dname {
        sname[sn] = c;
        sn += 1;
    }
    for &c in b".sig" {
        sname[sn] = c;
        sn += 1;
    }
    let sig_len = match src.kind {
        SourceKind::File => {
            let mut p = [0u8; 256];
            let pl = join_url_path(src, &sname[..sn], &mut p);
            crate::fsio::read_path(&p[..pl], &mut sig_buf)
        }
        _ => {
            let mut resp = [0u8; 256];
            let r = http_get(src, &sname[..sn], &mut resp);
            if r < 0 {
                return None;
            }
            let m = r as usize;
            if m > sig_buf.len() {
                return None;
            }
            sig_buf[..m].copy_from_slice(&resp[..m]);
            m
        }
    };
    if sig_len != ed25519::SIG_LEN {
        return None;
    }
    let sig: [u8; 64] = sig_buf[..64].try_into().unwrap();
    if !ed25519::verify(&trust::REPO_ANCHOR, &d[..dlen], &sig) {
        return None;
    }
    // 应用
    let n = crate::delta::apply_delta(&d[..dlen], base, out)?;
    Some(n)
}

fn fmt_u64(mut v: u64, out: &mut [u8]) -> usize {
    if v == 0 {
        if out.is_empty() {
            return 0;
        }
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    let c = n.min(out.len());
    out[..c].copy_from_slice(&tmp[i..i + c]);
    c
}
