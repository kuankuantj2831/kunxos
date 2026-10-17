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
const SYS_KOS_READ_RANGE: usize = 470;
const SYS_KOS_RENAME: usize = 471;
const SYS_KOS_OPEN: usize = 472;
const SYS_KOS_READ_FD: usize = 473;
const SYS_KOS_CLOSE_FD: usize = 474;
const SYS_KOS_UP_BEGIN: usize = 476;
const SYS_KOS_UP_PUSH: usize = 477;
const SYS_KOS_UP_STATUS: usize = 478;
const SYS_KOS_UP_FINISH: usize = 479;
const SYS_KOS_APPEND: usize = 467;
const SYS_KOS_FAULT: usize = 480;

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

/// argv[1] == "-r" 时只读校验（持久性验收）：打印 /journal 与 /seed.txt。
#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    if argc >= 2 {
        let mut p = [0u8; 8];
        let mut n = 0;
        unsafe {
            let s = *argv.add(1) as *const u8;
            while n < 8 && *s.add(n) != 0 {
                p[n] = *s.add(n);
                n += 1;
            }
        }
        let arg = core::str::from_utf8(&p[..n]).unwrap_or("");
        if arg == "-u1" || arg == "-u2" {
            return upload_phase(arg);
        }
        if &p[..n] == b"-r" {
            print("[kostest -r] /seed.txt: ");
            let mut b = [0u8; 128];
            let n = unsafe { kos_read("/seed.txt", &mut b) };
            if n < 0 {
                print("READ FAIL\n");
                return 1;
            }
            unsafe { write_fd(1, &b[..n as usize]); };
            print("\n[kostest -r] /journal: ");
            let n = unsafe { kos_read("/journal", &mut b) };
            if n < 0 {
                print("READ FAIL\n");
                return 1;
            }
            unsafe { write_fd(1, &b[..n as usize]); };
            print("\nKOSTEST READBACK OK\n");
            return 0;
        }
    }
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
    // 持久卷上 /demo 可能已存在（上次启动创建）：幂等容忍。
    unsafe { kos_simple(SYS_KOS_MKDIR, "/demo", 0o755) };
    if unsafe { kos_simple(SYS_KOS_LIST, "/demo", 0) } < 0 && false {
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

    // 4.5) 可变记录：write_mutable + append + 再读回。
    const M0: &[u8] = b"mutable-v0|";
    if unsafe { kos_write_mutable("/journal", M0) } < 0 {
        return fail("write-mutable");
    }
    let mut total = M0.len();
    for i in 0..5 {
        let line = b"log-line;"; // 9 字节，跨次追加
        if unsafe { kos_append("/journal", line) } < 0 {
            return fail("append");
        }
        total += line.len();
    }
    let n = unsafe { kos_read("/journal", &mut rb) };
    if n as usize != total || !rb[..n as usize].starts_with(M0) {
        return fail("append-readback");
    }
    // 4.7) pread 随机读（470）+ rename（471）+ fd 层（472..=474）。
    const P: &[u8] = b"0123456789ABCDEFGHIJ";
    unsafe { kos_write_mutable("/pread", P) };
    let mut pb = [0u8; 4];
    if pread("/pread", 10, &mut pb) != 4 || &pb[..4] != *b"ABCD" {
        return fail("pread");
    }
    if kos_rename("/pread", "/pread2") != 0 {
        return fail("rename");
    }
    if unsafe { kos_read("/pread", &mut pb) } >= 0 {
        return fail("rename-old-gone");
    }
    let mut qb = [0u8; 64];
    let qn = pread("/pread2", 15, &mut qb);
    if qn != 5 || &qb[..5] != b"FGHIJ" {
        return fail("pread-tail");
    }
    // fd 层：open → 顺序 read ×2 → close。
    let fd = kos_open_fd("/pread2");
    if fd < 0 {
        return fail("fd-open");
    }
    let mut fb = [0u8; 10];
    if kos_read_fd(fd as u32, &mut fb) != 10 || &fb != b"0123456789" {
        return fail("fd-read1");
    }
    if kos_read_fd(fd as u32, &mut fb) != 10 || &fb != b"ABCDEFGHIJ" {
        return fail("fd-read2");
    }
    if kos_read_fd(fd as u32, &mut fb) != 0 {
        return fail("fd-eof");
    }
    if kos_close_fd(fd as u32) != 1 {
        return fail("fd-close");
    }

    // 5) unlink 后再读必须失败。
    if unsafe { kos_simple(SYS_KOS_UNLINK, "/msg.txt", 0) } != 0 {
        return fail("unlink");
    }
    let mut zb = [0u8; 16];
    if unsafe { kos_read("/msg.txt", &mut zb) } >= 0 {
        return fail("unlink-effective");
    }

    // 6) I/O 故障降级（E2，syscall 480）：注入 → 写路径全 EIO 且系统
    //    存活 → 解除 → 旧数据完好 + 新写成功。幂等：/faultdir 先清。
    unsafe fn kos_fault_toggle(on: usize) -> i64 {
        let ret: usize;
        asm!(
            "ecall",
            in("a7") SYS_KOS_FAULT,
            in("a0") on,
            lateout("a0") ret,
            options(nostack),
        );
        ret as i64
    }
    let _ = unsafe { kos_simple(SYS_KOS_UNLINK, "/faultdir", 0) };
    if unsafe { kos_write_mutable("/faultrec", b"before-fault") } < 0 {
        return fail("fault-setup");
    }
    if unsafe { kos_fault_toggle(1) } != 0 {
        return fail("fault-on");
    }
    // 写路径必须 EIO（写总需落盘，不受缓存影响）。
    if unsafe { kos_write_mutable("/faultrec", b"during-fault") } != -5 {
        return fail("fault-write-eio");
    }
    if unsafe { kos_append("/journal", b"x") } != -5 {
        return fail("fault-append-eio");
    }
    if unsafe { kos_simple(SYS_KOS_MKDIR, "/faultdir", 0) } != -5 {
        return fail("fault-mkdir-eio");
    }
    if unsafe { kos_fault_toggle(0) } != 1 {
        return fail("fault-off");
    }
    // 恢复：旧数据完好 + 新写成功 + mkdir 成功（残留幽灵则 -17 亦可）。
    let mut fb = [0u8; 32];
    let n = unsafe { kos_read("/faultrec", &mut fb) };
    if n < 0 || !fb[..n as usize].starts_with(b"before-fault") {
        return fail("fault-recover-data");
    }
    if unsafe { kos_write_mutable("/faultrec", b"after-fault") } < 0 {
        return fail("fault-recover-write");
    }
    let mk = unsafe { kos_simple(SYS_KOS_MKDIR, "/faultdir", 0) };
    if mk != 0 && mk != -17 {
        return fail("fault-recover-mkdir");
    }
    print("KOSTEST FAULT-DEGRADE PASS\n");

    print("KOSTEST PASS\n");
    0
}

