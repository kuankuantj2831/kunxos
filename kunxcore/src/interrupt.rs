//! 陷入与中断处理：统一陷入分发入口 + PLIC 外部中断分发。
//!
//! RISC-V 无 aarch64 的 16 槽异常向量表：所有陷入经单一 `stvec` 进入
//! `trap_entry` 汇编（main.rs，保存完整 832 字节陷阱帧后调用本模块的
//! [`trap_dispatch`]），由 `scause` 分派：
//!   - 中断（scause 最高位 1）：5 = 监督定时器（SBI，双通道复用）、
//!     9 = 外部中断（PLIC claim/complete）、1 = 软件中断（IPI，清挂起位）
//!   - 异常：8 = U 模式 ecall（系统调用，sepc+4）、12/13/15 = U 模式缺页
//!     （按需分页/COW）、3 = 断点（GDB 桩）、其余 U 模式异常 → el0_fault；
//!     S 模式异常 → sync_exception_handler（内核诊断，不返回）
//!
//! 中断处理函数表沿用 aarch64 版结构：`irq -> 处理函数` 映射 + per-IRQ
//! 分发计数（`irqstat` 观测点）+ 风暴 episode 环形日志（PLIC 无 IAR/CPUID
//! 位，快照改为使能位图）。

use core::arch::asm;

use crate::plic;
use crate::syscall_table::TrapFrame;

/// 中断处理函数类型。
pub type IrqHandler = fn();

/// 最大有效中断号（QEMU virt PLIC ndev=95）。
const MAX_IRQ: usize = 96;

/// 中断处理函数表：irq -> 处理函数。
static mut HANDLERS: [Option<IrqHandler>; MAX_IRQ] = [None; MAX_IRQ];

// ==================== 中断观测点（irqstat：风暴源定位） ====================
//
// 中断风暴只有风暴时刻的一次性转储，缺少「episode 前后分布」与「常态每源
// 分发频率」两类数据。观测点：
//   1. 每 IRQ 分发计数（IRQ_COUNTS）：irqstat 输出，定位高频源/风暴贡献者；
//   2. 风暴 episode 环形日志（STORM_LOG）：转储阈值时刻的 PLIC 状态快照，
//      事后可回溯，不必盯着串口抓现场。

/// 每 IRQ 分发计数（不含定时器——监督定时器不走 PLIC）。
static mut IRQ_COUNTS: [u64; MAX_IRQ] = [0; MAX_IRQ];

