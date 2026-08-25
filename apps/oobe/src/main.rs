// KunxOS OOBE：首次开机向导（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init_main 在 displayd 之后加载（/bin/oobe），仅在 /data/oobe/.done
// 不存在时运行。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，在本地 back buffer 上绘制
//      10 步向导（RGBA8888），`flip()` 一次性拷贝到显存：
//      欢迎 -> 语言 -> 模块 -> 地区/时区 -> 网络 -> 协议 -> 账号 -> 安全 -> 主题 -> 完成
//   2. 每步点击「下一步」时把该步配置写入 /data/oobe/（openat/write/close，
//      mkdir /data 与 /data/oobe）
//   3. 完成页点击「进入系统」写 .done 后 exit(0)
//
// 输入为 /dev/input/event0 真实触摸事件：每步点击「下一步」才推进，不自动跳过；
// 设备不可用（如 run-gui 无串口触摸）时回退为固定坐标模拟点击自动推进。
// 密码用 FNV-1a 简单哈希，安全哈希后续升级。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_oobe.rs"]
mod cjk_font;

// ==================== 系统调用号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MKDIRAT: usize = 34;
const SYS_IOCTL: usize = 29;
const SYS_MMAP: usize = 222;

// ==================== 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_NONBLOCK: usize = 0x800;
const SEEK_END: usize = 2;

/// mmap 保护位与标志。
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIOGET_VSCREENINFO 请求码（与内核 devfs 约定一致）。
const FBIOGET_VSCREENINFO: usize = 0x4600;

/// 帧缓冲设备路径。
const FB_PATH: &str = "/dev/fb0";

const NET_PIPE_PATH: &str = "/var/run/networkd.pipe";
const INPUT_PATH: &str = "/dev/input/event0";

// ==================== 显示参数 ====================

const DISPLAY_W: usize = 540;
const DISPLAY_H: usize = 960;
const TITLE_H: i32 = 44;
const NAV_H: i32 = 76;
const NAV_Y: i32 = DISPLAY_H as i32 - NAV_H;
const CONTENT_Y: i32 = TITLE_H + 8;

// 醒目的「下一步」按钮：底部大色块 + 高亮边框。
const NEXT_BTN_X: i32 = 400;
const NEXT_BTN_Y: i32 = 900;
const NEXT_BTN_W: i32 = 130;
const NEXT_BTN_H: i32 = 50;

// ==================== 账号步骤输入框布局 ====================

/// 三个输入框的 y 坐标（render_account / tap_account 共用，必须一致）。
const FIELD_Y0: i32 = 118;
const FIELD_Y1: i32 = 214;
const FIELD_Y2: i32 = 310;
const FIELD_X: i32 = 40;
const FIELD_W: i32 = 460;
const FIELD_H: i32 = 44;
/// 云服务 / 家长控制复选框的 y 坐标。
const CLOUD_Y: i32 = 384;
const PARENTAL_Y: i32 = 434;

// ==================== 颜色（0x00RRGGBB） ====================

// 浅色向导：纯白背景 + 黑色字体（标题加粗，内容不加粗）。
const C_TEXT: u32 = 0x00_00_00_00; // 黑色正文
const C_DIM: u32 = 0x00_00_00_00; // 黑色次级文字
const C_ACCENT: u32 = 0x00_35_B0_E6;
const C_RED: u32 = 0x00_FF_5A_5A;
const C_GREEN: u32 = 0x00_22_C5_5E;
const C_BTN: u32 = 0x00_1B_2B_4A; // 返回箭头按钮底色
const C_BTN_BORDER: u32 = 0x00_FF_FF_FF;
const C_WHITE: u32 = 0x00_FF_FF_FF;

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

static FONT_COLON: [u8; 8] = [0x00, 0x18, 0x18, 0x00, 0x00, 0x18, 0x18, 0x00];
static FONT_SPACE: [u8; 8] = [0x00; 8];
static FONT_DOT: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x00];
static FONT_DASH: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x7E, 0x00, 0x00, 0x00];
static FONT_SLASH: [u8; 8] = [0x02, 0x02, 0x04, 0x08, 0x10, 0x20, 0x20, 0x00];
static FONT_UNDER: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7E];
static FONT_STAR: [u8; 8] = [0x00, 0x14, 0x08, 0x3E, 0x08, 0x14, 0x00, 0x00];
static FONT_COMMA: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x18, 0x08];
static FONT_LPAREN: [u8; 8] = [0x08, 0x10, 0x20, 0x20, 0x20, 0x10, 0x08, 0x00];
static FONT_RPAREN: [u8; 8] = [0x10, 0x08, 0x04, 0x04, 0x04, 0x08, 0x10, 0x00];
static FONT_QUESTION: [u8; 8] = [0x3C, 0x42, 0x02, 0x0C, 0x10, 0x00, 0x10, 0x00];
static FONT_BANG: [u8; 8] = [0x08, 0x08, 0x08, 0x08, 0x08, 0x00, 0x08, 0x00];
static FONT_EQUALS: [u8; 8] = [0x00, 0x00, 0x7E, 0x00, 0x7E, 0x00, 0x00, 0x00];
static FONT_HASH: [u8; 8] = [0x24, 0x24, 0x7E, 0x24, 0x7E, 0x24, 0x24, 0x00];
static FONT_PERCENT: [u8; 8] = [0x62, 0x64, 0x08, 0x10, 0x26, 0x46, 0x00, 0x00];
static FONT_GT: [u8; 8] = [0x20, 0x10, 0x08, 0x04, 0x08, 0x10, 0x20, 0x00];

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
        b':' => &FONT_COLON,
        b'.' => &FONT_DOT,
        b'-' => &FONT_DASH,
        b'/' => &FONT_SLASH,
        b'_' => &FONT_UNDER,
        b'*' => &FONT_STAR,
        b',' => &FONT_COMMA,
        b'(' => &FONT_LPAREN,
        b')' => &FONT_RPAREN,
        b'?' => &FONT_QUESTION,
        b'!' => &FONT_BANG,
        b'=' => &FONT_EQUALS,
        b'#' => &FONT_HASH,
        b'%' => &FONT_PERCENT,
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
static mut STEP: usize = 0;
static mut LANG: usize = 0; // 0=zh_CN 1=en_US
static mut REGION: usize = 0; // 0=asia 1=europe 2=america 3=other
static mut TIMEZONE: usize = 0;
static mut NET_SKIP: bool = false;
static mut AGREEMENT: bool = false;
static mut CLOUD_ENABLED: bool = false;
static mut CLOUD_CONTROL: bool = false;
static mut CLOUD_FIND: bool = false;
static mut CLOUD_LOCK: bool = false;
static mut CLOUD_WIPE: bool = false;
static mut PARENTAL: bool = false;
static mut PARENTAL_FILTER: bool = false;
static mut PARENTAL_TIME: usize = 120;
static mut SEC_TYPE: usize = 0; // 0=pin 1=pattern 2=password
static mut DURESS_ACTION: usize = 0; // 0=fake 1=wipe 2=alert
static mut THEME_DARK: bool = true;
static mut WALLPAPER: usize = 2;

// ==================== 账号输入框状态 ====================

/// 账号步骤的用户名 / 密码 / 确认密码缓冲（定长，<= 16 字符）。
static mut USER_BUF: [u8; 16] = [0; 16];
static mut USER_LEN: usize = 0;
static mut PASS_BUF: [u8; 16] = [0; 16];
static mut PASS_LEN: usize = 0;
static mut CONFIRM_BUF: [u8; 16] = [0; 16];
static mut CONFIRM_LEN: usize = 0;
/// 当前焦点输入框：0=用户名 1=密码 2=确认密码 -1=无。
static mut FIELD_FOCUS: i32 = 0;

/// 向焦点输入框追加一个字符（超长忽略）。
fn append_field_char(f: usize, c: u8) {
    unsafe {
        let (p, len) = field_buf(f);
        if *len < 16 {
            *p.add(*len) = c;
            *len += 1;
        }
    }
}

