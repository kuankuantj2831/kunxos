//! CFS（完全公平调度）调度器。
//!
//! 在旧的简单轮转调度器基础上升级：
//!   - 每个任务维护 `vruntime`（虚拟运行时间）与 `weight`（由 nice 值推导）。
//!   - 每次时钟中断（10ms 时间片）累加当前任务的 vruntime，然后挑选 vruntime
//!     最小的就绪任务运行。
//!   - 三级优先级：实时（Realtime）> 普通（Normal）> 空闲（Idle）。
//!     Realtime 任务总是抢占 Normal/Idle；Idle 任务仅当无其他就绪任务时运行。
//!
//! 上下文切换（`switch_to`）保存/恢复 callee-saved 寄存器 x19-x30、栈指针 sp、
//! 以及 ELR_EL1 / SPSR_EL1 / TPIDR_EL0。其中 ELR/SPSR 是修正旧调度器缺陷的关键：
//! 旧的轮转调度器在 IRQ 处理过程中切换上下文，却不保存/恢复 ELR/SPSR，导致
//! `eret` 返回到错误的任务。此处把它们纳入任务上下文，保证抢占式切换正确。

use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::allocator;
use crate::allocator::alloc_pages_contig;
use crate::timer;
use crate::vm;
use crate::wfi;

/// 最大任务数（含 idle 任务）。
pub const MAX_TASKS: usize = 32;

/// 最大 CPU 数（QEMU virt -smp 最多 4 核）。
pub const MAX_CPUS: usize = 4;

/// 每个任务内核栈的页数（64 页 = 256KB）。
///
/// ELF 加载器（`spawn`/`execve`）在栈上持有进程页表（`ProcPageTable`，
/// 1024 条记录 = 16KB）、`fork_parent`/`create_child_process` 传值拷贝
/// 页表（另 16KB），加上 ELF 读入缓冲与深系统调用链，128KB 栈在 oobe 等
/// 含 2MB BSS 的应用 execve 时接近溢出（写穿栈底破坏位图 → 后续栈分配
/// 重叠 → 内核崩溃）。256KB 提供充足余量；32 任务 × 256KB = 8MB，可接受。
pub const STACK_PAGES: usize = 64;
/// 任务栈大小（字节）。
pub const STACK_SIZE: usize = STACK_PAGES * 4096;

/// 时间片（纳秒）：10ms。
const TIME_SLICE: u64 = 10_000_000;
/// 调度时间片（微秒）。短于时间片的定时唤醒不会额外扣除 vruntime。
const TIME_SLICE_US: u64 = TIME_SLICE / 1_000;
/// CFS 基准权重（nice = 0）。
const WEIGHT_BASE: u32 = 1024;

/// EL1h 模式、中断全开（DAIF = 0）的 SPSR 值。
const SPSR_EL1H: u64 = 0x5;

// ==================== 任务状态 / 优先级 ====================

/// 任务状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    /// 槽位未使用
    Empty,
    /// 就绪，等待调度
    Ready,
    /// 正在运行
    Running,
    /// 阻塞（等待 IPC 等）
    Blocked,
    /// 已退出
    Exited,
}

/// 调度优先级类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PriorityClass {
    /// 空闲优先级（仅无其他任务时运行）
    Idle,
    /// 普通优先级
    Normal,
    /// 实时优先级（总是抢占普通/空闲任务）
    Realtime,
}

// ==================== 任务上下文 ====================

/// 任务上下文（寄存器保存区）。
///
/// 字段顺序与 `switch_to` 汇编中的 stp/ldp 偏移严格对应：
///   x19@0x00 .. x30@0x58, sp@0x60, elr@0x68, spsr@0x70, tpidr@0x78,
///   ttbr0_el1@0x80, sp_el0@0x88
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskContext {
    pub x19: u64,
    pub x20: u64,
    pub x21: u64,
    pub x22: u64,
    pub x23: u64,
    pub x24: u64,
    pub x25: u64,
    pub x26: u64,
    pub x27: u64,
    pub x28: u64,
    pub x29: u64, // frame pointer
    pub x30: u64, // link register，即程序计数器（恢复地址/任务入口）
    pub sp: u64,  // 栈指针
    pub elr: u64, // 异常返回地址
    pub spsr: u64, // 保存的 PSTATE
    pub tpidr: u64, // 线程本地存储基址（TPIDR_EL0）
    pub ttbr0_el1: u64, // 用户地址空间页表根（TTBR0_EL1）
    pub sp_el0: u64, // 用户栈指针（SP_EL0）——上下文切换必须保存/恢复，
                     // 否则被抢占任务恢复后另一任务带着它的 SP_EL0 运行，
                     // 会把数据写到错误任务的用户栈上（系统性随机崩溃根源）
}

impl TaskContext {
    /// 全零上下文。
    pub const fn empty() -> Self {
        TaskContext {
            x19: 0, x20: 0, x21: 0, x22: 0, x23: 0, x24: 0,
            x25: 0, x26: 0, x27: 0, x28: 0, x29: 0, x30: 0,
            sp: 0, elr: 0, spsr: 0, tpidr: 0, ttbr0_el1: 0, sp_el0: 0,
        }
    }

