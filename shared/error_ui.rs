//! KunxOS 统一错误提示状态（no_std、无堆、无依赖）。
//!
//! 只维护"错误提示"的纯状态：消息内容、设置时刻、显示时长与清除。
//! 绘制由各应用完成（应用各自持有 back buffer 与绘制原语），
//! 通过 `err_active` / `err_msg` 取状态，再以自己的 `fill_rect` /
//! `draw_text` 画出横幅；app_kit 使用方可直接用 `kit::show_error` 与
//! `kit::draw_error_banner()`。
//!
//! 约定：
//! - 消息为 UTF-8 中文（≤ `ERR_MAX` 字节），超长截断。
//! - 自动消失：设置后 `ERR_DURATION_US` 微秒内有效。
//! - 再次 `err_set` 会覆盖旧消息并重置计时。

pub const ERR_MAX: usize = 64;
pub const ERR_DURATION_US: u64 = 4_000_000; // 4 秒

static mut ERR_BUF: [u8; ERR_MAX] = [0u8; ERR_MAX];
static mut ERR_LEN: usize = 0;
static mut ERR_US: u64 = 0;
static mut ERR_TICKS: u32 = 0; // 无墙钟应用（如 OOBE）用的帧计数剩余量

/// 设置错误提示（`now_us` 为当前墙钟微秒，由调用方传入）。
pub fn err_set(msg: &[u8], now_us: u64) {
    unsafe {
        let mut n = msg.len().min(ERR_MAX);
        // 截断回退到 UTF-8 字符边界：仅当实际发生截断时，检查末尾是否
        // 落在多字节序列内部 —— 数出末尾续字节数 k，再看起始字节声明的
        // 序列长度是否等于 k+1（完整则保留，不完整则整段丢弃）。
        if n < msg.len() && n > 0 {
            let mut k = 0usize;
            while k < 3 && k < n && (msg[n - 1 - k] & 0xC0) == 0x80 {
                k += 1;
            }
            let complete = if k > 0 && n > k {
                let b = msg[n - 1 - k];
                let expect = if b >= 0xF0 {
                    4
                } else if b >= 0xE0 {
                    3
                } else if b >= 0xC0 {
                    2
                } else {
                    0
                };
                expect == k + 1
            } else {
                false
            };
            if !complete {
                n -= k;
                if n > 0 && (msg[n - 1] & 0xC0) == 0xC0 {
                    n -= 1;
                }
            }
        }
        ERR_BUF[..n].copy_from_slice(&msg[..n]);
        ERR_LEN = n;
        if now_us == 0 {
            // 墙钟不可用（now_us 失败返回 0）：退化为帧计数模式 —— 否则
            // ERR_US=0 使 err_active 恒为 false，错误横幅永不显示。
            ERR_US = 0;
            ERR_TICKS = (ERR_DURATION_US / 16_000) as u32; // 按 16ms/帧折算
        } else {
            ERR_US = now_us;
            ERR_TICKS = 0;
        }
    }
}

/// 设置错误提示（无墙钟应用使用：按帧计数，`ticks` 帧后消失）。
pub fn err_set_ticks(msg: &[u8], ticks: u32) {
    unsafe {
        let n = msg.len().min(ERR_MAX);
        ERR_BUF[..n].copy_from_slice(&msg[..n]);
        ERR_LEN = n;
        ERR_US = 0;
        ERR_TICKS = ticks.max(1);
    }
}

/// 无墙钟应用在主循环每帧调用一次，推进错误提示计时。
pub fn err_tick() {
    unsafe {
        if ERR_TICKS > 0 {
            ERR_TICKS -= 1;
        }
    }
}

/// 立即清除错误提示。
pub fn err_clear() {
    unsafe {
        ERR_LEN = 0;
        ERR_US = 0;
        ERR_TICKS = 0;
    }
}

/// 错误提示当前是否有效（设置过且未超时）。
pub fn err_active(now_us: u64) -> bool {
    unsafe {
        (ERR_TICKS > 0 || (ERR_US > 0 && now_us.saturating_sub(ERR_US) <= ERR_DURATION_US))
            && ERR_LEN > 0
    }
}

/// 取当前错误消息（未设置或已过期返回空切片）。
pub fn err_msg() -> &'static [u8] {
    unsafe {
        if err_active(ERR_US) && ERR_LEN > 0 {
            &ERR_BUF[..ERR_LEN]
        } else {
            &[]
        }
    }
}

/// 剩余显示时长（微秒），用于需要淡出/倒计时的地方。
pub fn err_remaining_us(now_us: u64) -> u64 {
    unsafe {
        if ERR_LEN == 0 {
            0
        } else {
            ERR_DURATION_US.saturating_sub(now_us.saturating_sub(ERR_US))
        }
    }
}
