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

    /// 从 U 模式陷阱帧构建（RegSet.x[i] = 用户寄存器 x[i+1]，x0 恒零跳过；
    /// sp_el0 @槽32、elr @槽33、sstatus @槽34）。
    fn from_trap_frame(frame: *const u64) -> RegSet {
        let mut x = [0u64; 31];
        unsafe {
            for i in 0..31 {
                x[i] = *frame.add(i + 1);
            }
        }
        RegSet {
            x,
            sp: unsafe { *frame.add(32) },
            pc: unsafe { *frame.add(33) },
            cpsr: unsafe { *frame.add(34) },
        }
    }

    /// 写回 U 模式陷阱帧。
    fn write_to_trap_frame(&self, frame: *mut u64) {
        unsafe {
            *frame.add(0) = 0; // x0 恒零
            for i in 0..31 {
                *frame.add(i + 1) = self.x[i];
            }
            *frame.add(32) = self.sp;
            *frame.add(33) = self.pc;
            *frame.add(34) = self.cpsr;
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
///
/// SNAP 布局（34 槽）：x1..x31 @0..0xF0（x0 恒零跳过，槽 i = xi），
/// sp@0xF8、sepc@0x100、sstatus@0x108。
fn snapshot_kernel() {
    unsafe {
        let r = SNAP.as_mut_ptr();
        asm!(
            "sd x1, 0({r})",
            "sd x2, 8({r})",
            "sd x3, 16({r})",
            "sd x4, 24({r})",
            "sd x5, 32({r})",
            "sd x6, 40({r})",
            "sd x7, 48({r})",
            "sd x8, 56({r})",
            "sd x9, 64({r})",
            "sd x10, 72({r})",
            "sd x11, 80({r})",
            "sd x12, 88({r})",
            "sd x13, 96({r})",
            "sd x14, 104({r})",
            "sd x15, 112({r})",
            "sd x16, 120({r})",
            "sd x17, 128({r})",
            "sd x18, 136({r})",
            "sd x19, 144({r})",
            "sd x20, 152({r})",
            "sd x21, 160({r})",
            "sd x22, 168({r})",
            "sd x23, 176({r})",
            "sd x24, 184({r})",
            "sd x25, 192({r})",
            "sd x26, 200({r})",
            "sd x27, 208({r})",
            "sd x28, 216({r})",
            "sd x29, 224({r})",
            "sd x30, 232({r})",
            "sd x31, 240({r})",
            "mv {spv}, sp",
            "csrr {pcv}, sepc",
            "csrr {cpsrv}, sstatus",
            r = in(reg) r,
            spv = out(reg) SNAP[31],
            pcv = out(reg) SNAP[32],
            cpsrv = out(reg) SNAP[33],
            options(nostack),
        );
    }
}

/// 从 SNAP 恢复 x1..x31（shell `gdb` 继续时应用 GDB 的寄存器写）。
/// sp/pc/sstatus 不恢复（内核上下文无异常返回路径，恢复会破坏活动栈）。
/// x5（t0）用作基址暂存，最后从 SNAP 重载。
fn restore_gprs() {
    unsafe {
        let r = SNAP.as_ptr();
        asm!(
            "ld x1, 0({r})",
            "ld x2, 8({r})",
            "ld x3, 16({r})",
            "ld x4, 24({r})",
            "ld x6, 40({r})",
            "ld x7, 48({r})",
            "ld x8, 56({r})",
            "ld x9, 64({r})",
            "ld x10, 72({r})",
            "ld x11, 80({r})",
            "ld x12, 88({r})",
            "ld x13, 96({r})",
            "ld x14, 104({r})",
            "ld x15, 112({r})",
            "ld x16, 120({r})",
            "ld x17, 128({r})",
            "ld x18, 136({r})",
            "ld x19, 144({r})",
            "ld x20, 152({r})",
            "ld x21, 160({r})",
            "ld x22, 168({r})",
            "ld x23, 176({r})",
            "ld x24, 184({r})",
            "ld x25, 192({r})",
            "ld x26, 200({r})",
            "ld x27, 208({r})",
            "ld x28, 216({r})",
            "ld x29, 224({r})",
            "ld x30, 232({r})",
            "ld x31, 240({r})",
            "ld x5, 32({r})",
            r = in(reg) r,
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
    /// 断点写入宽度（字节）：2=压缩指令半字 ebreak，4=32 位指令。
    /// 原实现恒写 4 字节（0x9002+0x0000），目标为 16 位压缩指令时会
    /// 把下一指令的半字瞬态清零（R5-L1）。
    len: u8,
    active: bool,
}

const EMPTY_BP: Bp = Bp { addr: 0, orig: 0, len: 4, active: false };
static mut BPS: [Bp; MAX_BPS] = [EMPTY_BP; MAX_BPS];
static mut BP_COUNT: usize = 0;

/// EBREAK 指令编码（16 位压缩形态 ebreak = 0x9002，补全 32 位含下一半字）。
/// 断点写 16 位 0x9002（C 扩展内核必支持），恢复时还原原 16 位。
const BRK_INS: u32 = 0x9002;

/// 指令缓存维护：RISC-V 用 fence.i 同步取指流。
fn flush_insn(_addr: u64) {
    unsafe {
        asm!("fence.i", options(nostack));
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
    // RVC 允许 2 字节对齐（原 4 字节对齐要求拒绝半字位置的合法断点）
    if addr % 2 != 0 || addr == 0 {
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
        // 指令宽度判定（RVC）：低 2 位 != 0b11 即 16 位压缩指令。
        // 压缩指令只写 2 字节 ebreak（0x9002），不触碰下一半字。
        let compressed = orig[0] & 0x3 != 0x3;
        let len: u8 = if compressed { 2 } else { 4 };
        let brk = BRK_INS.to_le_bytes();
        if mem_write(addr, &brk[..len as usize]) != len as usize {
            return false;
        }
        flush_insn(addr);
        BPS[BP_COUNT] = Bp {
            addr,
            orig: u32::from_le_bytes(orig),
            len,
            active: true,
        };
        BP_COUNT += 1;
        true
    }
}

/// 摘除软件断点：恢复原指令。
fn bp_remove(addr: u64) -> bool {
    if let Some(idx) = bp_find(addr) {
        unsafe {
            let len = BPS[idx].len as usize;
            let orig = BPS[idx].orig.to_le_bytes();
            if mem_write(addr, &orig[..len]) == len {
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
    let len = bp_find(addr).map(|i| unsafe { BPS[i].len as usize }).unwrap_or(4);
    let brk = BRK_INS.to_le_bytes();
    if mem_write(addr, &brk[..len]) == len {
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

// ==================== RISC-V 临时推进断点（软件单步替代） ====================
//
// S 模式无硬件单步（set_ss 空实现）：断点处继续/单步改用「摘除断点 →
// 在下一条指令处植入临时 ebreak → 命中临时断点后重插原断点」的推进
// 状态机（R5-L1：原 AArch64 步进残留使断点一次性且 PENDING 永久悬空）。
// 已知限制：仅跟踪顺序流（分支目标不埋临时断点），与常见软件单步同款。

/// 临时断点地址（0 = 无）。
static mut TEMP_BP_ADDR: u64 = 0;
/// 临时断点命中后需重插的原断点地址。
static mut TEMP_RESUME_ADDR: u64 = 0;
/// 临时断点是否由本状态机植入（false = 该处已有真实断点，命中后只重插原断点）。
static mut TEMP_PLANTED: bool = false;
/// true = 命中临时断点后停回 GDB（`s` 单步语义）；false = 静默继续（`c`）。
static mut TEMP_STOP_REPORT: bool = false;

/// 读 `addr` 处原指令宽度（RVC：首半字低 2 位 != 0b11 → 2 字节，否则 4）。
fn insn_len_at(addr: u64) -> usize {
    let mut b = [0u8; 4];
    if mem_read(addr, 4, &mut b) == 4 {
        if b[0] & 0x3 != 0x3 {
            2
        } else {
            4
        }
    } else {
        4
    }
}

/// 武装「临时推进断点」：摘除 `bp_addr` 处断点，在其下一条指令处植入
/// 临时 ebreak（若该处已有真实断点则不植入，命中时只重插原断点并停下）。
/// `report` = 命中后是否停回 GDB（`s` 单步语义）。
unsafe fn arm_temp_advance(bp_addr: u64, report: bool) {
    unsafe {
        // 先清算残留的旧临时断点（R6-M）：断点 A 在分支指令、B 在跳转目标
        // 的场景中，A 处 `c` 武装的 TEMP 在 B 停下时仍活着——直接覆写会
        // 让 A 永久丢失、A+ilen 留下幽灵 ebreak。
        if TEMP_BP_ADDR != 0 {
            if TEMP_PLANTED {
                bp_remove(TEMP_BP_ADDR);
            }
            bp_reinsert(TEMP_RESUME_ADDR);
            TEMP_BP_ADDR = 0;
            TEMP_RESUME_ADDR = 0;
            TEMP_PLANTED = false;
            TEMP_STOP_REPORT = false;
        }
        let ilen = match bp_find(bp_addr) {
            Some(i) => {
                let o = BPS[i].orig;
                if (o & 0xFFFF) as u8 & 0x3 != 0x3 {
                    2
                } else {
                    4
                }
            }
            // bp_addr 处无断点也适用：非断点处 `s` 的单步推进（R6-H，
            // 原 Step 分支只处理「停在断点上」的情况 → 连续两次 s 自由运行）
            None => insn_len_at(bp_addr),
        };
        let next = bp_addr + ilen as u64;
        if bp_find(bp_addr).is_some() {
            bp_remove(bp_addr);
        }
        TEMP_RESUME_ADDR = bp_addr;
        TEMP_BP_ADDR = next;
        TEMP_STOP_REPORT = report;
        TEMP_PLANTED = bp_find(next).is_none();
        if TEMP_PLANTED && !bp_set(next) {
            // 植入失败（越界/不可写）：回滚——重插原断点并放弃推进，
            // 目标停在原地，绝不自由运行（R6-L）
            bp_reinsert(bp_addr);
            TEMP_BP_ADDR = 0;
            TEMP_RESUME_ADDR = 0;
            TEMP_PLANTED = false;
            TEMP_STOP_REPORT = false;
        }
    }
}

/// 单步标记（RISC-V S 模式无 MDSCR_EL1.SS 硬件单步 —— dcsr 在 M 模式；
/// 置位时 GDB 桩在指令边界经软件断点推进，见 STEP_MODE）。
fn set_ss(_on: bool) {}

/// 读 scause / sepc / sstatus（陷入现场 CSR）。
fn read_esr() -> u64 {
    let v: u64;
    unsafe { asm!("csrr {}, scause", out(reg) v) };
    v
}
fn read_elr() -> u64 {
    let v: u64;
    unsafe { asm!("csrr {}, sepc", out(reg) v) };
    v
}
fn read_spsr() -> u64 {
    let v: u64;
    unsafe { asm!("csrr {}, sstatus", out(reg) v) };
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

/// `trap_dispatch` 在 scause = 3（ebreak，来自 U 或 S）时调用。
/// 返回 0 = 恢复现场并 sret（继续运行）；非 0 = 走 el0_fault / 内核诊断
/// （未注册断点的 ebreak，按 SIGTRAP 终止进程）。
/// RISC-V S 模式无硬件单步（dcsr 在 M 模式），步进经软件断点推进。
#[no_mangle]
pub extern "C" fn gdb_debug_entry(frame: *mut u64) -> u32 {
    let esr = read_esr();
    let ec = esr & 0xFFF;
    let mut regs = RegSet::from_trap_frame(frame);

    match ec {
        3 => {
            // 临时推进断点命中：摘临时断点 → 重插原断点 → 静默继续或停回 GDB
            unsafe {
                if TEMP_BP_ADDR != 0 && regs.pc == TEMP_BP_ADDR {
                    if TEMP_PLANTED {
                        bp_remove(regs.pc);
                    }
                    bp_reinsert(TEMP_RESUME_ADDR);
                    let report = TEMP_STOP_REPORT;
                    TEMP_BP_ADDR = 0;
                    TEMP_RESUME_ADDR = 0;
                    TEMP_PLANTED = false;
                    TEMP_STOP_REPORT = false;
                    if !report {
                        regs.write_to_trap_frame(frame);
                        return 0; // `c` 的静默推进：继续运行
                    }
                    // `s` 的单步：落在下一条指令，停回 GDB（落入下方循环）
                } else if bp_find(regs.pc).is_none() {
                    // ebreak：必须是已注册断点，否则交给 el0_fault（SIGTRAP 终止）
                    return 1;
                }
            }
        }
        4 if false => {
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
            // RISC-V：断点处继续 = 摘断点 + 下一条指令植入临时推进断点
            //（命中后重插原断点并静默继续）。原 AArch64 set_ss 方案在
            // S 模式无硬件单步下是空操作 → 断点一次性消失（R5-L1）。
            if ec == 3 {
                if bp_find(regs.pc).is_some() {
                    unsafe { arm_temp_advance(regs.pc, false) };
                }
            }
        }
        LoopResult::Step => {
            // 单步：武装临时推进断点，命中后停回 GDB（pc = 下一条指令）。
            // 不要求当前停在断点上——非断点处 `s` 同样推进（R6-H：原实现
            // 此处什么都不做 → sret 后自由运行，GDB 挂起）
            unsafe { arm_temp_advance(regs.pc, true) };
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
// ==================== 手动入口（shell `gdb` / panic） ====================

/// GDB 断点入口（内核上下文）：保存现场并进入 GDB 循环；`c` 后恢复
/// x0-x30 并返回调用方。sp/pc/cpsr 的写不生效（内核上下文无异常返回路径）。
pub fn gdb_break() {
    unsafe { asm!("csrci sstatus, 2") };
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
    unsafe { asm!("csrsi sstatus, 2") };
    restore_gprs();
}

/// 当前断点数（诊断）。
pub fn bp_count() -> usize {
    unsafe { BP_COUNT }
}