    /// 新建线程首次运行的上下文。
    ///
    /// `x19` 存放参数、`x20` 存放入口地址，`x30` 指向 `thread_trampoline`，
    /// 首次调度时由它把参数搬到 x0 并跳转入入口函数。
    /// `ttbr0` 为该线程激活的地址空间根表（内核线程传 `vm::kernel_root_addr()`）。
    ///
    /// `elr` 初始为 `thread_trampoline` 自身（而非 0）：首次调度后 trampoline
    /// 会 `daifclr` 使能 IRQ，若在 `br x20` 之前被定时器抢占，`irq_entry` 的
    /// `eret` 使用当前 ELR_EL1 —— 若为 0 会跳转到地址 0（EL0 指令 abort）。
    /// 设为 trampoline 后，被抢占会回到 trampoline 重跑（x19/x20 已由
    /// switch_to 恢复），消除偶发「eret 到 PC=0」的 EL0 故障。
    pub fn new_thread(entry: fn(usize) -> !, arg: usize, stack_top: usize, ttbr0: usize) -> Self {
        TaskContext {
            x19: arg as u64,
            x20: entry as usize as u64,
            x30: thread_trampoline as *const () as usize as u64,
            sp: stack_top as u64,
            elr: thread_trampoline as *const () as usize as u64,
            spsr: SPSR_EL1H,
            tpidr: 0,
            ttbr0_el1: ttbr0 as u64, sp_el0: 0,
            ..TaskContext::empty()
        }
    }
}

// ==================== 任务控制块 ====================

/// 任务控制块（TCB，即调度实体）。
pub struct Task {
    /// 任务 id
    pub tid: u32,
    /// 任务名（调试用）
    pub name: &'static str,
    /// 状态
    pub state: TaskState,
    /// 上下文
    pub context: TaskContext,
    /// 虚拟运行时间（CFS）
    pub vruntime: u64,
    /// 权重（由 nice 推导）
    pub weight: u32,
    /// nice 值
    pub nice: i32,
    /// 优先级类
    pub class: PriorityClass,
    /// 内核栈基地址
    pub stack_base: usize,
    /// 内核栈顶地址
    pub stack_top: usize,
    /// 所属进程 id
    pub pid: u32,
}

impl Task {
    const fn empty() -> Self {
        Task {
            tid: 0,
            name: "",
            state: TaskState::Empty,
            context: TaskContext::empty(),
            vruntime: 0,
            weight: WEIGHT_BASE,
            nice: 0,
            class: PriorityClass::Idle,
            stack_base: 0,
            stack_top: 0,
            pid: 0,
        }
    }
}

/// 任务数组（静态分配，位于 .bss）。
static mut TASKS: [Task; MAX_TASKS] = [const { Task::empty() }; MAX_TASKS];

/// 每个 CPU 当前运行任务下标（SMP：各核独立）。
static mut CURRENT: [usize; MAX_CPUS] = [usize::MAX; MAX_CPUS];

/// 调度器自旋锁：保护任务表 / 睡眠表 / CURRENT 的并发访问。
///
/// 上下文切换采用「锁交接」模式：`lock()` 在切换前获取，切换后（任务被
/// 再次调度时）由恢复点上的 `unlock()` 释放——每个切换路径的 lock/unlock
/// 跨 `switch_to` 配对，全局任意时刻至多一个核处于切换临界区，因此同一个
/// 就绪任务绝不会被两个核同时选中。单核（默认）运行时锁无竞争，行为与
/// 原来完全一致。
static SCHED_LOCK: AtomicBool = AtomicBool::new(false);

