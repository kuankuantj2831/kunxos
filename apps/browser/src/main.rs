// KunxOS 浏览器应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/browser）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 地址栏：可点击聚焦后经 virtio-keyboard 输入 URL（回车或「前往」加载）
//   3. 书签按钮：点击直接打开预设页面（本地演示页，无网络栈依赖）
//   4. 页面区域渲染演示内容；「主页」返回书签页
//   5. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_browser.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_BAR: u32 = 0x00_16_22_38;
const C_BAR_BORDER: u32 = 0x00_3A_4A_6A;
const C_URL_BG: u32 = 0x00_0B_12_1E;
const C_ACCENT: u32 = 0x00_22_C5_5E;
const C_LINK: u32 = 0x00_4A_9F_E8;
const C_BOOKMARK: u32 = 0x00_23_36_50;
const C_BOOKMARK_BORDER: u32 = 0x00_4A_5A_7A;
const C_TITLE: u32 = 0x00_FF_FF_FF;
const C_BODY: u32 = 0x00_9A_A7_B5;
const C_DIM2: u32 = 0x00_6B_78_8C;

const URL_Y: i32 = 56;
const URL_H: i32 = 40;
const BK_TOP: i32 = 110;

// ==================== 书签 ====================

struct Bookmark {
    label: &'static str,
    url: &'static str,
    title: &'static str,
    body: &'static str,
    color: u32,
}

static BOOKMARKS: [Bookmark; 4] = [
    Bookmark { label: "主页", url: "kunxos://home", title: "KunxOS 首页", body: "欢迎使用 KunxOS 浏览器。点击下方书签浏览演示页面。本浏览器为本地渲染演示，不依赖网络栈。", color: 0x00_22_C5_5E },
    Bookmark { label: "新闻", url: "kunxos://news", title: "今日要闻", body: "KunxOS 1.0 发布：全新动画系统、应用商店与 OOBE 首次启动向导。开发者预览版已开放下载。", color: 0x00_E7_4C_3C },
    Bookmark { label: "应用", url: "kunxos://apps", title: "应用推荐", body: "文件管理器、时钟、日历、地图、天气等内置应用已全部上线，从桌面启动器即可打开。", color: 0x00_9B_59_B6 },
    Bookmark { label: "关于", url: "kunxos://about", title: "关于 KunxOS", body: "KunxOS 是一个从零开始编写的 ARM64 操作系统，使用 Rust 实现内核、驱动与应用，覆盖完整系统栈。", color: 0x00_F9_73_16 },
];

// ==================== 状态 ====================

static mut URL_BUF: [u8; 48] = [0; 48];
static mut URL_LEN: usize = 0;
static mut URL_FOCUS: bool = false;
static mut PAGE: usize = 0; // 当前页面下标（0..=BOOKMARKS.len()，=len 表示未知地址）
static mut UNKNOWN_URL: bool = false;

// ==================== 渲染 ====================

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("浏览器".as_bytes());

    // 地址栏
    let focused = unsafe { URL_FOCUS };
    fill_rect(16, URL_Y, DISPLAY_W as i32 - 32, URL_H, C_URL_BG);
    draw_rect(16, URL_Y, DISPLAY_W as i32 - 32, URL_H, if focused { C_ACCENT } else { C_BAR_BORDER });
    let url = unsafe { &URL_BUF[..URL_LEN] };
    draw_text(28, URL_Y + 16, url, C_TEXT);
    if unsafe { URL_LEN } == 0 {
        draw_text(28, URL_Y + 16, "输入网址或点击书签".as_bytes(), C_DIM2);
    }
    if focused && (unsafe { CURSOR_BLINK }) {
        let cx = 28 + text_width(url);
        fill_rect(cx, URL_Y + 10, 2, 20, C_TEXT);
    }
    // 前往按钮
    fill_rect(DISPLAY_W as i32 - 76, URL_Y + 4, 56, URL_H - 8, C_ACCENT);
    draw_text(DISPLAY_W as i32 - 66, URL_Y + 16, "前往".as_bytes(), C_TEXT);

    // 书签行
    draw_text(16, BK_TOP + 4, "书签".as_bytes(), C_DIM2);
    let mut bx = 16;
    for b in BOOKMARKS.iter() {
        let w = text_width(b.label.as_bytes()) + 24;
        fill_rect(bx, BK_TOP + 28, w, 36, C_BOOKMARK);
        draw_rect(bx, BK_TOP + 28, w, 36, C_BOOKMARK_BORDER);
        draw_text(bx + 12, BK_TOP + 42, b.label.as_bytes(), C_LINK);
        bx += w + 10;
    }

    // 页面内容
    let page = unsafe { PAGE };
    if page < BOOKMARKS.len() {
        let b = &BOOKMARKS[page];
        draw_text(24, 210, b.title.as_bytes(), C_TITLE);
        // 正文按行折行渲染
        let mut y = 250;
        let mut line = [0u8; 96];
        let mut n = 0;
        for &c in b.body.as_bytes() {
            let cw = if c >= 0x80 { 16 } else { 8 };
            if n > 0 && text_width(&line[..n]) + cw > DISPLAY_W as i32 - 48 {
                draw_text(24, y, &line[..n], C_BODY);
                y += 28;
                n = 0;
            }
            line[n] = c;
            n += 1;
        }
        if n > 0 {
            draw_text(24, y, &line[..n], C_BODY);
        }
    } else if unsafe { UNKNOWN_URL } {
        draw_text(24, 210, "无法打开该地址".as_bytes(), C_TITLE);
        draw_text(24, 250, "演示浏览器仅支持内置页面".as_bytes(), C_BODY);
    }

    draw_cursor();
}

