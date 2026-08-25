// KunxOS 时钟应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/clock）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 绘制全屏时钟：标题栏（返回键）+ 数字钟（HH:MM:SS，3 倍放大）+
//      模拟表盘（时针/分针/秒针）+ 公历日期
//   3. 每秒刷新一次（墙钟来自内核 RTC），指针移动时刷新光标
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

// ==================== 共享框架与字体 ====================

#[path = "../../../shared/cjk_font_clock.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_FACE: u32 = 0x00_1E_2B_42;
const C_FACE_BORDER: u32 = 0x00_3A_4A_6A;
const C_TICK: u32 = 0x00_9A_A7_B5;
const C_TICK_MAJOR: u32 = 0x00_FF_FF_FF;
const C_HOUR: u32 = 0x00_FF_FF_FF;
const C_MIN: u32 = 0x00_9A_A7_B5;
const C_SEC: u32 = 0x00_E7_4C_3C;
const C_ACC: u32 = 0x00_22_C5_5E;
const C_DIM2: u32 = 0x00_6B_78_8C;

// ==================== 正弦表（0..=90 度，×256） ====================

static SIN_TABLE: [i32; 91] = [
    0, 4, 9, 13, 18, 22, 27, 31, 36, 40, 45, 49, 54, 58, 63, 67, 71, 76, 80, 84,
    89, 93, 97, 101, 106, 110, 114, 118, 122, 126, 130, 134, 138, 142, 146, 150,
    154, 158, 162, 165, 169, 173, 177, 180, 184, 187, 191, 194, 198, 201, 205,
    208, 211, 215, 218, 221, 224, 227, 230, 233, 236, 239, 241, 244, 247, 249,
    252, 254, 256, 259, 261, 263, 265, 267, 269, 271, 273, 275, 276, 278, 280,
    281, 283, 284, 285, 287, 288, 289, 290, 291, 292,
];

/// 角度（度）→ sin ×256；0 点为 12 点钟方向，顺时针。
fn sin256(deg: i32) -> i32 {
    let d = ((deg % 360) + 360) % 360;
    if d <= 90 {
        SIN_TABLE[d as usize]
    } else if d <= 180 {
        SIN_TABLE[(180 - d) as usize]
    } else if d <= 270 {
        -SIN_TABLE[(d - 180) as usize]
    } else {
        -SIN_TABLE[(360 - d) as usize]
    }
}

fn cos256(deg: i32) -> i32 {
    sin256(deg + 90)
}

// ==================== 绘制 ====================

/// 8x8 字形按 scale 放大绘制（时钟数字用 3 倍）。
fn draw_char_scaled(x: i32, y: i32, c: u8, scale: i32, color: u32) {
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8 {
            if bits & (0x80 >> col) != 0 {
                for sy in 0..scale {
                    for sx in 0..scale {
                        put_pixel(x + col as i32 * scale + sx, y + row as i32 * scale + sy, color);
                    }
                }
            }
        }
    }
}

fn draw_text_scaled(x: i32, y: i32, s: &[u8], scale: i32, color: u32) {
    let mut cx = x;
    for &c in s {
        draw_char_scaled(cx, y, c, scale, color);
        cx += 8 * scale;
    }
}

fn text_width_scaled(s: &[u8], scale: i32) -> i32 {
    (s.len() as i32) * 8 * scale
}

fn draw_center_scaled(y: i32, s: &[u8], scale: i32, color: u32) {
    let tx = (DISPLAY_W as i32 - text_width_scaled(s, scale)) / 2;
    draw_text_scaled(tx, y, s, scale, color);
}

/// 表盘：圆 + 12 个主刻度 + 60 个次刻度。
fn draw_face(cx: i32, cy: i32, r: i32) {
    draw_circle(cx, cy, r, C_FACE_BORDER);
    draw_circle(cx, cy, r - 2, C_FACE);
    for k in 0..60 {
        let deg = k * 6;
        let major = k % 5 == 0;
        let (sx, sy) = hand_end(cx, cy, r - (if major { 10 } else { 5 }), deg);
        let (ex, ey) = hand_end(cx, cy, r - 2, deg);
        draw_line(sx, sy, ex, ey, if major { C_TICK_MAJOR } else { C_TICK });
    }
}

