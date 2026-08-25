// KunxOS 日历应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/calendar）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 月视图：星期表头 + 当月日期网格，今天高亮
//   3. 「上个月 / 下个月」按钮翻页，显示 年月
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_calendar.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_HEADER: u32 = 0x00_16_22_38;
const C_CELL: u32 = 0x00_1B_2B_4A;
const C_CELL_BORDER: u32 = 0x00_2A_3A_5A;
const C_TODAY: u32 = 0x00_22_C5_5E;
const C_TODAY_TEXT: u32 = 0x00_10_18_28;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A7_B5;
const C_WEEKEND: u32 = 0x00_E7_4C_3C;
const C_BTN: u32 = 0x00_23_36_50;
const C_BTN_BORDER: u32 = 0x00_4A_5A_7A;

const GRID_X: i32 = 12;
const GRID_Y: i32 = 200;
const CELL_W: i32 = 72;
const CELL_H: i32 = 72;
const CELL_GAP: i32 = 6;

/// 星期表头（日 一 二 三 四 五 六）。
static WEEKDAY_NAMES: [&[u8]; 7] = [
    "日".as_bytes(), "一".as_bytes(), "二".as_bytes(), "三".as_bytes(),
    "四".as_bytes(), "五".as_bytes(), "六".as_bytes(),
];

/// 月份名。
static MONTH_NAMES: [&[u8]; 12] = [
    "一月".as_bytes(), "二月".as_bytes(), "三月".as_bytes(), "四月".as_bytes(),
    "五月".as_bytes(), "六月".as_bytes(), "七月".as_bytes(), "八月".as_bytes(),
    "九月".as_bytes(), "十月".as_bytes(), "十一月".as_bytes(), "十二月".as_bytes(),
];

/// 月天数（非闰年）。
fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 }
        }
        _ => 30,
    }
}

// ==================== 状态 ====================

/// 当前显示的年月。
static mut CUR_Y: i32 = 2025;
static mut CUR_M: u32 = 1;

/// 今天的年月日（用于高亮）。
static mut TODAY_Y: i32 = 2025;
static mut TODAY_M: u32 = 1;
static mut TODAY_D: u32 = 1;

// ==================== 渲染 ====================

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("日历".as_bytes());

    // 年月标题 + 翻页按钮
    let cy = unsafe { CUR_Y };
    let cm = unsafe { CUR_M };
    let mut title = [0u8; 16];
    let mut n = 0;
    n = push_num(&mut title, n, cy as u32);
    title[n] = b' ';
    n += 1;
    let mname = MONTH_NAMES[(cm - 1) as usize];
    title[n..n + mname.len()].copy_from_slice(mname);
    n += mname.len();
    draw_text(220, 70, &title[..n], C_TEXT);

    // 上/下月按钮
    draw_btn(16, 64, "上月".as_bytes());
    draw_btn(DISPLAY_W as i32 - 88, 64, "下月".as_bytes());

    // 星期表头
    for (i, w) in WEEKDAY_NAMES.iter().enumerate() {
        let x = GRID_X + i as i32 * (CELL_W + CELL_GAP);
        let color = if i == 0 || i == 6 { C_WEEKEND } else { C_DIM };
        draw_text(x + (CELL_W - text_width(w)) / 2, GRID_Y - 30, w, color);
    }

    // 日期网格
    let first_dow = weekday_from_days(days_from_civil(cy, cm, 1)); // 0=周日
    let dim = days_in_month(cy, cm);
    let today = unsafe { (TODAY_Y, TODAY_M, TODAY_D) };
    for d in 1..=dim {
        let idx = (first_dow as i32) + d as i32 - 1;
        let r = idx / 7;
        let c = idx % 7;
        let x = GRID_X + c * (CELL_W + CELL_GAP);
        let y = GRID_Y + r * (CELL_H + CELL_GAP);
        if y + CELL_H > DISPLAY_H as i32 {
            break;
        }
        let is_today = cy == today.0 && cm == today.1 && d == today.2 as u32;
        fill_rect(x + 2, y, CELL_W - 4, CELL_H - 4, if is_today { C_TODAY } else { C_CELL });
        draw_rect(x, y, CELL_W, CELL_H, if is_today { C_TODAY } else { C_CELL_BORDER });
        let mut dbuf = [0u8; 4];
        let dn = push_num(&mut dbuf, 0, d);
        let tx = x + 8;
        let ty = y + 10;
        draw_text(tx, ty, &dbuf[..dn], if is_today { C_TODAY_TEXT } else { C_TEXT });
    }

    // 底部提示
    draw_text(16, DISPLAY_H as i32 - 60, "点击上月/下月翻页".as_bytes(), C_DIM);

    draw_cursor();
}

fn draw_btn(x: i32, y: i32, label: &[u8]) {
    let w = text_width(label) + 24;
    let h = 36;
    fill_rect(x + 3, y, w - 6, h, C_BTN);
    fill_rect(x, y + 3, w, h - 6, C_BTN);
    draw_rect(x, y, w, h, C_BTN_BORDER);
    draw_text(x + 12, y + 13, label, C_TEXT);
}

fn btn_hit(x: i32, y: i32, bx: i32, by: i32, label: &[u8]) -> bool {
    let w = text_width(label) + 24;
    let h = 36;
    x >= bx && x < bx + w && y >= by && y < by + h
}

/// 把数字追加到缓冲。
fn push_num(buf: &mut [u8], mut n: usize, mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if v == 0 {
        tmp[i - 1] = b'0';
        i -= 1;
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    while i < tmp.len() {
        buf[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[calendar] starting\n");
    if !fb_init() {
        print("[calendar] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[calendar] ready\n");

    // 初始化为今天
    let (y, m, d) = now_date();
    unsafe {
        CUR_Y = y;
        CUR_M = m;
        TODAY_Y = y;
        TODAY_M = m;
        TODAY_D = d;
    }

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[calendar] back -> exit\n");
                break;
            }
            if btn_hit(x, y, 16, 64, "上月".as_bytes()) {
                unsafe {
                    if CUR_M == 1 {
                        CUR_M = 12;
                        CUR_Y -= 1;
                    } else {
                        CUR_M -= 1;
                    }
                }
                dirty = true;
            } else if btn_hit(x, y, DISPLAY_W as i32 - 88, 64, "下月".as_bytes()) {
                unsafe {
                    if CUR_M == 12 {
                        CUR_M = 1;
                        CUR_Y += 1;
                    } else {
                        CUR_M += 1;
                    }
                }
                dirty = true;
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[calendar] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[calendar] ESC -> exit\n");
            break;
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[calendar] exiting\n");
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
        write_fd(2, b"PANIC: calendar\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
