//! 内核内存压力状态机（生命周期/LMK 的内核侧输入）。
//!
//! 定时器 tick（`sched_cfs::tick`）每 100ms 调用 [`poll`] 一次：按物理页
//! 空闲率计算压力等级并在等级跃迁时打印内核日志（可观测的"内核触发事件"）。
//! 用户态 lifd 经自定义系统调用 `SYS_LC_MEMPRESSURE`（404）或
//! `/proc/pressure` 读取当前等级，并按等级执行挂起/LMK 策略。
//!
//! 等级定义（与设计文档 docs/lifecycle.md 一致）：
//! - Normal（0）：空闲页 ≥ 25%
//! - Moderate（1）：10% ≤ 空闲页 < 25%  —— lifd 应挂起闲置后台应用、停发后台任务
//! - Critical（2）：空闲页 < 10%           —— lifd 执行 LMK（LRU + 优先级击杀）
//!
//! 支持测试注入：`handle_cmd("force 2")` 强制等级（`poll` 保持强制值直到
//! `clear`），供 QEMU 回归测试确定性触发 lifd 的挂起/LMK 策略，而不必真实
//! 耗尽 512MB 内存。强制通道经 `/proc/pressure` 写入：
//! `echo "force 2" > /proc/pressure` / `echo "clear" > /proc/pressure`。

/// 压力等级（返回给用户态的值）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PressureLevel {
    Normal = 0,
    Moderate = 1,
    Critical = 2,
}

/// 当前压力等级（由 tick 的 `poll` 更新；读路径仅读此值）。
static mut LEVEL: PressureLevel = PressureLevel::Normal;
/// tick 节流：每 100ms（10 个 10ms tick）计算一次。
const POLL_INTERVAL_US: u64 = 100_000;

/// 测试注入状态：-1 = 未注入（按真实空闲页计算），0/1/2 = 强制等级。
static mut FORCED: i8 = -1;

/// 当前压力等级（0/1/2）。`force` 未清除时返回强制值。
pub fn level() -> u8 {
    unsafe {
        if FORCED >= 0 {
            FORCED as u8
        } else {
            LEVEL as u8
        }
    }
}

/// 空闲页阈值（占总页比例，百分比）。
const MODERATE_THRESHOLD_PCT: u64 = 25; // < 25% 空闲 → Moderate
const CRITICAL_THRESHOLD_PCT: u64 = 10; // < 10% 空闲 → Critical

/// 上次计算时刻（µs，节流用）。
static mut LAST_POLL_US: u64 = 0;

/// tick 周期调用：按节流间隔（100ms）计算压力等级，等级变化时更新并打印日志。
///
/// 开销：每 100ms 一次位图扫描（used_pages，O(位图字数)≈ 数千次计数），
/// 远小于 10ms tick 预算；两次计算之间零开销（节流判断先行）。
/// 强制注入期间保持强制值，`clear` 后恢复按真实空闲页计算。
pub fn poll(now_us: u64) {
    unsafe {
        if now_us.saturating_sub(LAST_POLL_US) < POLL_INTERVAL_US {
            return;
        }
        LAST_POLL_US = now_us;
        let new = if FORCED >= 0 {
            if FORCED as u8 as u64 != LEVEL as u64 {
                PressureLevel::from_u8(FORCED as u8)
            } else {
                return;
            }
        } else {
            let total = crate::allocator::total_pages() as u64;
            if total == 0 {
                return;
            }
            let used = crate::allocator::used_pages() as u64;
            let free = total.saturating_sub(used);
            let free_pct = free * 100 / total;
            if free_pct < CRITICAL_THRESHOLD_PCT {
                PressureLevel::Critical
            } else if free_pct < MODERATE_THRESHOLD_PCT {
                PressureLevel::Moderate
            } else {
                PressureLevel::Normal
            }
        };
        if new != LEVEL {
            crate::uart_send_str("[mem] 压力 ");
            crate::print_dec(LEVEL as u64);
            crate::uart_send_str(" → ");
            crate::print_dec(new as u64);
            crate::uart_send_str("（强制=");
            crate::print_dec(FORCED.max(0) as u64);
            crate::uart_send_str("）\n");
            LEVEL = new;
        }
    }
}

impl PressureLevel {
    fn from_u8(v: u8) -> PressureLevel {
        match v {
            1 => PressureLevel::Moderate,
            2 => PressureLevel::Critical,
            _ => PressureLevel::Normal,
        }
    }
}

/// 处理 `/proc/pressure` 写命令（测试/运维注入）。
///
/// 支持：
/// - `force <0|1|2>`：强制压力等级（`poll` 保持该值，直到 `clear`）
/// - `clear`：取消强制，恢复按真实空闲页计算
///
/// 返回 0 = 成功；-1 = 命令不识别。
pub fn handle_cmd(args: &[&str]) -> i32 {
    if args.is_empty() {
        return -1;
    }
    match args[0] {
        "force" => {
            if args.len() < 2 {
                return -1;
            }
            let v = match args[1] {
                "0" => 0i8,
                "1" => 1i8,
                "2" => 2i8,
                _ => return -1,
            };
            unsafe {
                FORCED = v;
                LEVEL = PressureLevel::from_u8(v as u8);
            }
            crate::uart_send_str("[mem] 强制压力 ");
            crate::print_dec(v as u64);
            crate::uart_send_str("\n");
            0
        }
        "clear" => {
            unsafe {
                FORCED = -1;
            }
            crate::uart_send_str("[mem] 已清除强制（恢复按实测计算）\n");
            0
        }
        _ => -1,
    }
}

/// 供 procfs /proc/pressure 渲染。
pub fn describe(buf: &mut [u8]) -> usize {
    unsafe {
        let level = LEVEL as u64;
        let total = crate::allocator::total_pages() as u64;
        let free = crate::allocator::free_pages() as u64;
        let free_kb = free * 4096 / 1024;
        let total_kb = total * 4096 / 1024;
        let mut pos = 0usize;
        let s = b"level ";
        if pos + s.len() <= buf.len() {
            buf[pos..pos + s.len()].copy_from_slice(s);
            pos += s.len();
        }
        pos = push_dec(buf, pos, level);
        pos = push_str(buf, pos, " free_kB ");
        pos = push_dec(buf, pos, free_kb);
        pos = push_str(buf, pos, " total_kB ");
        pos = push_dec(buf, pos, total_kb);
        pos = push_str(buf, pos, "\n");
        pos
    }
}

fn push_dec(buf: &mut [u8], pos: usize, mut v: u64) -> usize {
    if v == 0 {
        if pos < buf.len() {
            buf[pos] = b'0';
            return pos + 1;
        }
        return pos;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    if pos + n <= buf.len() {
        buf[pos..pos + n].copy_from_slice(&tmp[i..]);
    }
    pos + n
}

fn push_str(buf: &mut [u8], pos: usize, s: &str) -> usize {
    let b = s.as_bytes();
    let n = b.len().min(buf.len().saturating_sub(pos));
    if n > 0 {
        buf[pos..pos + n].copy_from_slice(&b[..n]);
    }
    pos + n
}
