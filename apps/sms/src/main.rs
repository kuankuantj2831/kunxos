// KunxOS 短信应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/sms）。职责（本地数据管理版）：
//   1. SmsStore（store.rs）：/data/sms/ 会话级本地库
//      - 会话索引 index.dat（按联系人分组、按最后消息时间倒序）
//      - 每会话一个 conv<N>.dat（消息：文本、时间戳、已读、方向、发送状态）
//      - watermark.dat 水位（增量导入 rild 新收件，去重）
//   2. 会话视图（气泡：收左发右）+ 回复输入；发件气泡带状态标签
//      （草稿/发送中/失败）
//   3. 新建短信（号码 + 内容，键盘输入，回车发送）
//   4. 发送流程（modem.rs）：检查 rild.state SIM → 无 SIM 存草稿并提示
//      「无 SIM 卡」，有 SIM 写 SEND 命令并轮询 rild.state 推进
//      发送中 → 已发送/失败
//   5. 接收流程：轮询 rild.state INBOX 计数变化（RIL 推送）→ 导入
//      /data/rild/inbox.dat 新记录到 /data/sms/ → 标记 UI 重绘（通知 UI）
//   6. 通讯录集成：/data/contacts.dat 号码 → 姓名

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

mod store;
mod modem;

use crate::store::{Conv, SmsStatus, CONVS, CONV_N, MSGS, MSG_N};

// ==================== 常量 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_CLOSE: usize = 57;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;

const CONTACTS_PATH: &str = "/data/contacts.dat";

const C_BG: u32 = 0x00_F7_F8_FA;
const C_NAME: u32 = 0x00_1A_1C_22;
const C_PREVIEW: u32 = 0x00_6B_72_80;
const C_TIME: u32 = 0x00_6B_72_80;
const C_AVATAR: u32 = 0x00_2E_86_C1;
/// 彩色表面（蓝色头像、红色角标、绿色强调气泡）上的文字保持白色。
const C_ON_COLOR: u32 = 0x00_FF_FF_FF;
const C_BUBBLE_TEXT: u32 = 0x00_FF_FF_FF;
/// 收件气泡（浅色玻璃）上的文字用深色。
const C_BUBBLE_TEXT_IN: u32 = 0x00_1A_1C_22;
const C_BUBBLE_IN: u32 = 0x00_FF_FF_FF;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_END_COLOR: u32 = 0x00_E7_4C_3C;

const LIST_TOP: i32 = 56;
const ROW_H: i32 = 84;

// ==================== 视图状态 ====================

/// u32::MAX = 列表；0xFE = 新建短信；其余 = 会话下标（线程视图）。
static mut VIEW: u32 = u32::MAX;
static mut SCROLL: i32 = 0;
/// 输入缓冲（号码 / 文本）。
static mut INPUT: [u8; 64] = [0; 64];
static mut INPUT_LEN: usize = 0;
static mut INPUT_NUM: [u8; 20] = [0; 20];
static mut INPUT_NUM_LEN: usize = 0;
/// 输入阶段：0 = 号码，1 = 内容。
static mut INPUT_STAGE: u8 = 0;
/// 状态提示条（如「无 SIM 卡，已存草稿」）。
static mut NOTICE: [u8; 48] = [0; 48];
static mut NOTICE_LEN: usize = 0;
static mut NOTICE_TILL: u64 = 0;

// ==================== 文件工具 ====================

fn read_file(path: &str, buf: &mut [u8]) -> usize {
    unsafe {
        let fd = kit::open(path, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        // ★ 按 isize 判负：read 失败 -errno 的 usize 原值是 usize::MAX，
        // `n > 0` 拦不住 → 调用方按天文长度切片 panic（wifid 同族）。
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        if n > 0 {
            (n as usize).min(buf.len())
        } else {
            0
        }
    }
}

fn set_notice(s: &[u8]) {
    unsafe {
        let l = s.len().min(NOTICE.len());
        NOTICE[..l].copy_from_slice(&s[..l]);
        NOTICE_LEN = l;
        NOTICE_TILL = crate::kit::now_us() + 2_000_000;
    }
}

fn notice_active() -> bool {
    unsafe { NOTICE_LEN > 0 && crate::kit::now_us() < NOTICE_TILL }
}

// ==================== 通讯录 ====================

fn contact_name(num: &[u8]) -> &'static [u8] {
    static mut BUF: [u8; 2048] = [0; 2048];
    static mut NAME: [u8; 24] = [0; 24];
    unsafe {
        let n = read_file(CONTACTS_PATH, &mut BUF);
        if n == 0 {
            return &[];
        }
        let mut da = [0u8; 24];
        let mut na = 0;
        for &b in num {
            if b.is_ascii_digit() && na < 24 {
                da[na] = b;
                na += 1;
            }
        }
        if na == 0 {
            return &[];
        }
        let mut start = 0usize;
        while start < n {
            let end = match BUF[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &BUF[start..end];
            if let Some(sep) = line.iter().position(|&b| b == 0x1F) {
                let name = &line[..sep];
                let number = &line[sep + 1..];
                let mut db = [0u8; 24];
                let mut nb = 0;
                for &b in number {
                    if b.is_ascii_digit() && nb < 24 {
                        db[nb] = b;
                        nb += 1;
                    }
                }
                if nb == na && db[..nb] == da[..na] {
                    let l = name.len().min(23);
                    NAME[..l].copy_from_slice(&name[..l]);
                    return &NAME[..l];
                }
            }
            start = end + 1;
        }
        &[]
    }
}

/// 会话标题：联系人姓名（有）或号码（拷贝到静态缓冲）。
fn conv_title(c: &Conv) -> &'static [u8] {
    static mut TITLE: [u8; 24] = [0; 24];
    unsafe {
        let name = contact_name(&c.number[..c.num_len]);
        let src = if name.len() > 0 { name } else { &c.number[..c.num_len] };
        let l = src.len().min(23);
        TITLE[..l].copy_from_slice(&src[..l]);
        &TITLE[..l]
    }
}

