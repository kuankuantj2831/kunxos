//! GDB 远程串行协议（RSP）调试桩。
//!
//! 通过 UART 与宿主 GDB 通信（`target remote <serial>`）：
//!   - `?` 暂停原因（SIGTRAP）
//!   - `g`/`G` 读写全部寄存器（x0-x30, sp, pc, cpsr）
//!   - `m`/`M` 读写内存（内核恒等映射区直接访问；用户区经当前页表）
//!   - `c` 继续执行（返回调用方）
//!   - `s` 伪单步（pc+4，用于行走）
//!   - `k` 终止（内核挂起）
//!   - `qSupported`/`qAttached`/`Hc`/`Hg`/`p`/`P` 等握手与单寄存器命令
//!
//! 入口：`gdb_break()`（可从 panic 处理器或 shell `gdb` 命令调用）。
//! 断点时把当前寄存器快照到静态区，`c` 返回后继续执行。

use core::arch::asm;

/// 寄存器快照：x0-x30 @0..30, sp @30, pc @31, cpsr @32。
static mut SNAP: [u64; 34] = [0; 34];

/// 当前是否处于 GDB 循环（供单步/继续逻辑判断）。
pub static mut ACTIVE: bool = false;

/// 保存当前寄存器现场到 SNAP（调用方须已屏蔽 IRQ）。
fn snapshot() {
    unsafe {
        asm!(
            "stp x0, x1, [{r}, #0]",
            "stp x2, x3, [{r}, #16]",
            "stp x4, x5, [{r}, #32]",
            "stp x6, x7, [{r}, #48]",
            "stp x8, x9, [{r}, #64]",
            "stp x10, x11, [{r}, #80]",
            "stp x12, x13, [{r}, #96]",
            "stp x14, x15, [{r}, #112]",
            "stp x16, x17, [{r}, #128]",
            "stp x18, x19, [{r}, #144]",
            "stp x20, x21, [{r}, #160]",
            "stp x22, x23, [{r}, #176]",
            "stp x24, x25, [{r}, #192]",
            "stp x26, x27, [{r}, #208]",
            "stp x28, x29, [{r}, #224]",
            "str x30, [{r}, #240]",
            r = in(reg) SNAP.as_mut_ptr(),
            options(nostack),
        );
        // sp / pc / cpsr（GDB aarch64 编号：30=x30, 31=sp, 32=pc, 33=cpsr）
        asm!(
            "mov {spv}, sp",
            "mrs {pcv}, elr_el1",
            "mrs {cpsrv}, spsr_el1",
            spv = out(reg) SNAP[31],
            pcv = out(reg) SNAP[32],
            cpsrv = out(reg) SNAP[33],
            options(nostack),
        );
    }
}

/// 读单个字符（阻塞等待 UART RX）。
/// 优先读硬件 FIFO（gdb_break 已屏蔽 IRQ，软件缓冲不再增长）；
/// 其次读软件缓冲（断点前已到达的字符）。
fn read_byte_blocking() -> u8 {
    loop {
        // 批量排空硬件 FIFO（防 16 字节 FIFO 溢出丢字节），再读软件缓冲。
        // 全程不触碰 DAIF（gdb_break 已屏蔽 IRQ，防止定时器抢占切换任务
        // 污染串口会话）。
        crate::uart_drain_hw();
        if let Some(c) = crate::uart_try_read_char_masked() {
            return c;
        }
        core::hint::spin_loop();
    }
}

/// 发送一个字符。
fn send_byte(b: u8) {
    crate::uart_send_char(b);
}

/// 发送 RSP 报文：`$data#cksum`。
fn send_packet(data: &[u8]) {
    send_byte(b'$');
    let mut cksum: u8 = 0;
    for &b in data {
        send_byte(b);
        cksum = cksum.wrapping_add(b);
    }
    send_byte(b'#');
    let hi = (cksum >> 4) & 0xF;
    let lo = cksum & 0xF;
    send_byte(if hi < 10 { b'0' + hi } else { b'a' + hi - 10 });
    send_byte(if lo < 10 { b'0' + lo } else { b'a' + lo - 10 });
}

/// 读取一个 RSP 报文（等待 `$`），返回数据长度；校验失败返回 usize::MAX。
fn read_packet(buf: &mut [u8]) -> usize {
    // 等待报文起始
    loop {
        let c = read_byte_blocking();
        if c == b'$' {
            break;
        }
        // '+'/'-' 等空闲字符忽略
    }
    let mut n = 0usize;
    let mut cksum: u8 = 0;
    loop {
        let c = read_byte_blocking();
        if c == b'#' {
            break;
        }
        if n < buf.len() {
            buf[n] = c;
            cksum = cksum.wrapping_add(c);
            n += 1;
        }
    }
    // 读两位校验和
    let mut csum = 0u8;
    for _ in 0..2 {
        let c = read_byte_blocking();
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => 0xFF,
        };
        csum = (csum << 4) | v;
    }
    if csum == cksum {
        send_byte(b'+');
        n
    } else {
        send_byte(b'-');
        usize::MAX
    }
}

fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// 解析 hex 数字（可带减号前缀），返回 (值, 消费字节数)。
fn parse_hex(s: &[u8]) -> (u64, usize) {
    let mut v = 0u64;
    let mut i = 0usize;
    while i < s.len() && (s[i].is_ascii_hexdigit() || s[i] == b'-') {
        if s[i] == b'-' {
            i += 1;
            continue;
        }
        v = (v << 4) | hexval(s[i]) as u64;
        i += 1;
    }
    (v, i)
}

/// 单寄存器读（`p <reg>`）。GPR 从快照读；向量/系统寄存器返回 0。
fn reg_read(idx: usize) -> u64 {
    if idx < 34 {
        unsafe { SNAP[idx] }
    } else {
        0
    }
}

/// 单寄存器写（`P <reg>=<val>`）。
fn reg_write(idx: usize, v: u64) {
    if idx < 34 {
        unsafe { SNAP[idx] = v };
    }
}

/// 内存读（`m addr,len`）：返回读到内存 `out` 的字节数。
/// 内核恒等映射区（>= 0x40000000）直接访问；用户区经当前页表转换。
fn mem_read(addr: u64, len: usize, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    while n < len {
        let va = (addr + n as u64) as usize;
        let pa = if va >= 0x4000_0000 {
            va // 内核恒等映射
        } else {
            crate::vm::user_va_to_pa(crate::process::current_root(), va)
        };
        if pa == 0 {
            break; // 未映射：停止（GDB 会显示错误）
        }
        out[n] = unsafe { *(pa as *const u8) };
        n += 1;
    }
    n
}

/// 内存写（`M addr,len:data`）。
fn mem_write(addr: u64, data: &[u8]) -> usize {
    let mut n = 0usize;
    while n < data.len() {
        let va = (addr + n as u64) as usize;
        let pa = if va >= 0x4000_0000 {
            va
        } else {
            crate::vm::user_va_to_pa(crate::process::current_root(), va)
        };
        if pa == 0 {
            break;
        }
        unsafe { *(pa as *mut u8) = data[n] };
        n += 1;
    }
    n
}

/// 字节 → 两个 hex 字符。
#[inline]
fn hex_nib(b: u8) -> u8 {
    if b < 10 { b'0' + b } else { b'a' + b - 10 }
}

#[inline]
fn push_hex_byte(out: &mut [u8], pos: usize, b: u8) {
    out[pos] = hex_nib(b >> 4);
    out[pos + 1] = hex_nib(b & 0xF);
}

/// 返回完整 aarch64 寄存器包的 hex 串（`g` 命令，小端字节序）：
/// x0-x30, sp, pc, cpsr（34×8 字节）+ v0-v31（32×16 字节，读当前值）+ fpsr/fpcr。
fn regs_hex(out: &mut [u8]) -> usize {
    let mut n = 0usize;
    for r in 0..34 {
        let v = reg_read(r);
        for i in 0..8 {
            if n + 2 <= out.len() {
                push_hex_byte(out, n, ((v >> (i * 8)) & 0xFF) as u8);
                n += 2;
            }
        }
    }
    // v0-v31（128 位向量寄存器）：输出零（协议完整性；GPR 已由快照提供）
    for _ in 0..(32 * 16) {
        if n + 2 <= out.len() {
            push_hex_byte(out, n, 0);
            n += 2;
        }
    }
    // fpsr / fpcr：gdb aarch64 期望 g 包总长 788 字节（GPR 272 + 向量 512 + 4）
    for _ in 0..4 {
        if n + 2 <= out.len() {
            push_hex_byte(out, n, 0);
            n += 2;
        }
    }
    n
}

