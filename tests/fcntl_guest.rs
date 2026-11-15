//! Guest regression for fcntl completion: FD_CLOEXEC, dup3 flags, pipe O_NONBLOCK,
//! O_APPEND file offset. Run with tools/test_audit_guest.py (isolated image only):
//!   python tools/test_audit_guest.py --qemu <qemu-system-riscv64> \
//!       --build-dir build/fcntl-guest --source tests/fcntl_guest.rs \
//!       --success-marker "FCNTL REGRESSION PASS"
#![no_std]
#![no_main]
use core::arch::{asm, global_asm};

unsafe fn call(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    asm!("ecall", in("a7") n, inlateout("a0") a[0] => r, in("a1") a[1], in("a2") a[2],
         in("a3") a[3], in("a4") a[4], in("a5") a[5], options(nostack));
    r
}
fn print(s: &str) { unsafe { call(64, [1, s.as_ptr() as usize, s.len(), 0, 0, 0]); } }
fn check(ok: bool, label: &str) {
    if !ok {
        print("FCNTL GUEST FAIL: ");
        print(label);
        print("\n");
        unsafe { call(93, [1, 0, 0, 0, 0, 0]); }
        loop {}
    }
}

const CWD: usize = (-100isize) as usize;
const O_NONBLOCK: usize = 0x800;
const O_CLOEXEC: usize = 0x80000;
const O_APPEND: usize = 0x400;
const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_SETFD: usize = 2;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_DUPFD_CLOEXEC: usize = 1030;

unsafe fn open(path: &[u8], flags: usize) -> usize {
    let fd = call(56, [CWD, path.as_ptr() as usize, flags, 0, 0, 0]);
    check(fd >= 0, "open fixture");
    fd as usize
}
unsafe fn fcntl(fd: usize, cmd: usize, arg: usize) -> isize { call(25, [fd, cmd, arg, 0, 0, 0]) }
unsafe fn close(fd: usize) { call(57, [fd, 0, 0, 0, 0, 0]); }

