//! 进程模型：进程控制块（PCB）、`fork`、`execve`、`exit`、`wait`、信号与 fd 表。
//!
//! - PCB：`pid`、状态、地址空间（私有页表 [`vm::ProcPageTable`]）、fd 表、
//!   信号位图、父子关系、进程名。
//! - `fork()`：完整复制父进程地址空间（为每条用户映射分配新物理页并逐字节拷贝，
//!   不做写时复制）、复制 fd 表，并创建子线程。父进程返回子 pid，子进程返回 0。
//! - `exit(code)`：进程退出，唤醒等待它的父进程；`wait(pid)`：阻塞等待子进程退出
//!   （`pid = -1` 等待任意子进程），回收其地址空间与 fd 表。
//!
//! fork 的实现要点：`do_fork`（汇编）保存当前 callee-saved 寄存器与栈指针，
//! 调用 Rust `fork_parent` 分配子栈（拷贝父栈活动区）、创建子进程与子线程，
//! 子线程首次被调度时从 `fork_child_resume` 入口恢复现场、置 x0 = 0 并
//! 返回 `fork()` 的调用点 —— 从而获得“父返回 pid、子返回 0”的标准语义。

use core::arch::{asm, global_asm};

use crate::allocator;
use crate::sched_cfs::{self, PriorityClass, TaskContext};
use crate::timer;
use crate::vm;

/// 每个进程 fd 表上限。
pub const FD_MAX: usize = 16;
/// 进程数上限。
pub const MAX_PROC: usize = 16;
/// 进程名上限。
const MAX_NAME: usize = 16;

// ==================== 文件描述符 ====================

/// fd 指向的对象种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdKind {
    /// 空槽位
    None,
    /// 管道
    Pipe,
    /// 消息队列
    MsgQueue,
    /// 共享内存
    Shm,
    /// VFS 文件（常规/目录/设备等）
    File,
    /// 套接字对（socketpair 双向字节流，见 ipc::PairSock）
    Pair,
}

/// 文件描述符表项。
#[derive(Clone, Copy)]
pub struct FdEntry {
    pub kind: FdKind,
    /// 在对应对象池中的下标
    pub index: usize,
    /// 访问标志（读/写）
    #[allow(dead_code)]
    pub flags: u32,
}

const EMPTY_FD: FdEntry = FdEntry { kind: FdKind::None, index: 0, flags: 0 };

// ==================== 进程状态 ====================

/// 进程信号动作表（独立于 PCB：fork 拷贝 PCB 时不复制这些大数组，
/// 改为在 `fork` 时显式从父进程拷贝，保持 fork 拷贝体积与原版一致）。
#[derive(Clone, Copy)]
struct SigTable {
    handlers: [u64; 64],
    masks: [u64; 64],
}

const EMPTY_SIG: SigTable = SigTable { handlers: [0; 64], masks: [0; 64] };
static mut SIG_TABLES: [SigTable; MAX_PROC] = [EMPTY_SIG; MAX_PROC];

/// 进程状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessState {
    /// 槽位未使用
    Free,
    /// 运行中
    Running,
    /// 僵尸（已退出，等待父进程回收）
    Zombie,
}

/// 进程控制块（PCB）。
pub struct Process {
    pub pid: u32,
    pub state: ProcessState,
    /// 地址空间：私有页表根 + 用户映射记录
    pub pt: vm::ProcPageTable,
    /// 文件描述符表
    pub fds: [FdEntry; FD_MAX],
    /// 待处理信号位图（bit N 对应信号 N）
    pub signals: u64,
    /// 父进程 pid（-1 表示无）
    pub parent: i32,
    /// 退出码
    pub exit_code: i32,
    /// 正在 `wait` 本进程的线程（tid）
    pub waiter: Option<u32>,
    /// 进程名（供 `ps` 等工具显示）
    pub name: [u8; MAX_NAME],
    /// 进程名长度
    pub name_len: usize,
    /// 当前工作目录（NUL 结尾；相对路径系统调用据此解析）。
    pub cwd: [u8; CWD_MAX],
    /// 工作目录长度（不含 NUL）。
    pub cwd_len: usize,
}

impl Process {
    const fn empty() -> Self {
        Process {
            pid: 0,
            state: ProcessState::Free,
            pt: vm::ProcPageTable::empty(),
            fds: [EMPTY_FD; FD_MAX],
            signals: 0,
            parent: -1,
            exit_code: 0,
            waiter: None,
            name: [0; MAX_NAME],
            name_len: 0,
            cwd: [0; CWD_MAX],
            cwd_len: 0,
        }
    }
}

/// 进程工作目录缓冲长度（含 NUL）。
pub const CWD_MAX: usize = 128;

/// 默认工作目录（"/"）。
const DEFAULT_CWD: [u8; CWD_MAX] = {
    let mut c = [0u8; CWD_MAX];
    c[0] = b'/';
    c
};

// ==================== 工作目录 ====================

/// 当前进程工作目录（拷贝到调用方提供的缓冲），返回长度（不含 NUL）。
///
/// `out` 不足时截断；调用方保证 `out.len() >= 2`。
pub fn current_cwd(out: &mut [u8; CWD_MAX]) -> usize {
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        out[0] = b'/';
        out[1] = 0;
        return 1;
    }
    unsafe {
        let len = PROCESSES[pid as usize].cwd_len.min(CWD_MAX - 1);
        out[..len].copy_from_slice(&PROCESSES[pid as usize].cwd[..len]);
        out[len] = 0;
        len
    }
}

/// 设置当前进程工作目录（须为规范化的绝对路径，长度 < CWD_MAX）。
pub fn set_current_cwd(path: &str) {
    if path.len() >= CWD_MAX {
        return;
    }
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        return;
    }
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        p.cwd.fill(0);
        p.cwd[..path.len()].copy_from_slice(path.as_bytes());
        p.cwd_len = path.len();
    }
}

static mut PROCESSES: [Process; MAX_PROC] = [const { Process::empty() }; MAX_PROC];

// ==================== fork 汇编 ====================

