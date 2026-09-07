// KunxOS stub 应用：为尚无完整实现的应用提供占位 UI。
//
// 由 launcher 经 fork + execve 拉起；argv[0] 为路径（如 /bin/phone）。
// 取 basename 作为应用名，显示在整屏"敬请期待"占位画面上。点击左上角
// 返回键或按 ESC 退出（exit(0)）；launcher 回收后重绘桌面。
//
// 行为与其它 GUI 应用一致：直接绘制到 /dev/fb0，双缓冲，鼠标光标跟随指针。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

// 共享系统导航（底部导航栏 / 全面屏手势）。
#[path = "../../../shared/nav.rs"]
mod sys_nav;

// 共享错误提示状态（横幅绘制见 draw_error_banner）。
#[path = "../../../shared/error_ui.rs"]
mod err_ui;

// 共享光标 overlay 快速路径（鼠标移动不整屏重绘）。
#[path = "../../../shared/cursor_overlay.rs"]
mod cursor_overlay;


// 共享 CJK 16x16 点阵字体（子集由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_stub.rs"]
mod cjk_font;

// ==================== 系统调用编号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;
const SYS_GETUID: usize = 174;

/// getuid()：读取当前进程用户 ID（诊断 per-app UID 沙箱用）。
unsafe fn getuid() -> u32 {
    syscall1(SYS_GETUID, 0) as u32
}

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NONBLOCK: usize = 0x800;

const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

const FBIOGET_VSCREENINFO: usize = 0x4600;
/// FBIO_PRESENT 请求码：把 canvas 脏区拷入扫描缓冲并翻页（应用驱动显示节奏）。
const FBIO_PRESENT: usize = 0x4603;

const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";

// linux input_event 事件类型 / 编码。
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a; // 330
const BTN_LEFT: u16 = 0x110; // 272

// ==================== 显示 ====================

const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;
const TITLE_H: i32 = 44;
const BACK_SZ: i32 = 36;
const BACK_X: i32 = 4;
const BACK_Y: i32 = 4;

// ==================== 颜色（0x00RRGGBB） ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_TITLE_BG: u32 = 0x00_F2_F4_F8;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_BTN: u32 = 0x00_FF_FF_FF;
const C_BTN_BORDER: u32 = 0x00_D9_DE_E6;
const C_ACCENT: u32 = 0x00_00_B9_6B;

// ==================== 状态 ====================

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
static mut FB_STRIDE_PX: usize = DISPLAY_W;
/// 保留的 /dev/fb0 fd：FBIO_PRESENT 提交脏区需要（mmap 后页表已指向画布）。
static mut FB_FD: isize = -1;
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 从 argv[0] basename 派生的应用名（转大写）。
static mut APP_NAME: [u8; 24] = [0; 24];
static mut APP_NAME_LEN: usize = 0;

/// 鼠标光标状态（与 launcher/oobe 一致）。
static mut CUR_X: i32 = DISPLAY_W as i32 / 2;
static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut MOUSE_DOWN: bool = false;
static mut TOUCH_DOWN: bool = false;
/// 触摸手势追踪（按下点/时刻，用于系统导航手势分类）。
static mut TOUCH_PX: i32 = 0;
static mut TOUCH_PY: i32 = 0;
static mut TOUCH_US: u64 = 0;
static mut CURSOR_DIRTY: bool = true;

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

// ==================== 基础输入输出 ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe { write_fd(1, s.as_bytes()); }
}

/// 当前时间（微秒，自启动）。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(169, tv.as_mut_ptr() as usize, 0); // SYS_GETTIMEOFDAY
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
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

// ==================== 帧缓冲 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

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
        // 保留 fd：FBIO_PRESENT 提交脏区依赖它（mmap 后页表已指向画布）。
        FB_FD = fd as isize;
        true
    }
}

