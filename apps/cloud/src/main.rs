// KunxOS 云服务应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/cloud）。职责：
//   1. 读取 /data/oobe/cloud_* 配置（启用状态、账号、服务器、设备 ID、管理开关）
//   2. 经内核 TCP 栈与云服务端通信（明文 HTTP，见 cloud/PROTOCOL.md）：
//        register 注册设备 / backup 上传备份快照 / restore 从云端恢复 /
//        poll 心跳+轮询命令 / ack 确认命令执行结果
//   3. 执行设备管理命令：find（上报 IP）/ lock（远程锁定）/ wipe（擦除数据）
//   4. 全屏 GUI：状态行 + 操作按钮；锁定状态显示锁定覆盖层
//
// 服务器地址默认 10.0.2.2:8080（QEMU slirp 宿主机），可写入
// /data/oobe/cloud_server 覆盖。服务器需运行 cloud/gateway/gateway.py。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_cloud.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色（浅色主题） ====================

/// 页面基底背景（浅色渐变：顶部白 → 底部极浅灰）。
const C_BG: u32 = 0x00_F7_F8_FA;
/// 卡片/面板填充与描边（极浅灰卡片 + 浅灰描边，白底上以描边保持轮廓）。
const C_CARD: u32 = 0x00_FF_FF_FF;
const C_CARD_BORDER: u32 = 0x00_D9_DE_E6;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_WARN: u32 = 0x00_E7_4C_3C;
/// 远程锁定覆盖层背景（浅色主题下同为浅色基底）。
const C_LOCK_BG: u32 = 0x00_F7_F8_FA;
/// 彩色玻璃（强调绿）表面上的文字保持白色。
const C_ON_ACCENT: u32 = 0x00_FF_FF_FF;

const LABEL_X: i32 = 24;
const ROW_W: i32 = 492;
const ROW_H: i32 = 34;
const ROW_R: i32 = 8; // 行卡片圆角 min(ROW_H*25/100, 16..24)
const BTN_R: i32 = 16; // 按钮圆角 min(BTN_H*25/100, 16..24)

const BTN_W: i32 = 220;
const BTN_H: i32 = 66;
const BTN_X1: i32 = 40;
const BTN_X2: i32 = 280;
const BTN_Y1: i32 = 470;
const BTN_Y2: i32 = 556;
const BTN_Y3: i32 = 642;

// ==================== 配置路径 ====================

const CLOUD_DIR: &str = "/data/oobe/";
const F_ENABLED: &str = "/data/oobe/cloud_enabled";
const F_ACCOUNT: &str = "/data/oobe/cloud_account";
const F_SERVER: &str = "/data/oobe/cloud_server";
const F_DEVID: &str = "/data/oobe/cloud_device_id";
const F_CONTROL: &str = "/data/oobe/cloud_control";
const F_LOCKED: &str = "/data/oobe/locked";
const F_LOCAL_ACCOUNT: &str = "/data/oobe/local_account";

/// 默认服务器（QEMU slirp 宿主机）。
const DEFAULT_SERVER: &str = "10.0.2.2:8080";
const DEFAULT_DEVID: &str = "kunxos01";

/// 设备当前 IP（QEMU slirp 固定 10.0.2.15；真机可改 /data/oobe/cloud_ip）。
const DEVICE_IP: &str = "10.0.2.15";

// ==================== 全局状态 ====================

static mut ACCOUNT: [u8; 64] = [0u8; 64];
static mut TOKEN: [u8; 64] = [0u8; 64];
static mut SERVER: [u8; 64] = [0u8; 64];
static mut DEVID: [u8; 64] = [0u8; 64];
static mut ENABLED: bool = false;
static mut LOCKED: bool = false;
static mut CTRL_FIND: bool = false;
static mut CTRL_LOCK: bool = false;
static mut CTRL_WIPE: bool = false;

/// 状态栏文本。
static mut STATUS: [u8; 128] = [0u8; 128];
static mut STATUS_LEN: usize = 0;

/// HTTP 收发缓冲。
static mut REQ: [u8; 4096] = [0u8; 4096];
static mut RESP: [u8; 8192] = [0u8; 8192];

// ==================== 基础工具 ====================

fn set_status(s: &[u8]) {
    unsafe {
        let n = s.len().min(STATUS.len());
        STATUS[..n].copy_from_slice(&s[..n]);
        STATUS_LEN = n;
    }
}

fn append_status(s: &[u8]) {
    unsafe {
        let cur = STATUS_LEN;
        let n = s.len().min(STATUS.len() - cur);
        STATUS[cur..cur + n].copy_from_slice(&s[..n]);
        STATUS_LEN = cur + n;
    }
}

fn status_slice() -> &'static [u8] {
    unsafe { &STATUS[..STATUS_LEN] }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t' || b[s] == b'\r' || b[s] == b'\n') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r' || b[e - 1] == b'\n') {
        e -= 1;
    }
    &b[s..e]
}

fn starts_with(a: &[u8], p: &[u8]) -> bool {
    a.len() >= p.len() && a[..p.len()] == *p
}

/// 在 data 中查找 needle，返回索引；未找到返回 None。
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

/// 解析无符号十进制。
fn parse_u32(b: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    let mut any = false;
    for &c in b {
        if c < b'0' || c > b'9' {
            return None;
        }
        any = true;
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    if any {
        Some(v)
    } else {
        None
    }
}

/// FNV-1a 64 位哈希（与 oobe/gateway/worker 一致），hex 小写。
fn fnv1a_hex(data: &[u8], out: &mut [u8]) -> usize {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_0001_B3);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for i in 0..16 {
        out[i] = HEX[((h >> (60 - i * 4)) & 0xF) as usize];
    }
    16
}