extern "C" {
    /// fork 内核主体：父进程返回子 pid，子进程返回 0。
    /// 入参 x0 = 系统调用陷阱帧指针（父进程的），用于定位子进程陷阱帧副本。
    fn do_fork(trap_frame: usize) -> i32;
    /// 子进程首次调度入口：sp 指向子进程拷贝的陷阱帧，恢复现场并 eret 回用户态。
    fn syscall_return_restore();
    /// fork 子进程首次调度入口（先释放调度锁再进 syscall_return_restore）。
    fn el0_resume_entry();
}

global_asm!(
    r#"
    // fork 内核主体
    .section .text.do_fork, "ax"
    .global do_fork
    .type do_fork, %function
do_fork:
    // 暂存入参（陷阱帧指针）到 x9（调用者保存寄存器）
    mov x9, x0
    // 在栈上保存 callee-saved 寄存器（x19-x30），作为子进程上下文
    sub sp, sp, #96
    stp x19, x20, [sp, #0x00]
    stp x21, x22, [sp, #0x10]
    stp x23, x24, [sp, #0x20]
    stp x25, x26, [sp, #0x30]
    stp x27, x28, [sp, #0x40]
    stp x29, x30, [sp, #0x50]
    // x0 = 保存现场指针，x1 = 陷阱帧指针，调用 Rust 创建子进程
    mov x1, x9
    mov x0, sp
    bl fork_parent
    // 父进程路径：恢复现场并返回 pid
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

// ==================== 内部辅助 ====================

fn alloc_pid() -> u32 {
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state == ProcessState::Free {
                return i as u32;
            }
        }
    }
    u32::MAX
}

fn set_name(pid: u32, name: &str) {
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        let n = name.len().min(MAX_NAME);
        p.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        p.name_len = n;
    }
}

/// 当前进程 pid。
pub fn current_pid() -> u32 {
    sched_cfs::current_pid()
}

/// 当前进程的父进程 pid。
pub fn current_parent() -> i32 {
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        return 0;
    }
    unsafe { PROCESSES[pid as usize].parent }
}

// ==================== fork ====================

/// fork 时复制父进程地址空间到子页表（写时复制）。
///
/// 策略：
/// - **LAZY 预留页**：子进程同样只预留（不分配、不拷贝），各自按需落地。
/// - **只读共享池页**（`AP_USER_RO`，无 COW）：ELF 代码/rodata 段，引用计数
///   +1、直接复用父进程页地址（fork 提速，永不复制）。
/// - **RW 数据页**（含已 COW 页）：转为 COW 共享 —— 父进程页表降级为只读
///   （`cow_pte_flags` 置 `AP_USER_RO`）并标记 `FLAG_COW`，子进程复用同一
///   物理页（同样只读映射）；引用计数经 `cow_share` 计入父+子。任一进程
///   首次写入触发 permission fault，由 `cow_break` 复制出新页。
///
/// 返回 `(总映射数, 实际拷贝页数)`（COW 下拷贝数恒为 0，供启动计时统计）。
fn copy_address_space(parent_pid: u32, child_pt: &mut vm::ProcPageTable) -> (usize, usize) {
    let count = unsafe { PROCESSES[parent_pid as usize].pt.count };
    let copied = 0usize;
    let mut shared = 0usize;
    for i in 0..count {
        let p = unsafe { PROCESSES[parent_pid as usize].pt.user_pages[i] };
        if p.paddr == vm::LAZY_PAGE {
            // 按需分页预留页：子进程同样只预留（不分配、不拷贝），
            // 由各自后续访问时独立落地。
            if !child_pt.reserve_page(p.vaddr, p.flags) {
                continue;
            }
            continue;
        }
        if vm::is_user_ro(p.flags) && !vm::is_cow(p.flags) {
            // 只读共享池页：引用计数 +1，子进程复用同一物理页
            allocator::share_page(p.paddr);
            if !child_pt.add_page(p.vaddr, p.paddr, p.flags) {
                allocator::unshare_page(p.paddr);
                continue;
            }
            shared += 1;
            continue;
        }
        // RW 数据页（含已 COW 页）：写时复制共享。
        // 1) 父进程页表降级为只读 + 记录标记 COW（若尚未如此）
        unsafe {
            let parent = &mut PROCESSES[parent_pid as usize];
            let rec = &mut parent.pt.user_pages[i];
            if !vm::is_cow(rec.flags) {
                let cow_flags = rec.flags | vm::FLAG_COW;
                vm::map_page_in(parent.pt.ttbr0, rec.vaddr, rec.paddr, vm::cow_pte_flags(cow_flags));
                rec.flags = cow_flags;
            }
        }
        // 2) 引用计数计入父+子
        allocator::cow_share(p.paddr);
        // 3) 子进程复用同一物理页（只读映射 + COW 记录）
        if !child_pt.add_page_with_pte(
            p.vaddr,
            p.paddr,
            p.flags | vm::FLAG_COW,
            vm::cow_pte_flags(p.flags | vm::FLAG_COW),
        ) {
            allocator::unshare_page(p.paddr);
            continue;
        }
        shared += 1;
    }
    let _ = shared;
    (count, copied)
}

/// 用已复制好的地址空间创建子进程，返回 (子 pid, 子页表根)。
///
/// `child_pt` 以可变引用传入并整体移入 PCB：避免按值传递 16KB 页表
/// （`ProcPageTable`）额外占用内核栈——fork 调用链已很深，大栈帧是
/// 栈溢出的诱因之一。
fn create_child_process(
    parent_pid: u32,
    pid: u32,
    child_pt: &mut vm::ProcPageTable,
) -> (u32, usize) {

    // 先拷贝父进程的 fd 表 / 信号 / 名称 / 工作目录，避免与后续写入产生重叠借用。
    let fds: [FdEntry; FD_MAX];
    let signals: u64;
    let name: [u8; MAX_NAME];
    let name_len: usize;
    let cwd: [u8; CWD_MAX];
    let cwd_len: usize;
    unsafe {
        let p = &PROCESSES[parent_pid as usize];
        fds = p.fds;
        signals = p.signals;
        name = p.name;
        name_len = p.name_len;
        cwd = p.cwd;
        cwd_len = p.cwd_len;
    }

    // 继承父进程信号动作表（独立全局表，不影响 fork 拷贝体积）
    sig_table_copy(parent_pid, pid);

    // fork 复制 fd：文件/共享内存/管道/消息队列引用计数 +1，避免子进程
    // close/exit 导致父进程 UAF、双释放或整槽清空（H-7 修复的延伸）。
    for e in fds.iter() {
        match e.kind {
            FdKind::File => crate::vfs::open_ref(e.index),
            FdKind::Shm => crate::ipc::shm_add_ref(e.index),
            FdKind::Pipe => crate::ipc::pipe_add_ref(e.index),
            FdKind::MsgQueue => crate::ipc::msgq_add_ref(e.index),
            _ => {}
        }
    }

    let ttbr0 = child_pt.ttbr0;
    unsafe {
        PROCESSES[pid as usize] = Process {
            pid,
            state: ProcessState::Running,
            pt: core::mem::replace(child_pt, vm::ProcPageTable::empty()),
            fds,
            signals,
            parent: parent_pid as i32,
            exit_code: 0,
            waiter: None,
            name,
            name_len,
            cwd,
            cwd_len,
        };
    }
    (pid, ttbr0)
}

/// fork 的 Rust 侧主体（由汇编 `do_fork` 调用）。
///
/// `saved_sp` 指向 do_fork 保存在当前栈上的寄存器现场，其地址即需要拷贝的
/// 栈活动区下界；`trap_frame` 为父进程系统调用陷阱帧地址，用于定位子进程
/// 栈副本中的陷阱帧并置其 `x[0] = 0`（子进程 fork 返回 0 的正确语义）。
#[no_mangle]
extern "C" fn fork_parent(saved_sp: usize, trap_frame: usize) -> i32 {
    // 屏蔽 IRQ，保护栈拷贝与任务表写入的临界区
    unsafe { asm!("msr daifset, #2") };
    let t0 = timer::now_us();

    let (stack_base, stack_top) = sched_cfs::current_stack();
    let stack_size = stack_top - stack_base;

    // 拷贝父栈活动区 [saved_sp, stack_top) 到子栈顶部对应位置：
    // 子进程内核栈布局与父进程完全一致（含 do_fork 保存现场与陷阱帧），
    // 子栈在拷贝区之下仍有 ~256KB 可用栈（拷走的只是活动区），
    // execve（argv/envp + setup_stack 等大栈帧）不会溢出。
    let child_stack_top = sched_cfs::alloc_stack();
    let child_stack_base = child_stack_top - stack_size;
    let copy_len = stack_top - saved_sp;
    let child_sp = child_stack_top - copy_len;
    unsafe {
        core::ptr::copy_nonoverlapping(
            saved_sp as *const u8,
            child_sp as *mut u8,
            copy_len,
        );
    }

    // 子进程陷阱帧放在**子栈顶部**（child_stack_top - 272），而不是拷贝区
    // 中部：子进程首次 eret 后内核 sp = child_stack_top，其后的系统调用帧
    // 从栈顶向下分配 —— 与下方拷贝区（含 exec 的 exec_path 等残留数据）
    // 完全隔离，杜绝「残留数据污染 syscall 帧的 elr 字段 → eret 到 0」。
    // 副本中 x[0] 置 0（fork 返回 0），elr/spsr/sp_el0 保持父进程副本值。
    const TRAP_FRAME_SIZE: usize = 800;
    let child_frame = child_stack_top - TRAP_FRAME_SIZE;
    if trap_frame >= saved_sp && trap_frame + TRAP_FRAME_SIZE <= stack_top {
        unsafe {
            core::ptr::copy_nonoverlapping(
                trap_frame as *const u8,
                child_frame as *mut u8,
                TRAP_FRAME_SIZE,
            );
            *(child_frame as *mut u64) = 0;
        }
        // 防御护栏：校验子进程恢复现场（elr/sp_el0/spsr）的合理性。若父进程
        // 陷阱帧损坏（elr=0、elr 越界、sp_el0 不在用户栈区、spsr 非 EL0t），
        // 直接让本次 fork 失败——而不是调度一个「eret 到坏地址」的必死子进程。
        let c_elr = unsafe { *((child_frame + 0x100) as *const u64) };
        let c_sp = unsafe { *((child_frame + 0xf8) as *const u64) };
        let c_spsr = unsafe { *((child_frame + 0x108) as *const u64) };
        let c_x30 = unsafe { *((child_frame + 0xf0) as *const u64) };
        if c_elr == 0
            || c_elr >= crate::vm::USER_VA_TOP as u64
            || c_sp >= crate::vm::USER_STACK_TOP as u64
            || c_spsr & 0xF != 0
        {
            unsafe { asm!("msr daifclr, #2") };
            return -1;
        }
        // [诊断] 记录每次 fork 子进程的恢复现场，用于核对 PC=0 残余故障是否
        // 源自 fork 陷阱帧拷贝损坏（elr=0 / x30=0 应立即暴露）
        crate::uart_send_str("[fork-frame] elr=");
        crate::print_hex(c_elr);
        crate::uart_send_str(" sp=");
        crate::print_hex(c_sp);
        crate::uart_send_str(" x30=");
        crate::print_hex(c_x30);
        crate::uart_send_str(" spsr=");
        crate::print_hex(c_spsr);
        crate::uart_send_str("\n");
    } else {
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }

    // 创建子进程（完整复制地址空间 + fd 表复制）
    let parent_pid = sched_cfs::current_pid();
    let pid = alloc_pid();
    if pid == u32::MAX {
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }
    let mut child_pt = vm::ProcPageTable::new();
    let (total_pages, copied_pages) = copy_address_space(parent_pid, &mut child_pt);
    let (child_pid, child_ttbr0) = create_child_process(parent_pid, pid, &mut child_pt);
    if child_pid == u32::MAX {
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }

    // 启动计时（诊断）：fork 总耗时（含地址空间复制与任务插入）
    let t1 = timer::now_us();
    crate::uart_send_str("[perf] fork pid=");
    crate::print_dec(child_pid as u64);
    crate::uart_send_str(" pages(total=");
    crate::print_dec(total_pages as u64);
    crate::uart_send_str(",copied=");
    crate::print_dec(copied_pages as u64);
    crate::uart_send_str(") took=");
    crate::print_dec((t1 - t0) as u64);
    crate::uart_send_str("us\n");

    // 子进程上下文：首次调度直接进入 syscall_return_restore，sp 指向子进程
    // **拷贝的陷阱帧**（x0 已置 0，elr/spsr/sp_el0 均为子进程自身副本值）。
    // 完全不经过 syscall_dispatch：既不把返回值写回父进程陷阱帧（跨栈写），
    // 也不沿父调用链回卷 —— eret 直接回到用户态 fork() 调用点，x0 = 0。
    let parent_tpidr: u64;
    unsafe { asm!("mrs {}, tpidr_el0", out(reg) parent_tpidr) };
    // 父进程当前 SP_EL0（用户栈指针）：子进程首次被调度前必须保存，
    // 否则 switch_to 恢复的是空/错值，子进程带错误用户栈运行。
    let parent_sp_el0: u64;
    unsafe { asm!("mrs {}, sp_el0", out(reg) parent_sp_el0) };
    let mut ctx = TaskContext::empty();
    ctx.x30 = el0_resume_entry as *const () as usize as u64;
    ctx.sp = child_frame as u64;
    ctx.spsr = 0x5;
    // 初始 ELR 直接取陷阱帧副本中的 EL0 返回地址（偏移 0x100）：子进程首次
    // 调度后若在 syscall_return_restore 的 msr elr_el1 之前被抢占，switch_to
    // 保存/恢复的 ELR 是有效地址而非 0 —— 消除「ELR=0 在飞」窗口。
    ctx.elr = unsafe { *((child_frame + 0x100) as *const u64) };
    ctx.tpidr = parent_tpidr;
    ctx.ttbr0_el1 = child_ttbr0 as u64;
    ctx.sp_el0 = parent_sp_el0;

    let class = sched_cfs::current_class();
    let nice = sched_cfs::current_nice();
    let tid = sched_cfs::insert(
        "fork-child",
        child_pid,
        ctx,
        child_stack_base,
        child_stack_top,
        class,
        nice,
    );
    if tid == u32::MAX {
        // 任务槽位不足：回收子进程（地址空间/栈），返回失败
        unsafe {
            PROCESSES[child_pid as usize].pt.free_user_pages();
            vm::free_process_tables(PROCESSES[child_pid as usize].pt.ttbr0);
            PROCESSES[child_pid as usize] = Process::empty();
            let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
            for p in 0..pages {
                allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }

    unsafe { asm!("msr daifclr, #2") };
    child_pid as i32
}

// ==================== 公共接口 ====================

/// 初始化进程表：创建 pid 0（内核引导/init 父进程）并启动 init 线程。
pub fn init(init_entry: fn(usize) -> !) {
    unsafe {
        for i in 0..MAX_PROC {
            PROCESSES[i] = Process::empty();
        }
    }
    let pt = vm::ProcPageTable::new();
    let ttbr0 = pt.ttbr0;
    unsafe {
        PROCESSES[0] = Process {
            pid: 0,
            state: ProcessState::Running,
            pt,
            fds: [EMPTY_FD; FD_MAX],
            signals: 0,
            parent: -1,
            exit_code: 0,
            waiter: None,
            name: [0; MAX_NAME],
            name_len: 0,
            cwd: DEFAULT_CWD,
            cwd_len: 1,
        };
    }
    set_name(0, "kernel");
    // init 线程：pid 0，普通优先级，地址空间为内核恒等映射
    sched_cfs::spawn(0, "kernel", init_entry, 0, PriorityClass::Normal, 0, ttbr0);
}

/// 创建新进程（新地址空间 + 新线程），返回 pid。
///
/// 用于内核线程（如内置 Shell）：地址空间仅含内核恒等映射，无用户页。
pub fn spawn(
    name: &'static str,
    entry: fn(usize) -> !,
    arg: usize,
    class: PriorityClass,
    nice: i32,
) -> u32 {
    let pid = alloc_pid();
    if pid == u32::MAX {
        return u32::MAX;
    }
    let pt = vm::ProcPageTable::new();
    let ttbr0 = pt.ttbr0;
    unsafe {
        PROCESSES[pid as usize] = Process {
            pid,
            state: ProcessState::Running,
            pt,
            fds: [EMPTY_FD; FD_MAX],
            signals: 0,
            parent: current_pid() as i32,
            exit_code: 0,
            waiter: None,
            name: [0; MAX_NAME],
            name_len: 0,
            cwd: DEFAULT_CWD,
            cwd_len: 1,
        };
    }
    set_name(pid, name);
    if sched_cfs::spawn(pid, name, entry, arg, class, nice, ttbr0) == u32::MAX {
        // 任务槽位耗尽：回收该进程的页表后返回失败（不泄漏）
        unsafe {
            vm::free_process_tables(ttbr0);
            PROCESSES[pid as usize] = Process::empty();
        }
        return u32::MAX;
    }
    pid
}

/// 用给定的地址空间创建新进程（不创建线程），返回 pid。
///
/// 供 ELF 加载器使用：先构建好用户地址空间（代码 + 栈 + 数据映射），
/// 再注册进程并自行 spawn 用户线程。
pub fn create_process(pt: vm::ProcPageTable, name: &str) -> u32 {
    let pid = alloc_pid();
    if pid == u32::MAX {
        return u32::MAX;
    }
    unsafe {
        PROCESSES[pid as usize] = Process {
            pid,
            state: ProcessState::Running,
            pt,
            fds: [EMPTY_FD; FD_MAX],
            signals: 0,
            parent: current_pid() as i32,
            exit_code: 0,
            waiter: None,
            name: [0; MAX_NAME],
            name_len: 0,
            cwd: DEFAULT_CWD,
            cwd_len: 1,
        };
    }
    set_name(pid, name);
    pid
}

/// 重命名进程（execve 替换映像后更新显示名）。
pub fn rename(pid: u32, name: &str) {
    set_name(pid, name);
}

/// 把指定进程的 fd 槽位设为 `entry`（用于为新进程初始化 stdio 等）。
pub fn assign_fd(pid: u32, fd: usize, entry: FdEntry) -> i32 {
    if fd >= FD_MAX || (pid as usize) >= MAX_PROC {
        return -1;
    }
    unsafe {
        PROCESSES[pid as usize].fds[fd] = entry;
    }
    0
}

/// [诊断] 跨进程物理页重复分配检查：除共享 RO / COW 外的私有页若与其它
/// 进程的任意页共享同一物理地址，说明分配器双重分配（系统级损坏根源）。
pub fn check_double_allocations() {
    // 1) 全进程对（含自身）私有页重复物理地址检查
    for a in 0..MAX_PROC {
        let pa = unsafe { &PROCESSES[a] };
        if pa.pt.ttbr0 == 0 {
            continue;
        }
        for i in 0..pa.pt.count {
            let p = pa.pt.user_pages[i];
            if p.paddr == 0 || p.paddr == vm::LAZY_PAGE {
                continue;
            }
            if vm::is_user_ro(p.flags) && !vm::is_cow(p.flags) {
                continue;
            }
            for b in 0..MAX_PROC {
                let pb = unsafe { &PROCESSES[b] };
                if pb.pt.ttbr0 == 0 {
                    continue;
                }
                for j in 0..pb.pt.count {
                    let q = pb.pt.user_pages[j];
                    if q.paddr == p.paddr
                        && !(a == b && i == j)
                        && !(vm::is_user_ro(q.flags) && !vm::is_cow(q.flags))
                    {
                        crate::uart_send_str("\n[DOUBLE-ALLOC] paddr=");
                        crate::print_hex(p.paddr as u64);
                        crate::uart_send_str(" proc ");
                        crate::print_dec(a as u64);
                        crate::uart_send_str(" va=");
                        crate::print_hex(p.vaddr as u64);
                        crate::uart_send_str(" vs proc ");
                        crate::print_dec(b as u64);
                        crate::uart_send_str(" va=");
                        crate::print_hex(q.vaddr as u64);
                        crate::uart_send_str("\n");
                    }
                }
            }
        }
    }
    // 2) 用户页是否落在内核保留区（内核镜像 + .bss，分配器应标记已用）
    let kstart = crate::allocator::RAM_START;
    let kend = crate::kernel_end();
    for a in 0..MAX_PROC {
        let pa = unsafe { &PROCESSES[a] };
        if pa.pt.ttbr0 == 0 {
            continue;
        }
        for i in 0..pa.pt.count {
            let p = pa.pt.user_pages[i];
            if p.paddr >= kstart && p.paddr < kend && p.paddr != 0 && p.paddr != vm::LAZY_PAGE {
                crate::uart_send_str("\n[KERNEL-OVERLAP] proc ");
                crate::print_dec(a as u64);
                crate::uart_send_str(" va=");
                crate::print_hex(p.vaddr as u64);
                crate::uart_send_str(" paddr=");
                crate::print_hex(p.paddr as u64);
                crate::uart_send_str(" in kernel [");
                crate::print_hex(kstart as u64);
                crate::uart_send_str(",");
                crate::print_hex(kend as u64);
                crate::uart_send_str(")\n");
            }
        }
    }
}

/// 当前进程的地址空间根表（物理地址）。
pub fn current_root() -> usize {
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        return vm::kernel_root_addr();
    }
    unsafe { PROCESSES[pid as usize].pt.ttbr0 }
}

/// 替换当前进程的地址空间（execve 使用）。
///
/// 先安装新页表并把 TTBR0_EL1 切换到新根表（此时内核恒等映射在新根表中依然
/// 有效），随后释放旧地址空间（用户数据页 + 私有页表），避免「先释放仍被
/// TTBR0 引用的页表」造成的悬空。
pub fn replace_address_space(pid: u32, new_pt: vm::ProcPageTable) {
    let new_ttbr0 = new_pt.ttbr0;
    let old_pt = unsafe {
        let p = &mut PROCESSES[pid as usize];
        core::mem::replace(&mut p.pt, new_pt)
    };
    // 切到新根表（TTBR0 + tlbi vmalle1）
    sched_cfs::set_current_ttbr0(new_ttbr0);
    // 释放旧地址空间
    let mut old = old_pt;
    old.free_user_pages();
    vm::free_process_tables(old.ttbr0);
}

/// 复制当前进程：父进程返回子 pid，子进程返回 0。
///
/// `frame` 为当前系统调用陷阱帧指针（用于定位子进程陷阱帧副本，置其 x[0]=0）。
pub fn fork(frame: usize) -> i32 {
    let pid = unsafe { do_fork(frame) };
    // black_box 阻止尾调用优化，保证 do_fork 的返回地址存在（子进程在此返回 0）
    core::hint::black_box(pid)
}

/// 强制回收一个「刚创建即失败」的进程（如任务槽位耗尽）：释放地址空间、
/// fd 引用与进程槽位。进程从未被调度（无线程），无需任务清理。
pub fn reap_forced(pid: u32) {
    if (pid as usize) >= MAX_PROC {
        return;
    }
    unsafe {
        let ttbr0 = PROCESSES[pid as usize].pt.ttbr0;
        for fd in 0..FD_MAX {
            let e = PROCESSES[pid as usize].fds[fd];
            match e.kind {
                FdKind::File => crate::vfs::close_ref(e.index),
                FdKind::Pipe => crate::ipc::pipe_drop_ref(e.index),
                FdKind::MsgQueue => crate::ipc::msgq_drop_ref(e.index),
                FdKind::Shm => crate::ipc::shm_close_index(pid, e.index),
                _ => {}
            }
        }
        PROCESSES[pid as usize].pt.free_user_pages();
        vm::free_process_tables(ttbr0);
        PROCESSES[pid as usize] = Process::empty();
    }
}

/// 回收指定进程：释放用户数据页与私有页表，清空 fd 表，槽位标记 Free。
///
/// 调用前提：目标进程已是 Zombie 且不再被调度（TTBR0 已指向其它进程）。
fn reap(pid: u32) -> i32 {
    unsafe {
        let code = PROCESSES[pid as usize].exit_code;
        let ttbr0 = PROCESSES[pid as usize].pt.ttbr0;
        // 释放该进程全部 fd 资源（文件引用计数 -1、管道/队列/共享内存槽位释放），
        // 防止对象池耗尽 panic 与文件表项泄漏
        for fd in 0..FD_MAX {
            let e = PROCESSES[pid as usize].fds[fd];
            match e.kind {
                FdKind::File => crate::vfs::close_ref(e.index),
                FdKind::Pipe => crate::ipc::pipe_drop_ref(e.index),
                FdKind::MsgQueue => crate::ipc::msgq_drop_ref(e.index),
                FdKind::Shm => crate::ipc::shm_close_index(pid, e.index),
                _ => {}
            }
        }
        PROCESSES[pid as usize].pt.free_user_pages();
        vm::free_process_tables(ttbr0);
        // 回收该进程全部已退出线程的内核栈与 TCB 槽位（防 alloc_tid 耗尽 panic）
        sched_cfs::reap_pid(pid);
        PROCESSES[pid as usize] = Process::empty();
        code
    }
}

/// 当前进程退出（唤醒等待者，回收当前线程，不再返回）。
pub fn exit(code: i32) -> ! {
    // 屏蔽 IRQ，保证「终止兄弟线程 → 标记僵尸 → 唤醒等待者 → 切走」整个过程原子。
    unsafe { asm!("msr daifset, #2") };
    let pid = current_pid();
    // 终止同进程的所有线程：多线程进程退出时，兄弟线程必须在此之前停止，
    // 否则父进程 reap 释放页表后它们会以悬空 TTBR0 继续运行。
    sched_cfs::kill_pid(pid);
    unsafe {
        PROCESSES[pid as usize].state = ProcessState::Zombie;
        PROCESSES[pid as usize].exit_code = code;
        if let Some(wt) = PROCESSES[pid as usize].waiter {
            sched_cfs::wake_irq(wt);
        }
    }
    // 唤醒父进程（若其正阻塞于 wait_any）；仅当父进程注册了 SIGCHLD handler
    // 才置位（SIGCHLD 默认动作 = 忽略，不置位可避免 syscall 入口的
    // 待处理信号扫描开销 —— 该扫描会移动时序敏感窗口，放大 EL0 PC=0 竞态）
    let parent = unsafe { PROCESSES[pid as usize].parent };
    if parent >= 0 {
        sched_cfs::wake_pid(parent as u32);
        if parent < MAX_PROC as i32 && sig_handler(parent as u32, 17) != 0 {
            signal_send(parent as u32, 17);
        }
    }
    sched_cfs::exit_current()
}

/// 等待子进程退出。
///
/// `pid = -1` 或 `0`：等待任意子进程；`pid > 0`：等待指定子进程。
/// 返回 `(回收的 pid, 退出码)`；无子进程或进程不存在返回 `(-1, 0)`。
pub fn wait(pid: i32) -> (i32, i32) {
    let cur = current_pid();
    if pid == -1 || pid == 0 {
        // 等待任意子进程
        loop {
            // 找僵尸子进程
            let mut zombie = None;
            unsafe {
                for i in 0..MAX_PROC {
                    if PROCESSES[i].state == ProcessState::Zombie
                        && PROCESSES[i].parent == cur as i32
                    {
                        zombie = Some(i as u32);
                        break;
                    }
                }
            }
            if let Some(z) = zombie {
                let code = reap(z);
                return (z as i32, code);
            }
            // 判断是否还存在子进程（否则 ECHILD）
            let mut any_child = false;
            unsafe {
                for i in 0..MAX_PROC {
                    if PROCESSES[i].state != ProcessState::Free
                        && PROCESSES[i].parent == cur as i32
                    {
                        any_child = true;
                        break;
                    }
                }
            }
            if !any_child {
                return (-1, 0);
            }
            sched_cfs::block_current();
        }
    } else if pid > 0 && (pid as usize) < MAX_PROC {
        loop {
            let action = unsafe {
                let p = &mut PROCESSES[pid as usize];
                match p.state {
                    ProcessState::Free => return (-1, 0),
                    ProcessState::Zombie => Some(()),
                    ProcessState::Running => {
                        p.waiter = Some(sched_cfs::current_tid());
                        None
                    }
                }
            };
            if action.is_some() {
                let code = reap(pid as u32);
                return (pid, code);
            }
            sched_cfs::block_current();
        }
    } else {
        (-1, 0)
    }
}

/// 遍历全部存活进程，对每个进程调用 `f(pid, name)`。
pub fn ps(f: &mut dyn FnMut(u32, &str)) {
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state != ProcessState::Free {
                let name = core::str::from_utf8(&PROCESSES[i].name[..PROCESSES[i].name_len])
                    .unwrap_or("?");
                f(PROCESSES[i].pid, name);
            }
        }
    }
}

/// 向进程发送信号（置位信号位图）。
/// 设置进程信号 handler 与屏蔽字。
pub fn set_sig_handler(pid: u32, sig: usize, handler: u64, mask: u64) {
    if (pid as usize) < MAX_PROC && sig < 64 {
        unsafe {
            SIG_TABLES[pid as usize].handlers[sig] = handler;
            SIG_TABLES[pid as usize].masks[sig] = mask;
        }
    }
}

/// 读取进程信号 handler（0 = 默认动作）。
pub fn sig_handler(pid: u32, sig: usize) -> u64 {
    if (pid as usize) < MAX_PROC && sig < 64 {
        unsafe { SIG_TABLES[pid as usize].handlers[sig] }
    } else {
        0
    }
}

/// 读取进程信号屏蔽字。
pub fn sig_mask(pid: u32, sig: usize) -> u64 {
    if (pid as usize) < MAX_PROC && sig < 64 {
        unsafe { SIG_TABLES[pid as usize].masks[sig] }
    } else {
        0
    }
}

/// fork 时把父进程信号表拷贝给子进程（在 create_child_process 内调用）。
pub fn sig_table_copy(parent: u32, child: u32) {
    if (parent as usize) < MAX_PROC && (child as usize) < MAX_PROC {
        unsafe {
            SIG_TABLES[child as usize] = SIG_TABLES[parent as usize];
        }
    }
}

/// 清除进程的待处理信号位。
pub fn signal_clear(pid: u32, sig: u32) {
    if (pid as usize) < MAX_PROC && (sig as usize) < 64 {
        unsafe {
            PROCESSES[pid as usize].signals &= !(1u64 << sig);
        }
    }
}

pub fn signal_send(pid: u32, sig: u32) {
    if sig >= 64 || (pid as usize) >= MAX_PROC {
        return;
    }
    unsafe {
        PROCESSES[pid as usize].signals |= 1u64 << sig;
    }
}

/// 读取进程待处理信号位图。
pub fn signal_pending(pid: u32) -> u64 {
    if (pid as usize) >= MAX_PROC {
        return 0;
    }
    unsafe { PROCESSES[pid as usize].signals }
}

// ==================== fd 表 ====================

/// 在当前进程 fd 表中分配一个描述符。
pub fn alloc_fd(kind: FdKind, index: usize, flags: u32) -> i32 {
    let pid = current_pid();
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        for i in 0..FD_MAX {
            if p.fds[i].kind == FdKind::None {
                p.fds[i] = FdEntry { kind, index, flags };
                return i as i32;
            }
        }
    }
    -1
}

/// 获取当前进程 fd 对应的表项。
pub fn get_fd(fd: i32) -> Option<FdEntry> {
    if fd < 0 || (fd as usize) >= FD_MAX {
        return None;
    }
    let pid = current_pid();
    unsafe {
        let e = PROCESSES[pid as usize].fds[fd as usize];
        if e.kind == FdKind::None {
            None
        } else {
            Some(e)
        }
    }
}

/// 关闭当前进程的一个 fd。
pub fn close_fd(fd: i32) {
    if fd < 0 || (fd as usize) >= FD_MAX {
        return;
    }
    let pid = current_pid();
    unsafe {
        PROCESSES[pid as usize].fds[fd as usize] = EMPTY_FD;
    }
}

/// 在指定槽位写入 fd 表项（`dup3` 固定目标描述符用），槽位越界返回 -1。
pub fn set_fd(fd: usize, entry: FdEntry) -> i32 {
    if fd >= FD_MAX {
        return -1;
    }
    let pid = current_pid();
    unsafe {
        PROCESSES[pid as usize].fds[fd] = entry;
    }
    0
}

/// 判断指定 pid 是否为一个存活（非 Free）进程。
pub fn process_exists(pid: u32) -> bool {
    if (pid as usize) >= MAX_PROC {
        return false;
    }
    unsafe { PROCESSES[pid as usize].state != ProcessState::Free }
}

// ==================== 用户映射 ====================

/// 把 `paddr` 映射到当前进程的 `vaddr`（用户 RW），**不**纳入地址空间记录。
///
/// 用于共享内存（SHM）等跨进程共享的物理页：这些页由各自对象池管理，
/// fork 不复制、exit 不释放。
pub fn map_user_page(vaddr: usize, paddr: usize) {
    if !vm::user_va_range_ok(vaddr, vm::PAGE_SIZE) {
        return;
    }
    let pid = current_pid();
    unsafe {
        let root = PROCESSES[pid as usize].pt.ttbr0;
        vm::map_page_in(root, vaddr, paddr, vm::FLAG_USER_DATA);
    }
}

/// 把 `paddr` 映射到当前进程的 `vaddr`（用户 RW），并纳入地址空间记录。
///
/// 用于进程私有页（brk 堆、匿名 mmap）：fork 完整复制、exit 释放。
/// 返回是否成功；记录表满或地址非法时返回 `false`（不建立映射），
/// 调用方必须回滚已分配的物理页。
pub fn map_user_page_owned(vaddr: usize, paddr: usize) -> bool {
    let pid = current_pid();
    unsafe {
        PROCESSES[pid as usize].pt.add_page(vaddr, paddr, vm::FLAG_USER_DATA)
    }
}

/// 在当前进程地址空间中预留一页（按需分页）：仅记录、不分配物理页。
///
/// 由 `sys_mmap` / brk 的按需路径调用；首次访问时经 page fault 落地。
/// 返回是否成功；失败时调用方必须回滚已预留的页。
pub fn reserve_user_page(vaddr: usize) -> bool {
    let pid = current_pid();
    unsafe {
        PROCESSES[pid as usize].pt.reserve_page(vaddr, vm::FLAG_USER_DATA)
    }
}

/// 处理当前进程的一页缺页（EL0 data abort 的按需分页落地）。
///
/// 若 `vaddr` 页存在 LAZY 预留记录，则分配物理页、清零、建立映射并把记录
/// 更新为真实映射，返回 `true`；否则（非预留页/记录表满/物理内存不足）返回
/// `false`，调用方按 SIGSEGV 处理。
pub fn page_fault_resolve(vaddr: usize) -> bool {
    if !vm::user_va_range_ok(vaddr, vm::PAGE_SIZE) {
        return false;
    }
    let page = vaddr & !(vm::PAGE_SIZE - 1);
    let pid = current_pid();
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        for i in 0..p.pt.count {
            let rec = p.pt.user_pages[i];
            if rec.vaddr == page && rec.paddr == vm::LAZY_PAGE {
                // 落地：分配物理页 + 清零 + 建立映射 + 更新记录
                let Some(pa) = allocator::alloc_page() else { return false };
                core::ptr::write_bytes(pa as *mut u8, 0, vm::PAGE_SIZE);
                vm::map_page_in(p.pt.ttbr0, page, pa, rec.flags);
                p.pt.user_pages[i] = vm::UserPage { vaddr: page, paddr: pa, flags: rec.flags };
                PAGE_FAULTS_RESOLVED += 1;
                return true;
            }
        }
    }
    false
}

