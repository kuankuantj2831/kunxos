// KunxOS launcher：桌面启动器（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init 经 spawn_service("/bin/launcher") 拉起。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 绘制桌面：深蓝背景（#1a1a2e）+ 状态栏（时间/WiFi/电量）
//      + 应用网格（3 列）+ 底部 Dock（4 个常用应用）
//   3. 轮询 /dev/input/event0（触摸事件）；设备未就绪时回退为固定坐标模拟点击
//   4. 点击应用图标 -> fork(SIGCHLD) + execve 拉起应用；父进程 wait4 阻塞等待
//      应用退出，随后置 NEEDS_REDRAW 重绘桌面（回到桌面）
//
// 说明：内核已实现用户态 fork（clone(SIGCHLD)）/ execve / wait4，因此点击
// 图标会真正拉起 /bin/calculator、/bin/settings 等独立进程；应用自己 open/mmap
// /dev/fb0 绘制全屏窗口，退出后由 launcher 重绘桌面。framebuffer 由内核在启动时
// 初始化（gpu_simple::fb_init），launcher 与各应用直接 open/mmap /dev/fb0，
// 无需 displayd 合成器与 FLUSH 命令。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_launcher.rs"]
mod cjk_font;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_MMAP: usize = 222;
const SYS_WAIT4: usize = 260;

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C; // -100
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NONBLOCK: usize = 0x800;

/// mmap 保护位与标志。
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

/// clone 标志：SIGCHLD（标准 fork 语义）。
const SIGCHLD: usize = 17;

/// 帧缓冲设备与输入设备路径。
const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";

/// 路径缓冲上限。
const PATH_CAP: usize = 96;

// ==================== 显示参数 ====================

/// 逻辑分辨率（与 displayd / ramfb 默认一致）。
const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;

/// 状态栏高度（顶部）。
const STATUS_H: i32 = 24;

/// 应用网格：3 列 x 4 行。
const GRID_COLS: i32 = 3;
const GRID_ROWS: i32 = 4;
const APPS_PER_PAGE: usize = (GRID_COLS * GRID_ROWS) as usize;
const CELL_W: i32 = 140;
const CELL_H: i32 = 128;
const GRID_X: i32 = (DISPLAY_W as i32 - GRID_COLS * CELL_W) / 2;
const GRID_Y: i32 = 64;
const ICON_SIZE: i32 = 32;

/// 底部 Dock。
const DOCK_H: i32 = 48;
const DOCK_Y: i32 = DISPLAY_H as i32 - DOCK_H;
const DOCK_ICON: i32 = 32;
const DOCK_GAP: i32 = 24;

/// 模拟点击延迟（微秒）：输入设备未就绪时的固定坐标模拟点击。
const SIM_CLICK_US: u64 = 3_000_000;

// ==================== 颜色（0x00RRGGBB） ====================

const COLOR_BG: u32 = 0x00_1A_1A_2E;
const COLOR_STATUS_BG: u32 = 0x00_0F_17_2A;
const COLOR_TEXT: u32 = 0x00_FF_FF_FF;
const COLOR_ICON_BORDER: u32 = 0x00_FF_FF_FF;
const COLOR_PANEL: u32 = 0x00_18_27_3D;
const COLOR_PANEL_ROW: u32 = 0x00_23_36_50;
const COLOR_ACCENT: u32 = 0x00_22_C5_5E;
const COLOR_WARN: u32 = 0x00_E7_4C_3C;

const THEME_PATH: &str = "/data/oobe/theme";
const LANG_PATH: &str = "/data/oobe/lang";
const WIFI_PATH: &str = "/data/oobe/wifi_enabled";
const BRIGHTNESS_PATH: &str = "/data/oobe/brightness";

// ==================== 应用表 ====================

struct App {
    key: &'static str,
    name: &'static str,
    name_zh: &'static str,
    path: &'static str,
    color: u32,
    default_on: bool,
}

impl App {
    /// 按当前语言返回显示名（默认中文，`/data/oobe/lang` 含 "zh" 或为空时用中文）。
    fn display_name(&self) -> &'static str {
        if unsafe { LANG_ZH } {
            self.name_zh
        } else {
            self.name
        }
    }
}

static APPS: [App; 17] = [
    App { key: "phone",      name: "Phone",      name_zh: "电话",      path: "/bin/phone",      color: 0x00_2E_86_C1, default_on: true },
    App { key: "sms",        name: "SMS",        name_zh: "短信",      path: "/bin/sms",        color: 0x00_1A_BB_5E, default_on: true },
    App { key: "contacts",   name: "Contacts",   name_zh: "通讯录",    path: "/bin/contacts",   color: 0x00_9B_59_B6, default_on: true },
    App { key: "settings",   name: "Settings",   name_zh: "设置",      path: "/bin/settings",   color: 0x00_22_C5_5E, default_on: true },
    App { key: "files",      name: "Files",      name_zh: "文件",      path: "/bin/files",      color: 0x00_F1_C4_0F, default_on: true },
    App { key: "browser",    name: "Browser",    name_zh: "浏览器",    path: "/bin/browser",    color: 0x00_F9_73_16, default_on: true },
    App { key: "calc",       name: "Calculator", name_zh: "计算器",    path: "/bin/calculator", color: 0x00_EA_B3_08, default_on: true },
    App { key: "calendar",   name: "Calendar",   name_zh: "日历",      path: "/bin/calendar",   color: 0x00_E7_4C_3C, default_on: false },
    App { key: "camera",     name: "Camera",     name_zh: "相机",      path: "/bin/camera",     color: 0x00_34_49_5E, default_on: false },
    App { key: "clock",      name: "Clock",      name_zh: "时钟",      path: "/bin/clock",      color: 0x00_5D_6D_7E, default_on: true },
    App { key: "gallery",    name: "Gallery",    name_zh: "图库",      path: "/bin/gallery",    color: 0x00_AF_7A_C5, default_on: false },
    App { key: "music",      name: "Music",      name_zh: "音乐",      path: "/bin/music",      color: 0x00_E6_7E_22, default_on: false },
    App { key: "mail",       name: "Mail",       name_zh: "邮件",      path: "/bin/mail",       color: 0x00_29_80_B9, default_on: false },
    App { key: "map",        name: "Maps",       name_zh: "地图",      path: "/bin/map",        color: 0x00_27_AE_60, default_on: false },
    App { key: "weather",    name: "Weather",    name_zh: "天气",      path: "/bin/weather",    color: 0x00_5D_6D_7E, default_on: false },
    App { key: "store",      name: "Store",      name_zh: "应用商店",  path: "/bin/store",      color: 0x00_7D_3C_98, default_on: true },
    App { key: "terminal",   name: "Terminal",   name_zh: "终端",      path: "/bin/terminal",   color: 0x00_22_22_22, default_on: false },
];

/// 过滤后的可见应用（桌面只显示 modules 中的、且不在 modules_removed 中的应用）。
static mut VISIBLE: [usize; 17] = [0; 17];
static mut VISIBLE_COUNT: usize = 0;

const MODULES_PATH: &str = "/data/oobe/modules";
const REMOVED_PATH: &str = "/data/oobe/modules_removed";

// ==================== 8x8 位图字体 ====================

static FONT_DIGITS: [[u8; 8]; 10] = [
    // 0
    [0x3C, 0x42, 0x46, 0x4A, 0x52, 0x62, 0x3C, 0x00],
    // 1
    [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // 2
    [0x3C, 0x42, 0x02, 0x0C, 0x30, 0x40, 0x7E, 0x00],
    // 3
    [0x3C, 0x42, 0x02, 0x1C, 0x02, 0x42, 0x3C, 0x00],
    // 4
    [0x0C, 0x14, 0x24, 0x44, 0x7E, 0x04, 0x04, 0x00],
    // 5
    [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0x00],
    // 6
    [0x3C, 0x40, 0x40, 0x7C, 0x42, 0x42, 0x3C, 0x00],
    // 7
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x10, 0x10, 0x00],
    // 8
    [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0x00],
    // 9
    [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x02, 0x3C, 0x00],
];

static FONT_COLON: [u8; 8] = [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00];

static FONT_SPACE: [u8; 8] = [0x00; 8];