/// 从焦点输入框删除末尾一个字符。
fn delete_field_char(f: usize) {
    unsafe {
        let (_, len) = field_buf(f);
        if *len > 0 {
            *len -= 1;
        }
    }
}

/// 返回输入框的（缓冲指针，长度引用）。
fn field_buf(f: usize) -> (*mut u8, &'static mut usize) {
    unsafe {
        match f {
            1 => (PASS_BUF.as_mut_ptr(), &mut PASS_LEN),
            2 => (CONFIRM_BUF.as_mut_ptr(), &mut CONFIRM_LEN),
            _ => (USER_BUF.as_mut_ptr(), &mut USER_LEN),
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

const DEFAULT_USER: &[u8] = b"user";
const DEFAULT_PASS: &[u8] = b"1234";
const DURESS_PIN: &[u8] = b"9999";

const TIMEZONES: [&str; 8] = [
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Seoul",
    "Europe/London",
    "Europe/Paris",
    "America/New_York",
    "America/Los_Angeles",
    "UTC",
];

const REGIONS: [&str; 4] = ["Asia", "Europe", "America", "Other"];

fn region_default_tz(region: usize) -> usize {
    match region {
        1 => 3, // Europe -> London
        2 => 5, // America -> New_York
        3 => 7, // Other -> UTC
        _ => 0, // Asia -> Shanghai
    }
}

// ==================== 应用模块表 ====================

struct Module {
    key: &'static str,
    name: &'static str,
    name_zh: &'static str,
    core: bool,
    locked: bool,
    default_on: bool,
}

impl Module {
    /// 按当前语言返回显示名（默认中文）。
    fn display_name(&self) -> &'static str {
        if unsafe { LANG == 0 } { self.name_zh } else { self.name }
    }
}

static MODULES: [Module; 17] = [
    Module { key: "phone",      name: "Phone",      name_zh: "电话",      core: true,  locked: true,  default_on: true },
    Module { key: "sms",        name: "SMS",        name_zh: "短信",      core: true,  locked: true,  default_on: true },
    Module { key: "contacts",   name: "Contacts",   name_zh: "通讯录",    core: true,  locked: true,  default_on: true },
    Module { key: "settings",   name: "Settings",   name_zh: "设置",      core: true,  locked: true,  default_on: true },
    Module { key: "files",      name: "Files",      name_zh: "文件",      core: true,  locked: false, default_on: true },
    Module { key: "browser",    name: "Browser",    name_zh: "浏览器",    core: false, locked: false, default_on: true },
    Module { key: "calc",       name: "Calculator", name_zh: "计算器",    core: false, locked: false, default_on: true },
    Module { key: "calendar",   name: "Calendar",   name_zh: "日历",      core: false, locked: false, default_on: true },
    Module { key: "camera",     name: "Camera",     name_zh: "相机",      core: false, locked: false, default_on: true },
    Module { key: "clock",      name: "Clock",      name_zh: "时钟",      core: false, locked: false, default_on: true },
    Module { key: "gallery",    name: "Gallery",    name_zh: "图库",      core: false, locked: false, default_on: true },
    Module { key: "music",      name: "Music",      name_zh: "音乐",      core: false, locked: false, default_on: true },
    Module { key: "mail",       name: "Mail",       name_zh: "邮件",      core: false, locked: false, default_on: true },
    Module { key: "map",        name: "Maps",       name_zh: "地图",      core: false, locked: false, default_on: true },
    Module { key: "weather",    name: "Weather",    name_zh: "天气",      core: false, locked: false, default_on: true },
    Module { key: "store",      name: "Store",      name_zh: "应用商店",  core: false, locked: false, default_on: true },
    Module { key: "terminal",   name: "Terminal",   name_zh: "终端",      core: false, locked: false, default_on: true },
];

static mut MODULE_ON: [bool; 17] = [false; 17];

fn module_on(i: usize) -> bool {
    unsafe { MODULE_ON[i] }
}

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

unsafe fn path_ptr(path: &str) -> usize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    PATH_BUF.as_ptr() as usize
}

unsafe fn open(path: &str, flags: usize) -> isize {
    syscall3(SYS_OPENAT, AT_FDCWD, path_ptr(path), flags) as isize
}

unsafe fn mkdir(path: &str) {
    syscall3(SYS_MKDIRAT, AT_FDCWD, path_ptr(path), 0o755);
}

/// 创建 /data 与 /data/oobe（幂等，忽略已存在的错误）。
fn mkdir_all() {
    unsafe {
        mkdir("/data");
        mkdir("/data/oobe");
    }
    print("[oobe] /data/oobe ready\n");
}

/// 写文件（open O_CREAT|O_TRUNC + write + close）。
fn write_file(path: &str, data: &[u8]) {
    unsafe {
        let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
        if fd >= 0 {
            write_fd(fd as usize, data);
            syscall1(SYS_CLOSE, fd as usize);
        }
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
        FB_PTR = base as *mut u32;
        // 映射建立后即可关闭 fd（页表已指向显存物理页）。
        syscall1(SYS_CLOSE, fd as usize);
        true
    }
}

/// 把 back buffer 一次性拷贝到显存。
///
/// 当显存 stride 与逻辑宽度一致时执行单次 `copy_nonoverlapping`（最快路径）；
/// 否则退化为逐行拷贝，兼容带 padding 的 stride。
fn flip() {
    unsafe {
        if FB_PTR.is_null() {
            return;
        }
        if FB_W == DISPLAY_W && FB_H == DISPLAY_H && FB_STRIDE_PX == DISPLAY_W {
            core::ptr::copy_nonoverlapping(
                BACK_BUF.as_ptr(),
                FB_PTR,
                DISPLAY_W * DISPLAY_H,
            );
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

fn fill_circle(cx: i32, cy: i32, r: i32, color: u32) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put_pixel(cx + dx, cy + dy, color);
            }
        }
    }
}

fn fill_rounded(x: i32, y: i32, w: i32, h: i32, r: i32, color: u32) {
    fill_rect(x + r, y, w - 2 * r, h, color);
    fill_rect(x, y + r, w, h - 2 * r, color);
    for dy in 0..r {
        for dx in 0..r {
            if dx * dx + dy * dy <= r * r {
                put_pixel(x + r - 1 - dx, y + r - 1 - dy, color);
                put_pixel(x + w - r + dx, y + r - 1 - dy, color);
                put_pixel(x + r - 1 - dx, y + h - r + dy, color);
                put_pixel(x + w - r + dx, y + h - r + dy, color);
            }
        }
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

fn draw_center(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

/// 粗体文本：每个字形横向重复绘制一次（右移 1 像素）模拟加粗（标题用）。
fn draw_text_bold(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = cjk_font::utf8_next(s, i);
        if cp >= 0x80 {
            if let Some(idx) = cjk_font::cjk_index(cp as u16) {
                draw_cjk(cx, y, idx, color);
                draw_cjk(cx + 1, y, idx, color);
            }
            cx += cjk_font::CJK_W;
        } else {
            draw_char(cx, y, cp as u8, color);
            draw_char(cx + 1, y, cp as u8, color);
            cx += 8;
        }
        i += n;
    }
}

/// 水平居中粗体文本（标题用）。
fn draw_center_bold(y: i32, s: &[u8], color: u32) {
    let tx = (DISPLAY_W as i32 - text_width(s)) / 2;
    draw_text_bold(tx, y, s, color);
}

/// 纯白背景。
fn draw_background() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_WHITE);
}

fn draw_checkbox(x: i32, y: i32, checked: bool, color: u32) {
    draw_rect(x, y, 12, 12, color);
    if checked {
        fill_rect(x + 3, y + 3, 6, 6, color);
    }
}

