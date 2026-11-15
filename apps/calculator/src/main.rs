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
/// FBIO_PRESENT 请求码：把 canvas 脏区拷入扫描缓冲并翻页（应用驱动显示节奏）。
const FBIO_PRESENT: usize = 0x4603;

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

#[path = "../../../shared/display.rs"]
mod display;
static mut VIEW_W: usize = 540;
static mut VIEW_H: usize = 960;
fn display_w() -> usize { unsafe { VIEW_W } }
fn display_h() -> usize { unsafe { VIEW_H } }

fn refresh_geometry() -> bool {
    unsafe {
        if FB_FD < 0 { return false; }
        if syscall3(SYS_IOCTL, FB_FD as usize, display::RESIZE_IOCTL, 0) != 0 { return false; }
        let mut data = [0u8; display::INFO_BYTES];
        if syscall3(SYS_IOCTL, FB_FD as usize, display::INFO_IOCTL, data.as_mut_ptr() as usize) != 0 { return false; }
        let Some(info) = display::DisplayMetrics::decode(&data) else { return false; };
        if info.width as usize == VIEW_W && info.height as usize == VIEW_H { return false; }
        VIEW_W = info.width as usize; VIEW_H = info.height as usize;
        FB_W = VIEW_W; FB_H = VIEW_H; FB_STRIDE_PX = info.stride_bytes as usize / 4; BACK_BUF.resize_len(FB_W * FB_H);
        sys_nav::set_viewport(VIEW_W as i32, VIEW_H as i32);
        true
    }
}

const TITLE_H: i32 = 44;
const DISPLAY_Y: i32 = 52;
const DISPLAY_HGT: i32 = 132;
const BTN_TOP: i32 = DISPLAY_Y + DISPLAY_HGT + 8;
const BTN_COLS: i32 = 4;
const BTN_ROWS: i32 = 5;
const BTN_GAP: i32 = 8;
const BTN_MARGIN: i32 = 10;
fn btn_w() -> i32 { (display_w() as i32 - 2 * BTN_MARGIN - (BTN_COLS - 1) * BTN_GAP) / BTN_COLS }
/// 按键高度：5 行按键区整体位于系统导航栏（NAV_Y）之上。
fn btn_h() -> i32 { ((sys_nav::nav_y() - BTN_TOP - 20) / BTN_ROWS - BTN_GAP).max(36) }

/// 返回键命中区（标题栏左上角）。
const BACK_X: i32 = 4;
const BACK_Y: i32 = 4;
const BACK_SZ: i32 = 36;

// ==================== 颜色（0x00RRGGBB） ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_TITLE_BG: u32 = 0x00_F2_F4_F8;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_DISPLAY_BG: u32 = 0x00_FF_FF_FF;
const C_BTN: u32 = 0x00_FF_FF_FF;
const C_BTN_BORDER: u32 = 0x00_D9_DE_E6;
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
/// 保留的 /dev/fb0 fd：FBIO_PRESENT 提交脏区需要（mmap 后页表已指向画布，fd 仅用于 ioctl）。
static mut FB_FD: isize = -1;
static mut FB_W: usize = 540;
static mut FB_H: usize = 960;
static mut FB_STRIDE_PX: usize = 540;
#[path = "../../../shared/pixel_buffer.rs"]
mod pixel_buffer;
static mut BACK_BUF: pixel_buffer::PixelBuffer = pixel_buffer::PixelBuffer::new(display::MAX_PIXELS, 540 * 960);

/// 计算器状态。
static mut ACC: i64 = 0;
static mut CUR: i64 = 0;
static mut OP: u8 = 0; // 0=无, '+', '-', '*', '/', '%'
static mut ENTERING: bool = false;
static mut ERROR: bool = false;
static mut JUST_EQ: bool = false;

/// 输入状态。
static mut INPUT_FD: isize = -1;
static mut CUR_X: i32 = 270;
static mut CUR_Y: i32 = 480;
static mut TOUCH_DOWN: bool = false;
static mut MOUSE_DOWN: bool = false;
/// 触摸手势追踪（按下点/时刻，用于系统导航手势分类）。
static mut TOUCH_PX: i32 = 0;
static mut TOUCH_PY: i32 = 0;
static mut TOUCH_US: u64 = 0;

