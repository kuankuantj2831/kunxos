// KunxOS SFS v2 并发回归测试（多进程 → 多任务）。
//
// 场景：NPROC 个子进程同时（经 clone(SIGCHLD)）在 /var/run 创建、写入、
// 读回、截断、删除各自的文件。每个子进程用**唯一的填充字节**写跨 3 个
// 数据块（间接块层级）的载荷；读回时逐字节校验，任何串线（读到其它文件
// 的 inode/块、块被覆盖）都会导致校验失败。
//
// 该场景复现 2026-08-27 实测的两类 SFS v2 缺陷：
//   1. 文件内容串线（fs_create/dir_add_entry/data_block_alloc 并发下
//      「目录项 ↔ inode ↔ 数据块」未保持原子对应）；
//   2. txn_commit 时 jlog_place 除零 panic（事务状态全局共享被污染）。
// 修复后本测试应稳定输出 "SFS-RACE PASS"；任何校验失败输出 FAIL 并退出 1。
//
// 全部通过打印 "SFS-RACE PASS"，任一失败打印 "SFS-RACE FAIL" 并退出 1。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_CLONE: usize = 220;
const SYS_EXIT: usize = 93;
const SYS_WRITE: usize = 64;
const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_TRUNCATE: usize = 45;
const SYS_UNLINKAT: usize = 35;
const SYS_NANOSLEEP: usize = 101;
const SYS_WAIT4: usize = 260;

const SIGCHLD: usize = 17;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

// ==================== 测试参数 ====================

/// 并发子进程数。
const NPROC: usize = 4;
/// 每个子进程的创建/写/读/截断/删除轮数。
const ITERS: usize = 12;
/// 载荷字节数：跨 3 个 4KB 数据块（含间接块分配路径）。
const PAYLOAD: usize = 3 * 4096;

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        in("x4") a5,
        options(nostack)
    );
    ret
}

// ==================== 基础 I/O ====================

fn print(s: &str) {
    unsafe {
        syscall3(SYS_WRITE, 1, s.as_ptr() as usize, s.len());
    }
}

fn print_dec(mut v: usize) {
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
    unsafe {
        syscall3(SYS_WRITE, 1, buf[i..].as_ptr() as usize, buf.len() - i);
    }
}

/// 单进程基线：先在不并发时验证一次 12KB 写入/读回/截断/删除全程，把「并发
/// 串线」与「FS 自身缺陷」区分开。基线失败直接退出并打印 BASELINE FAIL。
fn baseline_check() -> i32 {
    static mut BPATH: [u8; 64] = [0u8; 64];
    static mut BPAY: [u8; PAYLOAD] = [0u8; PAYLOAD];
    static mut BRB: [u8; PAYLOAD] = [0u8; PAYLOAD];
    let bpath = unsafe { &mut *core::ptr::addr_of_mut!(BPATH) };
    let head = b"/var/run/sfsrace_base";
    let mut p = 0usize;
    for &c in head.iter() {
        bpath[p] = c;
        p += 1;
    }
    bpath[p] = 0;
    let ptr = bpath.as_ptr() as usize;

    let fd = open(ptr, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        print("[sfsrace] BASELINE FAIL: creat\n");
        return 1;
    }
    let pay = unsafe { &mut *core::ptr::addr_of_mut!(BPAY) };
    for b in pay.iter_mut() {
        *b = 0x5A;
    }
    let w = write_all(fd, pay);
    if w != PAYLOAD as i64 {
        print("[sfsrace] BASELINE FAIL: write got=");
        print_dec(w as usize);
        print(" want=");
        print_dec(PAYLOAD);
        print("\n");
        close(fd);
        return 1;
    }
    close(fd);

    let fd = open(ptr, O_RDONLY);
    if fd < 0 {
        print("[sfsrace] BASELINE FAIL: reopen\n");
        return 1;
    }
    let rb = unsafe { &mut *core::ptr::addr_of_mut!(BRB) };
    let n = read_all(fd, rb);
    close(fd);
    if n != PAYLOAD as i64 {
        print("[sfsrace] BASELINE FAIL: read got=");
        print_dec(n as usize);
        print("\n");
        return 1;
    }
    for &b in rb.iter() {
        if b != 0x5A {
            print("[sfsrace] BASELINE FAIL: content mismatch\n");
            return 1;
        }
    }

    // 基线也覆盖截断：truncate → 重新打开 → 必须读空（区分「并发串线」与
    // 「FS 自身截断缺陷」）。
    if truncate(ptr) != 0 {
        print("[sfsrace] BASELINE FAIL: truncate rc\n");
        return 1;
    }
    let fd = open(ptr, O_RDONLY);
    if fd < 0 {
        print("[sfsrace] BASELINE FAIL: reopen-empty\n");
        return 1;
    }
    let n = read_fd(fd, rb);
    close(fd);
    if n != 0 {
        print("[sfsrace] BASELINE FAIL: truncate not empty n=");
        print_dec(n as usize);
        print("\n");
        return 1;
    }
    unlink(ptr);
    print("[sfsrace] 基线 12KB+截断 单进程 OK\n");
    0
}

