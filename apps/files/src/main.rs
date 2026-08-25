// KunxOS 文件管理器应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/files）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 浏览模式：getdents64 列出目录项 + newfstatat 获取大小，
//      点击目录进入、点击 ".." 返回上级、点击文件打开查看器
//   3. 查看模式：打开文件读取前 5KB，按文本渲染，点击上下半屏翻页
//   4. 点击左上角「返回」或按 ESC 逐级退出（查看器 → 浏览 → 退出应用）
//
// 说明：framebuffer 由内核启动时初始化，应用直接 open/mmap /dev/fb0 覆盖桌面
// 全屏绘制；无窗口管理器，退出后桌面由 launcher 重绘。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;
const SYS_GETDENTS64: usize = 61;
const SYS_NEWFSTATAT: usize = 79;

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NONBLOCK: usize = 0x800;

/// mmap 保护位与标志。
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";

/// linux input_event 事件类型 / 码。
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a; // 330
const BTN_LEFT: u16 = 0x110; // 272

/// dirent64 d_type 值（DT_*，与内核 dt_of 一致）。
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

// ==================== 显示参数 ====================

const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;
const TITLE_H: i32 = 44;
const ROW_H: i32 = 36;
const LIST_TOP: i32 = TITLE_H + 4;
const MAX_ENTRIES: usize = 96;
const PATH_STACK_DEPTH: usize = 8;
const VIEW_CAP: usize = 5120;

// ==================== 颜色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_ROW: u32 = 0x00_1B_2B_4A;
const C_ROW_BORDER: u32 = 0x00_3A_4A_6A;
const C_TITLE: u32 = 0x00_16_22_38;
const C_ACCENT: u32 = 0x00_22_C5_5E;
const C_BACK: u32 = 0x00_35_B0_E6;
const C_TEXT: u32 = 0xFF_EC_F0_F1;
const C_DIM: u32 = 0x00_9A_A5_B5;
const C_SYM: u32 = 0x00_E8_A0_4C;

// ==================== 8x8 位图字体（公有领域 IBM VGA 风格，取自内核 ui_framework） ====================