/// 按需分页累计成功落地次数（全局诊断统计，供启动自检/dmesg 输出）。
static mut PAGE_FAULTS_RESOLVED: u64 = 0;

/// 读取按需分页累计落地次数。
pub fn page_faults_resolved() -> u64 {
    unsafe { PAGE_FAULTS_RESOLVED }
}

/// COW 写时复制累计 break 次数（全局诊断统计）。
static mut COW_BREAKS: u64 = 0;

/// 读取 COW break 累计次数。
pub fn cow_breaks() -> u64 {
    unsafe { COW_BREAKS }
}

/// 处理当前进程一页 COW 写保护缺页（permission fault）：把共享页复制出
/// 本进程私有新页并恢复可写。
///
/// 若 `vaddr` 页是 COW 共享页（`FLAG_COW`），则：
/// 1. 分配新物理页并拷贝共享页内容；
/// 2. 本进程页表改为指向新页（RW，清除 COW）；
/// 3. 共享页引用计数 -1（归零才释放旧页）。
///
/// 返回 `true` 表示已复制（调用方 eret 重试故障指令）；非 COW 页返回 `false`。
pub fn cow_break(vaddr: usize) -> bool {
    if !vm::user_va_range_ok(vaddr, vm::PAGE_SIZE) {
        return false;
    }
    let page = vaddr & !(vm::PAGE_SIZE - 1);
    let pid = current_pid();
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        for i in 0..p.pt.count {
            let rec = p.pt.user_pages[i];
            if rec.vaddr == page && vm::is_cow(rec.flags) && rec.paddr != vm::LAZY_PAGE {
                // 1) 分配新物理页 + 拷贝共享内容
                let Some(new_pa) = allocator::alloc_page() else { return false };
                core::ptr::copy_nonoverlapping(
                    rec.paddr as *const u8,
                    new_pa as *mut u8,
                    vm::PAGE_SIZE,
                );
                // 2) 本进程映射切到新页（RW，清 COW）；map_page_in 覆盖已有
                //    映射会做 TLB 失效，之后 eret 重试的写指令命中新页
                let new_flags = rec.flags & !vm::FLAG_COW;
                vm::map_page_in(p.pt.ttbr0, page, new_pa, vm::cow_pte_flags(new_flags));
                p.pt.user_pages[i] = vm::UserPage { vaddr: page, paddr: new_pa, flags: new_flags };
                // 3) 共享页引用 -1，归零释放旧页
                if allocator::unshare_page(rec.paddr) {
                    allocator::free_page(rec.paddr);
                }
                COW_BREAKS += 1;
                return true;
            }
        }
    }
    false
}

