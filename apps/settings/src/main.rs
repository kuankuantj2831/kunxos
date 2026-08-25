// KunxOS Settings 应用：系统设置（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/settings）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 绘制全屏设置窗口：标题栏（返回键）+ 设置项列表 + 应用管理（恢复已移除模块）
//   3. 主循环：轮询 /dev/input/event0 触摸事件（恢复/返回）与 /dev/tty 键盘（ESC）
//   4. 点击左上角「返回」或底部「BACK」或按 ESC 退出（exit(0)），
//      由 launcher wait4 回收后重绘桌面
//
// 说明：framebuffer 由内核启动时初始化，应用直接 open/mmap /dev/fb0 覆盖桌面
// 全屏绘制；无窗口管理器，退出后桌面由 launcher 重绘。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_settings.rs"]
mod cjk_font;

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
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NONBLOCK: usize = 0x800;

const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";
const MODULES_PATH: &str = "/data/oobe/modules";
const REMOVED_PATH: &str = "/data/oobe/modules_removed";

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

const BACK_X: i32 = 4;
const BACK_Y: i32 = 4;
const BACK_SZ: i32 = 36;

const ROW_MARGIN: i32 = 20;
const ROW_W: i32 = DISPLAY_W as i32 - 2 * ROW_MARGIN;
const ROW_H: i32 = 52;
const ROW_GAP: i32 = 10;
const LIST_Y: i32 = 64;

const APP_ROW_Y: i32 = LIST_Y + 5 * (ROW_H + ROW_GAP);
const APP_ROW_H: i32 = 64;
const STATUS_Y: i32 = APP_ROW_Y + APP_ROW_H + 24;

const BACK_BTN_H: i32 = 60;
const BACK_BTN_Y: i32 = DISPLAY_H as i32 - BACK_BTN_H - 16;
const BACK_BTN_X: i32 = ROW_MARGIN;
const BACK_BTN_W: i32 = ROW_W;

// ==================== 颜色（0x00RRGGBB） ====================

const C_BG: u32 = 0x00_0F_17_2A;
const C_TITLE_BG: u32 = 0x00_0C_1A_36;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A8_C0;
const C_ROW: u32 = 0x00_1B_2B_4A;
const C_ROW_BORDER: u32 = 0x00_3A_4A_6A;
const C_ACCENT: u32 = 0x00_22_C5_5E;
const C_GREEN: u32 = 0x00_22_C5_5E;
const C_BACK: u32 = 0x00_35_B0_E6;

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

static FONT_MINUS: [u8; 8] = [0x00, 0x00, 0x00, 0x3E, 0x00, 0x00, 0x00, 0x00];
static FONT_DOT: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00];
static FONT_LT: [u8; 8] = [0x08, 0x10, 0x20, 0x40, 0x20, 0x10, 0x08, 0x00];
static FONT_GT: [u8; 8] = [0x20, 0x10, 0x08, 0x04, 0x08, 0x10, 0x20, 0x00];
static FONT_SPACE: [u8; 8] = [0x00; 8];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'-' => &FONT_MINUS,
        b'.' => &FONT_DOT,
        b'<' => &FONT_LT,
        b'>' => &FONT_GT,
        _ => &FONT_SPACE,
    }
}

// ==================== 状态 ====================

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
static mut FB_STRIDE_PX: usize = DISPLAY_W;
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 恢复结果：-1 未执行，0 无已移除模块，>0 已恢复数量。
static mut RESTORED: i32 = -1;

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

