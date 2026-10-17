//! R2/S3 客户端（AWS Signature Version 4，手写实现，no_std、零依赖）。
//!
//! 与天气/云应用同一套 TCP 内核网络栈（SYS_SOCKET/SYS_CONNECT/SYS_SENDTO/
//! SYS_RECVFROM/SYS_DNS_RESOLVE），请求经 AWS SigV4 签名后走标准 S3 API：
//!   - `s3_get`      GET Object（Packages.json 索引、.kunx 包，读入内存缓冲）
//!   - `s3_download` GET Object 流式写入文件（大包下载 + 进度回调）
//!   - `s3_head`     HEAD Object（更新检查用：Content-Length / ETag，预留）
//!
//! 说明：
//! - 端点以 `host[:port]` 形式配置（R2 path-style：`/<bucket>/<key>`）。
//! - 当前内核无 TLS 栈：`https://` 端点被**明确拒绝**（返回 R2_ERR_TLS），
//!   不静默降级——与 kunxpkg/repo.rs 对 https 的处理一致；待内核 TLS 落地后
//!   同一签名代码路径直接可用。
//! - GET 请求体为空：payload 哈希统一为 SHA-256("")（AWS 允许该用法）。
//! - 每次请求用当前系统墙钟（UTC）生成 `x-amz-date` 与 scope date。

#[path = "../../../kunxsec/src/sha256.rs"]
mod sha256;

#[path = "../../../kunxpkg/src/hex.rs"]
mod hex;

// ==================== 错误码 ====================

/// HTTPS 端点：内核无 TLS 栈，明确拒绝。
pub const R2_ERR_TLS: isize = -30;
/// 配置缺失/非法。
pub const R2_ERR_CONFIG: isize = -31;
/// DNS 解析失败。
pub const R2_ERR_RESOLVE: isize = -32;
/// TCP 连接失败。
pub const R2_ERR_CONNECT: isize = -33;
/// 发送失败。
pub const R2_ERR_SEND: isize = -34;
/// 接收失败。
pub const R2_ERR_RECV: isize = -35;
/// 非 2xx 状态。
pub const R2_ERR_HTTP: isize = -36;
/// 403（凭证错误 / 权限不足）。
pub const R2_ERR_AUTH: isize = -37;
/// 404（对象不存在）。
pub const R2_ERR_NOTFOUND: isize = -38;
/// 响应超过缓冲/目标大小。
pub const R2_ERR_BIG: isize = -39;
/// 响应头非法/不完整。
pub const R2_ERR_BADRESP: isize = -40;
/// 系统墙钟不可用（无法生成签名时间戳）。
pub const R2_ERR_TIME: isize = -41;

/// R2 软件源配置（静态缓冲，来自 /data/store/config）。
#[derive(Clone, Copy)]
pub struct R2Config {
    /// 端点 host（不含 scheme；`https://` 由调用方标记 tls_only）。
    pub host: [u8; 128],
    pub host_len: usize,
    pub port: u16,
    /// 端点需要 TLS（配置里写了 https://）——当前内核不支持。
    pub tls_only: bool,
    pub bucket: [u8; 64],
    pub bucket_len: usize,
    pub region: [u8; 32],
    pub region_len: usize,
    pub access_key: [u8; 64],
    pub access_key_len: usize,
    pub secret_key: [u8; 64],
    pub secret_key_len: usize,
}

impl R2Config {
    pub const fn empty() -> R2Config {
        R2Config {
            host: [0; 128],
            host_len: 0,
            port: 80,
            tls_only: false,
            bucket: [0; 64],
            bucket_len: 0,
            region: [0; 32],
            region_len: 0,
            access_key: [0; 64],
            access_key_len: 0,
            secret_key: [0; 64],
            secret_key_len: 0,
        }
    }

    /// 是否已具备发起请求所需的全部字段。
    pub fn ready(&self) -> bool {
        self.host_len > 0 && self.bucket_len > 0 && self.access_key_len > 0 && self.secret_key_len > 0
    }
}