/// 按键标签（4 列 x 5 行，行优先）。
static BTNS: [&[u8]; 20] = [
    b"C", b"DEL", b"%", b"/",
    b"7", b"8", b"9", b"*",
    b"4", b"5", b"6", b"-",
    b"1", b"2", b"3", b"+",
    b"+/-", b"0", b"00", b"=",
];

// ==================== 系统调用封装 ====================

use kunxrt::syscall::syscall1;

use kunxrt::syscall::syscall2;

use kunxrt::syscall::syscall3;

use kunxrt::syscall::syscall6;

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

/// 当前时间（微秒，自启动）。
fn now_us() -> u64 {
    // 单调钟：RTC 就绪前后墙钟会跳变，时间差判定（动画/超时）必须用
    // CLOCK_MONOTONIC（黑屏事故同族防御）。
    let mut tv = [0u8; 16];
    unsafe {
        syscall3(113, 1, tv.as_mut_ptr() as usize, 0); // CLOCK_GETTIME, MONOTONIC
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
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
        let len = display::MAX_PIXELS * 4;
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
        // 保留 fd：FBIO_PRESENT / FBIO_PUT_DAMAGE 依赖它提交脏区（mmap 后
        // 页表已指向画布，fd 仅用于 ioctl）。
        FB_FD = fd as isize;
        VIEW_W = FB_W; VIEW_H = FB_H;
        BACK_BUF.resize_len(FB_W * FB_H);
        sys_nav::set_viewport(FB_W as i32, FB_H as i32);
        true
    }
}

/// 把 back buffer 一次性拷贝到显存，并提交整屏 FBIO_PRESENT 让内核翻页上屏。
fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        if FB_W == display_w() && FB_H == display_h() && FB_STRIDE_PX == display_w() {
            core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB_PTR, display_w() * display_h());
        } else {
            let w = FB_W.min(display_w());
            let h = FB_H.min(display_h());
            for y in 0..h {
                core::ptr::copy_nonoverlapping(
                    BACK_BUF.as_ptr().add(y * display_w()),
                    FB_PTR.add(y * FB_STRIDE_PX),
                    w,
                );
            }
        }
        fb_present();
    }
}

/// 提交整屏脏区并请求呈现（FBIO_PRESENT，单矩形 (0,0,FB_W,FB_H)）。
///
/// 内核在首次 FBIO_PRESENT 后进入应用驱动模式：不再由定时器自动翻页，
/// 显示节奏完全由本调用驱动（与 terminal 的 present_frame 同一最小实现）。
fn fb_present() {
    unsafe {
        if FB_FD < 0 || FB_PTR.is_null() {
            return;
        }
        let mut dbuf = [0u8; 4 + 16];
        // 缓冲布局：[count:u32][{x:i32, y:i32, w:u32, h:u32}]，全部小端。
        dbuf[0..4].copy_from_slice(&1u32.to_le_bytes());
        dbuf[4..8].copy_from_slice(&0i32.to_le_bytes());
        dbuf[8..12].copy_from_slice(&0i32.to_le_bytes());
        dbuf[12..16].copy_from_slice(&(FB_W as u32).to_le_bytes());
        dbuf[16..20].copy_from_slice(&(FB_H as u32).to_le_bytes());
        // 忽略返回值：内核在应用接管模式下始终呈现（无脏区时为 no-op）。
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

/// 提交任意小脏矩形并请求呈现（FBIO_PRESENT，光标移动快速路径）。
///
/// 光标移动只提交 ~26x26 矩形：内核脏区拷贝与翻页微秒级完成，替代整屏
/// 2MB 提交（QEMU 软件渲染下整屏提交是移动卡顿的直接原因）。
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

// ==================== 绘制原语 ====================

fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= display_w() as i32 || y >= display_h() as i32 {
        return;
    }
    unsafe { BACK_BUF[y as usize * display_w() + x as usize] = color; }
}

// ==================== 毛玻璃 / 液态玻璃（计算器） ====================

use sys_nav::glass::{GlassStyle, GlassTarget, STYLE_DANGER, glass_rect_style};

/// 毛玻璃目标：背景取自 BACK_BUF，像素经 put_pixel 写入。
struct CalcGlass;
impl GlassTarget for CalcGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < display_w() as i32 && y < display_h() as i32 {
            unsafe { BACK_BUF[y as usize * display_w() + x as usize] }
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
    glass_rect_style(&mut CalcGlass, x, y, w, h, r, &STYLE_PANEL, true);
}

/// 自定义样式液态玻璃面板。
fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut CalcGlass, x, y, w, h, r, st, true);
}