fn draw_notice() {
    if !notice_active() {
        return;
    }
    unsafe {
        let s = &NOTICE[..NOTICE_LEN];
        let w = text_width(s);
        let x = (DISPLAY_W as i32 - w) / 2;
        draw_glass(x - 14, 54, w + 28, 32, 12);
        draw_text(x, 66, s, C_TEXT);
    }
}

// ==================== 渲染 ====================

fn draw_list() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("短信".as_bytes());
    draw_notice();
    let mut y = LIST_TOP;
    for c in 0..unsafe { CONV_N } {
        let conv = unsafe { &CONVS[c] };
        let m = unsafe { &MSGS[conv.last] };
        if y + ROW_H > DISPLAY_H as i32 {
            break;
        }
        draw_glass(12, y, DISPLAY_W as i32 - 24, ROW_H - 10, 16);
        // 头像（姓名首字符）
        let title = conv_title(conv);
        // 磁盘数据可能带空号码记录：空标题用 '?' 占位，避免 title[0] panic。
        let initial = if title.is_empty() { b'?' } else { title[0] };
        draw_disc(44, y + ROW_H / 2 - 5, 20, C_AVATAR);
        draw_char(40, y + ROW_H / 2 - 14, initial, C_ON_COLOR);
        // 名称 + 预览 + 时间
        draw_text(76, y + 14, title, C_NAME);
        draw_text(76, y + 40, &m.text[..m.text_len.min(24)], C_PREVIEW);
        let time = &m.stamp[8..14]; // HHMMSS
        let mut t = [0u8; 6];
        if time.len() >= 6 {
            t[0] = time[0];
            t[1] = time[1];
            t[2] = b':';
            t[3] = time[2];
            t[4] = time[3];
            t[5] = 0;
        }
        draw_text(DISPLAY_W as i32 - 16 - text_width(&t[..5]), y + 14, &t[..5], C_TIME);
        // 未读标记
        if conv.unread > 0 {
            let mut badge = [0u8; 4];
            badge[0] = b'0' + conv.unread.min(9) as u8;
            draw_disc(DISPLAY_W as i32 - 30, y + 44, 10, C_END_COLOR);
            draw_char(DISPLAY_W as i32 - 33, y + 40, badge[0], C_ON_COLOR);
        }
        y += ROW_H;
    }
    // 新建短信按钮
    draw_glass_style(16, DISPLAY_H as i32 - 84, DISPLAY_W as i32 - 32, 60, 16, &STYLE_ACCENT);
    let tw = text_width("新建短信".as_bytes());
    draw_text((DISPLAY_W as i32 - tw) / 2, DISPLAY_H as i32 - 66, "新建短信".as_bytes(), C_ON_COLOR);
    draw_sys_nav();
}