fn draw_radio(x: i32, y: i32, selected: bool, color: u32) {
    let cx = x + 6;
    let cy = y + 6;
    for dy in -6..=6 {
        for dx in -6..=6 {
            let d = dx * dx + dy * dy;
            if d <= 36 && d >= 25 {
                put_pixel(cx + dx, cy + dy, color);
            }
        }
    }
    if selected {
        fill_circle(cx, cy, 3, color);
    }
}

/// 选项胶囊：选中 = 强调色底；未选中 = 白底 + 黑色边框（浅色主题）。
fn draw_option_pill(x: i32, y: i32, w: i32, h: i32, r: i32, selected: bool) {
    if selected {
        fill_rounded(x, y, w, h, r, C_ACCENT);
    } else {
        fill_rounded(x, y, w, h, r, C_WHITE);
        draw_rect(x, y, w, h, C_TEXT);
    }
}

/// 右箭头按钮（色块拼成的圆角矩形 + 三角）。
fn draw_arrow_right(x: i32, y: i32, color: u32) {
    fill_rounded(x, y, 44, 44, 6, color);
    let c = C_WHITE;
    let cx = x + 14;
    let cy = y + 22;
    for i in 0..14 {
        fill_rect(cx + i, cy - 8 + i, 2, 16 - 2 * i, c);
    }
}

/// 左箭头按钮。
fn draw_arrow_left(x: i32, y: i32, color: u32) {
    fill_rounded(x, y, 44, 44, 6, color);
    let c = C_WHITE;
    let cx = x + 14;
    let cy = y + 22;
    for i in 0..14 {
        fill_rect(cx + i, cy - i, 2, 2 * i + 2, c);
    }
}

/// 醒目的「下一步」按钮：白色大色块 + 当前步骤高亮边框 + 居中黑色文字。
fn draw_next_button() {
    fill_rounded(NEXT_BTN_X, NEXT_BTN_Y, NEXT_BTN_W, NEXT_BTN_H, 10, C_WHITE);
    draw_rect(NEXT_BTN_X, NEXT_BTN_Y, NEXT_BTN_W, NEXT_BTN_H, C_ACCENT);
    draw_rect(NEXT_BTN_X + 1, NEXT_BTN_Y + 1, NEXT_BTN_W - 2, NEXT_BTN_H - 2, C_ACCENT);
    let s = "下一步 >".as_bytes();
    let tw = text_width(s);
    let tx = NEXT_BTN_X + (NEXT_BTN_W - tw) / 2;
    let ty = NEXT_BTN_Y + (NEXT_BTN_H - 8) / 2;
    draw_text(tx, ty, s, C_TEXT);
}

fn draw_title() {
    // 白色标题栏 + 黑色加粗标题、黑色步骤序号。
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_WHITE);
    let (name, num) = unsafe {
        match STEP {
            0 => (if LANG == 0 { "Welcome" } else { "Welcome" }, 0usize),
            1 => (if LANG == 0 { "语言" } else { "Language" }, 2),
            2 => (if LANG == 0 { "模块" } else { "Modules" }, 3),
            3 => (if LANG == 0 { "时区" } else { "Timezone" }, 4),
            4 => (if LANG == 0 { "网络" } else { "Network" }, 5),
            5 => (if LANG == 0 { "协议" } else { "Agreement" }, 6),
            6 => (if LANG == 0 { "账户" } else { "Account" }, 7),
            7 => (if LANG == 0 { "安全" } else { "Security" }, 8),
            8 => (if LANG == 0 { "主题" } else { "Theme" }, 9),
            _ => (if LANG == 0 { "完成" } else { "Complete" }, 10),
        }
    };
    draw_text_bold(16, (TITLE_H - 8) / 2, name.as_bytes(), C_TEXT);
    if num > 0 {
        let mut buf = [0u8; 12];
        let mut n = 0usize;
        for &c in (if unsafe { LANG == 0 } { "步骤 " } else { "Step " }).as_bytes() {
            buf[n] = c;
            n += 1;
        }
        let v = num;
        if v >= 10 {
            buf[n] = b'0' + (v / 10) as u8;
            n += 1;
        }
        buf[n] = b'0' + (v % 10) as u8;
        n += 1;
        for &c in b"/10" {
            buf[n] = c;
            n += 1;
        }
        let tx = DISPLAY_W as i32 - 16 - text_width(&buf[..n]);
        draw_text(tx, (TITLE_H - 8) / 2, &buf[..n], C_DIM);
    }
}

fn draw_dots() {
    let total = 10;
    let gap = 16;
    let total_w = (total - 1) * gap;
    let start = (DISPLAY_W as i32 - total_w) / 2;
    let cy = NAV_Y + NAV_H / 2;
    for i in 0..total {
        let cx = start + i * gap;
        if unsafe { i as usize == STEP } {
            fill_circle(cx, cy, 5, C_ACCENT);
        } else {
            fill_circle(cx, cy, 3, C_DIM);
        }
    }
}

fn draw_nav() {
    // 白色导航栏。
    fill_rect(0, NAV_Y, DISPLAY_W as i32, NAV_H, C_WHITE);
    fill_rect(0, NAV_Y, DISPLAY_W as i32, 1, C_TEXT);
    if unsafe { STEP > 0 } {
        draw_arrow_left(32, NAV_Y + (NAV_H - 44) / 2, C_BTN);
    }
    if unsafe { STEP < 9 } {
        draw_next_button();
    } else {
        // 完成页：进入系统按钮
        let bw = 180;
        let bx = DISPLAY_W as i32 - 32 - bw;
        fill_rounded(bx, NAV_Y + (NAV_H - 44) / 2, bw, 44, 6, C_GREEN);
        draw_rect(bx, NAV_Y + (NAV_H - 44) / 2, bw, 44, C_WHITE);
        draw_center(NAV_Y + (NAV_H - 8) / 2, (if unsafe { LANG == 0 } { "进入系统" } else { "Enter" }).as_bytes(), C_TEXT);
    }
    draw_dots();
}

// ==================== 各步骤渲染 ====================

fn render_welcome() {
    draw_center_bold(280, (if unsafe { LANG == 0 } { "Welcome to" } else { "Welcome to" }).as_bytes(), C_TEXT);
    draw_center_bold(300, b"KunxOS", C_TEXT);
    draw_center(340, (if unsafe { LANG == 0 } { "你的安全移动操作系统" } else { "Your secure mobile OS" }).as_bytes(), C_DIM);
}

fn render_language() {
    let x = 60;
    let mut y = CONTENT_Y + 24;
    draw_text(x, y, (if unsafe { LANG == 0 } { "选择语言" } else { "Select language" }).as_bytes(), C_TEXT);
    y += 32;
    for i in 0..2 {
        let label: &[u8] = (if i == 0 { "简体中文" } else { "English" }).as_bytes();
        let selected = unsafe { LANG == i };
        draw_option_pill(x, y, 420, 48, 6, selected);
        draw_radio(x + 16, y + 18, selected, C_TEXT);
        draw_text(x + 44, y + 20, label, C_TEXT);
        y += 60;
    }
}

fn render_modules() {
    let x = 40;
    let mut y = CONTENT_Y;
    draw_text(x, y, "选择你的应用".as_bytes(), C_TEXT);
    y += 28;
    draw_text(x, y, "系统核心".as_bytes(), C_DIM);
    y += 24;
    for i in 0..17 {
        if i == 5 {
            y += 8;
            draw_text(x, y, "预装应用".as_bytes(), C_DIM);
            y += 24;
        }
        let on = module_on(i);
        let c = if MODULES[i].locked { C_DIM } else { C_TEXT };
        draw_checkbox(x, y, on, c);
        draw_text(x + 20, y + 2, MODULES[i].display_name().as_bytes(), c);
        y += 22;
    }
    draw_text(x, y + 4, "可在设置中恢复已移除的应用".as_bytes(), C_DIM);
}

