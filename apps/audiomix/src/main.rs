// KunxOS audiomix：多应用混音 + 焦点抢占场景测试（EL0 用户态，裸 syscall）。
//
// 经 shell `run /bin/audiomix` 启动。场景（对应测试报告「多应用 / 焦点抢占」）：
//   1. fork 子进程 A = `audiotest 2 523 8 1`（音乐用途，523Hz，8s，持焦）
//   2. fork 子进程 B = `audiotest 3 659 8 1`（游戏用途，659Hz，8s，持焦）
//      → 策略：最高持焦 = 音乐(2)，音乐满音量、游戏被压低（0.25）
//   3. 等待 2s 后读取 A/B 的 .st 状态，断言 A=focused、B=ducked
//   4. 本进程以电话用途(0)开流并申请焦点
//      → 策略：电话持焦，音乐/游戏全部静音
//   5. 等待 2s 后断言 A/B = muted
//   6. 释放电话焦点，等待 2s，断言 A=focused、B=ducked（恢复）
//   7. wait4 回收 A/B，输出 PASS/FAIL 摘要
//
// 焦点状态读取：直接打开 /var/run/audio/<pid>.st 解析 `<gain> <state> <seq>`。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/audio.rs"]
mod audio;

use audio::*;

// ==================== 系统调用 ====================

const SYS_WRITE: usize = 64;
const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const SIGCHLD: usize = 17;

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
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

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
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
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

// ==================== 进程原语 ====================

unsafe fn fork() -> isize {
    syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as isize
}

unsafe fn execve(path: &[u8], argv: *const usize) -> isize {
    syscall3(SYS_EXECVE, path.as_ptr() as usize, argv as usize, 0) as isize
}

unsafe fn wait4(pid: i32, status: *mut i32) -> isize {
    syscall4(SYS_WAIT4, pid as usize, status as usize, 0, 0) as isize
}

/// 派生 `audiotest <usage> <freq> <secs> 1` 子进程，返回子 pid。
unsafe fn spawn_tone(usage: u8, freq: u32, secs: u32) -> i32 {
    let pid = fork();
    if pid == 0 {
        // 子进程：execve /bin/audiotest
        static mut P: [u8; 32] = [0; 32];
        static mut A0: [u8; 16] = [0; 16];
        static mut A1: [u8; 16] = [0; 16];
        static mut A2: [u8; 16] = [0; 16];
        static mut A3: [u8; 16] = [0; 16];
        static mut A4: [u8; 16] = [0; 16];
        static mut ARGV: [usize; 6] = [0; 6];
        let p = &mut *core::ptr::addr_of_mut!(P);
        let a0 = &mut *core::ptr::addr_of_mut!(A0);
        let a1 = &mut *core::ptr::addr_of_mut!(A1);
        let a2 = &mut *core::ptr::addr_of_mut!(A2);
        let a3 = &mut *core::ptr::addr_of_mut!(A3);
        let a4 = &mut *core::ptr::addr_of_mut!(A4);
        let argv = &mut *core::ptr::addr_of_mut!(ARGV);
        let path = b"/bin/audiotest";
        for (i, &c) in path.iter().enumerate() {
            p[i] = c;
        }
        p[path.len()] = 0;
        // 数字参数转字符串
        itoa(a0, usage as u32);
        itoa(a1, freq);
        itoa(a2, secs);
        a3[0] = b'1';
        a3[1] = 0;
        a4[0] = 0;
        argv[0] = p.as_ptr() as usize;
        argv[1] = a0.as_ptr() as usize;
        argv[2] = a1.as_ptr() as usize;
        argv[3] = a2.as_ptr() as usize;
        argv[4] = a3.as_ptr() as usize;
        argv[5] = 0;
        execve(&p[..path.len() + 1], argv.as_ptr());
        // execve 失败
        write_fd(2, "audiomix: execve audiotest 失败\n".as_bytes());
        syscall1(SYS_EXIT, 127);
        return 0;
    }
    pid as i32
}

/// 十进制写入 NUL 结尾字符串缓冲。
fn itoa(buf: &mut [u8], mut v: u32) {
    if v == 0 {
        buf[0] = b'0';
        buf[1] = 0;
        return;
    }
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    buf[..n].copy_from_slice(&tmp[i..]);
    buf[n] = 0;
}

// ==================== 状态读取 ====================

static mut PATH_BUF: [u8; 64] = [0; 64];

