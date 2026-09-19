// KunxOS audiolat：音频延迟测量工具（EL0 用户态，裸 syscall、no_std）。
//
// 经 shell `run /bin/audiolat [secs] [--direct]` 启动。
//   secs    : 测量时长（默认 5s）
//   --direct: 直连 /dev/audio（内核路径，绕过 audiod）
//   默认    : 经 audiod 客户端协议（完整应用 → audiod → 内核 → DAC 路径）
//
// 测量内容：
//   1. 缓冲延迟直方图：每 100ms 采样 `(sw_frames - hw_frames)`（内核写入
//      位置与播放位置之差）→ 毫秒直方图。这是「写入到 DAC」的流水线深度，
//      包含内核环形缓冲 + 硬件在途缓冲 + QEMU 后端缓冲。
//   2. IRQ 统计（内核测量）：IRQ 周期（10ms 名义值，抖动反映调度质量）、
//      IRQ 内补充耗时（refill）、提交 → 消费耗时（dma）。
//      「IRQ 到 DAC」端到端 ≈ refill_avg + dma_avg。
//   3. underrun / late_write 计数（内核统计）。
//
// 输出格式即测试报告模板的数据源（docs/audio/test-report-template.md）。

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
const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDWR: usize = 2;

const SEEK_SET: usize = 0;

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

fn print_us(v: u64) {
    print_dec(v / 1000);
    print(".");
    let frac = v % 1000;
    print_dec(frac / 100);
    print_dec((frac % 100) / 10);
    print("ms");
}

// ==================== 内核统计接口 ====================

const AUDIO_IOCTL_STATS: u32 = 0x4103;

#[repr(C)]
struct AudioStats {
    running: u32,
    rate: u32,
    channels: u32,
    bits: u32,
    sw_frames: u64,
    hw_frames: u64,
    underruns: u64,
    late_writes: u64,
    irq_count: u64,
    period_min_us: u64,
    period_avg_us: u64,
    period_max_us: u64,
    refill_min_us: u64,
    refill_avg_us: u64,
    refill_max_us: u64,
    dma_min_us: u64,
    dma_avg_us: u64,
    dma_max_us: u64,
}

static mut AFD: i32 = -1;

/// 打开 /dev/audio（统计用，不写）。
fn audio_open_stats() -> i32 {
    unsafe {
        let mut p = [0u8; 32];
        let path = b"/dev/audio";
        for (i, &c) in path.iter().enumerate() {
            p[i] = c;
        }
        p[path.len()] = 0;
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDWR) as i32;
        AFD = fd;
        fd
    }
}

fn read_stats(st: &mut AudioStats) -> i32 {
    unsafe {
        syscall3(
            SYS_IOCTL,
            AFD as usize,
            AUDIO_IOCTL_STATS as usize,
            (st as *mut AudioStats) as usize,
        ) as i32
    }
}

// ==================== 直连写入（--direct） ====================

static mut DIRECT_FD: i32 = -1;

fn direct_write(pcm: &[u8]) -> i32 {
    unsafe {
        if DIRECT_FD < 0 {
            let mut p = [0u8; 32];
            let path = b"/dev/audio";
            for (i, &c) in path.iter().enumerate() {
                p[i] = c;
            }
            p[path.len()] = 0;
            let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDWR) as i32;
            if fd < 0 {
                return -1;
            }
            DIRECT_FD = fd;
        }
        syscall3(SYS_WRITE, DIRECT_FD as usize, pcm.as_ptr() as usize, pcm.len()) as i32
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

fn synth_chunk(freq: u32, chunk_idx: u32, out: &mut [u8]) {
    build_sine();
    let step: u32 = (freq as u64 * SINE_LEN as u64 / SAMPLE_RATE as u64) as u32;
    if step == 0 {
        out.fill(0);
        return;
    }
    let phase_base = chunk_idx as u64 * CHUNK_FRAMES as u64 * step as u64;
    for i in 0..CHUNK_FRAMES {
        let phase = (phase_base + i as u64 * step as u64) & (SINE_LEN as u64 - 1);
        let mut v = unsafe { SINE[phase as usize] as i32 };
        v = (v * 13107) >> 15; // 0.4 幅度
        let s = v.clamp(-32767, 32767) as i16;
        let off = i * 4;
        out[off] = (s as u16 & 0xFF) as u8;
        out[off + 1] = ((s as u16 >> 8) & 0xFF) as u8;
        out[off + 2] = (s as u16 & 0xFF) as u8;
        out[off + 3] = ((s as u16 >> 8) & 0xFF) as u8;
    }
}

// ==================== 直方图 ====================

/// 缓冲延迟直方图桶（毫秒，上界）。
const BUCKETS: [u64; 8] = [5, 10, 15, 20, 30, 50, 100, u64::MAX];
const NBUCKETS: usize = 8;
static mut HIST: [u64; NBUCKETS] = [0; NBUCKETS];
static mut SAMPLES: u64 = 0;
static mut LAT_MIN_MS: u64 = u64::MAX;
static mut LAT_MAX_MS: u64 = 0;
static mut LAT_SUM_MS: u64 = 0;

fn hist_add(ms: u64) {
    unsafe {
        for (i, &b) in BUCKETS.iter().enumerate() {
            if ms < b {
                HIST[i] += 1;
                break;
            }
        }
        SAMPLES += 1;
        if ms < LAT_MIN_MS {
            LAT_MIN_MS = ms;
        }
        if ms > LAT_MAX_MS {
            LAT_MAX_MS = ms;
        }
        LAT_SUM_MS += ms;
    }
}

// ==================== 参数解析 ====================

fn arg_str(argv: *const usize, idx: usize) -> Option<&'static [u8]> {
    unsafe {
        if argv.is_null() {
            return None;
        }
        let p = *argv.add(idx);
        if p == 0 {
            return None;
        }
        let mut i = 0usize;
        while *((p + i) as *const u8) != 0 {
            i += 1;
        }
        Some(core::slice::from_raw_parts(p as *const u8, i))
    }
}

