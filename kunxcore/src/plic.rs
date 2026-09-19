//! PLIC（Platform-Level Interrupt Controller）驱动，替换 aarch64 的 GICv2。
//!
//! QEMU virt RISC-V 的 PLIC（`sifive,plic-1.0.0`，DTB `riscv,ndev = 95`）：
//!   - 优先级：`base + irq*4`（0 = 禁止投递，≥1 参与）
//!   - 使能：`base + 0x2000 + ctx*0x80 + (irq/32)*4`
//!   - 每上下文寄存器块：`base + 0x200000 + ctx*0x1000`
//!     （+0 threshold，+4 claim/complete）
//!
//! 上下文编号：每 hart 两个（M 模式 ctx=2*hart，S 模式 ctx=2*hart+1）；
//! 内核运行于 S 模式，hart0 的 S 上下文 = 1。claim 读即应答，处理完成后
//! 写回同一 IRQ 号即 EOI——对应 GIC 的 IAR/EOIR 语义，但顺序相反
//! （GIC 先 EOI 再分发，PLIC 必须 claim→分发→complete）。

use core::ptr::{read_volatile, write_volatile};

/// PLIC 基地址默认值（QEMU virt），设备树解析失败时回退使用。
pub const PLIC_DEFAULT: usize = 0x0C00_0000;

/// PLIC 支持的最大中断号（QEMU virt ndev=95，含 0 号保留位）。
const MAX_IRQ: usize = 96;

// 寄存器布局偏移
const PRIO_BASE: usize = 0x0000;
const ENABLE_BASE: usize = 0x2000;
/// 每上下文使能区位图跨度（0x80 = 128 字，覆盖 4096 源，FU540 兼容布局，
/// Linux `irq-sifive-plic` 与 QEMU `sifive_plic` 同此值）。
const ENABLE_STRIDE: usize = 0x80;
const CONTEXT_BASE: usize = 0x2000_00;
const CONTEXT_STRIDE: usize = 0x1000;
const CONTEXT_THRESHOLD: usize = 0x0;
const CONTEXT_CLAIM: usize = 0x4;

/// PLIC 基地址（初始化后固定）。
static mut PLIC_BASE: usize = PLIC_DEFAULT;

/// hart0 S 模式上下文号（M=0、S=1）。
const S_CONTEXT: usize = 1;

fn base() -> usize {
    unsafe { PLIC_BASE }
}

fn ctx_addr(ctx: usize) -> usize {
    base() + CONTEXT_BASE + ctx * CONTEXT_STRIDE
}

/// 初始化 PLIC（CPU0 主核）：所有优先级置 1、屏蔽全部中断源，
/// 打开 hart0 S 上下文（threshold=0 放行所有优先级）。
///
/// 对应 `gic_init(base, cpu_base)`：参数换成 PLIC 单基地址。
pub fn plic_init(b: usize) {
    unsafe {
        PLIC_BASE = b;
        // 优先级 1（最低可投递）；源 0 保留不动
        for irq in 1..MAX_IRQ {
            write_volatile((base() + PRIO_BASE + irq * 4) as *mut u32, 1);
        }
        // 屏蔽 hart0 S 上下文全部源（随后按需 enable）
        for i in 0..(MAX_IRQ / 32) {
            write_volatile(
                (base() + ENABLE_BASE + S_CONTEXT * ENABLE_STRIDE + i * 4) as *mut u32,
                0,
            );
        }
        // threshold=0：放行优先级 ≥1 的全部中断
        write_volatile((ctx_addr(S_CONTEXT) + CONTEXT_THRESHOLD) as *mut u32, 0);
    }
}

/// 使能当前 hart 的 S 上下文（secondary 核进入调度前调用，
/// 对应 `gic_init_secondary`）。
pub fn plic_init_secondary() {
    let ctx = S_CONTEXT + 2 * crate::smp::cpu_id() as usize;
    unsafe {
        for i in 0..(MAX_IRQ / 32) {
            let addr = base() + ENABLE_BASE + ctx * ENABLE_STRIDE + i * 4;
            write_volatile(addr as *mut u32, 0);
        }
        write_volatile((ctx_addr(ctx) + CONTEXT_THRESHOLD) as *mut u32, 0);
    }
}

/// 当前核 S 上下文号。
fn cur_sctx() -> usize {
    S_CONTEXT + 2 * crate::smp::cpu_id() as usize
}

/// 使能指定中断号（写使能位图对应位）。
pub fn plic_enable_irq(irq: u32) {
    assert!((irq as usize) < MAX_IRQ, "plic：中断号超出范围");
    let ctx = cur_sctx();
    let reg = (irq / 32) as usize;
    let bit = irq % 32;
    unsafe {
        let addr = base() + ENABLE_BASE + ctx * ENABLE_STRIDE + reg * 4;
        let val = read_volatile(addr as *const u32);
        write_volatile(addr as *mut u32, val | (1 << bit));
    }
}

/// 禁用指定中断号（中断风暴护栏屏蔽风暴源，冷路径）。
pub fn plic_disable_irq(irq: u32) {
    assert!((irq as usize) < MAX_IRQ, "plic：中断号超出范围");
    let ctx = cur_sctx();
    let reg = (irq / 32) as usize;
    let bit = irq % 32;
    unsafe {
        let addr = base() + ENABLE_BASE + ctx * ENABLE_STRIDE + reg * 4;
        let val = read_volatile(addr as *const u32);
        write_volatile(addr as *mut u32, val & !(1 << bit));
    }
}

/// 设置中断优先级（1=最低、7=最高；0=禁止投递）。
///
/// 语义与 GIC 相反方向（GIC 低值=高优先级），调用方传 GIC 风格值时
/// 在此换算，保持 `gic_set_priority(irq, 0x80)` 一类的中优先级行为。
pub fn plic_set_priority(irq: u32, _gic_style_pri: u8) {
    assert!((irq as usize) < MAX_IRQ, "plic：中断号超出范围");
    let _ = _gic_style_pri;
    // 统一置 1（最低）：内核不依赖 PLIC 优先级抢占（S 模式无 IRQ 抢占层级）
    unsafe {
        write_volatile((base() + PRIO_BASE + irq as usize * 4) as *mut u32, 1);
    }
}

/// GIC 目标核设置的空操作保留接口（PLIC 经 hart 上下文寄存器天然路由，
/// `plic_enable_irq` 只作用于当前核）。
pub fn plic_set_target(_irq: u32, _cpu: u8) {}

/// 应答并取号：读 claim 寄存器返回当前最高优先级挂起中断（0 = 无）。
///
/// 对应 `gic_read_iar`。**注意**：claim 即应答，此后到 complete 之前同源
/// 不会重投——分发必须及时 `plic_complete`。
pub fn plic_claim() -> u32 {
    unsafe { read_volatile((ctx_addr(cur_sctx()) + CONTEXT_CLAIM) as *const u32) }
}

/// 完成处理：写回 claim 到的 IRQ 号（对应 `gic_write_eoir`）。
pub fn plic_complete(irq: u32) {
    unsafe {
        write_volatile((ctx_addr(cur_sctx()) + CONTEXT_CLAIM) as *mut u32, irq);
    }
}

/// 诊断读取：使能位图第 `word` 字（风暴探测器转储用）。
pub fn plic_enable_read(word: usize) -> u32 {
    if word >= MAX_IRQ / 32 {
        return 0;
    }
    unsafe {
        read_volatile(
            (base() + ENABLE_BASE + cur_sctx() * ENABLE_STRIDE + word * 4) as *const u32,
        )
    }
}