/// 确保 `[va, va+len)` 范围内所有 LAZY 预留页均已落地（分配物理页并映射）。
///
/// 供内核态访问用户缓冲区前调用（`user_ok` 等路径）：按需分页后用户 mmap
/// 出来的缓冲区初始未映射，内核在 EL1 直接解引用未映射用户页会触发 data
/// abort 挂死内核，因此访问前必须先把范围内的预留页全部落地。
///
/// 返回是否成功；范围非法或任一页落地失败返回 `false`。
pub fn ensure_user_range_mapped(va: usize, len: usize) -> bool {
    if !vm::user_va_range_ok(va, len) {
        return false;
    }
    let end = va + len;
    let mut cur = va & !(vm::PAGE_SIZE - 1);
    while cur < end {
        // 已映射页直接跳过（避免对每页都 O(n) 遍历记录表）
        if !vm::user_range_mapped(current_root(), cur, vm::PAGE_SIZE) {
            if !page_fault_resolve(cur) {
                return false;
            }
        }
        cur += vm::PAGE_SIZE;
    }
    true
}

/// 确保 `[va, va+len)` 范围内所有页均已落地**且可写**：LAZY 页落地、
/// COW 共享页 break 出私有副本。
///
/// 供内核态**写入**用户缓冲区前调用（`copy_to_user` 等路径）：COW 页在页表
/// 中映射为只读（`AP_USER_RO`），但该只读只约束 EL0 —— EL1 内核写共享页
/// 不会触发 fault，会直接污染其它进程的数据。因此内核写用户内存前必须先
/// 把范围内的 COW 页全部 break 成本进程私有可写页。
///
/// 返回是否成功；范围非法或任一页处理失败返回 `false`。
pub fn ensure_user_range_writable(va: usize, len: usize) -> bool {
    if !vm::user_va_range_ok(va, len) {
        return false;
    }
    let end = va + len;
    let mut cur = va & !(vm::PAGE_SIZE - 1);
    while cur < end {
        // 先落地 LAZY 页（未映射则无法判断是否 COW，且写前必须映射）
        if !vm::user_range_mapped(current_root(), cur, vm::PAGE_SIZE) {
            if !page_fault_resolve(cur) {
                return false;
            }
        }
        // COW 共享页 → break 出私有可写副本
        if !cow_break(cur) {
            // 非 COW 页：无需处理（已映射即可写）
        }
        cur += vm::PAGE_SIZE;
    }
    true
}

