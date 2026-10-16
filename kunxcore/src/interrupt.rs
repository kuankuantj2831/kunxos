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

// ==================== IRQ 故障遏制跟踪 ====================
//
// 「中断处理自身崩溃」没有可归咎的进程（不能杀无辜任务），遏制手段是：
// 屏蔽肇事 IRQ 线 + 放弃本次中断处理、恢复被打断的上下文（帧展开，
// main.rs `irq_fault_contain` + `irq_fault_unwind`）。为此需要知道故障
// 发生时「正在处理哪个中断、其外层陷阱帧在哪」：
//   - IRQ_FRAME[cpu]：trap_dispatch 进入中断分支时记录的外层帧指针；
//   - IRQ_LINE[cpu]：external_irq_dispatch 进入 handler 前记录的线号；
//   - IRQ_KIND[cpu]：5=定时器 9=外部中断。
// 已知局限：处理中途被抢占换出时，槽位可能被后续中断覆写/迟清——只
// 影响遏制归因精度，不影响正常中断路径。

static mut IRQ_FRAME: [usize; MAX_CPUS_TRACK] = [0; MAX_CPUS_TRACK];
static mut IRQ_LINE: [u32; MAX_CPUS_TRACK] = [0; MAX_CPUS_TRACK];
static mut IRQ_KIND: [u32; MAX_CPUS_TRACK] = [0; MAX_CPUS_TRACK];
/// 进入中断处理时的当前任务 tid：时间片抢占会在中断处理中途切走任务
/// （tick → do_switch），槽位残留的是**被切出任务**的帧。若不加归属校验，
/// 新任务在时间片窗口内发生 S 态异常会被误判为「中断处理故障」并展开到
/// 他人帧 —— 跨地址空间 sret（H-1）。取用侧按 tid 一致才承认槽位。
static mut IRQ_TID: [u32; MAX_CPUS_TRACK] = [0; MAX_CPUS_TRACK];
/// 定时器中断内的遏制次数（定时器无法屏蔽——屏蔽即调度停摆；超上限
/// 说明定时器路径有系统性 bug，转为整机诊断停机）。
static mut TIMER_CONTAIN_N: [u32; MAX_CPUS_TRACK] = [0; MAX_CPUS_TRACK];
/// 定时器遏制上限。
const TIMER_CONTAIN_MAX: u32 = 64;

/// CPU 数上界（与 sched_cfs::MAX_CPUS 一致，避免跨模块常量耦合）。
const MAX_CPUS_TRACK: usize = crate::sched_cfs::MAX_CPUS;

fn cur_cpu() -> usize {
    (crate::smp::cpu_id() as usize).min(MAX_CPUS_TRACK - 1)
}

/// 标记进入中断处理（trap_dispatch 中断分支入口调用）。
fn irq_ctx_enter(frame: *mut TrapFrame, kind: u32) {
    unsafe {
        let c = cur_cpu();
        IRQ_FRAME[c] = frame as usize;
        IRQ_KIND[c] = kind;
        IRQ_TID[c] = crate::sched_cfs::current_tid();
    }
}

/// 标记离开中断处理（trap_dispatch 中断分支返回前调用）。
fn irq_ctx_exit() {
    unsafe {
        let c = cur_cpu();
        IRQ_FRAME[c] = 0;
        IRQ_KIND[c] = 0;
        IRQ_LINE[c] = 0;
    }
}

/// 标记进入/离开具体外部中断 handler（external_irq_dispatch 调用）。
fn irq_line_set(irq: u32) {
    unsafe { IRQ_LINE[cur_cpu()] = irq };
}

fn irq_line_clear() {
    unsafe { IRQ_LINE[cur_cpu()] = 0 };
}

/// 当前 CPU 是否处于中断处理上下文（供故障遏制判定），返回外层帧指针。
/// tid 归属校验：槽位记录的是进入中断时的任务；此后时间片抢占切到别的
/// 任务（中断处理中途 do_switch）时，残留帧属于被切出任务 —— 对当前任务
/// 而言不算中断上下文，返回 0（故障按当前任务自己的路径遏制，绝不信
/// 展开到他人帧的跨地址空间恢复）。
pub fn irq_ctx_frame() -> usize {
    unsafe {
        let c = cur_cpu();
        if IRQ_FRAME[c] != 0 && IRQ_TID[c] == crate::sched_cfs::current_tid() {
            IRQ_FRAME[c]
        } else {
            0
        }
    }
}

/// 当前正在处理的外部中断线号（0=不在 handler 内）。
pub fn irq_ctx_line() -> u32 {
    unsafe { IRQ_LINE[cur_cpu()] }
}

/// 当前中断来源（5=定时器 9=外部）。
pub fn irq_ctx_kind() -> u32 {
    unsafe { IRQ_KIND[cur_cpu()] }
}

/// 清除本 CPU 的中断上下文标记（遏制展开前调用）。
pub fn irq_ctx_reset() {
    irq_ctx_exit();
}

/// 定时器遏制计数（超上限不再遏制）。返回 true 表示仍可遏制。
pub fn timer_contain_tick() -> bool {
    unsafe {
        let c = cur_cpu();
        TIMER_CONTAIN_N[c] += 1;
        TIMER_CONTAIN_N[c] <= TIMER_CONTAIN_MAX
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
            // 记录当前 handler 线号：处理中故障时据此屏蔽肇事 IRQ 线
            irq_line_set(irq);
            handler();
            irq_line_clear();
        } else {
            // 未注册 handler 的源：当场从 PLIC 屏蔽并留痕。电平触发源
            // （virtio 等）不处理就永远重挂起 —— complete 后立刻再陷入，
            // 嵌套陷阱帧（每次 832B）会在风暴护栏（20 万次）生效前写穿
            // 内核栈、砸烂静态数据（表现为路径字符串逐步损坏、全零寄存器、
            // 垃圾 sepc 的同步异常）。
            crate::uart_send_str("[irq] 未处理的 IRQ ");
            crate::print_dec(irq as u64);
            crate::uart_send_str("，已在 PLIC 屏蔽\n");
            plic::plic_disable_irq(irq);
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
        irq_ctx_enter(frame, code as u32);
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
        irq_ctx_exit();
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
            // S 模式异常：三级遏制，优先级从高到低——
            //   1. IRQ 遏制：故障发生在中断处理里 → 屏蔽肇事 IRQ 线 +
            //      帧展开恢复被打断的上下文（不杀无辜进程，不返回）；
            //   2. 进程遏制：发生在 syscall 上下文（坏指针/坏数据）→
            //      归咎该进程并终止（不返回）；
            //   3. 内核线程遏制：非 syscall 上下文但当前是可调度内核线程
            //      （wifi/perflog 等）→ 只终止该线程，系统继续运行；
            //   4. 整机诊断停机（idle 自身的 bug → 看门狗最后兜底复位）。
            if !crate::irq_fault_contain(frame, code, tval)
                && !crate::kernel_fault_contain(frame, code, tval)
                && !crate::kthread_fault_contain("S态异常", code, tval)
            {
                crate::sync_exception_handler();
            }
        }
    }
}
