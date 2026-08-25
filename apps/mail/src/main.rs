// KunxOS 邮件应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/mail）。职责：
//   1. 打开 /dev/fb0（openat）并 mmap 到用户空间，直接写入 RGBA8888 像素
//   2. 收件箱列表：发件人 + 主题 + 时间，未读高亮圆点
//   3. 点邮件进入正文视图：发件人/主题/正文折行渲染
//   4. 点击左上角「返回」或按 ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_mail.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::*;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_10_18_28;
const C_ROW: u32 = 0x00_1B_2B_4A;
const C_ROW_BORDER: u32 = 0x00_2A_3A_5A;
const C_SENDER: u32 = 0x00_FF_FF_FF;
const C_SUBJECT: u32 = 0x00_9A_A7_B5;
const C_TIME: u32 = 0x00_6B_78_8C;
const C_UNREAD: u32 = 0x00_22_C5_5E;
const C_BODY: u32 = 0x00_BB_C5_D0;
const C_DIM2: u32 = 0x00_6B_78_8C;

const LIST_TOP: i32 = 56;
const ROW_H: i32 = 76;

// ==================== 数据 ====================

struct Mail {
    sender: &'static str,
    subject: &'static str,
    time: &'static str,
    body: &'static str,
    unread: bool,
}

static MAILS: [Mail; 6] = [
    Mail { sender: "KunxOS 团队", subject: "系统更新 1.0 发布", time: "09:00", unread: true, body: "KunxOS 1.0 已发布。本次更新带来全新动画系统、应用商店、OOBE 向导与磁盘加密支持。请尽快升级。" },
    Mail { sender: "应用商店", subject: "本周精选应用", time: "08:30", unread: true, body: "本周精选：文件管理器、日历、时钟、音乐。所有应用均可在桌面启动器直接打开。" },
    Mail { sender: "系统通知", subject: "存储空间告警", time: "昨天", unread: false, body: "磁盘剩余空间不足 10%。建议清理 /data 下的临时文件以释放空间。" },
    Mail { sender: "KunxOS 团队", subject: "开发者预览版说明", time: "昨天", unread: false, body: "开发者预览版仅用于评估，请勿用于生产环境。已知问题详见 BUG_REPORT.md。" },
    Mail { sender: "网络守护", subject: "网络配置已更新", time: "周一", unread: false, body: "networkd 已重新加载配置。当前使用 slirp 用户态网络，IP 由 QEMU 自动分配。" },
    Mail { sender: "安全中心", subject: "安全更新提示", time: "周一", unread: false, body: "建议定期检查安全更新。磁盘加密已启用，/data 分区以 XOR 流密码保护。" },
];

// ==================== 状态 ====================

/// u32::MAX = 列表；否则为打开的邮件下标。
static mut VIEWING: u32 = u32::MAX;

// ==================== 渲染 ====================

fn render_list() {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    draw_title_bar("邮件".as_bytes());
    for (i, m) in MAILS.iter().enumerate() {
        let y = LIST_TOP + i as i32 * ROW_H;
        if y + ROW_H > DISPLAY_H as i32 {
            break;
        }
        fill_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 8, C_ROW);
        draw_rect(12, y, DISPLAY_W as i32 - 24, ROW_H - 8, C_ROW_BORDER);
        if m.unread {
            draw_disc(26, y + ROW_H / 2 - 4, 5, C_UNREAD);
        }
        draw_text(44, y + 12, m.sender.as_bytes(), C_SENDER);
        draw_text(44, y + 38, m.subject.as_bytes(), C_SUBJECT);
        draw_text(DISPLAY_W as i32 - 16 - text_width(m.time.as_bytes()), y + 12, m.time.as_bytes(), C_TIME);
    }
    draw_cursor();
}

fn render_view(idx: usize) {
    fill_rect(0, 0, DISPLAY_W as i32, DISPLAY_H as i32, C_BG);
    let m = &MAILS[idx];

    // 标题栏：返回 + 主题
    fill_rect(0, 0, DISPLAY_W as i32, TITLE_H, C_TITLE_BG);
    fill_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN);
    draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, C_BTN_BORDER);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, m.subject.as_bytes(), C_TEXT);

    // 发件人 + 时间
    draw_text(24, 72, "发件人: ".as_bytes(), C_DIM2);
    draw_text(84, 72, m.sender.as_bytes(), C_TEXT);
    draw_text(24, 100, m.time.as_bytes(), C_DIM2);
    draw_line(16, 120, DISPLAY_W as i32 - 16, 120, C_ROW_BORDER);

    // 正文折行
    let mut y = 144i32;
    let mut line = [0u8; 96];
    let mut n = 0;
    for &c in m.body.as_bytes() {
        let cw = if c >= 0x80 { 16 } else { 8 };
        if n > 0 && text_width(&line[..n]) + cw > DISPLAY_W as i32 - 48 {
            draw_text(24, y, &line[..n], C_BODY);
            y += 28;
            n = 0;
        }
        line[n] = c;
        n += 1;
    }
    if n > 0 {
        draw_text(24, y, &line[..n], C_BODY);
    }

    draw_text(16, DISPLAY_H as i32 - 60, "返回键回到收件箱".as_bytes(), C_DIM2);
    draw_cursor();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[mail] starting\n");
    if !fb_init() {
        print("[mail] /dev/fb0 mmap failed, no framebuffer\n");
        unsafe { exit(1); }
    }
    print("[mail] ready\n");

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
                    print("[mail] back -> exit\n");
                    break;
                }
                let row = (y - LIST_TOP) / ROW_H;
                if row >= 0 && (row as usize) < MAILS.len() {
                    unsafe { VIEWING = row as u32; }
                    print("[mail] open mail ");
                    print_dec(row as u64);
                    print("\n");
                    dirty = true;
                }
            } else {
                if back_hit(x, y) {
                    unsafe { VIEWING = u32::MAX; }
                    dirty = true;
                }
            }
        }
        match ev.key {
            KeyAction::Esc => {
                print("[mail] ESC -> exit\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[mail] ESC -> exit\n");
            break;
        }
        if dirty {
            let viewing = unsafe { VIEWING };
            if viewing == u32::MAX {
                render_list();
            } else {
                render_view(viewing as usize);
            }
            flip();
            dirty = false;
        }
        sleep_ms(16);
    }

    print("[mail] exiting\n");
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
        write_fd(2, b"PANIC: mail\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
