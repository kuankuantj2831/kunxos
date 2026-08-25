// KunxOS 电话应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/phone）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 拨号界面：号码显示屏 + 3x4 键盘（1-9 * 0 #）+ 删除/呼叫键
//   3. 呼叫流程：拨号 → 呼叫（模拟接通动画）→ 挂断；键盘可触屏或 virtio-keyboard
//   4. 点击左上角「返回」或按 ESC 退出（exit(0)），由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_phone.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_DISP_BG: u32 = 0x00_16_22_38;
const C_DISP_BORDER: u32 = 0x00_3A_4A_6A;
const C_KEY: u32 = 0x00_23_36_50;
const C_KEY_BORDER: u32 = 0x00_4A_5A_7A;
const C_CALL: u32 = 0x00_1A_BB_5E;
const C_CALL_BORDER: u32 = 0x00_2E_CC_6E;
const C_END: u32 = 0x00_E7_4C_3C;
const C_END_BORDER: u32 = 0x00_F7_5C_4C;
const C_DEL: u32 = 0x00_9A_A7_B5;

const KEY_ROWS: i32 = 4;
const KEY_COLS: i32 = 3;
const KEY_W: i32 = 96;
const KEY_H: i32 = 72;
const KEY_GAP: i32 = 14;
const PAD_X: i32 = (DISPLAY_W as i32 - (KEY_COLS * KEY_W + (KEY_COLS - 1) * KEY_GAP)) / 2;
const PAD_Y: i32 = 560;

// ==================== 状态 ====================

static mut NUMBER: [u8; 16] = [0; 16];
static mut NUMBER_LEN: usize = 0;
static mut CALLING: bool = false;
static mut CALL_TICK: u32 = 0;

// ==================== 键盘 ====================

/// 3x4 键盘字符（'*' 与 '#' 用 ASCII 渲染）。
const KEYS: [[u8; 3]; 4] = [
    [b'1', b'2', b'3'],
    [b'4', b'5', b'6'],
    [b'7', b'8', b'9'],
    [b'*', b'0', b'#'],
];

fn key_rect(row: i32, col: i32) -> (i32, i32, i32, i32) {
    (
        PAD_X + col * (KEY_W + KEY_GAP),
        PAD_Y + row * (KEY_H + KEY_GAP),
        KEY_W,
        KEY_H,
    )
}

/// 返回按键所在 (row, col)；非键盘区返回 None。
fn key_hit(x: i32, y: i32) -> Option<(i32, i32)> {
    for r in 0..KEY_ROWS {
        for c in 0..KEY_COLS {
            let (kx, ky, kw, kh) = key_rect(r, c);
            if x >= kx && x < kx + kw && y >= ky && y < ky + kh {
                return Some((r, c));
            }
        }
    }
    None
}

// ==================== 渲染 ====================

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

fn draw_key(r: i32, c: i32) {
    let (kx, ky, kw, kh) = key_rect(r, c);
    let ch = KEYS[r as usize][c as usize];
    fill_rect(kx + 4, ky, kw - 8, kh, C_KEY);
    fill_rect(kx, ky + 4, kw, kh - 8, C_KEY);
    fill_rect(kx + 4, ky + 4, kw - 8, kh - 8, C_KEY);
    draw_rect(kx, ky, kw, kh, C_KEY_BORDER);
    let tx = kx + (kw - 8) / 2;
    let ty = ky + (kh - 8) / 2 - 4;
    draw_char_scaled(tx, ty, ch, 1, C_TEXT);
}

