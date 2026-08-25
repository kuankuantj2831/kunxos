// KunxOS Terminal 应用（骨架）：no_std 裸 syscall 用户态程序。
//
// 生命周期验证用途：连接 displayd -> 创建窗口 surface -> 绘制色块 ->
// 3 秒后销毁 surface 并退出，供 launcher 验证窗口回收与 z-order 管理。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;
const SYS_IOCTL: usize = 29;
const SYS_READ: usize = 63;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDWR: usize = 2;
const O_RDONLY: usize = 0;
const SEEK_END: usize = 2;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const FBIOGET_VSCREENINFO: usize = 0x4600;
const INPUT_PATH: &str = "/dev/input/event0";
const FB_PATH: &str = "/dev/fb0";

const PIPE_PATH: &str = "/var/run/displayd.pipe";
const APP_NAME: &str = "terminal";
/// 窗口色块颜色（深灰）。
const COLOR: u32 = 0x00_22_22_22;
const WIN_W: u32 = 240;
const WIN_H: u32 = 320;
/// 假定 surface id（desktop=0，首个应用窗口=1）。
const SURFACE_ID: i32 = 1;
const LIFETIME_MS: usize = 3000;

/// 色块帧缓冲（64x64）。
const BLOCK: usize = 64;
static mut FRAMEBUFFER: [u32; BLOCK * BLOCK] = [0; BLOCK * BLOCK];

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
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, in("x4") a5, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, in("x4") a5, in("x5") a6, options(nostack));
    ret
}

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_dec(mut v: u32) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 12];
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

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 文件操作与 displayd 协议 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

