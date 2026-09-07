//! Modem 硬件抽象：`/dev/modem` 与 `/dev/modemctl` 字符设备。
//!
//! 架构（RIL 等价物 rild 在用户态，本模块只提供 AT 字节流传输）：
//!
//! ```text
//!   rild（AT 命令 / URC）⇄ /dev/modem ⇄ modem 后端
//!                                           ├─ QEMU：内置虚拟调制解调器
//!                                           │   （模拟 AT 响应 + 可注入来电/短信）
//!                                           └─ Pi4：PL011 UART2（GPIO 外接 USB-TTL → Quectel EC25 等）
//!   测试 / 演示          ⇄ /dev/modemctl  （仅虚拟调制解调器支持事件注入）
//! ```
//!
//! 设计要点：
//! - `/dev/modem`（次设备号 0）：AT 字节流。`read` 非阻塞返回调制解调器
//!   → 主机的字节（URC 与命令响应）；`write` 把 AT 命令交给后端。
//! - `/dev/modemctl`（次设备号 1）：写一行控制指令，向虚拟调制解调器注入
//!   网络侧事件（来电 / 应答 / 挂断 / 短信 / 信号 / SIM 状态），使 QEMU 下
//!   可以端到端演练来电与短信流程；Pi4 真机后端返回 -1（事件来自真实网络）。
//! - 虚拟调制解调器支持 RIL 所需的精简 AT 命令集（AT/ATE/CSQ/COPS/CREG/
//!   CPIN/CSCA/CMGF/CMGS/CMGR/CMGL/CMGD/CNMI/CLCC/ATD/ATH/ATA），
//!   并主动上报 URC（+CRING / +CMTI / NO CARRIER）。
//! - Pi4 后端为轮询式 PL011（无中断）：rild 以 ~50ms 周期轮询 /dev/modem，
//!   足以覆盖短信与电路域语音信令（本设计不追求高速数据）。

// 仅 Pi4 UART2 后端使用（qemu 构建下避免 unused_imports 警告）
#[cfg(feature = "pi4")]
use core::ptr::{read_volatile, write_volatile};

use crate::devfs;

// ==================== 常量 ====================

/// 虚拟调制解调器收件箱槽位数（AT+CMGR/CMGL/CMGD 操作该存储）。
const SM_SLOTS: usize = 8;
/// 单条短信 PDU 最大长度（含 SMSC 字段；标准上限 176 字节，留余量）。
const PDU_MAX: usize = 192;
/// 存储的 PDU 十六进制串上限（PDU_MAX 字节 × 2 字符）。
const PDU_HEX_MAX: usize = 420;
/// 主机 → 调制解调器的命令行缓冲。
const RX_LINE_CAP: usize = 160;
/// 调制解调器 → 主机的发送队列（URC 与响应）。
const TX_QUEUE_CAP: usize = 2048;

/// 紧急号码集合（无 SIM 时仍允许拨出，3GPP TS 22.101）。
const EMERGENCY: [&[u8]; 6] = [b"112", b"911", b"110", b"119", b"120", b"122"];

// ---- Pi4 第二路 PL011（UART2，BCM2711 0xFE201400）寄存器 ----
const PI4_UART2_BASE: usize = 0xFE20_1400;
const REG_DR: usize = 0x00;
const REG_FR: usize = 0x18;
const REG_IBRD: usize = 0x24;
const REG_FBRD: usize = 0x28;
const REG_LCR_H: usize = 0x2C;
const REG_CR: usize = 0x30;
const FR_RXFE: u32 = 1 << 4; // 接收 FIFO 空
const FR_TXFF: u32 = 1 << 5; // 发送 FIFO 满

// ==================== 全局状态 ====================

/// 是否使用虚拟调制解调器（QEMU）而非真实 UART（Pi4）。
static mut VIRTUAL: bool = false;
/// 后端是否已初始化。
static mut READY: bool = false;

// ---- 发送队列（调制解调器 → 主机） ----
static mut TX_QUEUE: [u8; TX_QUEUE_CAP] = [0; TX_QUEUE_CAP];
static mut TX_HEAD: usize = 0;
static mut TX_COUNT: usize = 0;