static FONT8X8: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x18, 0x3C, 0x3C, 0x18, 0x18, 0x00, 0x18, 0x00], // !
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // "
    [0x36, 0x36, 0x7F, 0x36, 0x7F, 0x36, 0x36, 0x00], // #
    [0x0C, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x0C, 0x00], // $
    [0x00, 0x63, 0x33, 0x18, 0x0C, 0x66, 0x63, 0x00], // %
    [0x1C, 0x36, 0x1C, 0x6E, 0x3B, 0x33, 0x6E, 0x00], // &
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00], // '
    [0x18, 0x0C, 0x06, 0x06, 0x06, 0x0C, 0x18, 0x00], // (
    [0x06, 0x0C, 0x18, 0x18, 0x18, 0x0C, 0x06, 0x00], // )
    [0x00, 0x66, 0x3C, 0xFF, 0x3C, 0x66, 0x00, 0x00], // *
    [0x00, 0x0C, 0x0C, 0x3F, 0x0C, 0x0C, 0x00, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ,
    [0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C, 0x00], // .
    [0x60, 0x30, 0x18, 0x0C, 0x06, 0x03, 0x01, 0x00], // /
    [0x3E, 0x63, 0x73, 0x7B, 0x6F, 0x67, 0x3E, 0x00], // 0
    [0x0C, 0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x3F, 0x00], // 1
    [0x1E, 0x33, 0x30, 0x1C, 0x06, 0x33, 0x3F, 0x00], // 2
    [0x1E, 0x33, 0x30, 0x1C, 0x30, 0x33, 0x1E, 0x00], // 3
    [0x38, 0x3C, 0x36, 0x33, 0x7F, 0x30, 0x78, 0x00], // 4
    [0x3F, 0x03, 0x1F, 0x30, 0x30, 0x33, 0x1E, 0x00], // 5
    [0x1C, 0x06, 0x03, 0x1F, 0x33, 0x33, 0x1E, 0x00], // 6
    [0x3F, 0x33, 0x30, 0x18, 0x0C, 0x0C, 0x0C, 0x00], // 7
    [0x1E, 0x33, 0x33, 0x1E, 0x33, 0x33, 0x1E, 0x00], // 8
    [0x1E, 0x33, 0x33, 0x3E, 0x30, 0x18, 0x0E, 0x00], // 9
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x00], // :
    [0x00, 0x0C, 0x0C, 0x00, 0x00, 0x0C, 0x0C, 0x06], // ;
    [0x18, 0x0C, 0x06, 0x03, 0x06, 0x0C, 0x18, 0x00], // <
    [0x00, 0x00, 0x3F, 0x00, 0x00, 0x3F, 0x00, 0x00], // =
    [0x06, 0x0C, 0x18, 0x30, 0x18, 0x0C, 0x06, 0x00], // >
    [0x1E, 0x33, 0x30, 0x18, 0x0C, 0x00, 0x0C, 0x00], // ?
    [0x3E, 0x63, 0x7B, 0x7B, 0x7B, 0x03, 0x1E, 0x00], // @
    [0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00], // A
    [0x3F, 0x66, 0x66, 0x3E, 0x66, 0x66, 0x3F, 0x00], // B
    [0x3C, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3C, 0x00], // C
    [0x1F, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1F, 0x00], // D
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x46, 0x7F, 0x00], // E
    [0x7F, 0x46, 0x16, 0x1E, 0x16, 0x06, 0x0F, 0x00], // F
    [0x3C, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7C, 0x00], // G
    [0x33, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x33, 0x00], // H
    [0x1E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // I
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E, 0x00], // J
    [0x67, 0x66, 0x36, 0x1E, 0x36, 0x66, 0x67, 0x00], // K
    [0x0F, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7F, 0x00], // L
    [0x63, 0x77, 0x7F, 0x7F, 0x6B, 0x63, 0x63, 0x00], // M
    [0x63, 0x67, 0x6F, 0x7B, 0x73, 0x63, 0x63, 0x00], // N
    [0x1C, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1C, 0x00], // O
    [0x3F, 0x66, 0x66, 0x3E, 0x06, 0x06, 0x0F, 0x00], // P
    [0x1E, 0x33, 0x33, 0x33, 0x3B, 0x1E, 0x38, 0x00], // Q
    [0x3F, 0x66, 0x66, 0x3E, 0x36, 0x66, 0x67, 0x00], // R
    [0x1E, 0x33, 0x07, 0x0E, 0x38, 0x33, 0x1E, 0x00], // S
    [0x3F, 0x2D, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // T
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3F, 0x00], // U
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // V
    [0x63, 0x63, 0x63, 0x6B, 0x7F, 0x77, 0x63, 0x00], // W
    [0x63, 0x63, 0x36, 0x1C, 0x1C, 0x36, 0x63, 0x00], // X
    [0x33, 0x33, 0x33, 0x1E, 0x0C, 0x0C, 0x1E, 0x00], // Y
    [0x7F, 0x63, 0x31, 0x18, 0x4C, 0x66, 0x7F, 0x00], // Z
    [0x1E, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1E, 0x00], // [
    [0x03, 0x06, 0x0C, 0x18, 0x30, 0x60, 0x40, 0x00], // backslash
    [0x1E, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1E, 0x00], // ]
    [0x08, 0x1C, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF], // _
    [0x0C, 0x0C, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x1E, 0x30, 0x3E, 0x33, 0x6E, 0x00], // a
    [0x07, 0x06, 0x06, 0x3E, 0x66, 0x66, 0x3B, 0x00], // b
    [0x00, 0x00, 0x1E, 0x33, 0x03, 0x33, 0x1E, 0x00], // c
    [0x38, 0x30, 0x30, 0x3E, 0x33, 0x33, 0x6E, 0x00], // d
    [0x00, 0x00, 0x1E, 0x33, 0x3F, 0x03, 0x1E, 0x00], // e
    [0x1C, 0x36, 0x06, 0x0F, 0x06, 0x06, 0x0F, 0x00], // f
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x1F], // g
    [0x07, 0x06, 0x36, 0x6E, 0x66, 0x66, 0x67, 0x00], // h
    [0x0C, 0x00, 0x0E, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // i
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1E], // j
    [0x07, 0x06, 0x66, 0x36, 0x1E, 0x36, 0x67, 0x00], // k
    [0x0E, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x1E, 0x00], // l
    [0x00, 0x00, 0x33, 0x7F, 0x7F, 0x6B, 0x63, 0x00], // m
    [0x00, 0x00, 0x1F, 0x33, 0x33, 0x33, 0x33, 0x00], // n
    [0x00, 0x00, 0x1E, 0x33, 0x33, 0x33, 0x1E, 0x00], // o
    [0x00, 0x00, 0x3B, 0x66, 0x66, 0x3E, 0x06, 0x0F], // p
    [0x00, 0x00, 0x6E, 0x33, 0x33, 0x3E, 0x30, 0x78], // q
    [0x00, 0x00, 0x3B, 0x6E, 0x66, 0x06, 0x0F, 0x00], // r
    [0x00, 0x00, 0x3E, 0x03, 0x1E, 0x30, 0x1F, 0x00], // s
    [0x08, 0x0C, 0x3E, 0x0C, 0x0C, 0x2C, 0x18, 0x00], // t
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6E, 0x00], // u
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1E, 0x0C, 0x00], // v
    [0x00, 0x00, 0x63, 0x6B, 0x7F, 0x7F, 0x36, 0x00], // w
    [0x00, 0x00, 0x63, 0x36, 0x1C, 0x36, 0x63, 0x00], // x
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3E, 0x30, 0x1F], // y
    [0x00, 0x00, 0x3F, 0x19, 0x0C, 0x26, 0x3F, 0x00], // z
    [0x38, 0x0C, 0x0C, 0x07, 0x0C, 0x0C, 0x38, 0x00], // {
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00], // |
    [0x07, 0x0C, 0x0C, 0x38, 0x0C, 0x0C, 0x07, 0x00], // }
    [0x6E, 0x3B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ~
];

// ==================== 共享 CJK 16x16 点阵字库（由 tools/gen_cjk_font.py 生成子集） ====================

#[path = "../../../shared/cjk_font_files.rs"]
mod cjk_font;

// ==================== 状态 ====================

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
static mut FB_STRIDE_PX: usize = DISPLAY_W;
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

static mut INPUT_FD: isize = -1;
static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut TOUCH_DOWN: bool = false;
static mut MOUSE_DOWN: bool = false;

/// 鼠标指针位图（9x9）：经典箭头，黑描边 + 白填充。
static CURSOR_BITS: [u16; 9] = [
    0b100000000,
    0b110000000,
    0b101000000,
    0b100100000,
    0b100010000,
    0b100001000,
    0b111111100,
    0b100010100,
    0b100010010,
];