fn draw_thread(idx: usize) {
    draw_gradient_bg(C_BG, glass_dark());
    let conv = unsafe { &CONVS[idx] };
    let title = conv_title(conv);
    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    let bs = BACK_SZ - 8;
    draw_glass(BACK_X, BACK_Y, bs, bs, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);
    draw_text(48, (TITLE_H - 8) / 2, title, C_TEXT);
    draw_notice();

    let mut y = 72i32;
    for i in 0..unsafe { MSG_N } {
        let m = unsafe { &MSGS[i] };
        if m.num_len != conv.num_len || m.number[..m.num_len] != conv.number[..conv.num_len] {
            continue;
        }
        let text = &m.text[..m.text_len];
        let w = text_width(text) + 24;
        let bubble_w = w.max(40).min(DISPLAY_W as i32 - 64);
        if m.out {
            let x = DISPLAY_W as i32 - bubble_w - 16;
            draw_glass_style(x, y, bubble_w, 40, 10, &STYLE_ACCENT);
            draw_text(x + 12, y + 16, text, C_BUBBLE_TEXT);
            // 状态标签（草稿 / 发送中 / 失败）
            match m.status {
                SmsStatus::Draft => draw_text(x, y + 38, "草稿".as_bytes(), C_TIME),
                SmsStatus::Sending => draw_text(x, y + 38, "发送中…".as_bytes(), C_TIME),
                SmsStatus::Failed => draw_text(x, y + 38, "失败".as_bytes(), C_TIME),
                _ => {}
            }
        } else {
            draw_glass(16, y, bubble_w, 40, 10);
            draw_text(28, y + 16, text, C_BUBBLE_TEXT_IN);
        }
        y += 56;
        if y > DISPLAY_H as i32 - 120 {
            break;
        }
    }
    // 回复输入条
    let iy = DISPLAY_H as i32 - 76;
    draw_glass(16, iy, DISPLAY_W as i32 - 32, 56, 12);
    let input = unsafe { &INPUT[..INPUT_LEN] };
    if input.is_empty() {
        draw_text(32, iy + 22, "输入回复，回车发送".as_bytes(), C_TIME);
    } else {
        draw_text(32, iy + 22, input, C_TEXT);
    }
    draw_sys_nav();
}

fn draw_compose() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("新短信".as_bytes());
    draw_notice();
    draw_glass(0, 0, DISPLAY_W as i32, TITLE_H, 0);
    let bs = BACK_SZ - 8;
    draw_glass(BACK_X, BACK_Y, bs, bs, 10);
    draw_text(BACK_X + 10, BACK_Y + 10, b"<", C_TEXT);

    // 号码输入
    draw_text(32, 90, "收件人".as_bytes(), C_TIME);
    draw_glass(16, 120, DISPLAY_W as i32 - 32, 56, 12);
    let num = unsafe { &INPUT_NUM[..INPUT_NUM_LEN] };
    if num.is_empty() && unsafe { INPUT_STAGE } == 0 {
        draw_text(32, 142, "输入号码".as_bytes(), C_TIME);
    } else {
        draw_text(32, 142, num, C_TEXT);
    }
    // 内容输入
    draw_text(32, 210, "内容".as_bytes(), C_TIME);
    draw_glass(16, 240, DISPLAY_W as i32 - 32, 140, 12);
    let input = unsafe { &INPUT[..INPUT_LEN] };
    if input.is_empty() && unsafe { INPUT_STAGE } == 1 {
        draw_text(32, 280, "输入内容，回车发送".as_bytes(), C_TIME);
    } else {
        draw_text(32, 280, input, C_TEXT);
    }
    // 发送按钮
    draw_glass_style(16, DISPLAY_H as i32 - 110, DISPLAY_W as i32 - 32, 64, 16, &STYLE_ACCENT);
    let tw = text_width("发送".as_bytes());
    draw_text((DISPLAY_W as i32 - tw) / 2, DISPLAY_H as i32 - 90, "发送".as_bytes(), C_ON_COLOR);
    draw_sys_nav();
}

fn render() {
    unsafe {
        match VIEW {
            u32::MAX => draw_list(),
            0xFE => draw_compose(),
            idx => {
                if (idx as usize) < CONV_N {
                    draw_thread(idx as usize);
                } else {
                    draw_list();
                }
            }
        }
    }
}

// ==================== 发送 ====================