/// 设置项列表（左标签, 右值）。
static ROWS: [(&[u8], &[u8]); 5] = [
    (b"\xe6\x97\xa0\xe7\xba\xbf\xe7\xbd\x91\xe7\xbb\x9c", b"\xe5\xb7\xb2\xe8\xbf\x9e\xe6\x8e\xa5"),   // 无线网络 / 已连接
    (b"\xe8\x93\x9d\xe7\x89\x99", b"\xe5\x85\xb3\xe9\x97\xad"),                                     // 蓝牙 / 关闭
    (b"\xe6\x98\xbe\xe7\xa4\xba", b"540X960"),                                                           // 显示
    (b"\xe5\xa3\xb0\xe9\x9f\xb3", b"\xe5\xbc\x80\xe5\x90\xaf"),                                     // 声音 / 开启
    (b"\xe5\x85\xb3\xe4\xba\x8e", b"KUNXOS 0.9.0"),                                                       // 关于
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

unsafe fn read_file(path: &str, buf: &mut [u8]) -> isize {
    let fd = open(path, O_RDONLY);
    if fd < 0 {
        return -1;
    }
    let mut total = 0usize;
    while total < buf.len() {
        let n = syscall3(
            SYS_READ,
            fd as usize,
            buf.as_mut_ptr().add(total) as usize,
            buf.len() - total,
        ) as isize;
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    syscall1(SYS_CLOSE, fd as usize);
    total as isize
}

unsafe fn write_file(path: &str, data: &[u8]) {
    let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
    if fd >= 0 {
        write_fd(fd as usize, data);
        syscall1(SYS_CLOSE, fd as usize);
    }
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

/// 绘制单个 16x16 中文字形。
fn draw_cjk(x: i32, y: i32, idx: usize, color: u32) {
    let base = idx * 32;
    for row in 0..16i32 {
        let b0 = cjk_font::CJK_BITMAPS[base + row as usize * 2];
        let b1 = cjk_font::CJK_BITMAPS[base + row as usize * 2 + 1];
        let bits = ((b0 as u16) << 8) | b1 as u16;
        for col in 0..16i32 {
            if bits & (0x8000u16 >> col) != 0 {
                put_pixel(x + col, y + row, color);
            }
        }
    }
}

fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = cjk_font::utf8_next(s, i);
        if cp >= 0x80 {
            if let Some(idx) = cjk_font::cjk_index(cp as u16) {
                draw_cjk(cx, y, idx, color);
            }
            cx += cjk_font::CJK_W;
            i += n;
        } else {
            draw_char(cx, y, cp as u8, color);
            cx += 8;
            i += n;
        }
    }
}

fn text_width(s: &[u8]) -> i32 {
    cjk_font::text_width(s)
}

/// 十进制格式化到缓冲，返回长度。
fn fmt_u32(buf: &mut [u8], mut v: u32) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut pos = buf.len();
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = buf.len() - pos;
    for i in 0..n {
        buf[i] = buf[pos + i];
    }
    n
}

// ==================== 应用管理（恢复已移除模块） ====================

fn is_sep(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

fn trim_end(data: &[u8]) -> usize {
    let mut end = data.len();
    while end > 0 && (is_sep(data[end - 1]) || data[end - 1] == b',') {
        end -= 1;
    }
    end
}

fn count_tokens(data: &[u8]) -> u32 {
    let mut n = 0u32;
    let mut i = 0usize;
    while i < data.len() {
        while i < data.len() && (is_sep(data[i]) || data[i] == b',') {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        n += 1;
        while i < data.len() && data[i] != b',' {
            i += 1;
        }
    }
    n
}

/// 把 modules_removed 合并回 modules，返回恢复数量。
fn restore_removed() -> i32 {
    static mut MOD_BUF: [u8; 512] = [0u8; 512];
    static mut REM_BUF: [u8; 512] = [0u8; 512];
    static mut NEW_MOD: [u8; 512] = [0u8; 512];
    unsafe {
        let mods = read_file(MODULES_PATH, &mut MOD_BUF);
        let rems = read_file(REMOVED_PATH, &mut REM_BUF);
        if rems <= 0 {
            print("[settings] no removed modules\n");
            return 0;
        }
        let rem_len = trim_end(&REM_BUF[..rems as usize]);
        if rem_len == 0 {
            print("[settings] no removed modules\n");
            return 0;
        }
        let count = count_tokens(&REM_BUF[..rem_len]);

        let mut n = 0usize;
        if mods > 0 {
            let m = trim_end(&MOD_BUF[..mods as usize]);
            if m > 0 {
                NEW_MOD[..m].copy_from_slice(&MOD_BUF[..m]);
                n = m;
            }
        }
        // 越界保护：拼接后必须仍在 NEW_MOD 内（防止 rem_len 大时切片越界 panic）
        if n + 1 + rem_len > NEW_MOD.len() {
            print("[settings] modules file too large, restore aborted\n");
            return 0;
        }
        if n > 0 {
            NEW_MOD[n] = b',';
            n += 1;
        }
        NEW_MOD[n..n + rem_len].copy_from_slice(&REM_BUF[..rem_len]);
        n += rem_len;

        write_file(MODULES_PATH, &NEW_MOD[..n]);
        write_file(REMOVED_PATH, b"");

        print("[settings] restored ");
        print_dec(count);
        print(" app(s)\n");
        count as i32
    }
}

// ==================== 布局与命中测试 ====================

fn back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

fn app_row_hit(x: i32, y: i32) -> bool {
    x >= ROW_MARGIN && x < ROW_MARGIN + ROW_W && y >= APP_ROW_Y && y < APP_ROW_Y + APP_ROW_H
}

fn back_btn_hit(x: i32, y: i32) -> bool {
    x >= BACK_BTN_X && x < BACK_BTN_X + BACK_BTN_W && y >= BACK_BTN_Y && y < BACK_BTN_Y + BACK_BTN_H
}

// ==================== 渲染 ====================

fn render_title() {
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_ROW);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_ROW_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    let s = "设置".as_bytes();
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, (TITLE_H - 8) / 2, s, C_TEXT);
}