/// URL 编码（保留字母数字与 -_.~，其余 %XX）。
fn urlencode(src: &[u8], dst: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut n = 0usize;
    for &c in src {
        if (c >= b'a' && c <= b'z') || (c >= b'A' && c <= b'Z') || (c >= b'0' && c <= b'9')
            || c == b'-' || c == b'_' || c == b'.' || c == b'~'
        {
            if n < dst.len() {
                dst[n] = c;
                n += 1;
            }
        } else {
            if n + 2 < dst.len() {
                dst[n] = b'%';
                dst[n + 1] = HEX[(c >> 4) as usize];
                dst[n + 2] = HEX[(c & 0xF) as usize];
                n += 3;
            }
        }
    }
    n
}

fn urldecode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < src.len() && n < dst.len() {
        let c = src[i];
        if c == b'%' && i + 2 < src.len() {
            let hi = hex_val(src[i + 1]);
            let lo = hex_val(src[i + 2]);
            if hi < 16 && lo < 16 {
                dst[n] = (hi << 4) | lo;
                n += 1;
                i += 3;
                continue;
            }
        }
        dst[n] = c;
        n += 1;
        i += 1;
    }
    n
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 16,
    }
}

// ==================== base64 ====================

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < src.len() && n + 4 <= dst.len() {
        let a = src[i];
        let b = if i + 1 < src.len() { src[i + 1] } else { 0 };
        let c = if i + 2 < src.len() { src[i + 2] } else { 0 };
        dst[n] = B64[(a >> 2) as usize];
        dst[n + 1] = B64[(((a & 0x03) << 4) | (b >> 4)) as usize];
        if i + 1 < src.len() {
            dst[n + 2] = B64[(((b & 0x0F) << 2) | (c >> 6)) as usize];
        } else {
            dst[n + 2] = b'=';
        }
        if i + 2 < src.len() {
            dst[n + 3] = B64[(c & 0x3F) as usize];
        } else {
            dst[n + 3] = b'=';
        }
        n += 4;
        i += 3;
    }
    n
}

fn b64_char_val(c: u8) -> i32 {
    match c {
        b'A'..=b'Z' => (c - b'A') as i32,
        b'a'..=b'z' => (c - b'a' + 26) as i32,
        b'0'..=b'9' => (c - b'0' + 52) as i32,
        b'+' => 62,
        b'/' => 63,
        _ => -1,
    }
}

fn b64_decode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in src {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = b64_char_val(c);
        if v < 0 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if n < dst.len() {
                dst[n] = ((acc >> bits) & 0xFF) as u8;
                n += 1;
            }
        }
    }
    n
}

// ==================== 文件操作 ====================

static mut FILE_BUF: [u8; 512] = [0u8; 512];

/// 读取整个小文件到 buf，返回长度；失败返回 -1。
fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return -1;
        }
        let mut total = 0usize;
        while total < buf.len() {
            let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr().add(total) as usize, buf.len() - total) as isize;
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        total as isize
    }
}

fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, 0x40 | 0x200 | 1); // O_CREAT | O_TRUNC | O_WRONLY
        if fd < 0 {
            return false;
        }
        let mut off = 0usize;
        while off < data.len() {
            let n = syscall3(SYS_WRITE, fd as usize, data.as_ptr().add(off) as usize, data.len() - off) as isize;
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        off == data.len()
    }
}

fn unlink(path: &str) {
    unsafe {
        let mut p = [0u8; 128];
        let n = path.len().min(p.len() - 1);
        p[..n].copy_from_slice(&path.as_bytes()[..n]);
        p[n] = 0;
        syscall3(SYS_UNLINKAT, AT_FDCWD, p.as_ptr() as usize, 0);
    }
}

/// 读取 /data/oobe/<name> 内容到全局 FILE_BUF 并 trim，返回切片。
fn read_oobe(name: &str) -> &'static [u8] {
    unsafe {
        let mut path = [0u8; 96];
        let mut n = 0usize;
        for &c in CLOUD_DIR.as_bytes() {
            path[n] = c;
            n += 1;
        }
        for &c in name.as_bytes() {
            if n < path.len() {
                path[n] = c;
                n += 1;
            }
        }
        let p = core::str::from_utf8(&path[..n]).unwrap_or("");
        let len = read_file(p, &mut FILE_BUF);
        if len <= 0 {
            return &[];
        }
        trim(&FILE_BUF[..len as usize])
    }
}

// ==================== 配置加载 ====================

/// 复制字符串到静态缓冲。
fn load_str(val: &[u8], dst: &mut [u8]) {
    let n = val.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&val[..n]);
    dst[n] = 0;
}

/// 解析 cloud_account（"user:token"）。
fn parse_account(v: &[u8]) {
    unsafe {
        if let Some(i) = find_sub(v, b":") {
            let u = trim(&v[..i]);
            let t = trim(&v[i + 1..]);
            if !u.is_empty() && !t.is_empty() {
                load_str(u, &mut ACCOUNT);
                load_str(t, &mut TOKEN);
                return;
            }
        }
        ACCOUNT[0] = 0;
        TOKEN[0] = 0;
    }
}

/// 解析 cloud_control（"enabled:1,find:1,lock:0,wipe:0"）。
fn parse_control(v: &[u8]) {
    unsafe {
        CTRL_FIND = false;
        CTRL_LOCK = false;
        CTRL_WIPE = false;
        for part in v.split(|&c| c == b',') {
            if let Some(i) = find_sub(part, b":") {
                let k = trim(&part[..i]);
                let val = trim(&part[i + 1..]);
                let on = val == b"1";
                if k == b"find" {
                    CTRL_FIND = on;
                } else if k == b"lock" {
                    CTRL_LOCK = on;
                } else if k == b"wipe" {
                    CTRL_WIPE = on;
                }
            }
        }
    }
}

