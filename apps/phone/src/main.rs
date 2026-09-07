// KunxOS 电话应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/phone）。职责：
//   1. 拨号界面：号码显示屏 + 3x4 键盘 + 删除/呼叫键
//   2. 与 rild 交互：写 /var/run/rild.cmd 下发 DIAL/HANGUP/ANSWER；
//      每帧轮询 /var/run/rild.state 呈现通话状态
//   3. 通话状态：拨号中 / 来电（接听/拒绝）/ 通话中（计时 + 免提切换）
//   4. 无 SIM 时紧急呼叫（112/911）提示
//   5. 通讯录集成：/data/contacts.dat 号码 → 姓名
//   6. 未接来电横幅

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

// ==================== 系统调用与常量 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;

const STATE_PATH: &str = "/var/run/rild.state";
const CMD_PATH: &str = "/var/run/rild.cmd";
const CONTACTS_PATH: &str = "/data/contacts.dat";

/// /dev/audio ioctl（与内核一致）。
const AUDIO_IOCTL_SET_ROUTE: u32 = 0x410B;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
/// 键盘按键填充（实际绘制由 kit 浅色玻璃接管，保留常量供兼容）。
const C_KEY: u32 = 0x00_FF_FF_FF;
/// 通话头像圆盘：绿色彩色表面（保留）。
const C_CALL: u32 = 0x00_00_B9_6B;
const C_END: u32 = 0x00_E7_4C_3C;
/// 删除键文字：红色危险玻璃上的白字（彩色表面保留）。
const C_DEL: u32 = 0x00_FF_FF_FF;
const C_DIM: u32 = 0x00_6B_72_80;

const KEY_ROWS: i32 = 4;
const KEY_COLS: i32 = 3;
const KEY_W: i32 = 96;
const KEY_H: i32 = 72;
const KEY_GAP: i32 = 14;
const PAD_X: i32 = (DISPLAY_W as i32 - (KEY_COLS * KEY_W + (KEY_COLS - 1) * KEY_GAP)) / 2;
const PAD_Y: i32 = 560;

const KEYS: [[u8; 3]; 4] = [
    [b'1', b'2', b'3'],
    [b'4', b'5', b'6'],
    [b'7', b'8', b'9'],
    [b'*', b'0', b'#'],
];

// ==================== 通话状态（来自 rild.state） ====================

#[derive(Clone, Copy, PartialEq)]
enum CallSt {
    Idle,
    Dialing,
    Ringing,
    Active,
    RilDown,
}

static mut CALL: CallSt = CallSt::Idle;
static mut NUM: [u8; 24] = [0; 24];
static mut NUM_LEN: usize = 0;
static mut NAME: [u8; 24] = [0; 24];
static mut NAME_LEN: usize = 0;
static mut START_SEC: u64 = 0;
static mut MISSED: bool = false;
static mut MISS_NUM: [u8; 24] = [0; 24];
static mut MISS_LEN: usize = 0;
static mut SIM_ABSENT: bool = false;
static mut SIGNAL: u8 = 0;
static mut OP: [u8; 16] = [0; 16];
static mut OP_LEN: usize = 0;
static mut ROUTE: u32 = 0;

/// 拨号输入缓冲。
static mut NUMBER: [u8; 16] = [0; 16];
static mut NUMBER_LEN: usize = 0;
/// 通话中动画 tick。
static mut CALL_TICK: u32 = 0;

// ==================== 状态文件读取 ====================