// ==================== 系统调用与 TCP 网络层 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_CLOCK_GETTIME: usize = 113;
const CLOCK_MONOTONIC: i32 = 1;
const SYS_SOCKET: usize = 198;
const SYS_CONNECT: usize = 203;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SHUTDOWN: usize = 210;
const SYS_DNS_RESOLVE: usize = 403;

const AF_INET: usize = 2;
const SOCK_STREAM: usize = 1;

use core::arch::asm;

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, in("a2") a3, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, in("a5") a6, options(nostack));
    ret
}

/// 当前墙钟（微秒，UTC epoch）。失败返回 0。
fn now_us() -> u64 {
    // 单调钟：RTC 就绪前后墙钟会跳变，时间差判定（动画/超时）必须用
    // CLOCK_MONOTONIC（黑屏事故同族防御）。
    let mut tv = [0u8; 16];
    unsafe {
        syscall3(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC as usize, tv.as_mut_ptr() as usize, 0);
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
}
/// 写 fd（供流式下载落盘）。
fn write_fd(fd: isize, data: &[u8]) -> isize {
    unsafe {
        let mut off = 0usize;
        while off < data.len() {
            let n = syscall3(SYS_WRITE, fd as usize, data.as_ptr().add(off) as usize, data.len() - off) as isize;
            if n <= 0 {
                return off as isize;
            }
            off += n as usize;
        }
        data.len() as isize
    }
}

fn sock_open() -> isize {
    unsafe { syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0) as isize }
}

fn sock_connect(fd: isize, ip: u32, port: u16) -> isize {
    let mut sa = [0u8; 8];
    sa[0] = AF_INET as u8;
    sa[1] = 0;
    sa[2] = (port >> 8) as u8;
    sa[3] = (port & 0xFF) as u8;
    sa[4] = (ip >> 24) as u8;
    sa[5] = ((ip >> 16) & 0xFF) as u8;
    sa[6] = ((ip >> 8) & 0xFF) as u8;
    sa[7] = (ip & 0xFF) as u8;
    unsafe { syscall2(SYS_CONNECT, fd as usize, sa.as_ptr() as usize) as isize }
}

fn sock_send(fd: isize, data: &[u8]) -> isize {
    unsafe { syscall6(SYS_SENDTO, fd as usize, data.as_ptr() as usize, data.len(), 0, 0, 0) as isize }
}

fn sock_recv(fd: isize, buf: &mut [u8]) -> isize {
    unsafe { syscall6(SYS_RECVFROM, fd as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0) as isize }
}

fn sock_close(fd: isize) {
    unsafe { syscall1(SYS_SHUTDOWN, fd as usize) };
}

fn dns_resolve(name: &[u8]) -> u32 {
    let mut buf = [0u8; 256];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name[..n]);
    buf[n] = 0;
    unsafe { syscall1(SYS_DNS_RESOLVE, buf.as_ptr() as usize) as u32 }
}

/// 解析 "host[:port]" → (ip, port, ok)；域名走内核 DNS。
fn resolve(host: &[u8], default_port: u16) -> (u32, u16, bool) {
    let mut h = host;
    let mut port = default_port;
    if let Some(i) = host.iter().position(|&c| c == b':') {
        h = &host[..i];
        let p = &host[i + 1..];
        if !p.is_empty() {
            let mut v: u32 = 0;
            for &c in p {
                if !c.is_ascii_digit() {
                    return (0, 0, false);
                }
                v = v * 10 + (c - b'0') as u32;
            }
            if v > 65535 {
                return (0, 0, false);
            }
            port = v as u16;
        }
    }
    if let Some(ip) = ip_parse(h) {
        return (ip, port, true);
    }
    let ip = dns_resolve(h);
    if ip != 0 {
        (ip, port, true)
    } else {
        (0, 0, false)
    }
}