/// 运算符按钮玻璃样式（琥珀色）。
static STYLE_OP: GlassStyle = GlassStyle {
    tint: 0x00_E2_96_06,
    alpha: 232,
    spec: 46,
    sheen: 22,
    border: 0x00_F5_C2_6B,
    border_a: 160,
};

/// 浅色面板玻璃：白色着色 + 浅灰描边（白色背景上仍可辨识的卡片/按钮）。
static STYLE_PANEL: GlassStyle = GlassStyle {
    tint: 0x00_FF_FF_FF,
    alpha: 225,
    spec: 60,
    sheen: 18,
    border: 0x00_D9_DE_E6,
    border_a: 220,
};

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // 先整体裁剪再逐行 slice::fill（宽通道 memset）：逐像素 put_pixel 的
    // 每像素 4 次边界检查降为每行一次。
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(display_w() as i32);
    let y1 = (y + h).min(display_h() as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    unsafe {
        for yy in y0..y1 {
            let base = yy as usize * display_w() + x0 as usize;
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
                    let in_b = px_ >= 0 && py_ >= 0 && px_ < display_w() as i32 && py_ < display_h() as i32;
                    if in_b {
                        let bg = unsafe { BACK_BUF[py_ as usize * display_w() + px_ as usize] };
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
        } else {
            // 输入溢出：此前静默丢弃按键，屏幕无反应。
            ERROR = true;
            show_error("数字超出范围".as_bytes());
        }
    }
}

/// 应用挂起的运算符：ACC = ACC OP CUR，结果写回 CUR；除零/溢出置 ERROR 并返回 false。
fn apply_op() -> bool {
    unsafe {
        let a = ACC;
        let b = CUR;
        let r = match OP {
            b'+' => match a.checked_add(b) {
                Some(v) => v,
                None => {
                    ERROR = true;
                    return false;
                }
            },
            b'-' => match a.checked_sub(b) {
                Some(v) => v,
                None => {
                    ERROR = true;
                    return false;
                }
            },
            b'*' => match a.checked_mul(b) {
                Some(v) => v,
                None => {
                    ERROR = true;
                    return false;
                }
            },
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
    let x = BTN_MARGIN + col * (btn_w() + BTN_GAP);
    let y = BTN_TOP + row * (btn_h() + BTN_GAP);
    (x, y, btn_w(), btn_h())
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

// ==================== 动画（按压反馈 / 结果弹出）====================
// 规格对齐 docs/animations.md 3.1（图标按压反馈）：按住下沉变暗（即时），
// 松开闪光自最强线性衰减（FastOutSlowIn 节奏）；"=" 结果 ease-out 滑入。
// 全部整数运算，无堆、无浮点。

/// 按住的按钮槽号（-1 = 无）；按下即视觉反馈，抬起转闪光。
static mut HOLD_BTN: isize = -1;
/// 按下状态发生变化（主循环需重绘一次）。
static mut HOLD_CHANGED: bool = false;
/// 松开闪光：按钮槽号（-1 = 空闲）与起始时刻（µs）。
static mut FLASH_BTN: isize = -1;
static mut FLASH_START: u64 = 0;
/// 闪光时长（ms）。
const FLASH_MS: u64 = 150;
/// "=" 结果弹出起始时刻（µs，0 = 空闲）与时长（ms）。
static mut RESULT_START: u64 = 0;
const RESULT_MS: u64 = 180;

/// FastOutSlowIn 风格 ease-out（0..255 → 0..255，二次曲线近似）。
fn ease_out8(t: u32) -> u32 {
    let inv = 255 - t.min(255);
    255 - (inv * inv + 127) / 255
}

/// 像素向黑色压暗（a: 0..255）。
fn darken_px(bg: u32, a: u32) -> u32 {
    let (r, g, b) = ((bg >> 16) & 0xFF, (bg >> 8) & 0xFF, bg & 0xFF);
    let k = 255 - a.min(255);
    (((r * k / 255) << 16) | ((g * k / 255) << 8) | (b * k / 255)) & 0xFF_FF_FF
}

/// 在按钮圆角矩形范围内把像素压暗（按压/闪光叠加层）。
fn flash_overlay(i: usize, a: u32) {
    if a == 0 {
        return;
    }
    let (x, y, w, h) = btn_rect(i);
    let r = (h * 22 / 100).min(22);
    let x1 = x + w - 1;
    let y1 = y + h - 1;
    for yy in y..y + h {
        for xx in x..x + w {
            // 圆角覆盖测试：四个角按 1/4 圆判定，其余全覆盖
            let inside = if xx < x + r && yy < y + r {
                let dx = xx - (x + r);
                let dy = yy - (y + r);
                dx * dx + dy * dy <= r * r
            } else if xx > x1 - r && yy < y + r {
                let dx = xx - (x1 - r);
                let dy = yy - (y + r);
                dx * dx + dy * dy <= r * r
            } else if xx < x + r && yy > y1 - r {
                let dx = xx - (x + r);
                let dy = yy - (y1 - r);
                dx * dx + dy * dy <= r * r
            } else if xx > x1 - r && yy > y1 - r {
                let dx = xx - (x1 - r);
                let dy = yy - (y1 - r);
                dx * dx + dy * dy <= r * r
            } else {
                true
            };
            if inside {
                let idx = yy as usize * display_w() + xx as usize;
                let bg = unsafe { BACK_BUF[idx] };
                unsafe {
                    BACK_BUF[idx] = darken_px(bg, a);
                }
            }
        }
    }
}

/// 按钮当前的闪光叠加强度（0 = 无）。
fn flash_alpha(i: usize) -> u32 {
    unsafe {
        if FLASH_BTN != i as isize {
            return 0;
        }
        let el = now_us().saturating_sub(FLASH_START) / 1000;
        if el >= FLASH_MS {
            return 0;
        }
        let t = (el * 255 / FLASH_MS) as u32;
        200 * (255 - ease_out8(t)) / 255
    }
}

/// 渲染单个按钮（常态 / 按住下沉 / 松开闪光自动叠加）。
fn render_button(i: usize) {
    let (x, y, w, h) = btn_rect(i);
    let r = (h * 22 / 100).min(22);
    match button_color(i) {
        C_CLEAR => draw_glass_style(x, y, w, h, r, &STYLE_DANGER),
        C_OP => draw_glass_style(x, y, w, h, r, &STYLE_OP),
        _ => draw_glass(x, y, w, h, r),
    }
    let label = BTNS[i];
    let tx = x + (w - text_width(label)) / 2;
    // 红色删除键玻璃上用白字，其余浅色玻璃用深色字。
    let label_col = if button_color(i) == C_CLEAR { 0x00_FF_FF_FF } else { C_TEXT };
    // 按住：内容下沉 1px + 恒定压暗（按压态）；松开：闪光衰减叠加。
    let held = unsafe { HOLD_BTN == i as isize };
    let ty = y + (h - 8) / 2 + if held { 1 } else { 0 };
    draw_text(tx, ty, label, label_col);
    let fa = flash_alpha(i);
    if held {
        flash_overlay(i, 90);
    }
    if fa > 0 {
        flash_overlay(i, fa);
    }
}

/// 推进动画状态：返回是否仍有动画在播（主循环需逐帧重绘）。
/// 到期时清除状态，末次调用返回 false（caller 重绘一次恢复常态）。
fn advance_anim() -> bool {
    let mut active = false;
    unsafe {
        let now = now_us();
        if FLASH_BTN >= 0 {
            active = true;
            if now.saturating_sub(FLASH_START) / 1000 >= FLASH_MS {
                FLASH_BTN = -1;
            }
        }
        if RESULT_START != 0 {
            active = true;
            if now.saturating_sub(RESULT_START) / 1000 >= RESULT_MS {
                RESULT_START = 0;
            }
        }
    }
    active
}

// ==================== 渲染 ====================

fn render_title() {
    // 液态玻璃标题栏（模糊背景 + 深色着色 + 高光描边）。
    draw_glass(0, 0, display_w() as i32, TITLE_H, 0);
    // 返回键：玻璃圆角方块。
    draw_glass(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    // 标题
    let s = b"CALCULATOR";
    let tx = (display_w() as i32 - text_width(s)) / 2;
    draw_text(tx, (TITLE_H - 8) / 2, s, C_TEXT);
}

fn render_display() {
    // 液态玻璃显示面板（圆角，模糊按钮区背景）。
    draw_glass(8, DISPLAY_Y, display_w() as i32 - 16, DISPLAY_HGT, 20);
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
    // 当前值；"=" 结果弹出：ease-out 自下 8px 滑入（动画期内主循环逐帧重绘）
    let n = fmt_i64(&mut buf, cur);
    let tx = display_w() as i32 - 16 - n as i32 * 8;
    let mut dy = 0i32;
    unsafe {
        if RESULT_START != 0 {
            let el = now_us().saturating_sub(RESULT_START) / 1000;
            if el < RESULT_MS {
                let t = ((el * 255) / RESULT_MS) as u32;
                let e = ease_out8(t);
                dy = (((255 - e) * 8) / 255) as i32;
            }
        }
    }
    draw_text(tx, DISPLAY_Y + DISPLAY_HGT - 28 + dy, &buf[..n], C_TEXT);
}

fn render_buttons() {
    for i in 0..BTNS.len() {
        render_button(i);
    }
}

fn render() {
    // 液态玻璃基底：白色背景打底（顶部纯白 → 底部极淡冷灰的浅色渐变）。
    let (tr, tg, tb) = (0xFFu32, 0xFFu32, 0xFFu32);
    let (br, bg, bb) = (0xE9u32, 0xEDu32, 0xF3u32);
    for y in 0..display_h() as i32 {
        let t = (y as u32 * 255 / display_h() as u32).min(255);
        let inv = 255 - t;
        let r = (tr * inv + br * t) / 255;
        let g = (tg * inv + bg * t) / 255;
        let b = (tb * inv + bb * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        let iy = y as usize * display_w();
        unsafe {
            for x in 0..display_w() {
                BACK_BUF[iy + x] = row;
            }
        }
    }
    render_title();
    render_display();
    render_buttons();
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
    // overlay 直接写画布，flip 的整屏提交不含光标像素；补光标小矩形呈现
    // 上屏即可（不再整屏重复提交）。
    fb_present_rects(&[cursor_overlay::under_rect(
        unsafe { CUR_X },
        unsafe { CUR_Y },
    )]);
}

/// 光标移动快速路径：只更新光标小区域，不重绘整屏。
fn cursor_move() {
    // 移动光标并把新旧位置区以小脏矩形提交呈现（不再整屏提交）。
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
    struct CalcCanvas;
    impl sys_nav::NavCanvas for CalcCanvas {
        fn put(&mut self, x: i32, y: i32, color: u32) {
            put_pixel(x, y, color);
        }
        fn backdrop(&self, x: i32, y: i32) -> u32 {
            if x >= 0 && y >= 0 && x < display_w() as i32 && y < display_h() as i32 {
                unsafe { BACK_BUF[y as usize * display_w() + x as usize] }
            } else {
                0
            }
        }
    }
    let mut cv = CalcCanvas;
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
    fill_rect(0, 48, display_w() as i32, 44, 0x00_7A_1F_1F);
    draw_rect(0, 48, display_w() as i32, 44, 0x00_C0_40_40);
    let tx = (display_w() as i32 - text_width(msg)) / 2;
    draw_text(tx, 62, msg, 0x00_FF_FF_FF);
}

// ==================== 输入 ====================

/// 轮询 /dev/input/event0，检测一次完整点击（按下并抬起）并跟踪鼠标指针。
///
/// 返回 `(点击坐标, 指针是否移动)`：指针移动时上层需重绘以更新光标位置。
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
                    // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
                    if code == REL_X {
                        CUR_X = (CUR_X + value).clamp(0, display_w() as i32 - 1);
                        moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + value).clamp(0, display_h() as i32 - 1);
                        moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = value.clamp(0, display_w() as i32 - 1);
                        moved = true;
                    } else if code == ABS_Y {
                        CUR_Y = value.clamp(0, display_h() as i32 - 1);
                        moved = true;
                    }
                }
                EV_KEY => {
                    if code == BTN_TOUCH {
                        if value == 1 {
                            TOUCH_DOWN = true;
                            TOUCH_PX = CUR_X;
                            TOUCH_PY = CUR_Y;
                            TOUCH_US = now_us();
                            press_feedback_begin();
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
                    } else if code == BTN_LEFT {
                        if value == 1 {
                            MOUSE_DOWN = true;
                            // QEMU GUI（virtio-tablet）以 BTN_LEFT 上报：补齐手势
                            // 状态，全面屏手势在鼠标/数位板下与触摸同规则。
                            TOUCH_PX = CUR_X;
                            TOUCH_PY = CUR_Y;
                            TOUCH_US = now_us();
                            press_feedback_begin();
                        } else if value == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            let mode = sys_nav::nav_mode();
                            let a = sys_nav::nav_hit(CUR_X, CUR_Y, mode);
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                let dur = now_us().saturating_sub(TOUCH_US) / 1000;
                                let g = sys_nav::classify_swipe(
                                    TOUCH_PX, TOUCH_PY, CUR_X, CUR_Y, dur, mode,
                                );
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

/// 轮询 /dev/tty（fd 0，标准输入）：检测 ESC（0x1b）按键。
fn poll_key() -> bool {
    // ★ 独立非阻塞 tty fd：fd 0 是阻塞语义（musl fgets），无数据时直读
    // 会把应用卡死在内核 yield 循环（tty_read 此前无视 O_NONBLOCK——
    // 内核已修，此处也换非阻塞 fd 双保险）。
    static mut TTY_POLL_FD: isize = -1;
    let fd = unsafe {
        if TTY_POLL_FD < 0 {
            let mut p = [0u8; 16];
            let s = b"/dev/tty\0";
            p[..s.len()].copy_from_slice(s);
            TTY_POLL_FD = syscall3(SYS_OPENAT, 0xFFFF_FFFF_FFFF_FF9Cusize, p.as_ptr() as usize, 0x800) as isize;
        }
        TTY_POLL_FD
    };
    if fd < 0 {
        return false;
    }
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
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

/// 按下瞬间记录按压目标按钮（导航区除外）：按住显示下沉态。
/// 由 poll_input 在 EV_KEY 按下事件调用。
fn press_feedback_begin() {
    unsafe {
        let b = if sys_nav::nav_hit(CUR_X, CUR_Y, sys_nav::nav_mode())
            != sys_nav::NavAction::None
        {
            -1
        } else {
            button_at(CUR_X, CUR_Y).map_or(-1, |i| i as isize)
        };
        if b != HOLD_BTN {
            HOLD_BTN = b;
            HOLD_CHANGED = true;
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[calculator] 正在启动\n");

    if !fb_init() {
        print("[calculator] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[calculator] 帧缓冲 ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" 已映射\n");

    redraw();
    ttf_text::font_init();
    redraw();
    print("[calculator] 就绪\n");

    loop {
        if refresh_geometry() { render(); flip(); }
        let (click, nav, moved) = poll_input();
        if moved {
            // 指针移动：overlay 快速路径，不重绘整屏。
            cursor_move();
        }
        // 系统导航动作优先：返回=退出(0)，桌面=退出(1)，最近任务=退出(2)。
        if nav != sys_nav::NavAction::None {
            print("[calculator] 导航 -> 退出\n");
            let code = sys_nav::nav_exit_code(nav);
            unsafe { exit(code); }
        }
        // 按压态开始/结束：重绘一次显示下沉反馈
        unsafe {
            if HOLD_CHANGED {
                HOLD_CHANGED = false;
                redraw();
            }
        }
        if let Some((x, y)) = click {
            print("[calculator] 点击 (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            if back_hit(x, y) {
                print("[calculator] 返回 -> 退出\n");
                break;
            }
            if let Some(i) = button_at(x, y) {
                press_button(i);
                unsafe {
                    // 松开：按压态转闪光衰减；"=" 触发结果弹出
                    FLASH_BTN = i as isize;
                    FLASH_START = now_us();
                    HOLD_BTN = -1;
                    if i == 19 && !ERROR {
                        RESULT_START = now_us();
                    }
                }
                redraw();
            } else {
                unsafe {
                    if HOLD_BTN != -1 {
                        // 释放落空（按住拖出网格后松开，鼠标路径现在也会走到）：
                        // 必须置变更标志，否则按钮画面停在下沉态直到下一次
                        // 点击/动画才自愈。
                        HOLD_BTN = -1;
                        HOLD_CHANGED = true;
                    }
                }
            }
        }
        // 动画推进：闪光/结果弹出期间逐帧重绘（动画期不睡眠，帧率拉满；
        // 空闲时 8ms 让出）
        let animating = advance_anim();
        if animating {
            redraw();
        }
        if poll_key() {
            print("[calculator] ESC -> 退出\n");
            break;
        }
        if !animating {
            sleep_ms(8);
        }
    }

    print("[calculator] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

kunxrt::entry!();

kunxrt::panic_handler!("calculator");