/// 指针终点（长度 L，角度 deg，0 点 = 12 点方向顺时针）。
fn hand_end(cx: i32, cy: i32, l: i32, deg: i32) -> (i32, i32) {
    (cx + l * sin256(deg) / 256, cy - l * cos256(deg) / 256)
}

fn draw_hand(cx: i32, cy: i32, l: i32, deg: i32, w: i32, color: u32) {
    let (ex, ey) = hand_end(cx, cy, l, deg);
    // 粗指针：沿主方向画 w 条平行线。
    let (px, py) = hand_end(cx, cy, 1, deg + 90);
    let dx = px - cx;
    let dy = py - cy;
    for i in -(w / 2)..=(w / 2) {
        draw_line(cx + dx * i, cy + dy * i, ex + dx * i, ey + dy * i, color);
    }
}

// ==================== 渲染 ====================

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("时钟".as_bytes());

    let (h, m, s) = now_hms();
    let (y, mo, d) = now_date();

    // 数字钟：HH:MM:SS（3 倍放大）
    let mut time = [0u8; 8];
    time[0] = b'0' + (h / 10) as u8;
    time[1] = b'0' + (h % 10) as u8;
    time[2] = b':';
    time[3] = b'0' + (m / 10) as u8;
    time[4] = b'0' + (m % 10) as u8;
    time[5] = b':';
    time[6] = b'0' + (s / 10) as u8;
    time[7] = b'0' + (s % 10) as u8;
    draw_center_scaled(80, &time, 3, C_TEXT);

    // 日期行
    let mut date = [0u8; 12];
    let mut n = 0;
    n = push_num(&mut date, n, y as u32);
    date[n] = b'-';
    n += 1;
    n = push_num2(&mut date, n, mo);
    date[n] = b'-';
    n += 1;
    n = push_num2(&mut date, n, d);
    draw_center(150, &date[..n], C_DIM);

    // 模拟表盘
    let cx = (DISPLAY_W / 2) as i32;
    let cy = 520;
    let r = 150;
    draw_face(cx, cy, r);

    // 指针角度：小时（含分偏移）、分（含秒偏移）、秒
    let hdeg = ((h % 12) * 30 + (m * 6) / 12) as i32;
    let mdeg = (m * 6 + s / 10) as i32;
    let sdeg = (s * 6) as i32;
    draw_hand(cx, cy, r - 55, hdeg, 6, C_HOUR);
    draw_hand(cx, cy, r - 25, mdeg, 4, C_MIN);
    draw_hand(cx, cy, r - 8, sdeg, 2, C_SEC);
    draw_disc(cx, cy, 5, C_ACC);

    // 12/3/6/9 数字标注
    draw_center_scaled(cy - r - 26, b"12", 1, C_DIM);
    draw_text_scaled(cx + r + 8, cy - 4, b"3", 1, C_DIM);
    draw_center_scaled(cy + r + 12, b"6", 1, C_DIM);
    draw_text_scaled(cx - r - 20, cy - 4, b"9", 1, C_DIM);

    draw_cursor();
}

/// 把数字追加到缓冲（不补零）。
fn push_num(buf: &mut [u8; 12], mut n: usize, mut v: u32) -> usize {
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

/// 两位数（不足补零）。
fn push_num2(buf: &mut [u8; 12], n: usize, v: u32) -> usize {
    buf[n] = b'0' + (v / 10) as u8;
    buf[n + 1] = b'0' + (v % 10) as u8;
    n + 2
}

// ==================== main ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[clock] starting\n");
    if !fb_init() {
        print("[clock] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[clock] ready\n");
    print("[clock] framebuffer mapped\n");

    let mut last_sec = u32::MAX;
    let mut dirty = true;

    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            print("[clock] tap (");
            print_dec(x as u64);
            print(",");
            print_dec(y as u64);
            print(")\n");
            if back_hit(x, y) {
                print("[clock] back -> exit\n");
                break;
            }
        }
        if ev.key == KeyAction::Esc || poll_tty_esc() {
            print("[clock] ESC -> exit\n");
            break;
        }
        let (_, _, s) = now_hms();
        if s != last_sec {
            last_sec = s;
            dirty = true;
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(32);
    }

    print("[clock] exiting\n");
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
        write_fd(2, b"PANIC: clock\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
