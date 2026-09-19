//! goldfish 实时时钟（RTC）驱动。
//!
//! 读取/写入自 Unix 纪元（1970-01-01 00:00:00 UTC）以来的秒数。
//! QEMU virt RISC-V 的 goldfish RTC 位于 0x101000；无 RTC 平台时 `init`
//! 保持 `EPOCH_OFFSET = 0`，此时 wall 时间退化为单调时钟（与旧行为一致）。
//!
//! 寄存器（goldfish-rtc）：
//!   TIME_LOW   0x00  秒低 32 位
//!   TIME_HIGH  0x04  秒高 32 位
//!   ALARM_LOW/HIGH 0x08/0x0C（未用：IRQ 使能恒关）
//!   IRQ        0x10（未用）

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

/// 自启动时刻的 Unix 纪元秒（启动时从 PL031 读取；为 0 表示无 RTC）。
static EPOCH_SECS: AtomicU64 = AtomicU64::new(0);

/// 初始化 RTC：读取启动时刻的纪元秒。
pub fn init(rtc_base: Option<usize>) {
    if let Some(base) = rtc_base {
        // 等待 RTC 有效（RTCDV 置位前读取可能返回 0；PL031 上电即有效，防御性循环）
        let mut epoch = read_secs(base);
        for _ in 0..4 {
            if epoch != 0 {
                break;
            }
            core::hint::spin_loop();
            epoch = read_secs(base);
        }
        if epoch != 0 {
            EPOCH_SECS.store(epoch, Ordering::Relaxed);
            crate::kinfo! {
                crate::uart_send_str("[rtc] goldfish epoch=");
                crate::print_dec(epoch);
                // 自检：打印当前墙钟日历时间（验证 civil_from_epoch）
                let wall = wall_time_us() / 1_000_000;
                let mut c = [0i32; 6];
                civil_from_epoch(wall, &mut c);
                crate::uart_send_str(" wall=");
                crate::print_dec(c[0] as u64);
                crate::uart_send_str("-");
                if c[1] < 10 { crate::uart_send_str("0"); }
                crate::print_dec(c[1] as u64);
                crate::uart_send_str("-");
                if c[2] < 10 { crate::uart_send_str("0"); }
                crate::print_dec(c[2] as u64);
                crate::uart_send_str(" ");
                if c[3] < 10 { crate::uart_send_str("0"); }
                crate::print_dec(c[3] as u64);
                crate::uart_send_str(":");
                if c[4] < 10 { crate::uart_send_str("0"); }
                crate::print_dec(c[4] as u64);
                crate::uart_send_str(":");
                if c[5] < 10 { crate::uart_send_str("0"); }
                crate::print_dec(c[5] as u64);
                crate::uart_send_str("\n");
            }
        } else {
            crate::kinfo! { crate::uart_send_str("[rtc] 存在 RTC 但 epoch=0，改用单调时钟\n"); }
        }
    } else {
        crate::kinfo! { crate::uart_send_str("[rtc] 无 RTC 设备，使用单调时钟\n"); }
    }
}

/// 读取 PL031 Data 寄存器（秒）。
fn read_secs(base: usize) -> u64 {
    unsafe {
        let lo = read_volatile((base + 0x00) as *const u32) as u64;
        let hi = read_volatile((base + 0x04) as *const u32) as u64;
        lo | (hi << 32)
    }
}

/// 设置当前时间（秒）：写 Load 寄存器并确认。
pub fn set_epoch_secs(secs: u64) {
    if let Some(base) = rtc_base_of() {
        unsafe {
            write_volatile((base + 0x00) as *mut u32, (secs & 0xFFFF_FFFF) as u32);
            write_volatile((base + 0x04) as *mut u32, (secs >> 32) as u32);
        }
        EPOCH_SECS.store(secs, Ordering::Relaxed);
    }
}

/// 返回 RTC 基址（内核自检用）。
fn rtc_base_of() -> Option<usize> {
    kunxhal::current_platform().rtc_base()
}

/// 自启动时刻的纪元秒（0 表示无 RTC）。
pub fn boot_epoch_secs() -> u64 {
    EPOCH_SECS.load(Ordering::Relaxed)
}

/// 当前墙钟时间（微秒）：`纪元秒 × 1e6 + 单调时钟`。
/// 无 RTC 时退化为单调时钟（仅 uptime）。
pub fn wall_time_us() -> u64 {
    let epoch = EPOCH_SECS.load(Ordering::Relaxed);
    let up = crate::timer::now_us();
    if epoch == 0 {
        up
    } else {
        epoch.saturating_mul(1_000_000).saturating_add(up)
    }
}

// ==================== 日历换算（Howard Hinnant 算法） ====================

/// 把 Unix 秒换算成日历字段，写入 `out`：年、月、日、时、分、秒。
pub fn civil_from_epoch(secs: u64, out: &mut [i32; 6]) {
    let days = (secs / 86400) as i64;
    let tod = (secs % 86400) as i64;
    out[3] = (tod / 3600) as i32;
    out[4] = ((tod % 3600) / 60) as i32;
    out[5] = (tod % 60) as i32;

    // days → (y, m, d)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    out[0] = (if m <= 2 { y + 1 } else { y }) as i32;
    out[1] = m as i32;
    out[2] = d as i32;
}
