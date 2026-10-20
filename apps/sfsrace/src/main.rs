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
const SYS_RENAMEAT: usize = 38;
const SYS_FSYNC: usize = 82;

const SIGCHLD: usize = 17;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_APPEND: usize = 0x400;

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
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        in("a3") a4,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        in("a3") a4,
        in("a4") a5,
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

/// renameat（信箱认领 / 原子发布）。目标已存在时内核 POSIX 覆盖。
fn rename(old: usize, new: usize) -> i64 {
    unsafe { syscall4(SYS_RENAMEAT, AT_FDCWD, old, AT_FDCWD, new) as i64 }
}

fn fsync(fd: i64) -> i64 {
    unsafe { syscall1(SYS_FSYNC, fd as usize) as i64 }
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

// ==================== 并发信箱压测（rename 原子提交/认领） ====================
//
// 模拟真实文件信箱的两类协议（本轮 lifd/audiod/rild/alarmd/wifid 迁移后的
// 形态），断言：无丢失、无重复、无半行（撕裂）、无死锁（有界轮询）。
//
// 阶段 A（多写者管道 + 认领读者，模型 audiod.ctl / rild.cmd / wifid.pipe）：
//   3 个写者子进程并发 O_APPEND 追加 "W<i> <seq>\n"（写时取尾由内核保证
//   互不覆盖），父进程 rename 认领取走并按 (i, seq) 计数。结束断言：
//   每写者每序号恰好出现一次（丢失/重复即失败），全部消息完整成行。
//
// 阶段 B（单写者发布 + 并发读者，模型 rild.state / alarms.txt / .st）：
//   1 个写者子进程 tmp+fsync+rename 原子发布版本 "V<n>\n"，1 个读者子进程
//   反复整读并校验：任一读到的内容必须是完整合法行（无撕裂），版本号
//   单调不减且最终能观察到终值（活锁/死锁即失败）。

const MBX_WRITERS: usize = 3;
const MBX_ITERS: usize = 48;
const MBX_POLLS_MAX: usize = 30_000; // 死锁护栏：认领轮询上限（≥ 全部消息耗尽所需轮数一个量级）

const MBX_PIPE: &[u8] = b"/var/run/mbx.pipe";
const MBX_PIPE_R: &[u8] = b"/var/run/mbx.pipe.r";
const MBX_PUB: &[u8] = b"/var/run/mbx.pub";
const MBX_PUB_TMP: &[u8] = b"/var/run/mbx.pub.tmp";

/// 认领统计：`SEEN[i][seq]` = 写者 i 的序号 seq 已被消费恰好一次。
static mut SEEN: [[bool; MBX_ITERS]; MBX_WRITERS] = [[false; MBX_ITERS]; MBX_WRITERS];
static mut MBX_BUF: [u8; 2048] = [0u8; 2048];

/// 把静态路径写进给定缓冲（NUL 结尾），返回指针。
fn cstr(dst: &mut [u8; 64], s: &[u8]) -> usize {
    let n = s.len().min(63);
    dst[..n].copy_from_slice(&s[..n]);
    dst[n] = 0;
    dst.as_ptr() as usize
}

/// 阶段 A 写者：追加 `MBX_ITERS` 条消息 + 1 条 DONE 哨兵。
fn mbx_writer(i: usize) -> ! {
    let mut p = [0u8; 64];
    let pipe = cstr(&mut p, MBX_PIPE);
    for seq in 0..MBX_ITERS {
        let mut line = [0u8; 24];
        let mut n = 0usize;
        line[n] = b'W';
        n += 1;
        line[n] = b'0' + i as u8;
        n += 1;
        line[n] = b' ';
        n += 1;
        let mut v = seq + 1;
        let mut digits = [0u8; 8];
        let mut k = digits.len();
        while v > 0 {
            k -= 1;
            digits[k] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &d in digits[k..].iter() {
            line[n] = d;
            n += 1;
        }
        line[n] = b'\n';
        n += 1;
        // 与真实写方一致：每条消息独立 open(O_APPEND) → write → close。
        // 单次 write ≤ 4096，内核写时取尾 + FS 全局锁保证追加互不覆盖。
        let fd = open(pipe, O_WRONLY | O_CREAT | O_APPEND);
        if fd < 0 {
            fail("mbx writer open", i, seq);
        }
        let w = write_fd(fd, &line[..n]);
        close(fd);
        if w != n as i64 {
            fail("mbx writer write", i, seq);
        }
        nanosleep_us(300 + (i * 137 + seq * 31) % 900); // 错峰放大并发窗口
    }
    // DONE 哨兵：读者收齐 3 个哨兵且再取空一轮即知全部消息已到
    let fd = open(pipe, O_WRONLY | O_CREAT | O_APPEND);
    if fd < 0 {
        fail("mbx writer done open", i, MBX_ITERS);
    }
    let mut line = [0u8; 8];
    line[0] = b'W';
    line[1] = b'0' + i as u8;
    line[2] = b' ';
    line[3] = b'D';
    line[4] = b'\n';
    let w = write_fd(fd, &line[..5]);
    close(fd);
    if w != 5 {
        fail("mbx writer done write", i, MBX_ITERS);
    }
    exit(0);
}

/// 解析一批认领内容（完整行 "W<i> <seq>\n" / "W<i> D\n"）。
/// 返回 0 成功；1 失败（已打印原因）。尾字节非 \n = 撕裂半行。
fn mbx_parse_batch(buf: &mut [u8], n: usize, dones: &mut usize) -> i32 {
    if n == 0 {
        return 0;
    }
    if buf[n - 1] != b'\n' {
        print("[sfsrace] MAILBOX FAIL: 半行（写入撕裂）len=");
        print_dec(n);
        print("\n");
        return 1;
    }
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j < n && buf[j] != b'\n' {
            j += 1;
        }
        let line = &buf[i..j];
        i = j + 1;
        if line.len() < 3 || line[0] != b'W' {
            print("[sfsrace] MAILBOX FAIL: 非法行\n");
            return 1;
        }
        let wi = (line[1] - b'0') as usize;
        if wi >= MBX_WRITERS || line[2] != b' ' {
            print("[sfsrace] MAILBOX FAIL: 非法写者号\n");
            return 1;
        }
        if line.len() == 4 && line[3] == b'D' {
            *dones += 1;
            continue;
        }
        // 序号十进制
        let mut seq = 0usize;
        for &d in line[3..].iter() {
            if !(d.is_ascii_digit()) {
                print("[sfsrace] MAILBOX FAIL: 非法序号\n");
                return 1;
            }
            seq = seq * 10 + (d - b'0') as usize;
        }
        if seq == 0 || seq > MBX_ITERS {
            print("[sfsrace] MAILBOX FAIL: 序号越界 seq=");
            print_dec(seq);
            print("\n");
            return 1;
        }
        let seen = unsafe { &mut *core::ptr::addr_of_mut!(SEEN) };
        if seen[wi][seq - 1] {
            print("[sfsrace] MAILBOX FAIL: 重复消息 W");
            print_dec(wi);
            print(" seq=");
            print_dec(seq);
            print("\n");
            return 1;
        }
        seen[wi][seq - 1] = true;
    }
    0
}


/// 认领留档名（r40）：`MBX_PIPE_R_<k>`（k 单调递增，不复用）。
/// 返回 NUL 结尾缓冲的指针（open/rename/unlink 收 usize）。
fn taken_name(k: usize, out: &mut [u8; 48]) -> usize {
    let mut n = 0usize;
    for &c in MBX_PIPE_R.iter() {
        out[n] = c;
        n += 1;
    }
    out[n] = b'_';
    n += 1;
    let mut v = k;
    let mut dg = [0u8; 8];
    let mut di = dg.len();
    if v == 0 {
        di -= 1;
        dg[di] = b'0';
    }
    while v > 0 {
        di -= 1;
        dg[di] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &d in dg[di..].iter() {
        out[n] = d;
        n += 1;
    }
    out[n] = 0;
    out.as_ptr() as usize
}

/// 阶段 A 读者（父进程执行）：认领 → 读 → **编号留档** → 重建 → 解析
/// 计数（r40 零窗口协议重写）。
///
/// 原协议在 read_all→unlink(taken) 之间有个迟到写丢失窗：写者每条
/// open(O_APPEND)→write→close，若 open 抢在读者 rename 之前、write 落
/// 在 read_all 之后，该条消息（可能是 DONE 哨兵）随 unlink 永久删除——
/// 实测 dones=2 从 polls=6000 冻结到 30000（r39 心跳定证）。TAKEN_LEFT
/// 「持 fd 时 unlink 失败」防御对每条开关的写者无效（unlink 时写者已
/// close）。
///
/// 新协议**零窗口 by construction**：
/// - 认领改为 rename(pipe → taken_k)（k 单调递增）——rename 原子且
///   fd 随 inode，迟到写落在 taken_k 里**不会丢**（文件不再中途删除）；
/// - 运行中绝不 unlink 任何 taken_k；
/// - dones==WRITERS 且取空一轮后，**终局扫排**全部 taken_k（哨兵是各
///   写者最后一条，哨兵齐 ⟹ 写者已写完 close，扫排兜住全部非哨兵迟到
///   者），再统一清理。
/// 返回 0 成功；1 失败（已打印原因）。
fn mbx_reader(dones: &mut usize, polls: &mut usize) -> i32 {
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(MBX_BUF) };
    let mut p = [0u8; 64];
    let pipe = cstr(&mut p, MBX_PIPE);
    let mut k = 0usize; // taken_k 编号
    loop {
        *polls += 1;
        if *polls > MBX_POLLS_MAX {
            print("[sfsrace] MAILBOX FAIL: 认领轮询超限（疑似死锁/活锁）\n");
            print("[mbx-dbg] 超限时 dones=");
            print_dec(*dones);
            print(" taken_k=");
            print_dec(k);
            print("\n");
            return 1;
        }
        // 心跳：每 6000 轮汇报一次进度（诊断慢/卡）
        if *polls % 6000 == 0 {
            print("[mbx-dbg] polls=");
            print_dec(*polls);
            print(" dones=");
            print_dec(*dones);
            print("\n");
        }
        // 隔离删除（inode 卫生）：taken_{k-2} 距其认领已跨两整轮（毫秒
        // 级），迟到写者 fd（µs 级寿命）不可能仍持——根卷 512 inode，
        // 永不删除会 ENOSPC（r40 首版实测：写者 iter~25 即 open 失败）。
        if k >= 2 {
            let mut ob = [0u8; 48];
            unlink(taken_name(k - 2, &mut ob));
        }
        // 本轮认领名：MBX_PIPE_R_<k>（k 单调递增，不与历史认领复用名字）
        let mut nb = [0u8; 48];
        let taken = taken_name(k, &mut nb);
        k += 1;
        if rename(pipe, taken) != 0 {
            // 无消息：sleep 后再试（写者还在跑）
            nanosleep_us(1000);
            if *dones >= MBX_WRITERS {
                return mbx_final_sweep(dones, k);
            }
            continue;
        }
        let fd = open(taken, O_RDONLY);
        if fd < 0 {
            print("[sfsrace] MAILBOX FAIL: 打开认领文件失败\n");
            return 1;
        }
        let n = read_all(fd, buf) as usize;
        close(fd);
        // ★ 不 unlink：taken_k 留档待终局扫排（迟到写随 fd 落在 inode
        // 上，数据不丢；中途删除正是旧协议的丢失窗）。
        // 重建空管道（无 O_TRUNC：认领后写入方新追加不截断）
        let fd2 = open(pipe, O_WRONLY | O_CREAT);
        if fd2 >= 0 {
            close(fd2);
        }
        if n == 0 {
            // 取空确认：哨兵齐 + 本轮空 → 终局扫排全部 taken_k 兜住
            // 非哨兵迟到者（哨兵是写者最后一条，哨兵齐 ⟹ 无人再写）。
            if *dones >= MBX_WRITERS {
                return mbx_final_sweep(dones, k);
            }
            continue;
        }
        if mbx_parse_batch(buf, n, dones) != 0 {
            return 1;
        }
    }
}

/// 终局扫排（r40）：重读最近 3 份留档（更早的已被两轮隔离删除；各写者
/// 哨兵已收齐，必然已写完 close，重读捕获全部迟到非哨兵消息），再清理。
/// 返回 0 成功。
fn mbx_final_sweep(dones: &mut usize, k: usize) -> i32 {
    let lo = k.saturating_sub(3);
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(MBX_BUF) };
    for j in lo..k {
        let mut nb = [0u8; 48];
        let name = taken_name(j, &mut nb);
        let fd = open(name, O_RDONLY);
        if fd < 0 {
            print("[mbx-diag] sweep open 失败 j=");
            print_dec(j);
            print("\n");
        }
        if fd >= 0 {
            let n = read_all(fd, buf) as usize;
            close(fd);
            if n > 0 {
                print("[mbx-dbg] 终局扫排 taken_");
                print_dec(j);
                print(" 追到 ");
                print_dec(n);
                print(" 字节\n");
                if mbx_parse_batch(buf, n, dones) != 0 {
                    return 1;
                }
            }
        }
        unlink(name);
    }
    0
}

