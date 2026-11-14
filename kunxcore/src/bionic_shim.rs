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
    /// 等待进程 pid（futex 按用户 VA 匹配，不同进程地址空间独立——同 VA
    /// 在两个进程里是不同变量，唤醒/迁移必须限定本进程，防跨进程干扰锁）
    pid: u32,
    /// 超时截止点（单调时钟 µs；None = 不限时等待）
    deadline: Option<u64>,
}

const EMPTY_WAITER: FutexWaiter = FutexWaiter { used: false, addr: 0, tid: 0, pid: 0, deadline: None };

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
    // PAN 下 EL1 读用户 futex 字需临时放行（无 PAN 时为空操作）
    crate::mmu::uaccess_begin();
    let v = core::ptr::read_volatile(addr as *const u32);
    crate::mmu::uaccess_end();
    Some(v)
}

/// FUTEX_WAIT 返回值：值不匹配 / 无等待槽位（等价 Linux `EAGAIN`）。
const EAGAIN: i32 = -11;
/// FUTEX_WAIT 返回值：等待超时。
const ETIMEDOUT: i32 = -110;

/// FUTEX_WAIT：若 `*addr == val` 则阻塞，直到被 `futex_wake` 唤醒或超时。
///
/// `deadline` 为单调时钟超时截止点（µs），`None` = 不限时等待。
/// 返回 0（被唤醒）或 -EAGAIN（值不匹配/无等待槽位）、-ETIMEDOUT（超时）。
///
/// 登记 → 二次检查 → 阻塞全程处于 IRQ 屏蔽窗口（`read_daif`/`write_daif`
/// 保存-恢复）：定时器/其他任务不会在「登记后、阻塞前」插入，futex_wake
/// 见到的等待者必然已 `Blocked` —— 消除「wake 消费槽位但等待者仍在就绪态
/// → 永久挂起」的丢唤醒窗口（与 ipc.rs 的 `wait_reg` + `block_current` 同构）。
pub fn futex_wait(addr: usize, val: u32, deadline: Option<u64>) -> i32 {
    // 首次检查：值已变化则立即返回（避免丢唤醒竞态）
    if unsafe { peek_u32(addr) } != Some(val) {
        return EAGAIN;
    }

    let tid = sched_cfs::current_tid();
    let pid = sched_cfs::current_pid();
    let saved = unsafe { sched_cfs::read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    // 登记等待项（IRQ 屏蔽下进行）
    let mut slot = None;
    unsafe {
        for i in 0..MAX_FUTEX_WAITERS {
            if !FUTEX_WAITERS[i].used {
                slot = Some(i);
                break;
            }
        }
    }
    let Some(i) = slot else {
        unsafe { sched_cfs::write_daif(saved) };
        return EAGAIN; // 无空位
    };
    unsafe {
        FUTEX_WAITERS[i] = FutexWaiter { used: true, addr, tid, pid, deadline };
    }
    // 二次检查（登记后、阻塞前，同处 IRQ 屏蔽窗口，消除竞态）
    if unsafe { peek_u32(addr) } != Some(val) {
        unsafe {
            FUTEX_WAITERS[i] = EMPTY_WAITER;
            sched_cfs::write_daif(saved);
        }
        return EAGAIN;
    }
    // 阻塞（IRQ 屏蔽下进入，切换原子完成）：设置超时则走定时睡眠
    // （`sleep_until` 内部登记 SLEEPERS，到期由 tick 唤醒），否则纯阻塞。
    match deadline {
        Some(d) => sched_cfs::sleep_until(d),
        None => sched_cfs::block_current(),
    }
    // 被唤醒后：futex_wake 已消费槽位 → 返回 0；槽位仍在 → 超时/提前醒来，
    // 自清槽位并返回 -ETIMEDOUT（等价 Linux 超时语义）。
    unsafe {
        if FUTEX_WAITERS[i].used
            && FUTEX_WAITERS[i].tid == tid
            && FUTEX_WAITERS[i].addr == addr
        {
            FUTEX_WAITERS[i] = EMPTY_WAITER;
            ETIMEDOUT
        } else {
            0
        }
    }
}

/// FUTEX_WAKE：唤醒至多 `n` 个等待在 `addr` 上的线程，返回唤醒数量。
///
/// 仅当目标确实处于 `Blocked` 才清槽并唤醒：等待者尚未实际阻塞（登记 →
/// 阻塞窗口，仅 SMP 抢占下可见；单核下 `futex_wait` 的屏蔽窗口已排除）
/// 时不消费槽位，避免「醒了就绪态任务却把槽位丢给下一次错误唤醒」。
pub fn futex_wake(addr: usize, n: u32) -> i32 {
    let mut woken = 0u32;
    // 仅匹配本进程的等待者：不同进程地址空间独立，同 VA 是不同变量
    let cur_pid = sched_cfs::current_pid();
    // 表操作全程屏蔽 IRQ：futex_wait 在屏蔽窗口内写表，此处（及 requeue/
    // cleanup）若在 IRQ 使能下读写，tick 抢占会撕裂「查表 → 清槽 → 唤醒」
    let saved = unsafe { sched_cfs::read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        for i in 0..MAX_FUTEX_WAITERS {
            if woken >= n {
                break;
            }
            let w = FUTEX_WAITERS[i];
            if w.used && w.addr == addr && w.pid == cur_pid {
                if sched_cfs::is_blocked(w.tid) {
                    FUTEX_WAITERS[i] = EMPTY_WAITER;
                    sched_cfs::wake(w.tid);
                    woken += 1;
                }
            }
        }
    }
    unsafe { sched_cfs::write_daif(saved) };
    woken as i32
}

/// 线程退出/被杀时清掉其全部 futex 等待项：否则 32 个槽位逐渐被死亡线程
/// 占满，所有 `futex_wait` 将返回 EAGAIN（等待能力耗尽）。
///
/// 由 `sched_cfs::kill_pid`（进程退出/终止统一清理路径）调用。
pub fn futex_cleanup_tid(tid: u32) {
    let saved = unsafe { sched_cfs::read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        for w in FUTEX_WAITERS.iter_mut() {
            if w.used && w.tid == tid {
                *w = EMPTY_WAITER;
            }
        }
    }
    unsafe { sched_cfs::write_daif(saved) };
}

/// FUTEX_CMP_REQUEUE 的简化实现：唤醒 `n` 个，其余等待者迁移到 `addr2`。
#[allow(dead_code)]
pub fn futex_requeue(addr: usize, n: u32, addr2: usize) -> i32 {
    let mut woken = 0u32;
    let cur_pid = sched_cfs::current_pid();
    let saved = unsafe { sched_cfs::read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    unsafe {
        for i in 0..MAX_FUTEX_WAITERS {
            let w = FUTEX_WAITERS[i];
            if w.used && w.addr == addr && w.pid == cur_pid {
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
    unsafe { sched_cfs::write_daif(saved) };
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
    // 入参：a0 = trap_frame, a1 = child_stack, a2 = tls, a3 = parent_tidptr, a4 = child_tidptr
    .section .text.do_clone, "ax"
    .global do_clone
    .type do_clone, %function
do_clone:
    // 保存 callee-saved 寄存器（ra + s0-s11）作为现场锚点
    addi sp, sp, -112
    sd ra, 0x00(sp)
    sd s0, 0x08(sp)
    sd s1, 0x10(sp)
    sd s2, 0x18(sp)
    sd s3, 0x20(sp)
    sd s4, 0x28(sp)
    sd s5, 0x30(sp)
    sd s6, 0x38(sp)
    sd s7, 0x40(sp)
    sd s8, 0x48(sp)
    sd s9, 0x50(sp)
    sd s10, 0x58(sp)
    sd s11, 0x60(sp)
    // 暂存入参：t3-t6 + 帧尾空槽 0x68（13 个 callee-saved 只占 0x00..0x67）
    mv t3, a0        # trap_frame
    mv t4, a1        # child_stack
    mv t5, a2        # tls
    mv t6, a3        # parent_tidptr
    sd a4, 0x68(sp)  # child_tidptr
    // clone_parent(saved_sp, trap_frame, child_stack, tls, parent_tidptr, child_tidptr)
    mv a0, sp
    mv a1, t3
    mv a2, t4
    mv a3, t5
    mv a4, t6
    ld a5, 0x68(sp)
    call clone_parent
    // 父线程路径：恢复现场并返回子 tid
    mv t3, a0
    ld ra, 0x00(sp)
    ld s0, 0x08(sp)
    ld s1, 0x10(sp)
    ld s2, 0x18(sp)
    ld s3, 0x20(sp)
    ld s4, 0x28(sp)
    ld s5, 0x30(sp)
    ld s6, 0x38(sp)
    ld s7, 0x40(sp)
    ld s8, 0x48(sp)
    ld s9, 0x50(sp)
    ld s10, 0x58(sp)
    ld s11, 0x60(sp)
    addi sp, sp, 112
    mv a0, t3
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
    unsafe { asm!("csrci sstatus, 2") };

    let (stack_base, stack_top) = sched_cfs::current_stack();
    let stack_size = stack_top - stack_base;

    // 拷贝父栈活动区 [saved_sp, stack_top) 到子栈顶部对应位置（与 fork 一致）：
    // 子线程内核栈布局与父线程完全一致，拷贝区之下仍有 ~256KB 可用栈。
    // 子栈分配失败（OOM）：clone 明确失败（此前 panic，clone 风暴可打挂内核）。
    let Some(child_stack_top) = sched_cfs::alloc_stack() else {
        unsafe { asm!("csrsi sstatus, 2") };
        crate::uart_send_str("[clone] 子线程栈分配失败 (OOM)\n");
        return u32::MAX;
    };
    let child_stack_base = child_stack_top - stack_size;
    let copy_len = stack_top - saved_sp;
    let child_sp = child_stack_top - copy_len;
    unsafe {
        core::ptr::copy_nonoverlapping(saved_sp as *const u8, child_sp as *mut u8, copy_len);
    }

    // 子线程陷阱帧放在**子栈顶部**（child_stack_top - 832，同 fork）：
    // 首次 sret 后内核 sp = child_stack_top，后续系统调用帧从栈顶向下分配，
    // 与拷贝区（exec 残留）隔离。修改：新用户栈（x2 与 sp_el0 槽 @0x100）与
    // 返回值 a0（x[10] @0x50）=0。
    const TRAP_FRAME_SIZE: usize = 832;
    let child_frame = child_stack_top - TRAP_FRAME_SIZE;
    if trap_frame >= saved_sp && trap_frame + TRAP_FRAME_SIZE <= stack_top {
        unsafe {
            core::ptr::copy_nonoverlapping(
                trap_frame as *const u8,
                child_frame as *mut u8,
                TRAP_FRAME_SIZE,
            );
            *((child_frame + 0x10) as *mut u64) = child_stack as u64; // x2 槽
            *((child_frame + 0x100) as *mut u64) = child_stack as u64; // sp_el0 槽
            *((child_frame + 0x50) as *mut u64) = 0; // a0 槽：子线程返回 0
        }
    } else {
        // 陷阱帧不在父栈活动区：归还已分配的子栈（此前该路径漏释放，
        // 反复失败 clone 按 256KB/次泄漏内核物理页）。
        let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
        unsafe {
            for p in 0..pages {
                crate::allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        unsafe { asm!("csrsi sstatus, 2") };
        return u32::MAX;
    }

    // 子线程上下文：首次调度直接进入 trap_return，sp 指向子线程**拷贝的
    // 陷阱帧**（a0 已置 0、sp_el0 已改为新用户栈，elr/sstatus 均为子线程
    // 自身副本值）。不经过 syscall_dispatch（无跨栈写、不回卷），sret 直接
    // 回到用户态 clone() 调用点，a0 = 0（子线程语义）。
    // RISC-V 无用户 TLS 寄存器编程（tp 为内核 hartid），tls 参数仅存档。
    let _ = tls;
    let mut ctx = sched_cfs::TaskContext::empty();
    ctx.ra = el0_resume_entry as *const () as usize as u64;
    ctx.sp = child_frame as u64;
    // CLONE_THREAD：共享地址空间，satp_root 与父线程一致
    ctx.satp_root = process::current_root() as u64;

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
        unsafe { asm!("csrsi sstatus, 2") };
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
        // PAN 下写用户 tid 槽位需临时放行（无 PAN 时为空操作）
        crate::mmu::uaccess_begin();
        if tid_ok(parent_tidptr) {
            core::ptr::write_volatile(parent_tidptr as *mut u32, tid);
        }
        if tid_ok(child_tidptr) {
            core::ptr::write_volatile(child_tidptr as *mut u32, tid);
        }
        crate::mmu::uaccess_end();
    }

    unsafe { asm!("csrsi sstatus, 2") };
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
    if tid > 0 {
        // 记录子线程的 CLONE_CHILD_CLEARTID 地址（musl 新线程传
        // &__thread_list_lock——线程退出时内核清零并 futex 唤醒，
        // 以释放线程列表锁；pthread_join/__tl_lock 都依赖此语义）
        let t = tid as usize;
        unsafe {
            if t < CLEAR_TID.len() {
                CLEAR_TID[t] = child_tidptr;
            }
        }
    }
    core::hint::black_box(tid)
}

/// 各 tid 的 clear_child_tid 槽位（CLONE_CHILD_CLEARTID 语义：线程退出时
/// 内核向该用户地址写 0 并 futex 唤醒等待者——musl 的 pthread_join 依赖）。
static mut CLEAR_TID: [usize; 1024] = [0; 1024];

/// `set_tid_address`：记录 clear_child_tid 并返回当前线程 tid。
pub fn set_tid_address(clear_child_tid: usize) -> i32 {
    let tid = sched_cfs::current_tid() as usize;
    unsafe {
        if tid < CLEAR_TID.len() {
            CLEAR_TID[tid] = clear_child_tid;
        }
    }
    tid as i32
}

/// 任务死亡/槽位回收时清除其 CLEAR_TID 登记。
///
/// 经 kill_req / 进程整体 exit 死亡的线程不经过 `exit_thread_notify`，
/// 槽位残留旧用户地址；tid 即任务槽位下标、会被立刻复用 —— 新任务正常
/// 退出时读到陈旧地址，只要该 VA 恰在新任务地址空间可写（同二进制线程
/// VA 布局相同，概率不低），就把无辜用户字清零并 `futex_wake` 错误地址。
pub fn clear_tid_slot(tid: u32) {
    let t = tid as usize;
    unsafe {
        if t < CLEAR_TID.len() {
            CLEAR_TID[t] = 0;
        }
    }
}

/// 线程退出通知：CLONE_CHILD_CLEARTID——清零 tid 槽位并 futex 唤醒
/// 等待者（join 方）。须在 dying 线程的地址空间仍活跃时调用。
pub fn exit_thread_notify() {
    let tid = sched_cfs::current_tid() as usize;
    let addr = unsafe {
        if tid < CLEAR_TID.len() {
            let a = CLEAR_TID[tid];
            CLEAR_TID[tid] = 0;
            a
        } else {
            0
        }
    };
    if addr == 0 {
        return;
    }
    let ok = addr % 4 == 0
        && addr.checked_add(4).map_or(false, |e| e <= USER_TOP)
        && process::ensure_user_range_writable(addr, 4);
    if ok {
        unsafe {
            crate::mmu::uaccess_begin();
            core::ptr::write_volatile(addr as *mut u32, 0);
            crate::mmu::uaccess_end();
            futex_wake(addr, 1);
        }
    }
}

// ==================== malloc：brk / mmap 匿名映射 ====================

/// 程序断点与 mmap 游标按进程分别维护（此前为全局单值：进程 A 首次 brk 后，
/// 进程 B 的 brk(0)/mmap 会从 A 的断点/游标继续，导致 B 的堆与 B 自身代码/
/// 栈重叠或越界）。索引为进程 pid。
static mut BRK_CUR: [usize; process::MAX_PROC] = [0; process::MAX_PROC];
static mut MMAP_NEXT: [usize; process::MAX_PROC] =
    [0x3000_0000; process::MAX_PROC];

/// 匿名 mmap 游标基址（文件 mmap 游标 linux_compat::FILE_MMAP_NEXT 的硬上限，
/// 两游标各自推进，跨过此界会互相覆盖 PTE）。
pub const MMAP_BASE: usize = 0x3000_0000;

/// 匿名 mmap 游标增长上限（ASLR 防碰撞）。
///
/// 用户栈经 ASLR 随机落在 [0x3E00_0000, 0x3F00_0000)（栈顶）之下 1MB 处，
/// 最低栈底 = 0x3DF0_0000（下方还有 1 页守护页）。匿名映射游标自
/// 0x3000_0000 向上增长，若放任其超过 0x3D00_0000 就可能与随机化后的栈区
/// （或栈底守护页）重叠：栈溢出会静默写进 mmap 区，ASLR 形同虚设。此上限
/// 在栈区下方保留 15MB 隔离带。
pub const MMAP_CAP: usize = 0x3D00_0000;

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

/// 进程退出/回收后复位该 pid 的断点与 mmap 游标。
///
/// pid 槽位会被复用：若不清零，下一个进程（如 OOBE 退出后复用的 launcher）
/// 会继承上一个进程遗留的未对齐断点（如 0x39a71b0），其堆分配返回的指针落在
/// 内核 brk_set 只从页对齐处开始映射的区间之下 → 首次写入即触发 EL0 fault。
pub fn reset_brk_state(pid: usize) {
    if pid >= process::MAX_PROC {
        return;
    }
    unsafe {
        BRK_CUR[pid] = 0;
        MMAP_NEXT[pid] = 0x3000_0000;
        // 文件 mmap 游标同样按 pid 槽位单调增长，必须一并复位，
        // 否则槽位反复复用后越过 MMAP_CAP → 该槽位永久 ENOMEM。
        crate::linux_compat::reset_file_mmap(pid);
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
    // ★ 增长上界钳制：brk 可越过 0x1FF0_0000（信号蹦床页）、0x2000_0000
    //（fb mmap）与 0x2E00_0000+（文件/匿名 mmap 区）——map_page_in 对已有
    // PTE 静默覆盖，堆页会顶掉蹦床/mmap 映射（rt_sigreturn 跳入堆数据、
    // 应用缓冲被堆互踩）。ELF 堆从低于 0x1000_0000 起，钳到蹦床页之下。
    const BRK_MAX: usize = 0x1FF0_0000;
    if new_brk > BRK_MAX {
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
        // 增长：为 [align_up(old), align_up(new)) 分配并映射新页。
        // 映射范围向上取整到页边界，使调用方可在未对齐断点内安全使用
        // 整页（避免尾部 gap 写崩）。
        let start = (old + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1);
        let end_map = (new_brk + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1);
        let mut va = start;
        let mut mapped = 0usize;
        while va < end_map {
            let pa = match allocator::alloc_page() {
                Some(p) => p,
                None => {
                    crate::uart_send_str("[brk] alloc_page 失败 pid=");
                    crate::print_dec(pid as u64);
                    crate::uart_send_str(" new=");
                    crate::print_hex(new_brk as u64);
                    crate::uart_send_str(" old=");
                    crate::print_hex(old as u64);
                    crate::uart_send_str("\n");
                    process::rollback_user_pages(start, mapped);
                    return old;
                }
            };
            unsafe { core::ptr::write_bytes(pa as *mut u8, 0, vm::PAGE_SIZE) };
            // 记录表满：回滚本次新映射并释放本页，保持原断点
            if !process::map_user_page_owned(va, pa) {
                crate::uart_send_str("[brk] map 失败 pid=");
                crate::print_dec(pid as u64);
                crate::uart_send_str(" va=");
                crate::print_hex(va as u64);
                crate::uart_send_str(" old=");
                crate::print_hex(old as u64);
                crate::uart_send_str("\n");
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
    mmap_anon_with_flags(len, vm::FLAG_USER_DATA)
}

pub fn mmap_anon_with_flags(len: usize, flags: vm::MapFlags) -> i64 {
    if len == 0 {
        return -1;
    }
    let pages = len.div_ceil(vm::PAGE_SIZE);
    let size = pages.checked_mul(vm::PAGE_SIZE).unwrap_or(usize::MAX);
    let pid = cur_pid();
    unsafe {
        let va = MMAP_NEXT[pid];
        // 拒绝越界/回绕/碰撞栈区：映射范围必须完整落在用户地址空间内，
        // 且不得越过 ASLR 栈区保护上限（MMAP_CAP）
        if !vm::user_va_range_ok(va, size) || va + size > MMAP_CAP {
            return -1;
        }
        MMAP_NEXT[pid] += size;
        if va + size > 0x3F00_0000 {
            crate::uart_send_str("[mmap-anon-高地址] va=");
            crate::print_hex(va as u64);
            crate::uart_send_str(" size=");
            crate::print_dec(size as u64);
            crate::uart_send_str("\n");
        }
        let mut reserved = 0usize;
        for i in 0..pages {
            if vm::user_range_occupied(process::current_root(), va + i * vm::PAGE_SIZE, vm::PAGE_SIZE)
                || !process::reserve_user_page_with_flags(va + i * vm::PAGE_SIZE, flags) {
                // 记录表满：回滚已预留页，避免泄漏；游标同步回退（否则
                // 反复失败的 mmap 会把地址空间烧穿，该 pid 永久 ENOMEM）
                process::rollback_user_pages(va, reserved);
                MMAP_NEXT[pid] = va;
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
