//! 结构化内核日志（printk 等价物）：级别 + 模块标签 + 时间戳，环形缓冲。
//!
//! - [`log_write`]：把一条 `(level, tag, msg)` 记录写入环形缓冲（IRQ 安全）。
//! - [`printk`] 宏：`printk!(level, tag, "fmt", ...)`，同时写入结构化缓冲
//!   并镜像到 UART 控制台（因此也会进入 KMSG_BUF 原始控制台捕获）。
//! - `/dev/kmsg` 字符设备（major 11, minor 1）：
//!   - `read`：按全局游标增量读取下一条记录（供 logd 守护进程 tail）。
//!   - `write`：注入用户态日志（`<level>[tag] msg` 或 `<level> tag: msg`），
//!     镜像到控制台 —— logcat 等价物的内核侧通道。
//! - [`log_snapshot`]：完整快照（供 shell `log` / `dmesg` 命令）。
//!
//! 与 `KMSG_BUF`（原始字节环形，捕获一切控制台输出）互补：KMSG_BUF 保留
//! 启动过程全部输出；本模块提供带级别/标签/时间戳的结构化视图。

use core::arch::asm;
use core::fmt::Write;

/// 日志级别（Linux kernel 语义）。
pub const LOG_EMERG: u8 = 0;
pub const LOG_ALERT: u8 = 1;
pub const LOG_CRIT: u8 = 2;
pub const LOG_ERR: u8 = 3;
pub const LOG_WARNING: u8 = 4;
pub const LOG_NOTICE: u8 = 5;
pub const LOG_INFO: u8 = 6;
pub const LOG_DEBUG: u8 = 7;

/// 级别名称（显示用）。
pub fn level_name(level: u8) -> &'static str {
    match level {
        0 => "EMERG",
        1 => "ALERT",
        2 => "CRIT",
        3 => "ERR",
        4 => "WARNING",
        5 => "NOTICE",
        6 => "INFO",
        _ => "DEBUG",
    }
}

/// 一条结构化日志记录（定长，环形缓冲元素）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LogRec {
    /// 日志级别 0..=7（0xFF = 空槽位）。
    pub level: u8,
    /// 模块标签（NUL 结尾）。
    pub tag: [u8; 15],
    /// 写入时刻（单调微秒）。
    pub ts_us: u64,
    /// 消息长度。
    pub len: u16,
    /// 消息正文。
    pub msg: [u8; 120],
}

const EMPTY_REC: LogRec = LogRec {
    level: 0xFF,
    tag: [0; 15],
    ts_us: 0,
    len: 0,
    msg: [0; 120],
};

/// 环形缓冲容量（256 × 144B ≈ 36KB .bss）。
const LOG_CAP: usize = 256;
static mut RING: [LogRec; LOG_CAP] = [EMPTY_REC; LOG_CAP];
/// 最旧记录下标。
static mut HEAD: usize = 0;
/// 有效记录数（≤ LOG_CAP）。
static mut COUNT: usize = 0;
/// `/dev/kmsg` 全局读游标（logd 增量读取；多读者场景可扩展为 per-open）。
static mut READ_CURSOR: usize = 0;

/// 写入一条日志记录（IRQ 安全：保存/恢复 DAIF，可从任意上下文调用）。
pub fn log_write(level: u8, tag: &str, msg: &str) {
    let saved: u64;
    unsafe { asm!("csrr {}, sstatus", out(reg) saved) };
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        let idx = (HEAD + COUNT) % LOG_CAP;
        let rec = &mut RING[idx];
        rec.level = level.min(7);
        rec.tag.fill(0);
        let tl = tag.len().min(14);
        rec.tag[..tl].copy_from_slice(&tag.as_bytes()[..tl]);
        rec.ts_us = crate::timer::now_us();
        let ml = msg.len().min(rec.msg.len());
        rec.msg[..ml].copy_from_slice(&msg.as_bytes()[..ml]);
        rec.len = ml as u16;
        if COUNT == LOG_CAP {
            HEAD = (HEAD + 1) % LOG_CAP;
        } else {
            COUNT += 1;
        }
        // 游标同步：若游标正落在被覆盖的最旧记录上，前移一位
        if COUNT == LOG_CAP && READ_CURSOR == HEAD {
            READ_CURSOR = (READ_CURSOR + 1) % LOG_CAP;
        }
    }
    unsafe {
        asm!("csrw sstatus, {}", in(reg) saved);
    }
}

/// 把一条记录格式化为 `"<level> <ts_us> <tag>: <msg>
/// ` 写入 `out`，
/// 返回写入字节数。
fn fmt_rec(rec: &LogRec, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    // <level>
    if n < out.len() {
        out[n] = b'<';
        n += 1;
    }
    if n < out.len() {
        out[n] = b'0' + rec.level.min(9);
        n += 1;
    }
    if n < out.len() {
        out[n] = b'>';
        n += 1;
    }
    // 时间戳（十进制微秒）
    let mut ts = rec.ts_us;
    let mut tb = [0u8; 20];
    let mut ti = tb.len();
    loop {
        ti -= 1;
        tb[ti] = b'0' + (ts % 10) as u8;
        ts /= 10;
        if ts == 0 {
            break;
        }
    }
    let ts_str = core::str::from_utf8(&tb[ti..]).unwrap_or("");
    let copy = |s: &str, out: &mut [u8], n: &mut usize| {
        for &b in s.as_bytes() {
            if *n < out.len() {
                out[*n] = b;
                *n += 1;
            }
        }
    };
    copy(ts_str, out, &mut n);
    copy(" ", out, &mut n);
    let tlen = rec.tag.iter().position(|&c| c == 0).unwrap_or(rec.tag.len());
    copy(core::str::from_utf8(&rec.tag[..tlen]).unwrap_or(""), out, &mut n);
    copy(": ", out, &mut n);
    let mlen = (rec.len as usize).min(rec.msg.len());
    copy(core::str::from_utf8(&rec.msg[..mlen]).unwrap_or(""), out, &mut n);
    copy("\n", out, &mut n);
    n
}