fn load_config() {
    unsafe {
        ENABLED = read_oobe("cloud_enabled") == b"1";
        parse_account(read_oobe("cloud_account"));
        let srv = read_oobe("cloud_server");
        if srv.is_empty() {
            load_str(DEFAULT_SERVER.as_bytes(), &mut SERVER);
        } else {
            load_str(srv, &mut SERVER);
        }
        let dev = read_oobe("cloud_device_id");
        if dev.is_empty() {
            load_str(DEFAULT_DEVID.as_bytes(), &mut DEVID);
        } else {
            load_str(dev, &mut DEVID);
        }
        parse_control(read_oobe("cloud_control"));
        LOCKED = read_oobe("locked") == b"1";
    }
}

fn save_enabled(on: bool) {
    unsafe {
        if !write_file(F_ENABLED, if on { b"1" } else { b"0" }) {
            print("[cloud] 写入 F_ENABLED 失败\n");
            show_error("写入设置失败".as_bytes());
        }
        ENABLED = on;
    }
}

/// 启用云服务：若无账号则用本地账号 + 派生令牌补齐，并落盘。
fn ensure_credentials() {
    unsafe {
        if ACCOUNT[0] == 0 || TOKEN[0] == 0 {
            let la = read_oobe("local_account");
            let mut acc = [0u8; 64];
            let mut n = 0usize;
            let user = if la.is_empty() { b"kunxos".as_slice() } else { la };
            for &c in user {
                if n < acc.len() {
                    acc[n] = c;
                    n += 1;
                }
            }
            let mut seed = [0u8; 96];
            let mut s = 0usize;
            for &c in &acc[..n] {
                seed[s] = c;
                s += 1;
            }
            seed[s] = b':';
            s += 1;
            for &c in DEVID.iter() {
                if c == 0 {
                    break;
                }
                seed[s] = c;
                s += 1;
            }
            let mut tok = [0u8; 16];
            let tl = fnv1a_hex(&seed[..s], &mut tok);
            load_str(&acc[..n], &mut ACCOUNT);
            load_str(&tok[..tl], &mut TOKEN);
            // 写回 /data/oobe/cloud_account = user:token
            let mut line = [0u8; 96];
            let mut m = 0usize;
            for &c in &acc[..n] {
                line[m] = c;
                m += 1;
            }
            line[m] = b':';
            m += 1;
            for &c in &tok[..tl] {
                line[m] = c;
                m += 1;
            }
            if !write_file(F_ACCOUNT, &line[..m]) {
                print("[cloud] 写入 F_ACCOUNT 失败\n");
                show_error("凭证保存失败".as_bytes());
            }
        }
        // 确保 device_id 已落盘
        let dev = read_oobe("cloud_device_id");
        if dev.is_empty() {
            if !write_file(F_DEVID, cstr(&DEVID)) {
                print("[cloud] 写入 F_DEVID 失败\n");
            }
        }
    }
}

// ==================== HTTP 客户端 ====================

/// 解析 "host[:port]" → (ip, port)。host 支持点分十进制 IP 或域名
/// （域名经内核 DNS 系统调用解析，QEMU slirp → 宿主 DNS）。
/// 返回 false 表示地址无法解析。
fn parse_server(srv: &[u8]) -> (u32, u16, bool) {
    let mut host = srv;
    let mut port: u16 = 8080;
    if let Some(i) = find_sub(srv, b":") {
        host = &srv[..i];
        if let Some(p) = parse_u32(&srv[i + 1..]) {
            if p <= 65535 {
                port = p as u16;
            }
        }
    }
    if let Some(ip) = ip_parse(host) {
        return (ip, port, true);
    }
    // 域名：走内核 DNS 解析（超时约数秒）
    let ip = unsafe { dns_resolve(host) };
    if ip != 0 {
        (ip, port, true)
    } else {
        (0, 0, false)
    }
}

/// parse_server 失败的原因提示：形如 IP 的字符串 → 地址无效；
/// 含字母的主机名 → 域名解析失败（此前一律报"服务器地址无效"，
/// 把 DNS 失败与非法地址混为一谈）。
fn server_fail_hint(srv: &[u8]) -> &'static [u8] {
    let host = if let Some(i) = find_sub(srv, b":") { &srv[..i] } else { srv };
    let looks_ip = !host.is_empty() && host.iter().all(|&c| c.is_ascii_digit() || c == b'.');
    if looks_ip {
        "服务器地址无效".as_bytes()
    } else {
        "域名解析失败".as_bytes()
    }
}

/// 发送 HTTP 请求（HTTP/1.0 + Connection: close），把完整响应读入 resp。
/// 返回响应总字节数；失败返回负值。
fn http_request(ip: u32, port: u16, req: &[u8], resp: &mut [u8]) -> isize {
    unsafe {
        let fd = sock_open();
        if fd < 0 {
            return -1;
        }
        if sock_connect(fd, ip, port) < 0 {
            sock_close(fd);
            return -2;
        }
        let mut sent = 0usize;
        while sent < req.len() {
            let n = sock_send(fd, &req[sent..]);
            if n <= 0 {
                sock_close(fd);
                return -3;
            }
            sent += n as usize;
        }
        let mut total = 0usize;
        while total < resp.len() {
            let n = sock_recv(fd, &mut resp[total..]);
            if n < 0 {
                // 接收错误（区别于对端正常关闭 n == 0）
                sock_close(fd);
                return -4;
            }
            if n == 0 {
                break;
            }
            total += n as usize;
        }
        sock_close(fd);
        LAST_RESP_LEN = total;
        total as isize
    }
}

/// 最近一次 HTTP 响应的有效长度（供错误码解析复用）。
static mut LAST_RESP_LEN: usize = 0;