// ---- 浏览状态 ----

/// 当前目录（绝对路径，以 '\0' 结尾）。初始值在 k_main 中置为 "/"。
static mut CWD: [u8; 128] = [0; 128];
/// 返回路径栈（进入子目录时压入；".." / 返回键弹出）。
static mut PATH_STACK: [[u8; 128]; PATH_STACK_DEPTH] = [[0; 128]; PATH_STACK_DEPTH];
static mut STACK_DEPTH: usize = 0;
/// 列表滚动偏移（条目下标）。
static mut LIST_SCROLL: usize = 0;

/// 目录项缓冲（最多 96 项）。
static mut ENT_NAMES: [[u8; 32]; MAX_ENTRIES] = [[0; 32]; MAX_ENTRIES];
static mut ENT_LEN: [usize; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_TYPE: [u8; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_SIZE: [u32; MAX_ENTRIES] = [0; MAX_ENTRIES];
static mut ENT_COUNT: usize = 0;

// ---- 查看状态 ----

static mut VIEW_MODE: bool = false;
static mut VIEW_PATH: [u8; 128] = [0; 128];
static mut VIEW_BUF: [u8; VIEW_CAP] = [0; VIEW_CAP];
static mut VIEW_LEN: usize = 0;
/// 查看器行滚动（行号偏移）。
static mut VIEW_SCROLL: usize = 0;

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
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, in("x1") a2, options(nostack));
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
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        in("x4") a5,
        in("x5") a6,
        options(nostack)
    );
    ret
}

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); }
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
    unsafe { write_fd(1, &buf[i..]); }
}

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe { syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0); }
}

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

/// 把 `path` 拷贝到 PATH_BUF（NUL 结尾）并 openat。
unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// 统计 `path` 的 (类型码, 大小)。类型码 = 0 表示 stat 失败。
fn stat_size(path: &str) -> (u8, u32) {
    unsafe {
        let mut st = [0u8; 128];
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
        PATH_BUF[n] = 0;
        let r = syscall4(
            SYS_NEWFSTATAT,
            AT_FDCWD,
            PATH_BUF.as_ptr() as usize,
            st.as_mut_ptr() as usize,
            0,
        );
        if r != 0 {
            return (0, 0);
        }
        // struct stat：st_mode@16(u32)，st_size@48(i64)
        let mode = u32::from_le_bytes([st[16], st[17], st[18], st[19]]);
        let size = i64::from_le_bytes([
            st[48], st[49], st[50], st[51], st[52], st[53], st[54], st[55],
        ]);
        let typ = match mode & 0o170000 {
            0o040000 => DT_DIR,
            0o120000 => DT_LNK,
            0o020000 => 2, // 字符设备
            0o060000 => 6, // 块设备
            0o010000 => 1, // FIFO
            _ => DT_REG,
        };
        (typ, size.max(0) as u32)
    }
}

