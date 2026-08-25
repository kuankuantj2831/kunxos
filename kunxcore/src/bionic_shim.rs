//! bionic 运行时 shim（Phase 9）：为 Android bionic / glibc / musl 提供内核侧支撑。
//!
//! Linux 应用（busybox、SDL2、微信等）经 bionic 或 glibc 链接，其 libc 依赖的
//! 并非裸系统调用，而是一组「运行时语义」。本模块在系统调用翻译层之下，补齐这些
//! 语义，按 bionic 的三块核心划分：
//!
//! - **pthread**：`clone(CLONE_THREAD)` 共享地址空间线程、`futex` 等待/唤醒
//!   （互斥量/条件变量的基石）、`set_tid_address`、TLS（`TPIDR_EL0`）。
//! - **malloc**：`brk` 程序断点（堆）管理与 `mmap(MAP_ANONYMOUS)` 匿名映射。
//! - **stdio**：终端判断（`isatty`）、窗口大小（`TIOCGWINSZ`）与标准流初始化。
//!
//! 说明：本内核为单地址空间 EL0 模型（用户页表继承内核恒等映射），陷入 EL1 时
//! TTBR0_EL1 仍是用户页表，因此这里可经物理地址/用户地址直接读写用户内存。

use core::arch::{asm, global_asm};

use crate::allocator;
use crate::process;
use crate::sched_cfs;
use crate::vm;

/// 用户地址上界（与翻译层 `USER_TOP` 一致，此处独立声明避免模块耦合）。
/// 用户地址空间整体位于 0x0000_0000-0x3FFF_FFFF，内核恒等映射从 0x4000_0000 开始。
const USER_TOP: usize = 0x4000_0000;

// ==================== pthread：futex ====================

/// futex 等待项容量。
const MAX_FUTEX_WAITERS: usize = 32;

#[derive(Clone, Copy)]
struct FutexWaiter {
    used: bool,
    /// 等待的用户态 futex 地址
    addr: usize,
    /// 等待线程 tid
    tid: u32,
}

const EMPTY_WAITER: FutexWaiter = FutexWaiter { used: false, addr: 0, tid: 0 };

static mut FUTEX_WAITERS: [FutexWaiter; MAX_FUTEX_WAITERS] = [EMPTY_WAITER; MAX_FUTEX_WAITERS];

/// 复位 futex 等待表（幂等，供初始化调用）。
pub fn init() {
    unsafe {
        for w in FUTEX_WAITERS.iter_mut() {
            *w = EMPTY_WAITER;
        }
    }
}

/// 安全地读取用户态 `u32`（边界 + 映射校验），非法地址返回 `None`。
///
/// 仅范围校验不够：用户地址空间稀疏，未映射地址在 EL1 解引用会触发
/// data abort 挂死内核（用户态 DoS），因此必须确认页已映射。按需分页的
/// LAZY 预留页先落地再读取。
unsafe fn peek_u32(addr: usize) -> Option<u32> {
    if addr == 0 || addr >= USER_TOP || addr & 0x3 != 0 {
        return None;
    }
    if !process::ensure_user_range_mapped(addr, 4) {
        return None;
    }
    Some(core::ptr::read_volatile(addr as *const u32))
}

/// FUTEX_WAIT：若 `*addr == val` 则阻塞，直到被 `futex_wake` 唤醒。
///
/// 返回 0（被唤醒）或 -EAGAIN（值不匹配/无等待槽位，等价 Linux `EAGAIN`）。
pub fn futex_wait(addr: usize, val: u32) -> i32 {
    // 首次检查：值已变化则立即返回（避免丢唤醒竞态）
    if unsafe { peek_u32(addr) } != Some(val) {
        return -11; // EAGAIN
    }

    let tid = sched_cfs::current_tid();
    unsafe {
        // 登记等待项
        let mut slot = None;
        for i in 0..MAX_FUTEX_WAITERS {
            if !FUTEX_WAITERS[i].used {
                slot = Some(i);
                break;
            }
        }
        let Some(i) = slot else { return -11 }; // EAGAIN：无空位
        FUTEX_WAITERS[i] = FutexWaiter { used: true, addr, tid };

        // 二次检查（登记后、阻塞前，消除竞态窗口）
        if peek_u32(addr) != Some(val) {
            FUTEX_WAITERS[i] = EMPTY_WAITER;
            return -11; // EAGAIN
        }
    }
    // 阻塞，直到被 futex_wake 唤醒
    sched_cfs::block_current();
    0
}