static FONT_UPPER: [[u8; 8]; 26] = [
    // A
    [0x18, 0x24, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x00],
    // B
    [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0x00],
    // C
    [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0x00],
    // D
    [0x78, 0x44, 0x42, 0x42, 0x42, 0x44, 0x78, 0x00],
    // E
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0x00],
    // F
    [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0x00],
    // G
    [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3C, 0x00],
    // H
    [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0x00],
    // I
    [0x3E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // J
    [0x1E, 0x04, 0x04, 0x04, 0x04, 0x44, 0x38, 0x00],
    // K
    [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0x00],
    // L
    [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0x00],
    // M
    [0x42, 0x66, 0x5A, 0x42, 0x42, 0x42, 0x42, 0x00],
    // N
    [0x42, 0x62, 0x52, 0x4A, 0x46, 0x42, 0x42, 0x00],
    // O
    [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // P
    [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0x00],
    // Q
    [0x3C, 0x42, 0x42, 0x42, 0x4A, 0x44, 0x3A, 0x00],
    // R
    [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0x00],
    // S
    [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x42, 0x3C, 0x00],
    // T
    [0x7F, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x00],
    // U
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // V
    [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    // W
    [0x42, 0x42, 0x42, 0x42, 0x5A, 0x66, 0x42, 0x00],
    // X
    [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0x00],
    // Y
    [0x41, 0x22, 0x14, 0x08, 0x08, 0x08, 0x08, 0x00],
    // Z
    [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0x00],
];

static FONT_LOWER: [[u8; 8]; 26] = [
    // a
    [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00],
    // b
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x7C, 0x00],
    // c
    [0x00, 0x00, 0x3C, 0x42, 0x40, 0x42, 0x3C, 0x00],
    // d
    [0x02, 0x02, 0x3E, 0x42, 0x42, 0x42, 0x3E, 0x00],
    // e
    [0x00, 0x00, 0x3C, 0x42, 0x7E, 0x40, 0x3C, 0x00],
    // f
    [0x0E, 0x10, 0x10, 0x3C, 0x10, 0x10, 0x10, 0x00],
    // g
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    // h
    [0x40, 0x40, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    // i
    [0x08, 0x00, 0x18, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // j
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x44, 0x38, 0x00],
    // k
    [0x40, 0x40, 0x44, 0x48, 0x70, 0x48, 0x44, 0x00],
    // l
    [0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0x00],
    // m
    [0x00, 0x00, 0x66, 0x5A, 0x5A, 0x42, 0x42, 0x00],
    // n
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x42, 0x42, 0x00],
    // o
    [0x00, 0x00, 0x3C, 0x42, 0x42, 0x42, 0x3C, 0x00],
    // p
    [0x00, 0x00, 0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40],
    // q
    [0x00, 0x00, 0x3E, 0x42, 0x42, 0x3E, 0x02, 0x02],
    // r
    [0x00, 0x00, 0x5C, 0x62, 0x40, 0x40, 0x40, 0x00],
    // s
    [0x00, 0x00, 0x3E, 0x40, 0x3C, 0x02, 0x7C, 0x00],
    // t
    [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x0E, 0x00],
    // u
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x42, 0x3E, 0x00],
    // v
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x24, 0x18, 0x00],
    // w
    [0x00, 0x00, 0x42, 0x42, 0x5A, 0x5A, 0x24, 0x00],
    // x
    [0x00, 0x00, 0x42, 0x24, 0x18, 0x24, 0x42, 0x00],
    // y
    [0x00, 0x00, 0x42, 0x42, 0x42, 0x3E, 0x02, 0x3C],
    // z
    [0x00, 0x00, 0x7E, 0x04, 0x18, 0x20, 0x7E, 0x00],
];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'0'..=b'9' => &FONT_DIGITS[(c - b'0') as usize],
        b':' => &FONT_COLON,
        b'A'..=b'Z' => &FONT_UPPER[(c - b'A') as usize],
        b'a'..=b'z' => &FONT_LOWER[(c - b'a') as usize],
        _ => &FONT_SPACE, // 未知字符渲染为空
    }
}

// ==================== 帧缓冲与状态 ====================

/// 映射到用户空间的帧缓冲指针（由 `fb_init` 经 open/mmap /dev/fb0 建立）。
static mut FB: *mut u32 = core::ptr::null_mut();
/// 实际分辨率（默认 540x960，可被 ioctl 结果覆盖）。
static mut FB_W: usize = DISPLAY_W;
static mut FB_H: usize = DISPLAY_H;
/// 每行像素数（stride / 4）。
static mut FB_STRIDE_PX: usize = DISPLAY_W;

/// 双缓冲 back buffer：所有绘制先写入这里，`flip()` 一次性拷贝到显存，
/// 避免逐像素直写显存导致的撕裂与闪烁。
static mut BACK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// 是否需要重绘（初始为 true；输入 / 窗口变化 / 时钟跳变时置位）。
static mut NEEDS_REDRAW: bool = true;
/// 上次绘制到状态栏的分钟数（用于时钟变化时触发重绘）。
static mut LAST_MINUTE: u64 = u64::MAX;

/// 桌面与系统面板状态。全部采用定长数据，保持 no_std/无堆。
static mut CURRENT_PAGE: usize = 0;
static mut THEME_DARK: bool = true;
static mut WALLPAPER: usize = 2;
static mut LANG_ZH: bool = true;
static mut WIFI_ENABLED: bool = true;
static mut BRIGHTNESS: u32 = 85;
static mut SHADE_OPEN: bool = false;
static mut EDIT_MODE: bool = false;
static mut EDIT_APP: usize = usize::MAX;
static mut SHOW_BATTERY_PERCENT: bool = false;
static mut SOUND_ENABLED: bool = true;
static mut RECENT_APP: usize = usize::MAX;

/// Toast 与触摸涟漪均由主循环按时间自动失效。
static mut TOAST: &'static [u8] = b"";
static mut TOAST_UNTIL: u64 = 0;
static mut RIPPLE_X: i32 = 0;
static mut RIPPLE_Y: i32 = 0;
static mut RIPPLE_START: u64 = 0;
static mut RIPPLE_ACTIVE: bool = false;

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

/// 当前时间（微秒，自启动）。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        let sec = u64::from_le_bytes([
            tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7],
        ]);
        let usec = u64::from_le_bytes([
            tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15],
        ]);
        sec * 1_000_000 + usec
    }
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

// ==================== 模块过滤 ====================

/// 读取整个文件到 buf，返回长度；失败返回 -1。
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

unsafe fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return false;
    }
    let n = write_fd(fd as usize, data);
    syscall1(SYS_CLOSE, fd as usize);
    n == data.len()
}

fn bytes_contains(data: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || data.len() < needle.len() {
        return false;
    }
    for i in 0..=data.len() - needle.len() {
        if data[i..i + needle.len()].iter().zip(needle).all(|(a, b)| a == b) {
            return true;
        }
    }
    false
}

fn read_number(path: &str, fallback: u32) -> u32 {
    let mut buf = [0u8; 8];
    let n = unsafe { read_file(path, &mut buf) };
    if n <= 0 {
        return fallback;
    }
    let mut value = 0u32;
    let mut seen = false;
    for &c in &buf[..n as usize] {
        if c.is_ascii_digit() {
            value = value.saturating_mul(10).saturating_add((c - b'0') as u32);
            seen = true;
        } else if seen {
            break;
        }
    }
    if seen { value } else { fallback }
}

fn save_number(path: &str, value: u32) {
    let mut buf = [0u8; 4];
    let mut pos = buf.len();
    let mut v = value;
    if v == 0 {
        pos -= 1;
        buf[pos] = b'0';
    }
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe { write_file(path, &buf[pos..]); }
}

/// 读取 OOBE 与设置页保存的外观/快捷设置。
fn load_preferences() {
    let mut buf = [0u8; 64];
    let n = unsafe { read_file(THEME_PATH, &mut buf) };
    if n > 0 {
        let data = &buf[..n as usize];
        unsafe {
            THEME_DARK = !bytes_contains(data, b"light");
            for wallpaper in 0..4 {
                let marker = [b'w', b'a', b'l', b'l', b'p', b'a', b'p', b'e', b'r', b':', b'0' + wallpaper as u8];
                if bytes_contains(data, &marker) {
                    WALLPAPER = wallpaper;
                }
            }
        }
    }
    let n = unsafe { read_file(LANG_PATH, &mut buf) };
    if n > 0 {
        unsafe { LANG_ZH = bytes_contains(&buf[..n as usize], b"zh"); }
    }
    unsafe {
        WIFI_ENABLED = read_number(WIFI_PATH, 1) != 0;
        BRIGHTNESS = read_number(BRIGHTNESS_PATH, 85).clamp(25, 100);
    }
}

fn page_count() -> usize {
    unsafe { ((VISIBLE_COUNT + APPS_PER_PAGE - 1) / APPS_PER_PAGE).max(1) }
}

fn show_toast(text: &'static [u8]) {
    unsafe {
        TOAST = text;
        TOAST_UNTIL = now_us() + 2_000_000;
        NEEDS_REDRAW = true;
    }
}

/// 判断逗号分隔串 `data` 是否包含 token `key`。
fn token_present(data: &[u8], key: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < data.len() {
        while pos < data.len() && is_sep(data[pos]) {
            pos += 1;
        }
        let start = pos;
        while pos < data.len() && data[pos] != b',' && !is_sep(data[pos]) {
            pos += 1;
        }
        let mut end = pos;
        while end > start && is_sep(data[end - 1]) {
            end -= 1;
        }
        if end - start == key.len() && data[start..end].iter().zip(key).all(|(a, b)| a == b) {
            return true;
        }
        while pos < data.len() && data[pos] != b',' {
            pos += 1;
        }
        if pos < data.len() {
            pos += 1;
        }
    }
    false
}