fn render_timezone() {
    let x = 60;
    let mut y = CONTENT_Y + 16;
    draw_text(x, y, "地区".as_bytes(), C_TEXT);
    y += 26;
    for i in 0..4 {
        let selected = unsafe { REGION == i };
        draw_option_pill(x, y, 200, 40, 6, selected);
        draw_radio(x + 12, y + 14, selected, C_TEXT);
        draw_text(x + 36, y + 16, REGIONS[i].as_bytes(), C_TEXT);
        y += 48;
    }
    let tx = 280;
    y = CONTENT_Y + 16;
    draw_text(tx, y, "时区".as_bytes(), C_TEXT);
    y += 26;
    let tz = unsafe { TIMEZONE };
    for i in 0..8 {
        let selected = tz == i;
        draw_option_pill(tx, y, 220, 32, 5, selected);
        draw_radio(tx + 10, y + 10, selected, C_TEXT);
        draw_text(tx + 30, y + 12, TIMEZONES[i].as_bytes(), C_TEXT);
        y += 36;
    }
}

fn render_network() {
    let x = 60;
    let mut y = CONTENT_Y + 24;
    draw_text(x, y, "网络状态".as_bytes(), C_TEXT);
    y += 32;
    draw_text(x, y, "已连接".as_bytes(), C_GREEN);
    y += 28;
    draw_text(x, y, b"IP: 10.0.2.15", C_TEXT);
    y += 24;
    draw_text(x, y, b"Gateway: 10.0.2.2", C_TEXT);
    y += 40;
    let skip = unsafe { NET_SKIP };
    draw_checkbox(x, y, skip, C_TEXT);
    draw_text(x + 20, y + 2, "跳过网络设置".as_bytes(), C_TEXT);
}

fn render_agreement() {
    let x = 40;
    let mut y = CONTENT_Y + 8;
    draw_text(x, y, "KunxOS 用户协议".as_bytes(), C_TEXT);
    y += 26;
    // 简化显示前 3 条 + 同意条款
    let lines: [&[u8]; 7] = [
        "1. 我们尽量少收集数据，本地数据".as_bytes(),
        "   保留在你的设备上。".as_bytes(),
        "2. 云控制（查找/锁定/擦除）需要".as_bytes(),
        "   KunxOS 服务器，服务器不稳定。".as_bytes(),
        "3. KunxOS 部分组件开源，遵循".as_bytes(),
        "   MIT 许可证。".as_bytes(),
        "4. 更多条款请参阅项目文档。".as_bytes(),
    ];
    for l in lines.iter() {
        draw_text(x, y, l, C_DIM);
        y += 18;
    }
    y += 16;
    let ok = unsafe { AGREEMENT };
    draw_checkbox(x, y, ok, C_TEXT);
    draw_text(x + 20, y + 2, "我已阅读并同意".as_bytes(), C_TEXT);
    y += 24;
    draw_text(x, y, "（可选，不勾选也可使用）".as_bytes(), C_DIM);
}

fn render_account() {
    let x = FIELD_X;
    // 标题
    draw_text(x, CONTENT_Y + 4, "本地账户".as_bytes(), C_TEXT);
    draw_text(x + 130, CONTENT_Y + 4, b"*", C_RED);
    // 用户名输入框
    draw_text(x, FIELD_Y0 - 30, "用户名".as_bytes(), C_DIM);
    let ub = unsafe { &USER_BUF[..USER_LEN] };
    let f0 = unsafe { FIELD_FOCUS == 0 };
    draw_field(x, FIELD_Y0, ub, f0, false);
    // 密码输入框（掩码显示）
    draw_text(x, FIELD_Y1 - 30, "密码（6-16 位）".as_bytes(), C_DIM);
    let pb = unsafe { &PASS_BUF[..PASS_LEN] };
    let f1 = unsafe { FIELD_FOCUS == 1 };
    draw_field(x, FIELD_Y1, pb, f1, true);
    // 确认密码输入框
    draw_text(x, FIELD_Y2 - 30, "确认密码".as_bytes(), C_DIM);
    let cb = unsafe { &CONFIRM_BUF[..CONFIRM_LEN] };
    let f2 = unsafe { FIELD_FOCUS == 2 };
    draw_field(x, FIELD_Y2, cb, f2, true);
    // 云服务复选框
    let cloud = unsafe { CLOUD_ENABLED };
    draw_checkbox(x, CLOUD_Y, cloud, C_TEXT);
    draw_text(x + 20, CLOUD_Y + 2, "启用云服务".as_bytes(), C_TEXT);
    // 家长控制复选框
    let pc = unsafe { PARENTAL };
    draw_checkbox(x, PARENTAL_Y, pc, C_TEXT);
    draw_text(x + 20, PARENTAL_Y + 2, "家长控制（本地）".as_bytes(), C_TEXT);
}

/// 绘制一个输入框：白色底 + 边框（聚焦为强调色），文本靠左；密码掩码为 `*`，
/// 聚焦时在末尾显示 `_` 光标。
fn draw_field(x: i32, y: i32, s: &[u8], focused: bool, masked: bool) {
    fill_rect(x, y, FIELD_W, FIELD_H, C_WHITE);
    draw_rect(x, y, FIELD_W, FIELD_H, if focused { C_ACCENT } else { C_TEXT });
    let mut buf = [0u8; 20];
    let mut n = 0usize;
    for &c in s {
        buf[n] = if masked { b'*' } else { c };
        n += 1;
        if n >= buf.len() {
            break;
        }
    }
    if focused && n < buf.len() {
        buf[n] = b'_';
        n += 1;
    }
    draw_text(x + 8, y + (FIELD_H - 16) / 2, &buf[..n], C_TEXT);
}

fn render_security() {
    let x = 40;
    let mut y = CONTENT_Y + 4;
    draw_text(x, y, "锁屏方式".as_bytes(), C_TEXT);
    y += 26;
    let names: [&[u8]; 3] = ["PIN（4 位数字）".as_bytes(), "图案（3x3）".as_bytes(), "密码（6-16 位）".as_bytes()];
    for i in 0..3 {
        let selected = unsafe { SEC_TYPE == i };
        draw_option_pill(x, y, 320, 40, 6, selected);
        draw_radio(x + 14, y + 14, selected, C_TEXT);
        draw_text(x + 40, y + 16, names[i], C_TEXT);
        y += 48;
    }
    y += 8;
    draw_text(x, y, "胁迫设置".as_bytes(), C_TEXT);
    y += 18;
    draw_text(x, y, "胁迫 PIN：9999".as_bytes(), C_DIM);
    y += 26;
    draw_text(x, y, "触发动作".as_bytes(), C_TEXT);
    y += 24;
    let acts: [&[u8]; 3] = ["伪装系统".as_bytes(), "静默擦除".as_bytes(), "发送警报".as_bytes()];
    for i in 0..3 {
        let selected = unsafe { DURESS_ACTION == i };
        draw_radio(x, y, selected, C_TEXT);
        draw_text(x + 24, y + 2, acts[i], C_TEXT);
        y += 24;
    }
}

fn render_theme() {
    let mut x = 60;
    let mut y = CONTENT_Y + 16;
    draw_text(x, y, "外观".as_bytes(), C_TEXT);
    y += 26;
    for i in 0..2 {
        let selected = unsafe { (i == 1) == THEME_DARK };
        let label: &[u8] = if i == 0 { "浅色".as_bytes() } else { "深色".as_bytes() };
        draw_option_pill(x, y, 200, 40, 6, selected);
        draw_radio(x + 14, y + 14, selected, C_TEXT);
        draw_text(x + 40, y + 16, label, C_TEXT);
        y += 48;
    }
    y += 8;
    draw_text(x, y, "壁纸".as_bytes(), C_TEXT);
    y += 26;
    let wp: [u32; 3] = [0x00_1E_3A_8A, 0x00_8A_2E_1E, 0x00_1E_5A_3C];
    let wnames: [&[u8]; 3] = ["星空".as_bytes(), "熔岩".as_bytes(), "森林".as_bytes()];
    for i in 0..3 {
        let selected = unsafe { WALLPAPER == i };
        fill_rect(x, y, 48, 48, wp[i]);
        if selected {
            draw_rect(x, y, 48, 48, C_BTN_BORDER);
        }
        draw_text(x + 60, y + 20, wnames[i], C_TEXT);
        x += 160;
    }
    // 预览区域
    let pv = unsafe { WALLPAPER };
    fill_rect(60, 400, 420, 200, wp[pv]);
    draw_rect(60, 400, 420, 200, C_BTN_BORDER);
    draw_center(490, "预览".as_bytes(), C_TEXT);
}

