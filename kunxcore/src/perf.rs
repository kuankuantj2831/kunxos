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

/// 每呈现一帧调用一次（IRQ 安全；呈现热路径只做整数运算与标记置位）。
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

        // 帧间隔采样（全局：内核动画路径 + 应用直绘路径都经 present()）
        if FS_LAST_PRESENT_US != 0 {
            let iv = now.saturating_sub(FS_LAST_PRESENT_US);
            if iv < FS_MIN {
                FS_MIN = iv;
            }
            if iv > FS_MAX {
                FS_MAX = iv;
            }
            FS_SUM += iv;
            if iv > FS_DROP_US {
                FS_DROPS += 1;
            }
            if iv > FS_SPIKE_US {
                FS_SPIKES += 1;
            }
            if FS_N < FS_WINDOW {
                FS_SAMPLES[FS_N] = iv;
                FS_N += 1;
            }
        }
        FS_LAST_PRESENT_US = now;
        FS_COUNT += 1;
        if FS_COUNT >= FS_REPORT_EVERY {
            FS_PENDING = true;
        }
        // t2f：本帧是否为输入跟随帧（差值在 2 个 60Hz 帧内）
        let last_in = crate::input::last_input_us();
        if last_in != 0 {
            let d = now.saturating_sub(last_in);
            if d <= T2F_WINDOW_US {
                T2F_COUNT += 1;
                if d < T2F_MIN {
                    T2F_MIN = d;
                }
                if d > T2F_MAX {
                    T2F_MAX = d;
                }
                T2F_SUM += d;
                if T2F_N < T2F_SAMPLES.len() {
                    T2F_SAMPLES[T2F_N] = d;
                    T2F_N += 1;
                }
            }
        }
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

// ==================== 帧间隔与触控→上屏延迟（t2f） ====================
//
// anim::FrameStats 只度量内核驱动的动画管线（frame_tick → composite+present）；
// 应用接管显示（FB_ACTIVE）后呈现节奏由应用的 FBIO_PRESENT 驱动。本节在
// note_frame（每次真实呈现都调用，双路径覆盖）补齐全局度量：
//   - 帧间隔 min/avg/p95/max、掉帧(>12ms)/尖峰(>20ms)——阈值与
//     docs/perf-baseline.md §2 判定标准一致；
//   - 应用直绘路径合成+呈现耗时（note_app_render，fb_present 计时上报）；
//   - t2f：最近指针事件（input::last_input_us）到本帧呈现的延迟。仅当差值
//     ≤ 2 个 60Hz 帧（33.3ms）时计入——超过该窗口的呈现（时钟刷新等后台
//     重绘）与本次输入无跟随关系，计入会污染分布。
// 报告打印（数百字符串口输出在 115200 波特下忙等 >10ms）绝不在呈现热路径
// 执行：note_* 只做无分配整数运算并置 PENDING，由内核主循环线程上下文
// 调 poll_report() 冲刷（与 anim::poll_deferred_stats 同模式）。

const FS_WINDOW: usize = 64;
const FS_REPORT_EVERY: u64 = 120;
const FS_DROP_US: u64 = 12_000;
const FS_SPIKE_US: u64 = 20_000;
const T2F_WINDOW_US: u64 = 33_333;

static mut FS_LAST_PRESENT_US: u64 = 0;
static mut FS_COUNT: u64 = 0;
static mut FS_MIN: u64 = u64::MAX;
static mut FS_MAX: u64 = 0;
static mut FS_SUM: u64 = 0;
static mut FS_SAMPLES: [u64; FS_WINDOW] = [0; FS_WINDOW];
static mut FS_N: usize = 0;
static mut FS_DROPS: u64 = 0;
static mut FS_SPIKES: u64 = 0;
static mut FS_RENDER_SUM: u64 = 0;
static mut FS_RENDER_MAX: u64 = 0;
static mut FS_RENDER_N: u64 = 0;
// fb_present 关中断临界区（元数据 + 提交阶段，不含像素搬运）耗时：
// 该值逼近直绘渲染总耗时说明 IRQ 窗口退化回「全程屏蔽」（输入/调度被
// 呈现路径独占）；显著小于总耗时即像素阶段已让出 CPU 给 IRQ。
static mut FS_CRIT_SUM: u64 = 0;
static mut FS_CRIT_MAX: u64 = 0;
static mut T2F_COUNT: u64 = 0;
static mut T2F_MIN: u64 = u64::MAX;
static mut T2F_MAX: u64 = 0;
static mut T2F_SUM: u64 = 0;
static mut T2F_SAMPLES: [u64; 32] = [0; 32];
static mut T2F_N: usize = 0;
static mut FS_PENDING: bool = false;

/// 应用直绘路径合成+呈现耗时上报（compositor::fb_present 成功呈现时调用）。
///
/// `render_us` 为呈现总耗时；`crit_us` 为其中 IRQ 被屏蔽的临界区耗时
/// （fb_present 元数据 + 提交阶段，不含 canvas→back 像素搬运）。
pub fn note_app_render(render_us: u64, crit_us: u64) {
    unsafe {
        FS_RENDER_SUM += render_us;
        FS_RENDER_N += 1;
        if render_us > FS_RENDER_MAX {
            FS_RENDER_MAX = render_us;
        }
        FS_CRIT_SUM += crit_us;
        if crit_us > FS_CRIT_MAX {
            FS_CRIT_MAX = crit_us;
        }
    }
}

