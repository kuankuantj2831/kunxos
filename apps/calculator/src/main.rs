// KunxOS Calculator 应用：全屏计算器（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/calculator）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 绘制全屏计算器窗口：标题栏（返回键）+ 显示屏 + 4x5 按键区
//   3. 主循环：轮询 /dev/input/event0 触摸事件（数字/运算符/=）与
//      /dev/tty 键盘（ESC）
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收后重绘桌面
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

// ==================== 显示参数 ====================

const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;
const TITLE_H: i32 = 44;
const DISPLAY_Y: i32 = 52;
const DISPLAY_HGT: i32 = 132;
const BTN_TOP: i32 = DISPLAY_Y + DISPLAY_HGT + 8;
const BTN_COLS: i32 = 4;
const BTN_ROWS: i32 = 5;
const BTN_GAP: i32 = 8;
const BTN_MARGIN: i32 = 10;
const BTN_W: i32 = (DISPLAY_W as i32 - 2 * BTN_MARGIN - (BTN_COLS - 1) * BTN_GAP) / BTN_COLS;
const BTN_H: i32 = 145;

/// 返回键命中区（标题栏左上角）。
const BACK_X: i32 = 4;
const BACK_Y: i32 = 4;
const BACK_SZ: i32 = 36;

// ==================== 颜色（0x00RRGGBB） ====================

const C_BG: u32 = 0x00_0F_17_2A;
const C_TITLE_BG: u32 = 0x00_0C_1A_36;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A8_C0;
const C_DISPLAY_BG: u32 = 0x00_1A_1A_2E;
const C_BTN: u32 = 0x00_1B_2B_4A;
const C_BTN_BORDER: u32 = 0x00_3A_4A_6A;
const C_OP: u32 = 0x00_EA_B3_08;
const C_CLEAR: u32 = 0x00_E7_4C_3C;

// ==================== 8x8 位图字体 ====================

static FONT_DIGITS: [[u8; 8]; 10] = [
    [0x3C, 0x42, 0x46, 0x4A, 0x52, 0x62, 0x3C, 0x00],
    [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x3C, 0x42, 0x02, 0x0C, 0x30, 0x40, 0x7E, 0x00],
    [0x3C, 0x42, 0x02, 0x1C, 0x02, 0x42, 0x3C, 0x00],
    [0x0C, 0x14, 0x24, 0x44, 0x7E, 0x04, 0x04, 0x00],
    [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0x00],
    [0x3C, 0x40, 0x40, 0x7C, 0x42, 0x42, 0x3C, 0x00],
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x10, 0x10, 0x00],
    [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0x00],
    [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x02, 0x3C, 0x00],
];

static FONT_UPPER: [[u8; 8]; 26] = [
    [0x18, 0x24, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0x00],
    [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0x00],
    [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00],
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0x00],
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0x00],
    [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3C, 0x00],
    [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0x00],
    [0x3E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x1E, 0x04, 0x04, 0x04, 0x04, 0x44, 0x38, 0x00],
    [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0x00],
    [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0x00],
    [0x42, 0x66, 0x5A, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x42, 0x62, 0x52, 0x4A, 0x46, 0x42, 0x42, 0x00],
    [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0x00],
    [0x3C, 0x42, 0x42, 0x42, 0x4A, 0x44, 0x3A, 0x00],
    [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0x00],
    [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x42, 0x3C, 0x00],
    [0x7F, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    [0x42, 0x42, 0x42, 0x42, 0x5A, 0x66, 0x42, 0x00],
    [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0x00],
    [0x41, 0x22, 0x14, 0x08, 0x08, 0x08, 0x08, 0x00],
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0x00],
];