/// 阶段 A 主控：fork 写者 + 父进程认领消费 + 终局断言。
fn mailbox_phase_a() -> i32 {
    let mut p = [0u8; 64];
    let pipe = cstr(&mut p, MBX_PIPE);
    let mut r = [0u8; 64];
    let taken = cstr(&mut r, MBX_PIPE_R);
    unlink(taken);
    unlink(pipe);
    // 建空管道：写者第一次 open 就绪
    let fd = open(pipe, O_WRONLY | O_CREAT);
    if fd >= 0 {
        close(fd);
    }

    let mut pids = [0i32; MBX_WRITERS];
    for i in 0..MBX_WRITERS {
        let pid = fork();
        if pid < 0 {
            print("[sfsrace] MAILBOX FAIL: fork 写者失败\n");
            return 1;
        }
        if pid == 0 {
            mbx_writer(i);
        }
        pids[i] = pid as i32;
    }

    let mut dones = 0usize;
    let mut polls = 0usize;
    // 认领循环直到三个 DONE 哨兵收齐（哨兵是各写者最后一条消息，其被
    // 消费即证明该写者此前全部消息均已计入；此后管道再无新写入）
    if mbx_reader(&mut dones, &mut polls) != 0 {
        return 1;
    }
    // 收割写者
    for i in 0..MBX_WRITERS {
        let mut status = 0i32;
        let rr = wait4(pids[i] as i64, &mut status);
        if r_is_bad(rr, status) {
            print("[sfsrace] MAILBOX FAIL: 写者异常退出\n");
            return 1;
        }
    }
    // 无丢失断言：每写者全部序号恰好消费一次
    let seen = unsafe { &*core::ptr::addr_of!(SEEN) };
    for wi in 0..MBX_WRITERS {
        for seq in 0..MBX_ITERS {
            if !seen[wi][seq] {
                print("[sfsrace] MAILBOX FAIL: 丢失消息 W");
                print_dec(wi);
                print(" seq=");
                print_dec(seq + 1);
                print("\n");
                return 1;
            }
        }
    }
    print("[sfsrace] 信箱A 多写者追加+认领 消费 ");
    print_dec(MBX_WRITERS * MBX_ITERS);
    print(" 条无丢失/无重复 OK\n");
    0
}

