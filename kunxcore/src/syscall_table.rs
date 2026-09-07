//! 系统调用陷阱帧与 EL0 `svc` 分发入口。
//!
//! `main.rs` 的 `lower_sync` 汇编异常入口在进入 EL1 时把完整寄存器现场保存到
//! 内核栈上的 [`TrapFrame`]，然后调用 [`syscall_dispatch`]。本模块只负责这个
//! 契约：解析 `x8`（系统调用号）并转发给 [`crate::linux_compat::dispatch`]，
//! 由翻译层完成 Linux 系统调用 → 内核服务的映射与语义差异处理（Phase 9）。
//!
//! 陷阱帧布局与 `main.rs` 汇编保存顺序严格对应：
//! `x[0..31]`（x0..x30）@0x000..0x0F8，`sp_el0`@0x0F8，`elr`@0x100，`spsr`@0x108。

use crate::linux_compat;

// ==================== 陷阱帧 ====================

/// 与 main.rs `lower_sync` 汇编保存布局严格对应：
/// `x[0..31]`（x0..x30）@0x000..0x0F8，`sp_el0`@0x0F8，`elr`@0x100，`spsr`@0x108。
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr: u64,
    pub spsr: u64,
}

// ==================== 系统调用分发 ====================

/// EL0 `svc` 分发入口（由 main.rs 汇编 `lower_sync` 调用）。
///
/// 入参指向栈上的 [`TrapFrame`]；返回前把结果写入 `frame.x[0]`。
#[no_mangle]
pub extern "C" fn syscall_dispatch(frame: *mut TrapFrame) {
    unsafe {
        let f = &mut *frame;
        let nr = f.x[8] as usize;
        // 记录分发前的 elr：若信号投递把陷阱帧改写为进入 handler（elr 变化），
        // 结果不得写回 x0（x0 此时是 handler 的 signum 参数，覆写会破坏信号语义）。
        let elr_before = f.elr;
        let ret = linux_compat::dispatch(nr, f);
        if f.elr == elr_before {
            f.x[0] = ret as u64;
        }
    }
}