// ---- 命令行接收（主机 → 调制解调器） ----
static mut RX_LINE: [u8; RX_LINE_CAP] = [0; RX_LINE_CAP];
static mut RX_LEN: usize = 0;

// ---- AT+CMGS PDU 数据接收状态 ----
static mut CMGS_EXPECT: usize = 0; // >0 表示正在接收 PDU（期望总字节数）
static mut CMGS_BUF: [u8; PDU_MAX] = [0; PDU_MAX];
static mut CMGS_LEN: usize = 0;
static mut MR: u8 = 0; // 消息引用号（AT+CMGS 递增）

// ---- 短信存储（虚拟调制解调器收件箱） ----
#[derive(Clone, Copy)]
/// 短信存储槽：`data` 保存 PDU 十六进制串（+CMGR 按 3GPP 27.005 原样返回），
/// `len` 为十六进制字符数。
struct SmsSlot {
    len: usize,
    data: [u8; PDU_HEX_MAX],
}
static mut SM: [SmsSlot; SM_SLOTS] = [SmsSlot { len: 0, data: [0; PDU_HEX_MAX] }; SM_SLOTS];
static mut SM_COUNT: usize = 0;

// ---- 呼叫状态（CLCC 语义） ----
static mut CALL_ACTIVE: bool = false;
static mut CALL_DIR: u8 = 0; // 0 = 主叫(MO)，1 = 被叫(MT)
static mut CALL_STAT: u8 = 0; // 0 active, 2 dialing, 3 ringing
static mut CALL_NUM: [u8; 24] = [0; 24];
static mut CALL_NUM_LEN: usize = 0;

// ---- SIM / 网络状态（可经 modemctl 注入） ----
static mut SIM_ABSENT: bool = false;
static mut SIGNAL: u8 = 20; // +CSQ rssi 0..31
static mut OPERATOR: [u8; 16] = *b"CMCC\0\0\0\0\0\0\0\0\0\0\0\0";
static mut OPERATOR_LEN: usize = 4;
static mut REG: u8 = 1; // +CREG 状态（1 = 已注册家庭网）
static mut SMSC: [u8; 24] = *b"+8613800100500\0\0\0\0\0\0\0\0\0\0";
static mut SMSC_LEN: usize = 14;

// ==================== 基础工具 ====================

fn hex_nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 把十六进制字符串解码到 `out`，返回字节数（长度必须为偶数，超长截断）。
fn hex_decode(s: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i + 1 < s.len() && n < out.len() {
        let (Some(hi), Some(lo)) = (hex_nib(s[i]), hex_nib(s[i + 1])) else {
            break;
        };
        out[n] = (hi << 4) | lo;
        n += 1;
        i += 2;
    }
    n
}

/// 追加字节到发送队列（丢弃溢出，AT 流量远小于队列容量）。
fn tx_push(data: &[u8]) {
    unsafe {
        for &b in data {
            if TX_COUNT >= TX_QUEUE_CAP {
                break;
            }
            let pos = (TX_HEAD + TX_COUNT) % TX_QUEUE_CAP;
            TX_QUEUE[pos] = b;
            TX_COUNT += 1;
        }
    }
}

fn tx_push_str(s: &str) {
    tx_push(s.as_bytes());
}

/// 从发送队列取走最多 `buf.len()` 字节，返回字节数。
fn tx_pop(buf: &mut [u8]) -> usize {
    unsafe {
        let n = buf.len().min(TX_COUNT);
        for i in 0..n {
            buf[i] = TX_QUEUE[(TX_HEAD + i) % TX_QUEUE_CAP];
        }
        TX_HEAD = (TX_HEAD + n) % TX_QUEUE_CAP;
        TX_COUNT -= n;
        n
    }
}

fn ok() {
    tx_push_str("\r\nOK\r\n");
}

fn err(cme: u32) {
    let mut buf = [0u8; 24];
    let s = format_cme(cme, &mut buf);
    tx_push(s);
}