/// 把路径写入静态缓冲（NUL 结尾），返回起始指针（内核 read_cstr 依赖 NUL）。
fn path_buf() -> Option<&'static mut [u8; 64]> {
    unsafe { Some(&mut *core::ptr::addr_of_mut!(PATH_BUF)) }
}

static mut PATH_BUF: [u8; 64] = [0u8; 64];

fn build_path(buf: &mut [u8; 64], i: usize, iter: usize) -> usize {
    let head = b"/var/run/sfsrace_";
    let mut p = 0usize;
    for &c in head.iter() {
        buf[p] = c;
        p += 1;
    }
    // 子进程序号：单位数字（0..NPROC-1 < 10）
    buf[p] = b'0' + (i % 10) as u8;
    p += 1;
    buf[p] = b'_';
    p += 1;
    // 迭代号：6 位定宽
    let mut digits = [b'0'; 6];
    let mut v = iter % 1000_000;
    let mut k = 6;
    while v > 0 {
        k -= 1;
        digits[k] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for k in 0..6 {
        buf[p] = digits[k];
        p += 1;
    }
    buf[p] = 0;
    buf.as_ptr() as usize
}

fn open(path: usize, flags: usize) -> i64 {
    unsafe { syscall3(SYS_OPENAT, AT_FDCWD, path, flags) as i64 }
}

fn write_fd(fd: i64, buf: &[u8]) -> i64 {
    unsafe { syscall3(SYS_WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) as i64 }
}

fn read_fd(fd: i64, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as i64 }
}

/// 内核 sys_write 每次最多搬运 4096 字节（与真实守护进程一致的写入语义）；
/// 分块写完，容忍部分写（返回 0 或负值视为失败）。返回已写总字节数。
fn write_all(fd: i64, buf: &[u8]) -> i64 {
    let mut done = 0i64;
    while (done as usize) < buf.len() {
        let n = write_fd(fd, &buf[done as usize..]);
        if n <= 0 {
            return done; // 写不动了：返回已写字节数，由调用方判定
        }
        done += n;
    }
    done
}

/// 分块读完（read 同样受 4096 限制）。返回累计读取字节数，EOF/错误时提前返回。
fn read_all(fd: i64, buf: &mut [u8]) -> i64 {
    let mut done = 0i64;
    while (done as usize) < buf.len() {
        let n = read_fd(fd, &mut buf[done as usize..]);
        if n <= 0 {
            return done; // EOF 或错误
        }
        done += n;
    }
    done
}

fn close(fd: i64) {
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
}

fn truncate(path: usize) -> i64 {
    unsafe { syscall2(SYS_TRUNCATE, path, 0) as i64 }
}

fn unlink(path: usize) -> i64 {
    unsafe { syscall2(SYS_UNLINKAT, AT_FDCWD, path) as i64 }
}

fn fork() -> i64 {
    unsafe { syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as i64 }
}

fn wait4(pid: i64, status: &mut i32) -> i64 {
    unsafe { syscall4(SYS_WAIT4, pid as usize, status as *mut i32 as usize, 0, 0) as i64 }
}

fn nanosleep_us(us: usize) {
    unsafe {
        // timespec { tv_sec = 0, tv_nsec = us*1000 }
        REQ[0] = 0;
        REQ[1] = ((us % 1_000_000) * 1000) as u64;
        syscall2(SYS_NANOSLEEP, REQ.as_ptr() as usize, 0);
    }
}

static mut REQ: [u64; 2] = [0; 2];

fn exit(code: usize) -> ! {
    unsafe { syscall1(SYS_EXIT, code) };
    loop {}
}

fn fail(msg: &str, i: usize, iter: usize) -> ! {
    print("[sfsrace] FAIL: ");
    print(msg);
    print(" child=");
    print_dec(i);
    print(" iter=");
    print_dec(iter);
    print("\n");
    exit(1);
}

// ==================== 子进程：串行压力循环 ====================

