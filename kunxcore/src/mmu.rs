//! MMU 初始化：配置系统寄存器、建立恒等映射并启用 MMU。
//!
//! 页表构造已下沉到 `crate::vm`（`vm::build_identity_map`），此处负责：
//!   1. 读取链接脚本段边界并调用 `vm` 构造恒等映射
//!   2. 配置 MAIR_EL1 / TCR_EL1 / TTBR0_EL1
//!   3. 启用 MMU 与缓存，并做 TLB 维护

use core::arch::asm;
use core::ptr::addr_of;

use crate::vm;

// ==================== 链接脚本导出的段边界 ====================

extern "C" {
    // 代码段起始（0x40080000）
    static _text_start: u8;
    // 数据段起始（.text + .rodata 之后，4KB 对齐）
    static _data_start: u8;
}

// ==================== 系统寄存器写 ====================

/// 写 MAIR_EL1：attr0=普通内存（写回），attr1=设备内存（nGnRnE）。
///
/// # Safety
/// 需要 EL1 权限。
unsafe fn write_mair() {
    let mair: u64 = 0x0000_0000_0000_00FF;
    asm!("msr mair_el1, {}", in(reg) mair);
}

/// 写 TCR_EL1：T0SZ=25（39 位 VA，根表 L1）、TG0=4KB、inner shareable、写回缓存。
///
/// PS=40 位物理地址（0b101 << 32）：QEMU 11.x 的 PCIe ECAM 位于
/// 0x4010_0000_00（40 位地址），32 位 PA（PS=0）会在 L1 块翻译时报
/// address size fault。
///
/// # Safety
/// 需要 EL1 权限。
unsafe fn write_tcr() {
    let tcr: u64 = 0x3519 | (0b101 << 32);
    asm!("msr tcr_el1, {}", in(reg) tcr);
}

/// 写 TTBR0_EL1（内核页表基地址，必须 4KB 对齐）。
///
/// # Safety
/// 需要 EL1 权限；base 必须为 4KB 对齐的页表物理地址。
unsafe fn write_ttbr0_el1(base: u64) {
    asm!("msr ttbr0_el1, {}", in(reg) base);
}

/// 使能 MMU（SCTLR_EL1.M=1）并开启数据/指令缓存（C=1、I=1）。
///
/// # Safety
/// 调用前必须已正确配置 MAIR/TCR/TTBR0 与页表，否则会立即触发异常。
unsafe fn enable_mmu() {
    asm!(
        "mrs {s}, sctlr_el1",
        "orr {s}, {s}, #0x1",     // M：使能 MMU
        "orr {s}, {s}, #0x4",     // C：使能数据缓存
        "orr {s}, {s}, #0x1000",  // I：使能指令缓存
        "msr sctlr_el1, {s}",
        "isb",
        s = out(reg) _,
    );
}

// ==================== 初始化 ====================

/// 初始化并启用 MMU。
///
/// 建立 1GB 恒等映射（RAM 0x40000000-0x80000000）+ 设备区映射，
/// 配置 MAIR/TCR/TTBR0_EL1，启用 MMU 并做 TLB 维护。
pub fn init() {
    unsafe {
        // 读取内核段边界（链接脚本导出）
        let text_start = addr_of!(_text_start) as usize; // 0x40080000
        let data_start = addr_of!(_data_start) as usize; // .text+.rodata 之后

        // 构造恒等映射页表（vm 模块）
        vm::build_identity_map(text_start, data_start);

        // 配置系统寄存器
        write_mair();
        write_tcr();
        write_ttbr0_el1(vm::kernel_root_addr() as u64);

        // 内存屏障：确保页表写入对 MMU 可见
        asm!("dsb ishst");

        // 启用 MMU + 缓存
        enable_mmu();

        // TLB 维护：使整个 EL1 TLB 失效
        asm!("tlbi vmalle1");
        asm!("dsb ish");
        asm!("isb");
    }
}