fn ip_parse(s: &[u8]) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut pi = 0usize;
    let mut acc: u32 = 0;
    let mut digits = false;
    for &c in s {
        if c.is_ascii_digit() {
            acc = acc * 10 + (c - b'0') as u32;
            if acc > 255 {
                return None;
            }
            digits = true;
        } else if c == b'.' {
            if !digits || pi >= 3 {
                return None;
            }
            parts[pi] = acc;
            pi += 1;
            acc = 0;
            digits = false;
        } else {
            return None;
        }
    }
    if !digits || pi != 3 {
        return None;
    }
    parts[3] = acc;
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

// ==================== 时间与 URL 编码 ====================

/// epoch 天数 → 公历 (年,月,日)（UTC，Howard Hinnant 算法）。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

/// 写入两位十进制（v < 100）。
fn push2(out: &mut [u8], off: usize, v: u32) {
    out[off] = b'0' + (v / 10) as u8;
    out[off + 1] = b'0' + (v % 10) as u8;
}

/// 写入四位十进制（v < 10000）。
fn push4(out: &mut [u8], off: usize, v: i32) {
    out[off] = b'0' + ((v / 1000) % 10) as u8;
    out[off + 1] = b'0' + ((v / 100) % 10) as u8;
    out[off + 2] = b'0' + ((v / 10) % 10) as u8;
    out[off + 3] = b'0' + (v % 10) as u8;
}

/// epoch 秒 → `YYYYMMDDTHHMMSSZ`(16B) 与 `YYYYMMDD`(8B)。
fn fmt_amz_date(epoch: u64, stamp_out: &mut [u8; 8], full_out: &mut [u8; 16]) {
    let secs = epoch as i64;
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let h = (sod / 3600) as u32;
    let mi = ((sod % 3600) / 60) as u32;
    let s = (sod % 60) as u32;
    push4(stamp_out, 0, y);
    push2(stamp_out, 4, m);
    push2(stamp_out, 6, d);
    push4(full_out, 0, y);
    push2(full_out, 4, m);
    push2(full_out, 6, d);
    full_out[8] = b'T';
    push2(full_out, 9, h);
    push2(full_out, 11, mi);
    push2(full_out, 13, s);
    full_out[15] = b'Z';
}

/// RFC 3986 路径编码（保留字母数字与 `-._~/`）。
fn uri_encode(src: &[u8], dst: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut n = 0usize;
    for &c in src {
        let unres = (c >= b'a' && c <= b'z')
            || (c >= b'A' && c <= b'Z')
            || (c >= b'0' && c <= b'9')
            || c == b'-' || c == b'.' || c == b'_' || c == b'~' || c == b'/';
        if unres {
            if n < dst.len() {
                dst[n] = c;
                n += 1;
            }
        } else if n + 2 < dst.len() {
            dst[n] = b'%';
            dst[n + 1] = HEX[(c >> 4) as usize];
            dst[n + 2] = HEX[(c & 0xF) as usize];
            n += 3;
        }
    }
    n
}

