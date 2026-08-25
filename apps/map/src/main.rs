// KunxOS 地图应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/map）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 静态地图：街区方块 + 道路网格 + 河流 + 定位标记
//   3. 左上角罗盘 N，右上角「+ / -」缩放按钮（改变网格步长）
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_map.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_1E_2B_3A;
const C_BLOCK: u32 = 0x00_2E_4A_5A;
const C_ROAD: u32 = 0x00_3A_4A_6A;
const C_RIVER: u32 = 0x00_2E_6B_A8;
const C_PARK: u32 = 0x00_2E_8B_57;
const C_MARKER: u32 = 0x00_E7_4C_3C;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A7_B5;
const C_BTN: u32 = 0x00_23_36_50;
const C_BTN_BORDER: u32 = 0x00_4A_5A_7A;

const MAP_TOP: i32 = 52;
const MAP_BOTTOM: i32 = 900;

// ==================== 状态 ====================

/// 缩放级别（网格步长像素）。
static mut ZOOM: i32 = 60;

// ==================== 渲染 ====================

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("地图".as_bytes());

    let zoom = unsafe { ZOOM };

    // 街区（伪随机色块）+ 道路网格
    let mut seed: u32 = 0x9E3779B9;
    let mut r = MAP_TOP;
    while r < MAP_BOTTOM {
        let mut c = 0;
        let mut col = 0;
        while c < DISPLAY_W as i32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (seed >> 24) & 0xF;
            let color = match v {
                0..=7 => C_BLOCK,
                8..=10 => C_PARK,
                11 => C_RIVER,
                _ => C_BLOCK,
            };
            let bw = zoom.saturating_sub(4).min(DISPLAY_W as i32 - c);
            let bh = zoom.saturating_sub(4).min(MAP_BOTTOM - r);
            fill_rect(c + 2, r + 2, bw.saturating_sub(4), bh.saturating_sub(4), color);
            // 河流贯穿
            if col == 6 && color != C_RIVER {
                fill_rect(c + 2, r + 2, 12, bh.saturating_sub(4), C_RIVER);
            }
            c += zoom;
            col += 1;
        }
        // 道路横线
        fill_rect(0, r + zoom - 2, DISPLAY_W as i32, 4, C_ROAD);
        r += zoom;
    }
    // 道路竖线（在块之上）
    let mut c = zoom;
    while c < DISPLAY_W as i32 {
        fill_rect(c - 2, MAP_TOP, 4, MAP_BOTTOM - MAP_TOP, C_ROAD);
        c += zoom;
    }

    // 定位标记（中心）
    let cx = (DISPLAY_W / 2) as i32;
    let cy = ((MAP_TOP + MAP_BOTTOM) / 2) as i32;
    draw_disc(cx, cy, 8, C_MARKER);
    draw_circle(cx, cy, 14, C_MARKER);
    draw_disc(cx, cy, 3, C_TEXT);

    // 罗盘
    draw_circle(36, 130, 18, C_BTN_BORDER);
    draw_text(32, 126, "N".as_bytes(), C_TEXT);
    draw_line(36, 118, 36, 106, C_TEXT);

    // 缩放按钮
    draw_btn(DISPLAY_W as i32 - 64, 64, "+");
    draw_btn(DISPLAY_W as i32 - 64, 116, "-");

    // 底部标签
    draw_text(16, DISPLAY_H as i32 - 40, "KunxOS 地图（演示）".as_bytes(), C_DIM);

    draw_cursor();
}

fn draw_btn(x: i32, y: i32, label: &str) {
    let w = 44;
    let h = 40;
    fill_rect(x + 3, y, w - 6, h, C_BTN);
    fill_rect(x, y + 3, w, h - 6, C_BTN);
    draw_rect(x, y, w, h, C_BTN_BORDER);
    draw_text(x + 18, y + 15, label.as_bytes(), C_TEXT);
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[map] starting\n");
    if !fb_init() {
        print("[map] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[map] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[map] back -> exit\n");
                break;
            }
            // 缩放
            if x >= DISPLAY_W as i32 - 64 && x < DISPLAY_W as i32 - 20 && y >= 64 && y < 104 {
                let z = unsafe { ZOOM };
                if z < 90 {
                    unsafe { ZOOM = z + 10; }
                    dirty = true;
                }
            } else if x >= DISPLAY_W as i32 - 64 && x < DISPLAY_W as i32 - 20 && y >= 116 && y < 156 {
                let z = unsafe { ZOOM };
                if z > 30 {
                    unsafe { ZOOM = z - 10; }
                    dirty = true;
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[map] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[map] ESC -> exit\n");
            break;
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[map] exiting\n");
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
        write_fd(2, b"PANIC: map\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