fn sched_lock() {
    while SCHED_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn sched_unlock() {
    SCHED_LOCK.store(false, Ordering::Release);
}

/// 当前 CPU 号。
fn current_cpu() -> usize {
    (crate::smp::cpu_id() as usize).min(MAX_CPUS - 1)
}

/// 睡眠等待槽位：每个任务同一时间至多登记一个截止时间。
struct Sleeper {
    active: bool,
    tid: u32,
    deadline_us: u64,
}

const EMPTY_SLEEPER: Sleeper = Sleeper { active: false, tid: 0, deadline_us: 0 };
static mut SLEEPERS: [Sleeper; MAX_TASKS] = [const { EMPTY_SLEEPER }; MAX_TASKS];
static mut LAST_SLICE_US: [u64; MAX_CPUS] = [0; MAX_CPUS];
static mut NEED_RESCHED: [bool; MAX_CPUS] = [false; MAX_CPUS];

/// 启动时切换用的临时上下文（每核一份；保存后丢弃，不再切回）。
static mut BOOT_CTX: [TaskContext; MAX_CPUS] = [const { TaskContext::empty() }; MAX_CPUS];

// ==================== 上下文切换汇编 ====================

extern "C" {
    /// 切换到目标上下文（保存 ELR/SPSR/TPIDR 等完整状态）。
    fn switch_to(from: *mut TaskContext, to: *const TaskContext);
    /// 线程入口蹦床：使能 IRQ，把参数搬到 x0，跳转入入口函数。
    fn thread_trampoline();
}

global_asm!(
    r#"
    // 上下文切换：x0 = from（保存目标），x1 = to（恢复来源）
    .section .text.switch_to, "ax"
    .global switch_to
    .type switch_to, %function
switch_to:
    // 保存 callee-saved 寄存器到 from
    stp x19, x20, [x0, #0x00]
    stp x21, x22, [x0, #0x10]
    stp x23, x24, [x0, #0x20]
    stp x25, x26, [x0, #0x30]
    stp x27, x28, [x0, #0x40]
    stp x29, x30, [x0, #0x50]
    mov x9, sp
    str x9, [x0, #0x60]
    // 保存 ELR_EL1 / SPSR_EL1 / TPIDR_EL0 / TTBR0_EL1 / SP_EL0
    mrs x9, elr_el1
    str x9, [x0, #0x68]
    mrs x9, spsr_el1
    str x9, [x0, #0x70]
    mrs x9, tpidr_el0
    str x9, [x0, #0x78]
    mrs x9, ttbr0_el1
    str x9, [x0, #0x80]
    mrs x9, sp_el0
    str x9, [x0, #0x88]
    // 从 to 恢复
    ldp x19, x20, [x1, #0x00]
    ldp x21, x22, [x1, #0x10]
    ldp x23, x24, [x1, #0x20]
    ldp x25, x26, [x1, #0x30]
    ldp x27, x28, [x1, #0x40]
    ldp x29, x30, [x1, #0x50]
    ldr x9, [x1, #0x60]
    mov sp, x9
    ldr x9, [x1, #0x68]
    msr elr_el1, x9
    ldr x9, [x1, #0x70]
    msr spsr_el1, x9
    ldr x9, [x1, #0x78]
    msr tpidr_el0, x9
    // 恢复 TTBR0_EL1 并做完整 TLB 维护（tlbi vmalle1）
    ldr x9, [x1, #0x80]
    msr ttbr0_el1, x9
    ldr x9, [x1, #0x88]
    msr sp_el0, x9
    isb
    tlbi vmalle1
    dsb ish
    isb
    // 跳转到恢复后的 x30（目标任务）
    ret

    // 线程入口蹦床：x19 = 参数，x20 = 入口函数
    .section .text.thread_trampoline, "ax"
    .global thread_trampoline
    .type thread_trampoline, %function
thread_trampoline:
    // 首次运行：释放调度锁（切换路径在 switch 前获取、由被切到任务的
    // 恢复点释放；首次运行任务的恢复点是本蹦床而非某个 switch 路径，
    // 必须在此补齐解锁，否则调度锁永久持有 → 后续 block/yield 死锁）。
    bl sched_unlock_from_asm
    // 使能 IRQ（首次被调度进入时处于中断屏蔽态）
    msr daifclr, #2
    // 把参数搬到 x0，跳转入入口
    mov x0, x19
    mov x9, x20
    br x9
"#
);

/// 供 `thread_trampoline` 汇编调用：释放调度锁（首次运行任务的解锁点）。
#[no_mangle]
pub extern "C" fn sched_unlock_from_asm() {
    sched_unlock();
}

// ==================== 内部辅助 ====================

/// 优先级类的排序权值（越小优先级越高）。
fn class_rank(c: PriorityClass) -> u8 {
    match c {
        PriorityClass::Realtime => 0,
        PriorityClass::Normal => 1,
        PriorityClass::Idle => 2,
    }
}

/// 由 nice 值推导 CFS 权重（nice 范围 -5..=5，越界截断）。
fn weight_of(nice: i32) -> u32 {
    const TABLE: [u32; 11] = [
        3125, // -5
        2500, // -4
        2000, // -3
        1600, // -2
        1280, // -1
        1024, //  0
        819,  //  1
        655,  //  2
        524,  //  3
        419,  //  4
        335,  //  5
    ];
    let idx = (nice + 5).clamp(0, 10) as usize;
    TABLE[idx]
}

/// 当前全局最小 vruntime（新任务继承，避免抢占不公平）。
fn min_vruntime() -> u64 {
    let mut min = u64::MAX;
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].state != TaskState::Empty && TASKS[i].vruntime < min {
                min = TASKS[i].vruntime;
            }
        }
    }
    if min == u64::MAX {
        0
    } else {
        min
    }
}

/// 分配一个空闲任务槽位；表满返回 `None`（不再 panic，防 fork 炸弹 DoS）。
fn alloc_tid() -> Option<usize> {
    for i in 0..MAX_TASKS {
        unsafe {
            if TASKS[i].state == TaskState::Empty {
                return Some(i);
            }
        }
    }
    None
}

/// 分配一个 64KB 任务栈（物理连续），返回栈顶地址。
pub fn alloc_stack() -> usize {
    let base = alloc_pages_contig(STACK_PAGES).expect("sched_cfs: alloc task stack failed");
    // 用 0xA5 填充整个任务栈：内核栈越界写会破坏该模式，供故障诊断检测溢出
    unsafe {
        for p in 0..STACK_PAGES {
            core::ptr::write_bytes((base + p * 4096) as *mut u8, 0xA5, 4096);
        }
    }
    base + STACK_SIZE
}

/// 挑选下一个要运行的任务（就绪任务中 vruntime 最小、优先级最高）。
/// 无就绪任务时返回 `None`（调用方回退到本核 idle 任务）。
fn pick_next() -> Option<usize> {
    let mut best = usize::MAX;
    let mut best_rank = u8::MAX;
    let mut best_vr = u64::MAX;
    let my_idle = idle_slot();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].state != TaskState::Ready {
                continue;
            }
            // 其他核的 idle 任务不可被本核选中（每核只运行自己的 idle）
            if TASKS[i].class == PriorityClass::Idle && i != my_idle {
                continue;
            }
            let rank = class_rank(TASKS[i].class);
            let vr = TASKS[i].vruntime;
            if rank < best_rank || (rank == best_rank && vr < best_vr) {
                best = i;
                best_rank = rank;
                best_vr = vr;
            }
        }
    }
    if best == usize::MAX {
        None
    } else {
        Some(best)
    }
}

/// 本核的 idle 任务槽位（SMP：每核一个 idle 任务，槽位 = CPU 号）。
fn idle_slot() -> usize {
    let cpu = current_cpu();
    if cpu < crate::smp::cpu_count() as usize {
        cpu
    } else {
        0
    }
}

/// 执行一次上下文切换：cur -> next，并把 next 置为 Running。
///
/// 调用者负责在切换前获取调度锁、在返回（任务被再次调度）后释放。
/// 切换期间 `CURRENT[本核]` 更新为 next，其他核在同一把锁保护下不会
/// 重复选中 next。
unsafe fn do_switch(cur: usize, next: usize) {
    CURRENT[current_cpu()] = next;
    TASKS[next].state = TaskState::Running;
    switch_to(
        addr_of_mut!(TASKS[cur].context),
        addr_of!(TASKS[next].context),
    );
}

/// idle 任务入口：等待中断，仅当无其他就绪任务时被调度。
fn idle_loop(_: usize) -> ! {
    loop {
        unsafe { wfi() };
    }
}

// ==================== 公共接口 ====================

/// 初始化调度器：清空任务表并为每个 CPU 创建 idle 任务。
pub fn init() {
    unsafe {
        for i in 0..MAX_TASKS {
            TASKS[i] = Task::empty();
            SLEEPERS[i] = EMPTY_SLEEPER;
        }
        for c in 0..MAX_CPUS {
            CURRENT[c] = usize::MAX;
            LAST_SLICE_US[c] = 0;
            NEED_RESCHED[c] = false;
        }
    }
    // 每核一个 idle 任务，槽位 = CPU 号（单核仍为槽位 0，行为不变）
    let ncpu = (crate::smp::cpu_count() as usize).min(MAX_CPUS);
    for cpu in 0..ncpu {
        let stack_top = alloc_stack();
        unsafe {
            let t = &mut TASKS[cpu];
            t.tid = cpu as u32;
            t.name = "idle";
            t.state = TaskState::Ready;
            t.vruntime = 0;
            t.weight = WEIGHT_BASE;
            t.nice = 0;
            t.class = PriorityClass::Idle;
            t.stack_base = stack_top - STACK_SIZE;
            t.stack_top = stack_top;
            t.pid = 0;
            t.context = TaskContext::new_thread(idle_loop, cpu, stack_top, vm::kernel_root_addr());
        }
    }
}

/// 创建新线程（自动分配内核栈），返回 tid。
///
/// `ttbr0` 为该线程激活的地址空间根表（用户线程传其进程页表根，
/// 内核线程传 `vm::kernel_root_addr()`）。
pub fn spawn(
    pid: u32,
    name: &'static str,
    entry: fn(usize) -> !,
    arg: usize,
    class: PriorityClass,
    nice: i32,
    ttbr0: usize,
) -> u32 {
    // 先取槽位再分配栈：槽位不足时不浪费物理页
    sched_lock();
    let Some(tid) = alloc_tid() else { sched_unlock(); return u32::MAX };
    let stack_top = alloc_stack();
    // 保持调用方 IRQ 屏蔽状态（与 insert 同理）：boot 期调用时若无条件
    // daifclr 会在中断处理注册前使能 IRQ。
    let irq_unmasked: u64;
    unsafe { asm!("mrs {}, daif", out(reg) irq_unmasked) };
    unsafe { asm!("msr daifset, #2") };
    unsafe {
        let t = &mut TASKS[tid];
        t.tid = tid as u32;
        t.name = name;
        t.state = TaskState::Ready;
        t.vruntime = min_vruntime();
        t.weight = weight_of(nice);
        t.nice = nice;
        t.class = class;
        t.stack_base = stack_top - STACK_SIZE;
        t.stack_top = stack_top;
        t.pid = pid;
        t.context = TaskContext::new_thread(entry, arg, stack_top, ttbr0);
    }
    sched_unlock();
    if irq_unmasked & (1 << 7) == 0 {
        unsafe { asm!("msr daifclr, #2") };
    }
    tid as u32
}

/// 以给定上下文插入线程（供 fork 使用，栈由调用者分配），返回 tid；
/// 槽位不足返回 `u32::MAX`（调用方负责回滚已分配资源）。
pub fn insert(
    name: &'static str,
    pid: u32,
    ctx: TaskContext,
    stack_base: usize,
    stack_top: usize,
    class: PriorityClass,
    nice: i32,
) -> u32 {
    sched_lock();
    let Some(tid) = alloc_tid() else { sched_unlock(); return u32::MAX };
    // ★ 保持调用方的 IRQ 屏蔽状态：`fork_parent` 在 IRQ 屏蔽的临界区里插入
    // 子任务，若此处无条件 `daifclr`，子任务会在 fork 尚未完成（父进程还
    // 在 fork_parent 尾部）时即可被调度 —— 与父进程的 syscall 返回路径
    // 并发，曾疑为「fork 返回后偶发 PC=0 / LR 损坏」的诱因之一。先读 DAIF
    // 的 I 位（bit7），仅当进入时 IRQ 未屏蔽才在退出时恢复。
    let irq_unmasked: u64;
    unsafe { asm!("mrs {}, daif", out(reg) irq_unmasked) };
    unsafe { asm!("msr daifset, #2") };
    unsafe {
        let t = &mut TASKS[tid];
        t.tid = tid as u32;
        t.name = name;
        t.state = TaskState::Ready;
        t.vruntime = min_vruntime();
        t.weight = weight_of(nice);
        t.nice = nice;
        t.class = class;
        t.stack_base = stack_base;
        t.stack_top = stack_top;
        t.pid = pid;
        t.context = ctx;
    }
    sched_unlock();
    if irq_unmasked & (1 << 7) == 0 {
        unsafe { asm!("msr daifclr, #2") };
    }
    tid as u32
}

/// 读取当前任务下标；调度器尚未启动（`CURRENT` 仍为哨兵值 `usize::MAX`）时
/// 返回 `None`。
///
/// 所有读取 `CURRENT` 的地方都应经此函数，避免在调度器启动前以 `usize::MAX`
/// 下标访问任务数组触发越界 panic（index out of bounds）。
fn current_idx() -> Option<usize> {
    let cur = unsafe { CURRENT[current_cpu()] };
    if cur < MAX_TASKS {
        Some(cur)
    } else {
        None
    }
}

/// 当前任务 tid。
pub fn current_tid() -> u32 {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].tid },
        None => 0,
    }
}

/// 当前任务所属进程 pid。
pub fn current_pid() -> u32 {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].pid },
        None => 0,
    }
}

/// 当前存活任务数（非 Empty 槽位），供 `sysinfo` 等统计。
pub fn task_count() -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].state != TaskState::Empty {
                n += 1;
            }
        }
    }
    n
}

/// 当前任务名（panic 诊断用；调度器未启动时返回 "?"）。
pub fn current_task_name() -> &'static str {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].name },
        None => "?",
    }
}

/// 当前任务的内核栈范围（base, top），供 fork 拷贝栈。
pub fn current_stack() -> (usize, usize) {
    match current_idx() {
        Some(i) => unsafe { (TASKS[i].stack_base, TASKS[i].stack_top) },
        None => (0, 0),
    }
}

/// [诊断] 检查**所有任务**内核栈底部的 0xA5 填充哨兵是否被破坏（内核栈溢出
/// 会写穿栈底 → 破坏物理页位图 → 后续栈分配重叠 → 系统级随机崩溃）。
pub fn check_all_stack_canaries() -> usize {
    let mut clobbered = 0usize;
    unsafe {
        for i in 0..MAX_TASKS {
            let t = &TASKS[i];
            if t.state == TaskState::Empty || t.stack_base == 0 {
                continue;
            }
            let mut bad = 0usize;
            for off in (0..64usize).step_by(8) {
                let v = *((t.stack_base + off) as *const u64);
                if v != 0xA5A5A5A5A5A5A5A5 {
                    bad += 1;
                }
            }
            if bad > 0 {
                clobbered += 1;
                crate::uart_send_str("[STACK-OVERFLOW] task ");
                crate::print_dec(t.tid as u64);
                crate::uart_send_str(" pid=");
                crate::print_dec(t.pid as u64);
                crate::uart_send_str(" stack_base=");
                crate::print_hex(t.stack_base as u64);
                crate::uart_send_str(" clobbered_words=");
                crate::print_dec(bad as u64);
                crate::uart_send_str("\n");
            }
        }
    }
    clobbered
}

/// 当前任务的内核栈顶物理/虚拟地址（供故障诊断转储栈顶内容）。
pub fn current_stack_top() -> usize {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].stack_top },
        None => 0,
    }
}

/// 当前任务的 nice 值。
pub fn current_nice() -> i32 {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].nice },
        None => 0,
    }
}

/// 调整当前任务的 nice 与 CFS 权重（GUI 前台进程优待用）。
///
/// 权重越大，vruntime 增长越慢、被选中的概率越高：`nice = -3`（权重 2000）
/// 约为普通任务（1024）的两倍 CPU 份额。保持 CFS 语义不变。
pub fn set_current_nice(nice: i32) {
    sched_lock();
    if let Some(cur) = current_idx() {
        unsafe {
            let t = &mut TASKS[cur];
            t.nice = nice.clamp(-5, 5);
            t.weight = weight_of(t.nice);
        }
    }
    sched_unlock();
}

/// 当前任务的优先级类。
pub fn current_class() -> PriorityClass {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].class },
        None => PriorityClass::Idle,
    }
}

