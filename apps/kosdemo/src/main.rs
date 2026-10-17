// kosdemo：用**纯标准 POSIX 系统调用**（openat/write/lseek/read/close）
// 操作 `/kos/` 路径下的 KOS 对象——证明应用零改造即可接入对象存储。
//
// 流程：创建 → 写 → lseek 回头读 → 追加模式打开再写 → 读尾。
// 全部通过打印 "KOSDEMO PASS"。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_CREAT: usize = 0x40;
const O_APPEND: usize = 0x400;
const O_WRONLY: usize = 1;
const O_RDONLY: usize = 0;
const SEEK_SET: usize = 0;
const SEEK_END: usize = 2;

unsafe fn sc3(n: usize, a: usize, b: usize, c: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, in("a0") a, in("a1") b, in("a2") c,
         lateout("a0") r, options(nostack));
    r
}

unsafe fn sc1(n: usize, a: usize) -> usize {
    let r: usize;
    asm!("ecall", in("a7") n, in("a0") a, lateout("a0") r, options(nostack));
    r
}

fn print(s: &str) {
    unsafe { sc3(SYS_WRITE, 1, s.as_ptr() as usize, s.len()) };
}

fn fail(step: &str) -> usize {
    print("KOSDEMO FAIL:");
    print(step);
    print("\n");
    1
}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 1) O_CREAT 创建并写。
    let fd = unsafe {
        sc3(SYS_OPENAT, AT_FDCWD, b"/kos/demo.txt\0".as_ptr() as usize,
            O_WRONLY | O_CREAT | 0x200) as i64  // +O_TRUNC：幂等重置
    };
    if fd < 0 {
        return fail("open-create");
    }
    let n = unsafe {
        sc3(SYS_WRITE, fd as usize, b"hello posix bridge".as_ptr() as usize, 18)
    };
    if n != 18 {
        return fail("write");
    }
    unsafe { sc1(SYS_CLOSE, fd as usize) };

    // 2) 重开只读 + lseek 到 offset 6，读回。
    let fd = unsafe {
        sc3(SYS_OPENAT, AT_FDCWD, b"/kos/demo.txt\0".as_ptr() as usize, O_RDONLY) as i64
    };
    if fd < 0 {
        return fail("open-read");
    }
    unsafe { sc3(SYS_LSEEK, fd as usize, 6, SEEK_SET) };
    let mut buf = [0u8; 6];
    let n = unsafe { sc3(SYS_WRITE, 0, buf.as_mut_ptr() as usize, 6) }; // 占位防优化
    let _ = n;
    // read = syscall 63
    let n = unsafe {
        let r: usize;
        asm!("ecall", in("a7") 63usize, in("a0") fd as usize,
             in("a1") buf.as_mut_ptr() as usize, in("a2") 6usize,
             lateout("a0") r, options(nostack));
        r
    };
    if n != 6 || &buf != b"posix " {
        return fail("lseek-read");
    }
    // 3) SEEK_END 前移 6 读尾。
    // POSIX SEEK_END 语义：pos = end + offset（从末尾前移 6 需 -6）。
    unsafe { sc3(SYS_LSEEK, fd as usize, (-6i64) as usize, SEEK_END) };
    let mut tail = [0u8; 6];
    let n = unsafe {
        let r: usize;
        asm!("ecall", in("a7") 63usize, in("a0") fd as usize,
             in("a1") tail.as_mut_ptr() as usize, in("a2") 6usize,
             lateout("a0") r, options(nostack));
        r
    };
    if n != 6 || &tail != b"bridge" {
        return fail("seek-end-read");
    }
    unsafe { sc1(SYS_CLOSE, fd as usize) };

    // 4) O_APPEND 追加。
    let fd = unsafe {
        sc3(SYS_OPENAT, AT_FDCWD, b"/kos/demo.txt\0".as_ptr() as usize,
            O_WRONLY | O_APPEND) as i64
    };
    if fd < 0 {
        return fail("open-append");
    }
    let n = unsafe { sc3(SYS_WRITE, fd as usize, b"+tail".as_ptr() as usize, 5) };
    if n != 5 {
        return fail("append-write");
    }
    unsafe { sc1(SYS_CLOSE, fd as usize) };

    print("KOSDEMO PASS\n");
    0
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
fn panic(_info: &PanicInfo) -> ! {
    unsafe { sc3(SYS_WRITE, 2, b"PANIC: kosdemo\n".as_ptr() as usize, 15) };
    loop {}
}
