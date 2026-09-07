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
/// # 安全
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
/// # 安全
/// 需要 EL1 权限。
unsafe fn write_tcr() {
    let tcr: u64 = 0x3519 | (0b101 << 32);
    asm!("msr tcr_el1, {}", in(reg) tcr);
}

/// 写 TTBR0_EL1（内核页表基地址，必须 4KB 对齐）。
///
/// # 安全
/// 需要 EL1 权限；base 必须为 4KB 对齐的页表物理地址。
unsafe fn write_ttbr0_el1(base: u64) {
    asm!("msr ttbr0_el1, {}", in(reg) base);
}

/// 使能 MMU（SCTLR_EL1.M=1）并开启数据/指令缓存（C=1、I=1）。
///
/// # 安全
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

// ==================== PAN（Privileged Access Never） ====================

/// SCTLR_EL1.PAN（bit 22，ARMv8.1-A 引入）：置 1 后，EL1 访问任何
/// EL0 可访问的内存页（AP 含 EL0 权限的用户页）都会触发 permission fault。
///
/// 用途（对标 Linux `CONFIG_ARM64_PAN`）：内核代码只有在显式进入 uaccess
/// 区（[`uaccess_begin`]..[`uaccess_end`]）时才能读写用户内存，杜绝内核因
/// 漏洞/bug 直接越权读写用户数据（如「直改用户指针绕过 user_ok 校验」）。
const SCTLR_PAN: u64 = 1 << 22;

/// PAN 是否被硬件实现（探测结果）。ARMv8.0 核（如 Cortex-A72）不支持时
/// 为 `false`，uaccess 辅助退化为空操作，行为与未开 PAN 完全一致。
static mut PAN_SUPPORTED: bool = false;

/// PAN 是否可用（供诊断输出与调试断言）。
pub fn pan_supported() -> bool {
    unsafe { PAN_SUPPORTED }
}

/// 探测并启用 PAN：写 SCTLR_EL1.PAN=1 后读回，位能保留说明硬件实现。
///
/// 对 ARMv8.0 核该位为 RES0，写入被忽略、读回为 0 —— 探测天然安全，
/// 无需区分实现版本。
fn enable_pan_if_available() {
    unsafe {
        let mut s: u64;
        asm!("mrs {}, sctlr_el1", out(reg) s);
        s |= SCTLR_PAN;
        asm!("msr sctlr_el1, {}", in(reg) s);
        asm!("isb");
        let back: u64;
        asm!("mrs {}, sctlr_el1", out(reg) back);
        if back & SCTLR_PAN != 0 {
            PAN_SUPPORTED = true;
        }
    }
}

/// 进入 uaccess 区：清 SCTLR_EL1.PAN，允许 EL1 直接读写用户内存。
///
/// 必须与 [`uaccess_end`] 配对；区内不允许被抢占（调用方须处于 IRQ 屏蔽
/// 上下文——本内核系统调用/异常处理入口均已 `daifset`）。
///
/// # 安全
/// 调用方须已通过 `user_ok`/`ensure_user_range_*` 校验目标用户范围合法且
/// 已映射，否则区内访问未映射/只读用户页会触发 EL1 data abort。
#[inline(always)]
pub unsafe fn uaccess_begin() {
    if PAN_SUPPORTED {
        let mut s: u64;
        asm!("mrs {}, sctlr_el1", out(reg) s);
        asm!("msr sctlr_el1, {}", in(reg) s & !SCTLR_PAN);
        asm!("isb");
    }
}

/// 退出 uaccess 区：置回 SCTLR_EL1.PAN，恢复对用户内存的 EL1 访问隔离。
///
/// # 安全
/// 须与 [`uaccess_begin`] 配对。
#[inline(always)]
pub unsafe fn uaccess_end() {
    if PAN_SUPPORTED {
        let mut s: u64;
        asm!("mrs {}, sctlr_el1", out(reg) s);
        asm!("msr sctlr_el1, {}", in(reg) s | SCTLR_PAN);
        asm!("isb");
    }
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

        // 探测并启用 PAN（内存硬化；v8.0 核自动跳过）
        enable_pan_if_available();
    }
}