/// 转储任务表（panic 诊断用）：逐任务打印 tid/pid/state/name/栈范围/
/// 上下文关键字段（x30、sp、elr），用于定位损坏的 TCB。
pub fn dump_tasks() {
    unsafe {
        for i in 0..MAX_TASKS {
            let t = &TASKS[i];
            crate::uart_send_str("  tid=");
            crate::print_dec(t.tid as u64);
            crate::uart_send_str(" pid=");
            crate::print_dec(t.pid as u64);
            crate::uart_send_str(" st=");
            crate::print_dec(state_code(t.state) as u64);
            crate::uart_send_str(" ");
            crate::uart_send_str(t.name);
            crate::uart_send_str(" stack=[");
            crate::print_hex(t.stack_base as u64);
            crate::uart_send_str(",");
            crate::print_hex(t.stack_top as u64);
            crate::uart_send_str("] sp=");
            crate::print_hex(t.context.sp);
            crate::uart_send_str(" x30=");
            crate::print_hex(t.context.x30);
            crate::uart_send_str(" elr=");
            crate::print_hex(t.context.elr);
            crate::uart_send_str(" ttbr0=");
            crate::print_hex(t.context.ttbr0_el1);
            crate::uart_send_str("\n");
        }
    }
}

fn state_code(s: TaskState) -> u8 {
    match s {
        TaskState::Empty => 0,
        TaskState::Ready => 1,
        TaskState::Running => 2,
        TaskState::Blocked => 3,
        TaskState::Exited => 4,
    }
}