fn send_message() {
    let (num, text) = unsafe { (&INPUT_NUM[..INPUT_NUM_LEN], &INPUT[..INPUT_LEN]) };
    if num.is_empty() || text.is_empty() {
        return;
    }
    let st = modem::send(num, text);
    match st {
        SmsStatus::Draft => set_notice("无 SIM 卡，已存草稿".as_bytes()),
        SmsStatus::Sending => set_notice("短信发送中…".as_bytes()),
        _ => {}
    }
    unsafe {
        INPUT_LEN = 0;
        INPUT_NUM_LEN = 0;
        INPUT_STAGE = 0;
        VIEW = u32::MAX;
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[sms] 启动中\n");
    if !fb_init() {
        print("[sms] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    store::init_db();
    modem::init();
    print("[sms] 就绪，消息数=");
    print_dec(unsafe { MSG_N } as u64);
    print("\n");

    let mut dirty = true;
    loop {
        if modem::poll() {
            dirty = true;
        }
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            let view = unsafe { VIEW };
            match view {
                u32::MAX => {
                    if back_hit(x, y) {
                        print("[sms] 返回 -> 退出\n");
                        break;
                    }
                    // 新建短信按钮
                    if x >= 16 && x < DISPLAY_W as i32 - 16 && y >= DISPLAY_H as i32 - 84
                        && y < DISPLAY_H as i32 - 24
                    {
                        unsafe { VIEW = 0xFE; }
                        dirty = true;
                        continue;
                    }
                    // 会话行
                    let row = (y - LIST_TOP - unsafe { SCROLL }) / ROW_H;
                    if row >= 0 && (row as usize) < unsafe { CONV_N } {
                        unsafe { VIEW = row as u32; }
                        print("[sms] 已打开会话\n");
                        store::mark_conv_read(row as usize);
                        dirty = true;
                    }
                }
                0xFE => {
                    if back_hit(x, y) {
                        unsafe { VIEW = u32::MAX; }
                        dirty = true;
                    } else if x >= 16 && x < DISPLAY_W as i32 - 16 && y >= DISPLAY_H as i32 - 110
                        && y < DISPLAY_H as i32 - 46
                    {
                        if unsafe { INPUT_NUM_LEN > 0 && INPUT_LEN > 0 } {
                            send_message();
                            dirty = true;
                        }
                    }
                }
                _ => {
                    if back_hit(x, y) {
                        unsafe { VIEW = u32::MAX; }
                        dirty = true;
                    }
                }
            }
        }
        match ev.key {
            KeyAction::Char(c) => {
                let view = unsafe { VIEW };
                if view == 0xFE {
                    // 新建短信：0 阶段输入号码，1 阶段输入内容
                    if unsafe { INPUT_STAGE } == 0 {
                        if c.is_ascii_digit() && unsafe { INPUT_NUM_LEN < INPUT_NUM.len() } {
                            unsafe {
                                INPUT_NUM[INPUT_NUM_LEN] = c;
                                INPUT_NUM_LEN += 1;
                            }
                            dirty = true;
                        }
                    } else if unsafe { INPUT_LEN < INPUT.len() } {
                        unsafe {
                            INPUT[INPUT_LEN] = c;
                            INPUT_LEN += 1;
                        }
                        dirty = true;
                    }
                } else if view != u32::MAX && unsafe { INPUT_LEN < INPUT.len() } {
                    // 回复输入
                    unsafe {
                        INPUT[INPUT_LEN] = c;
                        INPUT_LEN += 1;
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                let view = unsafe { VIEW };
                if view == 0xFE {
                    if unsafe { INPUT_STAGE } == 0 {
                        if unsafe { INPUT_NUM_LEN } > 0 {
                            unsafe { INPUT_NUM_LEN -= 1; }
                            dirty = true;
                        } else {
                            unsafe { VIEW = u32::MAX; }
                            dirty = true;
                        }
                    } else if unsafe { INPUT_LEN } > 0 {
                        unsafe { INPUT_LEN -= 1; }
                        dirty = true;
                    } else {
                        unsafe { INPUT_STAGE = 0; }
                        dirty = true;
                    }
                } else if view != u32::MAX {
                    if unsafe { INPUT_LEN } > 0 {
                        unsafe { INPUT_LEN -= 1; }
                        dirty = true;
                    }
                }
            }
            KeyAction::Enter => {
                let view = unsafe { VIEW };
                if view == 0xFE {
                    if unsafe { INPUT_STAGE } == 0 && unsafe { INPUT_NUM_LEN } > 0 {
                        unsafe { INPUT_STAGE = 1; }
                        dirty = true;
                    } else if unsafe { INPUT_STAGE } == 1 && unsafe { INPUT_LEN } > 0 {
                        send_message();
                        dirty = true;
                    }
                } else if view != u32::MAX {
                    // 回复发送：号码取会话号码
                    let idx = view as usize;
                    if idx < unsafe { CONV_N } {
                        unsafe {
                            INPUT_NUM[..CONVS[idx].num_len].copy_from_slice(
                                &CONVS[idx].number[..CONVS[idx].num_len],
                            );
                            INPUT_NUM_LEN = CONVS[idx].num_len;
                        }
                        if unsafe { INPUT_LEN } > 0 {
                            send_message();
                            dirty = true;
                        }
                    }
                }
            }
            KeyAction::Esc => {
                let view = unsafe { VIEW };
                if view != u32::MAX {
                    unsafe { VIEW = u32::MAX; }
                    dirty = true;
                } else {
                    print("[sms] ESC -> 退出\n");
                    break;
                }
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[sms] ESC -> 退出\n");
            break;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[sms] 正在退出\n");
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
        write_fd(2, b"PANIC: sms\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}