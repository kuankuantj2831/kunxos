// KOS 对象存储用户态测试（内核系统集成验收）。
//
// 通过私有系统调用号段 460..=466 直接驱动 KOS 引擎：
//   stat → 读种子对象 → 写/读回 → mkdir/嵌套写 → list → unlink → 再读失败。
// 全部通过打印 "KOSTEST PASS"；任一步失败打印 "KOSTEST FAIL:<step>"。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;
const SYS_KOS_READ: usize = 461;
const SYS_KOS_WRITE: usize = 462;
const SYS_KOS_MKDIR: usize = 463;
const SYS_KOS_UNLINK: usize = 464;
const SYS_KOS_LIST: usize = 465;
const SYS_KOS_STAT: usize = 466;

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") SYS_WRITE,
        in("a0") fd,
        in("a1") buf.as_ptr() as usize,
        in("a2") buf.len(),
        lateout("a0") ret,
        options(nostack),
    );
    ret
}

unsafe fn kos_read(path: &str, buf: &mut [u8]) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!(
        "ecall",
        in("a7") SYS_KOS_READ,
        in("a0") p.as_ptr() as usize,
        in("a1") buf.as_mut_ptr() as usize,
        in("a2") buf.len(),
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

unsafe fn kos_write(path: &str, data: &[u8], caps: u32) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!(
        "ecall",
        in("a7") SYS_KOS_WRITE,
        in("a0") p.as_ptr() as usize,
        in("a1") data.as_ptr() as usize,
        in("a2") data.len(),
        in("a3") caps,
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

unsafe fn kos_simple(nr: usize, path: &str, arg: u32) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!(
        "ecall",
        in("a7") nr,
        in("a0") p.as_ptr() as usize,
        in("a1") arg,
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); };
}

fn fail(step: &str) -> usize {
    print("KOSTEST FAIL:");
    print(step);
    print("\n");
    1
}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 1) 卷状态（466）：8×u64 小端 [gen, objects, free, pool]。
    let mut st = [0u64; 4];
    unsafe {
        let r: usize;
        asm!("ecall", in("a7") SYS_KOS_STAT, in("a0") st.as_mut_ptr() as usize,
             lateout("a0") r, options(nostack));
        if r as i64 != 0 {
            return fail("stat");
        }
    }
    print("[kostest] KOS 卷在线：对象数 ");
    print_hex(st[1] as u64);
    print(" 空闲块 ");
    print_hex(st[2]);
    print("\n");

    // 2) 读启动期种子对象。
    let mut buf = [0u8; 256];
    let n = unsafe { kos_read("/seed.txt", &mut buf) };
    if n < 0 {
        return fail("read-seed");
    }
    print("[kostest] seed: ");
    unsafe { write_fd(1, &buf[..n as usize]); };
    print("\n");

    // 3) 写 + 读回。
    const MSG: &[u8] = b"hello from kunxos userspace via kos";
    if unsafe { kos_write("/msg.txt", MSG, 0o644) } < 0 {
        return fail("write");
    }
    let mut rb = [0u8; 128];
    let n = unsafe { kos_read("/msg.txt", &mut rb) };
    if n < 0 || &rb[..n as usize] != MSG {
        return fail("readback");
    }

    // 4) mkdir + 嵌套写 + list。
    if unsafe { kos_simple(SYS_KOS_MKDIR, "/demo", 0o755) } != 0 {
        return fail("mkdir");
    }
    if unsafe { kos_write("/demo/inner.txt", b"nested", 0o644) } < 0 {
        return fail("write-nested");
    }
    let mut lb = [0u8; 512];
    let ln = unsafe {
        let ret: usize;
        asm!("ecall", in("a7") SYS_KOS_LIST, in("a0") b"/demo\0".as_ptr() as usize,
             in("a1") lb.as_mut_ptr() as usize, in("a2") lb.len(),
             lateout("a0") ret, options(nostack));
        ret as i64
    };
    if ln < 0 {
        return fail("list");
    }
    print("[kostest] /demo 列表：\n");
    unsafe { write_fd(1, &lb[..ln as usize]); };

    // 5) unlink 后再读必须失败。
    if unsafe { kos_simple(SYS_KOS_UNLINK, "/msg.txt", 0) } != 0 {
        return fail("unlink");
    }
    let mut zb = [0u8; 16];
    if unsafe { kos_read("/msg.txt", &mut zb) } >= 0 {
        return fail("unlink-effective");
    }

    print("KOSTEST PASS\n");
    0
}

fn print_hex(v: u64) {
    let mut s = [0u8; 18];
    s[0] = b'0';
    s[1] = b'x';
    for i in 0..16 {
        let n = ((v >> ((15 - i) * 4)) & 0xF) as u8;
        s[2 + i] = if n < 10 { b'0' + n } else { b'a' + n - 10 };
    }
    unsafe { write_fd(1, &s); };
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
    unsafe {
        write_fd(2, b"PANIC: kostest\n");
        asm!("li a7, 93", "ecall", options(nostack));
    }
    loop {}
}
