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
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::allocator;
use crate::allocator::alloc_pages_contig;
use crate::process::MAX_PROC;
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
/// 唤醒抢占粒度（vruntime，纳秒）：被唤醒任务的 vruntime 比当前运行任务
/// 落后超过该值时立即请求重调度，不必等当前任务用完整个 10ms 时间片
/// （Linux `check_preempt_wakeup` 思路；交互任务响应延迟从最坏 10ms 降到
/// 一次 IRQ 尾重调度）。鸿蒙交互调度 / ColorOS 潮汐引擎对交互线程的同款优待。
const WAKEUP_GRAN_NS: u64 = 2_000_000;

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
    /// 已停止（SIGSTOP，应用后台/挂起；直到 SIGCONT 才恢复调度）。
    ///
    /// 生命周期：应用进入后台时 lifd 发送 SIGSTOP，任务在下次进入内核时
    /// 置为 Stopped 并切走；SIGCONT 由 `cont_pid` 唤醒。Stopped 任务
    /// 不被 `pick_next` 选中、不被 `wake`/`wake_pid`（IPC 唤醒）误唤醒，
    /// 但会被 `kill_pid`（进程退出/终止）清理。
    Stopped,
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

/// 任务上下文（寄存器保存区，RISC-V）。
///
/// 只保存 callee-saved 寄存器 + 栈指针 + 地址空间：caller-saved 由调用约定
/// 由调用方保存；用户现场（含 sepc/sstatus/全部用户寄存器）在陷入时保存在
/// 各任务自己的内核栈陷阱帧里，切换后再经 trap_return 恢复，与 TaskContext
/// 无关。字段顺序与 `switch_to` 汇编中的 sd/ld 偏移严格对应：
///   ra@0x00, s0@0x08 .. s11@0x60, sp@0x68, satp_root@0x70
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskContext {
    pub ra: u64, // x1：恢复地址（任务入口蹦床 / switch 前的返回点）
    pub s0: u64, // x8：帧指针
    pub s1: u64, // x9
    pub s2: u64, // x18
    pub s3: u64, // x19
    pub s4: u64, // x20
    pub s5: u64, // x21
    pub s6: u64, // x22
    pub s7: u64, // x23
    pub s8: u64, // x24
    pub s9: u64, // x25
    pub s10: u64, // x26
    pub s11: u64, // x27
    pub sp: u64, // x2：栈指针
    pub satp_root: u64, // 地址空间根表物理地址（切换时换算 satp 装载值）
}

impl TaskContext {
    /// 全零上下文。
    pub const fn empty() -> Self {
        TaskContext {
            ra: 0, s0: 0, s1: 0, s2: 0, s3: 0, s4: 0, s5: 0, s6: 0,
            s7: 0, s8: 0, s9: 0, s10: 0, s11: 0, sp: 0, satp_root: 0,
        }
    }

