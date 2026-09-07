//! PL011 UART 驱动与 `print!` / `println!` 宏。
//!
//! QEMU virt：MMIO 0x0900_0000，时钟 24MHz。
//! Raspberry Pi 4：MMIO 0xFE20_1000，时钟 48MHz（骨架编译路径）。

use core::fmt::{self, Write};
use core::ptr::{read_volatile, write_volatile};

#[cfg(feature = "pi4")]
const UART_BASE: usize = 0xFE20_1000;
#[cfg(not(feature = "pi4"))]
const UART_BASE: usize = 0x0900_0000;

#[cfg(feature = "pi4")]
const UART_CLOCK: u32 = 48_000_000;
#[cfg(not(feature = "pi4"))]
const UART_CLOCK: u32 = 24_000_000;

/// UART 标志寄存器（FR）位：发送 FIFO 满。
const FR_TXFF: u32 = 1 << 5;

/// PL011 寄存器偏移。
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_IBRD: usize = 0x24;
const UART_FBRD: usize = 0x28;
const UART_LCR_H: usize = 0x2C;
const UART_CR: usize = 0x30;

/// 初始化 PL011：115200 8N1，使能 FIFO。
pub fn init() {
    unsafe {
        // 先关闭 UART，才能安全修改寄存器
        write_volatile((UART_BASE + UART_CR) as *mut u32, 0);

        // 波特率除数 = clock / (16 * baud)，小数部分 6 位
        let divisor = UART_CLOCK / (16 * 115200);
        let remainder = UART_CLOCK % (16 * 115200);
        let frac = (remainder * 64 + (16 * 115200) / 2) / (16 * 115200);
        write_volatile((UART_BASE + UART_IBRD) as *mut u32, divisor);
        write_volatile((UART_BASE + UART_FBRD) as *mut u32, frac);

        // 8N1：字长 8 位（WLEN=0b11），使能 FIFO（FEN）
        write_volatile((UART_BASE + UART_LCR_H) as *mut u32, (0b11 << 5) | (1 << 4));

        // 使能 UART（UARTEN）、发送（TXE）、接收（RXE）
        write_volatile((UART_BASE + UART_CR) as *mut u32, (1 << 0) | (1 << 8) | (1 << 9));
    }
}

/// 发送单个字符（把 `\n` 转换为 `\r\n`）；超时失败时静默丢弃该字符。
pub fn send_byte(c: u8) {
    if c == b'\n' {
        send_byte_raw(b'\r');
    }
    send_byte_raw(c);
}

/// 发送单个字符（不处理换行转换）；发送 FIFO 长时间不空（UART 挂起/不存在）
/// 时超时返回 false，调用方容忍失败（丢弃该字符，避免 bootloader 死循环）。
fn send_byte_raw(c: u8) -> bool {
    let mut spins = 0usize;
    // 等待发送 FIFO 非满（TXFF 位清零），带超时
    while unsafe { read_volatile((UART_BASE + UART_FR) as *const u32) } & FR_TXFF != 0 {
        core::hint::spin_loop();
        spins += 1;
        if spins > 100_000 {
            return false;
        }
    }
    unsafe {
        write_volatile((UART_BASE + UART_DR) as *mut u32, c as u32);
    }
    true
}

/// 发送字符串。
pub fn send_str(s: &str) {
    for b in s.bytes() {
        send_byte(b);
    }
}

/// 供 `write!` 使用的 UART 写入器。
pub struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        send_str(s);
        Ok(())
    }
}

/// 输出一个十六进制无符号数（64 位，8 位十六进制）。
pub fn print_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    for i in (0..16).rev() {
        let d = (v & 0xF) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
    }
    send_str(core::str::from_utf8(&buf).unwrap());
}

/// 输出一个十进制无符号数。
pub fn print_dec(mut v: u64) {
    if v == 0 {
        send_byte(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    send_str(core::str::from_utf8(&buf[i..]).unwrap());
}

/// 格式化打印（不带换行）。
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::uart::UartWriter, $($arg)*);
    }};
}

/// 格式化打印（带换行）。
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
