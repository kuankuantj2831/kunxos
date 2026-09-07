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

    // 中断风暴探测器：同一中断连续重入超阈值时转储 GIC 状态（冷路径，
    // 正常运行下 STORM_N 永远到不了阈值；把「静默冻结」变成可诊断现场）。
    unsafe {
        static mut STORM_IRQ: u32 = u32::MAX;
        static mut STORM_N: u64 = 0;
        if irq == STORM_IRQ {
            STORM_N += 1;
        } else {
            STORM_IRQ = irq;
            STORM_N = 0;
        }
        if STORM_N == 2000 || STORM_N == 20000 || STORM_N == 200000 {
            // GICD_ISPENDR0/1（挂起）0x200/0x204、GICD_ICACTIVER0（SGI/PPI 活动）
            // 0x300、GICD_ICACTIVER1（SPI 活动）0x304、GICC_RPR（运行优先级）0x14。
            let (pend0, rpr) = gic::gic_diag_read(0x200, 0x14);
            let (pend1, act1) = gic::gic_diag_read(0x204, 0x304);
            let (act0, _ctlr) = gic::gic_diag_read(0x300, 0x000);
            crate::uart_send_str("[storm] irq=");
            crate::print_dec(irq as u64);
            crate::uart_send_str(" iar=");
            crate::print_hex(iar as u64);
            crate::uart_send_str(" pend0=");
            crate::print_hex(pend0 as u64);
            crate::uart_send_str(" pend1=");
            crate::print_hex(pend1 as u64);
            crate::uart_send_str(" act0=");
            crate::print_hex(act0 as u64);
            crate::uart_send_str(" act1=");
            crate::print_hex(act1 as u64);
            crate::uart_send_str(" rpr=");
            crate::print_hex(rpr as u64);
            crate::uart_send_str(" pid=");
            crate::print_dec(crate::process::current_pid() as u64);
            crate::uart_send_str("\n");
        }
        // 风暴护栏（冷路径，每次风暴 episode 只执行一次）：把风暴源中断号从
        // GICD 层禁用（GICD_ICENABLERn 写 1 清除对应 enable 位，复用
        // gic::gic_disable_irq）。只清一个 enable 位，其余中断照常投递，
        // 调度不受影响。
        //
        // ★ 仅对 SPI（irq>=32）执行屏蔽写：GICv2 架构上 SGI/PPI 的
        // ICENABLER 写是 RAO/WI（无效），但 QEMU 的 GIC 实现会照单全收 ——
        // 此前对 PPI 风暴（iar=0x1b，CNTV 调度器时钟）写屏蔽直接把调度器
        // tick 关掉，整机冻结（kunx_pkg_test 实测：护栏一触发系统即停摆）。
        // PPI 风暴 episode 会自行消散（应用回归 33 次探测期间系统全程正常
        // 推进），这里只把诊断静音，不做屏蔽写。
        if STORM_N == 200000 {
            if (32..1020).contains(&irq) {
                gic::gic_disable_irq(irq);
                crate::uart_send_str("[storm] 已屏蔽 irq=");
                crate::print_dec(irq as u64);
                crate::uart_send_str("（风暴护栏）\n");
            }
            // STORM_N 推到永不匹配打印阈值的量级：即使屏蔽写被硬件忽略
            // （GICv2 架构上 SGI 的 ICENABLER 写是 RAO/WI）导致同一中断号
            // 继续风暴，也不再打印刷屏；irq 换号时计数自然复位重新探测。
            STORM_N = u64::MAX / 2;
        }
    }

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