fn read_state() {
    unsafe {
        let fd = kit::open(STATE_PATH, O_RDONLY);
        if fd < 0 {
            CALL = CallSt::RilDown;
            return;
        }
        let mut buf = [0u8; 512];
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len());
        syscall1(SYS_CLOSE, fd as usize);
        // 读失败 -1（usize 天文数字）按 isize 判负，否则切片越界 panic
        let n = n as isize;
        if n <= 0 {
            CALL = CallSt::RilDown;
            return;
        }
        let n = (n as usize).min(buf.len());
        // 逐行解析 KEY:VALUE
        let mut start = 0usize;
        while start < n {
            let end = match buf[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &buf[start..end];
            if let Some(colon) = line.iter().position(|&b| b == b':') {
                let key = &line[..colon];
                let val = &line[colon + 1..];
                match key {
                    b"CALL" => {
                        CALL = match val {
                            b"IDLE" => CallSt::Idle,
                            b"DIALING" => CallSt::Dialing,
                            b"RINGING" => CallSt::Ringing,
                            b"ACTIVE" => CallSt::Active,
                            _ => CallSt::Idle,
                        };
                    }
                    b"NUM" => {
                        NUM_LEN = val.len().min(NUM.len());
                        NUM[..NUM_LEN].copy_from_slice(&val[..NUM_LEN]);
                    }
                    b"NAME" => {
                        NAME_LEN = val.len().min(NAME.len());
                        NAME[..NAME_LEN].copy_from_slice(&val[..NAME_LEN]);
                    }
                    b"START" => {
                        START_SEC = parse_u64(val);
                    }
                    b"MISSED" => MISSED = val == b"1",
                    b"MISSNUM" => {
                        MISS_LEN = val.len().min(MISS_NUM.len());
                        MISS_NUM[..MISS_LEN].copy_from_slice(&val[..MISS_LEN]);
                    }
                    b"SIM" => SIM_ABSENT = val == b"ABSENT",
                    b"SIG" => SIGNAL = parse_u64(val).min(31) as u8,
                    b"OP" => {
                        OP_LEN = val.len().min(OP.len());
                        OP[..OP_LEN].copy_from_slice(&val[..OP_LEN]);
                    }
                    b"ROUTE" => ROUTE = parse_u64(val) as u32,
                    _ => {}
                }
            }
            start = end + 1;
        }
    }
}

fn parse_u64(s: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in s {
        if b.is_ascii_digit() {
            v = v * 10 + (b - b'0') as u64;
        }
    }
    v
}

// ==================== 命令下发 ====================

fn send_cmd(cmd: &[u8]) {
    unsafe {
        let fd = kit::open(CMD_PATH, O_WRONLY | O_CREAT);
        if fd >= 0 {
            syscall3(SYS_WRITE, fd as usize, cmd.as_ptr() as usize, cmd.len());
            syscall1(SYS_CLOSE, fd as usize);
        } else {
            print("[phone] 无法打开 rild.cmd\n");
        }
    }
}

fn dial_number() {
    unsafe {
        let mut cmd = [0u8; 40];
        cmd[..5].copy_from_slice(b"DIAL:");
        let n = 5 + NUMBER_LEN;
        cmd[5..n].copy_from_slice(&NUMBER[..NUMBER_LEN]);
        cmd[n] = b'\n';
        send_cmd(&cmd[..n + 1]);
        print("[phone] 拨号指令已发送\n");
    }
}

fn hangup_cmd() {
    send_cmd(b"HANGUP\n");
}

fn answer_cmd() {
    send_cmd(b"ANSWER\n");
}

// ==================== 渲染 ====================

fn key_rect(row: i32, col: i32) -> (i32, i32, i32, i32) {
    (
        PAD_X + col * (KEY_W + KEY_GAP),
        PAD_Y + row * (KEY_H + KEY_GAP),
        KEY_W,
        KEY_H,
    )
}

fn draw_center_in(y: i32, s: &[u8], color: u32, box_x: i32, box_w: i32) {
    let tx = box_x + (box_w - text_width(s)) / 2;
    draw_text(tx, y, s, color);
}