/// 冲刷帧统计报告（内核主循环线程上下文调用；呈现/IRQ 路径只置标记）。
pub fn poll_report() {
    unsafe {
        if !FS_PENDING {
            return;
        }
        FS_PENDING = false;
        // 快照 + 复位在关中断窗口内完成：note_frame 在 IRQ/呈现上下文推进
        // 同一组计数，可被抢占的「读-排序-清零」会把新样本抹掉或跨窗口
        // 混算。UART 打印（毫秒级忙等）留在可中断上下文 —— 帧统计冲刷
        // 此前刚因 IRQ 内忙等打满帧预算而从 CNTP 挪出来，不能倒退。
        let saved = crate::sched_cfs::read_daif();
        core::arch::asm!("csrci sstatus, 2");
        let n = FS_N;
        if n == 0 {
            // 报告标记跨窗口竞态（同 anim::report_frame_stats 的 n==0 防御）
            crate::sched_cfs::write_daif(saved);
            return;
        }
        let mut buf = [0u64; FS_WINDOW];
        buf[..n].copy_from_slice(&FS_SAMPLES[..n]);
        let count = FS_COUNT;
        let min = FS_MIN;
        let max = FS_MAX;
        let sum = FS_SUM;
        let drops = FS_DROPS;
        let spikes = FS_SPIKES;
        let rn = FS_RENDER_N;
        let rsum = FS_RENDER_SUM;
        let rmax = FS_RENDER_MAX;
        let csum = FS_CRIT_SUM;
        let cmax = FS_CRIT_MAX;
        let tn = T2F_N;
        let mut tb = [0u64; 32];
        if tn > 0 {
            tb[..tn].copy_from_slice(&T2F_SAMPLES[..tn]);
        }
        let tcount = T2F_COUNT;
        let tmin = T2F_MIN;
        let tmax = T2F_MAX;
        let tsum = T2F_SUM;
        // 复位报告窗口（保留 FS_LAST_PRESENT_US 保证跨窗口间隔连续）
        FS_COUNT = 0;
        FS_MIN = u64::MAX;
        FS_MAX = 0;
        FS_SUM = 0;
        FS_SAMPLES = [0; FS_WINDOW];
        FS_N = 0;
        FS_DROPS = 0;
        FS_SPIKES = 0;
        FS_RENDER_SUM = 0;
        FS_RENDER_MAX = 0;
        FS_RENDER_N = 0;
        FS_CRIT_SUM = 0;
        FS_CRIT_MAX = 0;
        T2F_COUNT = 0;
        T2F_MIN = u64::MAX;
        T2F_MAX = 0;
        T2F_SUM = 0;
        T2F_SAMPLES = [0; 32];
        T2F_N = 0;
        crate::sched_cfs::write_daif(saved);

        // ---- 排序 + 打印（可中断上下文） ----
        for i in 1..n {
            let key = buf[i];
            let mut j = i;
            while j > 0 && buf[j - 1] > key {
                buf[j] = buf[j - 1];
                j -= 1;
            }
            buf[j] = key;
        }
        let p95 = buf[(n * 95 / 100).min(n - 1)];
        let avg = sum / count.max(1);
        crate::uart_send_str("[perf] 帧(n=");
        crate::print_dec(count);
        crate::uart_send_str(" 间隔(min=");
        crate::print_dec(min);
        crate::uart_send_str(",avg=");
        crate::print_dec(avg);
        crate::uart_send_str(",p95=");
        crate::print_dec(p95);
        crate::uart_send_str(",max=");
        crate::print_dec(max);
        crate::uart_send_str(")us 掉帧(>12ms)=");
        crate::print_dec(drops);
        crate::uart_send_str(" 尖峰(>20ms)=");
        crate::print_dec(spikes);
        if rn > 0 {
            crate::uart_send_str(" 直绘渲染(avg=");
            crate::print_dec(rsum / rn);
            crate::uart_send_str(",max=");
            crate::print_dec(rmax);
            crate::uart_send_str(")us");
            // 关中断临界区占比：验收 fb_present IRQ 窗口收窄的观测点
            crate::uart_send_str(" 关中断(avg=");
            crate::print_dec(csum / rn);
            crate::uart_send_str(",max=");
            crate::print_dec(cmax);
            crate::uart_send_str(")us");
        }
        crate::uart_send_str("\n");

        // t2f：触控→上屏延迟分布（输入跟随帧）
        if tn > 0 {
            for i in 1..tn {
                let key = tb[i];
                let mut j = i;
                while j > 0 && tb[j - 1] > key {
                    tb[j] = tb[j - 1];
                    j -= 1;
                }
                tb[j] = key;
            }
            let tp95 = tb[(tn * 95 / 100).min(tn - 1)];
            crate::uart_send_str("[perf] t2f(n=");
            crate::print_dec(tcount);
            crate::uart_send_str(" min=");
            crate::print_dec(tmin);
            crate::uart_send_str(" avg=");
            crate::print_dec(tsum / tcount.max(1));
            crate::uart_send_str(" p95=");
            crate::print_dec(tp95);
            crate::uart_send_str(" max=");
            crate::print_dec(tmax);
            crate::uart_send_str("us\n");
        }
    }
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
    // 调度 deadline miss（帧错过归因：期限已过才被调度选中的次数）
    crate::uart_send_str(" dl_miss=");
    crate::print_dec(crate::sched_cfs::deadline_misses());
    // 当前报告窗口的 t2f 汇总（不冲刷；完整分布见周期性 [perf] 帧/t2f 行）
    unsafe {
        if T2F_COUNT > 0 {
            crate::uart_send_str(" t2f(n=");
            crate::print_dec(T2F_COUNT);
            crate::uart_send_str(" avg=");
            crate::print_dec(T2F_SUM / T2F_COUNT.max(1));
            crate::uart_send_str(" max=");
            crate::print_dec(T2F_MAX);
            crate::uart_send_str("us)");
        }
    }
    crate::uart_send_str("\n");
}