fn is_sep(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

/// 读取 /data/oobe/modules 与 modules_removed，构建可见应用列表。
fn filter_modules() {
    static mut MOD_BUF: [u8; 512] = [0u8; 512];
    static mut REM_BUF: [u8; 512] = [0u8; 512];
    unsafe {
        let mods = read_file(MODULES_PATH, &mut MOD_BUF);
        let rems = read_file(REMOVED_PATH, &mut REM_BUF);
        VISIBLE_COUNT = 0;
        for i in 0..APPS.len() {
            let key = APPS[i].key.as_bytes();
            let visible = if mods > 0 {
                token_present(&MOD_BUF[..mods as usize], key)
                    && !(rems > 0 && token_present(&REM_BUF[..rems as usize], key))
            } else {
                // modules 缺失：回退到内置默认模块。
                APPS[i].default_on
            };
            if visible {
                VISIBLE[VISIBLE_COUNT] = i;
                VISIBLE_COUNT += 1;
            }
        }
        print("[launcher] loaded ");
        print_dec(VISIBLE_COUNT as u32);
        print(" modules\n");
    }
}

// ==================== 帧缓冲初始化 ====================

/// 打开 /dev/fb0、查询分辨率并 mmap 到用户空间；成功返回 true。
fn fb_init() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        // 查询分辨率（24 字节：xres/yres/xres_virtual/yres_virtual/bpp/stride）
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
        FB = base as *mut u32;
        // 映射建立后即可关闭 fd（页表已指向显存物理页）。
        syscall1(SYS_CLOSE, fd as usize);
        true
    }
}

// ==================== 绘制原语 ====================

fn put_pixel(x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= DISPLAY_W as i32 || y >= DISPLAY_H as i32 {
        return;
    }
    unsafe {
        BACK_BUF[y as usize * DISPLAY_W + x as usize] = color;
    }
}

/// 把 back buffer 一次性拷贝到显存。
///
/// 当显存 stride 与逻辑宽度一致时执行单次 `copy_nonoverlapping`（最快路径）；
/// 否则退化为逐行拷贝，兼容带 padding 的 stride。
fn flip() {
    unsafe {
        if FB.is_null() {
            return;
        }
        if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
            if BRIGHTNESS >= 100 {
                core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), FB, DISPLAY_W * DISPLAY_H);
            } else {
                let level = BRIGHTNESS;
                for i in 0..DISPLAY_W * DISPLAY_H {
                    let p = BACK_BUF[i];
                    let r = ((p >> 16) & 0xFF) * level / 100;
                    let g = ((p >> 8) & 0xFF) * level / 100;
                    let b = (p & 0xFF) * level / 100;
                    *FB.add(i) = (r << 16) | (g << 8) | b;
                }
            }
        } else {
            let w = FB_W.min(DISPLAY_W);
            let h = FB_H.min(DISPLAY_H);
            let level = BRIGHTNESS;
            for y in 0..h {
                for x in 0..w {
                    let p = BACK_BUF[y * DISPLAY_W + x];
                    let r = ((p >> 16) & 0xFF) * level / 100;
                    let g = ((p >> 8) & 0xFF) * level / 100;
                    let b = (p & 0xFF) * level / 100;
                    *FB.add(y * FB_STRIDE_PX + x) = (r << 16) | (g << 8) | b;
                }
            }
        }
    }
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

fn draw_background() {
    let (top, bottom, accent) = unsafe {
        match (THEME_DARK, WALLPAPER) {
            (false, 0) => (0x00_EF_F6_FF, 0x00_BF_DB_FE, 0x00_60_A5_FA),
            (false, 1) => (0x00_F0_FD_FA, 0x00_A7_F3_D0, 0x00_14_B8_A6),
            (false, 2) => (0x00_FF_F7_ED, 0x00_FE_D7_AA, 0x00_F9_73_16),
            (false, _) => (0x00_FA_F5_FF, 0x00_E9_D5_FF, 0x00_A8_55_F7),
            (true, 0) => (0x00_0B_10_20, 0x00_1E_3A_5F, 0x00_25_63_EB),
            (true, 1) => (0x00_0B_1F_1A, 0x00_13_46_3B, 0x00_0F_76_6E),
            (true, 2) => (0x00_1A_1A_2E, 0x00_16_21_3E, 0x00_EA_58_0C),
            (true, _) => (0x00_18_10_2A, 0x00_31_1B_53, 0x00_7E_22_CE),
        }
    };
    for y in 0..DISPLAY_H {
        let t = y as u32;
        let inv = DISPLAY_H as u32 - t;
        let r = (((top >> 16) & 0xFF) * inv + ((bottom >> 16) & 0xFF) * t) / DISPLAY_H as u32;
        let g = (((top >> 8) & 0xFF) * inv + ((bottom >> 8) & 0xFF) * t) / DISPLAY_H as u32;
        let b = ((top & 0xFF) * inv + (bottom & 0xFF) * t) / DISPLAY_H as u32;
        let row = (r << 16) | (g << 8) | b;
        for x in 0..DISPLAY_W {
            let wave = if ((x / 72 + y / 96) & 1) == 0 { accent } else { row };
            let color = if x % 72 < 2 && y > 180 { blend(row, wave, 26) } else { row };
            put_pixel(x as i32, y as i32, color);
        }
    }
}

fn blend(a: u32, b: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha.min(255);
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * alpha) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * alpha) / 255;
    let bl = ((a & 0xFF) * inv + (b & 0xFF) * alpha) / 255;
    (r << 16) | (g << 8) | bl
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

/// 绘制单个 16x16 中文字形（`idx` 为 `cjk_font::cjk_index` 返回的索引）。
fn draw_cjk(x: i32, y: i32, idx: usize, color: u32) {
    let base = idx * 32;
    for row in 0..16i32 {
        let b0 = cjk_font::CJK_BITMAPS[base + row as usize * 2];
        let b1 = cjk_font::CJK_BITMAPS[base + row as usize * 2 + 1];
        let bits = ((b0 as u16) << 8) | b1 as u16;
        let _ = bits;
        for col in 0..16i32 {
            if false {
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

/// 按当前语言选择文本（默认中文）。
fn txt(en: &'static str, zh: &'static str) -> &'static [u8] {
    if unsafe { LANG_ZH } { zh.as_bytes() } else { en.as_bytes() }
}

fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn draw_wifi(x: i32, y: i32, color: u32) {
    let cx = x + 7;
    let cy = y + 11;
    // 底部圆点
    put_pixel(cx, cy, color);
    put_pixel(cx + 1, cy, color);
    put_pixel(cx, cy + 1, color);
    put_pixel(cx + 1, cy + 1, color);
    // 三个同心圆弧（上半个圆）
    let mut r = 3;
    while r <= 7 {
        let mut dx = -r;
        while dx <= r {
            let dy = isqrt(r * r - dx * dx);
            put_pixel(cx + dx, cy - dy, color);
            dx += 1;
        }
        r += 2;
    }
}

fn draw_battery(x: i32, y: i32, color: u32) {
    // 主体 18x10，正极凸起 2x4，约 70% 电量
    draw_rect(x, y, 18, 10, color);
    fill_rect(x + 18, y + 3, 2, 4, color);
    fill_rect(x + 2, y + 2, 12, 6, color);
}

fn draw_icon_at(x: i32, y: i32, size: i32, color: u32) {
    fill_rect(x, y, size, size, color);
    draw_rect(x, y, size, size, COLOR_ICON_BORDER);
}

fn draw_line(mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: u32) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put_pixel(x0, y0, color);
        if x0 == x1 && y0 == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x0 += sx; }
        if e2 <= dx { err += dx; y0 += sy; }
    }
}

/// 以简单几何符号替代纯色块，缩放时仍保持可辨识。
fn draw_app_icon(app: &App, x: i32, y: i32, size: i32) {
    draw_icon_at(x, y, size, app.color);
    let white = 0x00_FF_FF_FF;
    let q = (size / 4).max(1);
    let h = (size / 2).max(2);
    match app.key {
        "phone" => {
            draw_line(x + q, y + q, x + size - q, y + size - q, white);
            fill_rect(x + q - 2, y + q - 1, 5, 8, white);
            fill_rect(x + size - q - 2, y + size - q - 6, 5, 8, white);
        }
        "sms" | "mail" => {
            draw_rect(x + q / 2, y + q, size - q, h, white);
            draw_line(x + q / 2, y + q, x + size / 2, y + q + h / 2, white);
            draw_line(x + size - q / 2 - 1, y + q, x + size / 2, y + q + h / 2, white);
        }
        "contacts" => {
            fill_rect(x + size / 2 - 3, y + q, 6, 6, white);
            fill_rect(x + q, y + h, size - 2 * q, q, white);
        }
        "settings" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            fill_rect(x + size / 2 - 2, y + size / 2 - 2, 4, 4, app.color);
        }
        "files" => {
            fill_rect(x + q / 2, y + q, size - q, size - q - 2, white);
            fill_rect(x + q, y + q - 3, h, 4, white);
        }
        "browser" | "map" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + size / 2, y + q, x + size / 2, y + size - q, white);
            draw_line(x + q, y + size / 2, x + size - q, y + size / 2, white);
        }
        "calculator" | "calc" => {
            draw_rect(x + q, y + q / 2, size - 2 * q, size - q, white);
            fill_rect(x + q + 3, y + q + 2, size - 2 * q - 6, 3, white);
            fill_rect(x + q + 3, y + h + 3, 3, 3, white);
            fill_rect(x + size - q - 6, y + h + 3, 3, 3, white);
        }
        "calendar" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + q, y + h, x + size - q, y + h, white);
        }
        "camera" | "gallery" => {
            draw_rect(x + q / 2, y + q, size - q, h, white);
            fill_rect(x + size / 2 - 3, y + size / 2 - 3, 6, 6, white);
        }
        "clock" => {
            draw_rect(x + q, y + q, size - 2 * q, size - 2 * q, white);
            draw_line(x + size / 2, y + size / 2, x + size / 2, y + q + 2, white);
            draw_line(x + size / 2, y + size / 2, x + size - q - 2, y + size / 2, white);
        }
        "music" => {
            draw_line(x + h, y + q, x + h, y + size - q, white);
            draw_line(x + h, y + q, x + size - q, y + q - 2, white);
            fill_rect(x + q, y + size - q - 3, q, q, white);
        }
        "weather" => {
            fill_rect(x + q, y + h, size - 2 * q, q, white);
            fill_rect(x + h - 3, y + q, 7, 7, white);
        }
        "store" => {
            draw_rect(x + q, y + q, size - 2 * q, size - q - 2, white);
            draw_line(x + q, y + q, x + size / 2, y + q - 5, white);
            draw_line(x + size - q, y + q, x + size / 2, y + q - 5, white);
        }
        "terminal" => {
            draw_line(x + q, y + q, x + h, y + h, white);
            draw_line(x + h, y + h, x + q, y + size - q, white);
            draw_line(x + h, y + size - q, x + size - q, y + size - q, white);
        }
        _ => fill_rect(x + q, y + q, size - 2 * q, size - 2 * q, white),
    }
}

