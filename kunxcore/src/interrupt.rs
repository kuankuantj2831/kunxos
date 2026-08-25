//! 中断处理：中断处理函数表与 IRQ 分发入口。
//!
//! 维护一个 irq -> 处理函数 的映射表，`irq_handler` 由汇编异常向量表调用，
//! 负责应答中断（IAR）、写 EOI 并查表分发。

use crate::gic;

/// 中断处理函数类型。
pub type IrqHandler = fn();

/// 最大有效中断号（GICv2 IAR 中断号字段为 10 位，0..1019 有效）。
const MAX_IRQ: usize = 1020;

/// 中断处理函数表：irq -> 处理函数。
static mut HANDLERS: [Option<IrqHandler>; MAX_IRQ] = [None; MAX_IRQ];

/// 注册中断处理函数。
pub fn register_handler(irq: u32, handler: IrqHandler) {
    if (irq as usize) < MAX_IRQ {
        unsafe {
            HANDLERS[irq as usize] = Some(handler);
        }
    }
}

/// 注销中断处理函数。
#[allow(dead_code)]
pub fn unregister_handler(irq: u32) {
    if (irq as usize) < MAX_IRQ {
        unsafe {
            HANDLERS[irq as usize] = None;
        }
    }
}

/// IRQ 处理入口，由汇编异常向量表调用。
///
/// 流程：读 IAR 应答中断 → 立即写 EOI（释放中断线、降低优先级）→ 查表分发。
/// 在分发前完成 EOI，是因为定时器等处理函数会触发上下文切换而不返回，
/// 若 EOI 推迟到返回之后，GIC 会一直认为该中断仍处于 active 状态，导致
/// 后续同优先级中断无法投递。
#[no_mangle]
pub extern "C" fn irq_handler() {
    let iar = gic::gic_read_iar();
    let irq = iar & 0x3FF;

    // 1020-1023 为特殊值（spurious / 保留），直接返回
    if irq >= 1020 {
        return;
    }

    // 定时器（CNTV 调度 tick / CNTP 动画帧）：先确认（重装清除中断源）再 EOI，
    // 避免电平触发重触发风暴；之后再分发（调度器 tick 可能切换上下文，
    // EOI 已在切换前完成）。动画定时器回调不切换上下文，同样安全。
    if irq == crate::timer::TIMER_IRQ || irq == crate::timer::TIMER2_IRQ {
        if irq == crate::timer::TIMER_IRQ {
            crate::timer::timer_ack();
        } else {
            crate::timer::timer2_ack();
        }
        gic::gic_write_eoir(iar);
        if let Some(handler) = unsafe { HANDLERS[irq as usize] } {
            handler();
        }
        crate::sched_cfs::resched_if_needed();
        return;
    }

    // 其余中断：先写 EOI 再分发（必须传入含 CPUID 位的原始 IAR 值）
    gic::gic_write_eoir(iar);

    if let Some(handler) = unsafe { HANDLERS[irq as usize] } {
        handler();
    }
    crate::sched_cfs::resched_if_needed();
}