fn render_list() {
    for i in 0..ROWS.len() {
        let y = LIST_Y + i as i32 * (ROW_H + ROW_GAP);
        fill_rect(ROW_MARGIN, y, ROW_W, ROW_H, C_ROW);
        draw_rect(ROW_MARGIN, y, ROW_W, ROW_H, C_ROW_BORDER);
        let (left, right) = ROWS[i];
        draw_text(ROW_MARGIN + 16, y + (ROW_H - 8) / 2, left, C_TEXT);
        let rw = text_width(right);
        draw_text(ROW_MARGIN + ROW_W - 16 - rw, y + (ROW_H - 8) / 2, right, C_DIM);
    }
}

fn render_app_row() {
    fill_rect(ROW_MARGIN, APP_ROW_Y, ROW_W, APP_ROW_H, C_ACCENT);
    draw_rect(ROW_MARGIN, APP_ROW_Y, ROW_W, APP_ROW_H, C_ROW_BORDER);
    draw_text(ROW_MARGIN + 16, APP_ROW_Y + (APP_ROW_H - 8) / 2, "应用管理".as_bytes(), C_TEXT);
    let s = "恢复 >".as_bytes();
    let rw = text_width(s);
    draw_text(ROW_MARGIN + ROW_W - 16 - rw, APP_ROW_Y + (APP_ROW_H - 8) / 2, s, C_TEXT);
}

fn render_status() {
    let r = unsafe { RESTORED };
    if r < 0 {
        draw_text(ROW_MARGIN + 4, STATUS_Y, "点击应用管理恢复已移除应用".as_bytes(), C_DIM);
        draw_text(ROW_MARGIN + 4, STATUS_Y + 20, "已移除应用".as_bytes(), C_DIM);
    } else if r == 0 {
        draw_text(ROW_MARGIN + 4, STATUS_Y, "没有已移除的应用".as_bytes(), C_GREEN);
    } else {
        let mut buf = [0u8; 32];
        let mut n = 0usize;
        for &c in "已恢复 ".as_bytes() {
            buf[n] = c;
            n += 1;
        }
        let mut num = [0u8; 12];
        let ln = fmt_u32(&mut num, r as u32);
        buf[n..n + ln].copy_from_slice(&num[..ln]);
        n += ln;
        for &c in " 个应用".as_bytes() {
            buf[n] = c;
            n += 1;
        }
        draw_text(ROW_MARGIN + 4, STATUS_Y, &buf[..n], C_GREEN);
        draw_text(ROW_MARGIN + 4, STATUS_Y + 20, "重启桌面后生效".as_bytes(), C_DIM);
    }
}

fn render_back_btn() {
    fill_rect(BACK_BTN_X, BACK_BTN_Y, BACK_BTN_W, BACK_BTN_H, C_BACK);
    draw_rect(BACK_BTN_X, BACK_BTN_Y, BACK_BTN_W, BACK_BTN_H, C_TEXT);
    let s = "返回".as_bytes();
    let tx = BACK_BTN_X + (BACK_BTN_W - text_width(s)) / 2;
    draw_text(tx, BACK_BTN_Y + (BACK_BTN_H - 8) / 2, s, C_TEXT);
}

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    render_title();
    render_list();
    render_app_row();
    render_status();
    render_back_btn();
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
    print("[settings] starting\n");

    if !fb_init() {
        print("[settings] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[settings] framebuffer ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" mapped\n");

    render();
    flip();
    print("[settings] ready\n");

    loop {
        let (click, moved) = poll_input();
        if moved {
            // 指针移动：重绘以更新光标位置。
            render();
            flip();
        }
        if let Some((x, y)) = click {
            print("[settings] tap (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            if back_hit(x, y) || back_btn_hit(x, y) {
                print("[settings] back -> exit\n");
                break;
            }
            if app_row_hit(x, y) {
                let n = restore_removed();
                unsafe { RESTORED = n; }
                render();
                flip();
            }
        }
        if poll_key() {
            print("[settings] ESC -> exit\n");
            break;
        }
        sleep_ms(16);
    }

    print("[settings] exiting\n");
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
        write_fd(2, b"PANIC: settings\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