#[no_mangle]
pub extern "C" fn k_main() -> usize {
    print("FCNTL REGRESSION START\n");
    unsafe {
        // ---- FD_CLOEXEC 基本读写 ----
        let fd = open(b"/tmp/fcntl-fixture\0", 0x242); // O_RDWR|O_CREAT|O_TRUNC
        check(fcntl(fd, F_GETFD, 0) == 0, "new fd has no CLOEXEC");
        check(fcntl(fd, F_SETFD, 1) == 0 && fcntl(fd, F_GETFD, 0) == 1, "F_SETFD sets CLOEXEC");
        check(fcntl(fd, F_SETFD, 0) == 0 && fcntl(fd, F_GETFD, 0) == 0, "F_SETFD clears CLOEXEC");
        check(fcntl(12345, F_GETFD, 0) == -9, "F_GETFD invalid fd");
        check(fcntl(12345, F_SETFD, 1) == -9, "F_SETFD invalid fd");

        // ---- open(O_CLOEXEC) / F_DUPFD(_CLOEXEC) / dup3 ----
        let cfd = open(b"/tmp/fcntl-fixture\0", 2 | O_CLOEXEC);
        check(fcntl(cfd, F_GETFD, 0) == 1, "open O_CLOEXEC sets flag");
        let d1 = fcntl(cfd, F_DUPFD, 10);
        check(d1 >= 10 && fcntl(d1 as usize, F_GETFD, 0) == 0, "F_DUPFD clears CLOEXEC on the copy");
        let d2 = fcntl(fd, F_DUPFD_CLOEXEC, 10);
        check(d2 >= 10 && fcntl(d2 as usize, F_GETFD, 0) == 1, "F_DUPFD_CLOEXEC sets CLOEXEC on the copy");
        check(fcntl(fd, F_GETFD, 0) == 0, "F_DUPFD_CLOEXEC leaves the source unchanged");
        check(call(24, [fd, 14, O_CLOEXEC, 0, 0, 0]) == 14 && fcntl(14, F_GETFD, 0) == 1, "dup3 O_CLOEXEC");
        check(call(24, [fd, 13, 0, 0, 0, 0]) == 13 && fcntl(13, F_GETFD, 0) == 0, "dup3 without flags");
        // dup3 覆盖已带 CLOEXEC 的目标：新表项不应继承旧标志
        check(call(24, [fd, 14, 0, 0, 0, 0]) == 14 && fcntl(14, F_GETFD, 0) == 0, "dup3 target resets CLOEXEC");
        check(call(24, [fd, 12, 1, 0, 0, 0]) == -22, "dup3 rejects unknown flags");
        close(14); close(13); close(d2 as usize); close(d1 as usize); close(cfd);
        print("FCNTL cloexec PASS\n");

        // ---- pipe2 标志校验与 O_NONBLOCK ----
        let mut p = [0i32; 2];
        check(call(59, [p.as_mut_ptr() as usize, 1, 0, 0, 0, 0]) == -22, "pipe2 rejects unknown flags");
        check(call(59, [p.as_mut_ptr() as usize, O_NONBLOCK | O_CLOEXEC, 0, 0, 0, 0]) == 0, "pipe2 nonblock fixture");
        let (r, w) = (p[0] as usize, p[1] as usize);
        check(fcntl(r, F_GETFD, 0) == 1 && fcntl(w, F_GETFD, 0) == 1, "pipe2 O_CLOEXEC on both ends");
        check(fcntl(r, F_GETFL, 0) & O_NONBLOCK as isize != 0, "F_GETFL reports O_NONBLOCK on read end");
        check(fcntl(w, F_GETFL, 0) & 3 == 1, "write end keeps O_WRONLY");
        let mut buf = [0u8; 8];
        check(call(63, [r, buf.as_mut_ptr() as usize, 8, 0, 0, 0]) == -11, "empty nonblocking read -> EAGAIN");
        check(call(64, [w, b"Q".as_ptr() as usize, 1, 0, 0, 0]) == 1, "pipe write");
        check(call(63, [r, buf.as_mut_ptr() as usize, 8, 0, 0, 0]) == 1 && buf[0] == b'Q', "pipe read after write");
        // 缓冲写满：先写出进度，随后 EAGAIN（不能阻塞）
        let chunk = [b'x'; 1024];
        let mut hit = false;
        let mut i = 0;
        while i < 256 {
            let n = call(64, [w, chunk.as_ptr() as usize, chunk.len(), 0, 0, 0]);
            if n == -11 { hit = true; break; }
            check(n > 0, "nonblocking write makes progress or EAGAIN");
            i += 1;
        }
        check(hit, "full nonblocking pipe -> EAGAIN");
        // 清除 O_NONBLOCK 后 F_GETFL 不再报告；F_SETFL 拒绝其它位
        check(fcntl(r, F_SETFL, 0) == 0 && fcntl(r, F_GETFL, 0) & O_NONBLOCK as isize == 0, "F_SETFL clears O_NONBLOCK");
        check(fcntl(r, F_SETFL, O_NONBLOCK) == 0 && fcntl(r, F_GETFL, 0) & O_NONBLOCK as isize != 0, "F_SETFL sets O_NONBLOCK");
        check(fcntl(r, F_SETFL, 0x10000) == -38, "F_SETFL rejects unsupported pipe bits");
        // 排空后关闭写端：非阻塞读必须返回 EOF(0) 而不是 EAGAIN
        loop {
            let n = call(63, [r, buf.as_mut_ptr() as usize, 8, 0, 0, 0]);
            if n <= 0 { check(n == -11, "drained pipe reports EAGAIN while writer open"); break; }
        }
        close(w);
        check(call(63, [r, buf.as_mut_ptr() as usize, 8, 0, 0, 0]) == 0, "nonblocking read returns EOF after writer closed");
        close(r);
        print("FCNTL pipe PASS\n");

        // ---- O_APPEND 后文件偏移位于文件末尾 ----
        let a = open(b"/tmp/fcntl-append\0", 0x242);
        check(call(64, [a, b"AAAA".as_ptr() as usize, 4, 0, 0, 0]) == 4, "seed file");
        check(call(62, [a, 0, 0, 0, 0, 0]) == 0, "rewind");
        check(fcntl(a, F_SETFL, O_APPEND | 2) == 0, "enable O_APPEND");
        check(call(64, [a, b"B".as_ptr() as usize, 1, 0, 0, 0]) == 1, "append write");
        check(call(62, [a, 0, 1, 0, 0, 0]) == 5, "offset is at EOF after append (SEEK_CUR)");
        check(call(62, [a, 0, 2, 0, 0, 0]) == 5, "file length is 5");
        close(a);
        close(fd);
        print("FCNTL append PASS\n");
    }
    print("FCNTL REGRESSION PASS\n");
    0
}

global_asm!(".section .text._start, \"ax\"", ".global _start", "_start:", "call k_main", "li a7,93", "ecall", "j .");
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { print("FCNTL GUEST FAIL: panic\n"); loop {} }