/// 写入 "+CME ERROR: <n>\r\n" 到 buf，返回切片。
fn format_cme(code: u32, buf: &mut [u8]) -> &[u8] {
    let head = b"+CME ERROR: ";
    let mut n = 0;
    for &b in head {
        if n < buf.len() {
            buf[n] = b;
            n += 1;
        }
    }
    // 十进制
    let mut tmp = [0u8; 10];
    let mut t = tmp.len();
    let mut v = code;
    if v == 0 {
        t -= 1;
        tmp[t] = b'0';
    } else {
        while v > 0 && t > 0 {
            t -= 1;
            tmp[t] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    for &b in &tmp[t..] {
        if n < buf.len() {
            buf[n] = b;
            n += 1;
        }
    }
    if n + 2 <= buf.len() {
        buf[n] = b'\r';
        buf[n + 1] = b'\n';
        n += 2;
    }
    &buf[..n]
}

fn starts_with_ci(a: &[u8], prefix: &[u8]) -> bool {
    a.len() >= prefix.len() && a[..prefix.len()].iter().zip(prefix).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// 数字字符串是否为紧急号码。
fn is_emergency(num: &[u8]) -> bool {
    EMERGENCY.iter().any(|e| *e == num)
}

/// 提取 ATD 的目标号码：去掉前缀与结尾分号/空白。
fn dial_number(line: &[u8]) -> &[u8] {
    let mut s = &line[3..];
    while s.last() == Some(&b';') || s.last() == Some(&b' ') || s.last() == Some(&b'\r') {
        s = &s[..s.len() - 1];
    }
    s
}

fn parse_index(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0usize;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as usize;
    }
    Some(v)
}

/// 解析带引号的字符串参数（如 AT+CSCA="+86..."），返回引号内内容。
fn parse_quoted<'a>(line: &'a [u8], after: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(after)?;
    let rest = rest.strip_prefix(b"=")?;
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..end])
}

// ==================== 虚拟调制解调器：AT 命令处理 ====================