static FONT_PLUS: [u8; 8] = [0x00, 0x08, 0x08, 0x3E, 0x08, 0x08, 0x00, 0x00];
static FONT_MINUS: [u8; 8] = [0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00, 0x00];
static FONT_STAR: [u8; 8] = [0x00, 0x14, 0x08, 0x3E, 0x08, 0x14, 0x00, 0x00];
static FONT_SLASH: [u8; 8] = [0x02, 0x02, 0x04, 0x08, 0x10, 0x20, 0x20, 0x00];
static FONT_EQUALS: [u8; 8] = [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00];
static FONT_PERCENT: [u8; 8] = [0x62, 0x64, 0x08, 0x10, 0x26, 0x46, 0x00, 0x00];
static FONT_LT: [u8; 8] = [0x08, 0x10, 0x20, 0x40, 0x20, 0x10, 0x08, 0x00];
static FONT_SPACE: [u8; 8] = [0x00; 8];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'+' => &FONT_PLUS,
        b'-' => &FONT_MINUS,
        b'*' => &FONT_STAR,
        b'/' => &FONT_SLASH,
        b'=' => &FONT_EQUALS,
        b'%' => &FONT_PERCENT,
        b'<' => &FONT_LT,
        _ => &FONT_SPACE,
    }
}

// ==================== 状态 ====================

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
static mut FB_STRIDE_PX: usize = DISPLAY_W;
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 计算器状态。
static mut ACC: i64 = 0;
static mut CUR: i64 = 0;
static mut OP: u8 = 0; // 0=无, '+', '-', '*', '/', '%'
static mut ENTERING: bool = false;
static mut ERROR: bool = false;
static mut JUST_EQ: bool = false;

/// 输入状态。
static mut INPUT_FD: isize = -1;
static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut TOUCH_DOWN: bool = false;
static mut MOUSE_DOWN: bool = false;

/// 鼠标指针位图（9x9，'1' 为实心像素）：经典箭头，黑描边 + 白填充。
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

/// 按键标签（4 列 x 5 行，行优先）。
static BTNS: [&[u8]; 20] = [
    b"C", b"DEL", b"%", b"/",
    b"7", b"8", b"9", b"*",
    b"4", b"5", b"6", b"-",
    b"1", b"2", b"3", b"+",
    b"+/-", b"0", b"00", b"=",
];

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

// ==================== 文件操作与帧缓冲 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

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

fn draw_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dx in 0..w {
        put_pixel(x + dx, y, color);
        put_pixel(x + dx, y + h - 1, color);
    }
    for dy in 0..h {
        put_pixel(x, y + dy, color);
        put_pixel(x + w - 1, y + dy, color);
    }
}

fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8 {
            if bits & (0x80 >> col) != 0 {
                put_pixel(x + col as i32, y + row as i32, color);
            }
        }
    }
}

fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    for &c in s {
        draw_char(cx, y, c, color);
        cx += 8;
    }
}

fn text_width(s: &[u8]) -> i32 {
    s.len() as i32 * 8
}

/// 把 i64 格式化为十进制 ASCII，返回长度（含负号）。
fn fmt_i64(buf: &mut [u8], v: i64) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let neg = v < 0;
    let mut mag: u64 = if neg { (v as u64).wrapping_neg() } else { v as u64 };
    let mut pos = buf.len();
    while mag > 0 {
        pos -= 1;
        buf[pos] = b'0' + (mag % 10) as u8;
        mag /= 10;
    }
    if neg {
        pos -= 1;
        buf[pos] = b'-';
    }
    let n = buf.len() - pos;
    for i in 0..n {
        buf[i] = buf[pos + i];
    }
    n
}

// ==================== 计算逻辑 ====================

fn press_clear() {
    unsafe {
        ACC = 0;
        CUR = 0;
        OP = 0;
        ENTERING = false;
        ERROR = false;
        JUST_EQ = false;
    }
}

fn press_digit(d: u8) {
    unsafe {
        if ERROR {
            press_clear();
        }
        if JUST_EQ {
            CUR = 0;
            ACC = 0;
            OP = 0;
            ENTERING = true;
            JUST_EQ = false;
        } else if !ENTERING {
            CUR = 0;
            ENTERING = true;
        }
        if let Some(v) = CUR.checked_mul(10).and_then(|x| x.checked_add(d as i64)) {
            CUR = v;
        }
    }
}