    /// 新建线程首次运行的上下文。
    ///
    /// `s0` 存放参数、`s1` 存放入口地址，`ra` 指向 `thread_trampoline`，
    /// 首次调度时由它把参数搬到 a0 并跳转入入口函数。
    /// `satp_root` 为该线程激活的地址空间根表（内核线程传
    /// `vm::kernel_root_addr()`）。
    ///
    /// RISC-V 无 aarch64 的 ELR 抢占陷阱：内核态被抢占时 sepc 指向精确的
    /// 恢复点（下一条指令），sret 必然回到 trampoline 继续执行，不需要
    /// elr 双保险字段。
    pub fn new_thread(entry: fn(usize) -> !, arg: usize, stack_top: usize, satp_root: usize) -> Self {
        TaskContext {
            s0: arg as u64,
            s1: entry as usize as u64,
            ra: thread_trampoline as *const () as usize as u64,
            sp: stack_top as u64,
            satp_root: satp_root as u64,
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
    /// CPU 上限（0..=100，Doze 后台冻结用）：
    ///   - 100：不限（默认）；
    ///   - 1..=99：按比例限速 —— CFS 累加 vruntime 时按 `100/cap` 放大，
    ///     该任务实际获得约 `cap%` 的 CPU 时间（linux cpu.max 思路）；
    ///   - 0：软冻结 —— 保持 Ready 但永不被 `pick_next` 选中（与
    ///     [`TaskState::Stopped`] 的 SIGSTOP 冻结互为补充）。
    pub cpu_cap: u16,
    /// 累计调度 tick 数（`top` 的 CPU 用量列；10ms/tick）。
    pub cpu_ticks: u64,
    /// 一次性调度 deadline（µs 绝对时刻；0 = 无）。
    ///
    /// 「统一响应 deadline」维度（ColorOS 极光引擎思想的自研落地）：输入等
    /// 交互事件到达时给最近读者设置 `now + 一帧` 的期限，`pick_next` 在同
    /// 优先级类内先选带有效期限的任务（按期限早晚），期限在**被选中一次后
    /// 消费清零**——单次授权、有界提升，不改变 CFS 长期公平性，也不会饿死
    /// 后台任务。已过期仍被选中时计一次 miss（`deadline_misses`，帧错过
    /// 归因数据）。
    pub deadline_us: u64,
    /// SMP 终止请求：kill_pid 对**正在他核运行**的任务不直接置 `Exited`
    /// （否则 reap 会在它仍执行时释放 256KB 内核栈 → 跨任务栈损坏），而是
    /// 置此标志；目标核下次 tick（本任务现场已由 IRQ 帧保存完整）兑现为
    /// `Exited` 并切走，栈随后由 reap 回收。
    pub kill_req: bool,
    /// 系统调用上下文深度：`linux_compat::dispatch` 入口 +1、返回前 -1。
    /// 供故障遏制判定——内核 panic / S 态异常发生在深度>0 的任务上时，
    /// 故障源是「代表该用户进程执行 syscall 的内核代码」（坏指针/坏数据），
    /// 只终止该进程而不整机停机。进程在 syscall 内退出时由 `reap_pid`
    /// 的 `Task::empty()` 整槽复位兜底清零。
    pub syscall_depth: u32,
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
            cpu_cap: 100,
            cpu_ticks: 0,
            deadline_us: 0,
            kill_req: false,
            syscall_depth: 0,
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
/// 持锁者 tid（0=无主）。故障遏制出口（kill_pid/exit_current）会重取
/// sched_lock —— 若故障点自身持锁（panic 发生在 tick 的 TASKS/SLEEPERS
/// 临界区内），持有者已死永不释放，遏制路径在关中断下永久自旋（M-1）。
/// 与 fs_lock 同款自愈：等待中发现持有者已死即抢锁接管。
static SCHED_OWNER: AtomicU32 = AtomicU32::new(0);

fn sched_lock() {
    let me = current_tid();
    loop {
        if SCHED_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
        // 死亡持有者自愈：遏制击杀后槽位复用前 tid 不存活
        let holder = SCHED_OWNER.load(Ordering::Acquire);
        if holder != 0 && holder != me && !tid_alive(holder) {
            // 抢锁：把陈旧所有权连同锁位一并接管
            if SCHED_OWNER
                .compare_exchange(holder, me, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                SCHED_LOCK.store(false, Ordering::Release);
                continue;
            }
        }
        core::hint::spin_loop();
    }
    SCHED_OWNER.store(me, Ordering::Release);
}

fn sched_unlock() {
    SCHED_OWNER.store(0, Ordering::Release);
    SCHED_LOCK.store(false, Ordering::Release);
}

/// 改名指定任务（execve 后同步任务名，主线程 tid == pid）。
///
/// 不改名的后果：top / crash 转储永远显示 fork 时的「fork-child」，
/// CPU 热点与崩溃归属排查全部失真。
pub fn rename_task(tid: u32, name: &str) {
    if (tid as usize) >= MAX_TASKS {
        return;
    }
    // 名字竞技场：Task.name 是 &'static str（spawn 传字面量），execve 的
    // 动态名字须落进永久缓冲。每任务 32B，越界截断。
    static mut NAME_ARENA: [[u8; 32]; MAX_TASKS] = [[0; 32]; MAX_TASKS];
    let bytes = name.as_bytes();
    let mut n = bytes.len().min(31);
    // 字符边界回退：字节截断可能落在多字节 UTF-8 字符中间（M-2），
    // from_utf8_unchecked 会构造非法 &str（str API 全体 UB）。
    while n > 0 && (bytes[n - 1] & 0xC0) == 0x80 {
        n -= 1; // 回退连续的 UTF-8 续字节（10xxxxxx）
    }
    let saved = unsafe { read_daif() };
    sched_lock();
    unsafe {
        let slot = &mut NAME_ARENA[tid as usize];
        slot[..n].copy_from_slice(&bytes[..n]);
        slot[n] = 0;
        let t = &mut TASKS[tid as usize];
        if t.state != TaskState::Empty {
            // 指向本任务槽的 NUL 结尾字节切片；缓冲随内核永久存活。
            t.name = core::str::from_utf8_unchecked(&slot[..n]);
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 当前 CPU 号。
///
/// tp 在内核态的不变式是 hartid（启动汇编写入），但 musl 等用户运行时把
/// tp 用作 TLS 指针——用户态陷阱进入内核后 cpu_id() 读到的是用户 TLS
/// 值（巨大）。clamp 到**运行时核数 - 1**（而非编译期 MAX_CPUS - 1）：
/// 单核下任何垃圾值都归 0（唯一核），CURRENT 槽位恒正确。多核下 musl
/// 进程仍可能被误归属（已知局限，见记忆 kunxos-mouse-lag）；根治需在
/// 陷入汇编里重建 tp = hartid（停泊值高 8 位织入核号方案，未完成）。
fn current_cpu() -> usize {
    let cpus = crate::smp::cpu_count().max(1) as usize;
    (crate::smp::cpu_id() as usize).min(cpus - 1)
}

/// 睡眠等待槽位：每个任务同一时间至多登记一个截止时间。
struct Sleeper {
    active: bool,
    tid: u32,
    deadline_us: u64,
}

const EMPTY_SLEEPER: Sleeper = Sleeper { active: false, tid: 0, deadline_us: 0 };
static mut SLEEPERS: [Sleeper; MAX_TASKS] = [const { EMPTY_SLEEPER }; MAX_TASKS];

/// 清除指定 tid 的睡眠登记。任务被杀/回收时必须调用：否则槽位随死亡任务
/// 逐步占满（sleep_until 退化为忙等），且 tid 复用后过期唤醒会错杀新任务
/// （如无限 futex_wait 收到假 ETIMEDOUT）。
///
/// 调用方须持有 sched_lock 且已屏蔽 IRQ。
pub unsafe fn sleepers_clear_tid(tid: u32) {
    for s in SLEEPERS.iter_mut() {
        if s.active && s.tid == tid {
            *s = EMPTY_SLEEPER;
        }
    }
}
static mut LAST_SLICE_US: [u64; MAX_CPUS] = [0; MAX_CPUS];
static mut NEED_RESCHED: [bool; MAX_CPUS] = [false; MAX_CPUS];

/// deadline miss 计数：任务带过期 deadline 被 `pick_next` 选中的次数。
/// 帧错过归因数据（`deadline_misses`），由 shell/perf 读取。
static DEADLINE_MISSES: AtomicU64 = AtomicU64::new(0);

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
    // 上下文切换：a0 = from（保存目标），a1 = to（恢复来源）
    .section .text.switch_to, "ax"
    .global switch_to
    .type switch_to, %function
switch_to:
    // 保存 callee-saved（ra + s0-s11）与 sp 到 from
    sd ra, 0x00(a0)
    sd s0, 0x08(a0)
    sd s1, 0x10(a0)
    sd s2, 0x18(a0)
    sd s3, 0x20(a0)
    sd s4, 0x28(a0)
    sd s5, 0x30(a0)
    sd s6, 0x38(a0)
    sd s7, 0x40(a0)
    sd s8, 0x48(a0)
    sd s9, 0x50(a0)
    sd s10, 0x58(a0)
    sd s11, 0x60(a0)
    sd sp, 0x68(a0)
    // 从 to 恢复
    ld ra, 0x00(a1)
    ld s0, 0x08(a1)
    ld s1, 0x10(a1)
    ld s2, 0x18(a1)
    ld s3, 0x20(a1)
    ld s4, 0x28(a1)
    ld s5, 0x30(a1)
    ld s6, 0x38(a1)
    ld s7, 0x40(a1)
    ld s8, 0x48(a1)
    ld s9, 0x50(a1)
    ld s10, 0x58(a1)
    ld s11, 0x60(a1)
    ld sp, 0x68(a1)
    // 地址空间切换：satp = Sv39 | root>>12，随后全量 TLB 失效
    ld t0, 0x70(a1)
    srli t0, t0, 12
    li t1, 0x8000000000000000
    or t0, t0, t1
    csrw satp, t0
    sfence.vma
    // 跳转到恢复后的 ra（目标任务）
    ret

    // 线程入口蹦床：s0 = 参数，s1 = 入口函数
    .section .text.thread_trampoline, "ax"
    .global thread_trampoline
    .type thread_trampoline, %function
thread_trampoline:
    // 首次运行：释放调度锁（切换路径在 switch 前获取、由被切到任务的
    // 恢复点释放；首次运行任务的恢复点是本蹦床而非某个 switch 路径，
    // 必须在此补齐解锁，否则调度锁永久持有 → 后续 block/yield 死锁）。
    call sched_unlock_from_asm
    // 使能 IRQ（首次被调度进入时处于中断屏蔽态）
    csrsi sstatus, 2
    // 把参数搬到 a0，跳转入入口（entry 为 fn(usize) -> !，不返回）
    mv a0, s0
    jr s1
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

/// 为任务 `cur` 累加 `delta` vruntime，带 u64 溢出防护。
///
/// CFS 长期运行（尤其 `cpu_cap` 低时增量放大 100 倍）会让 vruntime 逼近
/// u64 上限：一旦回绕，该任务 vruntime 突然变成全局最小 → 独占 CPU 直到
/// 其它任务也回绕（公平性彻底翻转）。累加前若溢出在即，先做「rebase」：
/// 全体非空任务减去当前最小值（等价 CFS 归一化，相对公平不变），把数值域
/// 拉回安全区后再累加。
fn vruntime_accrue(cur: usize, delta: u64) {
    unsafe {
        if TASKS[cur].vruntime > u64::MAX - delta {
            let mut mn = u64::MAX;
            for i in 0..MAX_TASKS {
                if TASKS[i].state != TaskState::Empty && TASKS[i].state != TaskState::Exited {
                    mn = mn.min(TASKS[i].vruntime);
                }
            }
            if mn != u64::MAX {
                for i in 0..MAX_TASKS {
                    if TASKS[i].state != TaskState::Empty && TASKS[i].state != TaskState::Exited {
                        TASKS[i].vruntime = TASKS[i].vruntime.wrapping_sub(mn);
                    }
                }
            }
        }
        TASKS[cur].vruntime = TASKS[cur].vruntime.wrapping_add(delta);
    }
}

/// 唤醒抢占检查（调用方须持 sched_lock）：被唤醒任务若为实时类、或其
/// vruntime 比当前运行任务落后超过唤醒抢占粒度，则置 NEED_RESCHED。
///
/// 实际切换发生在：IRQ 上下文由 `irq_handler` 尾部的 `resched_if_needed`
/// 兑现（输入/设备唤醒路径）；syscall 上下文由当前任务随后的阻塞/让出
/// （`block_current`/`yield_now` 的 pick_next）或下一个定时器 tick 兑现。
fn wakeup_preempt(woken: usize) {
    unsafe {
        let cpu = current_cpu();
        let cur = CURRENT[cpu];
        if cur >= MAX_TASKS || cur == woken {
            return;
        }
        // 带 deadline 的唤醒（交互事件跟随）无条件请求抢占：期限语义下
        // 不应再被当前任务的 vruntime 余量推迟一拍。
        if class_rank(TASKS[woken].class) < class_rank(TASKS[cur].class)
            || TASKS[woken].deadline_us != 0
            || TASKS[woken].vruntime + WAKEUP_GRAN_NS < TASKS[cur].vruntime
        {
            NEED_RESCHED[cpu] = true;
        }
    }
}

/// 输入触控加速（CFS `place_entity` 唤醒置位思路）：把指定任务（最近读取
/// 输入设备的 UI 任务）的 vruntime 拉到全局最小值，使其成为下一个最应
/// 调度的任务，并对当前任务发起唤醒抢占检查 —— 指针/按键事件到达后，
/// 前台应用立即抢占后台负载，输入→画面延迟不再包含「等后台任务用完
/// 时间片」。供输入栈在设备 IRQ 上下文调用（IRQ 已屏蔽）；限频由调用方
/// 负责（一次加速授予一个完整时间片的调度优待）。
pub fn boost_tid(tid: u32) {
    let t = tid as usize;
    if t >= MAX_TASKS {
        return;
    }
    // ★ IRQ 上下文（输入加速）try-lock：锁被任务上下文持有时被打断者
    // 永远无法释放 → EL1 自旋死锁（整机静默冻结）。boost 是纯优化，
    // 竞争即放弃（下一个输入事件会重试）。
    if !SCHED_LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    unsafe {
        if TASKS[t].state != TaskState::Empty && TASKS[t].state != TaskState::Exited {
            TASKS[t].vruntime = min_vruntime();
            wakeup_preempt(t);
        }
    }
    sched_unlock();
}

/// 为指定任务设置一次性调度 deadline（µs 绝对时刻；0 = 清除）。
///
/// 供输入栈在设备 IRQ 上下文（IRQ 已屏蔽）调用：交互事件到达后，最近
/// 读者在下一个调度决策即被优先选中，直到其首次被调度（消费）或被更高
/// 优先级类抢占。与 [`boost_tid`] 配对使用：boost 保证 vruntime 全局最小，
/// deadline 保证期限语义（即使中途阻塞再唤醒也保持优先）。
///
/// IRQ 屏蔽与 spawn 同模式：本函数也可经 syscall 414 从进程上下文调用，
/// 持锁窗口必须屏蔽 tick 抢占（否则 tick 在 IRQ 上下文自旋等锁、被中断
/// 任务永远无法释放 → 单核死锁）。
pub fn set_tid_deadline(tid: u32, deadline_us: u64) {
    let t = tid as usize;
    if t >= MAX_TASKS {
        return;
    }
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        if TASKS[t].state != TaskState::Empty && TASKS[t].state != TaskState::Exited {
            TASKS[t].deadline_us = deadline_us;
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 当前任务为自己设置调度 deadline（syscall 414）：`rel_us` 为相对期限
/// （µs，0 = 清除，上限 1s 防滥用）。渲染线程可在处理输入事件前设置
/// 「本帧期限」，把帧预算显式告知调度器。成功返回 0。
pub fn set_current_deadline(rel_us: u64) -> i64 {
    let rel = rel_us.min(1_000_000);
    let dl = if rel == 0 { 0 } else { timer::now_us().saturating_add(rel) };
    set_tid_deadline(current_tid(), dl);
    0
}

/// 累计 deadline miss 次数（带过期 deadline 被选中的次数；帧错过归因）。
pub fn deadline_misses() -> u64 {
    DEADLINE_MISSES.load(Ordering::Relaxed)
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
/// 物理内存耗尽返回 `None`（调用方把 fork/spawn 失败上抛，而非 panic——
/// 此路径可被用户 fork/clone 风暴直接触发）。
pub fn alloc_stack() -> Option<usize> {
    let base = alloc_pages_contig(STACK_PAGES)?;
    // 用 0xA5 填充整个任务栈：内核栈越界写会破坏该模式，供故障诊断检测溢出
    unsafe {
        for p in 0..STACK_PAGES {
            core::ptr::write_bytes((base + p * 4096) as *mut u8, 0xA5, 4096);
        }
    }
    Some(base + STACK_SIZE)
}

/// 挑选下一个要运行的任务。
///
/// 排序键（依次比较）：优先级类 → 一次性 deadline → vruntime。
/// 同类内带有效 deadline 的任务先于无 deadline 任务（按期限早晚排序）；
/// 都无 deadline 时退化为原 CFS vruntime 语义。deadline 在被选中时消费
/// 清零：单次授权、有界提升——交互任务在下一调度决策即被优先，同时不会
/// 持续饿死后台负载（CFS 长期公平性不变）。
///
/// 无就绪任务时返回 `None`（调用方回退到本核 idle 任务）。
fn pick_next() -> Option<usize> {
    let mut best = usize::MAX;
    let mut best_rank = u8::MAX;
    let mut best_dl = 0u64;
    let mut best_vr = u64::MAX;
    let my_idle = idle_slot();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].state != TaskState::Ready {
                continue;
            }
            // ★ SMP：带 kill_req 的 Ready 任务（被标记时在他核运行、随后
            // 阻塞又唤醒）不再拉回运行——在此兑现为 Exited（与 tick 路径
            // 同语义），否则其将在 execve 替换后的地址空间上执行旧映像 PC。
            if TASKS[i].kill_req {
                TASKS[i].kill_req = false;
                TASKS[i].state = TaskState::Exited;
                continue;
            }
            // 软冻结（cpu_cap == 0）的任务保持 Ready 但永不被选中。
            if TASKS[i].cpu_cap == 0 {
                continue;
            }
            // 其他核的 idle 任务不可被本核选中（每核只运行自己的 idle）
            if TASKS[i].class == PriorityClass::Idle && i != my_idle {
                continue;
            }
            let rank = class_rank(TASKS[i].class);
            let dl = TASKS[i].deadline_us;
            let vr = TASKS[i].vruntime;
            let better = rank < best_rank
                || (rank == best_rank
                    && (if best_dl != 0 {
                        dl != 0 && dl < best_dl
                    } else {
                        dl != 0 || vr < best_vr
                    }));
            if better {
                best = i;
                best_rank = rank;
                best_dl = dl;
                best_vr = vr;
            }
        }
        // 消费 deadline：期限已过仍被选中 = 一次帧错过（归因计数）
        if best != usize::MAX && best_dl != 0 {
            if timer::now_us() > best_dl {
                DEADLINE_MISSES.fetch_add(1, Ordering::Relaxed);
            }
            TASKS[best].deadline_us = 0;
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
///
/// 同时承担电源管理记账：测量 WFI 驻留时长（供 power 模块计算 CPU 负载
/// 与空闲比例），并驱动 idle 状态机（enter/exit_wfi）。
fn idle_loop(cpu: usize) -> ! {
    loop {
        // 幂等重开总中断：故障遏制把任务标记退出后切换到本核 idle 时，
        // 恢复点可能带着关中断状态（故障陷阱屏蔽了 SIE，TaskContext 不
        // 保存 sstatus）——不重开则本核定时器不再陷入，心跳停跳会被互监
        // 看门狗误判为楔死而整机复位。每轮循环一次 csrsi，开销可忽略。
        unsafe { core::arch::asm!("csrsi sstatus, 2") };
        let t0 = timer::now_us();
        crate::power::enter_wfi(cpu as u32);
        unsafe { wfi() };
        // WFI 被中断唤醒（定时器 / 设备 IRQ）。若 IRQ 处理中发生了任务
        // 切换，本行要等 idle 任务再次被调度才执行 —— 记账仍正确。
        let t1 = timer::now_us();
        crate::power::exit_wfi(cpu as u32, t1.saturating_sub(t0));
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
        // idle 任务栈：启动期一次性分配，失败即无法调度，panic 可接受。
        let stack_top = alloc_stack().expect("sched_cfs：空闲任务栈分配失败");
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
    // ★ 调度锁必须在 IRQ 屏蔽下获取：sched_lock 之后跟着 alloc_stack()（256KB
    // 逐字节填充，耗时数百微秒），若 IRQ 使能，tick 在 IRQ 上下文 sched_lock
    // 自旋 → 被中断的 spawn 永远无法释放锁 → 单核硬死锁（audio ioctl 等路径
    // 在 IRQ 使能下调用本函数）。保存-恢复 DAIF，与 set_current_nice 同模式。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    // 先取槽位再分配栈：槽位不足时不浪费物理页
    sched_lock();
    let Some(tid) = alloc_tid() else { sched_unlock(); unsafe { write_daif(saved) }; return u32::MAX };
    // 任务栈 OOM：明确失败（此前 panic，fork/clone 风暴可打挂内核）。
    let Some(stack_top) = alloc_stack() else { sched_unlock(); unsafe { write_daif(saved) }; return u32::MAX };
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
        t.deadline_us = 0;
        t.kill_req = false;
    }
    sched_unlock();
    unsafe { write_daif(saved) };
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
    // ★ 同 spawn：调度锁必须在 IRQ 屏蔽下获取（防 tick 自旋死锁）；
    // 且 fork_parent 在 IRQ 屏蔽临界区内调用本函数，保持调用方 IRQ 状态。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let Some(tid) = alloc_tid() else { sched_unlock(); unsafe { write_daif(saved) }; return u32::MAX };
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
        t.deadline_us = 0;
        t.kill_req = false;
    }
    sched_unlock();
    unsafe { write_daif(saved) };
    tid as u32
}

/// 读取当前任务下标；调度器尚未启动（`CURRENT` 仍为哨兵值 `usize::MAX`）时
/// 返回 `None`。
///
/// 所有读取 `CURRENT` 的地方都应经此函数，避免在调度器启动前以 `usize::MAX`
/// 下标访问任务数组触发越界 panic（index out of bounds）。
pub fn current_idx() -> Option<usize> {
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

/// 指定 tid 的任务是否仍然存活（非 Empty/Exited）。
///
/// 供内核锁做「死亡持有者抢锁自愈」判断（fs_simple::fs_lock 等）：
/// tid 不同且已死亡 → 持有者不会再来释放，允许夺回。
pub fn tid_alive(tid: u32) -> bool {
    let t = tid as usize;
    if t >= MAX_TASKS {
        return false;
    }
    unsafe {
        TASKS[t].state != TaskState::Empty && TASKS[t].state != TaskState::Exited
    }
}

/// 当前任务所属进程 pid。
pub fn current_pid() -> u32 {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].pid },
        None => 0,
    }
}

// ==================== 系统调用上下文标记（故障遏制） ====================

/// 标记当前任务进入系统调用上下文（`linux_compat::dispatch` 入口调用）。
///
/// 内核 panic / S 态异常发生时，`in_syscall_user` 据此判断故障是否发生在
/// 「代表用户进程执行 syscall」的内核代码里（用户传坏指针/坏数据触发），
/// 是则只终止该进程（SIGSEGV 语义），系统继续调度。
pub fn syscall_enter() {
    if let Some(i) = current_idx() {
        unsafe { TASKS[i].syscall_depth = TASKS[i].syscall_depth.saturating_add(1) };
    }
}

/// 离开系统调用上下文（`linux_compat::dispatch` 返回前调用）。
pub fn syscall_leave() {
    if let Some(i) = current_idx() {
        unsafe {
            if TASKS[i].syscall_depth > 0 {
                TASKS[i].syscall_depth -= 1;
            }
        }
    }
}

/// 当前任务是否是**可调度的内核线程**（非 idle、占用任务槽位、不在
/// syscall 上下文）——其故障可只杀线程保全系统（kthread_fault_contain）。
/// 必须排除用户任务：中断打断用户态代码（depth==0）的用户任务同样满足
/// 前三条，误入只杀线程会把进程孤儿化（不置 Zombie、父进程永久阻塞、
/// PCB 槽泄漏）—— 以「进程无用户地址空间」区分内核线程（H-2 另一半）。
pub fn is_sched_kthread() -> bool {
    match current_idx() {
        Some(i) => unsafe {
            TASKS[i].state != TaskState::Empty
                && TASKS[i].class != PriorityClass::Idle
                && TASKS[i].syscall_depth == 0
                && crate::process::processes_pub_ttbr0(TASKS[i].pid) == 0
        },
        None => false,
    }
}

/// 当前任务是否处于系统调用上下文，且不是内核任务（排除 idle）。
///
/// idle / 内核线程不带用户地址空间，其上的故障无法归咎于用户进程，
/// 遏制判定须配合 `process::processes_pub_ttbr0(pid) != 0` 使用。
pub fn in_syscall_user() -> bool {
    match current_idx() {
        Some(i) => unsafe {
            TASKS[i].syscall_depth != 0 && TASKS[i].class != PriorityClass::Idle
        },
        None => false,
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
    // 调度锁须在 IRQ 屏蔽下获取：syscall 上下文 IRQ 使能，持锁期间定时器
    // tick 抢锁会自旋死锁。保存-恢复 DAIF（read_daif/write_daif 可嵌套，
    // 调用方已屏蔽时不会错误重开中断）。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    if let Some(cur) = current_idx() {
        unsafe {
            let t = &mut TASKS[cur];
            t.nice = nice.clamp(-5, 5);
            t.weight = weight_of(t.nice);
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 当前任务的优先级类。
pub fn current_class() -> PriorityClass {
    match current_idx() {
        Some(i) => unsafe { TASKS[i].class },
        None => PriorityClass::Idle,
    }
}

/// 提升当前任务到实时类（SCHED_FIFO 等价物）：音频服务等周期性、对调度
/// 抖动敏感的任务使用。实时类任务不参与 CFS 时间片轮转，仅在主动让出
/// （yield/阻塞/睡眠）时切换，保证音频混音/写入线程的稳定周期。
pub fn set_current_class(class: PriorityClass) {
    // 同 `set_current_nice`：调度锁须在 IRQ 屏蔽下获取（syscall 上下文 IRQ 使能）
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    if let Some(cur) = current_idx() {
        unsafe {
            TASKS[cur].class = class;
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
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
            crate::uart_send_str(" ra=");
            crate::print_hex(t.context.ra);
            crate::uart_send_str(" satp_root=");
            crate::print_hex(t.context.satp_root);
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
        TaskState::Stopped => 5,
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
    // 内存压力状态机（每 100ms 节流）：等级变化时打印内核日志，
    // 供 lifd 经 SYS_LC_MEMPRESSURE / /proc/pressure 读取并执行挂起/LMK。
    crate::mem_pressure::poll(now);
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
                // ★ 仅在真正唤醒（目标已 Blocked）时才消费槽位：目标尚在
                // 「登记后、阻塞前」窗口（SMP 下他核 tick 可见）时保留槽位，
                // 待其阻塞后的下个 tick 唤醒，否则丢唤醒 → 永久阻塞。
                s.active = false;
            }
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
        // ★ SMP 终止请求兑现：kill_pid 对他核正在运行的任务只置 kill_req、
        // 不置 Exited（否则 reap 会在其未退出时释放内核栈）。此处（本核 IRQ
        // 上下文，当前任务现场已由中断帧完整保存）把 kill_req 兑现为 Exited，
        // 下方 pick_next 会跳过它并 do_switch 切走——语义与 exit_current 相同，
        // 栈由后续 reap_pid 回收。
        if TASKS[cur].kill_req {
            TASKS[cur].kill_req = false;
            TASKS[cur].state = TaskState::Exited;
        }
        // [perf] CPU 占用统计：idle 任务 tick vs 忙任务 tick（性能 overlay 数据源）
        crate::perf::note_cpu_tick(TASKS[cur].class == PriorityClass::Idle);
        // [top] 每任务累计调度 tick 计数（10ms/tick）
        TASKS[cur].cpu_ticks += 1;
        // CPU 上限（Doze）：cap<100 时按 100/cap 放大 vruntime 增量，
        // 该任务实际获得约 cap% 的 CPU 时间（linux cpu.max 思路）。
        let w = TASKS[cur].weight.max(1) as u64;
        let cap = TASKS[cur].cpu_cap.max(1) as u64;
        vruntime_accrue(cur, TIME_SLICE * WEIGHT_BASE as u64 * 100 / (w * cap));
        if TASKS[cur].state == TaskState::Running {
            TASKS[cur].state = TaskState::Ready;
        }
    }
    let next = match pick_next() {
        Some(n) => n,
        None => {
            // ★ 不把 Exited 拉回 Running：kill_req 刚在此兑现为 Exited 的任务
            // 若被本分支复活，kill_pid 的终止请求即被撤销（任务永生）。留在
            // Exited 的任务继续执行完当前内核路径，最迟下个 tick 被切走后
            // 由 reap 回收（本核 idle 恒就绪，本分支实际极少触达）。
            unsafe {
                if TASKS[cur].state != TaskState::Exited {
                    TASKS[cur].state = TaskState::Running;
                }
            }
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
    // 保存进入时 DAIF：此前入口无条件 daifset 且所有返回路径都不恢复，
    // 调用方从此永久运行在关中断态（fs_lock 退避把整个后续 FS 临界区拖进
    // 关中断窗口；perflog 线程第一次睡眠后恒关中断）。与 yield_now 同模式
    // 保存/恢复。
    let saved_daif = unsafe { read_daif() };
    let tid = current_tid();
    let mut registered = false;
    unsafe { asm!("csrci sstatus, 2") };
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
        // 醒来后清掉本任务可能残留的睡眠槽位：提前唤醒（IPC/设备）时 tick
        // 不消费槽位（目标非 Blocked），由这里自清，防槽位泄漏或过期唤醒。
        sched_lock();
        unsafe {
            for s in SLEEPERS.iter_mut() {
                if s.active && s.tid == tid {
                    s.active = false;
                }
            }
        }
        sched_unlock();
        // 恢复进入时的中断状态（阻塞前屏蔽是临界区需要，返回调用方时
        // 必须回到原状态）。
        unsafe { write_daif(saved_daif) };
    } else {
        // 槽位数量与任务数相同，正常情况下不会到达；保留可工作的降级路径。
        while timer::now_us() < deadline_us {
            yield_now();
        }
        unsafe { write_daif(saved_daif) };
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
                // 同 tick：仅真正唤醒才消费槽位（防「登记后、阻塞前」窗口丢唤醒），
                // 未阻塞时保留槽位，由 sleep_until 醒来后自清或到期 tick 消费。
                s.active = false;
            }
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
    asm!("csrr {}, sstatus", out(reg) d);
    d
}

/// 写回 DAIF（恢复中断状态）。
#[inline]
pub unsafe fn write_daif(d: u64) {
    asm!("csrw sstatus, {}", in(reg) d);
}

/// 主动让出 CPU（累加一个时间片的 vruntime 后再切换，保证公平轮转）。
///
/// 切换前后**保存-恢复调用方的 DAIF**：syscall 上下文（IRQ 屏蔽）调用时
/// 返回后保持屏蔽，普通内核上下文调用时返回后恢复使能，二者都正确。
#[inline(never)]
pub fn yield_now() {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe {
        // 此处已做出调度决策（pick_next），此前累积的唤醒抢占请求随之兑现，
        // 清除标志避免被唤醒任务就绪后再被无谓抢占一次。
        NEED_RESCHED[cpu] = false;
        // CPU 上限（Doze）：与 tick 一致，让出也按 cap 放大 vruntime。
        let w = TASKS[cur].weight.max(1) as u64;
        let cap = TASKS[cur].cpu_cap.max(1) as u64;
        vruntime_accrue(cur, TIME_SLICE * WEIGHT_BASE as u64 * 100 / (w * cap));
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
    // 被再次调度后恢复进入时的 DAIF（此前无条件 daifclr：屏蔽态调用者
    // （如 futex_wait 在阻塞前压 IRQ）恢复后中断被错误重开，破坏嵌套屏蔽）。
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 阻塞当前任务（调用者须先记录阻塞原因），直到被 `wake` 唤醒。
///
/// 返回后恢复调用前的 DAIF（同 `yield_now`）：syscall 上下文保持屏蔽，
/// 直到 eret 到 EL0 才由硬件使能中断。
#[inline(never)]
pub fn block_current() {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe {
        // 调度决策点：清除累积的唤醒抢占请求（同 yield_now）。
        NEED_RESCHED[cpu] = false;
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
    unsafe { write_daif(saved) };
}

/// 停止当前任务（SIGSTOP 语义）：置为 `Stopped` 并切走，直到
/// [`cont_pid`]（SIGCONT）唤醒。DAIF 处理与 `block_current` 一致：
/// 保存调用方 DAIF → 屏蔽 IRQ 执行切换 → 被再次调度后恢复中断。
///
/// 与 `block_current` 的区别：Stopped 不被 `wake`/`wake_pid`（IPC 唤醒）
/// 唤醒，只能由 SIGCONT 显式恢复 —— 保证后台应用不会被管道/消息队列的
/// 偶发写入"意外唤醒"。
#[inline(never)]
pub fn stop_current() {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return;
    }
    unsafe {
        // 调度决策点：清除累积的唤醒抢占请求（同 yield_now）。
        NEED_RESCHED[cpu] = false;
        TASKS[cur].state = TaskState::Stopped;
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
    unsafe { write_daif(saved) };
}

/// 唤醒属于指定进程的全部已停止任务（SIGCONT 语义），置为就绪。
/// 调用方须已屏蔽 IRQ（`sys_kill` 路径）。
pub fn cont_pid(pid: u32) {
    // 调度锁须在 IRQ 屏蔽下获取（sys_kill → syscall 上下文 IRQ 使能）。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Stopped {
                TASKS[i].state = TaskState::Ready;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 调整指定进程所有任务的 nice 值（`setpriority(pid, prio)` 内核后端）。
///
/// nice 越负权重越大（CFS 份额越高）：前台应用 −3（权重 2000 ≈ 普通 2 倍），
/// 后台/挂起应用 +2（权重 ~550）。供 lifd 按应用状态调整调度优待，
/// 与 CFS 语义完全兼容（不替换调度器）。root 可改任意 pid。
pub fn set_pid_nice(pid: u32, nice: i32) {
    if (pid as usize) >= MAX_PROC {
        return;
    }
    // 调度锁须在 IRQ 屏蔽下获取（sys_setpriority → syscall 上下文 IRQ 使能）。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid {
                let t = &mut TASKS[i];
                t.nice = nice.clamp(-5, 5);
                t.weight = weight_of(t.nice);
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 设置指定进程所有任务的 CPU 上限（Doze 后台冻结）。
///
/// `cap` 0..=100：0 = 软冻结（保持 Ready 但不调度），100 = 不限，
/// 其余按比例限速。进程不存在返回 `-1`（ESRCH 语义），成功返回 0。
pub fn set_task_cpu_cap(pid: u32, cap: u16) -> i32 {
    if (pid as usize) >= MAX_PROC {
        return -1;
    }
    let cap = cap.min(100);
    let mut found = false;
    // 调度锁须在 IRQ 屏蔽下获取（SYS_KUNX_PM_* → syscall 上下文 IRQ 使能）
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state != TaskState::Empty {
                TASKS[i].cpu_cap = cap;
                found = true;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
    if found {
        0
    } else {
        -1
    }
}

/// 读取指定进程的 CPU 上限（取该进程首个非空任务；无则 100）。
pub fn task_cpu_cap(pid: u32) -> u16 {
    if (pid as usize) >= MAX_PROC {
        return 100;
    }
    // 同 set_task_cpu_cap：调度锁须在 IRQ 屏蔽下获取
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let cap = unsafe {
        let mut found = 100u16;
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state != TaskState::Empty {
                found = TASKS[i].cpu_cap;
                break;
            }
        }
        sched_unlock();
        found
    };
    unsafe { write_daif(saved) };
    cap
}

/// 遍历任务表（含 CPU 上限与冻结标志）：供 /proc/power 展示 Doze 状态。
/// 回调参数 `(pid, cpu_cap, frozen)`，`frozen` = SIGSTOP 冻结中。
pub fn for_each_task_cap(mut f: impl FnMut(u32, u16, bool)) {
    unsafe {
        for i in 0..MAX_TASKS {
            let t = &TASKS[i];
            if t.state == TaskState::Empty {
                continue;
            }
            f(t.pid, t.cpu_cap, t.state == TaskState::Stopped);
        }
    }
}

/// 唤醒指定任务（置为就绪）。仅唤醒 `Blocked`（IPC 阻塞）任务；
/// `Stopped`（SIGSTOP）任务只能由 `cont_pid` 恢复。
pub fn wake(tid: u32) {
    if (tid as usize) < MAX_TASKS {
        // 保存-恢复 DAIF（而非无条件 daifclr）：从屏蔽中断的上下文调用时
        // 不得在返回后错误重开中断（与 wake_pid 同源）。
        let saved = unsafe { read_daif() };
        unsafe { asm!("csrci sstatus, 2") };
        sched_lock();
        unsafe {
            if TASKS[tid as usize].state == TaskState::Blocked {
                TASKS[tid as usize].state = TaskState::Ready;
                // 唤醒抢占检查：交互/实时任务唤醒后不必等当前任务用完时间片
                wakeup_preempt(tid as usize);
            }
        }
        sched_unlock();
        unsafe { write_daif(saved) };
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
            TASKS[cur].context.satp_root = ttbr0 as u64;
        }
        vm::write_ttbr0_el1(ttbr0);
    }
}

/// 唤醒属于指定进程的所有阻塞任务（供 exit 唤醒 wait 中的父进程等）。
///
/// 与 [`wake_irq`] 一致：调用方须已屏蔽 IRQ，此处不操作 DAIF。
pub fn wake_pid(pid: u32) {
    // 调度锁须在 IRQ 屏蔽下获取：调用方（sys_kill / process::exit 等）上下文
    // IRQ 可能是使能的，持锁期间定时器 tick 抢锁会自旋死锁。保存-恢复 DAIF，
    // 调用方已屏蔽时不会错误重开中断。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Blocked {
                TASKS[i].state = TaskState::Ready;
                // 唤醒抢占检查（同 wake）
                wakeup_preempt(i);
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 终止属于指定进程的所有任务（标记为 Exited，不再被调度）。
///
/// 供进程退出时使用：多线程进程（CLONE_THREAD 共享地址空间）退出时必须先
/// 终止兄弟线程，否则它们会在父进程 reap 释放页表后继续以悬空 TTBR0 运行。
/// 调用方须已屏蔽 IRQ。
pub fn kill_pid(pid: u32) {
    // 调度锁须在 IRQ 屏蔽下获取（sys_kill / process::exit 路径）。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid != pid || TASKS[i].state == TaskState::Empty || TASKS[i].state == TaskState::Exited {
                continue;
            }
            // 终止该线程的同时清掉其 futex 等待项（槽位不随线程死亡清空，
            // 32 个槽位会被死亡线程逐步占满 → 后续 futex_wait 全返 EAGAIN）。
            crate::bionic_shim::futex_cleanup_tid(TASKS[i].tid);
            // 同理：清掉其持有的全局网络锁，否则持锁任务死亡后所有联网
            // 操作在锁退避中无限等待。
            crate::net_stack::net_cleanup_tid(TASKS[i].tid);
            // 同理：清掉其睡眠/超时登记（nanosleep / 定时 futex_wait）。
            sleepers_clear_tid(TASKS[i].tid);
            // 同理：清 CLONE_CHILD_CLEARTID 登记（tid 复用后新任务退出会向
            // 陈旧用户地址清零 + 误触 futex）。
            if TASKS[i].state == TaskState::Running {
                // ★ SMP：正在某核运行的任务不能就地置 Exited——reap 会把它还
                // 在使用的 256KB 内核栈释放并复用，导致他核继续在已归还的栈上
                // 执行（跨任务栈损坏）。置 kill_req，由该核下次 tick 兑现退出。
                TASKS[i].kill_req = true;
            } else {
                TASKS[i].state = TaskState::Exited;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 终止指定进程除当前任务外的全部线程（execve 专用）。
///
/// 旧地址空间即将被释放，兄弟线程若继续运行会以悬空 TTBR0 调度（与 exit
/// 路径的 kill_pid 同族缺陷）；但 execve 调用者自身必须继续运行，不能对
/// 当前任务置 kill_req（否则下次 tick 误杀新映像）。
pub fn kill_siblings_of(pid: u32) {
    // 调度锁须在 IRQ 屏蔽下获取（与 kill_pid 同模式）。
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let cur = unsafe { CURRENT[current_cpu()] };
    unsafe {
        for i in 0..MAX_TASKS {
            if i == cur {
                continue;
            }
            if TASKS[i].pid != pid
                || TASKS[i].state == TaskState::Empty
                || TASKS[i].state == TaskState::Exited
            {
                continue;
            }
            crate::bionic_shim::futex_cleanup_tid(TASKS[i].tid);
            crate::net_stack::net_cleanup_tid(TASKS[i].tid);
            sleepers_clear_tid(TASKS[i].tid);
            crate::bionic_shim::clear_tid_slot(TASKS[i].tid);
            if TASKS[i].state == TaskState::Running {
                // SMP 兜底：他核正在运行的任务置 kill_req，由该核下次 tick 兑现。
                TASKS[i].kill_req = true;
            } else {
                TASKS[i].state = TaskState::Exited;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };

    // ★ SMP 静默等待：他核 Running 的兄弟此刻只带了 kill_req，须等其在
    // 所在核 tick 兑现并切走（离开 Running）后，execve 才能释放旧地址空
    // 间（replace_address_space 归还旧页表）——否则兄弟在最长一个 tick
    // 周期（10ms）内仍执行在旧页表上（use-after-free）。被标记后阻塞/
    // 唤醒的兄弟由 pick_next 的 kill_req 兑现兜住，不会再回到 Running。
    // 上界 1s（正常数十 ms 内静默）：超时说明他核异常，打印诊断后继续
    // （退化为旧行为，好过 execve 永久挂死）。
    let deadline = crate::timer::now_us() + 1_000_000;
    loop {
        let saved2 = unsafe { read_daif() };
        unsafe { asm!("csrci sstatus, 2") };
        sched_lock();
        let busy = unsafe {
            (0..MAX_TASKS).any(|i| {
                i != cur && TASKS[i].pid == pid && TASKS[i].state == TaskState::Running
            })
        };
        sched_unlock();
        unsafe { write_daif(saved2) };
        if !busy {
            return;
        }
        if crate::timer::now_us() > deadline {
            crate::kinfo! {
                crate::uart_send_str("[smp] kill_siblings_of pid=");
                crate::print_dec(pid as u64);
                crate::uart_send_str(" 兄弟静默超时（他核未兑现 kill_req），继续\n");
            }
            return;
        }
        sleep_until(crate::timer::now_us() + 1000);
    }
}

/// 回收属于指定进程的全部已退出任务：复位 TCB 槽位并归还 64KB 内核栈。
///
/// 由父进程 `wait`/`reap` 在目标进程已 Zombie（其全部线程均已 Exited）后调用；
/// 内部屏蔽 IRQ 并持调度锁保护任务表。
pub fn reap_pid(pid: u32) {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    unsafe {
        // ★ 护栏：该进程仍有存活任务（Running/Ready/Blocked/Stopped，含
        // 他核 kill_req 待兑现）时拒绝回收——否则会归还仍在执行任务的内核栈
        // （跨任务栈损坏，SMP 竞态）。kill_pid 已把非运行任务全部置 Exited，
        // 单核下剩余的「存活」任务只可能是当前任务自身，而父进程必然在其
        // 切走后才运行 reap，因此正常流程此护栏恒不触发（SMP 兜底）。
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid
                && TASKS[i].state != TaskState::Empty
                && TASKS[i].state != TaskState::Exited
            {
                sched_unlock();
                // 恢复进入前 DAIF（不得无条件重开中断）
                write_daif(saved);
                return;
            }
        }
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Exited {
                let base = TASKS[i].stack_base;
                if base != 0 {
                    for p in 0..STACK_PAGES {
                        allocator::free_page(base + p * allocator::PAGE_SIZE);
                    }
                }
                // 收尾：清掉可能残留的睡眠登记（兜底 kill 路径的遗漏）
                sleepers_clear_tid(TASKS[i].tid);
                    TASKS[i] = Task::empty();
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
}

/// 判断进程是否仍有存活（非 Empty/Exited）任务 —— SMP 下 reap 前的硬护栏。
///
/// 供 process::reap 在释放地址空间/页表**之前**调用：目标若仍被某核执行，
/// 先释放其页表 = 他核任务 use-after-free。
pub fn has_live_tasks(pid: u32) -> bool {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let mut live = false;
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid
                && TASKS[i].state != TaskState::Empty
                && TASKS[i].state != TaskState::Exited
            {
                live = true;
                break;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
    live
}

/// 判断进程是否处于 SIGSTOP 停止态（其任一任务为 `Stopped`）。
/// 供 procfs /proc/processes 与 lifd 诊断显示。
pub fn process_stopped(pid: u32) -> bool {
    // 同 wake_pid：调度锁须在 IRQ 屏蔽下获取
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let mut stopped = false;
    unsafe {
        for i in 0..MAX_TASKS {
            if TASKS[i].pid == pid && TASKS[i].state == TaskState::Stopped {
                stopped = true;
                break;
            }
        }
    }
    sched_unlock();
    unsafe { write_daif(saved) };
    stopped
}

/// 查询指定任务是否处于 `Blocked`（futex 唤醒前核对：仅当目标确实阻塞才
/// 消费等待槽位，防「清槽但目标未阻塞」丢唤醒）。调度锁在 IRQ 屏蔽下获取。
pub fn is_blocked(tid: u32) -> bool {
    if (tid as usize) >= MAX_TASKS {
        return false;
    }
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    sched_lock();
    let r = unsafe { TASKS[tid as usize].state == TaskState::Blocked };
    sched_unlock();
    unsafe { write_daif(saved) };
    r
}

/// 退出当前任务（不再返回）。
///
/// SMP：调度锁在切换后由被切到的任务恢复点释放（锁交接），本任务已
/// Exited，不再返回。
#[inline(never)]
pub fn exit_current() -> ! {
    unsafe { asm!("csrci sstatus, 2") };
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
        TASKS[cur].kill_req = false;
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

/// 线程级退出（Linux `exit(93)` 语义）：仅当同进程（CLONE_THREAD 共享
/// pid）还有其它存活任务时，清理本线程全局登记并仅退出当前任务、切换走，
/// 返回 `true`（调用点不再返回）。若当前任务已是进程最后一个存活任务，
/// 恢复 IRQ 后返回 `false`，由调用方走完整进程退出（kill_pid + Zombie +
/// 唤醒父进程）。
///
/// 「判定是否还有兄弟」与「退出当前任务」在同一调度锁临界区内完成：
/// 两个线程并发退出时不会都误判"还有其它存活者"而把进程留成 0 存活任务
/// 却永不 Zombie（父进程 wait 永久挂起）。判为整进程退出的竞争残留是
/// 良性的：两方都走 `exit()`，kill_pid/置 Zombie/唤醒等待者均幂等。
pub fn exit_thread_current() -> bool {
    let saved = unsafe { read_daif() };
    unsafe { asm!("csrci sstatus, 2") };
    let cpu = current_cpu();
    sched_lock();
    let cur = unsafe { CURRENT[cpu] };
    if cur >= MAX_TASKS {
        sched_unlock();
        unsafe { write_daif(saved) };
        return false;
    }
    let pid = unsafe { TASKS[cur].pid };
    let mut others = false;
    unsafe {
        for i in 0..MAX_TASKS {
            if i == cur {
                continue;
            }
            let t = &TASKS[i];
            if t.pid == pid && t.state != TaskState::Empty && t.state != TaskState::Exited {
                others = true;
                break;
            }
        }
    }
    if !others {
        sched_unlock();
        unsafe { write_daif(saved) };
        return false;
    }
    // 仍有兄弟线程：清掉本线程的 futex 等待项 / 网络锁 / 睡眠登记
    // （与 kill_pid 对每个任务的处理一致），然后仅退出当前任务。
    unsafe {
        let tid = TASKS[cur].tid;
        crate::bionic_shim::futex_cleanup_tid(tid);
        crate::net_stack::net_cleanup_tid(tid);
        sleepers_clear_tid(tid);
        crate::bionic_shim::clear_tid_slot(tid);
        TASKS[cur].kill_req = false;
        TASKS[cur].state = TaskState::Exited;
    }
    let next = match pick_next() {
        Some(n) => n,
        None => idle_slot(),
    };
    unsafe { do_switch(cur, next) };
    // 不应到达（本任务已 Exited）
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
                TaskState::Stopped => "T", // stopped（SIGSTOP，应用后台/挂起）
            };
            f(t.pid, st, t.name);
        }
    }
}

/// 遍历任务表，对每个非空任务调用
/// `f(pid, tid, 状态名, 任务名, nice, cpu_ticks, cpu_cap)`。
/// 供 `top` / procfs 等调试接口使用（只读，调用方自行屏蔽 IRQ 与否均可）。
pub fn for_each_task_full(
    mut f: impl FnMut(u32, u32, &'static str, &'static str, i32, u64, u16),
) {
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
                TaskState::Stopped => "T",
                TaskState::Empty => "?",
            };
            f(t.pid, t.tid, st, t.name, t.nice, t.cpu_ticks, t.cpu_cap);
        }
    }
}

/// 当前任务累计调度 tick 数（`top` 显示当前任务 CPU 用量）。
pub fn current_cpu_ticks() -> u64 {
    let cur = unsafe { CURRENT[current_cpu()] };
    if cur < MAX_TASKS {
        unsafe { TASKS[cur].cpu_ticks }
    } else {
        0
    }
}