/// 读取任意 pid 的焦点状态：返回 `(gain_Q15, state)`；文件不存在返回 (-1, 99)。
fn read_stream_state(pid: i32) -> (i32, u32) {
    unsafe {
        let p = PATH_BUF.as_mut_ptr();
        let d = b"/var/run/audio/";
        let mut n = 0usize;
        for &c in d.iter() {
            *p.add(n) = c;
            n += 1;
        }
        let mut tmp = [0u8; 12];
        let mut i = tmp.len();
        let mut v = pid as u32;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in tmp.iter().skip(i) {
            *p.add(n) = c;
            n += 1;
        }
        for &c in b".st".iter() {
            *p.add(n) = c;
            n += 1;
        }
        *p.add(n) = 0;
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, p as usize, O_RDONLY) as i32;
        if fd < 0 {
            return (-1, 99);
        }
        let mut buf = [0u8; 32];
        let r = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
        syscall1(SYS_CLOSE, fd as usize);
        if (r as usize) < 4 {
            return (-1, 99);
        }
        let mut gain: i32 = 0;
        let mut k = 0usize;
        while k < (r as usize) && buf[k].is_ascii_digit() {
            gain = gain * 10 + (buf[k] - b'0') as i32;
            k += 1;
        }
        while k < (r as usize) && buf[k] == b' ' {
            k += 1;
        }
        let mut state: u32 = 0;
        while k < (r as usize) && buf[k].is_ascii_digit() {
            state = state * 10 + (buf[k] - b'0') as u32;
            k += 1;
        }
        (gain, state)
    }
}

fn state_name(s: u32) -> &'static str {
    match s {
        FOCUS_FOCUSED => "focused",
        FOCUS_DUCKED => "ducked",
        FOCUS_MUTED => "muted",
        _ => "unknown",
    }
}

// ==================== 场景 ====================

static mut FAILS: u32 = 0;

fn check(name: &str, cond: bool) {
    print("[audiomix] ");
    print(if cond { "PASS " } else { "FAIL " });
    print(name);
    print("\n");
    if !cond {
        unsafe { FAILS += 1 };
    }
}

fn report_phase(tag: &str, pid_a: i32, pid_b: i32) {
    let (ga, sa) = read_stream_state(pid_a);
    let (gb, sb) = read_stream_state(pid_b);
    print("[audiomix] ");
    print(tag);
    print(": 音乐(pid ");
    print_dec(pid_a as u64);
    print(")=");
    if sa != 99 {
        print(state_name(sa));
        print(":");
        print_dec(ga as u64);
    } else {
        print("n/a");
    }
    print(" 游戏(pid ");
    print_dec(pid_b as u64);
    print(")=");
    if sb != 99 {
        print(state_name(sb));
        print(":");
        print_dec(gb as u64);
    } else {
        print("n/a");
    }
    print("\n");
}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[audiomix] 多应用混音 + 焦点抢占场景\n");

    // 1. fork 音乐 + 游戏两个子进程（各自持焦）
    let pid_a = unsafe { spawn_tone(USAGE_MUSIC, 523, 8) };
    let pid_b = unsafe { spawn_tone(USAGE_GAME, 659, 8) };
    print("[audiomix] 已生成音乐 pid=");
    print_dec(pid_a as u64);
    print(" 游戏 pid=");
    print_dec(pid_b as u64);
    print("\n");
    if pid_a <= 0 || pid_b <= 0 {
        print("[audiomix] fork 失败，中止\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }

    // 2. 等待子进程注册并开播（混音应同时混合两路）
    sleep_ms(2000);
    report_phase("阶段1 音乐+游戏", pid_a, pid_b);
    let (_, sa) = read_stream_state(pid_a);
    let (_, sb) = read_stream_state(pid_b);
    check("音乐聚焦 -> 满音量", sa == FOCUS_FOCUSED);
    check("游戏被音乐压低", sb == FOCUS_DUCKED);

    // 3. 电话申请焦点：音乐/游戏全部静音
    print("[audiomix] 电话请求焦点（抢占）\n");
    if open_stream(USAGE_PHONE) != 0 {
        print("[audiomix] 电话 open_stream 失败\n");
    }
    request_focus();
    sleep_ms(2000);
    report_phase("阶段2 电话抢占", pid_a, pid_b);
    let (_, sa) = read_stream_state(pid_a);
    let (_, sb) = read_stream_state(pid_b);
    check("音乐被电话静音", sa == FOCUS_MUTED);
    check("游戏被电话静音", sb == FOCUS_MUTED);

    // 4. 释放电话焦点：恢复音乐全量、游戏压低
    release_focus();
    sleep_ms(2000);
    report_phase("阶段3 电话释放", pid_a, pid_b);
    let (_, sa) = read_stream_state(pid_a);
    let (_, sb) = read_stream_state(pid_b);
    check("音乐恢复满音量", sa == FOCUS_FOCUSED);
    check("游戏恢复低音量", sb == FOCUS_DUCKED);

    // 5. 回收子进程
    let mut status = 0i32;
    let r1 = unsafe { wait4(pid_a, &mut status) };
    let r2 = unsafe { wait4(pid_b, &mut status) };
    close_stream();

    print("[audiomix] 子进程已回收 r1=");
    print_dec(r1 as u64);
    print(" r2=");
    print_dec(r2 as u64);
    print("\n");

    let fails = unsafe { FAILS };
    print("[audiomix] ");
    if fails == 0 {
        print("ALL PASS\n");
    } else {
        print("失败数=");
        print_dec(fails as u64);
        print("\n");
    }
    unsafe { syscall1(SYS_EXIT, if fails == 0 { 0 } else { 1 }) };
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
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
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
    unsafe {
        write_fd(2, b"PANIC: audiomix\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