/// 应用挂起的运算符：ACC = ACC OP CUR，结果写回 CUR；除零返回 false 并置 ERROR。
fn apply_op() -> bool {
    unsafe {
        let a = ACC;
        let b = CUR;
        let r = match OP {
            b'+' => a.wrapping_add(b),
            b'-' => a.wrapping_sub(b),
            b'*' => a.wrapping_mul(b),
            b'/' => {
                if b == 0 || (a == i64::MIN && b == -1) {
                    ERROR = true;
                    return false;
                }
                a / b
            }
            b'%' => {
                if b == 0 || (a == i64::MIN && b == -1) {
                    ERROR = true;
                    return false;
                }
                a % b
            }
            _ => a,
        };
        ACC = r;
        CUR = r;
        OP = 0;
        true
    }
}

fn press_op(op: u8) {
    unsafe {
        if ERROR {
            return;
        }
        JUST_EQ = false;
        if OP != 0 {
            if !apply_op() {
                return;
            }
        } else {
            ACC = CUR;
        }
        OP = op;
        ENTERING = false;
    }
}

fn press_eq() {
    unsafe {
        if ERROR {
            return;
        }
        if OP != 0 {
            if !apply_op() {
                return;
            }
        } else {
            ACC = CUR;
        }
        ENTERING = false;
        JUST_EQ = true;
    }
}

fn press_del() {
    unsafe {
        if ERROR {
            press_clear();
            return;
        }
        JUST_EQ = false;
        if ENTERING {
            CUR /= 10;
            if CUR == 0 {
                ENTERING = false;
            }
        } else {
            CUR = 0;
            ACC = 0;
            OP = 0;
        }
    }
}

fn press_neg() {
    unsafe {
        if ERROR {
            return;
        }
        CUR = CUR.wrapping_neg();
        if !ENTERING {
            ACC = CUR;
        }
        JUST_EQ = false;
    }
}

fn press_button(idx: usize) {
    match idx {
        0 => press_clear(),
        1 => press_del(),
        2 => press_op(b'%'),
        3 => press_op(b'/'),
        4..=6 => press_digit((idx as u32 - 4 + 7) as u8),
        7 => press_op(b'*'),
        8..=10 => press_digit((idx as u32 - 8 + 4) as u8),
        11 => press_op(b'-'),
        12..=14 => press_digit((idx as u32 - 12 + 1) as u8),
        15 => press_op(b'+'),
        16 => press_neg(),
        17 => press_digit(0),
        18 => {
            press_digit(0);
            press_digit(0);
        }
        19 => press_eq(),
        _ => {}
    }
}

// ==================== 布局与命中测试 ====================

fn btn_rect(idx: usize) -> (i32, i32, i32, i32) {
    let col = (idx as i32) % BTN_COLS;
    let row = (idx as i32) / BTN_COLS;
    let x = BTN_MARGIN + col * (BTN_W + BTN_GAP);
    let y = BTN_TOP + row * (BTN_H + BTN_GAP);
    (x, y, BTN_W, BTN_H)
}

fn button_at(x: i32, y: i32) -> Option<usize> {
    for i in 0..BTNS.len() {
        let (bx, by, bw, bh) = btn_rect(i);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return Some(i);
        }
    }
    None
}

fn back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

fn button_color(idx: usize) -> u32 {
    match idx {
        0 | 1 => C_CLEAR,
        2 | 3 | 7 | 11 | 15 | 19 => C_OP,
        _ => C_BTN,
    }
}

// ==================== 渲染 ====================

fn render_title() {
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    // 返回键
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    // 标题
    let s = b"CALCULATOR";
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, (TITLE_H - 8) / 2, s, C_TEXT);
}