/// 处理一行完整 AT 命令（不含行尾 CR/LF）。
fn process_line(line: &[u8]) {
    // 空行或非 AT 前缀
    if line.is_empty() {
        return;
    }
    if !starts_with_ci(line, b"AT") {
        err(100);
        return;
    }

    if line.eq_ignore_ascii_case(b"AT") || starts_with_ci(line, b"ATE") {
        ok();
    } else if starts_with_ci(line, b"AT+CMEE") {
        ok();
    } else if starts_with_ci(line, b"AT+CMGF") {
        // 固定 PDU 模式（RIL 只用 PDU）
        if line.contains(&b'?') {
            tx_push_str("\r\n+CMGF: 0\r\n");
        }
        ok();
    } else if starts_with_ci(line, b"AT+CSQ") {
        let sig = unsafe { SIGNAL };
        tx_push_str("\r\n+CSQ: ");
        push_dec(sig as u32);
        tx_push_str(",0\r\n");
        ok();
    } else if starts_with_ci(line, b"AT+COPS") {
        if line.contains(&b'?') {
            tx_push_str("\r\n+COPS: 0,0,\"");
            unsafe { tx_push(&OPERATOR[..OPERATOR_LEN]) };
            tx_push_str("\"\r\n");
        }
        ok();
    } else if starts_with_ci(line, b"AT+CREG") {
        if line.contains(&b'?') {
            tx_push_str("\r\n+CREG: 0,");
            push_dec(unsafe { REG } as u32);
            tx_push_str("\r\n");
        }
        ok();
    } else if starts_with_ci(line, b"AT+CPIN") {
        if line.contains(&b'?') {
            if unsafe { SIM_ABSENT } {
                err(10); // SIM not inserted
            } else {
                tx_push_str("\r\n+CPIN: READY\r\n");
                ok();
            }
        } else {
            // AT+CPIN="1234"：接受任意 PIN
            ok();
        }
    } else if starts_with_ci(line, b"AT+CSCA") {
        if line.contains(&b'?') {
            tx_push_str("\r\n+CSCA: \"");
            unsafe { tx_push(&SMSC[..SMSC_LEN]) };
            tx_push_str("\",145\r\n");
            ok();
        } else if let Some(num) = parse_quoted(line, b"AT+CSCA") {
            unsafe {
                SMSC_LEN = num.len().min(SMSC.len());
                SMSC[..SMSC_LEN].copy_from_slice(&num[..SMSC_LEN]);
            }
            ok();
        } else {
            err(100);
        }
    } else if starts_with_ci(line, b"AT+CMGS=") {
        let rest = &line[b"AT+CMGS=".len()..];
        match parse_index(rest) {
            Some(len) if len <= PDU_MAX => {
                unsafe {
                    CMGS_EXPECT = len;
                    CMGS_LEN = 0;
                }
                tx_push_str("\r\n> "); // 数据提示符（无行尾换行）
            }
            _ => err(100),
        }
    } else if starts_with_ci(line, b"AT+CMGR=") {
        let rest = &line[b"AT+CMGR=".len()..];
        match parse_index(rest) {
            Some(idx) if idx >= 1 && idx <= unsafe { SM_COUNT } => {
                let slot = unsafe { &SM[idx - 1] };
                tx_push_str("\r\n+CMGR: 0,,");
                push_dec((slot.len / 2) as u32); // 长度按 PDU 字节数上报
                tx_push_str("\r\n");
                unsafe { tx_push(&slot.data[..slot.len]) };
                tx_push_str("\r\n");
                ok();
            }
            _ => {
                tx_push_str("\r\n+CMS ERROR: 321\r\n");
            }
        }
    } else if starts_with_ci(line, b"AT+CMGL=") {
        // 仅支持 AT+CMGL=4（列出全部已收短信）
        let rest = &line[b"AT+CMGL=".len()..];
        if rest == b"4" {
            unsafe {
                for i in 0..SM_COUNT {
                    let slot = &SM[i];
                    tx_push_str("\r\n+CMGL: ");
                    push_dec((i + 1) as u32);
                    tx_push_str(",0,,");
                    push_dec((slot.len / 2) as u32);
                    tx_push_str("\r\n");
                    tx_push(&slot.data[..slot.len]);
                    tx_push_str("\r\n");
                }
            }
            ok();
        } else {
            err(100);
        }
    } else if starts_with_ci(line, b"AT+CMGD=") {
        let rest = &line[b"AT+CMGD=".len()..];
        match parse_index(rest) {
            Some(idx) if idx >= 1 && idx <= unsafe { SM_COUNT } => {
                unsafe { sm_delete(idx) };
                ok();
            }
            _ => err(100),
        }
    } else if starts_with_ci(line, b"AT+CNMI=") {
        ok();
    } else if starts_with_ci(line, b"AT+CLCC") {
        if unsafe { CALL_ACTIVE } {
            tx_push_str("\r\n+CLCC: 1,");
            push_dec(unsafe { CALL_DIR } as u32);
            tx_push_str(",");
            push_dec(unsafe { CALL_STAT } as u32);
            tx_push_str(",0,0,\"");
            unsafe { tx_push(&CALL_NUM[..CALL_NUM_LEN]) };
            tx_push_str("\",129\r\n");
        }
        ok();
    } else if starts_with_ci(line, b"ATD") {
        let num = dial_number(line);
        unsafe {
            if SIM_ABSENT && !is_emergency(num) {
                err(10); // 无 SIM 仅允许紧急呼叫
            } else if CALL_ACTIVE {
                err(100);
            } else if num.is_empty() {
                err(100);
            } else {
                CALL_ACTIVE = true;
                CALL_DIR = 0;
                CALL_STAT = 2; // dialing
                CALL_NUM_LEN = num.len().min(CALL_NUM.len());
                CALL_NUM[..CALL_NUM_LEN].copy_from_slice(&num[..CALL_NUM_LEN]);
                ok();
            }
        }
    } else if starts_with_ci(line, b"ATH") {
        unsafe { CALL_ACTIVE = false };
        ok();
    } else if starts_with_ci(line, b"ATA") {
        unsafe {
            if CALL_ACTIVE && CALL_STAT == 3 {
                CALL_STAT = 0; // answer → active
                ok();
            } else {
                err(100);
            }
        }
    } else if starts_with_ci(line, b"ATZ") || starts_with_ci(line, b"AT&F") {
        ok();
    } else {
        err(100);
    }
}

