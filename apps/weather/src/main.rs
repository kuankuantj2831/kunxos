// KunxOS 天气应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/weather）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 当前天气卡：城市 + 温度 + 天气图标（太阳/云/雨）+ 湿度/风速
//   3. 未来 5 天预报列表：星期 + 图标 + 最高/最低温
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_weather.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_1E_3A_5A;
const C_CARD: u32 = 0x00_2A_4A_6A;
const C_CARD_BORDER: u32 = 0x00_3A_5A_7A;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_B8_D0;
const C_SUN: u32 = 0x00_FF_D0_4C;
const C_CLOUD: u32 = 0x00_C8_D0_D8;
const C_RAIN: u32 = 0x00_5A_B8_F0;
const C_ACCENT: u32 = 0x00_22_C5_5E;

// ==================== 数据 ====================

struct Forecast {
    day: &'static str,
    cond: &'static str,
    high: i32,
    low: i32,
}

static FORECAST: [Forecast; 5] = [
    Forecast { day: "今天", cond: "晴", high: 28, low: 19 },
    Forecast { day: "明天", cond: "多云", high: 27, low: 18 },
    Forecast { day: "周三", cond: "小雨", high: 24, low: 17 },
    Forecast { day: "周四", cond: "多云", high: 26, low: 18 },
    Forecast { day: "周五", cond: "晴", high: 29, low: 20 },
];

// ==================== 渲染 ====================

/// 太阳：圆 + 光芒。
fn draw_sun(cx: i32, cy: i32, r: i32) {
    draw_disc(cx, cy, r, C_SUN);
    for k in 0..8 {
        let deg = k * 45;
        let (x1, y1) = ray_end(cx, cy, r + 6, deg);
        let (x2, y2) = ray_end(cx, cy, r + 12, deg);
        draw_line(x1, y1, x2, y2, C_SUN);
    }
}

fn ray_end(cx: i32, cy: i32, l: i32, deg: i32) -> (i32, i32) {
    // 简单 8 方向
    match deg {
        0 => (cx, cy - l),
        45 => (cx + l / 2, cy - l / 2),
        90 => (cx + l, cy),
        135 => (cx + l / 2, cy + l / 2),
        180 => (cx, cy + l),
        225 => (cx - l / 2, cy + l / 2),
        270 => (cx - l, cy),
        _ => (cx - l / 2, cy - l / 2),
    }
}

/// 云：三个圆并排。
fn draw_cloud(cx: i32, cy: i32) {
    draw_disc(cx - 16, cy + 6, 12, C_CLOUD);
    draw_disc(cx, cy, 16, C_CLOUD);
    draw_disc(cx + 18, cy + 6, 12, C_CLOUD);
}

/// 雨：云 + 斜线。
fn draw_rain(cx: i32, cy: i32) {
    draw_cloud(cx, cy);
    draw_line(cx - 12, cy + 24, cx - 18, cy + 36, C_RAIN);
    draw_line(cx + 4, cy + 24, cx - 2, cy + 36, C_RAIN);
    draw_line(cx + 20, cy + 24, cx + 14, cy + 36, C_RAIN);
}

fn cond_icon(cx: i32, cy: i32, cond: &str) {
    if cond == "晴" {
        draw_sun(cx, cy, 18);
    } else if cond == "小雨" {
        draw_rain(cx, cy);
    } else {
        draw_cloud(cx, cy);
    }
}

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("天气".as_bytes());

    // 当前天气卡
    fill_rect(16, 64, DISPLAY_W as i32 - 32, 240, C_CARD);
    draw_rect(16, 64, DISPLAY_W as i32 - 32, 240, C_CARD_BORDER);
    draw_text(32, 84, "北京市".as_bytes(), C_TEXT);
    draw_text(32, 112, "多云".as_bytes(), C_DIM);
    // 大温度
    let mut t = [0u8; 6];
    t[0] = b'2';
    t[1] = b'6';
    t[2] = 0xC2; // °
    t[3] = 0xB0;
    t[4] = b'C';
    draw_text(200, 96, &t[..5], C_TEXT);
    cond_icon(360, 170, "多云");
    // 附加信息
    draw_text(32, 220, "湿度 45%   风速 12km/h".as_bytes(), C_DIM);
    draw_text(32, 248, "空气质量 优".as_bytes(), C_DIM);

    // 未来 5 天
    draw_text(24, 330, "未来 5 天".as_bytes(), C_DIM);
    for (i, f) in FORECAST.iter().enumerate() {
        let y = 356 + i as i32 * 96;
        if y + 92 > DISPLAY_H as i32 {
            break;
        }
        fill_rect(16, y, DISPLAY_W as i32 - 32, 84, C_CARD);
        draw_rect(16, y, DISPLAY_W as i32 - 32, 84, C_CARD_BORDER);
        draw_text(32, y + 36, f.day.as_bytes(), C_TEXT);
        cond_icon(140, y + 42, f.cond);
        draw_text(180, y + 36, f.cond.as_bytes(), C_DIM);
        // 温度（高/低）
        let mut tmp = [0u8; 16];
        let mut n = 0;
        n = push_temp(&mut tmp, n, f.high);
        tmp[n] = b'/';
        n += 1;
        n = push_temp(&mut tmp, n, f.low);
        draw_text(DISPLAY_W as i32 - 160, y + 36, &tmp[..n], C_TEXT);
    }

    draw_cursor();
}

fn push_temp(buf: &mut [u8; 16], mut n: usize, v: i32) -> usize {
    if v < 0 {
        buf[n] = b'-';
        n += 1;
    }
    let mut tmp = [0u8; 4];
    let mut i = tmp.len();
    let mut v = v.unsigned_abs();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    if i == tmp.len() {
        buf[n] = b'0';
        n += 1;
    } else {
        while i < tmp.len() {
            buf[n] = tmp[i];
            n += 1;
            i += 1;
        }
    }
    buf[n] = 0xC2; // °
    buf[n + 1] = 0xB0;
    n + 2
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[weather] starting\n");
    if !fb_init() {
        print("[weather] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[weather] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[weather] back -> exit\n");
                break;
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[weather] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[weather] ESC -> exit\n");
            break;
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[weather] exiting\n");
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
        write_fd(2, b"PANIC: weather\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