/// 顶部状态行：SIM / 信号 / 运营商。
fn draw_status() {
    let mut line = [0u8; 40];
    let mut p = 0;
    if unsafe { SIM_ABSENT } {
        line[p..p + 7].copy_from_slice(b"NO SIM ");
        p += 7;
    } else {
        line[p..p + 4].copy_from_slice(b"SIM ");
        p += 4;
        let sig = unsafe { SIGNAL };
        line[p] = b'[';
        p += 1;
        let bars = (sig / 8).min(4) as usize;
        for i in 0..4 {
            line[p] = if i < bars { b'|' } else { b'.' };
            p += 1;
        }
        line[p] = b']';
        p += 1;
        line[p] = b' ';
        p += 1;
    }
    let op = unsafe { &OP[..OP_LEN] };
    for &b in op {
        if p < 40 {
            line[p] = b;
            p += 1;
        }
    }
    draw_text(16, 40, &line[..p], C_DIM);
}

fn draw_dialer() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("电话".as_bytes());
    draw_status();

    // 号码显示屏
    draw_glass(16, 64, DISPLAY_W as i32 - 32, 100, 24);
    let number = unsafe { &NUMBER[..NUMBER_LEN] };
    if number.is_empty() {
        if unsafe { SIM_ABSENT } {
            draw_text(32, 98, "无 SIM：紧急呼叫可用 (112/911)".as_bytes(), C_END);
        } else {
            draw_text(32, 126, "输入号码".as_bytes(), C_DIM);
        }
    }
    draw_text(32, 124, number, C_TEXT);

    // 键盘
    for r in 0..KEY_ROWS {
        for c in 0..KEY_COLS {
            let (kx, ky, kw, kh) = key_rect(r, c);
            let ch = KEYS[r as usize][c as usize];
            draw_glass_key(kx, ky, kw, kh, &[ch]);
        }
    }

    // 删除 + 呼叫（删除=红色危险玻璃白字，呼叫=强调绿玻璃白字）。
    draw_glass_style(16, PAD_Y - 96, 130, 64, 16, &STYLE_DANGER);
    draw_text(50, PAD_Y - 96 + 26, "删除".as_bytes(), C_DEL);
    draw_glass_style(DISPLAY_W as i32 - 146, PAD_Y - 96, 130, 64, 16, &STYLE_ACCENT);
    draw_center_in(PAD_Y - 96 + 26, "呼叫".as_bytes(), 0x00_FF_FF_FF, DISPLAY_W as i32 - 146, 130);

    // 未接来电横幅
    if unsafe { MISSED } {
        let num = unsafe { &MISS_NUM[..MISS_LEN] };
        let mut s = [0u8; 48];
        s[..7].copy_from_slice("未接:".as_bytes());
        let nl = num.len().min(40);
        if 7 + nl <= s.len() {
            s[7..7 + nl].copy_from_slice(&num[..nl]);
        }
        draw_glass_style(16, 120, DISPLAY_W as i32 - 32, 44, 12, &STYLE_DANGER);
        draw_text(32, 134, &s[..(7 + nl).min(s.len())], 0x00_FF_FF_FF);
    }

    draw_text(16, DISPLAY_H as i32 - 60, "支持键盘输入数字，回车拨号".as_bytes(), C_DIM);
    draw_sys_nav();
}

fn draw_dialing() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("电话".as_bytes());
    draw_status();
    draw_glass(16, 120, DISPLAY_W as i32 - 32, 160, 24);
    draw_text(32, 160, "拨号中...".as_bytes(), C_ACCENT);
    let num = unsafe { &NUM[..NUM_LEN] };
    draw_text(32, 200, num, C_TEXT);
    let name = unsafe { &NAME[..NAME_LEN] };
    if name.len() > 0 {
        draw_text(32, 240, name, C_DIM);
    }
    // 挂断（红色危险玻璃白字）
    draw_glass_style((DISPLAY_W as i32 - 150) / 2, 420, 150, 72, 20, &STYLE_DANGER);
    draw_center_in(448, "挂断".as_bytes(), 0x00_FF_FF_FF, (DISPLAY_W as i32 - 150) / 2, 150);
    draw_sys_nav();
}