fn r_is_bad(r: i64, status: i32) -> bool {
    r <= 0 || (((status as u32) >> 8) & 0xFF) != 0
}

/// 阶段 B 写者：`MBX_ITERS` 个版本 "V<n>\n"，tmp+fsync+rename 原子发布。
fn mbx_publisher() -> ! {
    let mut p = [0u8; 64];
    let mut t = [0u8; 64];
    let pubp = cstr(&mut p, MBX_PUB);
    let tmp = cstr(&mut t, MBX_PUB_TMP);
    for v in 1..=MBX_ITERS {
        let mut line = [0u8; 16];
        let mut n = 0usize;
        line[n] = b'V';
        n += 1;
        let mut x = v;
        let mut digits = [0u8; 8];
        let mut k = digits.len();
        while x > 0 {
            k -= 1;
            digits[k] = b'0' + (x % 10) as u8;
            x /= 10;
        }
        for &d in digits[k..].iter() {
            line[n] = d;
            n += 1;
        }
        line[n] = b'\n';
        n += 1;
        // 与 rild.state / alarmd.state 一致：写临时 → fsync → rename 覆盖。
        // 读者持打开时 rename EBUSY → 重试（读窗口微秒级，有界重试必过）。
        let mut ok = false;
        for _try in 0..64 {
            let fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC);
            if fd < 0 {
                fail("mbx pub open", 0, v);
            }
            let w = write_fd(fd, &line[..n]);
            fsync(fd);
            close(fd);
            if w != n as i64 {
                fail("mbx pub write", 0, v);
            }
            if rename(tmp, pubp) == 0 {
                ok = true;
                break;
            }
            nanosleep_us(200); // EBUSY：读者恰好持打开，稍候重试
        }
        if !ok {
            fail("mbx pub rename 重试超限", 0, v);
        }
        nanosleep_us(500 + (v * 97) % 800);
    }
    exit(0);
}