/// 子进程 `i`：反复 创建→写→读回校验→截断→删除 自己的文件。
fn child_main(i: usize) -> ! {
    static mut PAY: [u8; PAYLOAD] = [0u8; PAYLOAD];
    static mut RBUF: [u8; PAYLOAD] = [0u8; PAYLOAD];

    let marker = (0x50 + (i % 8)) as u8; // 每子进程唯一填充字节
    for iter in 0..ITERS {
        let path = path_buf().expect("no path buf");
        let p = build_path(path, i, iter);

        // 1. 创建（O_TRUNC 保证从空文件开始，同时覆盖「已存在文件重写」路径）
        let fd = open(p, O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            fail("creat", i, iter);
        }
        // 2. 写入跨 3 块的载荷
        let pay = unsafe { &mut *core::ptr::addr_of_mut!(PAY) };
        for b in pay.iter_mut() {
            *b = marker;
        }
        let w = write_all(fd, pay);
        if w != PAYLOAD as i64 {
            print("[sfsrace] FAIL: write child=");
            print_dec(i);
            print(" iter=");
            print_dec(iter);
            print(" want=");
            print_dec(PAYLOAD);
            print(" got=");
            print_dec(w as usize);
            print("\n");
            exit(1);
        }
        close(fd);

        // 3. 读回并逐字节校验（检测 inode/块串线）
        let fd = open(p, O_RDONLY);
        if fd < 0 {
            fail("reopen", i, iter);
        }
        let rbuf = unsafe { &mut *core::ptr::addr_of_mut!(RBUF) };
        let total = read_all(fd, rbuf);
        close(fd);
        if total != PAYLOAD as i64 {
            fail("read short", i, iter);
        }
        let mut bad = 0usize;
        for &b in rbuf.iter() {
            if b != marker {
                bad += 1;
            }
        }
        if bad > 0 {
            print("[sfsrace] FAIL: 检测到串线 child=");
            print_dec(i);
            print(" iter=");
            print_dec(iter);
            print(" bad_bytes=");
            print_dec(bad);
            print("\n");
            exit(1);
        }

        // 4. 截断为 0 并确认读空（验证 fs_truncate 释放块且不再残留数据）
        if truncate(p) != 0 {
            fail("truncate", i, iter);
        }
        let fd = open(p, O_RDONLY);
        if fd < 0 {
            fail("reopen-empty", i, iter);
        }
        let n = read_fd(fd, rbuf);
        close(fd);
        if n != 0 {
            print("[sfsrace] FAIL: 截断后非空 child=");
            print_dec(i);
            print(" iter=");
            print_dec(iter);
            print(" got=");
            print_dec(n as usize);
            print("\n");
            exit(1);
        }

        // 5. 删除
        if unlink(p) != 0 {
            fail("unlink", i, iter);
        }

        // 错峰：让各子进程的时间片交错（放大并发窗口）
        nanosleep_us(2_000 + (i * 700 + iter * 313) % 6000);
    }
    exit(0);
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    // 0. 单进程基线（无并发）：验证 FS 自身 12KB 写/读/删能力
    if baseline_check() != 0 {
        print("[sfsrace] SFS-RACE FAIL (baseline)\n");
        return 1;
    }

    print("[sfsrace] 正在启动 NPROC=");
    print_dec(NPROC);
    print(" ITERS=");
    print_dec(ITERS);
    print(" 载荷=");
    print_dec(PAYLOAD);
    print("\n");

    // 同时 fork NPROC 个子进程 —— 与 camerad/audiod/sensord 等守护进程
    // 并发写 /var/run 的真实场景一致（EL0 多任务 + 可抢占内核路径）。
    let mut pids = [0i32; NPROC];
    let mut nbad = 0usize;
    for i in 0..NPROC {
        let p = fork();
        if p < 0 {
            print("[sfsrace] FAIL: fork 失败\n");
            return 1;
        }
        if p == 0 {
            child_main(i); // 不返回
        }
        pids[i] = p as i32;
    }

    // 父进程逐个回收并汇总退出码
    for i in 0..NPROC {
        let mut status = 0i32;
        let r = wait4(pids[i] as i64, &mut status);
        if r <= 0 {
            print("[sfsrace] FAIL: wait4 child=");
            print_dec(i);
            print("\n");
            nbad += 1;
            continue;
        }
        let code = ((status as u32) >> 8) & 0xFF;
        print("[sfsrace] 子进程 ");
        print_dec(i);
        print(" 已退出 code=");
        print_dec(code as usize);
        print("\n");
        if code != 0 {
            nbad += 1;
        }
    }

    if nbad == 0 {
        print("[sfsrace] SFS-RACE PASS\n");
        0
    } else {
        print("[sfsrace] SFS-RACE FAIL\n");
        1
    }
}

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
    print("PANIC: sfsrace\n");
    // PanicInfo 仅由 str/数字拼成，输出缓冲必为合法 UTF-8。
    print(unsafe { core::str::from_utf8(&w.buf[..w.len]) }.unwrap_or(""));
    exit(1);
}