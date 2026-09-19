//! SMP 多核启动（QEMU virt RISC-V）。
//!
//! 与 aarch64（PSCI CPU_ON / QEMU loader 引导 secondary）不同：OpenSBI 把
//! **所有 hart** 都带到内核 `_start`（a0=hartid）。非 boot hart 在启动汇编
//! `park` 停车场自旋等待 `SCHED_READY`（主核完成 PLIC/定时器/调度器初始
//! 化），随后设置 per-hart 栈、调用 `smp_secondary_main` 进入调度器。
//!
//! 各 secondary hart 流程：等待 `SCHED_READY` → 初始化本核 satp/FS/SUM
//! （`mmu::init_secondary`）与定时器 → 进入调度器运行本核 idle 任务，由每核
//! 定时器 tick 从全局就绪队列取任务（天然负载均衡）。

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// 每核栈大小（secondary 专用）。
const STACK_SIZE: usize = 16384;
/// 除 CPU0 外最多支持的核数（与调度器 MAX_CPUS 对应，总核数 ≤ 4）。
const MAX_SECONDARY: usize = 3;

/// secondary 专用栈区（CPU1 起，每核 16KB；CPU0 用主栈）。
#[no_mangle]
pub static SECONDARY_STACKS: [u8; STACK_SIZE * MAX_SECONDARY] = [0; STACK_SIZE * MAX_SECONDARY];

/// 调度器 / 中断基础设施就绪标志：secondary 核等待此标志后进入调度循环
/// （启动汇编 park 停车场按字节轮询）。
#[no_mangle]
pub static SCHED_READY: AtomicBool = AtomicBool::new(false);

/// 当前 CPU 的 hartid（启动汇编写入 tp，此处读取）。
pub fn cpu_id() -> u32 {
    let hart: u64;
    unsafe { asm!("mv {}, tp", out(reg) hart, options(nostack)) };
    hart as u32
}

/// 从设备树统计 CPU 数（`/cpus` 下 compatible = "riscv" 的节点数）。
pub fn cpu_count() -> u32 {
    unsafe {
        if let Some(fdt) = crate::fdt::Fdt::parse(crate::DTB_ADDR) {
            let mut nodes = [crate::fdt::Node::empty(); 8];
            let n = fdt.find_all_compatible("riscv", &mut nodes);
            if n > 0 {
                return n as u32;
            }
        }
    }
    1
}

/// 多核启动（CPU0 侧）：统计核数并报告。
///
/// RISC-V 上无需显式唤醒：所有 hart 经 OpenSBI 进入 `_start`，非 boot hart
/// 已在停车场等待 `SCHED_READY`（rust_main 尾部置位）。
pub fn smp_init() {
    let total = cpu_count();
    if total <= 1 {
        crate::kinfo! { crate::uart_send_str("[smp] cpus=1\n"); }
        return;
    }
    crate::kinfo! {
        crate::uart_send_str("[smp] cpus=");
        crate::print_dec(total as u64);
        crate::uart_send_str("（secondary 停车场等待调度就绪）\n");
    }
}

/// secondary 核的 Rust 主函数（汇编停车场调用，a0 = hartid）。
///
/// 等待主核完成 PLIC / 定时器 / 调度器初始化后，初始化本核地址翻译状态
/// （satp/FS/SUM）与定时器，再进入调度器运行本核 idle 任务。
#[no_mangle]
pub extern "C" fn smp_secondary_main(cpu: u32) -> ! {
    crate::kinfo! {
        crate::uart_send_str("[smp] cpu");
        crate::print_dec(cpu as u64);
        crate::uart_send_str(" 等待调度器就绪\n");
    }

    // 等待主核就绪（IRQ 此时被 SIE 屏蔽，纯自旋）
    while !SCHED_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // 本核地址翻译状态（satp + FS/SUM/MXR，每核 CSR）
    unsafe { crate::mmu::init_secondary() };
    // 本核 PLIC S 上下文（使能位图 + threshold）
    crate::plic::plic_init_secondary();
    // 本核监督定时器：10ms 调度周期 + 调度 tick
    crate::timer::timer_init();
    crate::timer::timer_set_interval(10_000);

    crate::kinfo! {
        crate::uart_send_str("[smp] cpu");
        crate::print_dec(cpu as u64);
        crate::uart_send_str(" 已上线\n");
    }

    // 进入调度器（运行本核 idle 任务；此后由 tick 取全局就绪队列任务）
    crate::sched_cfs::start_secondary(cpu);
}