/// 读取目录 `path` 的全部条目到 ENT_* 缓冲，按名称字典序排序。
fn load_dir(path: &str) {
    unsafe {
        ENT_COUNT = 0;
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return;
        }
        let mut raw = [0u8; 2048];
        let mut names: [[u8; 32]; MAX_ENTRIES] = [[0; 32]; MAX_ENTRIES];
        let mut lens: [usize; MAX_ENTRIES] = [0; MAX_ENTRIES];
        let mut types: [u8; MAX_ENTRIES] = [0; MAX_ENTRIES];
        let mut count = 0usize;
        loop {
            let n = syscall3(
                SYS_GETDENTS64,
                fd as usize,
                raw.as_mut_ptr() as usize,
                raw.len(),
            );
            if n <= 0 {
                break;
            }
            let mut off = 0usize;
            while off + 19 <= n as usize {
                // d_ino u64@0, d_off u64@8, d_reclen u16@16, d_type u8@18, d_name@19
                let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
                if reclen < 19 || off + reclen > n as usize {
                    break;
                }
                let dtyp = raw[off + 18];
                let name = &raw[off + 19..off + reclen];
                let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                if count < MAX_ENTRIES {
                    names[count][..elen.min(31)].copy_from_slice(&name[..elen.min(31)]);
                    lens[count] = elen.min(31);
                    types[count] = dtyp;
                    count += 1;
                }
                off += reclen;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);

        // 插入排序（字节字典序；"." / ".." 不参与排序，固定在最前）。
        // 先抽出 ".."（若有），其余排序后放回。
        let mut dots: [[u8; 32]; 2] = [[0; 32]; 2];
        let mut dot_lens = [0usize; 2];
        let mut dot_count = 0usize;
        let mut rest: [[u8; 32]; MAX_ENTRIES] = [[0; 32]; MAX_ENTRIES];
        let mut rest_len = [0usize; MAX_ENTRIES];
        let mut rest_typ = [0u8; MAX_ENTRIES];
        let mut rest_n = 0usize;
        for i in 0..count {
            let nm = &names[i][..lens[i]];
            if nm == b".." || nm == b"." {
                if dot_count < 2 {
                    dots[dot_count][..lens[i]].copy_from_slice(nm);
                    dot_lens[dot_count] = lens[i];
                    dot_count += 1;
                }
            } else {
                rest[rest_n][..lens[i]].copy_from_slice(nm);
                rest_len[rest_n] = lens[i];
                rest_typ[rest_n] = types[i];
                rest_n += 1;
            }
        }
        for i in 1..rest_n {
            let mut j = i;
            while j > 0 {
                let a = &rest[j - 1][..rest_len[j - 1]];
                let b = &rest[j][..rest_len[j]];
                if a <= b {
                    break;
                }
                // 交换 j-1 与 j（先拷贝到临时缓冲，避免数组元素借用冲突）
                let al = rest_len[j - 1];
                let bl = rest_len[j];
                let mut ta = [0u8; 32];
                let mut tb = [0u8; 32];
                ta[..al].copy_from_slice(&rest[j - 1][..al]);
                tb[..bl].copy_from_slice(&rest[j][..bl]);
                rest[j - 1][..bl].copy_from_slice(&tb[..bl]);
                rest_len[j - 1] = bl;
                let tt = rest_typ[j - 1];
                rest_typ[j - 1] = rest_typ[j];
                rest_typ[j] = tt;
                rest[j][..al].copy_from_slice(&ta[..al]);
                rest_len[j] = al;
                j -= 1;
            }
        }
        // 合并：.. / . 在最前，其余跟随
        count = 0;
        for d in 0..dot_count {
            ENT_NAMES[count][..dot_lens[d]].copy_from_slice(&dots[d][..dot_lens[d]]);
            ENT_LEN[count] = dot_lens[d];
            ENT_TYPE[count] = DT_DIR;
            ENT_SIZE[count] = 0;
            count += 1;
        }
        for i in 0..rest_n {
            ENT_NAMES[count][..rest_len[i]].copy_from_slice(&rest[i][..rest_len[i]]);
            ENT_LEN[count] = rest_len[i];
            ENT_TYPE[count] = rest_typ[i];
            ENT_SIZE[count] = 0;
            count += 1;
        }
        ENT_COUNT = count;
        // 逐项 stat 取大小（跳过 ".." / "."）
        for i in 0..ENT_COUNT {
            let nm = &ENT_NAMES[i][..ENT_LEN[i]];
            if nm == b".." || nm == b"." {
                continue;
            }
            let mut full = [0u8; 160];
            let mut off = 0usize;
            for &c in path.as_bytes() {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = c;
                off += 1;
            }
            if off > 0 && full[off - 1] != b'/' && off + 1 < full.len() {
                full[off] = b'/';
                off += 1;
            }
            for &c in nm {
                if off + 1 >= full.len() {
                    break;
                }
                full[off] = c;
                off += 1;
            }
            full[off] = 0;
            let fs = core::str::from_utf8(&full[..off]).unwrap_or("");
            let (t, sz) = stat_size(fs);
            ENT_TYPE[i] = t;
            ENT_SIZE[i] = sz;
        }
    }
}

/// 拼接 cwd + "/" + name 到 `out`（NUL 结尾），返回字节数；放不下返回 0。
fn join_path(cwd: &[u8], name: &[u8], out: &mut [u8]) -> usize {
    let mut off = 0usize;
    for &c in cwd {
        if off + 1 >= out.len() {
            return 0;
        }
        out[off] = c;
        off += 1;
    }
    if off > 0 && out[off - 1] != b'/' && off + 1 < out.len() {
        out[off] = b'/';
        off += 1;
    }
    for &c in name {
        if off + 1 >= out.len() {
            return 0;
        }
        out[off] = c;
        off += 1;
    }
    out[off] = 0;
    off
}

/// 取 cwd 的父目录写入 `out`（NUL 结尾），返回长度。
fn parent_of(cwd: &[u8], out: &mut [u8]) -> usize {
    let mut end = cwd.len();
    while end > 0 && cwd[end - 1] == 0 {
        end -= 1;
    }
    while end > 0 && cwd[end - 1] == b'/' {
        end -= 1;
    }
    let mut slash = end;
    while slash > 0 && cwd[slash - 1] != b'/' {
        slash -= 1;
    }
    if slash == 0 {
        out[0] = b'/';
        out[1] = 0;
        1
    } else {
        let n = slash.min(out.len() - 1);
        out[..n].copy_from_slice(&cwd[..n]);
        out[n] = 0;
        n
    }
}

// ==================== 帧缓冲 ====================

/// 打开 /dev/fb0、查询分辨率并 mmap 到用户空间；成功返回 true。
fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        let mut info = [0u8; 24];
        let r = syscall3(SYS_IOCTL, fd as usize, FBIOGET_VSCREENINFO, info.as_mut_ptr() as usize);
        if r == 0 {
            let w = u32::from_le_bytes([info[0], info[1], info[2], info[3]]) as usize;
            let h = u32::from_le_bytes([info[4], info[5], info[6], info[7]]) as usize;
            let stride = u32::from_le_bytes([info[20], info[21], info[22], info[23]]) as usize;
            if w > 0 && h > 0 {
                FB_W = w;
                FB_H = h;
                FB_STRIDE_PX = if stride >= w * 4 { stride / 4 } else { w };
            }
        }
        let len = FB_W * FB_H * 4;
        let base = syscall6(
            SYS_MMAP,
            0,
            len,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd as usize,
            0,
        );
        if (base as isize) < 0 {
            syscall1(SYS_CLOSE, fd as usize);
            return false;
        }
        FB_PTR = base as *mut u32;
        syscall1(SYS_CLOSE, fd as usize);
        true
    }
}