fn push_dec(mut v: u32) {
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    if v == 0 {
        tmp[i - 1] = b'0';
        tx_push(&tmp[i - 1..]);
        return;
    }
    while v > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    tx_push(&tmp[i..]);
}

/// 删除第 `idx` 条（1 起）短信存储记录，后续前移。
unsafe fn sm_delete(idx: usize) {
    if idx < 1 || idx > SM_COUNT {
        return;
    }
    for i in idx..SM_COUNT {
        SM[i - 1] = SmsSlot { len: SM[i].len, data: SM[i].data };
    }
    SM_COUNT -= 1;
    SM[SM_COUNT] = SmsSlot { len: 0, data: [0; PDU_HEX_MAX] };
}

// ==================== modemctl 事件注入 ====================

/// 处理一行注入命令（来自 /dev/modemctl 的 write）。
fn process_ctl(line: &[u8]) {
    let (key, value) = match line.iter().position(|&b| b == b':') {
        Some(p) => (&line[..p], &line[p + 1..]),
        None => (line, &[][..]),
    };
    match key {
        b"CALL_IN" => {
            let num = value;
            if num.is_empty() || num.len() > 24 {
                return;
            }
            unsafe {
                CALL_ACTIVE = true;
                CALL_DIR = 1;
                CALL_STAT = 3; // ringing
                CALL_NUM_LEN = num.len();
                CALL_NUM[..CALL_NUM_LEN].copy_from_slice(num);
            }
            tx_push_str("\r\n+CRING: ");
            tx_push(num);
            tx_push_str("\r\n");
        }
        b"CALL_ANSWER" => {
            unsafe {
                if CALL_ACTIVE {
                    CALL_STAT = 0; // 远端应答 → 通话中
                }
            }
        }
        b"CALL_END" => {
            unsafe {
                if CALL_ACTIVE {
                    CALL_ACTIVE = false;
                    tx_push_str("\r\nNO CARRIER\r\n");
                }
            }
        }
        b"SMS_IN" => {
            // 校验十六进制格式后直接存储字符串（+CMGR 原样返回，rild 侧解码）
            let mut vlen = value.len();
            while vlen > 0 && (value[vlen - 1] == b'\r' || value[vlen - 1] == b' ' || value[vlen - 1] == b'\t') {
                vlen -= 1;
            }
            let v = &value[..vlen];
            if v.len() < 4 || v.len() % 2 != 0 || v.len() > PDU_HEX_MAX {
                return;
            }
            if !v.iter().all(|&c| c.is_ascii_hexdigit()) {
                return;
            }
            unsafe {
                if SM_COUNT >= SM_SLOTS {
                    sm_delete(1); // 满则丢弃最旧
                }
                let slot = &mut SM[SM_COUNT];
                slot.len = v.len();
                slot.data[..v.len()].copy_from_slice(v);
                SM_COUNT += 1;
                tx_push_str("\r\n+CMTI: \"SM\",");
                push_dec(SM_COUNT as u32);
                tx_push_str("\r\n");
            }
        }
        b"SIGNAL" => {
            if let Some(v) = parse_index(value) {
                unsafe { SIGNAL = v.min(31) as u8 };
            }
        }
        b"OP" => {
            unsafe {
                OPERATOR_LEN = value.len().min(OPERATOR.len());
                OPERATOR[..OPERATOR_LEN].copy_from_slice(&value[..OPERATOR_LEN]);
            }
        }
        b"SIM" => {
            if value.eq_ignore_ascii_case(b"ABSENT") {
                unsafe { SIM_ABSENT = true };
            } else if value.eq_ignore_ascii_case(b"READY") {
                unsafe { SIM_ABSENT = false };
            }
        }
        b"REG" => {
            if let Some(v) = parse_index(value) {
                unsafe { REG = v.min(5) as u8 };
            }
        }
        b"SMSC" => {
            unsafe {
                SMSC_LEN = value.len().min(SMSC.len());
                SMSC[..SMSC_LEN].copy_from_slice(&value[..SMSC_LEN]);
            }
        }
        b"RESET" => {
            unsafe {
                CALL_ACTIVE = false;
                SM_COUNT = 0;
                SIM_ABSENT = false;
                SIGNAL = 20;
                REG = 1;
                OPERATOR_LEN = 4;
                OPERATOR[..4].copy_from_slice(b"CMCC");
                SMSC_LEN = 14;
                SMSC[..14].copy_from_slice(b"+8613800100500");
                CMGS_EXPECT = 0;
            }
        }
        _ => {}
    }
}