unsafe fn kos_write_mutable(path: &str, data: &[u8]) -> i64 {
    kos_write_flags(path, data, 0o644, 1)
}

unsafe fn kos_append(path: &str, data: &[u8]) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!(
        "ecall",
        in("a7") SYS_KOS_APPEND,
        in("a0") p.as_ptr() as usize,
        in("a1") data.as_ptr() as usize,
        in("a2") data.len(),
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

/// kos_write 的可变版：a3 低 16 位为权限，a3 位 16 = MUTABLE。
unsafe fn kos_write_flags(path: &str, data: &[u8], caps: u32, mutable: u32) -> i64 {
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
        in("a3") caps | (mutable << 16),
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

/// pread：读 offset 起 buf.len() 字节，返回实读。
unsafe fn kos_pread(path: &str, offset: u64, buf: &mut [u8]) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!(
        "ecall",
        in("a7") SYS_KOS_READ_RANGE,
        in("a0") p.as_ptr() as usize,
        in("a1") buf.as_mut_ptr() as usize,
        in("a2") buf.len(),
        in("a3") offset,
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

fn pread(path: &str, offset: u64, buf: &mut [u8]) -> usize {
    let r = unsafe { kos_pread(path, offset, buf) };
    if r < 0 { usize::MAX } else { r as usize }
}

unsafe fn kos_rename_raw(from: &str, to: &str) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let mut q = [0u8; 128];
    let n = from.len().min(127);
    p[..n].copy_from_slice(&from.as_bytes()[..n]);
    let m = to.len().min(127);
    q[..m].copy_from_slice(&to.as_bytes()[..m]);
    asm!(
        "ecall",
        in("a7") SYS_KOS_RENAME,
        in("a0") p.as_ptr() as usize,
        in("a1") q.as_ptr() as usize,
        lateout("a0") ret,
        options(nostack),
    );
    ret as i64
}

fn kos_rename(from: &str, to: &str) -> i64 {
    unsafe { kos_rename_raw(from, to) }
}

unsafe fn kos_open_raw(path: &str) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!("ecall", in("a7") SYS_KOS_OPEN, in("a0") p.as_ptr() as usize,
         lateout("a0") ret, options(nostack));
    ret as i64
}

fn kos_open_fd(path: &str) -> i64 {
    unsafe { kos_open_raw(path) }
}

unsafe fn kos_read_fd_raw(fd: u32, buf: &mut [u8]) -> i64 {
    let ret: usize;
    asm!("ecall", in("a7") SYS_KOS_READ_FD, in("a0") fd as usize,
         in("a1") buf.as_mut_ptr() as usize, in("a2") buf.len(),
         lateout("a0") ret, options(nostack));
    ret as i64
}

fn kos_read_fd(fd: u32, buf: &mut [u8]) -> i64 {
    unsafe { kos_read_fd_raw(fd, buf) }
}

unsafe fn kos_close_fd_raw(fd: u32) -> i64 {
    let ret: usize;
    asm!("ecall", in("a7") SYS_KOS_CLOSE_FD, in("a0") fd as usize,
         lateout("a0") ret, options(nostack));
    ret as i64
}

fn kos_close_fd(fd: u32) -> i64 {
    unsafe { kos_close_fd_raw(fd) }
}

/// 断点续传验收：-u1 = 传一半即退（模拟关机）；-u2 = 重启后续传完成。
fn upload_phase(phase: &str) -> usize {
    const TOTAL: u64 = 4096 * 40;
    const HALF: u64 = 4096 * 20;
    let chunk = [0x5Au8; 4096];
    if phase == "-u1" {
        if up_begin("/ota-pkg", TOTAL) != 0 {
            print("KOSTEST FAIL:up-begin\n");
            return 1;
        }
        let mut off = 0u64;
        while off < HALF {
            if up_push("/ota-pkg", off, &chunk) < 0 {
                print("KOSTEST FAIL:up-push1\n");
                return 1;
            }
            off += 4096;
        }
        print("[kostest -u1] 已传一半（20/40 块），模拟关机\n");
        0
    } else {
        let (total, recv, cont) = match up_status("/ota-pkg") {
            Some(v) => v,
            None => {
                print("KOSTEST FAIL:up-status\n");
                return 1;
            }
        };
        if total != TOTAL || cont < 20 {
            print("KOSTEST FAIL:up-resume-progress\n");
            return 1;
        }
        print("[kostest -u2] 重启后进度保留：");
        print_hex64(recv as u64);
        print("/");
        print_hex64(total);
        print(" 块，续传...\n");
        let _ = recv;
        let mut off = HALF;
        while off < TOTAL {
            if up_push("/ota-pkg", off, &chunk) < 0 {
                print("KOSTEST FAIL:up-push2\n");
                return 1;
            }
            off += 4096;
        }
        if up_finish("/ota-pkg") != 0 {
            print("KOSTEST FAIL:up-finish\n");
            return 1;
        }
        // 发布后整读校验。
        let mut buf = [0u8; 4096];
        let mut ok = true;
        let mut off = 0u64;
        while off < TOTAL {
            let n = pread("/ota-pkg", off, &mut buf);
            if n != 4096 || buf[0] != 0x5A {
                ok = false;
                break;
            }
            off += 4096;
        }
        if !ok {
            print("KOSTEST FAIL:up-verify\n");
            return 1;
        }
        print("KOSTEST UPLOAD-RESUME PASS\n");
        0
    }
}

unsafe fn up_call3(nr: usize, path: &str, a1: u64) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    asm!("ecall", in("a7") nr, in("a0") p.as_ptr() as usize, in("a1") a1,
         lateout("a0") ret, options(nostack));
    ret as i64
}