fn time_hhmm(buf: &mut [u8; 5]) {
    let us = now_us();
    let secs = (us / 1_000_000) as u32;
    let total_min = secs / 60;
    let h = (total_min / 60) % 24;
    let m = total_min % 60;
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
}

// ==================== 桌面渲染 ====================

fn draw_status_bar() {
    fill_rect(0, 0, DISPLAY_W as i32, STATUS_H, COLOR_STATUS_BG);
    // 左侧：时间
    let mut hhmm = [0u8; 5];
    time_hhmm(&mut hhmm);
    draw_text(8, (STATUS_H - 8) / 2, &hhmm, COLOR_TEXT);
    // 右侧：WiFi + 电量
    unsafe {
        if WIFI_ENABLED {
            draw_wifi(486, (STATUS_H - 14) / 2, COLOR_TEXT);
        } else {
            draw_line(486, 5, 500, 18, COLOR_WARN);
            draw_line(500, 5, 486, 18, COLOR_WARN);
        }
    }
    draw_battery(512, (STATUS_H - 10) / 2, COLOR_TEXT);
    if unsafe { SHOW_BATTERY_PERCENT } {
        draw_text(532, 8, b"85", COLOR_TEXT);
    }
}

fn draw_overlay() {
    unsafe {
        if !SHADE_OPEN {
            if TOAST_UNTIL > now_us() && !TOAST.is_empty() {
                let w = text_width(TOAST).min(420) + 24;
                let x = (DISPLAY_W as i32 - w) / 2;
                fill_rect(x, DISPLAY_H as i32 - 104, w, 40, COLOR_STATUS_BG);
                draw_rect(x, DISPLAY_H as i32 - 104, w, 40, COLOR_ACCENT);
                draw_text(x + 12, DISPLAY_H as i32 - 88, TOAST, COLOR_TEXT);
            }
            return;
        }
        fill_rect(0, 0, DISPLAY_W as i32, 360, COLOR_PANEL);
        draw_text(24, 28, txt("QUICK SETTINGS", "快捷设置"), COLOR_TEXT);
        fill_rect(20, 56, 240, 58, COLOR_PANEL_ROW);
        draw_text(36, 78, txt(if WIFI_ENABLED { "WI-FI ON" } else { "WI-FI OFF" }, if WIFI_ENABLED { "无线已开启" } else { "无线已关闭" }), COLOR_TEXT);
        fill_rect(280, 56, 240, 58, COLOR_PANEL_ROW);
        draw_text(296, 78, txt(if SOUND_ENABLED { "SOUND ON" } else { "SOUND OFF" }, if SOUND_ENABLED { "声音已开启" } else { "声音已关闭" }), COLOR_TEXT);
        fill_rect(20, 128, 500, 58, COLOR_PANEL_ROW);
        draw_text(36, 150, txt("BRIGHTNESS", "亮度"), COLOR_TEXT);
        fill_rect(160, 153, 300, 6, COLOR_STATUS_BG);
        fill_rect(160, 153, (300 * BRIGHTNESS / 100) as i32, 6, COLOR_ACCENT);
        draw_text(472, 150, b"%", COLOR_TEXT);
        fill_rect(20, 200, 500, 58, COLOR_PANEL_ROW);
        draw_text(36, 222, txt("NOTIFICATIONS: 1 SYSTEM", "通知：1 条系统消息"), COLOR_TEXT);
        fill_rect(20, 272, 500, 58, COLOR_PANEL_ROW);
        draw_text(36, 294, txt("RECENT APPS", "最近应用"), COLOR_TEXT);
        if RECENT_APP != usize::MAX && RECENT_APP < APPS.len() {
            draw_text(360, 294, APPS[RECENT_APP].display_name().as_bytes(), COLOR_ACCENT);
        }
    }
}

fn draw_ripple() {
    unsafe {
        if !RIPPLE_ACTIVE { return; }
        let age = now_us().saturating_sub(RIPPLE_START);
        if age > 360_000 {
            RIPPLE_ACTIVE = false;
            return;
        }
        let radius = 8 + (age as i32 * 42 / 360_000);
        let alpha = 180u32.saturating_sub(age as u32 * 180 / 360_000);
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let d = isqrt(dx * dx + dy * dy);
                if d >= radius - 2 && d <= radius {
                    let px = (RIPPLE_X + dx).clamp(0, DISPLAY_W as i32 - 1) as usize;
                    let py = (RIPPLE_Y + dy).clamp(0, DISPLAY_H as i32 - 1) as usize;
                    let idx = py * DISPLAY_W + px;
                    BACK_BUF[idx] = blend(BACK_BUF[idx], COLOR_TEXT, alpha);
                }
            }
        }
    }
}

fn draw_grid() {
    let count = unsafe { VISIBLE_COUNT };
    let start = unsafe { CURRENT_PAGE * APPS_PER_PAGE };
    let end = (start + APPS_PER_PAGE).min(count);
    for visible_i in start..end {
        let slot = visible_i - start;
        let app_idx = unsafe { VISIBLE[visible_i] };
        let app = &APPS[app_idx];
        let col = (slot as i32) % GRID_COLS;
        let row = (slot as i32) / GRID_COLS;
        let cx = GRID_X + col * CELL_W;
        let cy = GRID_Y + row * CELL_H;
        let ix = cx + (CELL_W - ICON_SIZE) / 2;
        let iy = cy + 20;
        // 按压反馈：图标按 press_scale 缩小（居中）
        let (size, ox, oy) = pressed_icon_rect(app.key);
        let wiggle = unsafe { if EDIT_MODE && ((now_us() / 120_000 + slot as u64) & 1) == 0 { -2 } else { 2 } };
        let shift = if unsafe { EDIT_MODE } { wiggle } else { 0 };
        draw_app_icon(app, ix + ox + shift, iy + oy, size);
        if unsafe { EDIT_MODE } {
            fill_rect(ix + ICON_SIZE - 7 + shift, iy - 5, 13, 13, 0x00_DC_26_26);
            draw_line(ix + ICON_SIZE - 4 + shift, iy + 1, ix + ICON_SIZE + 2 + shift, iy + 1, COLOR_TEXT);
        }
        let name = app.display_name().as_bytes();
        let tx = cx + (CELL_W - text_width(name)) / 2;
        let ty = iy + ICON_SIZE + 6;
        draw_text(tx, ty, name, COLOR_TEXT);
    }
    draw_page_dots();
}

fn draw_page_dots() {
    let pages = page_count();
    if pages <= 1 { return; }
    let gap = 16;
    let start = DISPLAY_W as i32 / 2 - ((pages as i32 - 1) * gap) / 2;
    for i in 0..pages {
        let c = if i == unsafe { CURRENT_PAGE } { COLOR_TEXT } else { 0x00_78_85_98 };
        fill_rect(start + i as i32 * gap - 3, DOCK_Y - 24, 7, 7, c);
    }
}

/// 按压反馈几何：返回 (图标边长, x 偏移, y 偏移)；无按压返回原始尺寸。
fn pressed_icon_rect(key: &'static str) -> (i32, i32, i32) {
    let ps = pressed_scale_for_app(key);
    match ps {
        Some(scale) => {
            let size = ((ICON_SIZE as u32 * scale) / SCALE_1).max(1) as i32;
            let o = (ICON_SIZE - size) / 2;
            (size, o, o)
        }
        None => (ICON_SIZE, 0, 0),
    }
}

/// 指定应用（key）是否处于按压反馈态。
fn pressed_scale_for_app(key: &'static str) -> Option<u32> {
    unsafe {
        if MOTION.press_app == usize::MAX {
            return None;
        }
        if APPS[MOTION.press_app].key != key {
            return None;
        }
        match MOTION.phase {
            Phase::PressIn | Phase::PressOut => Some(MOTION.press_scale),
            _ => None,
        }
    }
}