// ==================== Pi4 PL011 UART2 后端 ====================

#[cfg(feature = "pi4")]
mod uart2 {
    use super::*;

    /// 初始化 UART2：8N1、115200 baud（48MHz 时钟）、使能收发。
    pub unsafe fn init() {
        let base = PI4_UART2_BASE;
        write_volatile((base + REG_CR) as *mut u32, 0); // 关闭后配置
        write_volatile((base + REG_IBRD) as *mut u32, 26);
        write_volatile((base + REG_FBRD) as *mut u32, 3);
        write_volatile((base + REG_LCR_H) as *mut u32, 0x70); // 8N1 + FIFO
        write_volatile((base + REG_CR) as *mut u32, 0x301); // UARTEN|TXE|RXE
    }

    /// 非阻塞读一个字节。
    pub fn read_byte() -> Option<u8> {
        let base = PI4_UART2_BASE;
        unsafe {
            if read_volatile((base + REG_FR) as *const u32) & FR_RXFE != 0 {
                None
            } else {
                Some(read_volatile((base + REG_DR) as *const u32) as u8)
            }
        }
    }

    /// 阻塞写一个字节（等待 FIFO 非满；rild 周期轮询，突发流量极小）。
    pub fn write_byte(b: u8) {
        let base = PI4_UART2_BASE;
        unsafe {
            for _ in 0..100_000 {
                if read_volatile((base + REG_FR) as *const u32) & FR_TXFF == 0 {
                    write_volatile((base + REG_DR) as *mut u32, b as u32);
                    return;
                }
                core::hint::spin_loop();
            }
        }
    }
}

// ==================== 字符设备操作 ====================

/// 后端接收字节泵入命令解析器。
fn feed_rx_byte(b: u8) {
    unsafe {
        // AT+CMGS 数据模式：字节先入 PDU 缓冲，遇到 0x1A（CTRL-Z）结束
        if CMGS_EXPECT > 0 {
            if b == 0x1A {
                // 结束：确认接收（不校验内容，rild 自持发送记录）
                CMGS_EXPECT = 0;
                CMGS_LEN = 0;
                MR = MR.wrapping_add(1);
                tx_push_str("\r\n+CMGS: ");
                push_dec(MR as u32);
                tx_push_str("\r\n");
                ok();
            } else if CMGS_LEN < CMGS_EXPECT && CMGS_LEN < CMGS_BUF.len() {
                CMGS_BUF[CMGS_LEN] = b;
                CMGS_LEN += 1;
            }
            return;
        }
        if b == b'\n' {
            // 行结束：去掉尾部 \r 后处理
            let mut end = RX_LEN;
            if end > 0 && RX_LINE[end - 1] == b'\r' {
                end -= 1;
            }
            let line = &RX_LINE[..end];
            process_line(line);
            RX_LEN = 0;
        } else if b != b'\r' {
            if RX_LEN < RX_LINE_CAP {
                RX_LINE[RX_LEN] = b;
                RX_LEN += 1;
            }
        }
    }
}

/// /dev/modem read：非阻塞取走后端 → 主机的字节。
fn modem_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    #[cfg(feature = "pi4")]
    {
        if !unsafe { VIRTUAL } {
            // 真机：从 UART2 接收 FIFO 排空
            let mut n = 0usize;
            while n < buf.len() {
                match uart2::read_byte() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            return n as i32;
        }
    }
    tx_pop(buf) as i32
}