/// 提交任意小脏矩形并请求呈现（FBIO_PRESENT）。
///
/// 光标移动只提交 ~26x26 矩形：内核脏区拷贝与翻页微秒级完成。此前 stub
/// 从不提交呈现，overlay 光标只存在于画布、扫描输出上看不到（不跟手）。
fn fb_present_rects(rects: &[(i32, i32, u32, u32)]) {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() || rects.is_empty() {
            return;
        }
        let n = rects.len().min(8);
        let mut dbuf = [0u8; 4 + 8 * 16];
        dbuf[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let r = rects[i];
            let off = 4 + i * 16;
            dbuf[off..off + 4].copy_from_slice(&r.0.to_le_bytes());
            dbuf[off + 4..off + 8].copy_from_slice(&r.1.to_le_bytes());
            dbuf[off + 8..off + 12].copy_from_slice(&r.2.to_le_bytes());
            dbuf[off + 12..off + 16].copy_from_slice(&r.3.to_le_bytes());
        }
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

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

// ==================== 绘图原语 ====================

fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] = color; }
}

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // 先整体裁剪再逐行 slice::fill（宽通道 memset）：逐像素 put_pixel 的
    // 每像素 4 次边界检查降为每行一次。
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(DISPLAY_W as i32);
    let y1 = (y + h).min(DISPLAY_H as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    unsafe {
        for yy in y0..y1 {
            let base = yy as usize * DISPLAY_W + x0 as usize;
            BACK_BUF[base..base + (x1 - x0) as usize].fill(color);
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

/// 8x8 点阵字体（ASCII 子集）。
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

static FONT_LOWER: [[u8; 8]; 26] = [
    [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00],
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x7C, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x40, 0x42, 0x3C, 0x00],
    [0x02, 0x02, 0x3E, 0x42, 0x42, 0x42, 0x3E, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x7E, 0x40, 0x3C, 0x00],
    [0x0E, 0x10, 0x10, 0x3C, 0x10, 0x10, 0x10, 0x00],
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x08, 0x00, 0x18, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x44, 0x38, 0x00],
    [0x40, 0x40, 0x44, 0x48, 0x70, 0x48, 0x44, 0x00],
    [0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    [0x00, 0x00, 0x66, 0x5A, 0x5A, 0x42, 0x42, 0x00],
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x42, 0x3C, 0x00],
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40],
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x02],
    [0x00, 0x00, 0x5C, 0x62, 0x40, 0x40, 0x40, 0x00],
    [0x00, 0x00, 0x3E, 0x40, 0x3C, 0x02, 0x7C, 0x00],
    [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x0E, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x42, 0x3E, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x5A, 0x5A, 0x24, 0x00],
    [0x00, 0x00, 0x42, 0x24, 0x18, 0x24, 0x42, 0x00],
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    [0x00, 0x00, 0x7E, 0x04, 0x18, 0x20, 0x7E, 0x00],
];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'a'..=b'z' => &FONT_LOWER[(c - b'a') as usize],
        _ => {
            const SPACE: [u8; 8] = [0; 8];
            &SPACE
        }
    }
}

// ==================== TTF 平滑字体（cn.ttf） ====================

/// 把单个码点以 cn.ttf 平滑字形绘制到 (x, y)（y 为字形顶部），返回推进宽度。
fn ttf_glyph(x: i32, y: i32, cp: u32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    let in_b = px_ >= 0 && py_ >= 0 && px_ < DISPLAY_W as i32 && py_ < DISPLAY_H as i32;
                    if in_b {
                        let bg = unsafe { BACK_BUF[py_ as usize * DISPLAY_W + px_ as usize] };
                        put_pixel(px_, py_, ttf_text::font_render::blend(bg, color, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    ttf_glyph(x, y, c as u32, color);
}

/// 绘制单个 16x16 CJK 字形。
fn draw_cjk(x: i32, y: i32, cp: u16, color: u32) {
    ttf_glyph(x, y, cp as u32, color);
}

fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += ttf_glyph(cx, y, cp, color);
        i += n;
    }
}

fn text_width(s: &[u8]) -> i32 {
    ttf_text::font_render::text_width(s, ttf_text::font_render::FONT_PX)
}

fn draw_center(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

// ==================== 毛玻璃 / 液态玻璃（stub 模板） ====================

use sys_nav::glass::{GlassStyle, GlassTarget, STYLE_DANGER, glass_rect, glass_rect_style};

/// 毛玻璃目标：背景取自 BACK_BUF，像素经 put_pixel 写入。
struct StubGlass;
impl GlassTarget for StubGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
            unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
        } else {
            0
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
}

/// 毛玻璃面板（浅色玻璃：白色磨砂 + 浅灰描边）。
fn draw_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(&mut StubGlass, x, y, w, h, r, false, true);
}