fn draw_incoming() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("来电".as_bytes());
    draw_disc((DISPLAY_W / 2) as i32, 220, 64, C_CALL);
    draw_char((DISPLAY_W / 2) as i32 - 6, 210, b'?', 0x00_FF_FF_FF);
    let num = unsafe { &NUM[..NUM_LEN] };
    draw_center(330, num, C_TEXT);
    let name = unsafe { &NAME[..NAME_LEN] };
    if name.len() > 0 {
        draw_center(370, name, C_DIM);
    }
    // 拒绝 / 接听（红/绿彩色玻璃白字）
    let bw = 170;
    let gap = 30;
    let total = bw * 2 + gap;
    let x0 = (DISPLAY_W as i32 - total) / 2;
    draw_glass_style(x0, 520, bw, 76, 20, &STYLE_DANGER);
    draw_center_in(548, "拒绝".as_bytes(), 0x00_FF_FF_FF, x0, bw);
    draw_glass_style(x0 + bw + gap, 520, bw, 76, 20, &STYLE_ACCENT);
    draw_center_in(548, "接听".as_bytes(), 0x00_FF_FF_FF, x0 + bw + gap, bw);
    draw_sys_nav();
}

fn draw_active() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("通话中".as_bytes());
    draw_disc((DISPLAY_W / 2) as i32, 220, 64, C_CALL);
    draw_char((DISPLAY_W / 2) as i32 - 6, 210, b'?', 0x00_FF_FF_FF);
    let num = unsafe { &NUM[..NUM_LEN] };
    draw_center(330, num, C_TEXT);
    let name = unsafe { &NAME[..NAME_LEN] };
    if name.len() > 0 {
        draw_center(370, name, C_DIM);
    }
    // 通话计时
    let elapsed = unsafe { now_us() / 1_000_000 }.saturating_sub(unsafe { START_SEC });
    // 分钟钳到 99（>99 分钟时 b'0'+(mm/10) 溢出成非数字字节）。
    let mm = (elapsed / 60).min(99);
    let ss = elapsed % 60;
    let mut t = [0u8; 8];
    t[0] = b'0' + (mm / 10) as u8;
    t[1] = b'0' + (mm % 10) as u8;
    t[2] = b':';
    t[3] = b'0' + (ss / 10) as u8;
    t[4] = b'0' + (ss % 10) as u8;
    draw_center(410, &t[..5], C_ACCENT);

    // 免提切换（开=强调绿玻璃白字，关=浅色玻璃深字）。
    let speaker = unsafe { ROUTE == 1 };
    let (sx, sw) = ((DISPLAY_W as i32 - 300) / 2, 300);
    draw_glass_style(sx, 480, sw, 64, 16, if speaker { &STYLE_ACCENT } else { &STYLE_LIGHT });
    draw_center_in(508, if speaker { "扬声器开".as_bytes() } else { "免提".as_bytes() }, if speaker { 0x00_FF_FF_FF } else { C_TEXT }, sx, sw);

    // 挂断（红色危险玻璃白字）
    draw_glass_style((DISPLAY_W as i32 - 170) / 2, 580, 170, 76, 20, &STYLE_DANGER);
    draw_center_in(608, "挂断".as_bytes(), 0x00_FF_FF_FF, (DISPLAY_W as i32 - 170) / 2, 170);
    draw_sys_nav();
}

