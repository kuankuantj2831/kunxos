// KunxOS 音乐应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/music）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 正在播放：专辑封面（渐变方块）+ 曲名 + 歌手 + 播放进度条
//   3. 控制条：上一首 / 播放暂停 / 下一首（模拟播放：进度随时间推进）
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_music.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_COVER_BORDER: u32 = 0x00_3A_4A_6A;
const C_TEXT: u32 = 0x00_FF_FF_FF;
const C_ARTIST: u32 = 0x00_9A_A7_B5;
const C_DIM: u32 = 0x00_6B_78_8C;
const C_ACCENT: u32 = 0x00_E6_7E_22;
const C_TRACK_BG: u32 = 0x00_23_36_50;
const C_BTN: u32 = 0x00_23_36_50;
const C_BTN_BORDER: u32 = 0x00_4A_5A_7A;

const TRACK_LEN_SEC: u32 = 214; // 3:34

// ==================== 数据 ====================

struct Track {
    title: &'static str,
    artist: &'static str,
    color: u32,
}

static TRACKS: [Track; 5] = [
    Track { title: "晨光", artist: "KunxOS 乐团", color: 0x00_E6_7E_22 },
    Track { title: "晚风", artist: "KunxOS 乐团", color: 0x00_2E_86_C1 },
    Track { title: "星河", artist: "KunxOS 乐团", color: 0x00_9B_59_B6 },
    Track { title: "远山", artist: "KunxOS 乐团", color: 0x00_1A_BB_5E },
    Track { title: "夜航", artist: "KunxOS 乐团", color: 0x00_E7_4C_3C },
];

// ==================== 状态 ====================

static mut TRACK_IDX: usize = 0;
static mut PLAYING: bool = false;
static mut POS_SEC: u32 = 0;
static mut LAST_TICK: u64 = 0;

// ==================== 渲染 ====================

/// 专辑封面：渐变方块 + 音符装饰。
fn draw_cover(x: i32, y: i32, size: i32, color: u32) {
    let c2 = color.wrapping_add(0x00_20_20_40);
    for py in 0..size {
        let t = py as u32 * 255 / size as u32;
        let r = ((color >> 16) & 0xFF) + ((((c2 >> 16) & 0xFF) as i32 - ((color >> 16) & 0xFF) as i32) * t as i32 / 255) as u32;
        let g = ((color >> 8) & 0xFF) + ((((c2 >> 8) & 0xFF) as i32 - ((color >> 8) & 0xFF) as i32) * t as i32 / 255) as u32;
        let b = (color & 0xFF) + ((((c2 & 0xFF) as i32 - (color & 0xFF) as i32) * t as i32 / 255) as u32);
        for px in 0..size {
            put_pixel(x + px, y + py, 0x00_00_00_00 | ((r & 0xFF) << 16) | ((g & 0xFF) << 8) | (b & 0xFF));
        }
    }
    draw_rect(x, y, size, size, C_COVER_BORDER);
}

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("音乐".as_bytes());

    let idx = unsafe { TRACK_IDX };
    let track = &TRACKS[idx];

    // 封面
    let cover_x = (DISPLAY_W as i32 - 300) / 2;
    draw_cover(cover_x, 80, 300, track.color);
    // 音符装饰

    // 曲名 + 歌手
    draw_center(410, track.title.as_bytes(), C_TEXT);
    draw_center(440, track.artist.as_bytes(), C_ARTIST);

    // 进度条
    let bar_x = 60;
    let bar_w = DISPLAY_W as i32 - 120;
    let pos = unsafe { POS_SEC };
    let frac = pos.min(TRACK_LEN_SEC) as i32 * bar_w / TRACK_LEN_SEC as i32;
    fill_rect(bar_x, 500, bar_w, 8, C_TRACK_BG);
    fill_rect(bar_x, 500, frac, 8, C_ACCENT);
    // 时间
    let mut t = [0u8; 10];
    t[0] = b'0' + (pos / 60) as u8;
    t[1] = b':';
    t[2] = b'0' + ((pos % 60) / 10) as u8;
    t[3] = b'0' + (pos % 10) as u8;
    draw_text(bar_x, 520, &t[..4], C_DIM);
    let rem = TRACK_LEN_SEC - pos.min(TRACK_LEN_SEC);
    t[0] = b'0' + (rem / 60) as u8;
    t[1] = b':';
    t[2] = b'0' + ((rem % 60) / 10) as u8;
    t[3] = b'0' + (rem % 10) as u8;
    draw_text(bar_x + bar_w - 32, 520, &t[..4], C_DIM);

    // 控制条：上一首 / 播放暂停 / 下一首
    draw_btn(120, 580, "< |".as_bytes());
    let playing = unsafe { PLAYING };
    draw_btn(240, 580, if playing { "||".as_bytes() } else { "|>".as_bytes() });
    draw_btn(360, 580, "> |".as_bytes());

    // 播放列表
    draw_text(24, 680, "播放列表".as_bytes(), C_DIM);
    for (i, tr) in TRACKS.iter().enumerate() {
        let y = 710 + i as i32 * 44;
        if i == idx {
            draw_disc(32, y + 12, 4, C_ACCENT);
        }
        draw_text(48, y, tr.title.as_bytes(), if i == idx { C_ACCENT } else { C_TEXT });
        draw_text(320, y, tr.artist.as_bytes(), C_DIM);
    }

    draw_cursor();
}