/// 定时器中断触发：累加当前任务 vruntime 并切换到下一个任务。
///
/// 该函数由 IRQ 处理链调用（此时 IRQ 已屏蔽），切换后不返回，
/// 直到本任务被再次调度时才从这里返回。
///
/// `#[inline(never)]`：本函数内部执行上下文切换（`do_switch`），被内联后
/// 切换恢复点会落在调用方（`timer_handle_irq`）的错误栈帧上，`ret` 会弹出
/// 垃圾返回地址（表现为 EL1 PC=0 的同步异常）。必须保持为独立函数。
///
/// SMP：每核定时器各自触发本函数；调度锁保证任务表并发安全，全局就绪
/// 队列 + 每核 CURRENT 使任务天然在核间迁移。
#[inline(never)]
pub fn tick() {
    let cpu = current_cpu();
    let now = timer::now_us();
    sched_lock();
    unsafe {
        for s in SLEEPERS.iter_mut() {
            if !s.active || s.deadline_us > now {
                continue;
            }
            let tid = s.tid as usize;
            if tid < MAX_TASKS && TASKS[tid].state == TaskState::Blocked {
                TASKS[tid].state = TaskState::Ready;
                NEED_RESCHED[cpu] = true;
            }
            s.active = false;
        }

        // 精确睡眠可让物理定时器早于 10ms 时间片到期；只唤醒任务，不提前
        // 累加 vruntime，保持原有 CFS 时间片语义。
        if LAST_SLICE_US[cpu] != 0 && now.saturating_sub(LAST_SLICE_US[cpu]) < TIME_SLICE_US {
            sched_unlock();
            return;
        }
        LAST_SLICE_US[cpu] = now;
        NEED_RESCHED[cpu] = false;
    }

    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        return;
    }
    unsafe {
        let w = TASKS[cur].weight.max(1) as u64;
        TASKS[cur].vruntime += TIME_SLICE * WEIGHT_BASE as u64 / w;
        if TASKS[cur].state == TaskState::Running {
            TASKS[cur].state = TaskState::Ready;
        }
    }
    let next = match pick_next() {
        Some(n) => n,
        None => {
            unsafe { TASKS[cur].state = TaskState::Running };
            sched_unlock();
            return;
        }
    };
    if next == cur {
        unsafe { TASKS[cur].state = TaskState::Running };
        sched_unlock();
        return;
    }
    unsafe { do_switch(cur, next) };
    // 被再次调度后：释放调度锁（与本路径的获取配对），恢复运行
    sched_unlock();
}