/// /dev/modem write：把 AT 命令字节交给后端。
fn modem_write(_minor: u32, buf: &[u8]) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    #[cfg(feature = "pi4")]
    {
        if !unsafe { VIRTUAL } {
            for &b in buf {
                uart2::write_byte(b);
            }
            return buf.len() as i32;
        }
    }
    for &b in buf {
        feed_rx_byte(b);
    }
    buf.len() as i32
}

/// /dev/modemctl write：虚拟调制解调器事件注入。
fn modemctl_write(_minor: u32, buf: &[u8]) -> i32 {
    if !unsafe { READY } || !unsafe { VIRTUAL } {
        return -1; // 真机无注入通道
    }
    let mut start = 0usize;
    for i in 0..=buf.len() {
        if i == buf.len() || buf[i] == b'\n' {
            if i > start {
                let mut line = &buf[start..i];
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                process_ctl(line);
            }
            start = i + 1;
        }
    }
    buf.len() as i32
}

/// /dev/modem + /dev/modemctl ioctl。
fn modem_ioctl(minor: u32, request: u32, out: &mut [u8]) -> i32 {
    if !unsafe { READY } {
        return -19; // ENODEV
    }
    match (minor, request) {
        (m, devfs::MODEM_IOCTL_PRESENT)
            if m == devfs::MODEM_MINOR || m == devfs::MODEMCTL_MINOR => {
            if out.len() >= 4 {
                out[0..4].copy_from_slice(&1u32.to_le_bytes());
                return 4;
            }
            0
        }
        (_, devfs::MODEM_IOCTL_STATUS) => {
            // 调试：输出虚拟调制解调器内部状态（仅 QEMU 有效）
            if !unsafe { VIRTUAL } {
                return -1;
            }
            if out.len() < 32 {
                return -1;
            }
            out[0..4].copy_from_slice(&(unsafe { CALL_ACTIVE as u32 }).to_le_bytes());
            out[4..8].copy_from_slice(&(unsafe { CALL_DIR as u32 }).to_le_bytes());
            out[8..12].copy_from_slice(&(unsafe { CALL_STAT as u32 }).to_le_bytes());
            out[12..16].copy_from_slice(&(unsafe { SIM_ABSENT as u32 }).to_le_bytes());
            out[16..20].copy_from_slice(&(unsafe { SM_COUNT as u32 }).to_le_bytes());
            32
        }
        _ => -25, // ENOTTY
    }
}

// ==================== 初始化 ====================

/// 初始化调制解调器子系统并注册字符设备。
///
/// QEMU：启用内置虚拟调制解调器；Pi4：初始化第二路 PL011（UART2），
/// 供外接 USB-TTL → Quectel EC25 等 AT 调制解调器使用。
pub fn init() {
    #[cfg(feature = "pi4")]
    {
        unsafe { uart2::init() };
        unsafe { VIRTUAL = false };
        crate::kinfo! { crate::uart_send_str("[modem] Pi4 PL011 UART2 后端就绪（/dev/modem）\n"); }
    }
    #[cfg(all(feature = "qemu", not(feature = "pi4")))]
    {
        unsafe { VIRTUAL = true };
        crate::kinfo! { crate::uart_send_str("[modem] QEMU 虚拟调制解调器就绪（/dev/modem、/dev/modemctl）\n"); }
    }
    unsafe { READY = true };

    let ops = devfs::ChrOps {
        read: modem_read,
        write: modem_write,
        ioctl: modem_ioctl,
    };
    devfs::register_chrdev(devfs::MODEM_MAJOR, "modem", ops);
    crate::vfs::mknod("/dev/modem", crate::vfs::FileType::Chr, devfs::MODEM_MAJOR, devfs::MODEM_MINOR);
    crate::vfs::mknod("/dev/modemctl", crate::vfs::FileType::Chr, devfs::MODEM_MAJOR, devfs::MODEMCTL_MINOR);
}