fn render_display() {
    fill_rect(0, DISPLAY_Y, DISPLAY_W as i32, DISPLAY_HGT, C_DISPLAY_BG);
    draw_rect(0, DISPLAY_Y, DISPLAY_W as i32, DISPLAY_HGT, C_DIM);
    let mut buf = [0u8; 24];
    let (acc, cur, op, error, just_eq) = unsafe { (ACC, CUR, OP, ERROR, JUST_EQ) };
    if error {
        draw_text(16, DISPLAY_Y + 20, b"ERROR", C_CLEAR);
        draw_text(16, DISPLAY_Y + DISPLAY_HGT - 28, b"0", C_TEXT);
        return;
    }
    // 挂起运算符/等号指示
    if op != 0 {
        let n = fmt_i64(&mut buf, acc);
        draw_text(16, DISPLAY_Y + 12, &buf[..n], C_DIM);
        let ox = 16 + n as i32 * 8 + 8;
        draw_text(ox, DISPLAY_Y + 12, &[op], C_OP);
    } else if just_eq {
        draw_text(16, DISPLAY_Y + 12, b"=", C_DIM);
    }
    // 当前值
    let n = fmt_i64(&mut buf, cur);
    let tx = DISPLAY_W as i32 - 16 - n as i32 * 8;
    draw_text(tx, DISPLAY_Y + DISPLAY_HGT - 28, &buf[..n], C_TEXT);
}

fn render_buttons() {
    for i in 0..BTNS.len() {
        let (x, y, w, h) = btn_rect(i);
        fill_rect(x, y, w, h, button_color(i));
        draw_rect(x, y, w, h, C_BTN_BORDER);
        let label = BTNS[i];
        let tx = x + (w - text_width(label)) / 2;
        let ty = y + (h - 8) / 2;
        draw_text(tx, ty, label, C_TEXT);
    }
}

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    render_title();
    render_display();
    render_buttons();
    // 鼠标指针：始终绘制在最上层
    draw_cursor();
}

/// 鼠标指针：先画黑色描边（8 邻域），再画白色实心，保证深浅背景下都可见。
fn draw_cursor() {
    let (x, y) = unsafe { (CUR_X, CUR_Y) };
    const OUTLINE: [(i32, i32); 8] =
        [(-1, -1), (1, -1), (-1, 1), (1, 1), (0, -1), (0, 1), (-1, 0), (1, 0)];
    for (ox, oy) in OUTLINE {
        draw_cursor_bits(x + ox, y + oy, 0x00_00_00_00);
    }
    draw_cursor_bits(x, y, 0x00_FF_FF_FF);
}

/// 把 9x9 指针位图画到 (x, y)。
fn draw_cursor_bits(x: i32, y: i32, color: u32) {
    for (row, bits) in CURSOR_BITS.iter().enumerate() {
        for col in 0..9 {
            if bits & (0x100 >> col) != 0 {
                put_pixel(x + col as i32, y + row as i32, color);
            }
        }
    }
}

// ==================== 输入 ====================

/// 轮询 /dev/input/event0，检测一次完整点击（按下并抬起）并跟踪鼠标指针。
///
/// 返回 `(点击坐标, 指针是否移动)`：指针移动时上层需重绘以更新光标位置。
fn poll_input() -> (Option<(i32, i32)>, bool) {
    unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
        }
        if INPUT_FD < 0 {
            return (None, false);
        }
        let mut click = None;
        let mut moved = false;
        loop {
            let mut buf = [0u8; 24];
            let n = syscall3(SYS_READ, INPUT_FD as usize, buf.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return (click, moved);
            }
            let etype = u16::from_le_bytes([buf[16], buf[17]]);
            let code = u16::from_le_bytes([buf[18], buf[19]]);
            let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
            match etype {
                EV_REL => {
                    // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
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
    }
}

/// 轮询 /dev/tty（fd 0，标准输入）：检测 ESC（0x1b）按键。
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

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[calculator] starting\n");

    if !fb_init() {
        print("[calculator] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[calculator] framebuffer ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" mapped\n");

    render();
    flip();
    print("[calculator] ready\n");

    loop {
        let (click, moved) = poll_input();
        if moved {
            // 指针移动：重绘以更新光标位置。
            render();
            flip();
        }
        if let Some((x, y)) = click {
            print("[calculator] tap (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            if back_hit(x, y) {
                print("[calculator] back -> exit\n");
                break;
            }
            if let Some(i) = button_at(x, y) {
                press_button(i);
                render();
                flip();
            }
        }
        if poll_key() {
            print("[calculator] ESC -> exit\n");
            break;
        }
        sleep_ms(16);
    }

    print("[calculator] exiting\n");
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
fn panic(_: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: calculator\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