fn draw_center_in(y: i32, s: &[u8], color: u32, box_x: i32, box_w: i32) {
    let tx = box_x + (box_w - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

fn render() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("电话".as_bytes());

    // 号码显示屏
    fill_rect(16, 64, DISPLAY_W as i32 - 32, 100, C_DISP_BG);
    draw_rect(16, 64, DISPLAY_W as i32 - 32, 100, C_DISP_BORDER);
    let calling = unsafe { CALLING };
    if calling {
        draw_text(32, 98, "呼叫中...".as_bytes(), C_ACCENT);
    } else if unsafe { NUMBER_LEN } == 0 {
        draw_text(32, 126, "输入号码".as_bytes(), C_DIM);
    }
    let number = unsafe { &NUMBER[..NUMBER_LEN] };
    draw_text(32, 124, number, C_TEXT);

    // 键盘
    for r in 0..KEY_ROWS {
        for c in 0..KEY_COLS {
            draw_key(r, c);
        }
    }

    // 删除（退格）与呼叫 / 挂断
    let (kx, ky, kw, kh) = (16, PAD_Y - 96, 130, 64);
    fill_rect(kx + 4, ky, kw - 8, kh, C_KEY);
    fill_rect(kx, ky + 4, kw, kh - 8, C_KEY);
    draw_rect(kx, ky, kw, kh, C_KEY_BORDER);
    draw_text(kx + 34, ky + 26, "删除".as_bytes(), C_DEL);

    let (hx, hy, hw, hh) = (DISPLAY_W as i32 - 146, PAD_Y - 96, 130, 64);
    if calling {
        fill_rect(hx + 4, hy, hw - 8, hh, C_END);
        fill_rect(hx, hy + 4, hw, hh - 8, C_END);
        draw_rect(hx, hy, hw, hh, C_END_BORDER);
        draw_center_in(hy + 26, "挂断".as_bytes(), C_TEXT, hx, hw);
    } else {
        fill_rect(hx + 4, hy, hw - 8, hh, C_CALL);
        fill_rect(hx, hy + 4, hw, hh - 8, C_CALL);
        draw_rect(hx, hy, hw, hh, C_CALL_BORDER);
        draw_center_in(hy + 26, "呼叫".as_bytes(), C_TEXT, hx, hw);
    }

    // 底部提示
    draw_text(16, DISPLAY_H as i32 - 60, "支持键盘输入数字，回车拨号".as_bytes(), C_DIM);

    draw_cursor();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[phone] starting\n");
    if !fb_init() {
        print("[phone] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[phone] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[phone] back -> exit\n");
                break;
            }
            let calling = unsafe { CALLING };
            if calling {
                if x >= DISPLAY_W as i32 - 146 && x < DISPLAY_W as i32 - 16
                    && y >= PAD_Y - 96 && y < PAD_Y - 32 {
                    print("[phone] hang up\n");
                    unsafe { CALLING = false; }
                    dirty = true;
                }
            } else {
                if x >= 16 && x < 146 && y >= PAD_Y - 96 && y < PAD_Y - 32 {
                    // 删除
                    if unsafe { NUMBER_LEN } > 0 {
                        unsafe { NUMBER_LEN -= 1; }
                        dirty = true;
                    }
                } else if x >= DISPLAY_W as i32 - 146 && x < DISPLAY_W as i32 - 16
                    && y >= PAD_Y - 96 && y < PAD_Y - 32 {
                    // 呼叫
                    if unsafe { NUMBER_LEN } > 0 {
                        print("[phone] calling ");
                        let number = unsafe { &NUMBER[..NUMBER_LEN] };
                        unsafe { write_fd(1, number); }
                        print("\n");
                        unsafe {
                            CALLING = true;
                            CALL_TICK = 0;
                        }
                        dirty = true;
                    }
                } else if let Some((r, c)) = key_hit(x, y) {
                    let ch = KEYS[r as usize][c as usize];
                    if unsafe { NUMBER_LEN < NUMBER.len() } {
                        unsafe {
                            NUMBER[NUMBER_LEN] = ch;
                            NUMBER_LEN += 1;
                        }
                        dirty = true;
                    }
                }
            }
        }
        // 键盘输入：数字追加、退格删除、回车拨号
        match ev.key {
            KeyAction::Char(c) => {
                let calling = unsafe { CALLING };
                if !calling && c.is_ascii_digit() && unsafe { NUMBER_LEN < NUMBER.len() } {
                    unsafe {
                        NUMBER[NUMBER_LEN] = c;
                        NUMBER_LEN += 1;
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                if !unsafe { CALLING } && unsafe { NUMBER_LEN } > 0 {
                    unsafe { NUMBER_LEN -= 1; }
                    dirty = true;
                }
            }
            KeyAction::Enter => {
                if !unsafe { CALLING } && unsafe { NUMBER_LEN } > 0 {
                    unsafe {
                        CALLING = true;
                        CALL_TICK = 0;
                    }
                    dirty = true;
                }
            }
            KeyAction::Esc => {
                print("[phone] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[phone] ESC -> exit\n");
            break;
        }
        if unsafe { CALLING } {
            unsafe { CALL_TICK += 1; }
            if unsafe { CALL_TICK } % 30 == 0 {
                dirty = true; // 动画帧
            }
        }
        if dirty {
            render();
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[phone] exiting\n");
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
        write_fd(2, b"PANIC: phone\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
