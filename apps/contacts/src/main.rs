// KunxOS 通讯录应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/contacts）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 联系人列表：头像（首字母圆）+ 姓名 + 号码
//   3. 点联系人进入详情：大头像、姓名、号码、邮箱 + 模拟呼叫按钮
//   4. 点击左上角「返回」或按 ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_contacts.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_ROW: u32 = 0x00_1B_2B_4A;
const C_ROW_BORDER: u32 = 0x00_2A_3A_5A;
const C_NAME: u32 = 0x00_FF_FF_FF;
const C_NUM: u32 = 0x00_9A_A7_B5;
const C_AVATAR: u32 = 0x00_2E_86_C1;
const C_ACCENT: u32 = 0x00_1A_BB_5E;
const C_CALL: u32 = 0x00_1A_BB_5E;
const C_CALL_BORDER: u32 = 0x00_2E_CC_6E;
const C_DIM2: u32 = 0x00_6B_78_8C;

const LIST_TOP: i32 = 56;
const ROW_H: i32 = 76;

// ==================== 数据 ====================

struct Contact {
    name: &'static str,
    name_zh: &'static str,
    phone: &'static str,
    email: &'static str,
    color: u32,
}

static CONTACTS: [Contact; 8] = [
    Contact { name: "Alice", name_zh: "阿丽", phone: "138 0000 0001", email: "alice@kunxos.dev", color: 0x00_E7_4C_3C },
    Contact { name: "Bob",   name_zh: "小波", phone: "138 0000 0002", email: "bob@kunxos.dev",   color: 0x00_2E_86_C1 },
    Contact { name: "Carol", name_zh: "小悦", phone: "138 0000 0003", email: "carol@kunxos.dev", color: 0x00_9B_59_B6 },
    Contact { name: "Dave",  name_zh: "大伟", phone: "138 0000 0004", email: "dave@kunxos.dev",  color: 0x00_1A_BB_5E },
    Contact { name: "Eve",   name_zh: "小伊", phone: "138 0000 0005", email: "eve@kunxos.dev",   color: 0x00_F9_73_16 },
    Contact { name: "Frank", name_zh: "阿强", phone: "138 0000 0006", email: "frank@kunxos.dev", color: 0x00_27_AE_60 },
    Contact { name: "Grace", name_zh: "小雅", phone: "138 0000 0007", email: "grace@kunxos.dev", color: 0x00_AF_7A_C5 },
    Contact { name: "Henry", name_zh: "小汉", phone: "138 0000 0008", email: "henry@kunxos.dev", color: 0x00_E6_7E_22 },
];

// ==================== 状态 ====================

/// u32::MAX = 列表视图；否则为详情联系人下标。
static mut VIEWING: u32 = u32::MAX;

// ==================== 渲染 ====================

fn draw_avatar(cx: i32, cy: i32, r: i32, color: u32, ch: u8) {
    draw_disc(cx, cy, r, color);
    draw_circle(cx, cy, r, 0x00_FF_FF_FF);
    draw_char(cx - 4, cy - 4, ch, C_TEXT);
}

fn render_list() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("通讯录".as_bytes());
    for (i, c) in CONTACTS.iter().enumerate() {
        let y = LIST_TOP + i as i32 * ROW_H;
        if y + ROW_H > DISPLAY_H as i32 {
            break;
        }
        fill_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 8, C_ROW);
        draw_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 8, C_ROW_BORDER);
        let initial = c.name.as_bytes()[0];
        draw_avatar(44, y + ROW_H / 2 - 4, 18, c.color, initial);
        draw_text(76, y + 14, c.name.as_bytes(), C_NAME);
        draw_text(76, y + 40, c.phone.as_bytes(), C_NUM);
    }
    draw_cursor();
}

fn render_detail(idx: usize) {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    let c = &CONTACTS[idx];

    // 标题栏：返回 + 联系人名
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, c.name.as_bytes(), C_TEXT);

    // 大头像
    let initial = c.name.as_bytes()[0];
    draw_avatar((DISPLAY_W / 2) as i32, 190, 60, c.color, initial);

    // 姓名（居中）
    draw_center(280, c.name.as_bytes(), C_TEXT);

    // 号码与邮箱行
    draw_text(60, 340, "手机".as_bytes(), C_DIM2);
    draw_text(120, 340, c.phone.as_bytes(), C_TEXT);
    draw_text(60, 380, "邮箱".as_bytes(), C_DIM2);
    draw_text(120, 380, c.email.as_bytes(), C_TEXT);

    // 呼叫按钮
    let (bx, by, bw, bh) = (170, 460, 200, 64);
    fill_rect(bx + 4, by, bw - 8, bh, C_CALL);
    fill_rect(bx, by + 4, bw, bh - 8, C_CALL);
    draw_rect(bx, by, bw, bh, C_CALL_BORDER);
    let tx = bx + (bw - text_width("呼叫".as_bytes())) / 2;
    draw_text(tx, by + 26, "呼叫".as_bytes(), C_TEXT);

    draw_text(16, DISPLAY_H as i32 - 60, "返回键回到列表".as_bytes(), C_DIM2);
    draw_cursor();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[contacts] starting\n");
    if !fb_init() {
        print("[contacts] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[contacts] ready\n");

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
                    print("[contacts] back -> exit\n");
                    break;
                }
                let row = (y - LIST_TOP) / ROW_H;
                if row >= 0 && (row as usize) < CONTACTS.len() {
                    unsafe { VIEWING = row as u32; }
                    print("[contacts] open detail ");
                    print_dec(row as u64);
                    print("\n");
                    dirty = true;
                }
            } else {
                if back_hit(x, y) {
                    unsafe { VIEWING = u32::MAX; }
                    dirty = true;
                } else if x >= 170 && x < 370 && y >= 460 && y < 524 {
                    print("[contacts] call ");
                    print(CONTACTS[viewing as usize].name);
                    print("\n");
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[contacts] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[contacts] ESC -> exit\n");
            break;
        }
        if dirty {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                render_list();
            } else {
                render_detail(viewing as usize);
            }
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[contacts] exiting\n");
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
        write_fd(2, b"PANIC: contacts\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
