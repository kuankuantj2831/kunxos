//! 性能统计与调试 overlay（类 Android 开发者选项）：
//!
//! - [`note_frame`]：帧计数器（app `FBIO_PRESENT` 与合成器 `present` 每帧调用），
//!   1 秒窗口滑动计算 FPS。
//! - [`note_cpu_tick`]：调度器 tick 调用，统计 idle/busy 比例 → CPU 占用%。
//! - [`draw_overlay`]：在屏幕右上角绘制 `FPS / CPU% / MEM%`（8x8 点阵放大 2x，
//!   半透明黑底），由合成器 `composite()` 在每帧呈现前调用。
//! - shell 命令：`perf on|off|stats` 控制 overlay 与打印统计。
//!
//! 开销：overlay 文本 ≤ 40 字符 × 16x16 = ~10K 像素写，单帧 <1ms（CPU 合成
//! 2MB 整帧拷贝的 ~0.5%），对 120 FPS 预算（8ms/帧）可忽略。

use crate::compositor::{Color, Surface};

// ==================== 帧率 ====================

static mut WINDOW_START_US: u64 = 0;
static mut WINDOW_FRAMES: u64 = 0;
static mut FPS_VAL: u32 = 0;
/// 累计呈现帧数（自启动）。
static mut TOTAL_FRAMES: u64 = 0;

/// 每呈现一帧调用一次（IRQ 安全）。
pub fn note_frame() {
    unsafe {
        TOTAL_FRAMES += 1;
        let now = crate::timer::now_us();
        if WINDOW_START_US == 0 {
            WINDOW_START_US = now;
        }
        let dt = now.saturating_sub(WINDOW_START_US);
        if dt >= 1_000_000 {
            if dt > 0 {
                FPS_VAL = (WINDOW_FRAMES.saturating_mul(1_000_000) / dt) as u32;
            }
            WINDOW_START_US = now;
            WINDOW_FRAMES = 0;
        }
        WINDOW_FRAMES += 1;
    }
}

/// 当前 FPS（1 秒窗口更新）。
pub fn fps() -> u32 {
    unsafe { FPS_VAL }
}

/// 累计帧数。
pub fn total_frames() -> u64 {
    unsafe { TOTAL_FRAMES }
}

// ==================== CPU 占用 ====================

static mut IDLE_TICKS: u64 = 0;
static mut BUSY_TICKS: u64 = 0;
static mut CPU_PCT: u32 = 50;

/// 调度器每个 tick 调用：`idle` = 当前任务是否为 idle 任务。
/// 每累计 1000 tick（~10s @10ms tick）更新一次百分比并清零窗口。
pub fn note_cpu_tick(idle: bool) {
    unsafe {
        if idle {
            IDLE_TICKS += 1;
        } else {
            BUSY_TICKS += 1;
        }
        let total = IDLE_TICKS + BUSY_TICKS;
        if total >= 1000 {
            if total > 0 {
                CPU_PCT = (BUSY_TICKS.saturating_mul(100) / total) as u32;
            }
            IDLE_TICKS = 0;
            BUSY_TICKS = 0;
        }
    }
}

/// CPU 占用百分比（10s 窗口）。
pub fn cpu_pct() -> u32 {
    unsafe { CPU_PCT }
}

// ==================== 内存 ====================

/// 已用物理内存 KB。
pub fn mem_used_kb() -> u32 {
    let pages = crate::allocator::used_pages();
    (pages * crate::allocator::PAGE_SIZE / 1024) as u32
}

/// 内存占用百分比。
pub fn mem_pct() -> u32 {
    let total = crate::allocator::total_pages();
    if total == 0 {
        return 0;
    }
    (crate::allocator::used_pages().saturating_mul(100) / total) as u32
}

// ==================== 文本格式化 ====================

/// 把统计格式化为 ASCII 文本（写入 `out`，返回长度）。
fn format_stats(out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let push_str = |s: &str, out: &mut [u8], n: &mut usize| {
        for &b in s.as_bytes() {
            if *n < out.len() {
                out[*n] = b;
                *n += 1;
            }
        }
    };
    push_str("FPS:", out, &mut n);
    push_dec(out, &mut n, fps() as u64);
    push_str(" CPU:", out, &mut n);
    push_dec(out, &mut n, cpu_pct() as u64);
    if n < out.len() {
        out[n] = b'%';
        n += 1;
    }
    push_str(" MEM:", out, &mut n);
    push_dec(out, &mut n, mem_pct() as u64);
    if n < out.len() {
        out[n] = b'%';
        n += 1;
    }
    n
}

fn push_dec(out: &mut [u8], n: &mut usize, mut v: u64) {
    let mut b = [0u8; 10];
    let mut i = b.len();
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &c in &b[i..] {
        if *n < out.len() {
            out[*n] = c;
            *n += 1;
        }
    }
}