fn draw_dock() {
    fill_rect(0, DOCK_Y, DISPLAY_W as i32, DOCK_H, COLOR_STATUS_BG);
    fill_rect(0, DOCK_Y, DISPLAY_W as i32, 1, COLOR_TEXT);
    // Dock 保持固定为当前页前四个应用，避免翻页后点击目标与视觉不一致。
    let page_start = unsafe { CURRENT_PAGE * APPS_PER_PAGE };
    let count = unsafe { VISIBLE_COUNT.saturating_sub(page_start).min(4) };
    if count == 0 {
        return;
    }
    let total_w = count as i32 * DOCK_ICON + (count as i32 - 1) * DOCK_GAP;
    let start_x = (DISPLAY_W as i32 - total_w) / 2;
    let dy = DOCK_Y + (DOCK_H - DOCK_ICON) / 2;
    for i in 0..count {
        let app = &APPS[unsafe { VISIBLE[page_start + i] }];
        let dx = start_x + i as i32 * (DOCK_ICON + DOCK_GAP);
        let (size, ox, oy) = pressed_icon_rect(app.key);
        draw_app_icon(app, dx + ox, dy + oy, size);
    }
}

fn render_frame() {
    unsafe {
        match MOTION.phase {
            // 转场期间：从快照缩放合成，避免重绘桌面内容
            Phase::Launch | Phase::Return => {
                zoom_blit(MOTION.cur_scale, MOTION.cx, MOTION.cy, MOTION.cur_fade);
            }
            _ => {
                draw_background();
                draw_status_bar();
                draw_grid();
                draw_dock();
                draw_overlay();
                draw_ripple();
            }
        }
        // 鼠标指针：鼠标移入系统后显示，始终绘制在最上层
        if MOUSE_SEEN {
            draw_cursor();
        }
    }
}

/// 鼠标指针：先画黑色描边（8 邻域），再画白色实心，保证深浅背景下都可见。
fn draw_cursor() {
    let (x, y) = unsafe { (CURSOR_X, CURSOR_Y) };
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

/// 统计 back buffer 中非零像素数（校验渲染结果被真实写入）。
fn count_pixels() -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..(DISPLAY_W * DISPLAY_H) {
            if BACK_BUF[i] != 0 {
                n += 1;
            }
        }
    }
    n
}

// ==================== 进程原语 ====================

/// fork：经 clone(SIGCHLD) 实现；成功父进程返回子 pid，子进程返回 0，失败返回负 errno。
unsafe fn fork() -> isize {
    syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as isize
}

/// execve：替换当前进程映像；失败返回负 errno。
unsafe fn execve(path: &[u8], argv: *const usize, envp: *const usize) -> isize {
    syscall3(SYS_EXECVE, path.as_ptr() as usize, argv as usize, envp as usize) as isize
}

/// waitpid：wait4 阻塞等待指定子进程退出，返回回收的 pid；失败返回负 errno。
unsafe fn waitpid(pid: isize, status: &mut i32) -> isize {
    syscall4(SYS_WAIT4, pid as usize, status as *mut i32 as usize, 0, 0) as isize
}

/// 拉起一个应用：fork + execve；父进程阻塞等待应用退出后重绘桌面。
fn spawn_app(app_idx: usize) {
    if app_idx >= APPS.len() {
        return;
    }
    let app = &APPS[app_idx];
    print("[launcher] launch ");
    print(app.name);
    print("\n");

    unsafe {
        let pid = fork();
        if pid == 0 {
            // 子进程：execve 应用 ELF。
            let mut argv = [0usize; 2];
            let mut p = [0u8; PATH_CAP];
            let len = app.path.len().min(PATH_CAP - 1);
            p[..len].copy_from_slice(&app.path.as_bytes()[..len]);
            p[len] = 0;
            argv[0] = p.as_ptr() as usize;
            argv[1] = 0;
            execve(&p[..len + 1], argv.as_ptr(), core::ptr::null());
            // execve 失败：子进程直接退出。
            print("[launcher] execve ");
            print(app.path);
            print(" failed\n");
            exit(127);
        } else if pid > 0 {
            // 父进程：阻塞等待应用退出。
            let mut status = 0i32;
            let r = waitpid(pid, &mut status);
            print("[launcher] app '");
            print(app.name);
            print("' exited");
            if r == pid {
                print(" code=");
                print_dec(((status >> 8) & 0xFF) as u32);
            }
            print(", back to desktop\n");
            // 应用覆盖了桌面（应用自己画 fb），退出后以缩放转场回到桌面。
            start_return();
        } else {
            print("[launcher] fork failed\n");
        }
    }
}

// ==================== 输入处理 ====================

/// 命中测试：返回被点击的应用索引（先网格，后 Dock）。
fn hit_test(x: i32, y: i32) -> Option<usize> {
    let count = unsafe { VISIBLE_COUNT };
    let start = unsafe { CURRENT_PAGE * APPS_PER_PAGE };
    let end = (start + APPS_PER_PAGE).min(count);
    // 网格
    for i in start..end {
        let slot = i - start;
        let col = (slot as i32) % GRID_COLS;
        let row = (slot as i32) / GRID_COLS;
        let cx = GRID_X + col * CELL_W;
        let cy = GRID_Y + row * CELL_H;
        let ix = cx + (CELL_W - ICON_SIZE) / 2;
        let iy = cy + 20;
        if x >= ix && x < ix + ICON_SIZE && y >= iy && y < iy + ICON_SIZE {
            return Some(unsafe { VISIBLE[i] });
        }
    }
    // Dock
    let page_start = unsafe { CURRENT_PAGE * APPS_PER_PAGE };
    let dock_count = count.saturating_sub(page_start).min(4);
    let total_w = dock_count as i32 * DOCK_ICON + (dock_count as i32 - 1) * DOCK_GAP;
    let start_x = (DISPLAY_W as i32 - total_w) / 2;
    let dy = DOCK_Y + (DOCK_H - DOCK_ICON) / 2;
    for i in 0..dock_count {
        let dx = start_x + i as i32 * (DOCK_ICON + DOCK_GAP);
        if x >= dx && x < dx + DOCK_ICON && y >= dy && y < dy + DOCK_ICON {
            return Some(unsafe { VISIBLE[page_start + i] });
        }
    }
    None
}

fn remove_app(app_idx: usize) {
    if app_idx >= APPS.len() || !APPS[app_idx].default_on {
        show_toast(txt("Built-in app cannot be removed", "内置应用不可移除"));
        return;
    }
    unsafe {
        let mut buf = [0u8; 512];
        let n = read_file(REMOVED_PATH, &mut buf);
        let mut used = if n > 0 { n as usize } else { 0 };
        if used > 0 && used < buf.len() - 1 && buf[used - 1] != b',' {
            buf[used] = b',';
            used += 1;
        }
        let key = APPS[app_idx].key.as_bytes();
        if used + key.len() < buf.len() {
            buf[used..used + key.len()].copy_from_slice(key);
            used += key.len();
            write_file(REMOVED_PATH, &buf[..used]);
            filter_modules();
            CURRENT_PAGE = CURRENT_PAGE.min(page_count() - 1);
            EDIT_MODE = false;
            EDIT_APP = usize::MAX;
            show_toast(txt("App removed", "应用已移除"));
        }
    }
}

/// 轮询 /dev/input/event0 所需的输入事件常量与状态。
///
/// 触摸面板物理分辨率（与 HAL 的 QEMU 配置一致，用于物理→屏幕坐标映射）。
const TOUCH_MAX_X: i32 = 540;
const TOUCH_MAX_Y: i32 = 960;

/// 手势分类阈值。
const TAP_MAX_DIST: i32 = 10; // 点击最大位移（px）
const TAP_MAX_MS: u64 = 500; // 点击最大持续时间（ms）
const SWIPE_MIN_DIST: i32 = 30; // 滑动最小位移（px）
const LONG_PRESS_MS: u64 = 800; // 长按最小持续时间（ms）

/// linux input_event 事件类型 / 码。
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a; // 330
const BTN_LEFT: u16 = 0x110; // 272

/// 鼠标指针状态：位置（屏幕像素，相对移动累积）、是否按下、是否已移动进入系统。
static mut CURSOR_X: i32 = 270;
static mut CURSOR_Y: i32 = 480;
static mut MOUSE_DOWN: bool = false;
static mut MOUSE_SEEN: bool = false;
static mut CURSOR_DIRTY: bool = false;

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

/// 触摸手势状态机。
struct Touch {
    pressed: bool,
    press_x: i32,
    press_y: i32,
    cur_x: i32,
    cur_y: i32,
    press_us: u64,
    max_dist: i32,
}

static mut TOUCH: Touch = Touch {
    pressed: false,
    press_x: 0,
    press_y: 0,
    cur_x: 0,
    cur_y: 0,
    press_us: 0,
    max_dist: 0,
};

/// 最近一次绝对坐标（ABS_X / ABS_Y 分属两个事件）。
static mut LAST_ABS_X: i32 = 0;
static mut LAST_ABS_Y: i32 = 0;

/// 识别出的手势。
#[derive(Clone, Copy)]
enum Gesture {
    None,
    Tap { x: i32, y: i32 },
    Swipe { dx: i32, dy: i32 },
    LongPress { x: i32, y: i32 },
}

/// 触摸面板物理坐标 → 屏幕像素坐标（540x960）。
fn map_screen(dev_x: i32, dev_y: i32) -> (i32, i32) {
    let sx = (dev_x * DISPLAY_W as i32 + TOUCH_MAX_X / 2) / TOUCH_MAX_X;
    let sy = (dev_y * DISPLAY_H as i32 + TOUCH_MAX_Y / 2) / TOUCH_MAX_Y;
    (sx, sy)
}