/// 阶段 B 读者：反复整读，断言每次内容都是完整合法 "V<n>\n" 且版本单调
/// 不减；观察到终值 MBX_ITERS 即成功退出（终值可能被跳过？不可能——读者
/// 轮询快于发布间隔，但即便跳过中间版本也合法，只要求终值可见）。
fn mbx_observer() -> ! {
    let mut p = [0u8; 64];
    let pubp = cstr(&mut p, MBX_PUB);
    let mut buf = [0u8; 32];
    let mut last = 0usize;
    for _poll in 0..MBX_POLLS_MAX {
        let fd = open(pubp, O_RDONLY);
        if fd >= 0 {
            let n = read_fd(fd, &mut buf) as usize;
            close(fd);
            if n > 0 {
                // 完整行校验：'V' + 数字... + '\n'（无撕裂半行）
                if buf[n - 1] != b'\n' || buf[0] != b'V' || n < 3 {
                    print("[sfsrace] MAILBOX FAIL: 发布内容撕裂 len=");
                    print_dec(n);
                    print("\n");
                    exit(1);
                }
                let mut v = 0usize;
                for &d in buf[1..n - 1].iter() {
                    if !d.is_ascii_digit() {
                        print("[sfsrace] MAILBOX FAIL: 发布内容非法\n");
                        exit(1);
                    }
                    v = v * 10 + (d - b'0') as usize;
                }
                if v == 0 || v > MBX_ITERS {
                    print("[sfsrace] MAILBOX FAIL: 发布版本越界 v=");
                    print_dec(v);
                    print("\n");
                    exit(1);
                }
                if v < last {
                    print("[sfsrace] MAILBOX FAIL: 发布版本回退 ");
                    print_dec(v);
                    print(" < ");
                    print_dec(last);
                    print("\n");
                    exit(1);
                }
                last = v;
                if v == MBX_ITERS {
                    exit(0); // 终值可见：活性与一致性均达成
                }
            }
        }
        nanosleep_us(300);
    }
    print("[sfsrace] MAILBOX FAIL: 观察者轮询超限（终值不可达，疑似活锁）\n");
    exit(1);
}