/// 一次风暴 episode 在转储阈值时刻的状态快照。
#[derive(Clone, Copy)]
pub struct StormEp {
    /// 风暴中断号（PLIC claim 值）。
    pub irq: u32,
    /// 连续重入次数（阈值 2000/20000/200000 各记一条）。
    pub n: u64,
    /// 快照时刻（µs，timer::now_us）。
    pub t_us: u64,
    /// 使能位图字 0/1/2（PLIC enable[ctx]）。
    pub pend0: u32,
    pub pend1: u32,
    pub act0: u32,
    pub act1: u32,
    /// 保留（GICC_RPR 的 RISC-V 无对应物，恒 0）。
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
unsafe fn storm_ep_record(irq: u32, n: u64) {
    let ep = StormEp {
        irq,
        n,
        t_us: crate::timer::now_us(),
        pend0: plic::plic_enable_read(0),
        pend1: plic::plic_enable_read(1),
        act0: plic::plic_enable_read(2),
        act1: 0,
        rpr: 0,
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

/// 风暴探测器：同一中断连续重入超阈值时转储状态（正常路径零阈值）。
unsafe fn storm_probe(irq: u32) {
    static mut STORM_IRQ: u32 = u32::MAX;
    static mut STORM_N: u64 = 0;
    if irq == STORM_IRQ {
        STORM_N += 1;
        // 风暴期间 CPU 被同号中断持续重入占据（episode 可达数百 ms），
        // 远超 16550 RX FIFO 容量——每 256 次重入主动排空一次（常态零开销），
        // 注入的触摸帧/终端输入不再按帧丢失。
        if STORM_N & 0xFF == 0 {
            crate::uart_rx_drain();
        }
    } else {
        STORM_IRQ = irq;
        STORM_N = 0;
    }
    if STORM_N == 2000 || STORM_N == 20000 || STORM_N == 200000 {
        crate::uart_send_str("[storm] irq=");
        crate::print_dec(irq as u64);
        crate::uart_send_str(" enable0=");
        crate::print_hex(plic::plic_enable_read(0) as u64);
        crate::uart_send_str(" enable1=");
        crate::print_hex(plic::plic_enable_read(1) as u64);
        crate::uart_send_str(" pid=");
        crate::print_dec(crate::process::current_pid() as u64);
        crate::uart_send_str("\n");
        storm_ep_record(irq, STORM_N);
        // 转储全程 IRQ 关闭且逐字节忙等 TX：间隙主动排空 UART RX，
        // 避免 16 字节硬件 FIFO 溢出丢输入。
        crate::uart_rx_drain();
    }
    // 风暴护栏（冷路径，每次 episode 只执行一次）：把风暴源从 PLIC 层禁用。
    // 只清一个 enable 位，其余中断照常投递，调度不受影响。
    if STORM_N == 200000 {
        plic::plic_disable_irq(irq);
        crate::uart_send_str("[storm] 已屏蔽 irq=");
        crate::print_dec(irq as u64);
        crate::uart_send_str("（风暴护栏）\n");
        // 即使屏蔽写被硬件忽略导致同一中断号继续风暴，也不再打印刷屏；
        // irq 换号时计数自然复位重新探测。
        STORM_N = u64::MAX / 2;
    }
}

/// 外部中断（scause=9）处理：claim → 计数/风暴探测 → 分发 → complete。
///
/// claim 后立即 complete 前置在分发前还是后？PLIC 与 GIC 相反：claim 即
/// 应答（清 pending），complete 释放线；电平触发源（virtio）在 handler
/// 清源前 complete 会立刻重挂起——因此 **先分发后 complete**，并循环
/// claim 直到无挂起（多源并发一次陷入全处理，减少异常往返）。
fn external_irq_dispatch() {
    loop {
        let irq = plic::plic_claim();
        if irq == 0 || irq as usize >= MAX_IRQ {
            return;
        }
        unsafe { IRQ_COUNTS[irq as usize] += 1 };
        unsafe { storm_probe(irq) };
        if let Some(handler) = unsafe { HANDLERS[irq as usize] } {
            handler();
        }
        plic::plic_complete(irq);
    }
}

/// 读取 scause / stval / sstatus（陷入现场 CSR）。
#[inline]
fn read_causes() -> (u64, u64, u64) {
    let cause: u64;
    let tval: u64;
    let status: u64;
    unsafe {
        asm!("csrr {}, scause", out(reg) cause);
        asm!("csrr {}, stval", out(reg) tval);
        asm!("csrr {}, sstatus", out(reg) status);
    }
    (cause, tval, status)
}

/// 统一陷入分发入口，由 main.rs `trap_entry` 汇编调用（a0 = 陷阱帧指针）。
///
/// 正常路径（中断/系统调用/已解析缺页/GDB 继续）返回到汇编统一恢复点；
/// 用户程序故障走 `el0_fault_handler`（不返回）；S 模式未处理异常走
/// `sync_exception_handler`（不返回）。
#[no_mangle]
pub extern "C" fn trap_dispatch(frame: *mut TrapFrame) {
    let (cause, tval, status) = read_causes();
    let is_interrupt = cause >> 63 != 0;
    let code = cause & 0xFFF_FFFF_FFFF_FFFF;

    if is_interrupt {
        match code {
            5 => {
                // 监督定时器：动画通道先判到期（120Hz 与调度 10ms 复用硬件）
                if crate::timer::ch2_due() {
                    crate::timer::timer2_handle_irq();
                }
                crate::timer::timer_handle_irq();
                crate::sched_cfs::resched_if_needed();
            }
            9 => {
                external_irq_dispatch();
                crate::sched_cfs::resched_if_needed();
            }
            1 => {
                // 软件中断（IPI）：清 SSIP 挂起即可
                unsafe { asm!("csrc sip, 2") };
            }
            _ => {
                // 未知中断源：诊断后继续（不挂死整机）
                crate::uart_send_str("[trap] 未知中断 cause=");
                crate::print_dec(code);
                crate::uart_send_str("\n");
            }
        }
        return;
    }

    // ---- 异常：SPP（sstatus bit8）= 0 表示来自用户态 ----
    let from_user = status & (1 << 8) == 0;
    match code {
        8 if from_user => {
            // ecall from U：系统调用。sepc 指向 ecall 本身，前进 4 字节
            unsafe {
                (*frame).elr += 4;
                // 恢复 IRQ（syscall 处理可被抢占：长调用/阻塞语义）；
                // trap_return 恢复现场前会重新屏蔽
                asm!("csrsi sstatus, 2");
            }
            crate::syscall_table::syscall_dispatch(frame);
        }
        12 | 13 | 15 if from_user => {
            // 取指/读/写缺页：按需分页（demand paging / COW / LAZY 落地）
            if crate::el0_page_fault_handler(frame as *mut u8, tval as usize) {
                return; // 已落地：恢复现场重试故障指令
            }
            crate::el0_fault_handler(frame as *mut u8);
        }
        3 => {
            // 断点：GDB 调试桩（用户态与内核态都可命中）
            // 返回 0 = 恢复现场继续；非 0 = 按来源走终止/内核诊断
            if crate::gdbstub::gdb_debug_entry(frame as *mut u64) == 0 {
                return;
            }
            if from_user {
                crate::el0_fault_handler(frame as *mut u8);
            } else {
                crate::sync_exception_handler();
            }
        }
        _ if from_user => {
            // 其余用户态异常（非法指令/访存错/未对齐）：SIGSEGV 语义终止
            crate::el0_fault_handler(frame as *mut u8);
        }
        _ => {
            // S 模式异常：内核诊断（不返回）
            crate::sync_exception_handler();
        }
    }
}