/// 把 back buffer 一次性拷贝到显存。
fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
            core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB_PTR, DISPLAY_W * DISPLAY_H);
        } else {
            let w = FB_W.min(DISPLAY_W);
            let h = FB_H.min(DISPLAY_H);
            for y in 0..h {
                core::ptr::copy_nonoverlapping(
                    BACK_BUF.as_ptr().add(y * DISPLAY_W),
                    FB_PTR.add(y * FB_STRIDE_PX),
                    w,
                );
            }
        }
    }
}

// ==================== 绘制原语 ====================

fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = color; }
}

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dy in 0..h {
        for dx in 0..w {
            put_pixel(x + dx, y + dy, color);
        }
    }
}

fn fill_screen(color: u32) {
    for y in 0..DISPLAY_H as i32 {
        for x in 0..DISPLAY_W as i32 {
            put_pixel(x, y, color);
        }
    }
}

fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    if c < 0x20 || c > 0x7E {
        return; // 控制字符 / 非 ASCII 不画（由 CJK 路径处理）
    }
    let glyph = FONT8X8[(c - 0x20) as usize];
    for (ry, bits) in glyph.iter().enumerate() {
        for rx in 0..8 {
            if bits & (1 << (7 - rx)) != 0 {
                put_pixel(x + rx as i32, y + ry as i32, color);
            }
        }
    }
}

/// 绘制 UTF-8 文本：ASCII 用 8x8，CJK 用 16x16 点阵。返回绘制后的 x 位置。
fn draw_text(x: i32, y: i32, s: &[u8], color: u32) -> i32 {
    let mut cx = x;
    let mut i = 0usize;
    while i < s.len() {
        let (cp, n) = cjk_font::utf8_next(s, i);
        if cp >= 0x80 {
            if let Some(idx) = cjk_font::cjk_index(cp as u16) {
                for row in 0..cjk_font::CJK_H {
                    let b0 = cjk_font::CJK_BITMAPS[idx * 32 + row as usize * 2];
                    let b1 = cjk_font::CJK_BITMAPS[idx * 32 + row as usize * 2 + 1];
                    for rx in 0..8 {
                        if b0 & (1 << (7 - rx)) != 0 {
                            put_pixel(cx + rx, y + row, color);
                        }
                        if b1 & (1 << (7 - rx)) != 0 {
                            put_pixel(cx + 8 + rx, y + row, color);
                        }
                    }
                }
            }
            cx += cjk_font::CJK_W;
        } else {
            if cp == b'\t' as u32 {
                cx += 8 * 4;
            } else if cp != b'\r' as u32 && cp != b'\n' as u32 {
                draw_char(cx, y, cp as u8, color);
                cx += 8;
            }
        }
        i += n;
    }
    cx
}

fn draw_cursor() {
    unsafe {
        let (cx, cy) = (CUR_X, CUR_Y);
        for (ry, bits) in CURSOR_BITS.iter().enumerate() {
            for rx in 0..9 {
                if bits & (1 << (8 - rx)) != 0 {
                    put_pixel(cx + rx as i32, cy + ry as i32, 0xFF_00_00_00);
                }
            }
        }
        for (ry, bits) in CURSOR_BITS.iter().enumerate() {
            for rx in 0..9 {
                if bits & (1 << (8 - rx)) != 0 {
                    put_pixel(cx + rx as i32 + 1, cy + ry as i32 + 1, 0xFF_FF_FF_FF);
                }
            }
        }
    }
}

// ==================== 渲染 ====================

/// 标题栏：返回按钮 + 应用名 + 当前路径。
fn render_title() {
    unsafe {
        fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE);
        // 返回按钮
        fill_rect(4, 4, 36, TITLE_H - 8, C_BACK);
        draw_text(12, 10, b"<", C_TEXT);
        draw_text(48, 10, "文件管理器".as_bytes(), C_TEXT);
        // 路径（右侧，截断显示）
        let mut plen = 0usize;
        while plen < CWD.len() && CWD[plen] != 0 {
            plen += 1;
        }
        if plen > 0 {
            let p = &CWD[..plen];
            // 计算宽度，超宽从右侧截断
            let maxw = DISPLAY_W as i32 - 48 - 170;
            let mut w = 0i32;
            let mut cut = plen;
            for i in (0..plen).rev() {
                let (cp, n) = cjk_font::utf8_next(p, i);
                let cw = if cp >= 0x80 { cjk_font::CJK_W } else { 8 };
                if w + cw > maxw {
                    cut = i + n;
                    break;
                }
                w += cw;
            }
            if cut < plen {
                draw_text(DISPLAY_W as i32 - 8 - w, 12, b"...", C_DIM);
            }
            draw_text(DISPLAY_W as i32 - 8 - w, 12, &p[cut..], C_DIM);
        }
    }
}