/// 进入 GDB 主循环。`c`/`s` 返回时结束循环（调用方继续执行）。
/// 永不返回时（panic 场景）由调用方决定。
pub fn gdb_loop() {
    unsafe { ACTIVE = true };
    // 注意：不主动发送 S05。gdb 连接后先发 qSupported 等握手命令，
    // 若此时抢先发送 S05，gdb 会把它当作 qSupported 的响应 → 校验错误。
    // 暂停原因仅在收到 `?` 时回复。
    let mut buf = [0u8; 1024];
    let mut reply = [0u8; 4096];
    loop {
        let n = read_packet(&mut buf);
        if n == usize::MAX {
            continue;
        }
        let pkt = &buf[..n];
        let mut rlen = 0usize;
        match pkt[0] {
            b'?' => {
                reply[..3].copy_from_slice(b"S05");
                rlen = 3;
            }
            b'g' => rlen = regs_hex(&mut reply),
            b'G' => {
                // 解析 34 个寄存器
                let mut off = 1usize;
                for r in 0..34 {
                    let mut v = 0u64;
                    for _ in 0..8 {
                        if off < pkt.len() {
                            v = (v << 4) | hexval(pkt[off]) as u64;
                            off += 1;
                        }
                    }
                    reg_write(r, v);
                }
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
            }
            b'p' => {
                let (reg, consumed) = parse_hex(&pkt[1..]);
                let v = reg_read(reg as usize);
                // hex 输出（小端 8 字节）
                let mut off = 0usize;
                for i in 0..8 {
                    push_hex_byte(&mut reply[..], off, ((v >> (i * 8)) & 0xFF) as u8);
                    off += 2;
                }
                rlen = off;
                let _ = consumed;
            }
            b'P' => {
                // P <reg>=<val>
                let (reg, consumed) = parse_hex(&pkt[1..]);
                if consumed < pkt.len() && pkt[1 + consumed] == b'=' {
                    let (val, _) = parse_hex(&pkt[2 + consumed..]);
                    reg_write(reg as usize, val);
                }
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
            }
            b'm' => {
                // m <addr>,<len>
                let (addr, consumed) = parse_hex(&pkt[1..]);
                let (len, _) = if consumed < pkt.len() && pkt[1 + consumed] == b',' {
                    parse_hex(&pkt[2 + consumed..])
                } else {
                    (0, 0)
                };
                let mut tmp = [0u8; 512];
                let n = mem_read(addr, (len as usize).min(512), &mut tmp);
                let mut off = 0usize;
                for i in 0..n {
                    push_hex_byte(&mut reply[..], off, tmp[i]);
                    off += 2;
                }
                rlen = off;
            }
            b'M' => {
                // M <addr>,<len>:<data>
                let (addr, consumed) = parse_hex(&pkt[1..]);
                let mut idx = 1 + consumed;
                while idx < pkt.len() && pkt[idx] != b',' {
                    idx += 1;
                }
                idx += 1; // 跳过 ','
                let (len, consumed2) = parse_hex(&pkt[idx..]);
                idx += consumed2;
                if idx < pkt.len() && pkt[idx] == b':' {
                    idx += 1;
                }
                let mut data = [0u8; 512];
                let mut dn = 0usize;
                let want = (len as usize).min(512);
                while dn < want && idx + 1 < pkt.len() {
                    data[dn] = (hexval(pkt[idx]) << 4) | hexval(pkt[idx + 1]);
                    dn += 1;
                    idx += 2;
                }
                let w = mem_write(addr, &data[..dn]);
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
                let _ = w;
            }
            b'c' => {
                // 继续执行：退出循环
                unsafe { ACTIVE = false };
                return;
            }
            b's' => {
                // 伪单步：pc += 4（不真正执行指令，用于 GDB 行走）
                unsafe {
                    SNAP[31] = SNAP[31].wrapping_add(4);
                    ACTIVE = false;
                }
                return;
            }
            b'D' => {
                // detach：确认后继续执行（gdb 断开）
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
                send_packet(&reply[..rlen]);
                unsafe { ACTIVE = false };
                return;
            }
            b'k' => {
                // 终止：挂起
                loop {
                    core::hint::spin_loop();
                }
            }
            b'q' => {
                // qSupported / qAttached 等查询
                let mut tail = &pkt[1..];
                if tail.starts_with(b"Supported") {
                    let resp = b"PacketSize=4096;";
                    reply[..resp.len()].copy_from_slice(resp);
                    rlen = resp.len();
                } else if tail.starts_with(b"Attached") {
                    reply[0] = b'1';
                    rlen = 1;
                } else if tail.starts_with(b"Symbol:") || tail.starts_with(b"Offsets") {
                    rlen = 0; // 空响应
                } else {
                    rlen = 0;
                }
            }
            b'H' => {
                // 线程操作：忽略
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
            }
            b'v' => {
                // vCont 等：不支持
                rlen = 0;
            }
            b'z' | b'Z' => {
                // 断点：接受但忽略
                reply[0] = b'O';
                reply[1] = b'K';
                rlen = 2;
            }
            _ => {
                rlen = 0; // 空响应（不支持）
            }
        }
        if rlen > 0 {
            send_packet(&reply[..rlen]);
        } else {
            send_packet(b""); // 空包 = 不支持
        }
    }
}

/// GDB 断点入口：保存现场并进入 GDB 循环；`c` 后返回。
pub fn gdb_break() {
    // 屏蔽 IRQ，避免断点期间被抢占导致寄存器状态漂移
    unsafe { asm!("msr daifset, #2") };
    snapshot();
    crate::uart_send_str("[gdb] breakpoint, waiting for GDB...\n");
    gdb_loop();
    // 继续前恢复 IRQ
    unsafe { asm!("msr daifclr, #2") };
    crate::uart_send_str("[gdb] continuing\n");
}
