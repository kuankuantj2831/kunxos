//! KunxOS 桌面应用公共框架（EL0 用户态，裸 syscall、no_std、无堆）。
//!
//! 各应用以 `#[path = "../../../shared/app_kit.rs"] mod kit;` 引入本模块，
//! 再用 `use kit::*;` 使用公共能力：系统调用封装、framebuffer 初始化与双缓冲、
//! 绘制原语（像素/矩形/线/圆）、ASCII 8x8 与 CJK 16x16 字体渲染、鼠标/触摸/
//! 键盘输入、光标、标题栏与返回键、墙钟时间与公历换算。
//!
//! 约定：
//! - 应用自身须声明 `#[path = "..."] mod cjk_font;`（由 tools/gen_cjk_font.py
//!   按应用源码生成的子集），本模块经 `crate::cjk_font` 访问字形。
//! - 应用入口：`k_main(argc, argv)`，内部自行 `global_asm!` 定义 `_start`
//!   蹦床（见 stub 应用示例）。
//! - 分辨率 540x960，每应用独占全屏；返回键 / ESC 退出后由 launcher 重绘桌面。

use core::arch::asm;

// ==================== 系统调用号（AArch64 asm-generic） ====================

pub const SYS_READ: usize = 63;
pub const SYS_WRITE: usize = 64;
pub const SYS_OPENAT: usize = 56;
pub const SYS_CLOSE: usize = 57;
pub const SYS_IOCTL: usize = 29;
pub const SYS_EXIT: usize = 93;
pub const SYS_NANOSLEEP: usize = 101;
pub const SYS_MMAP: usize = 222;
pub const SYS_GETTIMEOFDAY: usize = 169;

// ==================== 常量 ====================

pub const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
pub const O_RDONLY: usize = 0;
pub const O_RDWR: usize = 2;
pub const O_NONBLOCK: usize = 0x800;

pub const PROT_READ: usize = 1;
pub const PROT_WRITE: usize = 2;
pub const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
pub const FBIOGET_VSCREENINFO: usize = 0x4600;

pub const FB_PATH: &str = "/dev/fb0";
pub const INPUT_PATH: &str = "/dev/input/event0";

/// linux input_event 事件类型 / 码。
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a; // 330
pub const BTN_LEFT: u16 = 0x110; // 272

/// evdev 键码（QWERTY 物理布局编号）。
pub const KEY_ENTER: u16 = 28;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_ESC: u16 = 1;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;

// ==================== 显示参数 ====================

pub const DISPLAY_W: usize = 540;
pub const DISPLAY_H: usize = 960;
pub const TITLE_H: i32 = 44;
pub const BACK_SZ: i32 = 36;
pub const BACK_X: i32 = 4;
pub const BACK_Y: i32 = 4;

// ==================== 默认配色（应用可自行覆盖） ====================

pub const C_TITLE_BG: u32 = 0x00_0F_17_2A;
pub const C_TEXT: u32 = 0x00_FF_FF_FF;
pub const C_DIM: u32 = 0x00_9A_A7_B5;
pub const C_BTN: u32 = 0x00_23_36_50;
pub const C_BTN_BORDER: u32 = 0x00_FF_FF_FF;
pub const C_ACCENT: u32 = 0x00_22_C5_5E;

// ==================== 全局状态 ====================

pub static mut FB_PTR: *mut u32 = core::ptr::null_mut();
pub static mut FB_W: usize = DISPLAY_W;
pub static mut FB_H: usize = DISPLAY_H;
pub static mut FB_STRIDE_PX: usize = DISPLAY_W;
pub static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 鼠标/触摸指针状态。
pub static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
pub static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
pub static mut MOUSE_DOWN: bool = false;
pub static mut TOUCH_DOWN: bool = false;
pub static mut SHIFT: bool = false;
pub static mut INPUT_FD: isize = -1;

/// 鼠标指针位图（9x9）：经典箭头，黑描边 + 白填充。
pub static CURSOR_BITS: [u16; 9] = [
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

// ==================== 系统调用封装 ====================

#[inline(always)]
pub unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, in("x1") a2, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
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
pub unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
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

pub unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

pub fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); }
}

pub fn print_dec(mut v: u64) {
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
    unsafe { write_fd(1, &buf[i..]); }
}

pub fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe { syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0); }
}

pub unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

static mut PATH_BUF: [u8; 128] = [0u8; 128];

pub unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// 当前墙钟时间（微秒，epoch）。内核 RTC 提供 UTC 墙钟。
pub fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
}

// ==================== 公历换算（Howard Hinnant 算法） ====================

/// epoch 天数 → 公历 (年, 月, 日)，月/日 1 基。
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

/// 公历 (年, 月, 日) → epoch 天数；月/日 1 基。
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = ((153 * mp + 2) / 5) as i64 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// epoch 天数 → 星期（0=周日, 1=周一, …, 6=周六）。
pub fn weekday_from_days(days: i64) -> u32 {
    let w = (days + 4) % 7;
    if w < 0 { (w + 7) as u32 } else { w as u32 }
}