fn draw_btn(x: i32, y: i32, label: &[u8]) {
    let w = 96;
    let h = 56;
    fill_rect(x + 3, y, w - 6, h, C_BTN);
    fill_rect(x, y + 3, w, h - 6, C_BTN);
    draw_rect(x, y, w, h, C_BTN_BORDER);
    let tx = x + (w - text_width(label)) / 2;
    draw_text(tx, y + 24, label, C_TEXT);
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[music] starting\n");
    if !fb_init() {
        print("[music] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[music] ready\n");

    unsafe { LAST_TICK = now_us(); }
    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[music] back -> exit\n");
                break;
            }
            if x >= 120 && x < 216 && y >= 580 && y < 636 {
                prev_track();
                dirty = true;
            } else if x >= 240 && x < 336 && y >= 580 && y < 636 {
                unsafe { PLAYING = !PLAYING; }
                print("[music] play/pause\n");
                dirty = true;
            } else if x >= 360 && x < 456 && y >= 580 && y < 636 {
                next_track();
                dirty = true;
            } else {
                // 点列表切歌
                for i in 0..TRACKS.len() {
                    let ty = 710 + i as i32 * 44;
                    if y >= ty && y < ty + 44 {
                        unsafe { TRACK_IDX = i; }
                        print("[music] select track ");
                        print_dec(i as u64);
                        print("\n");
                        dirty = true;
                        break;
                    }
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[music] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[music] ESC -> exit\n");
            break;
        }
        // 播放进度
        let now = now_us();
        let last = unsafe { LAST_TICK };
        if now.wrapping_sub(last) >= 1_000_000 {
            unsafe { LAST_TICK = now; }
            if unsafe { PLAYING } {
                unsafe {
                    POS_SEC += 1;
                    if POS_SEC >= TRACK_LEN_SEC {
                        next_track();
                    }
                }
                dirty = true;
            }
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(32);
    }

    print("[music] exiting\n");
    unsafe { exit(0); }
}

fn prev_track() {
    unsafe {
        TRACK_IDX = if TRACK_IDX == 0 { TRACKS.len() - 1 } else { TRACK_IDX - 1 };
        POS_SEC = 0;
    }
    print("[music] prev\n");
}

fn next_track() {
    unsafe {
        TRACK_IDX = (TRACK_IDX + 1) % TRACKS.len();
        POS_SEC = 0;
    }
    print("[music] next\n");
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
        write_fd(2, b"PANIC: music\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
