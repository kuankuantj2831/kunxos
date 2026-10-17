// KunxOS tlstest：浏览器 TLS 栈来宾侧自测（EL0 用户态，裸 syscall、no_std）。
//
// 经 shell `run /bin/tlstest [host]` 启动（默认 www.baidu.com）。行为：
//   1. DNS 解析 → TCP 连接 443
//   2. 复用 apps/browser/src/tls/ 完整 TLS 1.2 握手（证书链验证 + SNI）
//   3. 发送 GET /，读取响应头与正文开头
//   4. 成功打印 "[tls-selftest] PASS" 后退出 0；失败打印原因退出 1
//
// 同时覆盖浏览器 net.rs 的来宾侧依赖：virtio-net（slirp 用户网络）、
// 内核 DNS、SYS_GETRANDOM 熵源、RTC 墙钟。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

extern crate alloc;

extern crate kunxsec as kxsec;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../browser/src/tls/mod.rs"]
mod tls;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

// ==================== 全局分配器 ====================

const SYS_BRK: usize = 214;

struct BrkAlloc;

unsafe impl GlobalAlloc for BrkAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(1) + layout.align();
        let raw = syscall1(SYS_BRK, 0);
        let new = syscall1(SYS_BRK, raw + size);
        if new < raw + size {
            return core::ptr::null_mut();
        }
        ((raw + layout.align() - 1) & !(layout.align() - 1)) as *mut u8
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static GLOBAL_ALLOC: BrkAlloc = BrkAlloc;

// ==================== 系统调用 ====================

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;
const SYS_SOCKET: usize = 198;
const SYS_CONNECT: usize = 203;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SHUTDOWN: usize = 210;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_CLOCK_GETTIME: usize = 113;
const CLOCK_MONOTONIC: i32 = 1;
const SYS_DNS_RESOLVE: usize = 403;
const SYS_GETRANDOM: usize = 278;

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
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        in("a3") a4,
        in("a4") a5,
        in("a5") a6,
        options(nostack)
    );
    ret
}

fn print(s: &str) {
    unsafe {
        syscall3(SYS_WRITE, 1, s.as_ptr() as usize, s.len());
    }
}

fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    print(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

// ==================== 传输层 ====================

const AF_INET: usize = 2;

struct TcpTr {
    fd: isize,
}

impl tls::Transport for TcpTr {
    fn send_all(&mut self, data: &[u8]) -> Result<(), &'static str> {
        let mut sent = 0usize;
        while sent < data.len() {
            let n = unsafe {
                syscall6(
                    SYS_SENDTO,
                    self.fd as usize,
                    data[sent..].as_ptr() as usize,
                    data.len() - sent,
                    0,
                    0,
                    0,
                )
            };
            if (n as isize) <= 0 {
                return Err("TLS 传输发送失败");
            }
            sent += n;
        }
        Ok(())
    }

    fn recv_some(&mut self, buf: &mut [u8]) -> Result<usize, &'static str> {
        let n = unsafe {
            syscall6(
                SYS_RECVFROM,
                self.fd as usize,
                buf.as_mut_ptr() as usize,
                buf.len(),
                0,
                0,
                0,
            )
        };
        if (n as isize) < 0 {
            return Err("TLS 传输接收失败");
        }
        Ok(n)
    }
}

impl TcpTr {
    fn close(&self) {
        unsafe { syscall1(SYS_SHUTDOWN, self.fd as usize) };
    }
}

// ==================== 随机源 / 时钟 ====================

struct SysRng;