fn render() {
    match unsafe { CALL } {
        CallSt::Idle => draw_dialer(),
        CallSt::Dialing => draw_dialing(),
        CallSt::Ringing => draw_incoming(),
        CallSt::Active => draw_active(),
        CallSt::RilDown => {
            draw_gradient_bg(C_BG, glass_dark());
            draw_title_bar("电话".as_bytes());
            draw_text(40, 200, "RIL 服务不可用".as_bytes(), C_END);
            draw_sys_nav();
        }
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[phone] 启动中\n");
    if !fb_init() {
        print("[phone] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    print("[phone] 就绪\n");

    let mut dirty = true;
    loop {
        read_state();
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        let call = unsafe { CALL };
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[phone] 返回 -> 退出\n");
                break;
            }
            match call {
                CallSt::Idle => {
                    // 清除未接来电横幅
                    if unsafe { MISSED } && x >= 16 && x < DISPLAY_W as i32 - 16 && y >= 120 && y < 164 {
                        unsafe { MISSED = false; }
                        dirty = true;
                    }
                    // 删除
                    if x >= 16 && x < 146 && y >= PAD_Y - 96 && y < PAD_Y - 32 {
                        if unsafe { NUMBER_LEN } > 0 {
                            unsafe { NUMBER_LEN -= 1; }
                            dirty = true;
                        }
                    } else if x >= DISPLAY_W as i32 - 146 && x < DISPLAY_W as i32 - 16
                        && y >= PAD_Y - 96 && y < PAD_Y - 32 {
                        if unsafe { NUMBER_LEN } > 0 {
                            dial_number();
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
                CallSt::Dialing => {
                    // 挂断
                    if x >= (DISPLAY_W as i32 - 150) / 2 && x < (DISPLAY_W as i32 + 150) / 2
                        && y >= 420 && y < 492 {
                        hangup_cmd();
                    }
                }
                CallSt::Ringing => {
                    let bw = 170;
                    let gap = 30;
                    let total = bw * 2 + gap;
                    let x0 = (DISPLAY_W as i32 - total) / 2;
                    if x >= x0 && x < x0 + bw && y >= 520 && y < 596 {
                        hangup_cmd(); // 拒绝
                    } else if x >= x0 + bw + gap && x < x0 + total && y >= 520 && y < 596 {
                        answer_cmd();
                    }
                }
                CallSt::Active => {
                    // 免提
                    let (sx, sw) = ((DISPLAY_W as i32 - 300) / 2, 300);
                    if x >= sx && x < sx + sw && y >= 480 && y < 544 {
                        toggle_speaker();
                        dirty = true;
                    }
                    // 挂断
                    if x >= (DISPLAY_W as i32 - 170) / 2 && x < (DISPLAY_W as i32 + 170) / 2
                        && y >= 580 && y < 656 {
                        hangup_cmd();
                    }
                }
                CallSt::RilDown => {}
            }
        }
        // 键盘输入
        match ev.key {
            KeyAction::Char(c) => {
                if call == CallSt::Idle && c.is_ascii_digit() && unsafe { NUMBER_LEN < NUMBER.len() } {
                    unsafe {
                        NUMBER[NUMBER_LEN] = c;
                        NUMBER_LEN += 1;
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                if call == CallSt::Idle && unsafe { NUMBER_LEN } > 0 {
                    unsafe { NUMBER_LEN -= 1; }
                    dirty = true;
                }
            }
            KeyAction::Enter => {
                if call == CallSt::Idle && unsafe { NUMBER_LEN } > 0 {
                    dial_number();
                    dirty = true;
                }
            }
            KeyAction::Esc => {
                print("[phone] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[phone] ESC -> 退出\n");
            break;
        }
        if unsafe { CALL == CallSt::Active || CALL == CallSt::Dialing } {
            unsafe { CALL_TICK += 1; }
            if unsafe { CALL_TICK } % 30 == 0 {
                dirty = true; // 计时动画帧
            }
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

    print("[phone] 正在退出\n");
    unsafe { exit(0); }
}

fn toggle_speaker() {
    // /dev/audio SET_ROUTE：0 听筒 / 1 扬声器
    unsafe {
        let fd = kit::open("/dev/audio", O_RDONLY);
        if fd >= 0 {
            let new_route = if ROUTE == 1 { 0u32 } else { 1u32 };
            let mut arg = [0u8; 8];
            arg[..4].copy_from_slice(&new_route.to_le_bytes());
            syscall3(SYS_IOCTL, fd as usize, AUDIO_IOCTL_SET_ROUTE as usize, arg.as_mut_ptr() as usize);
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

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
        write_fd(2, b"PANIC: phone\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