/// 自定义样式液态玻璃面板。
fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut StubGlass, x, y, w, h, r, st, true);
}

// ==================== 渲染 ====================

fn render() {
    // 液态玻璃基底：白色背景打底（顶部纯白 → 底部极淡冷灰的浅色渐变）。
    let (tr, tg, tb) = (0xFFu32, 0xFFu32, 0xFFu32);
    let (br, bg, bb) = (0xE9u32, 0xEDu32, 0xF3u32);
    for y in 0..DISPLAY_H as i32 {
        let t = (y as u32 * 255 / DISPLAY_H as u32).min(255);
        let inv = 255 - t;
        let r = (tr * inv + br * t) / 255;
        let g = (tg * inv + bg * t) / 255;
        let b = (tb * inv + bb * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        let iy = y as usize * DISPLAY_W;
        unsafe {
            for x in 0..DISPLAY_W {
                BACK_BUF[iy + x] = row;
            }
        }
    }
    // 液态玻璃标题栏 + 玻璃返回键
    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    draw_glass(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    // 应用名（居中）
    let name = unsafe { &APP_NAME[..APP_NAME_LEN] };
    draw_center(300, name, C_TEXT);
    // 敬请期待说明文字
    draw_center(400, "敬请期待".as_bytes(), C_DIM);
    // 提示行
    draw_center(700, "点击左上角返回或按 ESC 退出".as_bytes(), C_DIM);
    draw_sys_nav();
    // 注意：光标不画进 back buffer —— overlay 快速路径直接写显存。
}

/// 整屏重绘 + 光标 overlay（render/flip 的替代入口）。
fn redraw() {
    render();
    flip();
    cursor_overlay::overlay_redraw(
        unsafe { FB_PTR },
        unsafe { FB_STRIDE_PX },
        unsafe { FB_W } as i32,
        unsafe { FB_H } as i32,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    // 整帧提交 + 光标小矩形补呈（overlay 写在画布上，整帧提交不含光标）。
    fb_present_rects(&[
        (0, 0, unsafe { FB_W } as u32, unsafe { FB_H } as u32),
        cursor_overlay::under_rect(unsafe { CUR_X }, unsafe { CUR_Y }),
    ]);
}

/// 光标移动快速路径：只更新光标小区域，不重绘整屏。
fn cursor_move() {
    // 移动光标并把新旧位置区以小脏矩形提交呈现。
    let rects = cursor_overlay::overlay_move_rects(
        unsafe { FB_PTR },
        unsafe { FB_STRIDE_PX },
        unsafe { FB_W } as i32,
        unsafe { FB_H } as i32,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    fb_present_rects(&rects);
}

/// 系统导航栏（三键 / 手势指示条，按当前模式绘制在屏幕底部，液态玻璃底）。
/// 同时把未过期的错误提示横幅画在最上层。
fn draw_sys_nav() {
    draw_error_banner();
    struct StubCanvas;
    impl sys_nav::NavCanvas for StubCanvas {
        fn put(&mut self, x: i32, y: i32, color: u32) {
            put_pixel(x, y, color);
        }
        fn backdrop(&self, x: i32, y: i32) -> u32 {
            if x >= 0 && y >= 0 && x < DISPLAY_W as i32 && y < DISPLAY_H as i32 {
                unsafe { BACK_BUF[y as usize * DISPLAY_W + x as usize] }
            } else {
                0
            }
        }
    }
    let mut cv = StubCanvas;
    sys_nav::draw_nav_bar(&mut cv, sys_nav::nav_mode());
}

/// 设置一条用户可见的错误提示（自动消失）。
fn show_error(msg: &[u8]) {
    err_ui::err_set(msg, now_us());
}

/// 若存在未过期的错误提示，在标题栏下方绘制横幅。
fn draw_error_banner() {
    if !err_ui::err_active(now_us()) {
        return;
    }
    let msg = err_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    draw_glass_style(0, 48, DISPLAY_W as i32, 44, 0, &STYLE_DANGER);
    let tx = (DISPLAY_W as i32 - text_width(msg)) / 2;
    draw_text(tx, 62, msg, 0x00_FF_FF_FF);
}

// ==================== input ====================

static mut INPUT_FD: isize = -1;

/// 从 argv[0] 提取应用名（路径 basename，转大写）。
fn extract_name(argv: *const usize) {
    unsafe {
        if argv.is_null() {
            return;
        }
        let p0 = *argv as *const u8;
        if p0.is_null() {
            return;
        }
        // 找到 basename 起点（最后一个 '/' 之后）
        let mut base = 0usize;
        let mut i = 0usize;
        while i < 128 {
            let c = *p0.add(i);
            if c == 0 {
                break;
            }
            if c == b'/' {
                base = i + 1;
            }
            i += 1;
        }
        let mut n = 0usize;
        let mut j = base;
        while j < i && n < APP_NAME.len() {
            let c = *p0.add(j);
            APP_NAME[n] = if c >= b'a' && c <= b'z' { c - 32 } else { c };
            n += 1;
            j += 1;
        }
        APP_NAME_LEN = n;
    }
}

/// 轮询输入：处理移动（光标）与点击（Some((x,y))）。
/// 返回 `(点击, 导航动作, 指针是否移动)`。
fn poll_input() -> (Option<(i32, i32)>, sys_nav::NavAction, bool) {
    unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
        }
        if INPUT_FD < 0 {
            return (None, sys_nav::NavAction::None, false);
        }
        let mut click = None;
        let mut nav = sys_nav::NavAction::None;
        let mut moved = false;
        loop {
            let mut buf = [0u8; 24];
            let n = syscall3(SYS_READ, INPUT_FD as usize, buf.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return (click, nav, moved);
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
                    if code == BTN_LEFT {
                        if value == 1 {
                            MOUSE_DOWN = true;
                        } else if value == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            let a = sys_nav::nav_hit(CUR_X, CUR_Y, sys_nav::nav_mode());
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                click = Some((CUR_X, CUR_Y));
                            }
                        }
                    } else if code == BTN_TOUCH {
                        if value == 1 {
                            TOUCH_DOWN = true;
                            TOUCH_PX = CUR_X;
                            TOUCH_PY = CUR_Y;
                            TOUCH_US = now_us();
                        } else if value == 0 && TOUCH_DOWN {
                            TOUCH_DOWN = false;
                            let mode = sys_nav::nav_mode();
                            let a = sys_nav::nav_hit(CUR_X, CUR_Y, mode);
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                let g = sys_nav::classify_swipe(TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode);
                                if g != sys_nav::NavAction::None {
                                    nav = g;
                                } else {
                                    click = Some((CUR_X, CUR_Y));
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

fn back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

/// 轮询 /dev/tty（fd 0）以获取 ESC。
fn poll_key() -> bool {
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as usize, buf.len()) };
    // 读失败 -1（usize 天文数字）按 isize 判负，否则越界 panic
    let n = n as isize;
    if n <= 0 {
        return false;
    }
    for i in 0..(n.min(buf.len() as isize) as usize) {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}

// ==================== 主函数 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, argv: *const usize) -> usize {
    print("[stub] 正在启动\n");
    extract_name(argv);
    // [诊断] 打印进程 uid：验证 launcher 的 per-app UID 沙箱（应显示 10000）
    print("[stub] uid=");
    print_dec(unsafe { getuid() });
    print("\n");

    if !fb_init() {
        print("[stub] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[stub] 帧缓冲 ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" 已映射\n");

    redraw();
    ttf_text::font_init();
    redraw();
    print("[stub] 就绪\n");

    loop {
        let (click, nav, moved) = poll_input();
        if moved {
            // 指针移动：overlay 快速路径，不重绘整屏。
            cursor_move();
        }
        // 系统导航动作优先：返回=退出(0)，桌面=退出(1)，最近任务=退出(2)。
        if nav != sys_nav::NavAction::None {
            print("[stub] 导航 -> 退出\n");
            let code = sys_nav::nav_exit_code(nav);
            unsafe { exit(code); }
        }
        if let Some((x, y)) = click {
            print("[stub] 点击 (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            if back_hit(x, y) {
                print("[stub] 返回 -> 退出\n");
                break;
            }
        }
        if poll_key() {
            print("[stub] ESC -> 退出\n");
            break;
        }
        sleep_ms(16);
    }

    print("[stub] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 辅助函数 ====================

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
        write_fd(2, b"PANIC: stub\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