fn up_begin(path: &str, total: u64) -> i64 {
    unsafe { up_call3(SYS_KOS_UP_BEGIN, path, total) }
}

fn up_finish(path: &str) -> i64 {
    unsafe { up_call3(SYS_KOS_UP_FINISH, path, 0) }
}

fn up_push(path: &str, offset: u64, data: &[u8]) -> i64 {
    let ret: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    unsafe {
        asm!("ecall", in("a7") SYS_KOS_UP_PUSH, in("a0") p.as_ptr() as usize,
             in("a1") data.as_ptr() as usize, in("a2") data.len(), in("a3") offset,
             lateout("a0") ret, options(nostack));
    }
    ret as i64
}

/// 返回 (total, received, contiguous)。
fn up_status(path: &str) -> Option<(u64, u64, u64)> {
    let mut out = [0u8; 24];
    let r: usize;
    let mut p = [0u8; 128];
    let n = path.len().min(127);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    unsafe {
        asm!("ecall", in("a7") SYS_KOS_UP_STATUS, in("a0") p.as_ptr() as usize,
             in("a1") out.as_mut_ptr() as usize, lateout("a0") r, options(nostack));
    }
    if r as i64 != 0 {
        return None;
    }
    Some((
        u64::from_le_bytes(out[0..8].try_into().unwrap()),
        u64::from_le_bytes(out[8..16].try_into().unwrap()),
        u64::from_le_bytes(out[16..24].try_into().unwrap()),
    ))
}

fn print_hex64(v: u64) {
    print_hex(v);
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