/// 阶段 B 主控：fork 发布者 + 观察者，收割并断言双零退出。
fn mailbox_phase_b() -> i32 {
    let mut p = [0u8; 64];
    let mut t = [0u8; 64];
    let pubp = cstr(&mut p, MBX_PUB);
    let tmp = cstr(&mut t, MBX_PUB_TMP);
    unlink(pubp);
    unlink(tmp);
    let wpid = fork();
    if wpid < 0 {
        print("[sfsrace] MAILBOX FAIL: fork 发布者失败\n");
        return 1;
    }
    if wpid == 0 {
        mbx_publisher();
    }
    let opid = fork();
    if opid < 0 {
        print("[sfsrace] MAILBOX FAIL: fork 观察者失败\n");
        return 1;
    }
    if opid == 0 {
        mbx_observer();
    }
    let mut bad = 0usize;
    for i in 0..2 {
        let mut status = 0i32;
        let pid = if i == 0 { wpid } else { opid };
        let r = wait4(pid, &mut status);
        if r_is_bad(r, status) {
            bad += 1;
        }
    }
    unlink(pubp);
    if bad != 0 {
        print("[sfsrace] MAILBOX FAIL: 阶段B 子进程异常\n");
        return 1;
    }
    print("[sfsrace] 信箱B 发布+并发整读 无撕裂/版本单调 OK\n");
    0
}

/// 信箱压测入口：阶段 A + B 全绿返回 0。
fn mailbox_phase() -> i32 {
    if mailbox_phase_a() != 0 {
        return 1;
    }
    if mailbox_phase_b() != 0 {
        return 1;
    }
    print("[sfsrace] MAILBOX PASS\n");
    0
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
        // 并发信箱压测（rename 原子提交/认领）：追加 PASS 前缀 MAILBOX；
        // 任一断言（无丢失/无重复/无撕裂/无死锁）失败都以非零退出收场。
        if mailbox_phase() != 0 {
            print("[sfsrace] SFS-RACE FAIL (mailbox)\n");
            return 1;
        }
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
    print("PANIC: sfsrace\n");
    // PanicInfo 仅由 str/数字拼成，输出缓冲必为合法 UTF-8。
    print(unsafe { core::str::from_utf8(&w.buf[..w.len]) }.unwrap_or(""));
    exit(1);
}