/// 组装请求头并返回请求切片（写入全局 REQ）。
fn build_request(method: &[u8], path: &[u8], body: &[u8]) -> &'static [u8] {
    unsafe {
        let mut n = 0usize;
        // 边界宏：REQ 写满即停止追加（防御性；正常路径由调用方预留头空间）。
        macro_rules! put {
            ($c:expr) => {{
                let c: u8 = $c;
                if n < REQ.len() {
                    REQ[n] = c;
                    n += 1;
                }
            }};
        }
        // method path HTTP/1.0\r

        for &c in method {
            put!(c);
        }
        put!(b' ');
        for &c in path {
            put!(c);
        }
        for &c in b" HTTP/1.0\r\nHost: " {
            put!(c);
        }
        for &c in SERVER.iter() {
            if c == 0 {
                break;
            }
            put!(c);
        }
        for &c in b"\r\nContent-Length: " {
            put!(c);
        }
        let mut num = [0u8; 8];
        let nl = fmt_dec(body.len() as u64, &mut num);
        for &c in &num[..nl] {
            put!(c);
        }
        for &c in b"\r\nConnection: close\r\n\r\n" {
            put!(c);
        }
        for &c in body {
            put!(c);
        }
        &REQ[..n]
    }
}

/// 十进制格式化（无前导零），返回长度。
fn fmt_dec(mut v: u64, buf: &mut [u8]) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut pos = buf.len();
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = buf.len() - pos;
    buf.copy_within(pos.., 0);
    n
}

/// 取响应 body（跳过 "HTTP/1.x ...\r\n\r\n"）。返回 body 切片。
fn resp_body(resp: &[u8]) -> &[u8] {
    if let Some(i) = find_sub(resp, b"\r\n\r\n") {
        let s = i + 4;
        if s <= resp.len() {
            return &resp[s..];
        }
    }
    resp
}

/// 响应首行是否 OK。
fn resp_ok(body: &[u8]) -> bool {
    starts_with(body, b"OK")
}

/// 从最近一次响应解析 `ERR:<code>` 的中文提示。
/// 返回 `(提示语, 是否服务器业务错误)`；无 ERR 码（网络层失败）返回 None。
fn resp_err_hint() -> Option<&'static [u8]> {
    unsafe {
        let len = LAST_RESP_LEN;
        if len == 0 || len > RESP.len() {
            return None;
        }
        let b = resp_body(&RESP[..len]);
        if !starts_with(b, b"ERR:") {
            return None;
        }
        let rest = &b[4..];
        let end = rest.iter().position(|&c| c == b':' || c == b'\n').unwrap_or(rest.len());
        let code = &rest[..end];
        let hint: &'static [u8] = if code == b"bad_auth" {
            "认证失败（账号或令牌无效）".as_bytes()
        } else if code == b"exists" {
            "设备已被其他账号注册".as_bytes()
        } else if code == b"not_found" {
            "云服务资源不存在".as_bytes()
        } else if code == b"bad_request" {
            "请求格式错误".as_bytes()
        } else if code == b"internal" {
            "服务器内部错误".as_bytes()
        } else {
            "服务器返回错误".as_bytes()
        };
        Some(hint)
    }
}

// ==================== 云 API ====================

/// POST /api/register。返回 0 成功、1 设备已被其他账号占用、-1 网络/服务器错误。
fn api_register() -> i32 {
    unsafe {
        let mut body = [0u8; 320];
        let mut n = 0usize;
        let mut enc = [0u8; 96];
        for &c in b"u=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&ACCOUNT), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&t=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&TOKEN), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&d=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&DEVID), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&n=KunxOS" {
            body[n] = c;
            n += 1;
        }
        let req = build_request(b"POST", b"/api/register", &body[..n]);
        let (ip, port, ok) = parse_server(cstr(&SERVER));
        if !ok {
            set_status(server_fail_hint(cstr(&SERVER)));
            return -1;
        }
        let total = http_request(ip, port, req, &mut RESP);
        if total <= 0 {
            return -1;
        }
        let b = resp_body(&RESP[..total as usize]);
        if resp_ok(b) {
            return 0;
        }
        if starts_with(b, b"ERR:exists") {
            return 1;
        }
        -1
    }
}

/// POST /api/backup：上传 /data/oobe 全部文件快照。
fn api_backup() -> bool {
    unsafe {
        let mut body = [0u8; 4096];
        let mut n = 0usize;
        // 头 4 行：u= / t= / d= / n= （n 后补）
        for &c in b"u=" {
            body[n] = c;
            n += 1;
        }
        let mut enc = [0u8; 96];
        let el = urlencode(cstr(&ACCOUNT), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"\nt=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&TOKEN), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"\nd=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&DEVID), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        // 第一遍：统计将发送的文件数（与第二遍同判据：read_file > 0，
        // 空文件跳过不发 F 行，避免 n 与 F 行数不一致）
        let mut count = 0u32;
        for name in BACKUP_FILES {
            let p = oobe_path(name);
            let mut fbuf = [0u8; 512];
            let len = read_file(core::str::from_utf8(p).unwrap_or(""), &mut fbuf);
            if len > 0 {
                count += 1;
            }
        }
        // 写 n=<count> 行（直接写对，无预留回填）
        let mut num = [0u8; 8];
        let nl = fmt_dec(count as u64, &mut num);
        for &c in b"\nn=" {
            body[n] = c;
            n += 1;
        }
        for &c in &num[..nl] {
            body[n] = c;
            n += 1;
        }
        body[n] = b'\n';
        n += 1;
        // 第二遍：写 F|<b64 path>|<b64 data>

        for name in BACKUP_FILES {
            let path = oobe_path(name);
            let pn = path.len();
            let p = core::str::from_utf8(path).unwrap_or("");
            let mut fbuf = [0u8; 512];
            let len = read_file(p, &mut fbuf);
            if len <= 0 {
                continue;
            }
            // F|<b64 path>|<b64 data>

            let mut eb = [0u8; 128];
            let el = b64_encode(&path[..pn], &mut eb);
            let mut db = [0u8; 768];
            let dl = b64_encode(&fbuf[..len as usize], &mut db);
            let mut line = [0u8; 1024];
            let mut ln = 0usize;
            line[ln] = b'F';
            ln += 1;
            line[ln] = b'|';
            ln += 1;
            line[ln..ln + el].copy_from_slice(&eb[..el]);
            ln += el;
            line[ln] = b'|';
            ln += 1;
            line[ln..ln + dl].copy_from_slice(&db[..dl]);
            ln += dl;
            line[ln] = b'\n';
            ln += 1;
            // ★ 预留 512B 请求头空间：build_request 会把「头 + body」整体写入
            // REQ[4096]，body 占满 4096 时头部追加必越界（REQ[n] panic）。
            if n + ln > body.len() - 512 {
                break;
            }
            body[n..n + ln].copy_from_slice(&line[..ln]);
            n += ln;
        }
        // 请求体总长 = 累积写入位置
        let body_len = n;

        let req = build_request(b"POST", b"/api/backup", &body[..body_len]);
        let (ip, port, ok) = parse_server(cstr(&SERVER));
        if !ok {
            set_status(server_fail_hint(cstr(&SERVER)));
            return false;
        }
        let total = http_request(ip, port, req, &mut RESP);
        if total <= 0 {
            return false;
        }
        let b = resp_body(&RESP[..total as usize]);
        let ok = resp_ok(b);
        if ok {
            // 状态显示 OK:<n>
            let mut s = [0u8; 32];
            let mut sn = 0usize;
            for &c in "已备份 ".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            let nl = fmt_dec(count as u64, &mut num);
            for &c in &num[..nl] {
                s[sn] = c;
                sn += 1;
            }
            for &c in " 个文件".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            set_status(&s[..sn]);
        }
        ok
    }
}

