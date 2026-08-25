//! 线程抽象：共享地址空间、TPIDR_EL0 线程本地存储、创建与退出。
//!
//! 线程是调度实体，属于某个进程（共享该进程的地址空间与 fd 表）。
//! 每个线程通过 `TPIDR_EL0` 保存自己的本地存储（TLS）基址；该寄存器在
//! 上下文切换时由 `switch_to` 一并保存/恢复，因此各线程的 TLS 互不干扰。

use core::arch::asm;

use crate::sched_cfs::{self, PriorityClass};
use crate::vm;

/// 创建线程（共享所属进程的地址空间与 fd 表），返回 tid。
///
/// `entry` 为线程入口（接收 `arg`，不得返回）。
#[allow(dead_code)]
pub fn spawn(
    pid: u32,
    name: &'static str,
    entry: fn(usize) -> !,
    arg: usize,
    class: PriorityClass,
    nice: i32,
) -> u32 {
    sched_cfs::spawn(pid, name, entry, arg, class, nice, vm::kernel_root_addr())
}

/// 当前线程 tid。
#[allow(dead_code)]
pub fn current_tid() -> u32 {
    sched_cfs::current_tid()
}

/// 退出当前线程（不再返回）。
#[allow(dead_code)]
pub fn exit() -> ! {
    sched_cfs::exit_current()
}

/// 主动让出 CPU。
pub fn yield_now() {
    sched_cfs::yield_now();
}

/// 写入线程本地存储基址到 `TPIDR_EL0`。
///
/// # Safety
/// 需要 EL1 权限；调用者自行保证地址有效性。
pub unsafe fn set_tp(value: u64) {
    asm!("msr tpidr_el0, {}", in(reg) value);
}

/// 读取当前线程本地存储基址 `TPIDR_EL0`。
///
/// # Safety
/// 需要 EL1 权限。
pub unsafe fn get_tp() -> u64 {
    let v: u64;
    asm!("mrs {}, tpidr_el0", out(reg) v);
    v
}

/// 设置当前线程本地存储基址（安全封装）。
pub fn set_thread_local(value: u64) {
    unsafe { set_tp(value) };
}

/// 读取当前线程本地存储基址（安全封装）。
pub fn get_thread_local() -> u64 {
    unsafe { get_tp() }
}
