//! MMU 初始化：配置系统寄存器、建立恒等映射并启用 Sv39 分页。
//!
//! 页表构造已下沉到 `crate::vm`（`vm::build_identity_map`），此处负责：
//!   1. 读取链接脚本段边界并调用 `vm` 构造恒等映射
//!   2. 写 satp（MODE=8 Sv39 + 根表 PPN）并 sfence.vma
//!   3. 打开 S 模式的 FPU（sstatus.FS）与 SUM/MXR（S 访问 U 页）
//!
//! aarch64 版本的 MAIR/TCR/SCTLR.PAN 在 Sv39 无对应物：内存类型由 PMA
//! 决定（无 AttrIndx）、单一 satp 替代 TTBR0、无特权访问隔离（PAN 由
//! SUM 位取反语义承担——内核需要直读用户内存，SUM 恒开，uaccess 辅助
//! 退化为空操作）。

use core::arch::asm;
use core::ptr::addr_of;

use crate::vm;

// ==================== 链接脚本导出的段边界 ====================

extern "C" {
    // 代码段起始（0x80200000）
    static _text_start: u8;
    // 数据段起始（.text + .rodata 之后，4KB 对齐）
    static _data_start: u8;
}

// ==================== sstatus 位 ====================

/// sstatus.FS（bits[14:13]）= 11（Dirty）：浮点单元可用的最大状态。
/// 复位后 FS=Off（任何 FP 指令触发 illegal instruction），内核与用户态
/// 都可能由 Rust 编译器生成浮点指令，必须在进入 Rust 前置位。
const SSTATUS_FS_DIRTY: u64 = 0b11 << 13;
/// sstatus.SUM：S 模式可访问 U 位页（内核直读用户内存，对应 PAN 的取反）。
const SSTATUS_SUM: u64 = 1 << 18;
/// sstatus.MXR：S 模式可读仅执行页（保护内核对 RX 段的读访问兜底）。
const SSTATUS_MXR: u64 = 1 << 19;

/// 写 satp（切换到内核根表）并全量 TLB 失效。
///
/// # 安全
/// 需要 S 模式权限；base 必须为已构造好的 4KB 对齐页表物理地址。
unsafe fn write_satp(base: u64) {
    asm!(
        "csrw satp, {0}",
        "sfence.vma",
        in(reg) vm::satp_value(base as usize),
    );
}

/// PAN 是否被硬件实现：Sv39 无对应机制，恒 false（uaccess 辅助空操作）。
pub fn pan_supported() -> bool {
    false
}

/// 进入 uaccess 区：Sv39 下 SUM/MXR 恒开，空操作（保留调用点兼容）。
#[inline(always)]
pub unsafe fn uaccess_begin() {}

/// 退出 uaccess 区：空操作（同上）。
#[inline(always)]
pub unsafe fn uaccess_end() {}

// ==================== 初始化 ====================

/// 初始化并启用 Sv39 分页。
///
/// 建立恒等映射（低 1GB 设备区 + RAM 0x80000000-0xC0000000），
/// 写 satp（Sv39 + 根表），开 FPU/SUM/MXR。
pub fn init() {
    unsafe {
        // 读取内核段边界（链接脚本导出）
        let text_start = addr_of!(_text_start) as usize; // 0x80200000
        let data_start = addr_of!(_data_start) as usize; // .text+.rodata 之后

        // 构造恒等映射页表（vm 模块）
        vm::build_identity_map(text_start, data_start);

        // 页表写入对页表遍历可见后切换地址空间
        asm!("fence rw, rw");
        write_satp(vm::kernel_root_addr() as u64);

        // TLB 维护：全量失效（satp 切换后清除可能的陈旧项）
        asm!("sfence.vma");

        // FPU（FS=Dirty）+ SUM/MXR：进入 Rust 主体前完成（_start 已置过
        // FS，这里幂等强化并补 SUM/MXR）
        enable_fp_and_sum();
    }
}

/// 置 sstatus 的 FS=Dirty 与 SUM/MXR 位。
///
/// # 安全
/// 需要 S 模式权限。
pub unsafe fn enable_fp_and_sum() {
    asm!(
        "li {t}, {fs}",
        "csrs sstatus, {t}",
        "li {t}, {sumx}",
        "csrs sstatus, {t}",
        t = out(reg) _,
        fs = const SSTATUS_FS_DIRTY,
        sumx = const SSTATUS_SUM | SSTATUS_MXR,
    );
}

/// secondary 核的每核地址翻译状态初始化（SMP，`smp_secondary_main` 调用）。
///
/// satp/sstatus 均为**每核** CSR：CPU0 在 [`init`] 配置，而 secondary 经
/// _start 停车场唤醒——不配置则本核首次切入用户任务（换 satp 到用户页表）
/// 时无 SUM/MXR/FS → 访问用户内存即 fault。恒等映射页表由 CPU0 构建完毕
/// （SCHED_READY 释放副核前）本核直接复用，无需重建。
pub unsafe fn init_secondary() {
    write_satp(vm::kernel_root_addr() as u64);
    asm!("sfence.vma");
    enable_fp_and_sum();
}