/// 当前本地时间（由墙钟 epoch 换算，UTC）。
pub fn now_hms() -> (u32, u32, u32) {
    let us = now_us();
    let secs = (us / 1_000_000) as u64;
    let h = ((secs / 3600) % 24) as u32;
    let m = ((secs / 60) % 60) as u32;
    let s = (secs % 60) as u32;
    (h, m, s)
}

/// 当前公历日期（UTC）。
pub fn now_date() -> (i32, u32, u32) {
    let days = (now_us() / 1_000_000 / 86400) as i64;
    civil_from_days(days)
}

// ==================== framebuffer ====================

pub fn fb_init() -> bool {
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

/// 把 back buffer 整体拷贝到显存（分辨率为默认值时整块拷贝，否则逐行）。
pub fn flip() {
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

pub fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = color; }
}

pub fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dy in 0..h {
        for dx in 0..w {
            put_pixel(x + dx, y + dy, color);
        }
    }
}

pub fn draw_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dx in 0..w {
        put_pixel(x + dx, y, color);
        put_pixel(x + dx, y + h - 1, color);
    }
    for dy in 0..h {
        put_pixel(x, y + dy, color);
        put_pixel(x + w - 1, y + dy, color);
    }
}

/// Bresenham 直线。
pub fn draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let mut x0 = x0;
    let mut y0 = y0;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_pixel(x0, y0, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// 空心圆（中点画圆）。
pub fn draw_circle(cx: i32, cy: i32, r: i32, color: u32) {
    let mut x = r;
    let mut y = 0;
    let mut err = 1 - r;
    while x >= y {
        put_pixel(cx + x, cy + y, color);
        put_pixel(cx + y, cy + x, color);
        put_pixel(cx - y, cy + x, color);
        put_pixel(cx - x, cy + y, color);
        put_pixel(cx - x, cy - y, color);
        put_pixel(cx - y, cy - x, color);
        put_pixel(cx + y, cy - x, color);
        put_pixel(cx + x, cy - y, color);
        y += 1;
        if err <= 0 {
            err += 2 * y + 1;
        } else {
            x -= 1;
            err += 2 * (y - x) + 1;
        }
    }
}

/// 实心圆。
pub fn draw_disc(cx: i32, cy: i32, r: i32, color: u32) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put_pixel(cx + dx, cy + dy, color);
            }
        }
    }
}

// ==================== 8x8 位图字体（公有领域 IBM VGA 风格） ====================

pub static FONT8X8: [[u8; 8]; 95] = [
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

/// 取 ASCII 字形（0x20..=0x7E；其余返回空格）。
pub fn glyph(c: u8) -> &'static [u8; 8] {
    if c >= 0x20 && c <= 0x7E {
        &FONT8X8[(c - 0x20) as usize]
    } else {
        &FONT8X8[0]
    }
}

pub fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8 {
            if bits & (0x80 >> col) != 0 {
                put_pixel(x + col as i32, y + row as i32, color);
            }
        }
    }
}

/// 绘制单个 16x16 CJK 字形（索引来自 cjk_font::cjk_index）。
pub fn draw_cjk(x: i32, y: i32, idx: usize, color: u32) {
    let base = idx * 32;
    for row in 0..16i32 {
        let b0 = crate::cjk_font::CJK_BITMAPS[base + row as usize * 2];
        let b1 = crate::cjk_font::CJK_BITMAPS[base + row as usize * 2 + 1];
        let bits = ((b0 as u16) << 8) | b1 as u16;
        for col in 0..16i32 {
            if bits & (0x8000u16 >> col) != 0 {
                put_pixel(x + col, y + row, color);
            }
        }
    }
}

/// 按 UTF-8 渲染文本（ASCII 8px 宽、CJK 16px 宽）。
pub fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = crate::cjk_font::utf8_next(s, i);
        if cp >= 0x80 {
            if let Some(idx) = crate::cjk_font::cjk_index(cp as u16) {
                draw_cjk(cx, y, idx, color);
            }
            cx += crate::cjk_font::CJK_W;
            i += n;
        } else {
            draw_char(cx, y, cp as u8, color);
            cx += 8;
            i += n;
        }
    }
}

pub fn text_width(s: &[u8]) -> i32 {
    crate::cjk_font::text_width(s)
}