/// GET /api/restore：拉取快照并写回 /data/oobe。
fn api_restore() -> bool {
    unsafe {
        let mut path = [0u8; 512];
        let mut n = 0usize;
        for &c in b"/api/restore?u=" {
            path[n] = c;
            n += 1;
        }
        let mut enc = [0u8; 96];
        let el = urlencode(cstr(&ACCOUNT), &mut enc);
        path[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&t=" {
            path[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&TOKEN), &mut enc);
        path[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&d=" {
            path[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&DEVID), &mut enc);
        path[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        let req = build_request(b"GET", &path[..n], b"");
        let (ip, port, ok) = parse_server(cstr(&SERVER));
        if !ok {
            set_status(server_fail_hint(cstr(&SERVER)));
            return false;
        }
        let total = http_request(ip, port, req, &mut RESP);
        if total <= 0 {
            return false;
        }
        let body = resp_body(&RESP[..total as usize]);
        if !resp_ok(body) {
            return false;
        }
        // body: OK:<n>，随后每行格式 `F|<b64>|<b64>`（恢复文件清单）。
        let mut restored = 0u32;
        let mut failed = 0u32;
        for line in body.split(|&c| c == b'\n') {
            if !starts_with(line, b"F|") {
                continue;
            }
            let rest = &line[2..];
            let Some(p1) = find_sub(rest, b"|") else { continue };
            let mut path_b = [0u8; 96];
            let mut data_b = [0u8; 512];
            let pl = b64_decode(&rest[..p1], &mut path_b);
            let dl = b64_decode(&rest[p1 + 1..], &mut data_b);
            if pl == 0 {
                continue;
            }
            let p = core::str::from_utf8(&path_b[..pl]).unwrap_or("");
            // 安全校验：只允许恢复到 /data/oobe/
            if !starts_with(p.as_bytes(), CLOUD_DIR.as_bytes()) {
                continue;
            }
            if write_file(p, &data_b[..dl]) {
                restored += 1;
            } else {
                failed += 1;
            }
        }
        let mut s = [0u8; 40];
        let mut sn = 0usize;
        for &c in "已恢复 ".as_bytes() {
            s[sn] = c;
            sn += 1;
        }
        let mut num = [0u8; 8];
        let nl = fmt_dec(restored as u64, &mut num);
        for &c in &num[..nl] {
            s[sn] = c;
            sn += 1;
        }
        for &c in " 个文件".as_bytes() {
            s[sn] = c;
            sn += 1;
        }
        if failed > 0 {
            for &c in "，".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            let nl = fmt_dec(failed as u64, &mut num);
            for &c in &num[..nl] {
                s[sn] = c;
                sn += 1;
            }
            for &c in " 个失败".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            show_error("部分文件恢复失败".as_bytes());
        }
        set_status(&s[..sn]);
        // 恢复后重载配置
        load_config();
        true
    }
}

/// 记录 RESP 实际长度供 do_poll 复用。
static mut RESP_LEN: usize = 0;

/// POST /api/poll：心跳 + 拉取命令。返回命令行数；失败返回负值。
fn api_poll() -> i32 {
    unsafe {
        let mut body = [0u8; 320];
        let mut n = 0usize;
        let mut enc = [0u8; 96];
        for &c in b"u=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&ACCOUNT), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&t=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&TOKEN), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&d=" {
            body[n] = c;
            n += 1;
        }
        let el = urlencode(cstr(&DEVID), &mut enc);
        body[n..n + el].copy_from_slice(&enc[..el]);
        n += el;
        for &c in b"&ip=" {
            body[n] = c;
            n += 1;
        }
        for &c in DEVICE_IP.as_bytes() {
            body[n] = c;
            n += 1;
        }
        let req = build_request(b"POST", b"/api/poll", &body[..n]);
        let (ip, port, ok) = parse_server(cstr(&SERVER));
        if !ok {
            return -1;
        }
        let total = http_request(ip, port, req, &mut RESP);
        if total <= 0 {
            return -1;
        }
        RESP_LEN = total as usize;
        let b = resp_body(&RESP[..total as usize]);
        if !resp_ok(b) {
            return -1;
        }
        let mut cmds = 0;
        for line in b.split(|&c| c == b'\n') {
            if starts_with(line, b"C|") {
                cmds += 1;
            }
        }
        cmds
    }
}

/// POST /api/ack：确认命令结果。
fn api_ack(cmd_id: u32, result: &[u8]) -> bool {
    // 有界追加：ACCOUNT/DEVID/result 经 urlencode 最坏 ~96B/段，极端配置
    // 下合计可超过 256B body —— 无守卫 copy_from_slice 即 index panic。
    fn put(dst: &mut [u8], n: &mut usize, s: &[u8]) {
        let room = dst.len().saturating_sub(*n);
        let k = s.len().min(room);
        dst[*n..*n + k].copy_from_slice(&s[..k]);
        *n += k;
    }
    unsafe {
        let mut body = [0u8; 256];
        let mut n = 0usize;
        let mut enc = [0u8; 96];
        put(&mut body, &mut n, b"u=");
        let el = urlencode(cstr(&ACCOUNT), &mut enc);
        put(&mut body, &mut n, &enc[..el]);
        put(&mut body, &mut n, b"&t=");
        let el = urlencode(cstr(&TOKEN), &mut enc);
        put(&mut body, &mut n, &enc[..el]);
        put(&mut body, &mut n, b"&d=");
        let el = urlencode(cstr(&DEVID), &mut enc);
        put(&mut body, &mut n, &enc[..el]);
        put(&mut body, &mut n, b"&c=");
        let mut num = [0u8; 8];
        let nl = fmt_dec(cmd_id as u64, &mut num);
        put(&mut body, &mut n, &num[..nl]);
        put(&mut body, &mut n, b"&r=");
        let el = urlencode(result, &mut enc);
        put(&mut body, &mut n, &enc[..el]);
        let req = build_request(b"POST", b"/api/ack", &body[..n]);
        let (ip, port, ok) = parse_server(cstr(&SERVER));
        if !ok {
            return false;
        }
        let total = http_request(ip, port, req, &mut RESP);
        if total <= 0 {
            return false;
        }
        resp_ok(resp_body(&RESP[..total as usize]))
    }
}

/// cstr 辅助：取 NUL 结尾缓冲的有效切片。
fn cstr(b: &[u8]) -> &[u8] {
    let mut n = 0;
    while n < b.len() && b[n] != 0 {
        n += 1;
    }
    &b[..n]
}

// ==================== 备份文件清单 ====================

/// 拼 "/data/oobe/<name>" 路径到静态缓冲，返回切片（备份循环内串行使用）。
static mut OOBE_PATH_BUF: [u8; 96] = [0u8; 96];
fn oobe_path(name: &str) -> &'static [u8] {
    unsafe {
        let mut pn = 0usize;
        for &c in CLOUD_DIR.as_bytes() {
            OOBE_PATH_BUF[pn] = c;
            pn += 1;
        }
        for &c in name.as_bytes() {
            if pn < OOBE_PATH_BUF.len() {
                OOBE_PATH_BUF[pn] = c;
                pn += 1;
            }
        }
        &OOBE_PATH_BUF[..pn]
    }
}