/// 完整快照（按时间顺序）写入 `out`，返回字节数。
pub fn log_snapshot(out: &mut [u8]) -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..COUNT {
            let rec = &RING[(HEAD + i) % LOG_CAP];
            let start = n.min(out.len());
            n += fmt_rec(rec, &mut out[start..]);
            if n >= out.len() {
                break;
            }
        }
    }
    n
}

/// 增量读取下一条未读记录（全局游标，供 logd）。无新记录返回 0。
pub fn log_read_next(out: &mut [u8]) -> usize {
    unsafe {
        // 游标落后于 HEAD 时（缓冲整体覆盖过）追赶
        let mut idx = READ_CURSOR;
        let mut guard = 0usize;
        while COUNT > 0 && guard < LOG_CAP {
            let head = HEAD % LOG_CAP;
            let cursor_rec = (idx) % LOG_CAP;
            // 环形里 cursor 是否在 [HEAD, HEAD+COUNT) 内
            let rel = (cursor_rec + LOG_CAP - head) % LOG_CAP;
            if rel < COUNT {
                let rec = &RING[cursor_rec];
                let n = fmt_rec(rec, out);
                READ_CURSOR = (cursor_rec + 1) % LOG_CAP;
                return n;
            }
            idx = (idx + 1) % LOG_CAP;
            guard += 1;
        }
        0
    }
}

/// 从用户态（`/dev/kmsg` write）注入一条日志。
///
/// 支持两种格式：`<level>[tag] msg`（Linux 风格）或 `<level> tag: msg`；
/// 无 `<level>` 前缀时按 INFO。镜像到 UART 控制台（进入 KMSG_BUF）。
pub fn log_inject(buf: &[u8]) -> i32 {
    let mut level = LOG_INFO;
    let mut rest = buf;
    if buf.first() == Some(&b'<') {
        if let Some(gt) = buf.iter().position(|&c| c == b'>') {
            if gt == 2 && buf[1].is_ascii_digit() {
                level = (buf[1] - b'0').min(7);
                rest = &buf[gt + 1..];
            }
        }
    }
    // 解析标签：`[tag] msg` 或 `tag: msg`
    let mut tag = "user";
    let mut msg = rest;
    let r = rest;
    if r.first() == Some(&b'[') {
        if let Some(close) = r.iter().position(|&c| c == b']') {
            tag = core::str::from_utf8(&r[1..close]).unwrap_or("user");
            msg = &r[close + 1..];
            while msg.first() == Some(&b' ') {
                msg = &msg[1..];
            }
        }
    } else if let Some(colon) = r.iter().position(|&c| c == b':') {
        if colon > 0 && colon < 32 && r[..colon].iter().all(|&c| c.is_ascii_graphic()) {
            tag = core::str::from_utf8(&r[..colon]).unwrap_or("user");
            msg = &r[colon + 1..];
            while msg.first() == Some(&b' ') {
                msg = &msg[1..];
            }
        }
    }
    let msg_str = core::str::from_utf8(msg).unwrap_or("").trim_end();
    log_write(level, tag, msg_str);
    // 镜像到控制台：`[tag] msg`
    crate::uart_send_str("[");
    crate::uart_send_str(tag);
    crate::uart_send_str("] ");
    crate::uart_send_str(msg_str);
    crate::uart_send_str("\n");
    buf.len() as i32
}

// ==================== printk 宏 ====================

/// 写入格式化字符串到固定缓冲的适配器。
struct BufWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Write for BufWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len().saturating_sub(self.len));
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// 格式化一条日志并写入结构化缓冲 + 镜像 UART 控制台。
///
/// 控制台镜像受安静模式控制（[`crate::console_quiet`]）：安静模式下仅
/// WARNING 及更高级别上串口，信息级日志只进结构化缓冲（shell `log` 可查）。
pub fn printk_fmt(level: u8, tag: &str, args: core::fmt::Arguments) {
    let mut b = [0u8; 144];
    let mut w = BufWriter { buf: &mut b, len: 0 };
    let _ = w.write_fmt(args);
    let wlen = w.len;
    drop(w); // 结束对 b 的可变借用后再读取
    let s = core::str::from_utf8(&b[..wlen]).unwrap_or("");
    log_write(level, tag, s);
    // 控制台镜像：`[tag] msg`（安静模式下仅告警以上输出）
    if !crate::console_quiet() || level <= crate::log::LOG_WARNING {
        crate::uart_send_str("[");
        crate::uart_send_str(tag);
        crate::uart_send_str("] ");
        crate::uart_send_str(s);
        crate::uart_send_str("\n");
    }
}

/// `printk!(LEVEL, "tag", "fmt", ...)`：结构化日志 + 控制台镜像。
#[macro_export]
macro_rules! printk {
    ($level:expr, $tag:expr, $($arg:tt)*) => {
        $crate::log::printk_fmt($level, $tag, core::format_args!($($arg)*))
    };
}

/// 从结构化缓冲读出的记录数（诊断/测试）。
pub fn log_count() -> usize {
    unsafe { COUNT }
}
