// KunxOS audiotest：音频流测试应用（EL0 用户态，裸 syscall、no_std）。
//
// 经 shell `run /bin/audiotest <usage> <freq> <secs> [focus]` 启动（也可由
// audiomix fork 作为混音/焦点场景的子进程）：
//   usage: 0=电话 1=导航 2=音乐 3=游戏（默认 2）
//   freq : 音调频率 Hz（默认 440）
//   secs : 播放时长（默认 5）
//   focus: 1=播放开始即申请焦点（默认 1），0=不申请
//
// 行为：
//   1. open_stream(usage) 向 audiod 注册；按参数申请/不申请焦点
//   2. 以 10ms 块生成正弦波（带淡入淡出），经 shared/audio.rs 写入 audiod
//   3. 每 500ms 读取焦点状态（.st 文件），记录增益/状态变化（可观测
//      电话 > 导航 > 音乐 > 游戏的优先级抢占）
//   4. 每次 write_chunk 后轮询 .st 的 acked_seq 测「应用 → audiod」延迟
//   5. 播放结束：RELEASE + CLOSE，打印摘要后退出

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/audio.rs"]
mod audio;

use audio::*;

// ==================== 系统调用 ====================

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

fn print_i32(v: i32) {
    if v < 0 {
        print("-");
        print_dec((-v) as u64);
    } else {
        print_dec(v as u64);
    }
}

// ==================== 参数解析 ====================

/// 从 argv 数组解析第 `idx` 个参数为十进制数（失败返回 `dflt`）。
fn arg_u32(argv: *const usize, idx: usize, dflt: u32) -> u32 {
    unsafe {
        if argv.is_null() {
            return dflt;
        }
        let p = *argv.add(idx);
        if p == 0 {
            return dflt;
        }
        let mut v: u32 = 0;
        let mut any = false;
        let mut i = 0usize;
        loop {
            let c = *((p + i) as *const u8);
            if c == 0 {
                break;
            }
            if (b'0'..=b'9').contains(&c) {
                v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
                any = true;
            }
            i += 1;
        }
        if any {
            v
        } else {
            dflt
        }
    }
}

fn usage_name(u: u8) -> &'static str {
    match u {
        USAGE_PHONE => "phone",
        USAGE_NAV => "nav",
        USAGE_MUSIC => "music",
        _ => "game",
    }
}

fn state_name(s: u32) -> &'static str {
    match s {
        FOCUS_FOCUSED => "focused",
        FOCUS_DUCKED => "ducked",
        _ => "muted",
    }
}

// ==================== 正弦合成 ====================

const SINE_LEN: usize = 256;
static mut SINE: [i16; SINE_LEN] = [0; SINE_LEN];
static mut SINE_READY: bool = false;

fn build_sine() {
    unsafe {
        if SINE_READY {
            return;
        }
        let pi = core::f32::consts::PI;
        let two_pi = 2.0 * pi;
        for i in 0..SINE_LEN {
            let x = two_pi * i as f32 / SINE_LEN as f32;
            let mut xx = x % two_pi;
            let neg = xx > pi;
            if neg {
                xx -= pi;
            }
            if xx > pi / 2.0 {
                xx = pi - xx;
            }
            let num = 16.0 * xx * (pi - xx);
            let den = 5.0 * pi * pi - 4.0 * xx * (pi - xx);
            let v = if den == 0.0 { 0.0 } else { num / den };
            let v = if neg { -v } else { v };
            SINE[i] = (v * 32767.0) as i16;
        }
        SINE_READY = true;
    }
}

/// 合成一块 10ms 正弦波（16bit 立体声，0.4 幅度，整段淡入淡出）。
fn synth_chunk(freq: u32, chunk_idx: u32, total_chunks: u32, out: &mut [u8]) {
    build_sine();
    let step: u32 = (freq as u64 * SINE_LEN as u64 / SAMPLE_RATE as u64) as u32;
    if step == 0 {
        out.fill(0);
        return;
    }
    let fade_chunks = (total_chunks / 20).max(1);
    let phase_base = chunk_idx as u64 * CHUNK_FRAMES as u64 * step as u64;
    for i in 0..CHUNK_FRAMES {
        let phase = (phase_base + i as u64 * step as u64) & (SINE_LEN as u64 - 1);
        let mut v = unsafe { SINE[phase as usize] as i32 };
        v = (v * 13107) >> 15; // 0.4 幅度
        if chunk_idx < fade_chunks {
            v = v * (chunk_idx + 1) as i32 / fade_chunks as i32;
        } else if chunk_idx >= total_chunks - fade_chunks {
            let remain = total_chunks - chunk_idx;
            v = v * remain as i32 / fade_chunks as i32;
        }
        let s = v.clamp(-32767, 32767) as i16;
        let off = i * 4;
        out[off] = (s as u16 & 0xFF) as u8;
        out[off + 1] = ((s as u16 >> 8) & 0xFF) as u8;
        out[off + 2] = (s as u16 & 0xFF) as u8;
        out[off + 3] = ((s as u16 >> 8) & 0xFF) as u8;
    }
}

// ==================== 延迟测量 ====================

static mut LAST_GAIN: i32 = -1;
static mut LAST_STATE: u32 = u32::MAX;

/// 报告焦点状态变化（打印增益/状态；返回变化是否发生）。
fn report_state() {
    let (gain, state, ok) = stream_state();
    if !ok {
        return;
    }
    unsafe {
        if gain != LAST_GAIN || state != LAST_STATE {
            LAST_GAIN = gain;
            LAST_STATE = state;
            print("[audiotest] 焦点状态: gain=");
            print_dec(gain as u64);
            print(" (");
            // 增益转百分比
            print_dec((gain as u64) * 100 / 32768);
            print("%) ");
            print(state_name(state));
            print("\n");
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, argv: *const usize) -> usize {
    let usage = arg_u32(argv, 1, USAGE_MUSIC as u32) as u8;
    let freq = arg_u32(argv, 2, 440);
    let secs = arg_u32(argv, 3, 5).clamp(1, 60);
    let focus = arg_u32(argv, 4, 1) != 0;

    print("[audiotest] pid=");
    // 经 shared 库无法直接拿 pid，用 open_stream 内部注册即可；此处打印参数
    print(" usage=");
    print(usage_name(usage));
    print(" freq=");
    print_dec(freq as u64);
    print("Hz dur=");
    print_dec(secs as u64);
    print("s focus=");
    print_dec(focus as u64);
    print("\n");

    if open_stream(usage) != 0 {
        print("[audiotest] open_stream 失败 (audiod 未运行?)\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }
    if focus {
        request_focus();
    }
    report_state();

    let total_chunks = secs * 1000 / 10;
    let mut last_report = 0u32;
    for c in 0..total_chunks {
        let mut chunk = [0u8; CHUNK_BYTES];
        synth_chunk(freq, c, total_chunks, &mut chunk);
        let r = write_chunk(&chunk);
        if r != 0 {
            print("[audiotest] write_chunk 失败，停止\n");
            break;
        }
        // 每 500ms 报告一次状态
        if c >= last_report + 50 {
            last_report = c;
            report_state();
        }
        sleep_ms(10);
    }

    if focus {
        release_focus();
    }
    close_stream();
    print("[audiotest] 完成 (");
    print_dec(total_chunks as u64);
    print(" 个块)\n");
    unsafe { syscall1(SYS_EXIT, 0) };
    0
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
        write_fd(2, b"PANIC: audiotest\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
