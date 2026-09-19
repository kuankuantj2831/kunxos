// KunxOS 生命周期演示/测试应用（lcapp，EL0 用户态，裸 syscall、no_std、无堆）。
//
// 为 docs/lifecycle-test.md 的测试方案提供「真实应用侧」的可观测载体——自动化
// 测试（tools/lifecycle_test.py）通过串口标记断言：
//   - on_pause / on_suspend / on_resume / on_terminate 回调确实按序触发
//   - Home / Recents 手势走「后台化（SIGSTOP）→ 恢复（SIGCONT + RESUME）」
//     而非退出；恢复后自动重绘
//   - 后台任务申请 / 结束 / 超时（BG_TASK_EXPIRED）
//   - 本地通知经 pushd → lifd → NOTIFY 事件送达（on_notify）
//   - UI 状态经 lc_save_state / lc_restore_state 落盘到 SFS（重启可恢复）
//
// 全部生命周期行为由 shared/lifecycle.rs（经 app_kit 引入）提供；本应用只
// 注册回调并打印标记，本身不含任何生命周期状态机逻辑。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

// ==================== 共享框架与字体 ====================

#[path = "../../../shared/app_kit.rs"]
mod kit;

use kit::{
    KeyAction, NavAction, LcCallbacks, LcEvent, back_hit, cursor_overlay_move, draw_glass_button,
    draw_gradient_bg, draw_sys_nav, draw_text, draw_title_bar, exit, fb_init, flip, font_init_checked,
    lc_handle_nav, lc_notify_local, lc_request_bg_task, lc_end_bg_task, lc_restore_state,
    lc_save_state, lc_set_callbacks, lc_set_name, poll_input, print, print_dec, sleep_ms,
    BGT_AUDIO, syscall1, write_fd, SYS_EXIT,
};

// ==================== 配色与布局 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_GREEN: u32 = 0x00_00_B9_6B;
const C_AMBER: u32 = 0x00_E2_96_06;
const C_RED: u32 = 0x00_E7_4C_3C;

const BTN1_Y: i32 = 170;
const BTN2_Y: i32 = 250;
const BTN3_Y: i32 = 330;
const BTN_X: i32 = 70;
const BTN_W: i32 = 400;
const BTN_H: i32 = 56;

// ==================== 应用侧生命周期统计 ====================

static mut PAUSE_N: u32 = 0;
static mut SUSPEND_N: u32 = 0;
static mut RESUME_N: u32 = 0;
static mut TERMINATE_N: u32 = 0;
static mut NOTIFY_N: u32 = 0;
static mut BG_EXPIRED_N: u32 = 0;
static mut BG_ACTIVE: bool = false;
static mut SAVE_N: u32 = 0;

fn print_dec_u32(v: u32) {
    print_dec(v as u64);
}

fn text_line(y: i32, s: &str, color: u32) {
    draw_text(24, y, s.as_bytes(), color);
}

fn push_str(out: &mut [u8], n: &mut usize, s: &[u8]) {
    let k = s.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&s[..k]);
    *n += k;
}