fn format_size(v: u32, out: &mut [u8]) -> usize {
    // 输出形如 " 12KB" / " 3MB" / " 512B"
    let mut off = 0usize;
    out[off] = b' ';
    off += 1;
    let mut tmp = [0u8; 8];
    let mut i = tmp.len();
    let mut n = v;
    if n >= 1024 * 1024 {
        n /= 1024 * 1024;
        while n > 0 {
            i -= 1;
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        let len = tmp.len() - i;
        out[off..off + len].copy_from_slice(&tmp[i..]);
        off += len;
        out[off] = b'M';
        out[off + 1] = b'B';
        off + 2
    } else if n >= 1024 {
        n /= 1024;
        while n > 0 {
            i -= 1;
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        let len = tmp.len() - i;
        out[off..off + len].copy_from_slice(&tmp[i..]);
        off += len;
        out[off] = b'K';
        out[off + 1] = b'B';
        off + 2
    } else {
        // 0 字节显示 "0B"
        if n == 0 {
            tmp[tmp.len() - 1] = b'0';
            i = tmp.len() - 1;
        } else {
            while n > 0 {
                i -= 1;
                tmp[i] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        let len = tmp.len() - i;
        out[off..off + len].copy_from_slice(&tmp[i..]);
        off += len;
        out[off] = b'B';
        off + 1
    }
}

fn render_list() {
    unsafe {
        // 列表区域背景
        fill_rect(0, LIST_TOP, DISPLAY_W as i32, DISPLAY_H as i32 - LIST_TOP, C_BG);
        let rows_visible = ((DISPLAY_H as i32 - LIST_TOP) / ROW_H) as usize;
        let scroll = LIST_SCROLL.min(ENT_COUNT.saturating_sub(rows_visible));
        for r in 0..rows_visible {
            let idx = scroll + r;
            if idx >= ENT_COUNT {
                break;
            }
            let y = LIST_TOP + r as i32 * ROW_H;
            fill_rect(0, y, DISPLAY_W as i32, ROW_H - 2, C_ROW);
            let nm = &ENT_NAMES[idx][..ENT_LEN[idx]];
            // 类型标记
            let (tag, tag_color, name_color) = match ENT_TYPE[idx] {
                DT_DIR => (b"D", C_ACCENT, C_TEXT),
                DT_LNK => (b"L", C_SYM, C_SYM),
                2 => (b"C", C_BACK, C_TEXT),
                _ => (b"R", C_DIM, C_TEXT),
            };
            draw_char(12, y + (ROW_H - 8) / 2, tag[0], tag_color);
            // 名称（超宽截断）
            let namew = DISPLAY_W as i32 - 24 - 100;
            let mut w = 0i32;
            let mut cut = ENT_LEN[idx];
            for i in (0..ENT_LEN[idx]).rev() {
                let (cp, n) = cjk_font::utf8_next(nm, i);
                let cw = if cp >= 0x80 { cjk_font::CJK_W } else { 8 };
                if w + cw > namew {
                    cut = i + n;
                    break;
                }
                w += cw;
            }
            draw_text(28, y + (ROW_H - 8) / 2, &nm[..cut], name_color);
            if ENT_TYPE[idx] == DT_LNK {
                draw_text(28 + w, y + (ROW_H - 8) / 2, b" ->", C_SYM);
            }
            // 大小（右对齐）
            if ENT_TYPE[idx] != DT_DIR && ENT_TYPE[idx] != DT_LNK && ENT_TYPE[idx] != 2 {
                let mut szb = [0u8; 16];
                let sl = format_size(ENT_SIZE[idx], &mut szb);
                draw_text(DISPLAY_W as i32 - 12 - cjk_font::text_width(&szb[..sl]), y + (ROW_H - 8) / 2, &szb[..sl], C_DIM);
            }
            // 行分隔线
            fill_rect(0, y + ROW_H - 2, DISPLAY_W as i32, 2, C_ROW_BORDER);
        }
        // 底部提示
        draw_text(12, DISPLAY_H as i32 - 20, "点击进入 / 返回上级".as_bytes(), C_DIM);
        let mut cnt = [0u8; 32];
        let mut off = 0usize;
        for &c in "共 ".as_bytes() {
            cnt[off] = c;
            off += 1;
        }
        let mut v = ENT_COUNT as u32;
        let mut tmp = [0u8; 8];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in &tmp[i..] {
            cnt[off] = c;
            off += 1;
        }
        for &c in " 项".as_bytes() {
            cnt[off] = c;
            off += 1;
        }
        draw_text(DISPLAY_W as i32 - 12 - cjk_font::text_width(&cnt[..off]), DISPLAY_H as i32 - 20, &cnt[..off], C_DIM);
    }
}

/// 计算查看器可见行数与当前行数。
fn viewer_lines() -> (usize, usize) {
    let mut lines = 1usize;
    unsafe {
        for i in 0..VIEW_LEN {
            if VIEW_BUF[i] == b'\n' {
                lines += 1;
            }
        }
    }
    let rows = ((DISPLAY_H as i32 - TITLE_H - 24) / 20) as usize;
    (lines, rows.max(1))
}

fn render_viewer() {
    unsafe {
        fill_rect(0, TITLE_H, DISPLAY_W as i32, DISPLAY_H as i32 - TITLE_H, C_BG);
        let (total_lines, rows) = viewer_lines();
        let max_scroll = total_lines.saturating_sub(rows);
        if VIEW_SCROLL > max_scroll {
            VIEW_SCROLL = max_scroll;
        }
        // 逐行渲染（按 '\n' 切行，跳过 \r）
        let mut line = 0usize;
        let mut start = 0usize;
        let mut y = TITLE_H + 6;
        let first = VIEW_SCROLL;
        let last = (VIEW_SCROLL + rows).min(total_lines);
        let mut idx = 0usize;
        while idx <= VIEW_LEN {
            if idx == VIEW_LEN || VIEW_BUF[idx] == b'\n' {
                if line >= first && line < last {
                    let mut seg = [0u8; 512];
                    let mut n = 0usize;
                    let mut k = start;
                    while k < idx && n + 1 < seg.len() {
                        let c = VIEW_BUF[k];
                        if c != b'\r' {
                            seg[n] = c;
                            n += 1;
                        }
                        k += 1;
                    }
                    draw_text(8, y, &seg[..n], C_TEXT);
                    y += 20;
                }
                line += 1;
                start = idx + 1;
                idx += 1;
            } else {
                idx += 1;
            }
        }
        // 底部状态：文件路径 + 行数
        let mut vp = [0u8; 128];
        let mut vl = 0usize;
        while vl < VIEW_PATH.len() && VIEW_PATH[vl] != 0 {
            vp[vl] = VIEW_PATH[vl];
            vl += 1;
        }
        draw_text(8, DISPLAY_H as i32 - 18, &vp[..vl], C_DIM);
        let mut st = [0u8; 32];
        let mut off = 0usize;
        for &c in "第 ".as_bytes() {
            st[off] = c;
            off += 1;
        }
        let mut tmp = [0u8; 8];
        let mut i = tmp.len();
        let mut v = (VIEW_SCROLL + 1) as u32;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in &tmp[i..] {
            st[off] = c;
            off += 1;
        }
        for &c in "/共".as_bytes() {
            st[off] = c;
            off += 1;
        }
        let mut i2 = tmp.len();
        let mut v2 = total_lines as u32;
        while v2 > 0 {
            i2 -= 1;
            tmp[i2] = b'0' + (v2 % 10) as u8;
            v2 /= 10;
        }
        for &c in &tmp[i2..] {
            st[off] = c;
            off += 1;
        }
        for &c in " 行".as_bytes() {
            st[off] = c;
            off += 1;
        }
        draw_text(DISPLAY_W as i32 - 8 - cjk_font::text_width(&st[..off]), DISPLAY_H as i32 - 18, &st[..off], C_DIM);
    }
}

fn render() {
    unsafe {
        if VIEW_MODE {
            render_viewer();
        } else {
            render_title();
            render_list();
        }
        draw_cursor();
    }
}

// ==================== 输入 ====================

/// 轮询触摸 / 鼠标事件；返回 (点击坐标, 指针是否移动)。
fn poll_input() -> (Option<(i32, i32)>, bool) {
    unsafe {
        if INPUT_FD < 0 {
            let fd = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
            if fd < 0 {
                return (None, false);
            }
            INPUT_FD = fd;
        }
        let mut buf = [0u8; 24];
        let mut click: Option<(i32, i32)> = None;
        let mut moved = false;
        loop {
            let n = syscall3(SYS_READ, INPUT_FD as usize, buf.as_mut_ptr() as usize, buf.len());
            if n < 24 {
                break;
            }
            let etype = u16::from_le_bytes([buf[16], buf[17]]);
            let code = u16::from_le_bytes([buf[18], buf[19]]);
            let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
            match etype {
                EV_REL => {
                    if code == REL_X {
                        CUR_X = (CUR_X + value).clamp(0, DISPLAY_W as i32 - 1);
                        moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                        moved = true;
                    } else if code == ABS_Y {
                        CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                        moved = true;
                    }
                }
                EV_KEY => {
                    if code == BTN_TOUCH {
                        if value == 1 {
                            TOUCH_DOWN = true;
                        } else if value == 0 && TOUCH_DOWN {
                            TOUCH_DOWN = false;
                            click = Some((CUR_X, CUR_Y));
                        }
                    } else if code == BTN_LEFT {
                        if value == 1 {
                            MOUSE_DOWN = true;
                        } else if value == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            click = Some((CUR_X, CUR_Y));
                        }
                    }
                }
                _ => {}
            }
        }
        (click, moved)
    }
}

/// 轮询 /dev/tty（fd 0）：检测 ESC 按键。
fn poll_key() -> bool {
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as usize, buf.len()) };
    if n <= 0 {
        return false;
    }
    for i in 0..(n as usize) {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}

// ==================== 导航 ====================

fn back_hit(x: i32, y: i32) -> bool {
    x >= 4 && x < 40 && y >= 4 && y < TITLE_H - 4
}

/// 进入目录 `name`（压栈 + 更新 CWD + 重载列表）。
fn enter_dir(name: &[u8]) {
    unsafe {
        if STACK_DEPTH < PATH_STACK_DEPTH {
            PATH_STACK[STACK_DEPTH][..].fill(0);
            let mut plen = 0usize;
            while plen < CWD.len() && CWD[plen] != 0 {
                plen += 1;
            }
            let n = plen.min(127);
            PATH_STACK[STACK_DEPTH][..n].copy_from_slice(&CWD[..n]);
            STACK_DEPTH += 1;
        }
        let mut nb = [0u8; 128];
        if join_path(&CWD[..], name, &mut nb) > 0 {
            CWD.fill(0);
            let mut n = 0usize;
            while n < nb.len() && nb[n] != 0 {
                CWD[n] = nb[n];
                n += 1;
            }
        }
        LIST_SCROLL = 0;
    }
    let cwd = unsafe { &CWD[..] };
    let cl = unsafe { CWD.iter().position(|&c| c == 0).unwrap_or(CWD.len()) };
    let s = core::str::from_utf8(&cwd[..cl]).unwrap_or("/");
    load_dir(s);
}

/// 返回上级（栈非空则弹栈，否则直接取父目录）。
fn go_up() {
    unsafe {
        if STACK_DEPTH > 0 {
            STACK_DEPTH -= 1;
            CWD.fill(0);
            let mut n = 0usize;
            while n < PATH_STACK[STACK_DEPTH].len() && PATH_STACK[STACK_DEPTH][n] != 0 {
                CWD[n] = PATH_STACK[STACK_DEPTH][n];
                n += 1;
            }
        } else {
            let mut pb = [0u8; 128];
            let mut plen = 0usize;
            while plen < CWD.len() && CWD[plen] != 0 {
                plen += 1;
            }
            let n = parent_of(&CWD[..plen], &mut pb);
            CWD.fill(0);
            CWD[..n].copy_from_slice(&pb[..n]);
        }
        LIST_SCROLL = 0;
    }
    let cwd = unsafe { &CWD[..] };
    let cl = unsafe { CWD.iter().position(|&c| c == 0).unwrap_or(CWD.len()) };
    let s = core::str::from_utf8(&cwd[..cl]).unwrap_or("/");
    load_dir(s);
}

/// 打开文件 `path` 到查看器。
fn open_viewer(path: &str) {
    unsafe {
        let mut pbuf = [0u8; 160];
        let n = path.len().min(pbuf.len() - 1);
        pbuf[..n].copy_from_slice(&path.as_bytes()[..n]);
        pbuf[n] = 0;
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return;
        }
        let mut total = 0usize;
        while total < VIEW_CAP {
            let r = syscall3(
                SYS_READ,
                fd as usize,
                VIEW_BUF.as_mut_ptr().add(total) as usize,
                VIEW_CAP - total,
            ) as isize;
            if r <= 0 {
                break;
            }
            total += r as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        VIEW_LEN = total;
        VIEW_SCROLL = 0;
        VIEW_PATH.fill(0);
        let m = n.min(VIEW_PATH.len() - 1);
        VIEW_PATH[..m].copy_from_slice(&pbuf[..m]);
        VIEW_MODE = true;
    }
}

/// 处理浏览模式点击。
fn handle_browse_click(x: i32, y: i32) -> bool {
    if back_hit(x, y) {
        // 返回键：栈非空则回退一级，否则退出应用
        if unsafe { STACK_DEPTH > 0 } {
            go_up();
        } else {
            return true; // 退出
        }
        return false;
    }
    if y < LIST_TOP {
        return false;
    }
    let row = ((y - LIST_TOP) / ROW_H) as usize;
    let rows_visible = ((DISPLAY_H as i32 - LIST_TOP) / ROW_H) as usize;
    let idx = unsafe { LIST_SCROLL.min(ENT_COUNT.saturating_sub(rows_visible)) } + row;
    unsafe {
        if idx >= ENT_COUNT {
            return false;
        }
        let nm = &ENT_NAMES[idx][..ENT_LEN[idx]];
        if nm == b"." {
            return false;
        }
        if nm == b".." {
            go_up();
            return false;
        }
        let t = ENT_TYPE[idx];
        if t == DT_DIR {
            enter_dir(nm);
            return false;
        }
        // 普通文件 / 符号链接 / 设备：打开查看器
        let mut full = [0u8; 160];
        if join_path(&CWD[..], nm, &mut full) == 0 {
            return false;
        }
        let fl = full.iter().position(|&c| c == 0).unwrap_or(full.len());
        let s = core::str::from_utf8(&full[..fl]).unwrap_or("");
        open_viewer(s);
    }
    false
}

/// 处理查看模式点击：上半屏上翻页，下半屏下翻页，返回键退出查看器。
fn handle_viewer_click(x: i32, y: i32) -> bool {
    if back_hit(x, y) {
        unsafe { VIEW_MODE = false; }
        return false;
    }
    let (total_lines, rows) = viewer_lines();
    let max_scroll = total_lines.saturating_sub(rows);
    unsafe {
        if y < TITLE_H {
            return false;
        }
        if y < DISPLAY_H as i32 / 2 {
            VIEW_SCROLL = VIEW_SCROLL.saturating_sub(rows / 2);
        } else {
            VIEW_SCROLL = (VIEW_SCROLL + rows / 2).min(max_scroll);
        }
    }
    false
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[files] starting\n");

    if !fb_init() {
        print("[files] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[files] framebuffer ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" mapped\n");

    unsafe { CWD[0] = b'/'; }
    load_dir("/");
    render();
    flip();
    print("[files] ready\n");

    loop {
        let (click, moved) = poll_input();
        if moved {
            render();
            flip();
        }
        if let Some((x, y)) = click {
            print("[files] tap (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            let exit_app = if unsafe { VIEW_MODE } {
                handle_viewer_click(x, y)
            } else {
                handle_browse_click(x, y)
            };
            if exit_app {
                print("[files] back at root -> exit\n");
                break;
            }
            render();
            flip();
        }
        if poll_key() {
            print("[files] ESC -> back/exit\n");
            if unsafe { VIEW_MODE } {
                unsafe { VIEW_MODE = false; }
                render();
                flip();
            } else if unsafe { STACK_DEPTH > 0 } {
                go_up();
                render();
                flip();
            } else {
                break;
            }
        }
        sleep_ms(16);
    }

    print("[files] exiting\n");
    unsafe { exit(0); }
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
fn panic(info: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: files @ ");
        if let Some(loc) = info.location() {
            let f = loc.file().as_bytes();
            let fl = f.len().min(48);
            write_fd(2, &f[f.len() - fl..]);
            write_fd(2, b":");
            let mut nb = [0u8; 8];
            let mut i = nb.len();
            let mut v = loc.line() as u64;
            while v > 0 {
                i -= 1;
                nb[i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            write_fd(2, &nb[i..]);
        }
        write_fd(2, b"\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
