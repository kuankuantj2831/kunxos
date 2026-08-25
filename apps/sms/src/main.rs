// KunxOS 短信应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/sms）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 会话列表：联系人 + 最后一条消息预览 + 时间
//   3. 点会话进入对话视图：气泡消息（收左发右），点返回键回到列表
//   4. 点击左上角「返回」或按 ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_sms.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_ROW: u32 = 0x00_1B_2B_4A;
const C_ROW_BORDER: u32 = 0x00_2A_3A_5A;
const C_NAME: u32 = 0x00_FF_FF_FF;
const C_PREVIEW: u32 = 0x00_9A_A7_B5;
const C_TIME: u32 = 0x00_6B_78_8C;
const C_AVATAR: u32 = 0x00_2E_86_C1;
const C_BUBBLE_IN: u32 = 0x00_23_36_50;
const C_BUBBLE_OUT: u32 = 0x00_1A_AB_5E;
const C_BUBBLE_TEXT: u32 = 0x00_FF_FF_FF;

const LIST_TOP: i32 = 56;
const ROW_H: i32 = 84;

// ==================== 数据 ====================

struct Conversation {
    name: &'static str,
    name_zh: &'static str,
    preview: &'static str,
    time: &'static str,
    color: u32,
}

static CONVERSATIONS: [Conversation; 6] = [
    Conversation { name: "Alice", name_zh: "阿丽", preview: "晚上一起吃饭吗？", time: "09:41", color: 0x00_E7_4C_3C },
    Conversation { name: "Bob",   name_zh: "小波", preview: "好的，明天见！",   time: "08:15", color: 0x00_2E_86_C1 },
    Conversation { name: "Carol", name_zh: "小悦", preview: "文件我已经发你了", time: "昨天",  color: 0x00_9B_59_B6 },
    Conversation { name: "Dave",  name_zh: "大伟", preview: "[图片]",           time: "昨天",  color: 0x00_1A_BB_5E },
    Conversation { name: "Eve",   name_zh: "小伊", preview: "周末去爬山吗？",   time: "周一",  color: 0x00_F9_73_16 },
    Conversation { name: "系统",  name_zh: "系统", preview: "系统更新完成",     time: "周一",  color: 0x00_5D_6D_7E },
];

/// 会话 i 的对话（交替：收/发）。
struct Msg {
    out: bool,
    text: &'static str,
}

static THREADS: [[Msg; 4]; 6] = [
    [
        Msg { out: false, text: "晚上一起吃饭吗？" },
        Msg { out: true,  text: "好呀，几点？" },
        Msg { out: false, text: "七点，老地方。" },
        Msg { out: true,  text: "没问题！" },
    ],
    [
        Msg { out: false, text: "方案收到了吗？" },
        Msg { out: true,  text: "收到了，我看一下" },
        Msg { out: false, text: "好的，明天见！" },
        Msg { out: true,  text: "明天见" },
    ],
    [
        Msg { out: false, text: "文件我已经发你了" },
        Msg { out: true,  text: "收到，谢谢！" },
        Msg { out: false, text: "不客气" },
        Msg { out: true,  text: "嗯嗯" },
    ],
    [
        Msg { out: false, text: "[图片]" },
        Msg { out: true,  text: "拍得真不错！" },
        Msg { out: false, text: "哈哈谢谢" },
        Msg { out: true,  text: "下次一起去" },
    ],
    [
        Msg { out: false, text: "周末去爬山吗？" },
        Msg { out: true,  text: "去！几点出发？" },
        Msg { out: false, text: "早上八点" },
        Msg { out: true,  text: "好，不见不散" },
    ],
    [
        Msg { out: false, text: "系统更新完成" },
        Msg { out: true,  text: "已重启完成" },
        Msg { out: false, text: "请查看更新日志" },
        Msg { out: true,  text: "好的" },
    ],
];

// ==================== 状态 ====================

/// u32::MAX = 列表视图；否则为打开的会话下标。
static mut VIEWING: u32 = u32::MAX;
static mut SCROLL: i32 = 0;

// ==================== 渲染 ====================

fn render_list() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("短信".as_bytes());
    for (i, c) in CONVERSATIONS.iter().enumerate() {
        let y = LIST_TOP + i as i32 * ROW_H + unsafe { SCROLL };
        if y + ROW_H <= LIST_TOP || y >= DISPLAY_H as i32 {
            continue;
        }
        fill_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 10, C_ROW);
        draw_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 10, C_ROW_BORDER);
        // 头像
        draw_disc(44, y + ROW_H / 2 - 5, 20, c.color);
        let initial = c.name.as_bytes()[0];
        draw_char(40, y + ROW_H / 2 - 14, initial, C_TEXT);
        // 名字 + 预览 + 时间
        draw_text(76, y + 14, c.name.as_bytes(), C_NAME);
        draw_text(76, y + 40, c.preview.as_bytes(), C_PREVIEW);
        draw_text(DISPLAY_W as i32 - 16 - text_width(c.time.as_bytes()), y + 14, c.time.as_bytes(), C_TIME);
    }
    draw_cursor();
}

fn render_thread(idx: usize) {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    let title = CONVERSATIONS[idx].name.as_bytes();
    // 标题栏：返回 + 联系人名
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, title, C_TEXT);

    let thread = &THREADS[idx];
    let mut y = 72i32;
    for m in thread {
        let w = text_width(m.text.as_bytes()) + 24;
        let bubble_w = w.max(40);
        if m.out {
            let x = DISPLAY_W as i32 - bubble_w - 16;
            fill_rect(x + 4, y, bubble_w - 8, 40, C_BUBBLE_OUT);
            fill_rect(x, y + 4, bubble_w, 32, C_BUBBLE_OUT);
            fill_rect(x + 4, y + 4, bubble_w - 8, 32, C_BUBBLE_OUT);
            draw_text(x + 12, y + 16, m.text.as_bytes(), C_BUBBLE_TEXT);
        } else {
            let x = 16;
            fill_rect(x + 4, y, bubble_w - 8, 40, C_BUBBLE_IN);
            fill_rect(x, y + 4, bubble_w, 32, C_BUBBLE_IN);
            fill_rect(x + 4, y + 4, bubble_w - 8, 32, C_BUBBLE_IN);
            draw_text(x + 12, y + 16, m.text.as_bytes(), C_BUBBLE_TEXT);
        }
        y += 52;
    }
    draw_cursor();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[sms] starting\n");
    if !fb_init() {
        print("[sms] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[sms] ready\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            dirty = true;
        }
        if let Some((x, y)) = ev.tap {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                // 列表视图
                if back_hit(x, y) {
                    print("[sms] back -> exit\n");
                    break;
                }
                let row = (y - LIST_TOP - unsafe { SCROLL }) / ROW_H;
                if row >= 0 && (row as usize) < CONVERSATIONS.len() {
                    unsafe { VIEWING = row as u32; }
                    print("[sms] open thread ");
                    print_dec(row as u64);
                    print("\n");
                    dirty = true;
                }
            } else {
                // 会话视图：返回键回列表
                if back_hit(x, y) {
                    unsafe { VIEWING = u32::MAX; }
                    dirty = true;
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[sms] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[sms] ESC -> exit\n");
            break;
        }
        if dirty {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                render_list();
            } else {
                render_thread(viewing as usize);
            }
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[sms] exiting\n");
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
        write_fd(2, b"PANIC: sms\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