/// 非阻塞读取：返回读到的字节数（isize，负值为 errno）。
unsafe fn read_fd(fd: usize, buf: usize, len: usize) -> isize {
    syscall3(SYS_READ, fd, buf, len) as isize
}

/// 处理一段 `input_event` 字节流。
///
/// 返回 `(手势, 指针是否移动)`：本段中至多一个完成的手势；鼠标指针移动时
/// 第二项为 true（需要重绘以更新指针位置）。
fn process_events(buf: &[u8]) -> (Option<Gesture>, bool) {
    let mut result = None;
    let mut moved = false;
    let mut i = 0usize;
    while i + 24 <= buf.len() {
        let etype = u16::from_le_bytes([buf[i + 16], buf[i + 17]]);
        let code = u16::from_le_bytes([buf[i + 18], buf[i + 19]]);
        let value = i32::from_le_bytes([buf[i + 20], buf[i + 21], buf[i + 22], buf[i + 23]]);

        match etype {
            EV_REL => {
                // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
                if code == REL_X {
                    unsafe {
                        CURSOR_X = (CURSOR_X + value).clamp(0, DISPLAY_W as i32 - 1);
                        MOUSE_SEEN = true;
                        CURSOR_DIRTY = true;
                    }
                    moved = true;
                } else if code == REL_Y {
                    unsafe {
                        CURSOR_Y = (CURSOR_Y + value).clamp(0, DISPLAY_H as i32 - 1);
                        MOUSE_SEEN = true;
                        CURSOR_DIRTY = true;
                    }
                    moved = true;
                }
            }
            EV_ABS => unsafe {
                // 单点触摸：ABS_X / ABS_Y；多点（ABS_MT_*）为骨架。
                if code == ABS_X {
                    LAST_ABS_X = value;
                } else if code == ABS_Y {
                    LAST_ABS_Y = value;
                }
                if TOUCH.pressed {
                    let (sx, sy) = map_screen(LAST_ABS_X, LAST_ABS_Y);
                    TOUCH.cur_x = sx;
                    TOUCH.cur_y = sy;
                    let dx = sx - TOUCH.press_x;
                    let dy = sy - TOUCH.press_y;
                    let d = isqrt(dx * dx + dy * dy);
                    if d > TOUCH.max_dist {
                        TOUCH.max_dist = d;
                    }
                }
            },
            EV_KEY => {
                if code == BTN_LEFT {
                    // 鼠标左键：按下记录，抬起在指针处产生一次点击（Tap 手势）。
                    if value == 1 {
                        unsafe {
                            MOUSE_DOWN = true;
                            if let Some(app) = hit_test(CURSOR_X, CURSOR_Y) {
                                start_press(app);
                            }
                        }
                    } else if value == 0 && unsafe { MOUSE_DOWN } {
                        unsafe { MOUSE_DOWN = false; }
                        result = Some(Gesture::Tap {
                            x: unsafe { CURSOR_X },
                            y: unsafe { CURSOR_Y },
                        });
                    }
                } else if code == BTN_TOUCH {
                    if value == 1 {
                        unsafe {
                            let (sx, sy) = map_screen(LAST_ABS_X, LAST_ABS_Y);
                            TOUCH.pressed = true;
                            TOUCH.press_x = sx;
                            TOUCH.press_y = sy;
                            TOUCH.cur_x = sx;
                            TOUCH.cur_y = sy;
                            TOUCH.press_us = now_us();
                            TOUCH.max_dist = 0;
                            // 按压反馈：命中图标则开始缩小动画
                            if let Some(app) = hit_test(sx, sy) {
                                start_press(app);
                            }
                        }
                        print("[input] touch down (");
                        print_dec(unsafe { TOUCH.press_x } as u32);
                        print(",");
                        print_dec(unsafe { TOUCH.press_y } as u32);
                        print(")\n");
                    } else if value == 0 {
                        result = finish_touch();
                    }
                }
            }
            // EV_SYN 为帧边界：单点触摸在 BTN_TOUCH 抬起时完成手势。
            _ => {}
        }
        i += 24;
    }
    (result, moved)
}

/// 抬起时分类手势：点击 / 滑动 / 长按。
fn finish_touch() -> Option<Gesture> {
    unsafe {
        if !TOUCH.pressed {
            return None;
        }
        TOUCH.pressed = false;
        let dur_ms = now_us().saturating_sub(TOUCH.press_us) / 1000;
        let dist = TOUCH.max_dist;
        let (x, y) = (TOUCH.cur_x, TOUCH.cur_y);
        let g = if dist <= TAP_MAX_DIST {
            if dur_ms >= LONG_PRESS_MS {
                Gesture::LongPress { x, y }
            } else if dur_ms <= TAP_MAX_MS {
                Gesture::Tap { x, y }
            } else {
                Gesture::None
            }
        } else if dist > SWIPE_MIN_DIST {
            Gesture::Swipe {
                dx: TOUCH.cur_x - TOUCH.press_x,
                dy: TOUCH.cur_y - TOUCH.press_y,
            }
        } else {
            Gesture::None
        };
        Some(g)
    }
}