fn arg_u32(argv: *const usize, idx: usize, dflt: u32) -> u32 {
    match arg_str(argv, idx) {
        Some(s) => {
            let mut v: u32 = 0;
            let mut any = false;
            for &c in s {
                if (b'0'..=b'9').contains(&c) {
                    v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
                    any = true;
                }
            }
            if any {
                v
            } else {
                dflt
            }
        }
        None => dflt,
    }
}

fn arg_is(argv: *const usize, idx: usize, want: &[u8]) -> bool {
    match arg_str(argv, idx) {
        Some(s) => s == want,
        None => false,
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, argv: *const usize) -> usize {
    let secs = arg_u32(argv, 1, 5).clamp(1, 60);
    let direct = arg_is(argv, 2, b"--direct");

    print("[audiolat] 延迟测量 ");
    print_dec(secs as u64);
    print("s，路径=");
    print(if direct { "内核直连（--direct）" } else { "应用 → audiod → 内核" });
    print("\n");

    if audio_open_stats() < 0 {
        print("[audiolat] /dev/audio 不可用\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }

    if !direct {
        if open_stream(USAGE_MUSIC) != 0 {
            print("[audiolat] audiod 未运行，请使用 --direct\n");
            unsafe { syscall1(SYS_EXIT, 1) };
            return 1;
        }
        request_focus();
    }

    let total_chunks = secs * 100;
    let mut chunk_idx = 0u32;
    let mut last_sample = 0u32;
    for c in 0..total_chunks {
        let mut chunk = [0u8; CHUNK_BYTES];
        synth_chunk(440, chunk_idx, &mut chunk);
        chunk_idx += 1;
        let r = if direct {
            direct_write(&chunk)
        } else {
            write_chunk(&chunk)
        };
        if r != 0 {
            print("[audiolat] 写入失败，正在中止\n");
            break;
        }

        // 每 10 块（100ms）采样一次缓冲延迟
        if c >= last_sample + 10 {
            last_sample = c;
            let mut st: AudioStats = unsafe { core::mem::zeroed() };
            if read_stats(&mut st) >= 0 {
                let buffered = st.sw_frames.saturating_sub(st.hw_frames);
                let ms = buffered * 1000 / SAMPLE_RATE as u64;
                hist_add(ms);
            }
        }
        sleep_ms(10);
    }

    // ---- 报告 ----
    let mut st: AudioStats = unsafe { core::mem::zeroed() };
    if read_stats(&mut st) < 0 {
        print("[audiolat] 统计读取失败\n");
    } else {
        print("\n=== audiolat 报告 ===\n");
        print("路径： ");
        print(if direct { "内核直连" } else { "应用→audiod→内核" });
        print("\n");
        print("缓冲延迟直方图（write->DAC，样本数=");
        print_dec(unsafe { SAMPLES });
        print("）：\n");
        unsafe {
            for i in 0..NBUCKETS {
                let hi = BUCKETS[i];
                print("  ");
                if hi == u64::MAX {
                    print(">=100");
                } else {
                    print_dec(if i == 0 { 0 } else { BUCKETS[i - 1] });
                    print("-");
                    print_dec(hi);
                }
                print(" ms: ");
                print_dec(HIST[i]);
                if SAMPLES > 0 {
                    print(" (");
                    print_dec(HIST[i] * 100 / SAMPLES);
                    print("%)");
                }
                print("\n");
            }
        }
        print("缓冲 min/avg/max： ");
        if unsafe { LAT_MIN_MS } == u64::MAX {
            print("n/a");
        } else {
            print_dec(unsafe { LAT_MIN_MS });
            print("/");
            print_dec(if unsafe { SAMPLES } > 0 {
                unsafe { LAT_SUM_MS / SAMPLES }
            } else {
                0
            });
            print("/");
            print_dec(unsafe { LAT_MAX_MS });
        }
        print(" ms\n");

        print("内核 IRQ 统计（irq_count=");
        print_dec(st.irq_count);
        print("):\n");
        print("  IRQ 周期 min/avg/max： ");
        print_us(st.period_min_us);
        print(" / ");
        print_us(st.period_avg_us);
        print(" / ");
        print_us(st.period_max_us);
        print("\n");
        print("  IRQ 补充 min/avg/max： ");
        print_us(st.refill_min_us);
        print(" / ");
        print_us(st.refill_avg_us);
        print(" / ");
        print_us(st.refill_max_us);
        print("\n");
        print("  提交->消费（IRQ->DAC）min/avg/max： ");
        print_us(st.dma_min_us);
        print(" / ");
        print_us(st.dma_avg_us);
        print(" / ");
        print_us(st.dma_max_us);
        print("\n");
        print("  IRQ->DAC 端到端（refill_avg+dma_avg）： ");
        print_us(st.refill_avg_us + st.dma_avg_us);
        print("\n");
        print("underruns=");
        print_dec(st.underruns);
        print(" late_writes=");
        print_dec(st.late_writes);
        print("\n");
        print("sw_frames=");
        print_dec(st.sw_frames);
        print(" hw_frames=");
        print_dec(st.hw_frames);
        print("\n");
    }

    if !direct {
        release_focus();
        close_stream();
    }
    print("=== 结束 ===\n");
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
        write_fd(2, b"PANIC: audiolat\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
