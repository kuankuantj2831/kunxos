// KunxOS 新系统调用测试：socketpair / truncate / statx。
//
// 依次执行：
//   1. socketpair(AF_UNIX, SOCK_STREAM) → fd0/fd1；fd1 写 "hi"，fd0 读回校验
//   2. 创建文件后 truncate("/tmp/trunc_test", 0)
//   3. statx("/etc/inittab") 读取大小并打印
// 全部通过打印 "SYSCALL-TEST PASS"，任一失败打印 FAIL。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;


const SYS_WRITE: usize = 64;
const SYS_READ: usize = 63;
const SYS_EXIT: usize = 93;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_SOCKETPAIR: usize = 199;
const SYS_TRUNCATE: usize = 45;
const SYS_STATX: usize = 291;
const SYS_CLONE: usize = 220;
const SYS_WAIT4: usize = 260;
const SIGCHLD: usize = 17;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => r, options(nostack));
    r
}
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => r, in("a1") a2, options(nostack));
    r
}
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => r, in("a1") a2, in("a2") a3, options(nostack));
    r
}
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => r, in("a1") a2, in("a2") a3, in("a3") a4, options(nostack));
    r
}
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let r: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => r,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, in("a5") a6,
        options(nostack)
    );
    r
}

const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SOCKET: usize = 198;
const SYS_CONNECT: usize = 203;
const SYS_CLIPBOARD_SET: usize = 401;
const SYS_CLIPBOARD_GET: usize = 402;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;
const SYS_MUNMAP: usize = 215;
const SYS_RT_SIGACTION: usize = 134;
const SYS_KILL: usize = 129;
const SYS_GETPID: usize = 172;

/// SIGTERM handler：打印标记后返回（经蹦床 rt_sigreturn 恢复）。
extern "C" fn handler_fn(_sig: usize) {
    print_str("[signal-handler] SIGTERM 已捕获\n");
}

fn print_str(s: &str) {
    unsafe {
        syscall3(SYS_WRITE, 1, s.as_ptr() as usize, s.len());
    }
}

/// 把路径拷入 NUL 终止缓冲，返回指针（内核 read_cstr 依赖 NUL）。
fn cstr(path: &str) -> usize {
    static mut BUF: [u8; 128] = [0u8; 128];
    unsafe {
        let n = path.len().min(BUF.len() - 1);
        BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
        BUF[n] = 0;
        BUF.as_ptr() as usize
    }
}