fn find_sub(data: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || data.len() < needle.len() {
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

// ==================== HMAC-SHA256 ====================

fn hmac_sha256(key: &[u8], msg: &[u8], out: &mut [u8; 32]) {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let d = sha256::sha256(key);
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; 64];
    let mut opad = [0u8; 64];
    for i in 0..64 {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut h = sha256::Sha256::new();
    h.update(&ipad);
    h.update(msg);
    let inner = h.finalize();
    let mut h2 = sha256::Sha256::new();
    h2.update(&opad);
    h2.update(&inner);
    *out = h2.finalize();
}

// ==================== SigV4 签名 ====================

const SVC: &[u8] = b"s3";
const ALGO: &[u8] = b"AWS4-HMAC-SHA256";
const TERM: &[u8] = b"aws4_request";
const SIGNED_HDRS: &[u8] = b"host;x-amz-content-sha256;x-amz-date";

/// 把 `s` 追加到 `buf[off..]`，返回新 off（超长截断）。
fn push_out(buf: &mut [u8], off: usize, s: &[u8]) -> usize {
    let n = s.len().min(buf.len().saturating_sub(off));
    buf[off..off + n].copy_from_slice(&s[..n]);
    off + n
}

/// 签名并组装完整 HTTP 请求到 `head_buf`；成功返回 0，失败返回负错误码。
pub fn build_request(
    cfg: &R2Config,
    uri: &[u8],
    head_buf: &mut [u8],
) -> isize {
    if !cfg.ready() {
        return R2_ERR_CONFIG;
    }
    if cfg.tls_only {
        return R2_ERR_TLS;
    }
    let now_s = now_us() / 1_000_000;
    if now_s == 0 {
        return R2_ERR_TIME;
    }
    let mut date_stamp = [0u8; 8];
    let mut amz_date = [0u8; 16];
    fmt_amz_date(now_s, &mut date_stamp, &mut amz_date);

    // payload 哈希：GET 无请求体 → SHA-256("") 的 hex
    let mut payload_hash = [0u8; 64];
    {
        let ph = sha256::sha256(&[]);
        hex::encode(&ph, &mut payload_hash);
    }

    // host 头值（lowercase host[:port]；端口 80 省略）
    let mut host_hdr = [0u8; 144];
    let mut hn = 0usize;
    {
        let cap = host_hdr.len();
        for &c in &cfg.host[..cfg.host_len] {
            if hn < cap {
                host_hdr[hn] = c;
                hn += 1;
            }
        }
        if cfg.port != 80 && hn + 6 <= cap {
            host_hdr[hn] = b':';
            hn += 1;
            let _ = (cfg.port / 100) % 10;
            let mut sv = cfg.port;
            let mut t = [0u8; 5];
            let mut tn = 0usize;
            if sv == 0 {
                t[tn] = b'0';
                tn += 1;
            } else {
                while sv > 0 {
                    let d = (sv % 10) as u8;
                    t[tn] = b'0' + d;
                    tn += 1;
                    sv /= 10;
                }
                t[..tn].reverse();
            }
            for i in 0..tn {
                if hn < cap {
                    host_hdr[hn] = t[i];
                    hn += 1;
                }
            }
        }
    }

    // ---- 规范化请求 ----
    let mut canon = [0u8; 1024];
    let mut cn = 0usize;
    cn = push_out(&mut canon, cn, b"GET\n");
    cn = push_out(&mut canon, cn, uri);
    cn = push_out(&mut canon, cn, b"\n\n");
    cn = push_out(&mut canon, cn, b"host:");
    cn = push_out(&mut canon, cn, &host_hdr[..hn]);
    cn = push_out(&mut canon, cn, b"\n");
    cn = push_out(&mut canon, cn, b"x-amz-content-sha256:");
    cn = push_out(&mut canon, cn, &payload_hash);
    cn = push_out(&mut canon, cn, b"\n");
    cn = push_out(&mut canon, cn, b"x-amz-date:");
    cn = push_out(&mut canon, cn, &amz_date);
    cn = push_out(&mut canon, cn, b"\n\n");
    cn = push_out(&mut canon, cn, SIGNED_HDRS);
    cn = push_out(&mut canon, cn, b"\n");
    cn = push_out(&mut canon, cn, &payload_hash);
    cn = push_out(&mut canon, cn, b"\n");
    let canon_req = &canon[..cn];

    // 哈希规范化请求
    let ch = sha256::sha256(canon_req);
    let mut ch_hex = [0u8; 64];
    hex::encode(&ch, &mut ch_hex);

    // ---- 待签字符串 ----
    let mut sts = [0u8; 512];
    let mut sn = 0usize;
    sn = push_out(&mut sts, sn, ALGO);
    sn = push_out(&mut sts, sn, b"\n");
    sn = push_out(&mut sts, sn, &amz_date);
    sn = push_out(&mut sts, sn, b"\n");
    sn = push_out(&mut sts, sn, &date_stamp);
    sn = push_out(&mut sts, sn, b"/");
    sn = push_out(&mut sts, sn, &cfg.region[..cfg.region_len]);
    sn = push_out(&mut sts, sn, b"/s3/aws4_request\n");
    sn = push_out(&mut sts, sn, &ch_hex);
    let string_to_sign = &sts[..sn];

    // ---- 签名密钥 ----
    // kDate = HMAC("AWS4"+secret, date) —— date 是 HMAC 的 **message**，
    // 不进密钥。此前把 date 拼进 key、message 传空：派生密钥与服务端
    // 不一致，对真实 S3/R2 所有请求必然 403（本地 stub 不校验所以没测出）。
    let mut akf = [0u8; 96];
    let mut afn = 0usize;
    afn = push_out(&mut akf, afn, b"AWS4");
    afn = push_out(&mut akf, afn, &cfg.secret_key[..cfg.secret_key_len]);
    let mut kdate = [0u8; 32];
    hmac_sha256(&akf[..afn], &date_stamp, &mut kdate);
    let mut kregion = [0u8; 32];
    hmac_sha256(&kdate, &cfg.region[..cfg.region_len], &mut kregion);
    let mut kservice = [0u8; 32];
    hmac_sha256(&kregion, SVC, &mut kservice);
    let mut ksign = [0u8; 32];
    hmac_sha256(&kservice, TERM, &mut ksign);

    // 签名
    let mut sig = [0u8; 32];
    hmac_sha256(&ksign, string_to_sign, &mut sig);
    let mut sig_hex = [0u8; 64];
    hex::encode(&sig, &mut sig_hex);

    // ---- 授权头 ----
    let mut auth = [0u8; 400];
    let mut an = 0usize;
    an = push_out(&mut auth, an, ALGO);
    an = push_out(&mut auth, an, b" Credential=");
    an = push_out(&mut auth, an, &cfg.access_key[..cfg.access_key_len]);
    an = push_out(&mut auth, an, b"/");
    an = push_out(&mut auth, an, &date_stamp);
    an = push_out(&mut auth, an, b"/");
    an = push_out(&mut auth, an, &cfg.region[..cfg.region_len]);
    an = push_out(&mut auth, an, b"/s3/aws4_request, SignedHeaders=");
    an = push_out(&mut auth, an, SIGNED_HDRS);
    an = push_out(&mut auth, an, b", Signature=");
    an = push_out(&mut auth, an, &sig_hex);

    // ---- 完整请求头 ----
    let mut rn = 0usize;
    rn = push_out(head_buf, rn, b"GET ");
    rn = push_out(head_buf, rn, uri);
    rn = push_out(head_buf, rn, b" HTTP/1.1\r\nHost: ");
    rn = push_out(head_buf, rn, &host_hdr[..hn]);
    rn = push_out(head_buf, rn, b"\r\nx-amz-content-sha256: ");
    rn = push_out(head_buf, rn, &payload_hash);
    rn = push_out(head_buf, rn, b"\r\nx-amz-date: ");
    rn = push_out(head_buf, rn, &amz_date);
    rn = push_out(head_buf, rn, b"\r\nAuthorization: ");
    rn = push_out(head_buf, rn, &auth[..an]);
    push_out(head_buf, rn, b"\r\nConnection: close\r\n\r\n");
    0
}

// ==================== 响应读取 ====================

/// 从 stage 解析状态码与头；返回 (code, 头是否完整, content-length)。
fn parse_status(stage: &[u8]) -> (i32, bool, i64) {
    if stage.len() < 17 || &stage[..8] != b"HTTP/1." {
        return (-1, false, -1);
    }
    let mut code = 0i32;
    for i in 0..3 {
        let c = stage[9 + i];
        if !c.is_ascii_digit() {
            return (-1, false, -1);
        }
        code = code * 10 + (c - b'0') as i32;
    }
    let Some(he) = find_sub(stage, b"\r\n\r\n") else {
        return (code, false, -1);
    };
    let mut clen: i64 = -1;
    let mut ls = 8usize;
    while ls < he {
        let mut le = ls;
        while le < he && stage[le] != b'\r' && stage[le] != b'\n' {
            le += 1;
        }
        let line = &stage[ls..le];
        if line.len() > 15 && line[..14].eq_ignore_ascii_case(b"content-length")
            && line[14] == b':'
        {
            let mut v: i64 = 0;
            let mut any = false;
            for &c in &line[15..] {
                if c == b'\r' || c == b' ' || c == b'\t' {
                    continue;
                }
                if !c.is_ascii_digit() {
                    break;
                }
                v = v * 10 + (c - b'0') as i64;
                any = true;
            }
            if any {
                clen = v;
            }
        }
        if le < he && stage.get(le) == Some(&b'\r') && stage.get(le + 1) == Some(&b'\n') {
            ls = le + 2;
        } else {
            ls = le + 1;
        }
    }
    (code, true, clen)
}

fn status_err(status: i32) -> isize {
    if status == 403 {
        R2_ERR_AUTH
    } else if status == 404 {
        R2_ERR_NOTFOUND
    } else {
        R2_ERR_HTTP
    }
}

/// 连接 + 发送已签名请求；成功返回 socket fd，失败返回负错误码。
fn open_request(cfg: &R2Config, uri: &[u8]) -> isize {
    let mut head = [0u8; 2048];
    let r = build_request(cfg, uri, &mut head);
    if r < 0 {
        return r;
    }
    let req = &head[..head_len_of(&head)];
    let host = &cfg.host[..cfg.host_len];
    let (ip, port, ok) = resolve(host, cfg.port);
    if !ok || ip == 0 {
        return R2_ERR_RESOLVE;
    }
    let fd = sock_open();
    if fd < 0 {
        return R2_ERR_CONNECT;
    }
    if sock_connect(fd, ip, port) < 0 {
        sock_close(fd);
        return R2_ERR_CONNECT;
    }
    let mut sent = 0usize;
    while sent < req.len() {
        let n = sock_send(fd, &req[sent..]);
        if n <= 0 {
            sock_close(fd);
            return R2_ERR_SEND;
        }
        sent += n as usize;
    }
    fd
}

fn head_len_of(head: &[u8; 2048]) -> usize {
    // build_request 成功时请求内必有 "\r\n\r\n" 结尾；这里定位它
    match find_sub(head, b"\r\n\r\n") {
        Some(i) => i + 4,
        None => 2048,
    }
}

/// 通用传输：读完整响应。
/// - `resp.is_some()`：内存模式，body 写入 `resp`。
/// - `out_fd >= 0`：文件模式，body 流式写入（可选 `prog` 进度指针）。
/// 成功返回 body 字节数（内存模式）/ 写入文件字节数（文件模式）；失败负值。
pub fn transact(
    cfg: &R2Config,
    uri: &[u8],
    mut resp: Option<&mut [u8]>,
    out_fd: isize,
    prog: *mut u32,
    status_out: &mut i32,
) -> isize {
    let fd = open_request(cfg, uri);
    if fd < 0 {
        return fd;
    }
    // 首块（含响应头）
    let mut stage = [0u8; 4096];
    let mut stage_len = 0usize;
    loop {
        let n = sock_recv(fd, &mut stage[stage_len..]);
        if n < 0 {
            sock_close(fd);
            return R2_ERR_RECV;
        }
        if n == 0 {
            break;
        }
        stage_len += n as usize;
        if stage_len >= stage.len() || find_sub(&stage[..stage_len], b"\r\n\r\n").is_some() {
            break;
        }
    }
    let (code, ok, _clen) = parse_status(&stage[..stage_len]);
    if !ok {
        sock_close(fd);
        return R2_ERR_BADRESP;
    }
    *status_out = code;
    if code != 200 {
        sock_close(fd);
        return status_err(code);
    }
    let he = match find_sub(&stage[..stage_len], b"\r\n\r\n") {
        Some(i) => i + 4,
        None => {
            sock_close(fd);
            return R2_ERR_BADRESP;
        }
    };
    let mut body_cnt = 0u64;
    let head_body = &stage[he..stage_len];

    // 写首块中的 body
    if let Some(rb) = resp.as_deref_mut() {
        if body_cnt as usize + head_body.len() > rb.len() {
            sock_close(fd);
            return R2_ERR_BIG;
        }
        rb[..head_body.len()].copy_from_slice(head_body);
        body_cnt += head_body.len() as u64;
    } else if out_fd >= 0 {
        if head_body.len() > 0 {
            let w = write_fd(out_fd, head_body);
            if w < 0 {
                sock_close(fd);
                return w;
            }
        }
        body_cnt += head_body.len() as u64;
        if !prog.is_null() {
            unsafe { *prog = body_cnt as u32 };
        }
    } else {
        sock_close(fd);
        return R2_ERR_CONFIG;
    }

    // 余下循环
    let mut chunk = [0u8; 4096];
    loop {
        let n = sock_recv(fd, &mut chunk);
        if n < 0 {
            sock_close(fd);
            return R2_ERR_RECV;
        }
        if n == 0 {
            break;
        }
        let m = n as usize;
        if let Some(rb) = resp.as_deref_mut() {
            let cur = body_cnt as usize;
            if cur + m > rb.len() {
                sock_close(fd);
                return R2_ERR_BIG;
            }
            rb[cur..cur + m].copy_from_slice(&chunk[..m]);
        } else if out_fd >= 0 {
            let w = write_fd(out_fd, &chunk[..m]);
            // write_fd 返回实际写入字节数（磁盘满/介质错误时为短写，永不
            // 返回负值 —— `w < 0` 是死条件）：短写必须立即报错，否则下载
            // “成功”落盘残缺文件，直到验签才失败且报错误导。
            if w != m as isize {
                sock_close(fd);
                return R2_ERR_RECV;
            }
            if !prog.is_null() {
                unsafe { *prog = body_cnt as u32 + m as u32 };
            }
        } else {
            sock_close(fd);
            return R2_ERR_CONFIG;
        }
        body_cnt += m as u64;
    }
    sock_close(fd);
    body_cnt as isize
}

/// GET Object 读入内存缓冲；成功返回 body 长度，失败负值。
pub fn s3_get(cfg: &R2Config, key: &[u8], resp: &mut [u8], status_out: &mut i32) -> isize {
    let mut uri = [0u8; 384];
    let mut un = 0usize;
    un = push_out(&mut uri, un, b"/");
    un = push_out(&mut uri, un, &cfg.bucket[..cfg.bucket_len]);
    un = push_out(&mut uri, un, b"/");
    let mut enc = [0u8; 320];
    let el = uri_encode(key, &mut enc);
    un = push_out(&mut uri, un, &enc[..el]);
    transact(cfg, &uri[..un], Some(resp), -1, core::ptr::null_mut(), status_out)
}

/// GET Object 流式写入文件；成功返回写入字节数，失败负值。
pub fn s3_download(
    cfg: &R2Config,
    key: &[u8],
    out_fd: isize,
    prog: *mut u32,
    status_out: &mut i32,
) -> isize {
    let mut uri = [0u8; 384];
    let mut un = 0usize;
    un = push_out(&mut uri, un, b"/");
    un = push_out(&mut uri, un, &cfg.bucket[..cfg.bucket_len]);
    un = push_out(&mut uri, un, b"/");
    let mut enc = [0u8; 320];
    let el = uri_encode(key, &mut enc);
    un = push_out(&mut uri, un, &enc[..el]);
    transact(cfg, &uri[..un], None, out_fd, prog, status_out)
}