/// FUTEX_WAKE：唤醒至多 `n` 个等待在 `addr` 上的线程，返回唤醒数量。
pub fn futex_wake(addr: usize, n: u32) -> i32 {
    let mut woken = 0u32;
    unsafe {
        for i in 0..MAX_FUTEX_WAITERS {
            if woken >= n {
                break;
            }
            let w = FUTEX_WAITERS[i];
            if w.used && w.addr == addr {
                FUTEX_WAITERS[i] = EMPTY_WAITER;
                sched_cfs::wake(w.tid);
                woken += 1;
            }
        }
    }
    woken as i32
}

/// FUTEX_CMP_REQUEUE 的简化实现：唤醒 `n` 个，其余等待者迁移到 `addr2`。
#[allow(dead_code)]
pub fn futex_requeue(addr: usize, n: u32, addr2: usize) -> i32 {
    let mut woken = 0u32;
    unsafe {
        for i in 0..MAX_FUTEX_WAITERS {
            let w = FUTEX_WAITERS[i];
            if w.used && w.addr == addr {
                if woken < n {
                    FUTEX_WAITERS[i] = EMPTY_WAITER;
                    sched_cfs::wake(w.tid);
                    woken += 1;
                } else {
                    FUTEX_WAITERS[i].addr = addr2;
                }
            }
        }
    }
    woken as i32
}

// ==================== pthread：clone(CLONE_THREAD) ====================

extern "C" {
    /// clone 内核主体：父进程返回子 tid，子进程返回 0。
    fn do_clone(
        trap_frame: usize,
        child_stack: usize,
        tls: usize,
        parent_tidptr: usize,
        child_tidptr: usize,
    ) -> i32;
    /// 子线程首次调度入口：sp 指向子线程拷贝的陷阱帧，恢复现场并 eret 回用户态。
    fn syscall_return_restore();
    /// 子线程首次调度入口（先释放调度锁再进 syscall_return_restore）。
    fn el0_resume_entry();
}

