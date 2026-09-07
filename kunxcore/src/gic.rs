//! GICv2 中断控制器抽象。
//!
//! 封装 GIC Distributor 与 CPU Interface 的初始化、中断使能/禁用、
//! 优先级与目标 CPU 配置，以及中断应答（IAR）/结束（EOIR）接口。

use core::ptr::{read_volatile, write_volatile};

/// 默认 GICD / GICC 基地址（QEMU virt），设备树解析失败时回退使用。
const GICD_DEFAULT: usize = 0x0800_0000;
const GICC_DEFAULT: usize = 0x0801_0000;

// GIC Distributor 寄存器偏移
const GICD_CTLR: usize = 0x000;
const GICD_ISENABLER: usize = 0x100;
#[allow(dead_code)]
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;

// GIC CPU Interface 寄存器偏移
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00C;
const GICC_EOIR: usize = 0x010;

/// GIC Distributor 基地址（初始化后固定）。
static mut GICD_BASE: usize = GICD_DEFAULT;
/// CPU0 的 GIC CPU Interface 基地址（初始化后固定）。
static mut GICC_BASE: usize = GICC_DEFAULT;

/// 当前 CPU 的 GICC 基址（QEMU virt GICv2：每核偏移 0x20000）。
///
/// GIC CPU Interface 是每核一套（GICC_IAR/EOIR/CTLR/PMR 均 per-CPU），
/// SMP 下各核必须访问自己的接口，否则应答/EOI 写错核导致中断错乱。
fn gicc_base() -> usize {
    unsafe { GICC_BASE + crate::smp::cpu_id() as usize * 0x20000 }
}

/// 初始化 GICv2（CPU0 主核）。
///
/// * `base`     - GIC Distributor 基地址
/// * `cpu_base` - CPU0 的 GIC CPU Interface 基地址
///
/// 使能 Distributor 与当前核的 CPU Interface，并把优先级掩码设为 0xFF
/// （放行所有优先级）。
pub fn gic_init(base: usize, cpu_base: usize) {
    unsafe {
        GICD_BASE = base;
        GICC_BASE = cpu_base;

        // 使能 Distributor（GICD_CTLR.Enable）
        write_volatile((GICD_BASE + GICD_CTLR) as *mut u32, 1);
        // 使能 CPU Interface（GICC_CTLR.Enable）
        write_volatile((gicc_base() + GICC_CTLR) as *mut u32, 1);
        // 优先级掩码：0xFF 放行所有优先级
        write_volatile((gicc_base() + GICC_PMR) as *mut u32, 0xFF);
    }
}

/// 使能**当前核**的 GIC CPU Interface（secondary 核进入调度前调用）。
pub fn gic_init_secondary() {
    unsafe {
        // 使能 CPU Interface（GICC_CTLR.Enable）
        write_volatile((gicc_base() + GICC_CTLR) as *mut u32, 1);
        // 优先级掩码：0xFF 放行所有优先级
        write_volatile((gicc_base() + GICC_PMR) as *mut u32, 0xFF);
    }
}

/// 使能指定中断号（写入 GICD_ISENABLERn 对应位）。
pub fn gic_enable_irq(irq: u32) {
    assert!(irq < 1020, "gic：中断号超出范围");
    let reg = (irq / 32) as usize;
    let bit = irq % 32;
    unsafe {
        let addr = GICD_BASE + GICD_ISENABLER + reg * 4;
        write_volatile(addr as *mut u32, 1 << bit);
    }
}

/// 禁用指定中断号（写入 GICD_ICENABLERn 对应位）。
///
/// 供中断风暴护栏在冷路径屏蔽风暴源使用（每次风暴只写一次，写完即停）。
pub fn gic_disable_irq(irq: u32) {
    assert!(irq < 1020, "gic：中断号超出范围");
    let reg = (irq / 32) as usize;
    let bit = irq % 32;
    unsafe {
        let addr = GICD_BASE + GICD_ICENABLER + reg * 4;
        write_volatile(addr as *mut u32, 1 << bit);
    }
}

/// 设置中断优先级（GICD_IPRIORITYR 每字节对应一个中断，低值 = 高优先级）。
pub fn gic_set_priority(irq: u32, pri: u8) {
    // 越界防护：irq ≥ 1020 时 reg 超出 GICD 寄存器窗（可写穿 4KB bank）
    assert!(irq < 1020, "gic：中断号超出范围");
    let reg = (irq / 4) as usize;
    let shift = (irq % 4) * 8;
    unsafe {
        let addr = GICD_BASE + GICD_IPRIORITYR + reg * 4;
        let val = read_volatile(addr as *const u32);
        let val = (val & !(0xFF << shift)) | ((pri as u32) << shift);
        write_volatile(addr as *mut u32, val);
    }
}

/// 设置中断目标 CPU（GICD_ITARGETSR 每字节的每个位对应一个 CPU，仅对 SPI 有效）。
///
/// `cpu` 为位掩码：bit0 表示 CPU0，bit1 表示 CPU1，以此类推。
pub fn gic_set_target(irq: u32, cpu: u8) {
    // 越界防护：同 gic_set_priority
    assert!(irq < 1020, "gic：中断号超出范围");
    let reg = (irq / 4) as usize;
    let shift = (irq % 4) * 8;
    unsafe {
        let addr = GICD_BASE + GICD_ITARGETSR + reg * 4;
        let val = read_volatile(addr as *const u32);
        let val = (val & !(0xFF << shift)) | ((cpu as u32 & 0xFF) << shift);
        write_volatile(addr as *mut u32, val);
    }
}

/// 读取中断应答寄存器（IAR），返回原始值（低 10 位为中断号，bit[12:10] 为 CPUID）。
pub fn gic_read_iar() -> u32 {
    unsafe { read_volatile((gicc_base() + GICC_IAR) as *const u32) }
}

/// 写中断结束寄存器（EOIR），通知 GIC 中断处理完成。
///
/// 应传入 `gic_read_iar` 返回的原始值（含 CPUID 位）。
pub fn gic_write_eoir(id: u32) {
    unsafe { write_volatile((gicc_base() + GICC_EOIR) as *mut u32, id) }
}

/// 诊断读取：GICD `gicd_off` 与当前核 GICC `gicc_off` 偏移处的寄存器值。
///
/// 供中断风暴探测器转储 GIC 状态（走真实基地址，平台无关）。
pub fn gic_diag_read(gicd_off: usize, gicc_off: usize) -> (u32, u32) {
    unsafe {
        (
            read_volatile((GICD_BASE + gicd_off) as *const u32),
            read_volatile((gicc_base() + gicc_off) as *const u32),
        )
    }
}