/// 阻塞当前任务直到单调时钟到达 `deadline_us`，也允许设备事件提前唤醒。
#[inline(never)]
pub fn sleep_until(deadline_us: u64) {
    if deadline_us <= timer::now_us() {
        return;
    }
    let tid = current_tid();
    let mut registered = false;
    unsafe { asm!("msr daifset, #2") };
    sched_lock();
    unsafe {
        for s in SLEEPERS.iter_mut() {
            if !s.active {
                *s = Sleeper { active: true, tid, deadline_us };
                registered = true;
                break;
            }
        }
    }
    sched_unlock();
    if registered {
        block_current();
    } else {
        // 槽位数量与任务数相同，正常情况下不会到达；保留可工作的降级路径。
        // 保持当前 DAIF（syscall 上下文为屏蔽态），yield_now 会正确保存恢复。
        while timer::now_us() < deadline_us {
            yield_now();
        }
    }
}

/// 提前唤醒指定任务的定时睡眠。调用方已处于 IRQ 屏蔽上下文。
pub fn wake_sleeper_irq(tid: u32) {
    sched_lock();
    unsafe {
        for s in SLEEPERS.iter_mut() {
            if !s.active || s.tid != tid {
                continue;
            }
            let idx = tid as usize;
            if idx < MAX_TASKS && TASKS[idx].state == TaskState::Blocked {
                TASKS[idx].state = TaskState::Ready;
                NEED_RESCHED[current_cpu()] = true;
            }
            s.active = false;
            break;
        }
    }
    sched_unlock();
}