// ==================== 叠加层 ====================

static mut OVERLAY_ENABLED: bool = false;

pub fn overlay_enabled() -> bool {
    unsafe { OVERLAY_ENABLED }
}

/// 开关 overlay（shell `perf on|off`）。
pub fn set_overlay(on: bool) {
    unsafe { OVERLAY_ENABLED = on };
    crate::uart_send_str(if on { "[perf] overlay 已启用\n" } else { "[perf] overlay 已关闭\n" });
}

/// 当前 overlay 矩形（None = 未启用/未绘制）。合成器据此把 overlay 区域
/// 并入 clean 集（直写 back 的像素需回写缓存，真机可见），并联合上一帧
/// 矩形恢复底图（文本长度逐帧变化，窄帧会在宽残影上叠加）。
pub static mut OVERLAY_RECT: Option<(i32, i32, u32, u32)> = None;
static mut PREV_OVERLAY_RECT: Option<(i32, i32, u32, u32)> = None;

/// 在屏幕右上角绘制统计 overlay。由合成器每帧呈现前调用；无合成器时
/// （未接 ramfb）跳过。文本 8x8 点阵放大 2x，半透明黑底。
pub fn draw_overlay() {
    unsafe {
        if !OVERLAY_ENABLED {
            OVERLAY_RECT = None;
            return;
        }
        let (fb, w, h) = if crate::compositor::ready() {
            (
                // ★ 画到待翻转的 back（随 present 上屏）。此前画到 fb_target()
                // ＝应用 canvas：自绘模式 overlay 永不可见，直绘模式则把 overlay
                // 像素反复混入应用画布（应用画面被涂抹）。
                crate::compositor::back_buffer(),
                crate::compositor::width(),
                crate::compositor::height(),
            )
        } else {
            (0, 0, 0)
        };
        if fb == 0 || w == 0 || h == 0 {
            return;
        }
        let mut text = [0u8; 40];
        let tlen = format_stats(&mut text);
        let scale = 2u32;
        let tw = (tlen as i32) * 8 * scale as i32;
        let th = 8 * scale as i32;
        let x = (w as i32) - tw - 6;
        let y = 6i32;
        // 恢复集取当前 ∪ 上一帧矩形（宽度随统计文本长度变化）。
        let prev = PREV_OVERLAY_RECT.take();
        PREV_OVERLAY_RECT = Some((x, y, tw as u32, th as u32));
        OVERLAY_RECT = Some((x, y, tw as u32, th as u32));
        if let Some((px, py, pw, ph)) = prev {
            let ux = x.min(px);
            let uy = y.min(py);
            let ux1 = (x + tw).max(px + pw as i32);
            let uy1 = (y + th).max(py + ph as i32);
            crate::compositor::restore_back_from_canvas(ux, uy, ux1 - ux, uy1 - uy);
        } else {
            crate::compositor::restore_back_from_canvas(x, y, tw, th);
        }
        let s = Surface { pixels: fb as *mut u32, w, h };
        // 半透明黑底（alpha 混合 40%；底图恢复见上：当前 ∪ 上一帧矩形）
        for row in 0..th {
            for col in 0..tw {
                let px = x + col;
                let py = y + row;
                if px >= 0 && py >= 0 && px < w as i32 && py < h as i32 {
                    let idx = py as usize * w as usize + px as usize;
                    let dst = *s.pixels.add(idx);
                    let bg = Color::BLACK.pixel();
                    let r = ((bg >> 16 & 0xFF) * 40 + (dst >> 16 & 0xFF) * 215) / 255;
                    let g = ((bg >> 8 & 0xFF) * 40 + (dst >> 8 & 0xFF) * 215) / 255;
                    let b = ((bg & 0xFF) * 40 + (dst & 0xFF) * 215) / 255;
                    *s.pixels.add(idx) = (r << 16) | (g << 8) | b;
                }
            }
        }
        crate::ui_framework::draw_text_8x8(
            &s,
            x,
            y,
            core::str::from_utf8(&text[..tlen]).unwrap_or(""),
            Color::GREEN,
            scale,
        );
    }
}

/// 打印统计到控制台（shell `perf stats`）。
pub fn print_stats() {
    crate::uart_send_str("[perf] fps=");
    crate::print_dec(fps() as u64);
    crate::uart_send_str(" cpu=");
    crate::print_dec(cpu_pct() as u64);
    crate::uart_send_str("% mem=");
    crate::print_dec(mem_pct() as u64);
    crate::uart_send_str("% used=");
    crate::print_dec(mem_used_kb() as u64);
    crate::uart_send_str("kB total_frames=");
    crate::print_dec(total_frames());
    crate::uart_send_str("\n");
}
