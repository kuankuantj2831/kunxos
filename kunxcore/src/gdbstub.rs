//! GDB 远程串行协议（RSP）调试桩（aarch64）。
//!
//! 通过 UART 与宿主 GDB 通信（`target remote <serial>`）。支持：
//!   - `?` 暂停原因（SIGTRAP）；`g`/`G` 全部寄存器；`p`/`P` 单寄存器
//!   - `m`/`M` 内存读写（内核恒等映射区直接访问；用户区经当前页表）
//!   - `Z0`/`z0` **真实软件断点**（写入 BRK #0，命中后摘除/重插 + 单步跳过）
//!   - `s` **真实硬件单步**（MDSCR_EL1.SS，步进异常后停回 GDB）
//!   - `c` 继续（断点处继续 = 摘除断点 → 单步跳过 → 静默重插）
//!   - `D` detach、`k` 终止（挂起）、`qSupported`/`qAttached`/`H` 握手
//!   - 线程/硬件断点/watchpoint：预留（返回空响应 = 不支持）
//!
//! 入口：
//!   - [`gdb_debug_entry`]：EL0 BRK64（EC=0x3C）与软件步进（EC=0x32）异常，
//!     由 `main.rs` `lower_sync` 汇编调用，陷阱帧即寄存器现场。
//!   - [`sync_gdb_entry`]：EL1 BRK64（EC=0x3C）与软件步进（EC=0x33）异常，
//!     由 `main.rs` `sync_entry` 汇编调用（内核断点）。
//!   - [`gdb_break`]：shell `gdb` 命令 / panic 的手动入口（内核上下文）。
//!
//! 设计要点：
//!   - 寄存器集以 [`RegSet`] 表示（x0-x30, sp, pc, cpsr），与陷阱帧或
//!     PANIC_REGS 互拷；gdb 循环内统一经静态 `SNAP` 读写。
//!   - 断点摘除/重插通过 [`PENDING_REINSERT`] 与 [`STEP_MODE`] 状态机完成：
//!     `c`（断点处）= 静默步进（STEP_SILENT），`s` = 报告步进（STEP_REPORT）。

use core::arch::asm;

// ==================== 寄存器集 ====================

/// x0-x30 @0..30, sp @31, pc @32, cpsr @33。
static mut SNAP: [u64; 34] = [0; 34];

/// 当前是否处于 GDB 循环。
pub static mut ACTIVE: bool = false;

/// 寄存器集（与陷阱帧/PANIC_REGS 交换的载体）。
#[derive(Clone, Copy)]
pub struct RegSet {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub cpsr: u64,
}

impl RegSet {
    pub const fn empty() -> RegSet {
        RegSet { x: [0; 31], sp: 0, pc: 0, cpsr: 0 }
    }

    /// 从 SNAP 构建。
    fn from_snap() -> RegSet {
        unsafe {
            let mut x = [0u64; 31];
            for i in 0..31 {
                x[i] = SNAP[i];
            }
            RegSet { x, sp: SNAP[31], pc: SNAP[32], cpsr: SNAP[33] }
        }
    }

    /// 写回 SNAP。
    fn to_snap(&self) {
        unsafe {
            for i in 0..31 {
                SNAP[i] = self.x[i];
            }
            SNAP[31] = self.sp;
            SNAP[32] = self.pc;
            SNAP[33] = self.cpsr;
        }
    }

    /// 从 EL0 陷阱帧（x0..x30 @0..30, sp_el0 @31, elr @32, spsr @33）构建。
    fn from_trap_frame(frame: *const u64) -> RegSet {
        let mut x = [0u64; 31];
        unsafe {
            for i in 0..31 {
                x[i] = *frame.add(i);
            }
        }
        RegSet {
            x,
            sp: unsafe { *frame.add(31) },
            pc: unsafe { *frame.add(32) },
            cpsr: unsafe { *frame.add(33) },
        }
    }

    /// 写回 EL0 陷阱帧。
    fn write_to_trap_frame(&self, frame: *mut u64) {
        unsafe {
            for i in 0..31 {
                *frame.add(i) = self.x[i];
            }
            *frame.add(31) = self.sp;
            *frame.add(32) = self.pc;
            *frame.add(33) = self.cpsr;
        }
    }