fn push_dec_at(out: &mut [u8], n: usize, mut v: u64) -> usize {
    if v == 0 {
        if n < out.len() {
            out[n] = b'0';
            return n + 1;
        }
        return n;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let d = tmp.len() - i;
    if n + d <= out.len() {
        out[n..n + d].copy_from_slice(&tmp[i..]);
    }
    n + d
}

// ==================== 生命周期回调（打印标记供自动化测试断言） ====================

fn cb_pause() {
    unsafe { PAUSE_N += 1; }
    print("[lcapp] on_pause n=");
    print_dec_u32(unsafe { PAUSE_N });
    print("\n");
}

fn cb_suspend() {
    unsafe {
        SUSPEND_N += 1;
        // 保存 UI 状态（计数器）到 SFS：SIGSTOP 前宽限窗内同步落盘。
        // 每次挂起都推进保存计数，重启恢复时 `state_restored save_n>=1`。
        SAVE_N += 1;
        let mut st = [0u8; 12];
        st[0..4].copy_from_slice(&unsafe { SAVE_N }.to_le_bytes());
        st[4..8].copy_from_slice(&unsafe { RESUME_N }.to_le_bytes());
        st[8..12].copy_from_slice(&unsafe { PAUSE_N }.to_le_bytes());
        let ok = lc_save_state(&st);
        print("[lcapp] on_suspend 已保存=");
        print_dec_u32(if ok { st.len() as u32 } else { 0 });
        print("\n");
    }
}

fn cb_resume() {
    unsafe { RESUME_N += 1; }
    print("[lcapp] on_resume n=");
    print_dec_u32(unsafe { RESUME_N });
    print("\n");
}

fn cb_terminate() {
    unsafe {
        TERMINATE_N += 1;
        // 终止前最终保存（LMK 宽限窗内）。
        let mut st = [0u8; 12];
        st[0..4].copy_from_slice(&unsafe { SAVE_N }.to_le_bytes());
        st[4..8].copy_from_slice(&unsafe { RESUME_N }.to_le_bytes());
        st[8..12].copy_from_slice(&unsafe { TERMINATE_N }.to_le_bytes());
        let _ = lc_save_state(&st);
    }
    print("[lcapp] on_terminate n=");
    print_dec_u32(unsafe { TERMINATE_N });
    print("\n");
}

fn cb_notify(ev: &LcEvent) {
    unsafe { NOTIFY_N += 1; }
    // 打印标题（NUL 结尾数组）。
    let mut title = [0u8; 48];
    let mut tlen = 0usize;
    for &c in &ev.title {
        if c == 0 {
            break;
        }
        title[tlen] = c;
        tlen += 1;
    }
print("[lcapp] on_notify id=");
    print_dec_u32(ev.id);
    print(" title=");
    unsafe {
        write_fd(1, &title[..tlen]);
    }
    print("\n");
}

fn cb_bg_expired() {
    unsafe {
        BG_EXPIRED_N += 1;
        BG_ACTIVE = false;
    }
    print("[lcapp] on_bg_expired n=");
    print_dec_u32(unsafe { BG_EXPIRED_N });
    print("\n");
}

// ==================== 渲染 ====================

fn render() {
    // 液态玻璃基底：白色背景打底（浅色渐变，顶部纯白 → 底部极淡冷灰）。
    draw_gradient_bg(C_BG, false);
    draw_title_bar(b"LCApp");
    // 状态行：当前生命周期阶段（应用视角）。
let (state_label, state_color) = unsafe {
                if BG_ACTIVE {
                    ("BG-task active", C_AMBER)
                } else if SUSPEND_N > PAUSE_N {
                    ("suspended (pending)", C_AMBER)
                } else if PAUSE_N > RESUME_N {
                    ("backgrounded (stopped)", C_AMBER)
                } else {
                    ("running", C_GREEN)
                }
            };
            draw_text(24, 60, state_label.as_bytes(), state_color);
    // 计数行
    let mut line = [0u8; 128];
    let mut n = 0usize;
    push_str(&mut line, &mut n, b"pause=");
    n = push_dec_at(&mut line, n, unsafe { PAUSE_N } as u64);
    push_str(&mut line, &mut n, b" suspend=");
    n = push_dec_at(&mut line, n, unsafe { SUSPEND_N } as u64);
    push_str(&mut line, &mut n, b" resume=");
    n = push_dec_at(&mut line, n, unsafe { RESUME_N } as u64);
    push_str(&mut line, &mut n, b" terminate=");
    n = push_dec_at(&mut line, n, unsafe { TERMINATE_N } as u64);
    draw_text(24, 90, &line[..n], C_DIM);
    // 通知/后台任务统计
    let mut line2 = [0u8; 128];
    let mut n2 = 0usize;
    push_str(&mut line2, &mut n2, b"notify=");
    n2 = push_dec_at(&mut line2, n2, unsafe { NOTIFY_N } as u64);
    push_str(&mut line2, &mut n2, b" bg_expired=");
    n2 = push_dec_at(&mut line2, n2, unsafe { BG_EXPIRED_N } as u64);
    push_str(&mut line2, &mut n2, b" save_n=");
    n2 = push_dec_at(&mut line2, n2, unsafe { SAVE_N } as u64);
    draw_text(24, 110, &line2[..n2], C_DIM);
    // 按钮
    draw_glass_button(BTN_X, BTN1_Y, BTN_W, BTN_H, "请求后台任务".as_bytes(), false);
    draw_glass_button(BTN_X, BTN2_Y, BTN_W, BTN_H, "结束后台任务".as_bytes(), false);
    draw_glass_button(BTN_X, BTN3_Y, BTN_W, BTN_H, "发送本地通知".as_bytes(), false);
    // 提示
    text_line(BTN3_Y + 80, "Home/返回手势：后台化、恢复后自动重绘", C_DIM);
    draw_sys_nav();
}

fn hit_btn(row_y: i32, x: i32, y: i32) -> bool {
    x >= BTN_X && x < BTN_X + BTN_W && y >= row_y && y < row_y + BTN_H
}

// ==================== 入口 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[lcapp] 启动\n");
    if !fb_init() {
        print("[lcapp] /dev/fb0 映射失败\n");
        unsafe { exit(1); }
    }
    // 注册生命周期回调。
    let cb = LcCallbacks {
        on_pause: cb_pause,
        on_resume: cb_resume,
        on_suspend: cb_suspend,
        on_terminate: cb_terminate,
        on_notify: cb_notify,
        on_bg_expired: cb_bg_expired,
    };
    lc_set_callbacks(cb);
    // 自报应用名（状态文件 /data/state/lcapp.bin；默认 "app" 不够区分）。
    lc_set_name(b"lcapp");
    // 恢复上次保存的状态（启动时）。
    let mut st = [0u8; 12];
    let got = lc_restore_state(&mut st);
    if got >= 4 {
        unsafe {
            SAVE_N = u32::from_le_bytes([st[0], st[1], st[2], st[3]]);
        }
        print("[lcapp] 状态已恢复 save_n=");
        print_dec_u32(unsafe { SAVE_N });
        print("\n");
    } else {
        print("[lcapp] 无已保存状态\n");
    }
    render();
    flip();
    font_init_checked();
    render();
    flip();
    print("[lcapp] 就绪\n");

    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        // 系统导航：Back=退出；Home=后台化（阻塞至恢复后返回 true）；
        // Recents=后台化 + 最近任务（恢复后返回 true）。
        if ev.nav != NavAction::None {
            let resumed = lc_handle_nav(ev.nav);
            if resumed {
                // 从后台/挂起恢复：重绘（视觉恢复由 launcher 快照动画保证）。
                render();
                flip();
                print("[lcapp] 已恢复\n");
            }
        }
        if let Some((x, y)) = ev.tap {
            if back_hit(x, y) {
                print("[lcapp] 点击返回 -> 退出\n");
                break;
            }
            if hit_btn(BTN1_Y, x, y) {
                print("[lcapp] 已请求后台任务\n");
                let ok = lc_request_bg_task(BGT_AUDIO, 30_000);
                print("[lcapp] 后台任务请求 ok=");
                print_dec_u32(if ok { 1 } else { 0 });
                print("\n");
                unsafe { BG_ACTIVE = true; }
                render();
                flip();
            }
            if hit_btn(BTN2_Y, x, y) {
                print("[lcapp] 正在结束后台任务\n");
                let _ = lc_end_bg_task();
                unsafe { BG_ACTIVE = false; }
                print("[lcapp] 后台任务已结束 ok=1\n");
                render();
                flip();
            }
            if hit_btn(BTN3_Y, x, y) {
                print("[lcapp] 通知已入队\n");
                let ok = lc_notify_local(b"lcapp demo", b"hello from lcapp");
                print("[lcapp] 通知入队 ok=");
                print_dec_u32(if ok { 1 } else { 0 });
                print("\n");
            }
        }
        if ev.key == KeyAction::Esc {
            print("[lcapp] ESC -> 退出\n");
            break;
        }
        sleep_ms(16);
    }

    print("[lcapp] 正在退出\n");
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
        write_fd(2, b"PANIC: lcapp\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}