/// 水平居中绘制。
pub fn draw_center(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

// ==================== 标题栏与返回键 ====================

/// 绘制标题栏：深色底 + 左上角返回键 + 居中标题。
pub fn draw_title_bar(title: &[u8]) {
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_center((TITLE_H - 8) / 2, title, C_TEXT);
}

pub fn back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

// ==================== 光标 ====================

pub fn draw_cursor() {
    let (x, y) = unsafe { (CUR_X, CUR_Y) };
    const OUTLINE: [(i32, i32); 8] =
        [(-1, -1), (1, -1), (-1, 1), (1, 1), (0, -1), (0, 1), (-1, 0), (1, 0)];
    for (ox, oy) in OUTLINE {
        draw_cursor_bits(x + ox, y + oy, 0x00_00_00_00);
    }
    draw_cursor_bits(x, y, 0x00_FF_FF_FF);
}

pub fn draw_cursor_bits(x: i32, y: i32, color: u32) {
    for (row, bits) in CURSOR_BITS.iter().enumerate() {
        for col in 0..9 {
            if bits & (0x100 >> col) != 0 {
                put_pixel(x + col as i32, y + row as i32, color);
            }
        }
    }
}

// ==================== 输入 ====================

/// 单次输入轮询结果。
pub struct PollResult {
    /// 本次抬起产生的点击坐标。
    pub tap: Option<(i32, i32)>,
    /// 指针是否移动（需要重绘）。
    pub moved: bool,
    /// 键盘动作（最后一次）。
    pub key: KeyAction,
}

#[derive(Clone, Copy, PartialEq)]
pub enum KeyAction {
    None,
    Char(u8),
    Backspace,
    Enter,
    Esc,
}

/// evdev 键码 → ASCII 字符（QWERTY 物理布局逐键映射，与 oobe/terminal 同表）。
pub fn key_to_char(code: u16, shift: bool) -> Option<u8> {
    let c = match code {
        2 => b'1', 3 => b'2', 4 => b'3', 5 => b'4', 6 => b'5',
        7 => b'6', 8 => b'7', 9 => b'8', 10 => b'9', 11 => b'0',
        16 => b'q', 17 => b'w', 18 => b'e', 19 => b'r', 20 => b't',
        21 => b'y', 22 => b'u', 23 => b'i', 24 => b'o', 25 => b'p',
        30 => b'a', 31 => b's', 32 => b'd', 33 => b'f', 34 => b'g',
        35 => b'h', 36 => b'j', 37 => b'k', 38 => b'l',
        44 => b'z', 45 => b'x', 46 => b'c', 47 => b'v', 48 => b'b',
        49 => b'n', 50 => b'm',
        57 => b' ',
        12 => b'-', 13 => b'=',
        52 => b'.', 51 => b',', 53 => b'/',
        77 => b'@',
        _ => return None,
    };
    let c = if shift && c.is_ascii_lowercase() { c - 32 } else { c };
    if shift {
        Some(match c {
            b'1' => b'!', b'2' => b'@', b'3' => b'#', b'4' => b'$', b'5' => b'%',
            b'6' => b'^', b'7' => b'&', b'8' => b'*', b'9' => b'(', b'0' => b')',
            b'-' => b'_', b'=' => b'+', b'.' => b'>', b',' => b'<', b'/' => b'?',
            _ => c,
        })
    } else {
        Some(c)
    }
}

/// 轮询 /dev/input/event0：指针移动、点击抬起、键盘按键。
pub fn poll_input() -> PollResult {
    unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
        }
        let mut res = PollResult { tap: None, moved: false, key: KeyAction::None };
        if INPUT_FD < 0 {
            return res;
        }
        loop {
            let mut buf = [0u8; 24];
            let n = syscall3(SYS_READ, INPUT_FD as usize, buf.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return res;
            }
            let etype = u16::from_le_bytes([buf[16], buf[17]]);
            let code = u16::from_le_bytes([buf[18], buf[19]]);
            let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
            match etype {
                EV_REL => {
                    if code == REL_X {
                        CUR_X = (CUR_X + value).clamp(0, DISPLAY_W as i32 - 1);
                        res.moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = value.clamp(0, DISPLAY_W as i32 - 1);
                        res.moved = true;
                    } else if code == ABS_Y {
                        CUR_Y = value.clamp(0, DISPLAY_H as i32 - 1);
                        res.moved = true;
                    }
                }
                EV_KEY => {
                    match code {
                        BTN_LEFT => {
                            if value == 1 {
                                MOUSE_DOWN = true;
                            } else if value == 0 && MOUSE_DOWN {
                                MOUSE_DOWN = false;
                                res.tap = Some((CUR_X, CUR_Y));
                            }
                        }
                        BTN_TOUCH => {
                            if value == 1 {
                                TOUCH_DOWN = true;
                            } else if value == 0 && TOUCH_DOWN {
                                TOUCH_DOWN = false;
                                res.tap = Some((CUR_X, CUR_Y));
                            }
                        }
                        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => {
                            SHIFT = value == 1;
                        }
                        KEY_BACKSPACE => {
                            if value == 1 {
                                res.key = KeyAction::Backspace;
                            }
                        }
                        KEY_ENTER => {
                            if value == 1 {
                                res.key = KeyAction::Enter;
                            }
                        }
                        KEY_ESC => {
                            if value == 1 {
                                res.key = KeyAction::Esc;
                            }
                        }
                        _ => {
                            if value == 1 {
                                if let Some(c) = key_to_char(code, SHIFT) {
                                    res.key = KeyAction::Char(c);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 轮询 /dev/tty（fd 0）是否收到 ESC。
pub fn poll_tty_esc() -> bool {
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