    /// 从内核 PANIC_REGS（sync_entry 保存；x11 槽为基址标记）+ 当前 sp 构建。
    fn from_panic_regs(sp: u64) -> RegSet {
        let mut x = [0u64; 31];
        unsafe {
            for i in 0..31 {
                x[i] = crate::PANIC_REGS[i];
            }
        }
        RegSet { x, sp, pc: 0, cpsr: 0 }
    }

    /// 写回 PANIC_REGS（x0-x10, x12-x30；x11 槽保持基址标记）。
    fn write_to_panic_regs(&self) {
        unsafe {
            for i in 0..31 {
                if i != 11 {
                    crate::PANIC_REGS[i] = self.x[i];
                }
            }
        }
    }
}

/// 保存当前内核寄存器现场到 SNAP（shell `gdb` 入口用，调用方已屏蔽 IRQ）。
fn snapshot_kernel() {
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

/// 从 SNAP 恢复 x0-x30（shell `gdb` 继续时应用 GDB 的寄存器写）。
/// sp/pc/cpsr 不恢复（内核上下文无异常返回路径，恢复会破坏活动栈）。
fn restore_gprs() {
    unsafe {
        asm!(
            "ldp x0, x1, [{r}, #0]",
            "ldp x2, x3, [{r}, #16]",
            "ldp x4, x5, [{r}, #32]",
            "ldp x6, x7, [{r}, #48]",
            "ldp x8, x9, [{r}, #64]",
            "ldp x10, x11, [{r}, #80]",
            "ldp x12, x13, [{r}, #96]",
            "ldp x14, x15, [{r}, #112]",
            "ldp x16, x17, [{r}, #128]",
            "ldp x18, x19, [{r}, #144]",
            "ldp x20, x21, [{r}, #160]",
            "ldp x22, x23, [{r}, #176]",
            "ldp x24, x25, [{r}, #192]",
            "ldp x26, x27, [{r}, #208]",
            "ldp x28, x29, [{r}, #224]",
            "ldr x30, [{r}, #240]",
            r = in(reg) SNAP.as_ptr(),
            options(nostack),
        );
    }
}

// ==================== 断点表 ====================

/// 软件断点上限。
pub const MAX_BPS: usize = 16;

#[derive(Clone, Copy)]
struct Bp {
    addr: u64,
    orig: u32,
    active: bool,
}

const EMPTY_BP: Bp = Bp { addr: 0, orig: 0, active: false };
static mut BPS: [Bp; MAX_BPS] = [EMPTY_BP; MAX_BPS];
static mut BP_COUNT: usize = 0;

/// BRK #0 指令编码（11010100 00100000 00000000 00000000）。
const BRK_INS: u32 = 0xD420_0000;

/// 指令缓存维护：写回数据缓存行 + 无效化指令缓存行。
fn flush_insn(addr: u64) {
    unsafe {
        asm!(
            "dc cvau, {a}",
            "dsb ish",
            "ic ivau, {a}",
            "dsb ish",
            "isb",
            a = in(reg) addr,
            options(nostack),
        );
    }
}

/// 查找断点（地址匹配且 active），返回下标。
fn bp_find(addr: u64) -> Option<usize> {
    unsafe {
        for i in 0..BP_COUNT {
            if BPS[i].active && BPS[i].addr == addr {
                return Some(i);
            }
        }
    }
    None
}

/// 设置软件断点：保存原指令并写入 BRK #0。
fn bp_set(addr: u64) -> bool {
    if addr % 4 != 0 || addr == 0 {
        return false;
    }
    if let Some(_) = bp_find(addr) {
        return true; // 幂等
    }
    unsafe {
        if BP_COUNT >= MAX_BPS {
            return false;
        }
        // 读取原指令（4 字节）
        let mut orig = [0u8; 4];
        let n = mem_read(addr, 4, &mut orig);
        if n != 4 {
            return false;
        }
        // 写入 BRK
        let brk = BRK_INS.to_le_bytes();
        if mem_write(addr, &brk) != 4 {
            return false;
        }
        flush_insn(addr);
        BPS[BP_COUNT] = Bp { addr, orig: u32::from_le_bytes(orig), active: true };
        BP_COUNT += 1;
        true
    }
}

/// 摘除软件断点：恢复原指令。
fn bp_remove(addr: u64) -> bool {
    if let Some(idx) = bp_find(addr) {
        unsafe {
            let orig = BPS[idx].orig.to_le_bytes();
            if mem_write(addr, &orig) == 4 {
                flush_insn(addr);
            }
            BPS[idx].active = false;
            // 压缩表（保持前部连续）：把末项搬入空位
            if BP_COUNT > 0 {
                BPS[idx] = BPS[BP_COUNT - 1];
                BPS[BP_COUNT - 1] = EMPTY_BP;
                BP_COUNT -= 1;
            }
        }
        return true;
    }
    false
}

/// 重插断点（单步跳过原指令后）。
fn bp_reinsert(addr: u64) {
    let brk = BRK_INS.to_le_bytes();
    if mem_write(addr, &brk) == 4 {
        flush_insn(addr);
    }
}

// ==================== 单步状态机 ====================

/// 步进模式：0=无，1=报告（gdb `s`，步进异常后停回 GDB），
/// 2=静默（断点继续 `c`，步进异常后继续运行并重插断点）。
static mut STEP_MODE: u8 = 0;
/// 断点继续后待重插的断点地址。
static mut PENDING_REINSERT: bool = false;
static mut PENDING_ADDR: u64 = 0;

/// 置位/清除 MDSCR_EL1.SS（硬件单步）。
fn set_ss(on: bool) {
    unsafe {
        let mut v: u64;
        asm!("mrs {}, mdscr_el1", out(reg) v);
        if on {
            v |= 1;
        } else {
            v &= !1u64;
        }
        asm!("msr mdscr_el1, {}", in(reg) v);
    }
}

/// 读 ESR_EL1 / ELR_EL1 / SPSR_EL1。
fn read_esr() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, esr_el1", out(reg) v) };
    v
}
fn read_elr() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, elr_el1", out(reg) v) };
    v
}
fn read_spsr() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, spsr_el1", out(reg) v) };
    v
}