/// 返回下次物理定时器应触发的间隔：调度时间片和最近睡眠截止时间中的较小值。
pub fn next_timer_interval_us() -> u64 {
    let cpu = current_cpu();
    let now = timer::now_us();
    sched_lock();
    let mut next = unsafe {
        if LAST_SLICE_US[cpu] == 0 {
            TIME_SLICE_US
        } else {
            TIME_SLICE_US.saturating_sub(now.saturating_sub(LAST_SLICE_US[cpu])).max(1)
        }
    };
    unsafe {
        for s in SLEEPERS.iter() {
            if s.active {
                next = next.min(s.deadline_us.saturating_sub(now).max(1));
            }
        }
    }
    sched_unlock();
    next.clamp(500, TIME_SLICE_US)
}

/// 在 IRQ 处理结束前兑现设备唤醒产生的重调度请求。
///
/// `#[inline(never)]`：本函数内部执行上下文切换（`do_switch`）并在被再次
/// 调度后返回。若被内联进 `irq_handler`，切换后的恢复点会落在调用方中段，
/// `pop x30; ret` 弹出错误栈槽位 → 跳转垃圾地址（用户态 PC=0 / EL1 ELR=0）。
/// 与 `tick`/`yield_now`/`block_current` 同源的修复，此前遗漏此函数导致
/// 用户进程偶发跳转地址 0（被 el0_fault 拦截）。
#[inline(never)]
pub fn resched_if_needed() {
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS || !unsafe { NEED_RESCHED[cpu] } {
        sched_unlock();
        return;
    }
    unsafe {
        NEED_RESCHED[cpu] = false;
        if TASKS[cur].state == TaskState::Running {
            TASKS[cur].state = TaskState::Ready;
        }
    }
    let next = match pick_next() {
        Some(n) => n,
        None => {
            unsafe { TASKS[cur].state = TaskState::Running };
            sched_unlock();
            return;
        }
    };
    if next == cur {
        unsafe { TASKS[cur].state = TaskState::Running };
        sched_unlock();
        return;
    }
    unsafe { do_switch(cur, next) };
    sched_unlock();
}

/// 读取当前 DAIF 值。
#[inline]
pub unsafe fn read_daif() -> u64 {
    let d: u64;
    asm!("mrs {}, daif", out(reg) d);
    d
}

/// 写回 DAIF（恢复中断状态）。
#[inline]
pub unsafe fn write_daif(d: u64) {
    asm!("msr daif, {}", in(reg) d);
}

/// 主动让出 CPU（累加一个时间片的 vruntime 后再切换，保证公平轮转）。
///
/// 切换前后**保存-恢复调用方的 DAIF**：syscall 上下文（IRQ 屏蔽）调用时
/// 返回后保持屏蔽，普通内核上下文调用时返回后恢复使能，二者都正确。
#[inline(never)]
pub fn yield_now() {
    let saved = unsafe { read_daif() };
    unsafe { asm!("msr daifset, #2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe {
        let w = TASKS[cur].weight.max(1) as u64;
        TASKS[cur].vruntime += TIME_SLICE * WEIGHT_BASE as u64 / w;
        if TASKS[cur].state == TaskState::Running {
            TASKS[cur].state = TaskState::Ready;
        }
    }
    let next = match pick_next() {
        Some(n) => n,
        None => {
            unsafe { TASKS[cur].state = TaskState::Running };
            sched_unlock();
            unsafe { write_daif(saved) };
            return;
        }
    };
    if next == cur {
        unsafe { TASKS[cur].state = TaskState::Running };
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe { do_switch(cur, next) };
    // 被再次调度后恢复中断
    sched_unlock();
    unsafe { asm!("msr daifclr, #2") };
}

/// 阻塞当前任务（调用者须先记录阻塞原因），直到被 `wake` 唤醒。
///
/// 返回后恢复调用前的 DAIF（同 `yield_now`）：syscall 上下文保持屏蔽，
/// 直到 eret 到 EL0 才由硬件使能中断。
#[inline(never)]
pub fn block_current() {
    let saved = unsafe { read_daif() };
    unsafe { asm!("msr daifset, #2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe {
        TASKS[cur].state = TaskState::Blocked;
    }
    let next = match pick_next() {
        Some(n) => n,
        None => {
            unsafe { TASKS[cur].state = TaskState::Running };
            sched_unlock();
            unsafe { write_daif(saved) };
            return;
        }
    };
    unsafe { do_switch(cur, next) };
    sched_unlock();
    unsafe { asm!("msr daifclr, #2") };
}

/// 唤醒指定任务（置为就绪）。
pub fn wake(tid: u32) {
    if (tid as usize) < MAX_TASKS {
        unsafe { asm!("msr daifset, #2") };
        sched_lock();
        unsafe {
            if TASKS[tid as usize].state == TaskState::Blocked {
                TASKS[tid as usize].state = TaskState::Ready;
            }
        }
        sched_unlock();
        unsafe { asm!("msr daifclr, #2") };
    }
}

/// 唤醒指定任务（不操作 DAIF，供已屏蔽中断的上下文调用）。
///
/// 与 [`wake`] 的区别：调用者已处于中断屏蔽状态（如设备中断处理例程），
/// 这里直接修改任务状态，避免在中断处理中途重新使能 IRQ 引发嵌套中断。
pub fn wake_irq(tid: u32) {
    if (tid as usize) < MAX_TASKS {
        sched_lock();
        unsafe {
            if TASKS[tid as usize].state == TaskState::Blocked {
                TASKS[tid as usize].state = TaskState::Ready;
                NEED_RESCHED[current_cpu()] = true;
            }
        }
        sched_unlock();
    }
}

/// 切换当前任务的地址空间根表（TTBR0_EL1）并做 TLB 维护。
///
/// 供 execve 替换进程映像后调用：更新当前任务的上下文保存值，使后续
/// 调度往返都能恢复新根表，同时立即 `msr ttbr0_el1` + `tlbi vmalle1`。
/// 调用方须已屏蔽 IRQ（execve 处于系统调用上下文，异常入口已置 DAIF）。
pub fn set_current_ttbr0(ttbr0: usize) {
    unsafe {
        let cur = CURRENT[current_cpu()];
        if cur < MAX_TASKS {
            TASKS[cur].context.ttbr0_el1 = ttbr0 as u64;
        }
        vm::write_ttbr0_el1(ttbr0);
    }
}

/// 唤醒属于指定进程的所有阻塞任务（供 exit 唤醒 wait 中的父进程等）。
///
/// 与 [`wake_irq`] 一致：调用方须已屏蔽 IRQ，此处不操作 DAIF。
pub fn wake_pid(pid: u32) {
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Blocked {
                TASKS[i].state = TaskState::Ready;
            }
        }
    }
    sched_unlock();
}

/// 终止属于指定进程的所有任务（标记为 Exited，不再被调度）。
///
/// 供进程退出时使用：多线程进程（CLONE_THREAD 共享地址空间）退出时必须先
/// 终止兄弟线程，否则它们会在父进程 reap 释放页表后继续以悬空 TTBR0 运行。
/// 调用方须已屏蔽 IRQ。
pub fn kill_pid(pid: u32) {
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid
                && TASKS[i].state != TaskState::Empty
                && TASKS[i].state != TaskState::Exited
            {
                TASKS[i].state = TaskState::Exited;
            }
        }
    }
    sched_unlock();
}

