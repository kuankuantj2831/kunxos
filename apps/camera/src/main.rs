// KunxOS 相机应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/camera）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 取景器模拟：暗色画面 + 三分构图线 + 左上角 REC 指示
//   3. 底部快门：点击闪光并生成一张「照片」缩略图（进度条模拟处理）
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_camera.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_08_0C_14;
const C_GRID: u32 = 0x00_4A_5A_6A;
const C_SHUTTER: u32 = 0x00_FF_FF_FF;
const C_SHUTTER_IN: u32 = 0x00_10_18_28;
const C_REC: u32 = 0x00_E7_4C_3C;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A7_B5;

const SHUTTER_CX: i32 = (DISPLAY_W / 2) as i32;
const SHUTTER_CY: i32 = 850;
const SHUTTER_R: i32 = 44;
const THUMB_Y: i32 = 780;

// ==================== 状态 ====================

static mut PHOTO_COUNT: u32 = 0;
/// 闪光帧计数（>0 时画面全白）。
static mut FLASH: u32 = 0;
/// 处理进度（0..100）。
static mut PROCESSING: u32 = 0;

// ==================== 渲染 ====================

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("相机".as_bytes());

    // 取景器画面（渐变模拟）
    let mut seed: u32 = 0x12345678;
    for y in 100..700 {
        for x in 0..DISPLAY_W as i32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = ((seed >> 16) & 0xFF) as u32;
            let color = 0x00_10_14_20 | (v << 8) | (v << 16);
            put_pixel(x, y, color);
        }
    }
    // 三分构图线
    draw_line(0, 100, DISPLAY_W as i32, 100, C_GRID);
    draw_line(0, 700, DISPLAY_W as i32, 700, C_GRID);
    draw_line(DISPLAY_W as i32 / 3, 100, DISPLAY_W as i32 / 3, 700, C_GRID);
    draw_line(DISPLAY_W as i32 / 3 * 2, 100, DISPLAY_W as i32 / 3 * 2, 700, C_GRID);

    // REC 指示
    if unsafe { PHOTO_COUNT } % 2 == 0 {
        draw_disc(36, 80, 6, C_REC);
        draw_text(52, 74, "REC".as_bytes(), C_REC);
    }

    // 快门
    draw_circle(SHUTTER_CX, SHUTTER_CY, SHUTTER_R, C_SHUTTER);
    draw_disc(SHUTTER_CX, SHUTTER_CY, SHUTTER_R - 10, C_SHUTTER_IN);
    draw_circle(SHUTTER_CX, SHUTTER_CY, SHUTTER_R - 10, C_SHUTTER);

    // 缩略图（最近 4 张）
    let n = unsafe { PHOTO_COUNT };
    for i in 0..4 {
        let idx = if n > i { (n - 1 - i) as usize } else { usize::MAX };
        let x = 24 + i as i32 * 130;
        if idx != usize::MAX {
            let h = (idx as u32 * 2654435761).wrapping_rem(0xFFFFFF) as u32 | 0x00_30_30_30;
            fill_rect(x, THUMB_Y, 110, 56, h);
            draw_rect(x, THUMB_Y, 110, 56, C_GRID);
        }
    }

    // 处理中进度
    let p = unsafe { PROCESSING };
    if p > 0 && p < 100 {
        fill_rect(60, 200, DISPLAY_W as i32 - 120, 8, C_GRID);
        fill_rect(60, 200, (DISPLAY_W as i32 - 120) * (p as i32) / 100, 8, C_REC);
        draw_center(220, "处理中...".as_bytes(), C_DIM);
    }

    draw_text(16, DISPLAY_H as i32 - 40, "点击快门拍照".as_bytes(), C_DIM);
    draw_cursor();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[camera] starting\n");
    if !fb_init() {
        print("[camera] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[camera] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[camera] back -> exit\n");
                break;
            }
            let dx = x - SHUTTER_CX;
            let dy = y - SHUTTER_CY;
            if dx * dx + dy * dy <= SHUTTER_R * SHUTTER_R && unsafe { PROCESSING == 0 } {
                print("[camera] shutter!\n");
                unsafe {
                    FLASH = 4;
                    PROCESSING = 1;
                }
                dirty = true;
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[camera] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[camera] ESC -> exit\n");
            break;
        }
        if unsafe { FLASH > 0 } {
            unsafe { FLASH -= 1; }
            dirty = true;
        }
        if unsafe { PROCESSING > 0 } {
            unsafe { PROCESSING += 4; }
            if unsafe { PROCESSING >= 100 } {
                unsafe {
                    PROCESSING = 0;
                    PHOTO_COUNT += 1;
                }
            }
            dirty = true;
        }
        if dirty {
            render();
            // 闪光帧覆盖全白
            if unsafe { FLASH > 0 } {
                fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, 0x00_FF_FF_FF);
                draw_cursor();
            }
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[camera] exiting\n");
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
        write_fd(2, b"PANIC: camera\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