static mut CURSOR_BLINK: bool = false;
static mut BLINK_TICK: u32 = 0;

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[browser] starting\n");
    if !fb_init() {
        print("[browser] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[browser] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[browser] back -> exit\n");
                break;
            }
            // 地址栏聚焦
            if x >= 16 && x < DISPLAY_W as i32 - 76 && y >= URL_Y && y < URL_Y + URL_H {
                unsafe { URL_FOCUS = true; }
                dirty = true;
            } else if x >= DISPLAY_W as i32 - 76 && x < DISPLAY_W as i32 - 16
                && y >= URL_Y + 4 && y < URL_Y + URL_H - 4 {
                // 前往
                unsafe { URL_FOCUS = false; }
                go_url();
                dirty = true;
            } else if y >= BK_TOP + 28 && y < BK_TOP + 64 {
                // 书签
                let mut bx = 16;
                for (i, b) in BOOKMARKS.iter().enumerate() {
                    let w = text_width(b.label.as_bytes()) + 24;
                    if x >= bx && x < bx + w {
                        print("[browser] open bookmark ");
                        print(b.url);
                        print("\n");
                        unsafe {
                            URL_FOCUS = false;
                            PAGE = i;
                            UNKNOWN_URL = false;
                        }
                        set_url(b.url);
                        dirty = true;
                        break;
                    }
                    bx += w + 10;
                }
            } else {
                unsafe { URL_FOCUS = false; }
                dirty = true;
            }
        }
        // 键盘输入
        match ev.key {
            KeyAction::Char(c) => {
                if unsafe { URL_FOCUS } && unsafe { URL_LEN < URL_BUF.len() } {
                    unsafe {
                        URL_BUF[URL_LEN] = c;
                        URL_LEN += 1;
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                if unsafe { URL_FOCUS } && unsafe { URL_LEN > 0 } {
                    unsafe { URL_LEN -= 1; }
                    dirty = true;
                }
            }
            KeyAction::Enter => {
                if unsafe { URL_FOCUS } {
                    unsafe { URL_FOCUS = false; }
                    go_url();
                    dirty = true;
                }
            }
            KeyAction::Esc => {
                print("[browser] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[browser] ESC -> exit\n");
            break;
        }
        // 光标闪烁
        unsafe { BLINK_TICK += 1; }
        if unsafe { BLINK_TICK % 20 == 0 } {
            unsafe { CURSOR_BLINK = !CURSOR_BLINK; }
            if unsafe { URL_FOCUS } {
                dirty = true;
            }
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[browser] exiting\n");
    unsafe { exit(0); }
}

/// 把 URL 写入地址栏。
fn set_url(u: &str) {
    unsafe {
        let n = u.len().min(URL_BUF.len());
        URL_BUF[..n].copy_from_slice(&u.as_bytes()[..n]);
        URL_LEN = n;
    }
}

/// 按地址栏内容加载页面（匹配书签 URL，否则显示无法打开）。
fn go_url() {
    let url = unsafe { &URL_BUF[..URL_LEN] };
    for (i, b) in BOOKMARKS.iter().enumerate() {
        if url == b.url.as_bytes() {
            unsafe {
                PAGE = i;
                UNKNOWN_URL = false;
            }
            return;
        }
    }
    unsafe {
        PAGE = BOOKMARKS.len();
        UNKNOWN_URL = true;
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
        write_fd(2, b"PANIC: browser\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