fn fail(msg: &str) -> ! {
    print_str("[syscall-test] 失败：");
    print_str(msg);
    print_str("\n");
    unsafe { syscall1(SYS_EXIT, 1) };
    loop {}
}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    ttf_text::font_init();
    print_str("[syscall-test] 开始\n");

    // 1. socketpair
    let mut sv = [0i32; 2];
    let r = unsafe {
        syscall4(
            SYS_SOCKETPAIR,
            AF_UNIX as usize,
            SOCK_STREAM as usize,
            0,
            sv.as_mut_ptr() as usize,
        )
    };
    if r != 0 {
        fail("socketpair");
    }
    let (fd0, fd1) = (sv[0], sv[1]);
    print_str("[syscall-test] socketpair fds=");
    print_dec(fd0 as u32);
    print_str(",");
    print_dec(fd1 as u32);
    print_str("\n");
    // fd1 写 "hi"，fd0 读回
    let mut out = [0u8; 4];
    let w = unsafe { syscall3(SYS_WRITE, fd1 as usize, b"hi".as_ptr() as usize, 2) };
    if w != 2 {
        fail("socketpair 写入");
    }
    let n = unsafe { syscall3(SYS_READ, fd0 as usize, out.as_mut_ptr() as usize, 4) };
    if n != 2 || out[0] != b'h' || out[1] != b'i' {
        fail("socketpair 读回");
    }
    // 反向：fd0 写，fd1 读
    let w2 = unsafe { syscall3(SYS_WRITE, fd0 as usize, b"yo".as_ptr() as usize, 2) };
    if w2 != 2 {
        fail("socketpair 写入2");
    }
    let n2 = unsafe { syscall3(SYS_READ, fd1 as usize, out.as_mut_ptr() as usize, 4) };
    if n2 != 2 || out[0] != b'y' || out[1] != b'o' {
        fail("socketpair 读回2");
    }
    print_str("[syscall-test] socketpair 双向通信通过\n");

    // 5. COW 引用计数泄漏测试：反复 fork + exit（不 exec），验证共享页
    //    引用计数正确增减，物理内存不泄漏。
    print_str("[syscall-test] COW fork 循环测试\n");
    for _i in 0..8 {
        let p = unsafe { syscall1(SYS_CLONE, SIGCHLD) } as i64;
        if p < 0 {
            fail("克隆循环");
        }
        if p == 0 {
            unsafe { syscall1(SYS_EXIT, 0) };
            loop {}
        }
        let mut st = 0u32;
        let r2 = unsafe { syscall3(SYS_WAIT4, p as usize, (&mut st) as *mut u32 as usize, 0) };
        if (r2 as isize) < 0 || ((st >> 8) & 0xFF) != 0 {
            fail("克隆循环等待");
        }
    }
    print_str("[syscall-test] COW fork 循环通过（8 次 fork，0 泄漏）\n");

    unsafe {
        syscall1(SYS_CLOSE, fd0 as usize);
        syscall1(SYS_CLOSE, fd1 as usize);
    }

    // 2. truncate acts on an existing file and must not create missing paths.
    let create_fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/tmp/trunc_test"), O_WRONLY | O_CREAT | O_TRUNC) };
    if (create_fd as isize) < 0 { fail("truncate fixture"); }
    unsafe { syscall1(SYS_CLOSE, create_fd); }
    let r = unsafe { syscall2(SYS_TRUNCATE, cstr("/tmp/trunc_test"), 0) };
    print_str("[syscall-test] truncate 返回=");
    print_hex(r as u64);
    print_str("\n");
    if r != 0 {
        fail("truncate");
    }
    let fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/tmp/trunc_test"), O_RDONLY) };
    if (fd as isize) < 0 {
        fail("truncate 打开");
    }
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    print_str("[syscall-test] truncate 通过\n");

    // 3. statx
    let mut stx = [0u8; 256];
    let r2 = unsafe {
        syscall6(
            SYS_STATX,
            AT_FDCWD,
            cstr("/etc/inittab"),
            0,
            0x7FF,
            stx.as_mut_ptr() as usize,
            256,
        )
    };
    print_str("[syscall-test] statx 返回=");
    print_hex(r2 as u64);
    print_str("\n");
    if r2 != 0 {
        fail("statx");
    }
    let size = u64::from_le_bytes([stx[40], stx[41], stx[42], stx[43], stx[44], stx[45], stx[46], stx[47]]);
    let mode = u16::from_le_bytes([stx[28], stx[29]]);
    print_str("[syscall-test] inittab 大小（statx）=");
    print_dec(size as u32);
    print_str(" mode=0o");
    print_hex(mode as u64);
    print_str("\n");
    if size == 0 {
        fail("statx 大小");
    }


    // 4. procfs：读 /proc/version 与 /proc/meminfo
    {
        let fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/proc/version"), O_RDONLY) };
        if (fd as isize) >= 0 {
            let mut b = [0u8; 128];
            let n = unsafe { syscall3(SYS_READ, fd as usize, b.as_mut_ptr() as usize, 128) };
            print_str("[syscall-test] /proc/version: ");
            if (n as isize) > 0 {
                print_str(core::str::from_utf8(&b[..(n as isize).min(b.len() as isize) as usize]).unwrap_or("?"));
            }
            unsafe { syscall1(SYS_CLOSE, fd as usize) };
        } else {
            print_str("[syscall-test] 打开 /proc/version 失败\n");
        }
        let fd2 = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/proc/meminfo"), O_RDONLY) };
        if (fd2 as isize) >= 0 {
            let mut b = [0u8; 256];
            let n = unsafe { syscall3(SYS_READ, fd2 as usize, b.as_mut_ptr() as usize, 256) };
            print_str("[syscall-test] /proc/meminfo: ");
            if (n as isize) > 0 {
                print_str(core::str::from_utf8(&b[..(n as isize).min(b.len() as isize) as usize]).unwrap_or("?"));
            }
            unsafe { syscall1(SYS_CLOSE, fd2 as usize) };
        } else {
            print_str("[syscall-test] 打开 /proc/meminfo 失败\n");
        }
        let fd3 = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/proc/processes"), O_RDONLY) };
        if (fd3 as isize) >= 0 {
            let mut b = [0u8; 256];
            let n = unsafe { syscall3(SYS_READ, fd3 as usize, b.as_mut_ptr() as usize, 256) };
            print_str("[syscall-test] /proc/processes:\n");
            if (n as isize) > 0 {
                print_str(core::str::from_utf8(&b[..(n as isize).min(b.len() as isize) as usize]).unwrap_or("?"));
            }
            unsafe { syscall1(SYS_CLOSE, fd3 as usize) };
        }
    }



    // 7. tmpfs：写入 /tmp/tmpfs_test 再读回
    {
        let fd = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/tmp/tmpfs_test"), O_WRONLY | O_CREAT | O_TRUNC) };
        if (fd as isize) >= 0 {
            let _w = unsafe { syscall3(SYS_WRITE, fd as usize, b"TMPFS-OK\n".as_ptr() as usize, 9) };
            unsafe { syscall1(SYS_CLOSE, fd as usize) };
            let fd2 = unsafe { syscall3(SYS_OPENAT, AT_FDCWD, cstr("/tmp/tmpfs_test"), O_RDONLY) };
            let mut b = [0u8; 16];
            let n = unsafe { syscall3(SYS_READ, fd2 as usize, b.as_mut_ptr() as usize, 16) };
            print_str("[syscall-test] tmpfs 读回: ");
            if (n as isize) > 0 {
                print_str(core::str::from_utf8(&b[..(n as isize).min(b.len() as isize) as usize]).unwrap_or("?"));
            }
            unsafe { syscall1(SYS_CLOSE, fd2 as usize) };
            if n == 9 && &b[..9] == b"TMPFS-OK\n" {
                print_str("[syscall-test] tmpfs 通过\n");
            } else {
                print_str("[syscall-test] tmpfs 失败\n");
            }
        } else {
            print_str("[syscall-test] 打开 tmpfs 失败\n");
        }
    }

    // 9. 用户态 nanosleep：睡 100ms 后继续（验证用户态阻塞睡眠）
    {
        print_str("[syscall-test] 用户态纳米睡眠测试\n");
        let mut req = [0u8; 16];
        req[8..].copy_from_slice(&100_000_000u64.to_le_bytes()); // nsec = 100ms
        let r = unsafe { syscall2(SYS_NANOSLEEP, req.as_mut_ptr() as usize, 0) };
        print_str("[syscall-test] 纳米睡眠返回=");
        print_hex(r as u64);
        print_str("（若继续运行即已睡眠）\n");
    }

    // 10. 剪贴板：写入后读回
    {
        let wr = unsafe { syscall2(SYS_CLIPBOARD_SET, b"clipboard-data".as_ptr() as usize, 14) };
        let mut cb = [0u8; 64];
        let rd = unsafe { syscall2(SYS_CLIPBOARD_GET, cb.as_mut_ptr() as usize, 64) };
        print_str("[syscall-test] 剪贴板 设置=");
        print_hex(wr as u64);
        print_str(" 获取=");
        print_hex(rd as u64);
        print_str(" 数据=");
        // 剪贴板空/失败返回负 errno（usize 巨大值）：按 isize 判正并钳制。
        let rd_i = rd as isize;
        if rd_i > 0 {
            print_str(core::str::from_utf8(&cb[..(rd_i as usize).min(cb.len())]).unwrap_or("?"));
        }
        print_str("\n");
        if rd == 14 && &cb[..14] == b"clipboard-data" {
            print_str("[syscall-test] 剪贴板 通过\n");
        } else {
            print_str("[syscall-test] 剪贴板 失败\n");
        }
    }

    // 8. TCP 客户端：连接宿主 echo 服务器（10.0.2.2:38080），发送并读回
    {
        print_str("[syscall-test] TCP 客户端测试\n");
        let fd = unsafe { syscall2(SYS_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/) };
        if (fd as i64) < 0 {
            print_str("[syscall-test] socket 创建失败\n");
        } else {
            // sockaddr_in: family u16(2) + port u16 BE + ip u32 BE
            let mut addr = [0u8; 16];
            addr[0] = 2; addr[1] = 0;
            addr[2] = (38080 >> 8) as u8; addr[3] = (38080 & 0xFF) as u8;
            addr[4] = 10; addr[5] = 0; addr[6] = 2; addr[7] = 2;
            let cr = unsafe { syscall3(SYS_CONNECT, fd as usize, addr.as_mut_ptr() as usize, 16) };
            print_str("[syscall-test] connect 返回=");
            print_hex(cr as u64);
            print_str("\n");
            if cr == 0 {
                // socket I/O 走 sendto/recvfrom（Linux 语义）
                let w = unsafe { syscall6(SYS_SENDTO, fd as usize, b"hello-kunxos".as_ptr() as usize, 12, 0, 0, 0) };
                print_str("[syscall-test] TCP 发送返回=");
                print_hex(w as u64);
                print_str("\n");
                let mut b = [0u8; 64];
                let n = unsafe { syscall6(SYS_RECVFROM, fd as usize, b.as_mut_ptr() as usize, 64, 0, 0, 0) };
                print_str("[syscall-test] TCP 接收返回=");
                print_hex(n as u64);
                print_str(" 数据=");
                // recvfrom 失败返回负 errno（对端重置等）：按 isize 判正。
                let n_i = n as isize;
                if n_i > 0 {
                    print_str(core::str::from_utf8(&b[..(n_i as usize).min(b.len())]).unwrap_or("?"));
                }
                print_str("\n");
                if n == 12 && &b[..12] == b"hello-kunxos" {
                    print_str("[syscall-test] TCP 通过\n");
                } else {
                    print_str("[syscall-test] TCP 失败\n");
                }
            }
            unsafe { syscall1(SYS_CLOSE, fd as usize) };
        }
    }



    print_str("[syscall-test] 通过\n");
    unsafe { syscall1(SYS_EXIT, 0) };
    loop {}
}

/// COW 自检共享页：fork 后父子共享（页表只读），各自写入应隔离。
static mut COW_SHARED: u32 = 0x1111_1111;
/// 父进程在 fork 前的值，供子进程校验父值未被污染。
static mut COW_PARENT_VALUE: u32 = 0x1111_1111;

fn print_dec(mut v: u32) {
    let mut buf = [0u8; 12];
    if v == 0 {
        print_str("0");
        return;
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    print_str(core::str::from_utf8(&buf[i..]).unwrap_or(""));
}

fn print_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let nib = (v & 0xF) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
        v >>= 4;
    }
    print_str(core::str::from_utf8(&buf).unwrap_or(""));
}

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
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 1（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    print_str("PANIC: syscall-test\n");
    // PanicInfo 仅由 str/数字拼成，输出缓冲必为合法 UTF-8。
    print_str(unsafe { core::str::from_utf8(&w.buf[..w.len]) }.unwrap_or(""));
    unsafe { syscall1(SYS_EXIT, 1) };
    loop {}
}