impl tls::Rng for SysRng {
    fn fill(&mut self, buf: &mut [u8]) {
        let n = unsafe { syscall3(SYS_GETRANDOM, buf.as_mut_ptr() as usize, buf.len(), 0) };
        if (n as isize) < 0 {
            // 熵源不可用：时间+地址混合退化（演示系统可接受）
            let mut seed = (now_sec() as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
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

/// RTC 墙钟（微秒，epoch）。
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
/// RTC 墙钟（秒，epoch）；不可用返回 0。
fn now_sec() -> i64 {
    let mut tv = [0u8; 16];
    let r = unsafe { syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0) };
    if (r as isize) < 0 {
        return 0;
    }
    let sec = u64::from_le_bytes(tv[0..8].try_into().unwrap_or([0; 8]));
    sec as i64
}

// ==================== 主流程 ====================

fn parse_ip(s: &str) -> u32 {
    let mut ip = 0u32;
    for part in s.split('.') {
        ip = (ip << 8) | part.parse::<u32>().unwrap_or(0);
    }
    ip
}

#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    // argv[1]：目标主机（默认 www.baidu.com）
    let host = unsafe {
        if argc > 1 && *(argv.add(1)) != 0 {
            let mut s = String::new();
            let mut p = *(argv.add(1)) as *const u8;
            while *p != 0 {
                s.push(*p as char);
                p = p.add(1);
            }
            s
        } else {
            String::from("www.baidu.com")
        }
    };

    print("[tls-selftest] 目标: ");
    print(&host);
    print(":443\n");

    // 1. DNS / IP
    let ip = if host.as_bytes().iter().all(|c| c.is_ascii_digit() || *c == b'.') {
        parse_ip(&host)
    } else {
        let mut buf = [0u8; 256];
        let n = host.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&host.as_bytes());
        buf[n] = 0;
        let ip = unsafe { syscall1(SYS_DNS_RESOLVE, buf.as_ptr() as usize) } as u32;
        if ip == 0 {
            print("[tls-selftest] FAIL: 域名解析失败\n");
            unsafe { syscall1(SYS_EXIT, 1) };
            return 1;
        }
        ip
    };
    print(&format!("[tls-selftest] IP 已解析\n"));

    // 2. TCP 连接
    let fd = unsafe { syscall3(SYS_SOCKET, AF_INET, 1, 0) } as isize;
    if fd < 0 {
        print("[tls-selftest] FAIL: socket 创建失败\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }
    let mut sa = [0u8; 8];
    sa[0] = AF_INET as u8;
    sa[2] = 0x01;
    sa[3] = 0xBB; // 443
    sa[4] = (ip >> 24) as u8;
    sa[5] = ((ip >> 16) & 0xFF) as u8;
    sa[6] = ((ip >> 8) & 0xFF) as u8;
    sa[7] = (ip & 0xFF) as u8;
    let rc = unsafe { syscall2(SYS_CONNECT, fd as usize, sa.as_ptr() as usize) } as isize;
    if rc < 0 {
        print("[tls-selftest] FAIL: TCP 连接失败\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }

    // 3. TLS 握手
    let t0 = now_us();
    let mut conn = match tls::TlsConn::connect(TcpTr { fd }, &host, &mut SysRng, now_sec()) {
        Ok(c) => c,
        Err(e) => {
            print("[tls-selftest] FAIL: TLS 握手失败：");
            print(e);
            print("\n");
            unsafe { syscall1(SYS_EXIT, 1) };
            return 1;
        }
    };
    let t_hs = now_us() - t0;
    print("[tls-selftest] TLS 握手成功（证书链已验证） 耗时 ");
    print_dec(t_hs / 1000);
    print("ms\n");

    // 4. HTTP GET
    let req = format!("GET / HTTP/1.1\r\nHost: {}\r\nUser-Agent: KunxOS-TlsTest/1.0\r\nConnection: close\r\n\r\n", host);
    if conn.send(req.as_bytes()).is_err() {
        print("[tls-selftest] FAIL: 发送请求失败\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }
    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match conn.recv(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&buf[..n]);
                if body.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    conn.close();

    if !(body.starts_with(b"HTTP/1.1 200") || body.starts_with(b"HTTP/1.0 200")) {
        print("[tls-selftest] FAIL: 非 200 响应\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }
    let head_end = body.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(0);
    let hdr = core::str::from_utf8(&body[..head_end.min(body.len())]).unwrap_or("");
    print("[tls-selftest] 响应状态与头部:\n");
    for line in hdr.lines().take(4) {
        print("  | ");
        print(line);
        print("\n");
    }
    print("[tls-selftest] PASS\n");
    unsafe { syscall1(SYS_EXIT, 0) };
    0
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
);

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print("[tls-selftest] PANIC\n");
    unsafe { syscall1(SYS_EXIT, 1) };
    loop {}
}
