// KunxOS 图库应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/gallery）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 缩略图网格：程序生成的渐变「照片」瓷砖
//   3. 点缩略图进入全屏查看：放大渐变图 + 上下翻页（上一张/下一张）
//   4. 点击左上角「返回」或按 ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_gallery.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_BORDER: u32 = 0x00_2A_3A_5A;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_9A_A7_B5;
const C_BTN: u32 = 0x00_23_36_50;
const C_BTN_BORDER: u32 = 0x00_4A_5A_7A;

const COLS: i32 = 3;
const TILE_W: i32 = 164;
const TILE_H: i32 = 164;
const GAP: i32 = 10;
const GRID_X: i32 = 14;
const GRID_Y: i32 = 60;

/// 照片数。
const PHOTO_COUNT: u32 = 18;

// ==================== 状态 ====================

/// u32::MAX = 网格；否则为查看的照片下标。
static mut VIEWING: u32 = u32::MAX;

// ==================== 渲染 ====================

/// 由种子生成一个漂亮的渐变填充。
fn draw_photo(x: i32, y: i32, w: i32, h: i32, seed: u32) {
    let c1 = (seed * 2654435761).wrapping_rem(0xFFFFFF) | 0x00_40_00_00;
    let c2 = (seed.wrapping_mul(40503)).wrapping_rem(0xFFFFFF) | 0x00_00_00_40;
    for py in 0..h {
        let t = py as u32 * 255 / h as u32;
        for px in 0..w {
            let r = ((c1 >> 16) & 0xFF) + ((((c2 >> 16) & 0xFF) as i32 - ((c1 >> 16) & 0xFF) as i32) * t as i32 / 255) as u32;
            let g = ((c1 >> 8) & 0xFF) + ((((c2 >> 8) & 0xFF) as i32 - ((c1 >> 8) & 0xFF) as i32) * t as i32 / 255) as u32;
            let b = (c1 & 0xFF) + ((((c2 & 0xFF) as i32 - (c1 & 0xFF) as i32) * t as i32 / 255) as u32);
            put_pixel(x + px, y + py, 0x00_00_00_00 | ((r & 0xFF) << 16) | ((g & 0xFF) << 8) | (b & 0xFF));
        }
    }
}

fn render_grid() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("图库".as_bytes());
    for i in 0..PHOTO_COUNT {
        let r = (i as i32) / COLS;
        let c = (i as i32) % COLS;
        let x = GRID_X + c * (TILE_W + GAP);
        let y = GRID_Y + r * (TILE_H + GAP);
        if y + TILE_H > DISPLAY_H as i32 {
            break;
        }
        draw_photo(x, y, TILE_W, TILE_H, i);
        draw_rect(x, y, TILE_W, TILE_H, C_BORDER);
    }
    draw_cursor();
}

fn render_view(idx: u32) {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    // 标题栏：返回 + 序号
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    let mut t = [0u8; 16];
    t[..7].copy_from_slice("照片 ".as_bytes());
    let mut n = 7;
    n = push_num(&mut t, n, idx + 1);
    draw_text(48, (TITLE_H - 8) / 2, &t[..n], C_TEXT);

    // 大图（上下留边）
    draw_photo(20, 90, DISPLAY_W as i32 - 40, 560, idx);
    draw_rect(20, 90, DISPLAY_W as i32 - 40, 560, C_BORDER);

    // 上一张 / 下一张
    draw_btn(30, 700, "上一张".as_bytes());
    draw_btn(DISPLAY_W as i32 - 30 - 96, 700, "下一张".as_bytes());

    // 底部提示
    let mut hint = [0u8; 32];
    hint[..24].copy_from_slice("共 ".as_bytes());
    let mut hn = 3;
    hn = push_num(&mut hint, hn, PHOTO_COUNT);
    hint[hn] = b' ';
    hn += 1;
    hint[hn..hn + 6].copy_from_slice("张照片".as_bytes());
    hn += 6;
    draw_text(16, DISPLAY_H as i32 - 60, &hint[..hn], C_DIM);

    draw_cursor();
}

fn draw_btn(x: i32, y: i32, label: &[u8]) {
    let w = 96;
    let h = 44;
    fill_rect(x + 3, y, w - 6, h, C_BTN);
    fill_rect(x, y + 3, w, h - 6, C_BTN);
    draw_rect(x, y, w, h, C_BTN_BORDER);
    let tx = x + (w - text_width(label)) / 2;
    draw_text(tx, y + 18, label, C_TEXT);
}

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
    print("[gallery] starting\n");
    if !fb_init() {
        print("[gallery] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[gallery] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                if back_hit(x, y) {
                    print("[gallery] back -> exit\n");
                    break;
                }
                // 网格点击
                for i in 0..PHOTO_COUNT {
                    let r = (i as i32) / COLS;
                    let c = (i as i32) % COLS;
                    let tx = GRID_X + c * (TILE_W + GAP);
                    let ty = GRID_Y + r * (TILE_H + GAP);
                    if x >= tx && x < tx + TILE_W && y >= ty && y < ty + TILE_H {
                        unsafe { VIEWING = i; }
                        print("[gallery] open photo ");
                        print_dec(i as u64);
                        print("\n");
                        dirty = true;
                        break;
                    }
                }
            } else {
                if back_hit(x, y) {
                    unsafe { VIEWING = u32::MAX; }
                    dirty = true;
                } else if x >= 30 && x < 126 && y >= 700 && y < 744 {
                    if viewing > 0 {
                        unsafe { VIEWING -= 1; }
                        dirty = true;
                    }
                } else if x >= DISPLAY_W as i32 - 126 && x < DISPLAY_W as i32 - 30 && y >= 700 && y < 744 {
                    if viewing + 1 < PHOTO_COUNT {
                        unsafe { VIEWING += 1; }
                        dirty = true;
                    }
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[gallery] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[gallery] ESC -> exit\n");
            break;
        }
        if dirty {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                render_grid();
            } else {
                render_view(viewing);
            }
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[gallery] exiting\n");
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
        write_fd(2, b"PANIC: gallery\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
