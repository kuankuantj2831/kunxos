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

// ==================== 中断观测点（irqstat：风暴源定位） ====================
//
// 中断风暴（尤其 iar=0/SGI 锁存与 dist 寄存器视图自相矛盾的 act0 待复现
// 案例，见 BUG_REPORT「中断风暴 iar=0」）此前只有风暴时刻的一次性转储，
// 缺少「episode 前后分布」与「常态每源分发频率」两类数据。观测点：
//   1. 每 IRQ 分发计数（IRQ_COUNTS）：irqstat 输出，定位高频源/风暴贡献者；
//   2. 风暴 episode 环形日志（STORM_LOG）：转储阈值时刻的 GIC 状态快照
//      （含 act0），事后可回溯，不必盯着串口抓现场。

/// 每 IRQ 分发计数（含定时器；1020+ 伪中断不计）。
static mut IRQ_COUNTS: [u64; MAX_IRQ] = [0; MAX_IRQ];

/// 一次风暴 episode 在转储阈值时刻的 GIC 状态快照。
#[derive(Clone, Copy)]
pub struct StormEp {
    /// 风暴中断号（IAR 低 10 位）。
    pub irq: u32,
    /// 连续重入次数（阈值 2000/20000/200000 各记一条）。
    pub n: u64,
    /// 快照时刻（µs，timer::now_us）。
    pub t_us: u64,
    /// GICD_ISPENDR0/1（挂起）。
    pub pend0: u32,
    pub pend1: u32,
    /// GICD_ICACTIVER0（SGI/PPI 活动）/1（SPI 活动）。
    pub act0: u32,
    pub act1: u32,
    /// GICC_RPR（运行优先级）。
    pub rpr: u32,
    /// 快照时刻当前 pid。
    pub pid: u32,
}

const EMPTY_EP: StormEp = StormEp {
    irq: u32::MAX,
    n: 0,
    t_us: 0,
    pend0: 0,
    pend1: 0,
    act0: 0,
    act1: 0,
    rpr: 0,
    pid: u32::MAX,
};

/// 风暴 episode 环形日志（最近 8 条；新→旧遍历）。
static mut STORM_LOG: [StormEp; 8] = [EMPTY_EP; 8];
/// 累计写入条数（环形游标 = STORM_LOG_N % 8）。
static mut STORM_LOG_N: usize = 0;

/// 记录一条风暴 episode（转储阈值时刻调用，IRQ 已关闭）。
unsafe fn storm_ep_record(irq: u32, n: u64, pend0: u32, pend1: u32, act0: u32, act1: u32, rpr: u32) {
    let ep = StormEp {
        irq,
        n,
        t_us: crate::timer::now_us(),
        pend0,
        pend1,
        act0,
        act1,
        rpr,
        pid: crate::process::current_pid(),
    };
    STORM_LOG[STORM_LOG_N % 8] = ep;
    STORM_LOG_N = STORM_LOG_N.wrapping_add(1);
}

/// 遍历非零 per-IRQ 分发计数（IRQ 号升序；shell `irqstat` 观测点）。
pub fn for_each_irq_count(f: &mut dyn FnMut(u32, u64)) {
    for i in 0..MAX_IRQ {
        let c = unsafe { IRQ_COUNTS[i] };
        if c != 0 {
            f(i as u32, c);
        }
    }
}

/// 遍历风暴 episode 历史（新→旧，最多 8 条；shell `irqstat` 观测点）。
pub fn for_each_storm_ep(f: &mut dyn FnMut(&StormEp)) {
    let n = unsafe { STORM_LOG_N };
    for k in 0..n.min(8) {
        let e = unsafe { &STORM_LOG[(n - 1 - k) % 8] };
        f(e);
    }
}

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
            // 风暴期间 CPU 被同号中断持续重入占据（episode 可达数百 ms），
            // 远超 PL011 RX FIFO 的 ~1.4ms 容量 —— 仅在转储时刻排空 RX 不够。
            // 每 256 次重入主动排空一次（一次 volatile 读，常态零开销），
            // 注入的触摸帧/终端输入不再按帧丢失。
            if STORM_N & 0xFF == 0 {
                crate::uart_rx_drain();
            }
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
            // episode 落入环形日志（irqstat 事后回溯，不必盯串口抓现场）
            storm_ep_record(irq, STORM_N, pend0, pend1, act0, act1, rpr);
            // 转储全程 IRQ 关闭且逐字节忙等 TX（~13ms）：间隙主动排空
            // UART RX，避免 16 字节硬件 FIFO 溢出丢输入。
            crate::uart_rx_drain();
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

    // 每 IRQ 分发计数（irqstat 观测点；一次数组写，与风暴探测器同级的
    // 常态开销）
    unsafe { IRQ_COUNTS[irq as usize] += 1 };

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