/// /data/oobe 下参与备份的文件（OOBE 写入的全部配置）。
const BACKUP_FILES: [&str; 17] = [
    "lang",
    "modules",
    "modules_removed",
    "timezone",
    "theme",
    "agreement",
    "local_account",
    "cloud_enabled",
    "cloud_account",
    "cloud_control",
    "cloud_server",
    "cloud_device_id",
    "parental_enabled",
    "parental_control",
    "security_type",
    "security_hash",
    "duress_hash",
];

// ==================== 命令执行 ====================

/// 擦除设备数据：删除 /data/oobe 全部内容（含 .done），设备回到首次启动。
fn do_wipe() {
    const ALL: [&str; 20] = [
        "lang", "modules", "modules_removed", "timezone", "theme", "agreement",
        "local_account", "cloud_enabled", "cloud_account", "cloud_control",
        "cloud_server", "cloud_device_id", "parental_enabled", "parental_control",
        "security_type", "security_hash", "duress_hash", "duress_action",
        "locked", ".done",
    ];
    for name in ALL {
        let mut path = [0u8; 96];
        let mut n = 0usize;
        for &c in CLOUD_DIR.as_bytes() {
            path[n] = c;
            n += 1;
        }
        for &c in name.as_bytes() {
            if n < path.len() {
                path[n] = c;
                n += 1;
            }
        }
        let p = unsafe { core::str::from_utf8(&path[..n]) }.unwrap_or("");
        unlink(p);
    }
}

/// 处理一条命令（轮询响应 body 中的 C|<id>|<type>|<payload> 行）。
fn exec_command(line: &[u8]) {
    // C|<id>|<type>|<payload>
    let rest = &line[2..];
    let Some(p1) = find_sub(rest, b"|") else { return };
    let Some(p2) = find_sub(&rest[p1 + 1..], b"|") else { return };
    let id = parse_u32(&rest[..p1]);
    let Some(id) = id else { return };
    let ctype = &rest[p1 + 1..p1 + 1 + p2];
    let payload = &rest[p1 + 1 + p2 + 1..];
    let mut pbuf = [0u8; 64];
    let _pl = urldecode(payload, &mut pbuf); // payload 暂供日志/扩展

    print("[cloud] cmd id=");
    print_dec(id as u64);
    print(" type=");
    print(unsafe { core::str::from_utf8(ctype) }.unwrap_or(""));
    print("\n");

    if ctype == b"find" {
        // 上报 IP（服务端以 ack 结果刷新设备 ip/last_seen）
        let mut r = [0u8; 32];
        let mut n = 0usize;
        for &c in b"ip=" {
            r[n] = c;
            n += 1;
        }
        for &c in DEVICE_IP.as_bytes() {
            r[n] = c;
            n += 1;
        }
        if api_ack(id, &r[..n]) {
            print("[cloud] find 已上报\n");
            set_status("查找设备：已上报位置".as_bytes());
        } else {
            print("[cloud] find ack 失败\n");
            show_error("位置上报失败".as_bytes());
        }
    } else if ctype == b"lock" {
        unsafe {
            LOCKED = true;
        }
        if !write_file(F_LOCKED, b"1") {
            print("[cloud] 写入 F_LOCKED 失败\n");
        }
        let r = b"done";
        if api_ack(id, r) {
            print("[cloud] lock：已锁定\n");
            set_status("远程锁定：设备已锁定".as_bytes());
        } else {
            print("[cloud] lock ack 失败\n");
            show_error("锁定确认失败".as_bytes());
        }
    } else if ctype == b"wipe" {
        do_wipe();
        let r = b"wiped";
        if api_ack(id, r) {
            print("[cloud] wipe：已擦除\n");
            set_status("远程擦除：数据已清除".as_bytes());
        } else {
            print("[cloud] wipe ack 失败\n");
            show_error("擦除确认失败".as_bytes());
        }
    }
}