/// 处理一个完成的手势：点击启动应用、滑动切页、长按提示。
/// 返回是否发生了需要重绘的界面变化（本次实现中为点击命中图标并拉起应用）。
fn handle_gesture(g: Gesture) -> bool {
    match g {
        Gesture::Tap { x, y } => {
            print("[input] touch up (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(") -> click detected\n");
            unsafe {
                RIPPLE_X = x;
                RIPPLE_Y = y;
                RIPPLE_START = now_us();
                RIPPLE_ACTIVE = true;
            }
            if unsafe { SHADE_OPEN } {
                if y >= 56 && y < 114 && x < 270 {
                    unsafe { WIFI_ENABLED = !WIFI_ENABLED; }
                    save_number(WIFI_PATH, if unsafe { WIFI_ENABLED } { 1 } else { 0 });
                    show_toast(txt(if unsafe { WIFI_ENABLED } { "Wi-Fi ON" } else { "Wi-Fi OFF" }, if unsafe { WIFI_ENABLED } { "无线已开启" } else { "无线已关闭" }));
                } else if y >= 128 && y < 186 {
                    let level = ((x - 160).clamp(0, 300) * 100 / 300) as u32;
                    unsafe { BRIGHTNESS = level; }
                    save_number(BRIGHTNESS_PATH, level);
                    show_toast(txt("Brightness updated", "亮度已更新"));
                } else if y < 56 {
                    unsafe { SHADE_OPEN = false; }
                }
                return true;
            }
            if y < STATUS_H && x > 450 {
                unsafe { SHADE_OPEN = true; }
                return true;
            }
            if y < STATUS_H && x < 120 {
                unsafe { SHOW_BATTERY_PERCENT = !SHOW_BATTERY_PERCENT; }
                show_toast(txt(if unsafe { SHOW_BATTERY_PERCENT } { "Battery percentage ON" } else { "Battery percentage OFF" }, if unsafe { SHOW_BATTERY_PERCENT } { "电量百分比已开启" } else { "电量百分比已关闭" }));
                return true;
            }
            if unsafe { EDIT_MODE } {
                if let Some(i) = hit_test(x, y) {
                    remove_app(i);
                } else {
                    unsafe { EDIT_MODE = false; EDIT_APP = usize::MAX; }
                }
                return true;
            }
            match hit_test(x, y) {
                Some(i) => {
                    // 点击启动：先播放缩放转场，完成后再拉起应用
                    start_launch(i);
                    true
                }
                None => {
                    cancel_press();
                    false
                }
            }
        }
        Gesture::Swipe { dx, dy } => {
            print("[input] touch up -> swipe detected (");
            print_dec(dx as u32);
            print(",");
            print_dec(dy as u32);
            print(")\n");
            if unsafe { SHADE_OPEN } {
                if dy > SWIPE_MIN_DIST {
                    unsafe { SHADE_OPEN = false; }
                }
            } else if dy.abs() > dx.abs() && dy < -SWIPE_MIN_DIST {
                unsafe { SHADE_OPEN = true; }
            } else if dx.abs() > SWIPE_MIN_DIST {
                unsafe {
                    if dx < 0 {
                        CURRENT_PAGE = (CURRENT_PAGE + 1).min(page_count() - 1);
                    } else {
                        CURRENT_PAGE = CURRENT_PAGE.saturating_sub(1);
                    }
                }
                show_toast(txt("Desktop page", "桌面页"));
            }
            cancel_press();
            true
        }
        Gesture::LongPress { x, y } => {
            print("[input] long press (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
            unsafe {
                EDIT_MODE = true;
                EDIT_APP = hit_test(x, y).unwrap_or(usize::MAX);
            }
            show_toast(txt("Edit mode: tap icon to remove", "编辑模式：点击图标移除"));
            cancel_press();
            true
        }
        Gesture::None => false,
    }
}

/// 排空 /dev/input/event0 的非阻塞事件流，逐段解析并处理手势。
/// 返回本批事件中是否产生了需要重绘的变化。
fn handle_input(input_fd: isize) -> bool {
    let mut redraw = false;
    loop {
        let mut buf = [0u8; 48];
        let n = unsafe { read_fd(input_fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
        if n <= 0 {
            break; // EAGAIN（无事件）或读取错误
        }
        let (g, moved) = process_events(&buf[..n as usize]);
        if moved {
            redraw = true;
        }
        if let Some(g) = g {
            if handle_gesture(g) {
                redraw = true;
            }
        }
    }
    redraw
}

/// Settings 网格图标中心（供固定坐标模拟点击）。
fn settings_icon_center() -> (i32, i32) {
    let mut gi = 0usize;
    unsafe {
        for i in 0..VISIBLE_COUNT {
            if APPS[VISIBLE[i]].key == "settings" {
                gi = i;
                break;
            }
        }
    }
    let col = (gi as i32) % GRID_COLS;
    let row = (gi as i32) / GRID_COLS;
    let cx = GRID_X + col * CELL_W;
    let cy = GRID_Y + row * CELL_H;
    let ix = cx + (CELL_W - ICON_SIZE) / 2;
    let iy = cy + 20;
    (ix + ICON_SIZE / 2, iy + ICON_SIZE / 2)
}

// ==================== 运动 / 动画（对标 iOS + Android） ====================
//
// 与内核 kunxcore/src/anim.rs 遵循同一运动规范（数值见 docs/animations.md）：
//   - 缓动曲线：iOS Core Animation（EaseIn/EaseOut/EaseInEaseOut）
//     + Android Material（FastOutSlowIn/LinearOutSlowIn/FastOutLinearIn/Emphasized）
//   - 弹簧：阻尼谐振子（半隐式欧拉积分，无 exp 依赖）
//   - 交互：图标按压反馈（0.85 缩放）+ 应用启动缩放转场（iOS app-open 风格）
//     + 返回缩放转场（iOS app-dismiss 风格）

/// 定点缩放：1.0 = 256。
const SCALE_1: u32 = 256;
/// 按压图标缩放目标：0.85。
const PRESS_SCALE: u32 = 218;
/// 启动转场缩放目标：1.12。
const LAUNCH_SCALE: u32 = 287;
/// 各阶段时长（毫秒，与 iOS/Android 常用转场时长对齐）。
const PRESS_MS: u64 = 120;
const LAUNCH_MS: u64 = 300;
const RETURN_MS: u64 = 280;

/// 缓动曲线（与内核 anim.rs 同规范）。
#[derive(Clone, Copy, PartialEq)]
enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    EaseInEaseOut,
    FastOutSlowIn,
    LinearOutSlowIn,
    FastOutLinearIn,
    Emphasized,
}

impl Easing {
    fn cp(self) -> (f32, f32, f32, f32) {
        match self {
            Easing::Linear => (0.0, 0.0, 1.0, 1.0),
            Easing::EaseIn => (0.42, 0.0, 1.0, 1.0),
            Easing::EaseOut => (0.0, 0.0, 0.58, 1.0),
            Easing::EaseInEaseOut => (0.42, 0.0, 0.58, 1.0),
            Easing::FastOutSlowIn => (0.4, 0.0, 0.2, 1.0),
            Easing::LinearOutSlowIn => (0.0, 0.0, 0.2, 1.0),
            Easing::FastOutLinearIn => (0.4, 0.0, 1.0, 1.0),
            Easing::Emphasized => (0.2, 0.0, 0.0, 1.0),
        }
    }
    fn sample(self, t: f32) -> f32 {
        let (x1, y1, x2, y2) = self.cp();
        solve_bezier(x1, y1, x2, y2, t)
    }
}

#[inline]
fn bezier_sample(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    let mt2 = mt * mt;
    let mt3 = mt2 * mt;
    let t2 = t * t;
    let t3 = t2 * t;
    a * mt3 + 3.0 * b * mt2 * t + 3.0 * c * mt * t2 + d * t3
}

#[inline]
fn bezier_deriv(a: f32, b: f32, c: f32, d: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    3.0 * mt * mt * (b - a) + 6.0 * mt * t * (c - b) + 3.0 * t * t * (d - c)
}

/// 求贝塞尔 x 曲线在给定 x 处的参数 t（牛顿 + 二分）。
fn solve_x(x1: f32, x2: f32, x: f32) -> f32 {
    let mut t = x;
    for _ in 0..8 {
        let err = bezier_sample(0.0, x1, x2, 1.0, t) - x;
        if err.abs() < 1e-4 {
            break;
        }
        let d = bezier_deriv(0.0, x1, x2, 1.0, t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= err / d;
    }
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..12 {
        let m = (lo + hi) * 0.5;
        if bezier_sample(0.0, x1, x2, 1.0, m) < x {
            lo = m;
        } else {
            hi = m;
        }
    }
    ((lo + hi) * 0.5).clamp(0.0, 1.0)
}

fn solve_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    bezier_sample(0.0, y1, y2, 1.0, solve_x(x1, x2, x))
}

fn fsqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut r = x * 0.5;
    for _ in 0..8 {
        r = 0.5 * (r + x / r);
    }
    r
}

/// 阻尼谐振子弹簧（iOS 风格：dampingRatio + initialVelocity）。
struct Spring {
    stiffness: f32,
    damping: f32,
    mass: f32,
    target: f32,
    pos: f32,
    vel: f32,
}

impl Spring {
    const fn fixed() -> Spring {
        Spring {
            stiffness: 0.0,
            damping: 0.0,
            mass: 1.0,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    fn ios(damping_ratio: f32, initial_velocity: f32) -> Spring {
        let stiffness = 280.0;
        let mass = 1.0;
        let crit = 2.0 * fsqrt(stiffness * mass);
        Spring {
            stiffness,
            mass,
            damping: crit * damping_ratio.clamp(0.05, 2.0),
            target: 0.0,
            pos: 0.0,
            vel: initial_velocity,
        }
    }
    /// 推进 `ms` 毫秒，返回是否已稳定在目标附近。
    fn step_ms(&mut self, ms: f32) -> bool {
        let mut t = ms / 1000.0;
        while t > 0.0 {
            let h = if t > 0.001 { 0.001 } else { t };
            let a = (-self.stiffness * (self.pos - self.target) - self.damping * self.vel) / self.mass;
            self.vel += a * h;
            self.pos += self.vel * h;
            t -= h;
        }
        (self.pos - self.target).abs() < 0.5 && self.vel.abs() < 1.0
    }
}

/// 运动阶段。
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// 按压反馈（图标缩小）。
    PressIn,
    /// 松开回弹（弹簧）。
    PressOut,
    /// 应用启动转场（缩放 + 白闪）。
    Launch,
    /// 应用返回转场（缩放回桌面）。
    Return,
}

/// 运动状态机。
struct Motion {
    phase: Phase,
    app: usize,
    cx: i32,
    cy: i32,
    t0: u64,
    press_scale: u32,
    press_app: usize,
    press_spring: Spring,
    cur_scale: u32,
    cur_fade: u32,
}

static mut MOTION: Motion = Motion {
    phase: Phase::Idle,
    app: 0,
    cx: 0,
    cy: 0,
    t0: 0,
    press_scale: SCALE_1,
    press_app: usize::MAX,
    press_spring: Spring::fixed(),
    cur_scale: SCALE_1,
    cur_fade: 0,
};

/// 桌面快照（转场期间作为缩放源，避免转场中重绘桌面产生闪烁）。
static mut FRAME_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 向白色混合 `f`（0..255）。
fn blend_white(c: u32, f: u32) -> u32 {
    if f == 0 {
        return c;
    }
    if f >= 255 {
        return 0x00_FF_FF_FF;
    }
    let inv = 255 - f;
    let sr = (c >> 16) & 0xFF;
    let sg = (c >> 8) & 0xFF;
    let sb = c & 0xFF;
    let r = (255 * f + sr * inv) / 255;
    let g = (255 * f + sg * inv) / 255;
    let b = (255 * f + sb * inv) / 255;
    (r << 16) | (g << 8) | b
}

/// 以 `(cx, cy)` 为锚点把 `FRAME_BUF` 缩放（定点 `scale`，1.0=256）绘制到 BACK_BUF，
/// 可选白色覆盖 `fade`（0..255）。最近邻采样。
fn zoom_blit(scale: u32, cx: i32, cy: i32, fade: u32) {
    let s = scale.max(1) as i64;
    let inv = (256i64 << 16) / s;
    for py in 0..DISPLAY_H as i32 {
        for px in 0..DISPLAY_W as i32 {
            let sx = cx + (((px - cx) as i64 * inv) >> 16) as i32;
            let sy = cy + (((py - cy) as i64 * inv) >> 16) as i32;
            let c = if sx >= 0 && sy >= 0 && sx < DISPLAY_W as i32 && sy < DISPLAY_H as i32 {
                unsafe { FRAME_BUF[sy as usize * DISPLAY_W + sx as usize] }
            } else {
                COLOR_BG
            };
            let c = if fade > 0 { blend_white(c, fade) } else { c };
            unsafe { BACK_BUF[py as usize * DISPLAY_W + px as usize] = c; }
        }
    }
}

/// 应用图标中心（网格优先，其次 Dock）。
fn icon_center(app_idx: usize) -> (i32, i32) {
    unsafe {
        let count = VISIBLE_COUNT;
        for i in 0..count {
            if VISIBLE[i] == app_idx {
                let col = (i as i32) % GRID_COLS;
                let row = (i as i32) / GRID_COLS;
                let cx = GRID_X + col * CELL_W;
                let cy = GRID_Y + row * CELL_H;
                let ix = cx + (CELL_W - ICON_SIZE) / 2;
                let iy = cy + 20;
                return (ix + ICON_SIZE / 2, iy + ICON_SIZE / 2);
            }
        }
        let dock_count = count.min(4);
        for i in 0..dock_count {
            if VISIBLE[i] == app_idx {
                let total_w = dock_count as i32 * DOCK_ICON + (dock_count as i32 - 1) * DOCK_GAP;
                let start_x = (DISPLAY_W as i32 - total_w) / 2;
                let dy = DOCK_Y + (DOCK_H - DOCK_ICON) / 2;
                let dx = start_x + i as i32 * (DOCK_ICON + DOCK_GAP);
                return (dx + DOCK_ICON / 2, dy + DOCK_ICON / 2);
            }
        }
    }
    (DISPLAY_W as i32 / 2, DISPLAY_H as i32 / 2)
}

/// 触摸按下命中图标 → 开始按压反馈。
fn start_press(app_idx: usize) {
    unsafe {
        if MOTION.phase != Phase::Idle {
            return;
        }
        MOTION.phase = Phase::PressIn;
        MOTION.app = app_idx;
        MOTION.press_app = app_idx;
        MOTION.t0 = now_us();
        MOTION.press_scale = SCALE_1;
        NEEDS_REDRAW = true;
    }
}

/// 取消按压（滑动/长按/点空）→ 弹簧回弹。
fn cancel_press() {
    unsafe {
        if MOTION.phase != Phase::PressIn || MOTION.press_app == usize::MAX {
            return;
        }
        MOTION.press_spring = Spring::ios(0.65, 0.0);
        MOTION.press_spring.pos = MOTION.press_scale as f32;
        MOTION.press_spring.target = SCALE_1 as f32;
        MOTION.phase = Phase::PressOut;
        NEEDS_REDRAW = true;
    }
}

/// 开始应用启动转场：快照桌面 → 以图标为中心放大 + 白闪 → 完成后拉起应用。
fn start_launch(app_idx: usize) {
    unsafe {
        if MOTION.phase != Phase::Idle && MOTION.phase != Phase::PressIn {
            return;
        }
        // 快照当前桌面（此刻仍为普通桌面渲染路径）
        render_frame();
        core::ptr::copy_nonoverlapping(
            BACK_BUF.as_ptr(),
            FRAME_BUF.as_mut_ptr(),
            DISPLAY_W * DISPLAY_H,
        );
        let (cx, cy) = icon_center(app_idx);
        MOTION.phase = Phase::Launch;
        MOTION.app = app_idx;
        MOTION.cx = cx;
        MOTION.cy = cy;
        MOTION.t0 = now_us();
        MOTION.cur_scale = SCALE_1;
        MOTION.cur_fade = 0;
        MOTION.press_app = usize::MAX;
        NEEDS_REDRAW = true;
    }
}

/// 开始返回转场：快照当前显存（应用最后一帧）→ 居中缩小 + 白闪退场。
fn start_return() {
    unsafe {
        if !FB.is_null() {
            let w = FB_W.min(DISPLAY_W);
            let h = FB_H.min(DISPLAY_H);
            for y in 0..h {
                core::ptr::copy_nonoverlapping(
                    FB.add(y * FB_STRIDE_PX),
                    FRAME_BUF.as_mut_ptr().add(y * DISPLAY_W),
                    w,
                );
            }
        }
        MOTION.phase = Phase::Return;
        MOTION.t0 = now_us();
        MOTION.cx = DISPLAY_W as i32 / 2;
        MOTION.cy = DISPLAY_H as i32 / 2;
        MOTION.cur_scale = LAUNCH_SCALE;
        MOTION.cur_fade = 255;
        NEEDS_REDRAW = true;
    }
}

/// 推进运动状态机；返回是否仍在动画中（需要持续重绘）。
fn motion_advance(now: u64) -> bool {
    unsafe {
        let m = &mut MOTION;
        match m.phase {
            Phase::Idle => false,
            Phase::PressIn => {
                let t = now.saturating_sub(m.t0) as f32 / (PRESS_MS * 1000) as f32;
                let p = if t >= 1.0 {
                    1.0
                } else {
                    Easing::FastOutSlowIn.sample(t)
                };
                m.press_scale =
                    (SCALE_1 as f32 + (PRESS_SCALE as f32 - SCALE_1 as f32) * p + 0.5) as u32;
                true // 保持按压态直到抬起
            }
            Phase::PressOut => {
                if m.press_spring.step_ms(16.0) {
                    m.phase = Phase::Idle;
                    m.press_app = usize::MAX;
                    m.press_scale = SCALE_1;
                    return false;
                }
                m.press_scale = m.press_spring.pos as u32;
                true
            }
            Phase::Launch => {
                let t = now.saturating_sub(m.t0) as f32 / (LAUNCH_MS * 1000) as f32;
                if t >= 1.0 {
                    m.phase = Phase::Idle;
                    let app = m.app;
                    // 转场完成 → 真正拉起应用（阻塞至退出，随后自动进入返回转场）
                    spawn_app(app);
                    return true;
                }
                let p = Easing::EaseInEaseOut.sample(t);
                m.cur_scale =
                    (SCALE_1 as f32 + (LAUNCH_SCALE as f32 - SCALE_1 as f32) * p + 0.5) as u32;
                m.cur_fade = (255.0 * p + 0.5) as u32;
                true
            }
            Phase::Return => {
                let t = now.saturating_sub(m.t0) as f32 / (RETURN_MS * 1000) as f32;
                if t >= 1.0 {
                    m.phase = Phase::Idle;
                    NEEDS_REDRAW = true;
                    return false;
                }
                let p = Easing::EaseInEaseOut.sample(t);
                m.cur_scale =
                    (LAUNCH_SCALE as f32 + (SCALE_1 as f32 - LAUNCH_SCALE as f32) * p + 0.5) as u32;
                m.cur_fade = (255.0 * (1.0 - p) + 0.5) as u32;
                true
            }
        }
    }
}

/// 指定应用当前是否处于按压反馈态（返回其图标缩放）。
fn pressed_scale_for(app_idx: usize) -> Option<u32> {
    unsafe {
        match MOTION.phase {
            Phase::PressIn | Phase::PressOut if MOTION.press_app == app_idx => {
                Some(MOTION.press_scale)
            }
            _ => None,
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[launcher] KunxOS desktop launcher starting\n");

    // 读取 /data/oobe/modules 与 modules_removed，构建可见应用列表。
    filter_modules();
    load_preferences();

    // 打开 /dev/fb0 并 mmap 到用户空间，直接绘制像素。
    if !fb_init() {
        print("[launcher] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[launcher] framebuffer ");
    print_dec(unsafe { FB_W } as u32);
    print("x");
    print_dec(unsafe { FB_H } as u32);
    print(" mapped\n");

    // 首次渲染：绘制到 back buffer 后一次性翻转，避免首帧闪烁。
    unsafe { NEEDS_REDRAW = true; }
    render_frame();
    flip();
    unsafe {
        NEEDS_REDRAW = false;
        LAST_MINUTE = now_us() / 60_000_000;
    }
    print("[launcher] desktop ready, ");
    print_dec(unsafe { VISIBLE_COUNT } as u32);
    print(" apps\n");
    let lit = count_pixels();
    print("[launcher] rendered ");
    print_dec(lit as u32);
    print(" px\n");

    let start = now_us();
    let mut sim_clicked = false;

    // 打开真实输入设备（非阻塞）；失败则回退为固定坐标模拟点击。
    let input_fd = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    if input_fd < 0 {
        print("[input] /dev/input/event0 not ready, fallback to simulated taps\n");
    } else {
        print("[input] /dev/input/event0 ready\n");
    }

    loop {
        let now = now_us();

        // 1. 状态栏时钟：分钟变化时触发一次重绘（其余时刻保持空闲，避免闪烁）。
        let minute = now / 60_000_000;
        if unsafe { minute != LAST_MINUTE } {
            unsafe {
                LAST_MINUTE = minute;
                NEEDS_REDRAW = true;
            }
        }

        // 2. 输入处理：真实输入或固定坐标模拟点击。
        if input_fd >= 0 {
            if handle_input(input_fd) {
                unsafe { NEEDS_REDRAW = true; }
            }
        } else if !sim_clicked && now.saturating_sub(start) >= SIM_CLICK_US {
            sim_clicked = true;
            let (sx, sy) = settings_icon_center();
            print("[launcher] simulated tap at (");
            print_dec(sx as u32);
            print(",");
            print_dec(sy as u32);
            print(")\n");
            if let Some(i) = hit_test(sx, sy) {
                start_launch(i);
            }
        }

        // 2.5 运动推进：活动动画期间每帧重绘（按压反馈 / 启动转场 / 返回转场）。
        if motion_advance(now) || unsafe { RIPPLE_ACTIVE || TOAST_UNTIL > now } {
            unsafe { NEEDS_REDRAW = true; }
        }

        // 3. 仅在需要时重绘：绘制 back buffer + 一次翻转。
        if unsafe { NEEDS_REDRAW } {
            render_frame();
            flip();
            unsafe { NEEDS_REDRAW = false; }
        }

        // 4. 60fps 空闲等待。
        sleep_ms(16);
    }
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
        write_fd(2, b"PANIC: launcher\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