/// 回收属于指定进程的全部已退出任务：复位 TCB 槽位并归还 64KB 内核栈。
///
/// 由父进程 `wait`/`reap` 在目标进程已 Zombie（其全部线程均已 Exited）后调用；
/// 内部屏蔽 IRQ 并持调度锁保护任务表。
pub fn reap_pid(pid: u32) {
    unsafe { asm!("msr daifset, #2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Exited {
                let base = TASKS[i].stack_base;
                if base != 0 {
                    for p in 0..STACK_PAGES {
                        allocator::free_page(base + p * allocator::PAGE_SIZE);
                    }
                }
                TASKS[i] = Task::empty();
            }
        }
    }
    sched_unlock();
    unsafe { asm!("msr daifclr, #2") };
}

/// 退出当前任务（不再返回）。
///
/// SMP：调度锁在切换后由被切到的任务恢复点释放（锁交接），本任务已
/// Exited，不再返回。
#[inline(never)]
pub fn exit_current() -> ! {
    unsafe { asm!("msr daifset, #2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        loop {
            unsafe { wfi() };
        }
    }
    unsafe {
        TASKS[cur].state = TaskState::Exited;
    }
    let next = match pick_next() {
        Some(n) => n,
        None => idle_slot(),
    };
    unsafe { do_switch(cur, next) };
    // 不应到达（本任务已 Exited，锁由 next 的恢复路径释放）
    loop {
        unsafe { wfi() };
    }
}

/// 启动调度器（CPU0）：切换到第一个就绪任务，永不返回。
///
/// 首次切换前获取调度锁，由首个任务的首次运行（thread_trampoline 的
/// `sched_unlock_from_asm`）释放——保证「每个切换路径的锁获取都有配对
/// 释放」的交接不变量对首次运行任务同样成立。
pub fn start() -> ! {
    let cpu = current_cpu();
    sched_lock();
    let next = pick_next().unwrap_or(idle_slot());
    unsafe {
        CURRENT[cpu] = next;
        TASKS[next].state = TaskState::Running;
        switch_to(
            addr_of_mut!(BOOT_CTX[cpu]),
            addr_of!(TASKS[next].context),
        );
    }
    loop {
        unsafe { wfi() };
    }
}

/// secondary 核进入调度器（`smp_secondary_main` 调用）：直接运行本核
/// 的 idle 任务，随后由定时器 tick 从全局就绪队列取任务。永不返回。
pub fn start_secondary(cpu: u32) -> ! {
    let slot = (cpu as usize).min(MAX_CPUS - 1);
    sched_lock();
    unsafe {
        CURRENT[slot] = slot;
        TASKS[slot].state = TaskState::Running;
        switch_to(
            addr_of_mut!(BOOT_CTX[slot]),
            addr_of!(TASKS[slot].context),
        );
    }
    loop {
        unsafe { wfi() };
    }
}


/// 遍历任务表，对每个非空任务调用 `f(pid, 状态名, 任务名)`。
/// 供 procfs 等诊断接口使用（调用方自行屏蔽 IRQ 与否均可，只读）。
pub fn for_each_task(mut f: impl FnMut(u32, &'static str, &'static str)) {
    unsafe {
        for i in 0..MAX_TASKS {
            let t = &TASKS[i];
            if t.state == TaskState::Empty {
                continue;
            }
            let st = match t.state {
                TaskState::Running => "R",
                TaskState::Ready => "R",
                TaskState::Blocked => "S",
                TaskState::Exited => "Z",
                TaskState::Empty => "?",
            };
            f(t.pid, st, t.name);
        }
    }
}