fn pipe_send(cmd: &[u8]) {
    unsafe {
        let fd = open(PIPE_PATH, O_RDWR);
        if fd < 0 {
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
        write_fd(fd as usize, cmd);
        syscall1(SYS_CLOSE, fd as usize);
    }
}

fn push_dec(buf: &mut [u8], pos: usize, mut v: u32) -> usize {
    if v == 0 {
        buf[pos] = b'0';
        return pos + 1;
    }
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    buf[pos..pos + n].copy_from_slice(&tmp[i..]);
    pos + n
}

fn surf_create(w: u32, h: u32) {
    let mut buf = [0u8; 64];
    let mut n = 0usize;
    for &c in b"CREATE " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, w);
    buf[n] = b' ';
    n += 1;
    n = push_dec(&mut buf, n, h);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

fn surf_flush(id: i32) {
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    for &c in b"FLUSH " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, id as u32);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

fn surf_destroy(id: i32) {
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    for &c in b"DESTROY " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, id as u32);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

// ==================== 交互式终端 ====================

const TW: usize = 540;
const TH: usize = 960;
const ROW_H: usize = 16;
const COL_W: usize = 8;
const MAX_COLS: usize = TW / COL_W;
const MAX_ROWS: usize = (TH - 24) / ROW_H;
static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_STRIDE: usize = TW;
static mut CUR_X: usize = 0;
static mut CUR_Y: usize = 0;
static mut LINE: [u8; MAX_COLS] = [0; MAX_COLS];
static mut LINE_LEN: usize = 0;
static mut HISTORY: [[u8; MAX_COLS]; 8] = [[0; MAX_COLS]; 8];
static mut HISTORY_LEN: [usize; 8] = [0; 8];
static mut HISTORY_COUNT: usize = 0;
static mut HISTORY_POS: usize = 0;
static mut SHIFT: bool = false;

fn glyph(c: u8) -> [u8; 7] {
    match c {
        b'A' => [0x0E,0x11,0x11,0x1F,0x11,0x11,0x11], b'B' => [0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E],
        b'C' => [0x0F,0x10,0x10,0x10,0x10,0x10,0x0F], b'D' => [0x1E,0x11,0x11,0x11,0x11,0x11,0x1E],
        b'E' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x1F], b'F' => [0x1F,0x10,0x10,0x1E,0x10,0x10,0x10],
        b'G' => [0x0F,0x10,0x10,0x17,0x11,0x11,0x0F], b'H' => [0x11,0x11,0x11,0x1F,0x11,0x11,0x11],
        b'I' => [0x1F,0x04,0x04,0x04,0x04,0x04,0x1F], b'J' => [0x01,0x01,0x01,0x01,0x11,0x11,0x0E],
        b'K' => [0x11,0x12,0x14,0x18,0x14,0x12,0x11], b'L' => [0x10,0x10,0x10,0x10,0x10,0x10,0x1F],
        b'M' => [0x11,0x1B,0x15,0x15,0x11,0x11,0x11], b'N' => [0x11,0x19,0x15,0x13,0x11,0x11,0x11],
        b'O' => [0x0E,0x11,0x11,0x11,0x11,0x11,0x0E], b'P' => [0x1E,0x11,0x11,0x1E,0x10,0x10,0x10],
        b'Q' => [0x0E,0x11,0x11,0x11,0x15,0x12,0x0D], b'R' => [0x1E,0x11,0x11,0x1E,0x14,0x12,0x11],
        b'S' => [0x0F,0x10,0x10,0x0E,0x01,0x01,0x1E], b'T' => [0x1F,0x04,0x04,0x04,0x04,0x04,0x04],
        b'U' => [0x11,0x11,0x11,0x11,0x11,0x11,0x0E], b'V' => [0x11,0x11,0x11,0x11,0x11,0x0A,0x04],
        b'W' => [0x11,0x11,0x11,0x15,0x15,0x15,0x0A], b'X' => [0x11,0x11,0x0A,0x04,0x0A,0x11,0x11],
        b'Y' => [0x11,0x11,0x0A,0x04,0x04,0x04,0x04], b'Z' => [0x1F,0x01,0x02,0x04,0x08,0x10,0x1F],
        b'0' => [0x0E,0x11,0x13,0x15,0x19,0x11,0x0E], b'1' => [0x04,0x0C,0x04,0x04,0x04,0x04,0x0E],
        b'2' => [0x0E,0x11,0x01,0x02,0x04,0x08,0x1F], b'3' => [0x1E,0x01,0x01,0x0E,0x01,0x01,0x1E],
        b'4' => [0x02,0x06,0x0A,0x12,0x1F,0x02,0x02], b'5' => [0x1F,0x10,0x10,0x1E,0x01,0x01,0x1E],
        b'6' => [0x0E,0x10,0x10,0x1E,0x11,0x11,0x0E], b'7' => [0x1F,0x01,0x02,0x04,0x08,0x08,0x08],
        b'8' => [0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E], b'9' => [0x0E,0x11,0x11,0x0F,0x01,0x01,0x0E],
        b'>' => [0x10,0x08,0x04,0x02,0x04,0x08,0x10], b'-' => [0; 7], b'/' => [0x01,0x02,0x02,0x04,0x08,0x08,0x10],
        b'.' => [0,0,0,0,0,0x0C,0x0C], b':' => [0,0x0C,0x0C,0,0,0x0C,0x0C], b'_' => [0,0,0,0,0,0,0x1F],
        _ => [0; 7],
    }
}

fn put_pixel(x: usize, y: usize, color: u32) { unsafe { if x < TW && y < TH { *FB_PTR.add(y * FB_STRIDE + x) = color; } } }
fn fill_screen(color: u32) { unsafe { for y in 0..TH { for x in 0..TW { *FB_PTR.add(y * FB_STRIDE + x) = color; } } } }
fn draw_char(x: usize, y: usize, c: u8, color: u32) { let rows = glyph(c.to_ascii_uppercase()); for (ry, bits) in rows.iter().enumerate() { for rx in 0..5 { if bits & (1 << (4-rx)) != 0 { put_pixel(x+rx, y+ry, color); } } } }
fn draw_text(x: usize, y: usize, s: &[u8], color: u32) { for (i, c) in s.iter().enumerate() { draw_char(x + i * COL_W, y, *c, color); } }
fn redraw_terminal() { unsafe { fill_screen(0x00101820); draw_text(8, 8, b"KUNXOS TERMINAL", 0x00A8DADC); draw_text(8, 24, b"> ", 0x00A8DADC); draw_text(24, 24, &LINE[..LINE_LEN], 0x00F1FAEE); } }