/// 轮询并执行所有待处理命令。
fn do_poll() -> i32 {
    unsafe {
        let n = api_poll();
        if n < 0 {
            // 区分服务器业务错误与网络不可达。
            match resp_err_hint() {
                Some(h) => set_status(h),
                None => set_status("轮询失败（服务器不可达）".as_bytes()),
            }
            return -1;
        }
        // 重新读取 RESP（api_poll 之后 RESP 仍保留响应）
        let body = resp_body(&RESP[..RESP_LEN]);
        let mut executed = 0;
        for line in body.split(|&c| c == b'\n') {
            if starts_with(line, b"C|") {
                exec_command(line);
                executed += 1;
            }
        }
        if n == 0 {
            set_status("检查命令：无待处理命令".as_bytes());
        } else {
            let mut s = [0u8; 32];
            let mut sn = 0usize;
            for &c in "已执行 ".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            let mut num = [0u8; 8];
            let nl = fmt_dec(executed as u64, &mut num);
            for &c in &num[..nl] {
                s[sn] = c;
                sn += 1;
            }
            for &c in " 条命令".as_bytes() {
                s[sn] = c;
                sn += 1;
            }
            set_status(&s[..sn]);
        }
        executed
    }
}

// ==================== 渲染 ====================

fn render() {
    unsafe {
        draw_gradient_bg(C_BG, glass_dark());
        draw_title_bar("云服务".as_bytes());

        if LOCKED {
            render_lock();
            draw_sys_nav();
            return;
        }

        // 状态卡片
        let mut y = 64i32;
        row(y, "云服务".as_bytes(), if ENABLED { "已启用".as_bytes() } else { "未启用".as_bytes() }, if ENABLED { C_ACCENT } else { C_WARN });
        y += ROW_H + 8;
        row(y, "账号".as_bytes(), cstr(&ACCOUNT), C_TEXT);
        y += ROW_H + 8;
        row(y, "服务器".as_bytes(), cstr(&SERVER), C_TEXT);
        y += ROW_H + 8;
        row(y, "设备ID".as_bytes(), cstr(&DEVID), C_TEXT);
        y += ROW_H + 8;

        // 设备管理开关
        let mut ctrl = [0u8; 32];
        let mut cn = 0usize;
        for &c in "查找:".as_bytes() {
            ctrl[cn] = c;
            cn += 1;
        }
        ctrl[cn] = if CTRL_FIND { b'1' } else { b'0' };
        cn += 1;
        for &c in " 锁定:".as_bytes() {
            ctrl[cn] = c;
            cn += 1;
        }
        ctrl[cn] = if CTRL_LOCK { b'1' } else { b'0' };
        cn += 1;
        for &c in " 擦除:".as_bytes() {
            ctrl[cn] = c;
            cn += 1;
        }
        ctrl[cn] = if CTRL_WIPE { b'1' } else { b'0' };
        cn += 1;
        row(y, "设备管理".as_bytes(), &ctrl[..cn], C_DIM);
        y += ROW_H + 8;

        // 状态行
        draw_text(LABEL_X, y + 6, "状态".as_bytes(), C_DIM);
        draw_text(LABEL_X + 96, y + 6, status_slice(), C_TEXT);

        // 按钮
        btn(BTN_X1, BTN_Y1, if ENABLED { "立即同步" } else { "注册设备" }.as_bytes());
        btn(BTN_X2, BTN_Y1, "检查命令".as_bytes());
        btn(BTN_X1, BTN_Y2, "从云端恢复".as_bytes());
        btn(BTN_X2, BTN_Y2, if ENABLED { "停用云服务" } else { "启用云服务" }.as_bytes());

        // 提示
        draw_text(LABEL_X, BTN_Y3 + 8, "服务器需运行 cloud/gateway/gateway.py".as_bytes(), C_DIM);
        draw_text(LABEL_X, BTN_Y3 + 30, "管理后台: http://<服务器>/admin".as_bytes(), C_DIM);

        draw_sys_nav();
    }
}

fn row(y: i32, label: &[u8], value: &[u8], vcolor: u32) {
    draw_glass(LABEL_X, y, ROW_W, ROW_H, ROW_R);
    draw_text(LABEL_X + 12, y + (ROW_H - 8) / 2, label, C_DIM);
    draw_text(LABEL_X + 120, y + (ROW_H - 8) / 2, value, vcolor);
}

fn btn(x: i32, y: i32, label: &[u8]) {
    draw_glass_style(x, y, BTN_W, BTN_H, BTN_R, &STYLE_ACCENT);
    let tx = x + (BTN_W - text_width(label)) / 2;
    // 强调绿玻璃上保持白字。
    draw_text(tx, y + (BTN_H - 8) / 2, label, C_ON_ACCENT);
}

