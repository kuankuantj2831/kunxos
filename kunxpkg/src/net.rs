//! 网络传输层（远程软件源用）：TCP socket + DNS，经内核 Linux 兼容层系统调用。
//! 与 apps/cloud 使用的 `shared/app_kit.rs` 同一套协议（SYS_SOCKET 等）。

use core::arch::asm;

// ==================== 系统调用号 ====================

const SYS_SOCKET: usize = 198;
const SYS_CONNECT: usize = 203;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SHUTDOWN: usize = 210;
const SYS_DNS_RESOLVE: usize = 403;

/// AF_INET / SOCK_STREAM（AArch64 Linux 常量）。
pub const AF_INET: usize = 2;
pub const SOCK_STREAM: usize = 1;

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, in("a5") a6, options(nostack));
    ret
}

/// 打开 TCP 套接字；失败返回负值。
pub unsafe fn sock_open() -> isize {
    syscall3(SYS_SOCKET, AF_INET, SOCK_STREAM, 0) as isize
}

/// 连接 `ip:port`（ip 主机字节序）。失败返回负值。
pub unsafe fn sock_connect(fd: isize, ip: u32, port: u16) -> isize {
    let mut sa = [0u8; 8];
    sa[0] = AF_INET as u8;
    sa[1] = 0;
    sa[2] = (port >> 8) as u8;
    sa[3] = (port & 0xFF) as u8;
    sa[4] = (ip >> 24) as u8;
    sa[5] = ((ip >> 16) & 0xFF) as u8;
    sa[6] = ((ip >> 8) & 0xFF) as u8;
    sa[7] = (ip & 0xFF) as u8;
    syscall2(SYS_CONNECT, fd as usize, sa.as_ptr() as usize) as isize
}

/// 发送；失败返回负值。
pub unsafe fn sock_send(fd: isize, data: &[u8]) -> isize {
    syscall6(
        SYS_SENDTO,
        fd as usize,
        data.as_ptr() as usize,
        data.len(),
        0,
        0,
        0,
    ) as isize
}

/// 接收；对端关闭返回 0；失败返回负值。
pub unsafe fn sock_recv(fd: isize, buf: &mut [u8]) -> isize {
    syscall6(
        SYS_RECVFROM,
        fd as usize,
        buf.as_mut_ptr() as usize,
        buf.len(),
        0,
        0,
        0,
    ) as isize
}

/// 关闭 socket（必须用 SHUTDOWN，不能用 CLOSE）。
pub unsafe fn sock_close(fd: isize) {
    syscall1(SYS_SHUTDOWN, fd as usize);
}

/// DNS 解析（内核 → QEMU slirp → 宿主 DNS）。返回主机字节序 IPv4，0 = 失败。
pub unsafe fn dns_resolve(name: &[u8]) -> u32 {
    let mut buf = [0u8; 256];
    let n = name.len().min(buf.len() - 1);
    buf[..n].copy_from_slice(&name[..n]);
    buf[n] = 0;
    syscall1(SYS_DNS_RESOLVE, buf.as_ptr() as usize) as u32
}

/// 解析点分十进制 IPv4；失败 None。
pub fn ip_parse(s: &[u8]) -> Option<u32> {
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

/// 解析 "host[:port]" → (ip, port, ok)。域名经内核 DNS 解析。
pub fn resolve(host: &[u8], default_port: u16) -> (u32, u16, bool) {
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
    let ip = unsafe { dns_resolve(h) };
    if ip != 0 {
        (ip, port, true)
    } else {
        (0, 0, false)
    }
}
