//! SMP 多核启动（QEMU virt）。
//!
//! secondary 核由 QEMU 的 `-device loader,addr=<secondary_entry>,cpu-num=N`
//! 在复位时直接引导至 `secondary_entry`（见 Makefile `run-smp`）。QEMU
//! `-kernel` 直载模式下 EL1 的 `smc #0` 会触发 Undefined Instruction
//! （PSCI 固件未初始化 SCR_EL3），因此**不使用** PSCI CPU_ON 唤醒。
//!
//! 各 secondary 核流程：等待 `SCHED_READY`（主核完成 GIC/定时器/调度器
//! 初始化）→ 初始化本核 GIC CPU 接口与虚拟定时器 → 进入调度器运行本核
//! idle 任务，由每核定时器 tick 从全局就绪队列取任务（天然负载均衡）。

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// 每核栈大小（secondary 专用）。
const STACK_SIZE: usize = 16384;
/// 除 CPU0 外最多支持的核数（与调度器 MAX_CPUS 对应，总核数 ≤ 4）。
const MAX_SECONDARY: usize = 3;

/// secondary 专用栈区（CPU1 起，每核 16KB；CPU0 用主栈）。
#[no_mangle]
pub static SECONDARY_STACKS: [u8; STACK_SIZE * MAX_SECONDARY] = [0; STACK_SIZE * MAX_SECONDARY];

/// secondary 汇编入口（global_asm 定义；QEMU loader 把对应核引导至此）。
unsafe extern "C" {
    fn secondary_entry();
}

/// 调度器 / 中断基础设施就绪标志：secondary 核等待此标志后进入调度循环。
pub static SCHED_READY: AtomicBool = AtomicBool::new(false);

/// 当前 CPU 的 affinity0 号（QEMU virt：cpu0=0, cpu1=1, ...）。
pub fn cpu_id() -> u32 {
    let mpidr: u64;
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nostack)) };
    (mpidr & 0xFF) as u32
}

/// 从设备树统计 CPU 数（`/cpus` 下 `arm,cortex-a72` 兼容节点数）。
pub fn cpu_count() -> u32 {
    unsafe {
        if let Some(fdt) = crate::fdt::Fdt::parse(crate::DTB_ADDR) {
            let mut nodes = [crate::fdt::Node::empty(); 8];
            let n = fdt.find_all_compatible("arm,cortex-a72", &mut nodes);
            if n > 0 {
                return n as u32;
            }
        }
    }
    1
}

/// PSCI 0.2 调用（`smc #0`）。需 `-machine virt,secure=on`（EL3 固件初始化
/// SCR_EL3.SMC）；否则触发 Undefined Instruction。
#[allow(dead_code)]
fn psci_call(fn_id: u32, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "smc #0",
            inlateout("x0") fn_id as u64 => ret,
            in("x1") arg0,
            in("x2") arg1,
            in("x3") arg2,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// PSCI CPU_ON：引导 `cpu` 核从 `entry` 开始执行，返回 0 表示成功。
#[allow(dead_code)]
fn psci_cpu_on(cpu: u32, entry: u64) -> u64 {
    psci_call(0xC400_0003, cpu as u64, entry, 0)
}

/// 多核启动（CPU0 侧）：统计核数并尝试 PSCI CPU_ON 唤醒 secondary 核。
///
/// 说明（QEMU 11.1 实测）：`-kernel` 直载模式下 secondary 核被 QEMU 置于
/// powered-off（WFI）挂起态，须 PSCI CPU_ON 唤醒；但该模式没有可用固件：
/// - secure=off：无 EL3，`smc #0` 触发 Undefined Instruction（PANIC）
/// - secure=on：QEMU 自带的 EL3 桩不实现 PSCI（smc 落入 udf 死循环）
/// 真正的 PSCI 需要 `-bios` 提供实现 CPU_ON 的 EL3 固件（如 ATF）。
/// 因此默认不调用 PSCI（避免 panic/挂死），仅在确有 PSCI 环境时由
/// `psci_cpu_on` 唤醒；`secondary_entry` 与调度器的每核初始化（GIC/定时器/
/// idle 任务）已就绪，一旦环境提供 PSCI 即可上线。
pub fn smp_init() {
    let total = cpu_count();
    if total <= 1 {
        crate::uart_send_str("[smp] cpus=1\n");
        return;
    }
    crate::uart_send_str("[smp] cpus=");
    crate::print_dec(total as u64);
    crate::uart_send_str(" (secondary wake needs PSCI firmware)\n");
    // 在此插入 psci_cpu_on(cpu, secondary_entry) 循环即可在多核环境唤醒
    // secondary；QEMU -kernel 直载模式无 PSCI（见模块注释），故默认不调用。
}

/// secondary 核的 Rust 主函数（汇编入口调用，x0 = cpu id）。
///
/// 等待主核完成 GIC / 定时器 / 调度器初始化后，初始化本核 GIC CPU
/// 接口与虚拟定时器，再进入调度器运行本核 idle 任务。
#[no_mangle]
pub extern "C" fn smp_secondary_main(cpu: u32) -> ! {
    crate::uart_send_str("[smp] cpu");
    crate::print_dec(cpu as u64);
    crate::uart_send_str(" waiting for scheduler\n");

    // 等待主核就绪（IRQ 此时被 DAIF 屏蔽，纯自旋）
    while !SCHED_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // 本核 GIC CPU 接口（GICC_CTLR/PMR）
    crate::gic::gic_init_secondary();
    // 本核虚拟定时器（CNTV 每核独立）：10ms 调度周期 + 调度 tick
    crate::timer::timer_init();
    crate::timer::timer_set_interval(10_000);
    crate::interrupt::register_handler(crate::timer::TIMER_IRQ, crate::timer::timer_handle_irq);
    crate::gic::gic_enable_irq(crate::timer::TIMER_IRQ);
    crate::gic::gic_set_priority(crate::timer::TIMER_IRQ, 0x80);

    crate::uart_send_str("[smp] cpu");
    crate::print_dec(cpu as u64);
    crate::uart_send_str(" online\n");

    // 进入调度器（运行本核 idle 任务；此后由 tick 取全局就绪队列任务）
    crate::sched_cfs::start_secondary(cpu);
}