fn render_lock() {
    draw_gradient_bg(C_LOCK_BG, glass_dark());
    draw_center(360, "设备已被远程锁定".as_bytes(), C_WARN);
    draw_center(392, "输入云服务账号后可在服务器解除".as_bytes(), C_DIM);
    // 解锁按钮
    let bw = 200i32;
    let bx = (DISPLAY_W as i32 - bw) / 2;
    let by = 460i32;
    let bh = 64i32;
    draw_glass_style(bx, by, bw, bh, (bh * 25 / 100).min(16), &STYLE_ACCENT);
    let tx = bx + (bw - text_width("解锁".as_bytes())) / 2;
    // 强调绿玻璃上保持白字。
    draw_text(tx, by + (bh - 8) / 2, "解锁".as_bytes(), C_ON_ACCENT);
}

fn unlock_hit(x: i32, y: i32) -> bool {
    let bw = 200i32;
    let bx = (DISPLAY_W as i32 - bw) / 2;
    let by = 460i32;
    let bh = 64i32;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

fn btn_hit(x: i32, y: i32, bx: i32, by: i32) -> bool {
    x >= bx && x < bx + BTN_W && y >= by && y < by + BTN_H
}

// ==================== 操作 ====================

fn do_sync() {
    print("[cloud] 同步开始\n");
    set_status("同步中...".as_bytes());
    render();
    flip();
    cursor_overlay_redraw();
    ensure_credentials();
    let rc = api_register();
    if rc == 1 {
        set_status("设备已被其他账号注册".as_bytes());
        return;
    }
    if rc != 0 {
        // 区分服务器业务错误（bad_auth 等）与网络不可达。
        match resp_err_hint() {
            Some(h) => set_status(h),
            None => set_status("注册失败（服务器不可达）".as_bytes()),
        }
        return;
    }
    print("[cloud] 注册成功\n");
    if !api_backup() {
        match resp_err_hint() {
            Some(h) => set_status(h),
            None => set_status("备份失败（服务器不可达）".as_bytes()),
        }
        return;
    }
    print("[cloud] 备份成功\n");
}

fn do_restore_op() {
    print("[cloud] 恢复开始\n");
    set_status("恢复中...".as_bytes());
    render();
    flip();
    cursor_overlay_redraw();
    ensure_credentials();
    if !api_restore() {
        let hint = resp_err_hint().unwrap_or("服务器不可达".as_bytes());
        set_status(hint);
        show_error(hint);
        print("[cloud] 恢复失败\n");
        return;
    }
    print("[cloud] 恢复完成\n");
}

fn do_toggle() {
    unsafe {
        if ENABLED {
            save_enabled(false);
            set_status("云服务已停用".as_bytes());
            print("[cloud] 已停用\n");
        } else {
            ensure_credentials();
            save_enabled(true);
            set_status("云服务已启用，请点击注册设备".as_bytes());
            print("[cloud] 已启用\n");
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[cloud] 正在启动\n");

    if !fb_init() {
        print("[cloud] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }

    load_config();
    print("[cloud] enabled=");
    print(unsafe { if ENABLED { "1" } else { "0" } });
    print(" account=");
    print(unsafe { core::str::from_utf8(cstr(&ACCOUNT)) }.unwrap_or(""));
    print(" server=");
    print(unsafe { core::str::from_utf8(cstr(&SERVER)) }.unwrap_or(""));
    print(" devid=");
    print(unsafe { core::str::from_utf8(cstr(&DEVID)) }.unwrap_or(""));
    print("\n");

    set_status("就绪（点击注册设备或检查命令）".as_bytes());
    render();
    flip();
    cursor_overlay_redraw();
    kit::font_init_checked();
    render();
    flip();
    cursor_overlay_redraw();
    print("[cloud] 已就绪\n");

    loop {
        let pr = poll_input();
        let tap = pr.tap;
        let moved = pr.moved;
        if moved {
            // 指针移动：overlay 快速路径，不重绘整屏。
            cursor_overlay_move();
        }
        if pr.nav != NavAction::None {
            print("[nav] system nav -> exit\n");
            unsafe { exit(nav_exit_code(pr.nav)); }
        }
        if let Some((x, y)) = tap {
            print("[cloud] tap (");
            print_dec(x as u64);
            print(",");
            print_dec(y as u64);
            print(")\n");
            if back_hit(x, y) {
                print("[cloud] back -> exit\n");
                break;
            }
            if unsafe { LOCKED } {
                if unlock_hit(x, y) {
                    unsafe { LOCKED = false; }
                    if !write_file(F_LOCKED, b"0") {
                        print("[cloud] 写入 F_LOCKED 失败\n");
                        show_error("解锁状态保存失败".as_bytes());
                    }
                    set_status("设备已解锁".as_bytes());
                    render();
                    flip();
                    cursor_overlay_redraw();
                }
                continue;
            }
            if btn_hit(x, y, BTN_X1, BTN_Y1) {
                do_sync();
            } else if btn_hit(x, y, BTN_X2, BTN_Y1) {
                print("[cloud] 轮询开始\n");
                set_status("检查命令中...".as_bytes());
                render();
                flip();
                cursor_overlay_redraw();
                do_poll();
            } else if btn_hit(x, y, BTN_X1, BTN_Y2) {
                do_restore_op();
            } else if btn_hit(x, y, BTN_X2, BTN_Y2) {
                do_toggle();
            }
            render();
            flip();
            cursor_overlay_redraw();
        }
        if poll_tty_esc() {
            print("[cloud] ESC -> exit\n");
            break;
        }
        sleep_ms(16);
    }

    print("[cloud] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",
    "    mov x1, x2",
    "    bl k_main",
    "    mov x8, #93",
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: cloud ");
        if let Some(loc) = info.location() {
            write_fd(2, loc.file().as_bytes());
            write_fd(2, b":");
            let mut num = [0u8; 8];
            let nl = fmt_dec(loc.line() as u64, &mut num);
            write_fd(2, &num[..nl]);
        }
        write_fd(2, b"\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
