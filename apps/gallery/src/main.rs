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

const C_BG: u32 = 0x00_F7_F8_FA;
const C_BORDER: u32 = 0x00_D9_DE_E6;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_BTN: u32 = 0x00_FF_FF_FF;
const C_BTN_BORDER: u32 = 0x00_D9_DE_E6;

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

/// 由文件管理器打开时传入的图片路径（argv[1] 的 basename）。
static mut OPEN_NAME: [u8; 64] = [0; 64];
static mut OPEN_NAME_LEN: usize = 0;

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
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("图库".as_bytes());
    for i in 0..PHOTO_COUNT {
        let r = (i as i32) / COLS;
        let c = (i as i32) % COLS;
        let x = GRID_X + c * (TILE_W + GAP);
        let y = GRID_Y + r * (TILE_H + GAP);
        if y + TILE_H > DISPLAY_H as i32 {
            break;
        }
        // 照片网格单元容器 → 液态玻璃面板（照片内容原样画在其上）
        draw_glass(x, y, TILE_W, TILE_H, (TILE_H * 25 / 100).min(24));
        draw_photo(x, y, TILE_W, TILE_H, i);
    }
    draw_sys_nav();
}

fn render_view(idx: u32) {
    draw_gradient_bg(C_BG, glass_dark());
    // 标题栏：返回 + 序号（液态玻璃）
    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    let bs = BACK_SZ - 8;
    draw_glass(BACK_X, BACK_Y, bs, bs, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    let open = unsafe { (OPEN_NAME_LEN, &OPEN_NAME[..OPEN_NAME_LEN]) };
    if open.0 > 0 {
        draw_text(48, (TITLE_H - 8) / 2, open.1, C_TEXT);
    } else {
        let mut t = [0u8; 16];
        t[..7].copy_from_slice("照片 ".as_bytes());
        let mut n = 7;
        n = push_num(&mut t, n, idx + 1);
        draw_text(48, (TITLE_H - 8) / 2, &t[..n], C_TEXT);
    }

    // 大图（上下留边）：液态玻璃面板容器 + 照片内容原样
    draw_glass(20, 90, DISPLAY_W as i32 - 40, 560, 24);
    draw_photo(20, 90, DISPLAY_W as i32 - 40, 560, idx);

    // 上一张 / 下一张（液态玻璃按钮）
    draw_glass_button(30, 700, 96, 44, "上一张".as_bytes(), false);
    draw_glass_button(DISPLAY_W as i32 - 30 - 96, 700, 96, 44, "下一张".as_bytes(), false);

    // 底部提示
    let mut hint = [0u8; 32];
    hint[..4].copy_from_slice("共 ".as_bytes());
    let mut hn = 4usize;
    hn = push_num(&mut hint, hn, PHOTO_COUNT);
    if hn < hint.len() {
        hint[hn] = b' ';
        hn += 1;
    }
    let tail = "张照片".as_bytes();
    if hn + tail.len() <= hint.len() {
        hint[hn..hn + tail.len()].copy_from_slice(tail);
        hn += tail.len();
    }
    draw_text(16, DISPLAY_H as i32 - 60, &hint[..hn], C_DIM);

    draw_sys_nav();
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
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    print("[gallery] 启动中\n");
    if !fb_init() {
        print("[gallery] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[gallery] 就绪\n");

    // 文件管理器打开：argv[1] = 图片路径，取 basename 显示在查看页标题。
    if argc > 1 && !argv.is_null() {
        unsafe {
            let p = *argv.add(1) as *const u8;
            if !p.is_null() {
                let mut buf = [0u8; 64];
                let mut i = 0usize;
                while i < buf.len() - 1 {
                    let c = *p.add(i);
                    if c == 0 {
                        break;
                    }
                    buf[i] = c;
                    i += 1;
                }
                let mut start = 0usize;
                for j in 0..i {
                    if buf[j] == b'/' {
                        start = j + 1;
                    }
                }
                let n = i - start;
                if n > 0 {
                    OPEN_NAME[..n].copy_from_slice(&buf[start..i]);
                    OPEN_NAME[n] = 0;
                    OPEN_NAME_LEN = n;
                }
            }
        }
    }

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                if back_hit(x, y) {
                    print("[gallery] 返回 -> 退出\n");
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
                        print("[gallery] 打开照片 ");
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
                    } else {
                        show_error("已是第一张".as_bytes());
                    }
                } else if x >= DISPLAY_W as i32 - 126 && x < DISPLAY_W as i32 - 30 && y >= 700 && y < 744 {
                    if viewing + 1 < PHOTO_COUNT {
                        unsafe { VIEWING += 1; }
                        dirty = true;
                    } else {
                        show_error("已是最后一张".as_bytes());
                    }
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[gallery] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[gallery] ESC -> 退出\n");
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
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[gallery] 正在退出\n");
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
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: gallery\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