fn render_complete() {
    draw_center_bold(260, (if unsafe { LANG == 0 } { "KunxOS 已就绪" } else { "KunxOS is ready" }).as_bytes(), C_TEXT);
    let mut y = 320;
    // 摘要：用户名、已选模块数、锁屏方式
    draw_text(120, y, (if unsafe { LANG == 0 } { "用户：user" } else { "User: user" }).as_bytes(), C_TEXT);
    y += 28;
    let mut cnt = 0usize;
    for i in 0..17 {
        if module_on(i) {
            cnt += 1;
        }
    }
    let mut buf = [0u8; 40];
    let mut n = 0usize;
    for &c in (if unsafe { LANG == 0 } { "应用模块：" } else { "Modules: " }).as_bytes() {
        buf[n] = c;
        n += 1;
    }
    let mut v = cnt;
    if v == 0 {
        buf[n] = b'0';
        n += 1;
    } else {
        let mut tmp = [0u8; 8];
        let mut ti = tmp.len();
        while v > 0 {
            ti -= 1;
            tmp[ti] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        let c = tmp.len() - ti;
        buf[n..n + c].copy_from_slice(&tmp[ti..]);
        n += c;
    }
    draw_text(120, y, &buf[..n], C_TEXT);
    y += 28;
    draw_text(120, y, (if unsafe { LANG == 0 } { "锁屏：PIN" } else { "Lock: PIN" }).as_bytes(), C_TEXT);
    y += 40;
    draw_center(y, (if unsafe { LANG == 0 } { "所有配置将被保存。" } else { "All config will be saved." }).as_bytes(), C_DIM);
}

fn render() {
        draw_background();
        draw_title();
        unsafe {
        match STEP {
            0 => render_welcome(),
            1 => render_language(),
            2 => render_modules(),
            3 => render_timezone(),
            4 => render_network(),
            5 => render_agreement(),
            6 => render_account(),
            7 => render_security(),
            8 => render_theme(),
            _ => render_complete(),
        }
    }
        draw_nav();
        // 鼠标指针：鼠标移入系统后显示，始终绘制在最上层
    if unsafe { MOUSE_SEEN } {
        draw_cursor();
    }
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

// ==================== 字符串构建与保存 ====================

fn fnv1a_hex(data: &[u8], out: &mut [u8; 8]) {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    for i in 0..8 {
        let nib = (h >> ((7 - i) * 4)) & 0xF;
        out[i] = HEX[nib as usize];
    }
}

/// 构建 modules 或 modules_removed 的逗号分隔串，返回长度。
fn build_modules_str(out: &mut [u8], removed: bool) -> usize {
    let mut n = 0usize;
    for i in 0..17 {
        let on = module_on(i);
        if (removed && !on) || (!removed && on) {
            let key = MODULES[i].key.as_bytes();
            if n > 0 {
                out[n] = b',';
                n += 1;
            }
            out[n..n + key.len()].copy_from_slice(key);
            n += key.len();
        }
    }
    n
}

fn save_lang() {
    let v: &[u8] = if unsafe { LANG == 0 } { b"zh_CN" } else { b"en_US" };
    write_file("/data/oobe/lang", v);
}

fn save_modules() {
    let mut buf = [0u8; 256];
    let n = build_modules_str(&mut buf, false);
    write_file("/data/oobe/modules", &buf[..n]);
    let n2 = build_modules_str(&mut buf, true);
    write_file("/data/oobe/modules_removed", &buf[..n2]);
}

fn save_timezone() {
    let tz = unsafe { TIMEZONE };
    write_file("/data/oobe/timezone", TIMEZONES[tz].as_bytes());
}

fn save_agreement() {
    let v: &[u8] = if unsafe { AGREEMENT } { b"1" } else { b"0" };
    write_file("/data/oobe/agreement", v);
}

fn save_account() {
    // local_account = "<用户名>:<fnv1a hex>"
    let mut buf = [0u8; 64];
    let un = unsafe { USER_LEN.min(USER_BUF.len()) };
    let mut n = un;
    unsafe { buf[..n].copy_from_slice(&USER_BUF[..n]); }
    buf[n] = b':';
    n += 1;
    let mut h = [0u8; 8];
    let pl = unsafe { PASS_LEN };
    fnv1a_hex(unsafe { &PASS_BUF[..pl] }, &mut h);
    buf[n..n + 8].copy_from_slice(&h);
    n += 8;
    write_file("/data/oobe/local_account", &buf[..n]);

    let ce: &[u8] = if unsafe { CLOUD_ENABLED } { b"1" } else { b"0" };
    write_file("/data/oobe/cloud_enabled", ce);

    // cloud_account = "" 或 "username:token"
    if unsafe { CLOUD_ENABLED } {
        write_file("/data/oobe/cloud_account", b"user:token");
    } else {
        write_file("/data/oobe/cloud_account", b"");
    }

    // cloud_control = "enabled:0" 或 "enabled:1,find:1,lock:0,wipe:0"
    let mut cc = [0u8; 40];
    let mut m = 0usize;
    if unsafe { CLOUD_CONTROL } {
        for &c in b"enabled:1,find:" {
            cc[m] = c;
            m += 1;
        }
        cc[m] = if unsafe { CLOUD_FIND } { b'1' } else { b'0' };
        m += 1;
        for &c in b",lock:" {
            cc[m] = c;
            m += 1;
        }
        cc[m] = if unsafe { CLOUD_LOCK } { b'1' } else { b'0' };
        m += 1;
        for &c in b",wipe:" {
            cc[m] = c;
            m += 1;
        }
        cc[m] = if unsafe { CLOUD_WIPE } { b'1' } else { b'0' };
        m += 1;
    } else {
        for &c in b"enabled:0" {
            cc[m] = c;
            m += 1;
        }
    }
    write_file("/data/oobe/cloud_control", &cc[..m]);

    let pe: &[u8] = if unsafe { PARENTAL } { b"1" } else { b"0" };
    write_file("/data/oobe/parental_enabled", pe);

    // parental_control = "apps:,time:120,filter:0"
    let mut pc = [0u8; 48];
    let mut k = 0usize;
    for &c in b"apps:" {
        pc[k] = c;
        k += 1;
    }
    // 简化为空限制列表（默认未启用）
    for &c in b",time:" {
        pc[k] = c;
        k += 1;
    }
    let mut t = unsafe { PARENTAL_TIME };
    let mut tmp = [0u8; 8];
    let mut ti = tmp.len();
    if t == 0 {
        ti -= 1;
        tmp[ti] = b'0';
    } else {
        while t > 0 {
            ti -= 1;
            tmp[ti] = b'0' + (t % 10) as u8;
            t /= 10;
        }
    }
    let c = tmp.len() - ti;
    pc[k..k + c].copy_from_slice(&tmp[ti..]);
    k += c;
    for &ch in b",filter:" {
        pc[k] = ch;
        k += 1;
    }
    pc[k] = if unsafe { PARENTAL_FILTER } { b'1' } else { b'0' };
    k += 1;
    write_file("/data/oobe/parental_control", &pc[..k]);
}

fn save_security() {
    let ty: &[u8] = unsafe {
        match SEC_TYPE {
            1 => b"pattern",
            2 => b"password",
            _ => b"pin",
        }
    };
    write_file("/data/oobe/security_type", ty);

    let mut h = [0u8; 8];
    let pl = unsafe { PASS_LEN };
    fnv1a_hex(unsafe { &PASS_BUF[..pl] }, &mut h);
    write_file("/data/oobe/security_hash", &h);

    let mut d = [0u8; 8];
    fnv1a_hex(DURESS_PIN, &mut d);
    write_file("/data/oobe/duress_hash", &d);

    let act: &[u8] = unsafe {
        match DURESS_ACTION {
            1 => b"wipe",
            2 => b"alert",
            _ => b"fake_system",
        }
    };
    write_file("/data/oobe/duress_action", act);
}

fn save_theme() {
    // theme = "dark,wallpaper:2" / "light,wallpaper:N"
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    let mode: &[u8] = if unsafe { THEME_DARK } { b"dark" } else { b"light" };
    buf[..mode.len()].copy_from_slice(mode);
    n += mode.len();
    for &c in b",wallpaper:" {
        buf[n] = c;
        n += 1;
    }
    buf[n] = b'0' + unsafe { WALLPAPER } as u8;
    n += 1;
    write_file("/data/oobe/theme", &buf[..n]);
}

fn save_step(step: usize) {
    match step {
        1 => save_lang(),
        2 => save_modules(),
        3 => save_timezone(),
        5 => save_agreement(),
        6 => save_account(),
        7 => save_security(),
        8 => save_theme(),
        _ => {}
    }
}

// ==================== 日志 ====================

fn log_step(step: usize) {
    match step {
        0 => print("[oobe] welcome to KunxOS\n"),
        1 => print("[oobe] step 2/10: language\n"),
        2 => print("[oobe] step 3/10: modules\n"),
        3 => print("[oobe] step 4/10: timezone\n"),
        4 => print("[oobe] step 5/10: network\n"),
        5 => print("[oobe] step 6/10: agreement\n"),
        6 => print("[oobe] step 7/10: account\n"),
        7 => print("[oobe] step 8/10: security\n"),
        8 => print("[oobe] step 9/10: theme\n"),
        _ => print("[oobe] step 10/10: complete\n"),
    }
}

/// 查询网络状态（向 networkd 发送 STATUS，并打印硬编码 QEMU 默认值）。
fn query_network() {
    unsafe {
        let fd = open(NET_PIPE_PATH, O_RDWR);
        if fd >= 0 {
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
            write_fd(fd as usize, b"STATUS\n");
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
    print("[oobe] network STATUS: IP=10.0.2.15 GW=10.0.2.2\n");
}

// ==================== 输入事件 ====================

struct InputEvent {
    x: i32,
    y: i32,
    enter: bool,
    back: bool,
    /// 键盘输入的一个字符（仅 STEP=6 账号步骤使用）。
    ch: Option<u8>,
    /// 键盘退格键。
    backspace: bool,
}

// Linux input_event（64 位小端）：timeval 16B + type u16 + code u16 + value i32 = 24B
const EV_KEY: usize = 1;
const EV_REL: usize = 2;
const EV_ABS: usize = 3;
const ABS_X: usize = 0;
const ABS_Y: usize = 1;
const REL_X: usize = 0;
const REL_Y: usize = 1;
const ABS_MT_POSITION_X: usize = 0x35;
const ABS_MT_POSITION_Y: usize = 0x36;
const BTN_TOUCH: usize = 0x14a;
const BTN_LEFT: usize = 0x110;
const KEY_ENTER: usize = 28;
const KEY_BACKSPACE: usize = 14;
const KEY_LEFTSHIFT: usize = 42;
const KEY_RIGHTSHIFT: usize = 54;

/// 键盘 shift 状态（大写/符号）。
static mut SHIFT: bool = false;

/// Linux evdev 键码 → ASCII 字符（未按 shift 处理）。
///
/// evdev 键码按 **QWERTY 物理布局**编号（KEY_Q=16、KEY_W=17…，非字母表
/// 顺序），必须逐键显式映射（与终端应用 key_char 同一张表）。
fn key_to_char(code: usize, shift: bool) -> Option<u8> {
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

/// 鼠标指针状态：位置复用 CUR_X/CUR_Y（屏幕像素，相对移动累积）。
static mut MOUSE_DOWN: bool = false;
static mut MOUSE_SEEN: bool = true; // 默认显示指针，鼠标移动后跟随
/// 指针移动标记：等待循环据此重绘（OOBE 每步只渲染一次，需主动刷新光标）。
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

static mut INPUT_FD: isize = -1;
static mut CUR_X: i32 = DISPLAY_W as i32 / 2; // 屏幕中心
static mut CUR_Y: i32 = DISPLAY_H as i32 / 2;
static mut TOUCH_DOWN: bool = false;

/// 输入调试日志：[oobe] input: x=..., y=..., type=...
fn log_input(kind: &str, x: i32, y: i32) {
    print("[oobe] input: x=");
    print_dec(x as usize);
    print(", y=");
    print_dec(y as usize);
    print(", type=");
    print(kind);
    print("\n");
}

/// 输入累积缓冲：事件可能跨 read 分割（单次 read 未必返回完整 24 字节），
/// 先把零散字节累积进缓冲，凑满一个 `input_event` 后再解析，避免事件被丢弃。
static mut EV_BUF: [u8; 64] = [0u8; 64];
static mut EV_LEN: usize = 0;

/// 解析并消费缓冲头部的单个 input_event（24B）；是完整点击（按下+抬起）或
/// 回车键或返回按钮按下时返回 true，并把动作写入 ev。无论结果如何都消费该事件
/// （剩余字节前移，EV_LEN -= 24），保证连续事件流不被重复解析。
unsafe fn consume_input_event(ev: &mut InputEvent) -> bool {
    let buf = &EV_BUF[..24];
    let etype = u16::from_le_bytes([buf[16], buf[17]]) as usize;
    let code = u16::from_le_bytes([buf[18], buf[19]]) as usize;
    let value = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

    let mut click = false;
    // 每次解析前清空事件动作（read_input_event 复用同一结构体）。
    ev.enter = false;
    ev.back = false;
    ev.ch = None;
    ev.backspace = false;
    if etype == EV_REL {
        // 鼠标相对移动：累积到指针位置并钳制在屏幕内。
        match code {
            REL_X => CUR_X = (CUR_X + value).clamp(0, DISPLAY_W as i32 - 1),
            REL_Y => CUR_Y = (CUR_Y + value).clamp(0, DISPLAY_H as i32 - 1),
            _ => {}
        }
        MOUSE_SEEN = true;
        CURSOR_DIRTY = true;
    } else if etype == EV_ABS {
        match code {
            ABS_X => {
                CUR_X = value;
                CURSOR_DIRTY = true;
            }
            ABS_Y => {
                CUR_Y = value;
                log_input("move", CUR_X, CUR_Y);
                CURSOR_DIRTY = true;
            }
            ABS_MT_POSITION_X => CUR_X = value,
            ABS_MT_POSITION_Y => {
                CUR_Y = value;
                log_input("move", CUR_X, CUR_Y);
            }
            _ => {}
        }
    } else if etype == EV_KEY {
        if code == BTN_LEFT {
            // 鼠标左键：按下记录，抬起在指针处产生一次点击。
            if value == 1 {
                MOUSE_DOWN = true;
                log_input("down", CUR_X, CUR_Y);
            } else if value == 0 && MOUSE_DOWN {
                log_input("up", CUR_X, CUR_Y);
                MOUSE_DOWN = false;
                ev.x = CUR_X;
                ev.y = CUR_Y;
                ev.enter = false;
                ev.back = CUR_Y > 880 && CUR_X < 120;
                click = true;
            }
        } else if code == BTN_TOUCH {
            if value == 1 {
                TOUCH_DOWN = true;
                log_input("down", CUR_X, CUR_Y);
            } else if value == 0 {
                log_input("up", CUR_X, CUR_Y);
                if TOUCH_DOWN {
                    TOUCH_DOWN = false;
                    ev.x = CUR_X;
                    ev.y = CUR_Y;
                    ev.enter = false;
                    ev.back = CUR_Y > 880 && CUR_X < 120;
                    click = true;
                }
            }
        } else if code == KEY_ENTER {
            // 键盘回车：按下即下一步（账号步骤内为焦点推进，见 wait_for_tap）。
            log_input("key-enter", 0, 0);
            if value == 1 {
                ev.x = 0;
                ev.y = 0;
                ev.enter = true;
                click = true;
            }
        } else if code == KEY_BACKSPACE {
            if value == 1 {
                ev.backspace = true;
                click = true;
            }
        } else if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
            SHIFT = value != 0;
        } else if value == 1 {
            // 其它按键（字符键）：按下时翻译成字符。
            if let Some(c) = key_to_char(code, SHIFT) {
                ev.ch = Some(c);
                click = true;
                print("[oobe] key code=");
                print_dec(code);
                print(" ch='");
                write_fd(1, &[c]);
                print("'\n");
            }
        }
    }

    // 消费已解析的 24 字节，剩余字节前移。
    let rest = EV_LEN - 24;
    for i in 0..rest {
        EV_BUF[i] = EV_BUF[24 + i];
    }
    EV_LEN = rest;
    click
}

/// 读取一个输入事件；仅在检测到一次完整点击（按下并抬起）或回车键按下时返回 true，
/// 并把点击坐标/回车标志写入 ev。设备未就绪时返回 false。
fn read_input_event(ev: &mut InputEvent) -> bool {
        unsafe {
        if INPUT_FD < 0 {
            INPUT_FD = open(INPUT_PATH, O_RDONLY | O_NONBLOCK);
            if INPUT_FD >= 0 {
                print("[input] /dev/input/event0 ready\n");
            }
        }
        if INPUT_FD < 0 {
            return false;
        }

        loop {
            if EV_LEN >= 24 {
                // 已累积出完整事件：解析并消费；命中点击/回车立即返回。
                if consume_input_event(ev) {
                    return true;
                }
                continue;
            }
            // 缓冲不足一个事件：把新数据追加到缓冲尾部（非阻塞，无数据即返回）。
            let space = EV_BUF.len() - EV_LEN;
            if space == 0 {
                return false;
            }
            let n = syscall3(
                SYS_READ,
                INPUT_FD as usize,
                EV_BUF.as_ptr().add(EV_LEN) as usize,
                space,
            );
            if (n as isize) <= 0 {
                return false; // 无数据（非阻塞 EAGAIN）或读失败
            }
            EV_LEN += n;
        }
    }
}

/// 模块步骤第 `i` 行复选框的 y 坐标（与 render_modules 的绘制位置一致）。
fn module_row_y(i: usize) -> i32 {
    let mut y = CONTENT_Y + 28 + 24; // 104
    for j in 0..=i {
        if j == 5 {
            y += 8 + 24; // 「预装应用」分组标题
        }
        if j < i {
            y += 22;
        }
    }
    y
}

/// 处理点击：返回 1=下一步、-1=返回、2=本步选项被消费（已重绘状态）、0=未命中。
fn handle_tap(x: i32, y: i32) -> i32 {
    print("[oobe] tap at (");
    print_dec(x as usize);
    print(",");
    print_dec(y as usize);
    print("), checking buttons...\n");
    // 返回箭头区域（底部左侧）
    if y > 880 && x < 120 {
        print("[oobe] back button hit\n");
        return -1;
    }
    // 「下一步」按钮区域（底部右侧，400..530 x 900..950，含扩大命中区）
    if y > 880 && x > 380 {
        print("[oobe] next button hit -> next\n");
        return 1;
    }
    let step = unsafe { STEP };
    let r = match step {
        1 => tap_language(x, y),
        2 => tap_modules(x, y),
        3 => tap_timezone(x, y),
        4 => tap_network(x, y),
        5 => tap_agreement(x, y),
        6 => tap_account(x, y),
        7 => tap_security(x, y),
        8 => tap_theme(x, y),
        _ => 0,
    };
    if r == 2 {
        print("[oobe] option consumed\n");
    }
    r
}

/// 语言步骤：两个选项胶囊（简体中文 / English）。
fn tap_language(x: i32, y: i32) -> i32 {
    if x >= 60 && x < 480 {
        if y >= 108 && y < 156 {
            unsafe { LANG = 0; }
            return 2;
        }
        if y >= 168 && y < 216 {
            unsafe { LANG = 1; }
            return 2;
        }
    }
    0
}

/// 模块步骤：勾选/取消勾选应用（锁定项忽略）。
fn tap_modules(x: i32, y: i32) -> i32 {
    if x < 40 || x >= 520 {
        return 0;
    }
    for i in 0..17 {
        let ry = module_row_y(i);
        if y >= ry && y < ry + 22 {
            if !unsafe { MODULES[i].locked } {
                unsafe { MODULE_ON[i] = !MODULE_ON[i]; }
                return 2;
            }
            return 2; // 锁定项也消费点击（不改变状态）
        }
    }
    0
}

/// 地区/时区步骤：左列地区胶囊，右列时区胶囊。
fn tap_timezone(x: i32, y: i32) -> i32 {
    if x >= 60 && x < 260 {
        for i in 0..4 {
            let ry = CONTENT_Y + 16 + 26 + i as i32 * 48;
            if y >= ry && y < ry + 40 {
                unsafe {
                    REGION = i;
                    TIMEZONE = region_default_tz(i);
                }
                return 2;
            }
        }
    }
    if x >= 280 && x < 500 {
        for i in 0..8 {
            let ry = CONTENT_Y + 16 + 26 + i as i32 * 36;
            if y >= ry && y < ry + 32 {
                unsafe { TIMEZONE = i; }
                return 2;
            }
        }
    }
    0
}

/// 网络步骤：「跳过网络设置」复选框。
fn tap_network(x: i32, y: i32) -> i32 {
    if x >= 60 && y >= 200 && y < 222 {
        unsafe { NET_SKIP = !NET_SKIP; }
        return 2;
    }
    0
}

/// 协议步骤：「我已阅读并同意」复选框。
fn tap_agreement(x: i32, y: i32) -> i32 {
    if x >= 40 && y >= 210 && y < 222 {
        unsafe { AGREEMENT = !AGREEMENT; }
        return 2;
    }
    0
}

/// 账号步骤：输入框焦点切换 + 云服务/家长控制复选框。
fn tap_account(x: i32, y: i32) -> i32 {
    // 三个输入框（与 render_account 布局一致）
    let field_y = [FIELD_Y0, FIELD_Y1, FIELD_Y2];
    for (i, fy) in field_y.iter().enumerate() {
        if x >= 40 && x < 500 && y >= *fy && y < *fy + 44 {
            unsafe { FIELD_FOCUS = i as i32; }
            return 2;
        }
    }
    if x >= 40 && y >= CLOUD_Y && y < CLOUD_Y + 16 {
        unsafe { CLOUD_ENABLED = !CLOUD_ENABLED; }
        return 2;
    }
    if x >= 40 && y >= PARENTAL_Y && y < PARENTAL_Y + 16 {
        unsafe { PARENTAL = !PARENTAL; }
        return 2;
    }
    0
}

/// 安全步骤：锁屏方式胶囊 + 胁迫动作单选。
fn tap_security(x: i32, y: i32) -> i32 {
    if x >= 40 && x < 360 {
        for i in 0..3 {
            let ry = CONTENT_Y + 4 + 26 + i as i32 * 48;
            if y >= ry && y < ry + 40 {
                unsafe { SEC_TYPE = i; }
                return 2;
            }
        }
    }
    if x >= 40 {
        for i in 0..3 {
            let ry = CONTENT_Y + 4 + 26 + 3 * 48 + 8 + 18 + 26 + 24 + i as i32 * 24;
            if y >= ry && y < ry + 20 {
                unsafe { DURESS_ACTION = i; }
                return 2;
            }
        }
    }
    0
}

/// 主题步骤：外观胶囊 + 壁纸方块。
fn tap_theme(x: i32, y: i32) -> i32 {
    if x >= 60 && x < 260 {
        for i in 0..2 {
            let ry = CONTENT_Y + 16 + 26 + i as i32 * 48;
            if y >= ry && y < ry + 40 {
                unsafe { THEME_DARK = i == 1; }
                return 2;
            }
        }
    }
    if y >= 224 && y < 272 {
        for i in 0..3 {
            if x >= 60 + i as i32 * 160 && x < 60 + i as i32 * 160 + 48 {
                unsafe { WALLPAPER = i; }
                return 2;
            }
        }
    }
    0
}

/// 阻塞等待用户点击「下一步」或按下回车。
///
/// 30 秒无任何输入时回退为模拟点击自动推进（应用该步默认选择后进入下一步），
/// 保证 run-gui（/dev/input/event0 可打开但无串口触摸事件）下 OOBE 不会永久挂起。
fn wait_for_tap_next_button() -> i32 {
    let mut event = InputEvent {
        x: 0,
        y: 0,
        enter: false,
        back: false,
        ch: None,
        backspace: false,
    };
    let mut idle = 0usize;
    loop {
        if read_input_event(&mut event) {
            idle = 0;
            if event.enter {
                // 账号步骤：回车推进输入框焦点；最后一个输入框回车 = 下一步。
                if unsafe { STEP == 6 } {
                    let f = unsafe { FIELD_FOCUS };
                    if f >= 0 && f < 2 {
                        unsafe { FIELD_FOCUS = f + 1; }
                        print("[oobe] focus -> next field\n");
                        render();
                        flip();
                        continue;
                    }
                }
                print("[oobe] enter key -> next\n");
                return 1;
            }
            if event.backspace {
                if unsafe { STEP == 6 } {
                    let f = unsafe { FIELD_FOCUS };
                    if f >= 0 {
                        delete_field_char(f as usize);
                        print("[oobe] backspace\n");
                        render();
                        flip();
                    }
                }
                continue;
            }
            if let Some(c) = event.ch {
                if unsafe { STEP == 6 } {
                    let f = unsafe { FIELD_FOCUS };
                    if f >= 0 {
                        append_field_char(f as usize, c);
                        print("[oobe] typed char\n");
                        render();
                        flip();
                    }
                }
                continue;
            }
            if event.back {
                print("[oobe] back button hit\n");
                return -1;
            }
            let r = handle_tap(event.x, event.y);
            if r == 1 {
                return 1;
            }
            if r == -1 {
                return -1;
            }
            if r == 2 {
                // 本步选项被消费：立即重绘反映状态变化。
                render();
                flip();
            }
        } else {
            // 指针移动：重绘以更新光标位置（OOBE 每步只渲染一次）
            if unsafe { CURSOR_DIRTY } {
                unsafe { CURSOR_DIRTY = false; }
                render();
                flip();
            }
            sleep_ms(16);
            idle += 16;
            if idle >= 30000 {
                print("[oobe] no input for 30s, simulated tap -> next\n");
                return 1;
            }
        }
    }
}

/// 各步点击「下一步」前的默认选择处理。
fn apply_step_defaults(step: usize) {
    unsafe {
        match step {
            0 => {}
            1 => { LANG = 0; }
            2 => {
                for i in 0..17 {
                    MODULE_ON[i] = MODULES[i].default_on;
                }
            }
            3 => { REGION = 0; TIMEZONE = 0; }
            4 => { NET_SKIP = false; }
            5 => { AGREEMENT = false; }
            6 => { CLOUD_ENABLED = false; CLOUD_CONTROL = false; PARENTAL = false; }
            7 => { SEC_TYPE = 0; DURESS_ACTION = 0; }
            8 => { THEME_DARK = true; WALLPAPER = 2; }
            _ => {}
        }
    }
}

/// 模拟点击（固定坐标）：无输入设备时（如 run-gui）主流程回退调用本函数
/// 应用该步默认选择并自动推进，与 launcher 的模拟点击 fallback 一致。
fn simulate_click(step: usize) {
    let (x, y) = (DISPLAY_W as i32 - 32 - 22, NAV_Y + NAV_H / 2);
    print("[oobe] simulated tap next (");
    print_dec(x as usize);
    print(",");
    print_dec(y as usize);
    print(")\n");
    // 各步在点击「下一步」前的默认选择处理
    unsafe {
        match step {
            0 => {}
            1 => { LANG = 0; } // 默认简体中文
            2 => {
                for i in 0..17 {
                    MODULE_ON[i] = MODULES[i].default_on;
                }
            }
            3 => { REGION = 0; TIMEZONE = 0; }
            4 => { NET_SKIP = false; }
            5 => { AGREEMENT = false; }
            6 => { CLOUD_ENABLED = false; CLOUD_CONTROL = false; PARENTAL = false; }
            7 => { SEC_TYPE = 0; DURESS_ACTION = 0; }
            8 => { THEME_DARK = true; WALLPAPER = 2; }
            _ => {}
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[oobe] starting\n");
    mkdir_all();

    // 打开 /dev/fb0 并 mmap 到用户空间，直接绘制像素。
    if !fb_init() {
        print("[oobe] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[oobe] framebuffer ");
    print_dec(unsafe { FB_W });
    print("x");
    print_dec(unsafe { FB_H });
    print(" mapped\n");

    // 初始化默认模块勾选
    unsafe {
        for i in 0..17 {
            MODULE_ON[i] = MODULES[i].default_on;
        }
        STEP = 0;
        // 账号步骤默认值（可在输入框中修改）
        USER_BUF[..4].copy_from_slice(b"user");
        USER_LEN = 4;
        PASS_BUF[..4].copy_from_slice(b"1234");
        PASS_LEN = 4;
        CONFIRM_BUF[..4].copy_from_slice(b"1234");
        CONFIRM_LEN = 4;
        FIELD_FOCUS = 0;
    }

    let input_fd = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    unsafe { INPUT_FD = input_fd; }
    if input_fd < 0 {
        // 无输入设备（如 run-gui 无串口触摸）：回退为固定坐标模拟点击自动推进，
        // 与 launcher 的模拟点击 fallback 一致，避免 OOBE 永久挂起。
        print("[input] /dev/input/event0 not ready, fallback to simulated taps\n");
    } else {
        print("[input] /dev/input/event0 ready\n");
    }

    let mut step = 0usize;
    // 无输入设备时用固定坐标模拟点击自动推进（与 launcher 回退一致）。
    let simulated = input_fd < 0;
    while step < 10 {
        unsafe { STEP = step; }
        // 进入账号步骤时默认聚焦用户名输入框。
        if step == 6 {
            unsafe { FIELD_FOCUS = 0; }
        }
        log_step(step);
        if step == 4 {
            query_network();
        }
                render();
                flip();
        
        if step == 9 {
            // 完成页：进入系统
            print("[oobe] saving config...\n");
            write_file("/data/oobe/.done", b"1");
            print("[oobe] /data/oobe/.done created\n");
            break;
        }

        if simulated {
            // 无输入设备：模拟点击「下一步」（应用该步默认选择），短暂停留后推进。
                        simulate_click(step);
                        sleep_ms(1500);
                    } else {
            // 等待用户点击「下一步」（真实输入，不自动跳过）
                        let action = wait_for_tap_next_button();
                        if action < 0 {
                if step > 0 {
                    step -= 1;
                }
                continue;
            }
        }

            apply_step_defaults(step);
            save_step(step);

            step += 1;
    }

    print("[oobe] exiting\n");
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
        write_fd(2, b"PANIC: oobe\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
