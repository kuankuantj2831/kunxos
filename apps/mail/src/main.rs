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

const C_BG: u32 = 0x00_F7_F8_FA;
const C_ROW: u32 = 0x00_FF_FF_FF;
const C_ROW_BORDER: u32 = 0x00_E5_E9_F0;
const C_SENDER: u32 = 0x00_1A_1C_22;
const C_SUBJECT: u32 = 0x00_6B_72_80;
const C_TIME: u32 = 0x00_6B_72_80;
const C_UNREAD: u32 = 0x00_00_B9_6B;
const C_BODY: u32 = 0x00_1A_1C_22;
const C_DIM2: u32 = 0x00_6B_72_80;

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
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("邮件".as_bytes());
    for (i, m) in MAILS.iter().enumerate() {
        let y = LIST_TOP + i as i32 * ROW_H;
        if y + ROW_H > display_h() as i32 {
            break;
        }
        // 邮件行：液态玻璃卡片（r = min(68*25/100, 24) = 17）
        draw_glass(12, y, display_w() as i32 - 24, ROW_H - 8, 17);
        if m.unread {
            draw_disc(26, y + ROW_H / 2 - 4, 5, C_UNREAD);
        }
        draw_text(44, y + 12, m.sender.as_bytes(), C_SENDER);
        draw_text(44, y + 38, m.subject.as_bytes(), C_SUBJECT);
        draw_text(display_w() as i32 - 16 - text_width(m.time.as_bytes()), y + 12, m.time.as_bytes(), C_TIME);
    }
    draw_sys_nav();
}

fn render_view(idx: usize) {
    draw_gradient_bg(C_BG, glass_dark());
    let m = &MAILS[idx];

    // 标题栏：返回 + 主题（玻璃，与 draw_title_bar 同款）
    draw_glass(0, 0, display_w() as i32, TITLE_H, 0);
    draw_glass(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, m.subject.as_bytes(), C_TEXT);

    // 邮件内容面板（液态玻璃，覆盖内容区到导航栏之上）
    draw_glass(16, 64, display_w() as i32 - 32, content_bottom() - 64, 24);

    // 发件人 + 时间
    draw_text(24, 72, "发件人: ".as_bytes(), C_DIM2);
    draw_text(84, 72, m.sender.as_bytes(), C_TEXT);
    draw_text(24, 100, m.time.as_bytes(), C_DIM2);
    draw_line(16, 120, display_w() as i32 - 16, 120, C_ROW_BORDER);

    // 正文折行
    let mut y = 144i32;
    let mut line = [0u8; 96];
    let mut n = 0;
    for &c in m.body.as_bytes() {
        let cw = if c >= 0x80 { 16 } else { 8 };
        // ★ 缓冲写满先落一行：折行按像素宽度判定，窄字符（'i'/'l'/空格等
        // TTF 比例字形）96 字节可能仍不足一行宽 —— 无守卫时 line[96] 越界。
        if n >= line.len() {
            draw_text(24, y, &line[..n], C_BODY);
            y += 28;
            n = 0;
        }
        if n > 0 && text_width(&line[..n]) + cw > display_w() as i32 - 48 {
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

    draw_text(16, display_h() as i32 - 60, "返回键回到收件箱".as_bytes(), C_DIM2);
    draw_sys_nav();
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[mail] 启动中\n");
    if !fb_init() {
        print("[mail] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[mail] 就绪\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.resized { dirty = true; }
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
                    print("[mail] 返回 -> 退出\n");
                    break;
                }
                let row = (y - LIST_TOP) / ROW_H;
                if row >= 0 && (row as usize) < MAILS.len() {
                    unsafe { VIEWING = row as u32; }
                    print("[mail] 打开邮件 ");
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
                print("[mail] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[mail] ESC -> 退出\n");
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
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[mail] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
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
        write_fd(2, b"PANIC: mail\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