/// 回滚最近 `n` 个用户页映射（mmap/brk 分配中途失败时调用）：
/// 取消映射、归还物理页、移除地址空间记录，避免泄漏。
///
/// LAZY 预留页（按需分页）无物理页，仅移除记录；真实映射页才取消映射并归还。
pub fn rollback_user_pages(va: usize, n: usize) {
    if n == 0 {
        return;
    }
    let pid = current_pid();
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        let root = p.pt.ttbr0;
        for i in 0..n {
            vm::unmap_page_in(root, va + i * vm::PAGE_SIZE);
        }
        let total = p.pt.count;
        let start = total.saturating_sub(n);
        for i in start..total {
            let pg = p.pt.user_pages[i].paddr;
            if pg != 0 && pg != vm::LAZY_PAGE {
                allocator::free_page(pg);
            }
            p.pt.user_pages[i] = vm::EMPTY_USER_PAGE;
        }
        p.pt.count = start;
    }
}

/// 取消映射并释放 `[va, va+len)` 范围内已存在的进程私有页（MAP_FIXED
/// 覆盖旧映射时调用），返回释放的页数。共享内存等非私有映射不在记录表中，
/// 不会被误释放。
///
/// LAZY 预留页（按需分页）无物理页：仅移除记录，不取消映射也不归还。
/// COW/只读共享页引用计数 -1，归零才真正归还物理页。
pub fn unmap_user_range(va: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let pid = current_pid();
    let mut freed = 0usize;
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        let root = p.pt.ttbr0;
        let end = va + len;
        let mut i = 0usize;
        while i < p.pt.count {
            let pg = p.pt.user_pages[i];
            if pg.paddr != 0 && pg.vaddr >= va && pg.vaddr < end {
                if pg.paddr != vm::LAZY_PAGE {
                    vm::unmap_page_in(root, pg.vaddr);
                    if vm::is_user_ro(pg.flags) || vm::is_cow(pg.flags) {
                        // 共享页（只读共享池 / COW）：引用 -1，归零才归还
                        if allocator::unshare_page(pg.paddr) {
                            allocator::free_page(pg.paddr);
                            freed += 1;
                        }
                    } else {
                        // 私有 RW 页：直接归还
                        allocator::free_page(pg.paddr);
                        freed += 1;
                    }
                }
                // 移除记录：用末项填补空洞（i 不自增，填补项需继续检查）
                p.pt.count -= 1;
                p.pt.user_pages[i] = p.pt.user_pages[p.pt.count];
                p.pt.user_pages[p.pt.count] = vm::EMPTY_USER_PAGE;
            } else {
                i += 1;
            }
        }
    }
    freed
}