fn key_char(code: u16, shifted: bool) -> Option<u8> {
    let c = match code { 2=>b'1',3=>b'2',4=>b'3',5=>b'4',6=>b'5',7=>b'6',8=>b'7',9=>b'8',10=>b'9',11=>b'0',16=>b'q',17=>b'w',18=>b'e',19=>b'r',20=>b't',21=>b'y',22=>b'u',23=>b'i',24=>b'o',25=>b'p',30=>b'a',31=>b's',32=>b'd',33=>b'f',34=>b'g',35=>b'h',36=>b'j',37=>b'k',38=>b'l',44=>b'z',45=>b'x',46=>b'c',47=>b'v',48=>b'b',49=>b'n',50=>b'm',57=>b' ',12=>b'-',52=>b'.',53=>b'/',_=>return None}; Some(if shifted { c.to_ascii_uppercase() } else { c })
}

fn clear_line() { unsafe { LINE_LEN = 0; for c in LINE.iter_mut() { *c = 0; } } }
fn remember_line() { unsafe { if LINE_LEN == 0 { return; } for i in (1..8).rev() { HISTORY[i] = HISTORY[i-1]; HISTORY_LEN[i] = HISTORY_LEN[i-1]; } HISTORY[..1][0][..LINE_LEN].copy_from_slice(&LINE[..LINE_LEN]); HISTORY_LEN[0] = LINE_LEN; HISTORY_COUNT = (HISTORY_COUNT + 1).min(8); HISTORY_POS = 0; } }
fn show_history(pos: usize) { unsafe { clear_line(); if pos < HISTORY_COUNT { LINE_LEN = HISTORY_LEN[pos]; LINE[..LINE_LEN].copy_from_slice(&HISTORY[pos][..LINE_LEN]); } } }

fn execute_command() {
    let mut cmd = [0u8; MAX_COLS]; let len;
    unsafe { len = LINE_LEN; cmd[..len].copy_from_slice(&LINE[..len]); }
    remember_line();
    if len == 0 { return; }
    if &cmd[..len] == b"clear" { unsafe { clear_line(); CUR_Y = 0; } redraw_terminal(); return; }
    if &cmd[..len] == b"help" { print("commands: help clear history echo pkg <list|install|remove> <name>\n"); }
    else if &cmd[..len] == b"history" { print("history available in terminal\n"); }
    else if len >= 5 && &cmd[..5] == b"echo " { unsafe { write_fd(1, &cmd[5..len]); write_fd(1, b"\n"); } }
    else if len >= 4 && &cmd[..4] == b"pkg " { run_external(&cmd[4..len]); }
    else { print("command not found\n"); }
    clear_line();
}