// ==================== UART / RSP 报文 ====================

/// 读单个字符（阻塞等待 UART RX，gdb 循环全程屏蔽 IRQ）。
fn read_byte_blocking() -> u8 {
    loop {
        crate::uart_drain_hw();
        if let Some(c) = crate::uart_try_read_char_masked() {
            return c;
        }
        core::hint::spin_loop();
    }
}

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

/// 读取一个 RSP 报文，返回数据长度；校验失败返回 usize::MAX。
fn read_packet(buf: &mut [u8]) -> usize {
    loop {
        let c = read_byte_blocking();
        if c == b'$' {
            break;
        }
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

/// 单寄存器读（`p <reg>`）。
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

/// 内存读（`m addr,len`）：内核恒等映射区直接访问；用户区经当前页表。
fn mem_read(addr: u64, len: usize, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    while n < len {
        let va = (addr + n as u64) as usize;
        let pa = if va >= 0x4000_0000 {
            va
        } else {
            crate::vm::user_va_to_pa(crate::process::current_root(), va)
        };
        if pa == 0 {
            break;
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

#[inline]
fn hex_nib(b: u8) -> u8 {
    if b < 10 { b'0' + b } else { b'a' + b - 10 }
}

#[inline]
fn push_hex_byte(out: &mut [u8], pos: usize, b: u8) {
    out[pos] = hex_nib(b >> 4);
    out[pos + 1] = hex_nib(b & 0xF);
}

/// 完整 aarch64 寄存器包 hex 串（`g` 命令）：GPR 272B + 向量 512B + fpsr/fpcr。
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
    for _ in 0..(32 * 16 + 4) {
        if n + 2 <= out.len() {
            push_hex_byte(out, n, 0);
            n += 2;
        }
    }
    n
}

/// GDB 循环结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopResult {
    Continue,
    Step,
    Detach,
    Kill,
}

/// RSP 主循环：以 `regs` 为现场，处理 GDB 命令直到 c/s/D/k。
fn gdb_loop(regs: &mut RegSet) -> LoopResult {
    unsafe { ACTIVE = true };
    regs.to_snap();
    let mut result = LoopResult::Continue;
    let mut buf = [0u8; 1024];
    let mut reply = [0u8; 4096];
    'outer: loop {
        let n = read_packet(&mut buf);
        if n == usize::MAX {
            continue;
        }
        let pkt = &buf[..n];
        if pkt.is_empty() {
            // 空数据报文（"$#00"，校验和合法）：按 RSP 惯例回空包继续，
            // 此前无条件 pkt[0] 取下标 → index out of bounds panic（外部
            // 输入可直接触发内核 abort）。
            send_packet(b"");
            continue;
        }
        let rlen;
        match pkt[0] {
            b'?' => {
                reply[..3].copy_from_slice(b"S05");
                rlen = 3;
            }
            b'g' => rlen = regs_hex(&mut reply),
            b'G' => {
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
                reply[..2].copy_from_slice(b"OK");
                rlen = 2;
            }
            b'p' => {
                let (reg, _) = parse_hex(&pkt[1..]);
                let v = reg_read(reg as usize);
                let mut off = 0usize;
                for i in 0..8 {
                    push_hex_byte(&mut reply[..], off, ((v >> (i * 8)) & 0xFF) as u8);
                    off += 2;
                }
                rlen = off;
            }
            b'P' => {
                let (reg, consumed) = parse_hex(&pkt[1..]);
                if consumed < pkt.len() && pkt[1 + consumed] == b'=' {
                    let (val, _) = parse_hex(&pkt[2 + consumed..]);
                    reg_write(reg as usize, val);
                }
                reply[..2].copy_from_slice(b"OK");
                rlen = 2;
            }
            b'm' => {
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
                let (addr, consumed) = parse_hex(&pkt[1..]);
                let mut idx = 1 + consumed;
                while idx < pkt.len() && pkt[idx] != b',' {
                    idx += 1;
                }
                if idx >= pkt.len() {
                    // 畸形报文（缺逗号）：此前无条件 idx+=1 后再切片会越界 panic
                    rlen = 0;
                } else {
                idx += 1;
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
                mem_write(addr, &data[..dn]);
                reply[..2].copy_from_slice(b"OK");
                rlen = 2;
                }
            }
            b'c' => {
                // c [addr]：result 保持初始 Continue
                if pkt.len() > 1 {
                    let (addr, _) = parse_hex(&pkt[1..]);
                    reg_write(32, addr); // pc
                }
                break 'outer;
            }
            b's' => {
                if pkt.len() > 1 {
                    let (addr, _) = parse_hex(&pkt[1..]);
                    reg_write(32, addr);
                }
                result = LoopResult::Step;
                break 'outer;
            }
            b'D' => {
                reply[..2].copy_from_slice(b"OK");
                rlen = 2;
                send_packet(&reply[..rlen]);
                result = LoopResult::Detach;
                break 'outer;
            }
            b'k' => {
                result = LoopResult::Kill;
                break 'outer;
            }
            b'Z' | b'z' => {
                let set = pkt[0] == b'Z';
                // Z<kind>,<addr>,<len>
                if pkt.len() >= 2 {
                    let kind = hexval(pkt[1]);
                    let mut i = 2usize;
                    if i < pkt.len() && pkt[i] == b',' {
                        i += 1;
                    }
                    let (addr, _c) = parse_hex(&pkt[i..]);
                    let ok = match kind {
                        0 => {
                            if set {
                                bp_set(addr)
                            } else {
                                bp_remove(addr)
                            }
                        }
                        // Z1（硬件断点）/ Z2（watchpoint）预留：不支持
                        _ => false,
                    };
                    if ok {
                        reply[..2].copy_from_slice(b"OK");
                        rlen = 2;
                    } else {
                        rlen = 0;
                    }
                } else {
                    rlen = 0;
                }
            }
            b'q' => {
                let tail = &pkt[1..];
                if tail.starts_with(b"Supported") {
                    let resp = b"PacketSize=4096;";
                    reply[..resp.len()].copy_from_slice(resp);
                    rlen = resp.len();
                } else if tail.starts_with(b"Attached") {
                    reply[0] = b'1';
                    rlen = 1;
                } else {
                    rlen = 0;
                }
            }
            b'H' => {
                reply[..2].copy_from_slice(b"OK");
                rlen = 2;
            }
            _ => {
                rlen = 0;
            }
        }
        if rlen > 0 {
            send_packet(&reply[..rlen]);
        } else {
            send_packet(b"");
        }
    }
    let final_regs = RegSet::from_snap();
    *regs = final_regs;
    unsafe { ACTIVE = false };
    result
}

// ==================== EL0 调试异常入口（BRK64 / 软件步进） ====================

/// `lower_sync` 汇编在 ESR.EC == 0x3C（BRK64）或 0x32（软件步进 from EL0）
/// 时调用。返回 0 = 恢复现场并 eret（继续运行）；非 0 = 走 el0_fault
/// （未注册断点的 BRK / 非调试异常，按 SIGTRAP/SIGSEGV 终止进程）。
#[no_mangle]
pub extern "C" fn gdb_debug_entry(frame: *mut u64) -> u32 {
    let esr = read_esr();
    let ec = (esr >> 26) & 0x3F;
    let mut regs = RegSet::from_trap_frame(frame);

    match ec {
        0x3C => {
            // BRK64：必须是已注册断点，否则交给 el0_fault（SIGTRAP 终止）
            if bp_find(regs.pc).is_none() {
                return 1;
            }
        }
        0x32 => {
            // 软件步进 from EL0
            unsafe {
                if PENDING_REINSERT {
                    bp_reinsert(PENDING_ADDR);
                    PENDING_REINSERT = false;
                }
                if STEP_MODE == 2 {
                    // 断点继续的静默步进：继续运行，不停回 GDB
                    STEP_MODE = 0;
                    set_ss(false);
                    // 恢复用户 PSTATE（清除步进期间临时置位的 I/SS 位）
                    regs.cpsr &= !((1 << 7) | (1 << 21));
                    regs.write_to_trap_frame(frame);
                    return 0;
                }
                STEP_MODE = 0;
            }
            set_ss(false);
            regs.cpsr &= !((1 << 7) | (1 << 21));
        }
        _ => {
            return 1;
        }
    }
    regs.write_to_trap_frame(frame);

    // 进入 GDB 循环：先主动发送停止包（SIGTRAP）。RSP 协议要求调试桩在
    // 目标因断点/单步**非请求性**停下时立即通知 gdb，否则 gdb 在 `s` 或
    // `c` 之后会无限等待停止包（gdb 随后会 `?` 查询，循环内再次回复 S05）。
    send_packet(b"S05");
    crate::uart_send_str("[gdb] 停在 0x");
    crate::print_hex(regs.pc);
    crate::uart_send_str("\n");
    let result = gdb_loop(&mut regs);

    match result {
        LoopResult::Continue => {
            if ec == 0x3C {
                // 断点处继续：摘除断点、单步跳过原指令、静默重插
                if bp_find(regs.pc).is_some() {
                    bp_remove(regs.pc);
                    unsafe {
                        PENDING_REINSERT = true;
                        PENDING_ADDR = regs.pc;
                        STEP_MODE = 2;
                    }
                    set_ss(true);
                    regs.cpsr |= 1 << 7; // 步进期间屏蔽 IRQ
                }
            }
        }
        LoopResult::Step => {
            // 单步：置位 SS（断点处需先摘除，步进后重插）
            if ec == 0x3C && bp_find(regs.pc).is_some() {
                bp_remove(regs.pc);
                unsafe {
                    PENDING_REINSERT = true;
                    PENDING_ADDR = regs.pc;
                }
            }
            unsafe { STEP_MODE = 1 };
            set_ss(true);
            regs.cpsr |= 1 << 7;
        }
        LoopResult::Detach => {}
        LoopResult::Kill => {
            loop {
                core::hint::spin_loop();
            }
        }
    }
    regs.write_to_trap_frame(frame);
    0
}

// ==================== EL1 调试异常入口（内核断点 / 步进） ====================

/// `sync_entry` 汇编在 ESR.EC == 0x3C（BRK64）或 0x33（软件步进 from EL1）
/// 时调用。返回 0 = 恢复寄存器并 eret（继续内核执行）；非 0 = 走 panic。
#[no_mangle]
pub extern "C" fn sync_gdb_entry() -> u32 {
    let esr = read_esr();
    let ec = (esr >> 26) & 0x3F;
    let elr = read_elr();
    let spsr = read_spsr();
    let sp: u64;
    unsafe { asm!("mov {}, sp", out(reg) sp) };

    let mut regs = RegSet::from_panic_regs(sp);
    regs.pc = elr;
    regs.cpsr = spsr;

    match ec {
        0x3C => {
            if bp_find(regs.pc).is_none() {
                return 1; // 未注册断点的内核 BRK → panic
            }
        }
        0x33 => {
            unsafe {
                if PENDING_REINSERT {
                    bp_reinsert(PENDING_ADDR);
                    PENDING_REINSERT = false;
                }
                if STEP_MODE == 2 {
                    STEP_MODE = 0;
                    set_ss(false);
                    regs.cpsr &= !((1 << 7) | (1 << 21));
                    regs.write_to_panic_regs();
                    asm!("msr elr_el1, {}", in(reg) regs.pc);
                    asm!("msr spsr_el1, {}", in(reg) regs.cpsr);
                    return 0;
                }
                STEP_MODE = 0;
            }
            set_ss(false);
            regs.cpsr &= !((1 << 7) | (1 << 21));
        }
        _ => {
            return 1;
        }
    }

    send_packet(b"S05");
    crate::uart_send_str("[gdb] 内核停在 0x");
    crate::print_hex(regs.pc);
    crate::uart_send_str("\n");
    let result = gdb_loop(&mut regs);

    match result {
        LoopResult::Continue => {
            if ec == 0x3C {
                if bp_find(regs.pc).is_some() {
                    bp_remove(regs.pc);
                    unsafe {
                        PENDING_REINSERT = true;
                        PENDING_ADDR = regs.pc;
                        STEP_MODE = 2;
                    }
                    set_ss(true);
                    regs.cpsr |= 1 << 7;
                }
            }
        }
        LoopResult::Step => {
            if ec == 0x3C && bp_find(regs.pc).is_some() {
                bp_remove(regs.pc);
                unsafe {
                    PENDING_REINSERT = true;
                    PENDING_ADDR = regs.pc;
                }
            }
            unsafe { STEP_MODE = 1 };
            set_ss(true);
            regs.cpsr |= 1 << 7;
        }
        LoopResult::Detach => {}
        LoopResult::Kill => {
            loop {
                core::hint::spin_loop();
            }
        }
    }

    regs.write_to_panic_regs();
    unsafe {
        asm!("msr elr_el1, {}", in(reg) regs.pc);
        asm!("msr spsr_el1, {}", in(reg) regs.cpsr);
    }
    0
}

// ==================== 手动入口（shell `gdb` / panic） ====================

/// GDB 断点入口（内核上下文）：保存现场并进入 GDB 循环；`c` 后恢复
/// x0-x30 并返回调用方。sp/pc/cpsr 的写不生效（内核上下文无异常返回路径）。
pub fn gdb_break() {
    unsafe { asm!("msr daifset, #2") };
    snapshot_kernel();
    crate::uart_send_str("[gdb] 断点，等待 GDB...\n");
    let mut regs = RegSet::from_snap();
    let result = gdb_loop(&mut regs);
    if result == LoopResult::Kill {
        loop {
            core::hint::spin_loop();
        }
    }
    crate::uart_send_str("[gdb] 继续运行\n");
    unsafe { asm!("msr daifclr, #2") };
    restore_gprs();
}

/// 当前断点数（诊断）。
pub fn bp_count() -> usize {
    unsafe { BP_COUNT }
}