global_asm!(
    r#"
    // clone(CLONE_THREAD) 内核主体。
    // 入参：x0 = trap_frame, x1 = child_stack, x2 = tls, x3 = parent_tidptr, x4 = child_tidptr
    .section .text.do_clone, "ax"
    .global do_clone
    .type do_clone, %function
do_clone:
    // 保存 callee-saved 寄存器（x19-x30）作为子线程上下文
    sub sp, sp, #96
    stp x19, x20, [sp, #0x00]
    stp x21, x22, [sp, #0x10]
    stp x23, x24, [sp, #0x20]
    stp x25, x26, [sp, #0x30]
    stp x27, x28, [sp, #0x40]
    stp x29, x30, [sp, #0x50]
    // 暂存入参到调用者保存寄存器
    mov x6, x0
    mov x7, x1
    mov x8, x2
    mov x9, x3
    mov x10, x4
    // clone_parent(saved_sp, trap_frame, child_stack, tls, parent_tidptr, child_tidptr)
    mov x0, sp
    mov x1, x6
    mov x2, x7
    mov x3, x8
    mov x4, x9
    mov x5, x10
    bl clone_parent
    // 父线程路径：恢复现场并返回子 tid
    mov x9, x0
    ldp x19, x20, [sp, #0x00]
    ldp x21, x22, [sp, #0x10]
    ldp x23, x24, [sp, #0x20]
    ldp x25, x26, [sp, #0x30]
    ldp x27, x28, [sp, #0x40]
    ldp x29, x30, [sp, #0x50]
    add sp, sp, #96
    mov x0, x9
    ret
"#
);

/// clone 的 Rust 侧主体（由汇编 `do_clone` 调用）。
///
/// `saved_sp` 为 do_clone 保存 callee-saved 寄存器处的栈地址；`trap_frame`
/// 为系统调用陷阱帧地址（子线程 eret 时需恢复的新用户栈/TLS 就改在这里）。
#[no_mangle]
extern "C" fn clone_parent(
    saved_sp: usize,
    trap_frame: usize,
    child_stack: usize,
    tls: usize,
    parent_tidptr: usize,
    child_tidptr: usize,
) -> u32 {
    // 屏蔽 IRQ，保护栈拷贝与任务表写入的临界区
    unsafe { asm!("msr daifset, #2") };

    let (stack_base, stack_top) = sched_cfs::current_stack();
    let stack_size = stack_top - stack_base;

    // 拷贝父栈活动区 [saved_sp, stack_top) 到子栈顶部对应位置（与 fork 一致）：
    // 子线程内核栈布局与父线程完全一致，拷贝区之下仍有 ~256KB 可用栈。
    let child_stack_top = sched_cfs::alloc_stack();
    let child_stack_base = child_stack_top - stack_size;
    let copy_len = stack_top - saved_sp;
    let child_sp = child_stack_top - copy_len;
    unsafe {
        core::ptr::copy_nonoverlapping(saved_sp as *const u8, child_sp as *mut u8, copy_len);
    }

    // 子线程陷阱帧放在**子栈顶部**（child_stack_top - 272，同 fork）：
    // 首次 eret 后内核 sp = child_stack_top，后续系统调用帧从栈顶向下分配，
    // 与拷贝区（exec 残留）隔离。修改：新用户栈（sp_el0 字段 @0xF8）与
    // 返回值 x[0]=0。
    const TRAP_FRAME_SIZE: usize = 800;
    let child_frame = child_stack_top - TRAP_FRAME_SIZE;
    if trap_frame >= saved_sp && trap_frame + TRAP_FRAME_SIZE <= stack_top {
        unsafe {
            core::ptr::copy_nonoverlapping(
                trap_frame as *const u8,
                child_frame as *mut u8,
                TRAP_FRAME_SIZE,
            );
            *((child_frame + 0xF8) as *mut u64) = child_stack as u64;
            *(child_frame as *mut u64) = 0;
        }
    } else {
        unsafe { asm!("msr daifclr, #2") };
        return u32::MAX;
    }

    // 子线程上下文：首次调度直接进入 syscall_return_restore，sp 指向子线程
    // **拷贝的陷阱帧**（x0 已置 0、sp_el0 已改为新用户栈，elr/spsr 均为
    // 子线程自身副本值）。不经过 syscall_dispatch（无跨栈写、不回卷），
    // eret 直接回到用户态 clone() 调用点，x0 = 0（子线程语义）。
    let mut ctx = sched_cfs::TaskContext::empty();
    ctx.x30 = el0_resume_entry as *const () as usize as u64;
    ctx.sp = child_frame as u64;
    ctx.spsr = 0x5;
    // 初始 ELR 取陷阱帧副本中的 EL0 返回地址（防「ELR=0 在飞」窗口）
    ctx.elr = unsafe { *((child_frame + 0x100) as *const u64) };
    ctx.tpidr = tls as u64;
    // CLONE_THREAD：共享地址空间，TTBR0 与父线程一致
    ctx.ttbr0_el1 = process::current_root() as u64;

    let class = sched_cfs::current_class();
    let nice = sched_cfs::current_nice();
    // CLONE_THREAD：共享地址空间与 fd 表 —— 使用与父线程相同的 pid
    let tid = sched_cfs::insert("clone-child", sched_cfs::current_pid(), ctx, child_stack_base, child_stack_top, class, nice);
    if tid == u32::MAX {
        // 任务槽位不足：回收子栈，返回失败
        let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
        for p in 0..pages {
            crate::allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
        }
        unsafe { asm!("msr daifclr, #2") };
        return u32::MAX;
    }

    // 写回 tid（CLONE_PARENT_SETTID / CLONE_CHILD_CLEARTID）
    // 目标指针必须已映射且可写（未映射地址写入会触发 EL1 data abort 挂死
    // 内核；COW 共享页须先 break 出私有副本，避免污染其它进程）；
    // 按需分页的 LAZY 页先落地再写入。
    unsafe {
        let tid_ok = |p: usize| {
            p != 0
                && p % 4 == 0
                && p.checked_add(4).map_or(false, |e| e <= USER_TOP)
                && process::ensure_user_range_writable(p, 4)
        };
        if tid_ok(parent_tidptr) {
            core::ptr::write_volatile(parent_tidptr as *mut u32, tid);
        }
        if tid_ok(child_tidptr) {
            core::ptr::write_volatile(child_tidptr as *mut u32, tid);
        }
    }

    unsafe { asm!("msr daifclr, #2") };
    tid
}

/// `clone(CLONE_THREAD)` 入口：返回子线程 tid（父）/ 0（子）。
///
/// `trap_frame` 为系统调用陷阱帧；`child_stack` 为新线程用户栈；`tls` 为新线程
/// TLS 基址（写入 `TPIDR_EL0`）。
pub fn clone_thread(
    trap_frame: usize,
    child_stack: usize,
    tls: usize,
    parent_tidptr: usize,
    child_tidptr: usize,
) -> i32 {
    let tid = unsafe { do_clone(trap_frame, child_stack, tls, parent_tidptr, child_tidptr) };
    core::hint::black_box(tid)
}

/// `set_tid_address`：返回当前线程 tid（clear_child_tid 指针暂仅记录）。
pub fn set_tid_address(_clear_child_tid: usize) -> i32 {
    sched_cfs::current_tid() as i32
}

// ==================== malloc：brk / mmap 匿名映射 ====================

/// 程序断点与 mmap 游标按进程分别维护（此前为全局单值：进程 A 首次 brk 后，
/// 进程 B 的 brk(0)/mmap 会从 A 的断点/游标继续，导致 B 的堆与 B 自身代码/
/// 栈重叠或越界）。索引为进程 pid。
static mut BRK_CUR: [usize; process::MAX_PROC] = [0; process::MAX_PROC];
static mut MMAP_NEXT: [usize; process::MAX_PROC] =
    [0x3000_0000; process::MAX_PROC];

fn cur_pid() -> usize {
    let p = process::current_pid() as usize;
    if p < process::MAX_PROC {
        p
    } else {
        0
    }
}

fn brk_init() {
    unsafe {
        let pid = cur_pid();
        if BRK_CUR[pid] == 0 {
            BRK_CUR[pid] = crate::elf::current_brk();
        }
    }
}

/// `brk(0)`：返回当前程序断点。
pub fn brk_get() -> usize {
    brk_init();
    unsafe { BRK_CUR[cur_pid()] }
}

/// `brk(new)`：设置程序断点，按需映射/取消映射堆页，返回新断点。
pub fn brk_set(new_brk: usize) -> usize {
    brk_init();
    let pid = cur_pid();
    let old = unsafe { BRK_CUR[pid] };
    if new_brk == 0 {
        return old;
    }
    // 拒绝映射进内核恒等映射区（安全关键）
    if new_brk >= vm::USER_VA_TOP {
        return old;
    }

    if new_brk < old {
        // 收缩：取消映射 [align_up(new), old) 之间的页，并归还物理页、
        // 移除地址空间记录（此前只 unmap 不释放，反复 shrink/grow 会泄漏页）
        let start = (new_brk + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1);
        if start < old {
            process::unmap_user_range(start, old - start);
        }
        unsafe { BRK_CUR[pid] = new_brk };
        return new_brk;
    }

    if new_brk > old {
        // 增长：为 [align_up(old), new) 分配并映射新页
        let start = (old + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1);
        let mut va = start;
        let mut mapped = 0usize;
        while va < new_brk {
            let pa = match allocator::alloc_page() {
                Some(p) => p,
                None => {
                    // 内存不足：回滚本次新映射的页并保持原断点
                    process::rollback_user_pages(start, mapped);
                    return old;
                }
            };
            unsafe { core::ptr::write_bytes(pa as *mut u8, 0, vm::PAGE_SIZE) };
            // 记录表满：回滚本次新映射并释放本页，保持原断点
            if !process::map_user_page_owned(va, pa) {
                process::rollback_user_pages(start, mapped);
                allocator::free_page(pa);
                return old;
            }
            va += vm::PAGE_SIZE;
            mapped += 1;
        }
        unsafe { BRK_CUR[pid] = new_brk };
    }
    new_brk
}

/// 分配一块匿名内存（`mmap` 无 fd 分支），返回映射起始虚拟地址；失败 -1。
///
/// 按需分页：仅预留地址空间（记录 `LAZY_PAGE`），不分配物理页；首次访问时
/// 由内核 page fault 处理器落地。`brk` 堆仍为立即分配（堆小、malloc 立即写）。
pub fn mmap_anon(len: usize) -> i64 {
    if len == 0 {
        return -1;
    }
    let pages = len.div_ceil(vm::PAGE_SIZE);
    let size = pages.checked_mul(vm::PAGE_SIZE).unwrap_or(usize::MAX);
    let pid = cur_pid();
    unsafe {
        let va = MMAP_NEXT[pid];
        // 拒绝越界/回绕：映射范围必须完整落在用户地址空间内
        if !vm::user_va_range_ok(va, size) {
            return -1;
        }
        MMAP_NEXT[pid] += size;
        let mut reserved = 0usize;
        for i in 0..pages {
            if !process::reserve_user_page(va + i * vm::PAGE_SIZE) {
                // 记录表满：回滚已预留页，避免泄漏
                process::rollback_user_pages(va, reserved);
                return -1;
            }
            reserved += 1;
        }
        va as i64
    }
}

// ==================== stdio ====================

/// 判断 fd 是否指向终端（字符设备 /dev/tty）。
pub fn isatty(fd: i32) -> bool {
    matches!(crate::vfs::fstat(fd), Some((t, _)) if t == crate::fs_simple::T_CHR)
}

/// 返回终端窗口大小 (rows, cols, xpixel, ypixel)（固定 24×80）。
pub fn tty_winsize() -> [u16; 4] {
    [24, 80, 0, 0]
}