fn run_external(command: &[u8]) {
    let mut sub = [0u8; MAX_COLS];
    let mut n = 0usize;
    while n < command.len() && command[n] != b' ' && n < 26 && n + 1 < sub.len() {
        sub[n] = command[n];
        n += 1;
    }
    if n == 0 {
        return;
    }
    let mut path = [0u8; 32];
    let path_len = 5 + n;
    path[..5].copy_from_slice(b"/bin/");
    path[5..path_len].copy_from_slice(&sub[..n]);
    path[path_len] = 0;
    let mut arg_buf = [0u8; MAX_COLS];
    arg_buf[..n].copy_from_slice(&sub[..n]);
    arg_buf[n] = 0;
    let mut argv = [0usize; 4];
    argv[0] = arg_buf.as_ptr() as usize;
    let mut arg_start = n;
    while arg_start < command.len() && command[arg_start] == b' ' { arg_start += 1; }
    if arg_start < command.len() {
        let arg_len = (command.len() - arg_start).min(arg_buf.len() - n - 1);
        arg_buf[n + 1..n + 1 + arg_len].copy_from_slice(&command[arg_start..arg_start + arg_len]);
        arg_buf[n + 1 + arg_len] = 0;
        argv[1] = arg_buf[n + 1..].as_ptr() as usize;
        argv[2] = 0;
    } else {
        argv[1] = 0;
    }
    unsafe {
        let pid = syscall5(SYS_CLONE, 17, 0, 0, 0, 0) as isize;
        if pid == 0 {
            syscall3(SYS_EXECVE, path.as_ptr() as usize, argv.as_ptr() as usize, 0);
            syscall1(SYS_EXIT, 127);
            loop {}
        } else if pid > 0 {
            let mut status = 0i32;
            syscall4(SYS_WAIT4, pid as usize, &mut status as *mut i32 as usize, 0, 0);
            print("process exited\n");
        } else {
            print("cannot start process\n");
        }
    }
}

fn read_key(fd: isize) -> Option<(u16, i32)> { let mut buf = [0u8; 48]; unsafe { let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize; if n < 24 { return None; } Some((u16::from_le_bytes([buf[18], buf[19]]), i32::from_le_bytes([buf[20],buf[21],buf[22],buf[23]]))) } }

fn interactive_main() {
    unsafe { let fd = open(FB_PATH, O_RDWR); if fd < 0 { print("terminal: framebuffer unavailable\n"); return; } let mut info=[0u8;24]; syscall3(SYS_IOCTL,fd as usize,FBIOGET_VSCREENINFO,info.as_mut_ptr() as usize); let base=syscall6(SYS_MMAP,0,TW*TH*4,PROT_READ|PROT_WRITE,MAP_SHARED,fd as usize,0); FB_PTR=base as *mut u32; FB_STRIDE=TW; syscall1(SYS_CLOSE,fd as usize); }
    if unsafe { FB_PTR.is_null() } { print("terminal: framebuffer unavailable\n"); return; }
    redraw_terminal();
    let input = unsafe { open(INPUT_PATH, O_RDONLY) };
    loop {
        if let Some((code, value)) = read_key(input) {
            unsafe {
                if code == 42 || code == 54 { SHIFT = value != 0; }
                else if code == 1 && value == 1 { print("[terminal] ESC -> exit\n"); break; }
                else if value == 1 {
                    match code { 14 => { if LINE_LEN > 0 { LINE_LEN -= 1; } }, 28 => { execute_command(); }, 103 => { if HISTORY_POS < HISTORY_COUNT { HISTORY_POS += 1; show_history(HISTORY_POS-1); } }, 108 => { if HISTORY_POS > 0 { HISTORY_POS -= 1; show_history(HISTORY_POS-1); } else { clear_line(); } }, _ => if let Some(c)=key_char(code, SHIFT) { if LINE_LEN < MAX_COLS-3 { LINE[LINE_LEN]=c; LINE_LEN+=1; } } }
                    redraw_terminal();
                }
            }
        } else { sleep_ms(8); }
    }
    print("[terminal] exiting\n");
    unsafe { syscall1(SYS_EXIT, 0); }
    loop {}
}

// ==================== 绘制 ====================

fn draw_block() {
    unsafe {
        for p in FRAMEBUFFER.iter_mut() {
            *p = COLOR;
        }
    }
    let mut lit = 0usize;
    unsafe {
        for &p in FRAMEBUFFER.iter() {
            if p != 0 {
                lit += 1;
            }
        }
    }
    print("[");
    print(APP_NAME);
    print("] color block ");
    print_dec(lit as u32);
    print(" px\n");
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[terminal] interactive shell ready\n");
    interactive_main();
    0
}

// ==================== 入口 ====================

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
fn panic(_: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: terminal\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
