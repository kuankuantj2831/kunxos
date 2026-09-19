//! 系统调用陷阱帧与 U 模式 `ecall` 分发入口。
//!
//! `main.rs` 的 `trap_entry` 汇编在陷入时把完整寄存器现场保存到内核栈上的
//! [`TrapFrame`]，由 `interrupt.rs` 识别 ecall 后调用 [`syscall_dispatch`]。
//! 本模块只负责这个契约：解析 `a7`（系统调用号，x17）并转发给
//! [`crate::linux_compat::dispatch`]，由翻译层完成 Linux 系统调用 → 内核
//! 服务的映射与语义差异处理。
//!
//! 调用约定（Linux riscv64，与 asm-generic 系统调用号兼容 aarch64）：
//!   nr = a7（x17），参数 a0-a5（x10-x15），返回值 a0（x10）。
//!
//! 陷阱帧布局与 `main.rs` 汇编保存顺序严格对应：
//! `x[0..32]`（x0..x31，x0 恒 0 占位）@0x000..0x100，`sp_el0`（兼容占位，
//! RISC-V 用户 sp 即 x[2]）@0x100，`elr`（sepc）@0x108，`spsr`（sstatus）@0x110。

use crate::linux_compat;

// ==================== 陷阱帧 ====================

/// 与 main.rs `trap_entry` 汇编保存布局严格对应：
/// `x[0..32]` @0x000..0x100，`sp_el0`（占位）@0x100，`elr`（sepc）@0x108，
/// `spsr`（sstatus）@0x110；其后 0x120 起为 FP 现场（f0-f31 + fcsr）。
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 32],
    /// 兼容占位：RISC-V 用户栈指针即 `x[2]`，此槽保留（信号帧等不读）。
    pub sp_el0: u64,
    pub elr: u64,
    pub spsr: u64,
}

/// 系统调用参数寄存器 a0-a5 → 陷阱帧索引 x[10..16]。
pub const ARG_BASE: usize = 10;

/// 系统调用号寄存器 a7 → x[17]。
pub const NR_REG: usize = 17;

// ==================== 系统调用分发 ====================

/// U 模式 `ecall` 分发入口（由 `interrupt.rs` 调用）。
///
/// 入参指向栈上的 [`TrapFrame`]；返回前把结果写入 `x[10]`（a0）。
#[no_mangle]
pub extern "C" fn syscall_dispatch(frame: *mut TrapFrame) {
    unsafe {
        let f = &mut *frame;
        let nr = f.x[NR_REG] as usize;
        // 记录分发前的 elr：若信号投递把陷阱帧改写为进入 handler（elr 变化），
        // 结果不得写回 a0（a0 此时是 handler 的 signum 参数，覆写会破坏信号语义）。
        let elr_before = f.elr;
        let ret = linux_compat::dispatch(nr, f);
        if f.elr == elr_before {
            f.x[ARG_BASE] = ret as u64;
        }
    }
}
