//! 进程模型：进程控制块（PCB）、`fork`、`execve`、`exit`、`wait`、信号与 fd 表。
//!
//! - PCB：`pid`、状态、地址空间（私有页表 [`vm::ProcPageTable`]）、fd 表、
//!   信号位图、父子关系、进程名、uid/gid（安全模型）。
//! - `fork()`：**写时复制**（COW）复制父进程地址空间 —— 只读共享池页引用
//!   +1 直接复用；RW 数据页父进程页表降级只读并标记 `FLAG_COW`、子进程复用
//!   同一物理页，任一进程首次写入触发 permission fault 由 `cow_break`
//!   复制出私有页（见 [`vm::FLAG_COW`] 与 `copy_address_space`）。同时复制
//!   fd 表、信号位图、uid/gid 与 ASLR 运行时信息。
//! - `exit(code)`：进程退出，唤醒等待它的父进程；`wait(pid)`：阻塞等待子进程退出
//!   （`pid = -1` 等待任意子进程），回收其地址空间与 fd 表。
//!
//! fork 的实现要点：`do_fork`（汇编）保存当前 callee-saved 寄存器与栈指针，
//! 调用 Rust `fork_parent` 分配子栈（拷贝父栈活动区）、创建子进程与子线程，
//! 子线程首次被调度时从 `fork_child_resume` 入口恢复现场、置 x0 = 0 并
//! 返回 `fork()` 的调用点 —— 从而获得“父返回 pid、子返回 0”的标准语义。

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::allocator;
use crate::sched_cfs::{self, PriorityClass, TaskContext};
use crate::timer;
use crate::vm;

/// 每个进程 fd 表上限。
pub const FD_MAX: usize = 16;
/// 进程数上限。
///
/// 原为 16：系统常驻进程（kernel 0 + wifi worker 2 + init 1 + 11 个守护进程
/// 3..13）已占 15 槽，sfsrace 测试本身（pid 14）再 fork 6 个子进程共需 22 槽，
/// 16 槽必然「fork 失败」（alloc_pid 返回上限）—— 这是 2026-08-28 复现
/// `make test-sfsrace` 一直 FAIL、且遗留 `/sfsrace-test` 标记导致 `make run-gui`
/// 一启动就进测试模式并自动关机的直接原因。扩到 24：常驻 15 + sfsrace 6 子进程
/// + 余量，fork 不再因进程表满而失败。
pub const MAX_PROC: usize = 24;

/// SMP：进程槽位认领位（1 = 分配中/回收中），与 `PROCESSES` 平行。
///
/// 此前槽位分配/回收的 Free 窗口无互斥：他核 `alloc_pid` 扫描见 Free 即
/// 返回，与 `reap`「置 Free → 释放 fd/页表 → 清零 PCB」交错时双方同时
/// 写同一槽位（新 PCB 半初始化即被清零、新页表被 reap 释放）。每槽一枚
/// 认领位：`alloc_pid` 认领后持至 `state=Running` 写回（或失败清零后）
/// 释放；`reap` 在 Zombie→Free 前认领、PCB 清零后释放。认领只独占单个
/// 槽位，不阻塞其他槽位的并发分配（fork 的长地址空间复制不放大临界区）。
static PROC_CLAIM: [AtomicU32; MAX_PROC] = [const { AtomicU32::new(0) }; MAX_PROC];
/// 进程名上限。
const MAX_NAME: usize = 16;
/// 每个进程可登记的等待线程数（`wait(pid)` 多等待者：并发 wait 各自登记，
/// 防止单槽位被覆盖导致先等待的线程永不唤醒）。
const WAIT_WAITERS: usize = 4;

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
    /// 正在 `wait` 本进程的线程（tid，多等待者槽位：ipc.rs WAITERS 同构，
    /// 防单槽被并发 wait 覆写——先前只登记一个 tid，第二个等待线程会覆盖
    /// 第一个，后者在退出唤醒时被漏掉、永久阻塞）。
    pub waiters: [Option<u32>; WAIT_WAITERS],
    /// 进程名（供 `ps` 等工具显示）
    pub name: [u8; MAX_NAME],
    /// 进程名长度
    pub name_len: usize,
    /// 当前工作目录（NUL 结尾；相对路径系统调用据此解析）。
    pub cwd: [u8; CWD_MAX],
    /// 工作目录长度（不含 NUL）。
    pub cwd_len: usize,
    /// 用户 ID（安全模型：per-process 身份，Android 式 per-app UID 的地基）。
    /// 0 = root（init / 系统服务 / 内置 shell）；应用进程由 launcher 在
    /// execve 前 setuid 到非零 UID（如 10000+），从而受 kill 权限等检查约束。
    pub uid: u32,
    /// 组 ID（与 uid 配套，当前未参与文件权限判定——VFS 尚无 mode/owner）。
    pub gid: u32,
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
            waiters: [None; WAIT_WAITERS],
            name: [0; MAX_NAME],
            name_len: 0,
            cwd: [0; CWD_MAX],
            cwd_len: 0,
            uid: 0,
            gid: 0,
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

/// 认领一个 Free 槽位（原子，SMP）。
///
/// 认领成功后调用方**独占该槽位**，直到 `state=Running` 写回（成功）或
/// 槽位清零（失败），期间必须调用 [`proc_claim_release`] 释放，漏调会把
/// 该槽位永久占用（fork 慢性泄漏直至进程表满）。
fn alloc_pid() -> u32 {
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state != ProcessState::Free {
                continue;
            }
            if PROC_CLAIM[i].compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err()
            {
                continue; // 他核正在分配/回收此槽位
            }
            // 复查：认领成功即独占该槽位的 Free 生命周期（reap 亦须先取
            // 认领才能置 Free），此处仅对认领前的陈旧 state 读双保险。
            if PROCESSES[i].state == ProcessState::Free {
                return i as u32;
            }
            PROC_CLAIM[i].store(0, Ordering::Release);
        }
    }
    u32::MAX
}

/// 释放槽位认领（分配成功 `state=Running` 已写回，或失败路径槽位已
/// 清零/从未写入之后）。
fn proc_claim_release(pid: u32) {
    if (pid as usize) < MAX_PROC {
        PROC_CLAIM[pid as usize].store(0, Ordering::Release);
    }
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

/// 指定进程的父进程 pid（不存在返回 -1；供 kill 权限检查等）。
pub fn parent_of(pid: u32) -> i32 {
    if (pid as usize) >= MAX_PROC {
        return -1;
    }
    unsafe {
        let p = &PROCESSES[pid as usize];
        if p.state == ProcessState::Free {
            return -1;
        }
        p.parent
    }
}

/// 当前进程用户 ID（Android 式 per-app UID 的读取端）。
pub fn current_uid() -> u32 {
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        return 0;
    }
    unsafe { PROCESSES[pid as usize].uid }
}

/// 指定进程的用户 ID（不存在/空闲槽位返回 0；供 ps/kill 权限展示）。
pub fn uid_of(pid: u32) -> u32 {
    if (pid as usize) >= MAX_PROC {
        return 0;
    }
    unsafe {
        let p = &PROCESSES[pid as usize];
        if p.state == ProcessState::Free {
            return 0;
        }
        p.uid
    }
}

/// 当前进程组 ID。
pub fn current_gid() -> u32 {
    let pid = current_pid();
    if (pid as usize) >= MAX_PROC {
        return 0;
    }
    unsafe { PROCESSES[pid as usize].gid }
}

/// 设置当前进程 uid（调用方须已在 syscall 层完成权限校验：仅 root 可改）。
pub fn set_uid(uid: u32) {
    let pid = current_pid();
    if (pid as usize) < MAX_PROC {
        unsafe { PROCESSES[pid as usize].uid = uid };
    }
}

/// 设置当前进程 gid（同上，权限校验在 syscall 层）。
pub fn set_gid(gid: u32) {
    let pid = current_pid();
    if (pid as usize) < MAX_PROC {
        unsafe { PROCESSES[pid as usize].gid = gid };
    }
}

// ==================== ASLR 熵源 ====================

/// ASLR 熵：从 virtio-rng 取 4 字节（无 RNG 时回退到计时器混合伪随机，
/// 与 `sys_getrandom` 的降级路径一致）。演示用途，非密码学安全。
pub fn aslr_rand() -> u32 {
    let mut buf = [0u8; 4];
    let r = crate::virtio_rng::get_random(&mut buf);
    if r > 0 {
        return u32::from_le_bytes(buf);
    }
    let now = crate::timer::now_us();
    let seed = now
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .rotate_left(17)
        ^ ((sched_cfs::current_tid() as u64) << 32)
        ^ 0xC0FF_EE00;
    (seed >> 16) as u32
}

/// 返回 `[0, max)` 范围内的页对齐随机偏移（max 为页数）。
pub fn aslr_pages(max_pages: u32) -> usize {
    if max_pages == 0 {
        return 0;
    }
    (aslr_rand() % max_pages) as usize * vm::PAGE_SIZE
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
                // 父页降级失败（页表 OOM）：保持父页 RW，子进程该页的
                // COW 映射也会失败并被跳过 → 子进程访问时 SIGSEGV，
                // 不会出现父子权限不一致的共享写穿。
                if !vm::map_page_in(parent.pt.ttbr0, rec.vaddr, rec.paddr, vm::cow_pte_flags(cow_flags)) {
                    continue;
                }
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
            FdKind::Pipe => crate::ipc::pipe_add_ref(e.index, e.flags & crate::ipc::FD_WRITE != 0),
            FdKind::MsgQueue => crate::ipc::msgq_add_ref(e.index),
            FdKind::Pair => crate::ipc::pair_add_ref(e.index, e.flags as u32),
            _ => {}
        }
    }

    let ttbr0 = child_pt.ttbr0;
    unsafe {
        // 逐字段写入 + 直接 memcpy 页表：避免 `Process { pt: ... }` 结构体
        // 构造在栈上产生 96KB+ 临时（叠加 fork 深调用链会溢出内核任务栈）。
        let dst = &mut PROCESSES[pid as usize] as *mut Process;
        {
            let proc_q = core::mem::size_of::<Process>() / 8;
            let dstq = dst as *mut u64;
            for i in 0..proc_q {
                core::ptr::write_volatile(dstq.add(i), 0u64);
            }
        }
        (*dst).pid = pid;
        (*dst).state = ProcessState::Running;
        (*dst).parent = parent_pid as i32;
        (*dst).exit_code = 0;
        (*dst).waiters = [None; WAIT_WAITERS];
        (*dst).signals = signals;
        (*dst).name = name;
        (*dst).name_len = name_len;
        (*dst).cwd = cwd;
        (*dst).cwd_len = cwd_len;
        (*dst).fds = fds;
        // 安全模型：子进程继承父进程身份（uid/gid 随 fork 传递，随 exec 保持）
        (*dst).uid = PROCESSES[parent_pid as usize].uid;
        (*dst).gid = PROCESSES[parent_pid as usize].gid;
        {
            // 移动语义转入 PCB：ProcPageTable 内含堆上记录表（Pages Box），
            // u64 字拷贝会复制 Box 指针 → PCB 与 child_pt 双释放。read 进槽位
            // 后把源复位为空表：fork_parent 后续（失败回收路径）只能见到惰性
            // 空表，其 Box drop 不再触碰已移交的堆内存。
            core::ptr::write(
                core::ptr::addr_of_mut!((*dst).pt),
                core::ptr::read(child_pt as *const vm::ProcPageTable),
            );
            core::ptr::write(child_pt as *mut vm::ProcPageTable, vm::ProcPageTable::empty());
        }
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
    // 子进程内核栈：分配失败（OOM）时本次 fork 明确失败（此前 panic，
    // fork/clone 风暴可打挂内核）。
    let Some(child_stack_top) = sched_cfs::alloc_stack() else {
        unsafe { asm!("msr daifclr, #2") };
        crate::uart_send_str("[fork] 子进程栈分配失败（OOM）\n");
        return -12; // -ENOMEM
    };
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
            || c_sp == 0
            || c_spsr & 0xF != 0
        {
            // 陷阱帧护栏失败：归还已分配的子栈（此前 fork 失败路径漏释放，
            // 反复失败 fork 会永久泄漏 256KB/次）。
            let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
            unsafe {
                for p in 0..pages {
                    allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
                }
            }
            unsafe { asm!("msr daifclr, #2") };
            return -1;
        }
        // [诊断] 记录每次 fork 子进程的恢复现场，用于核对 PC=0 残余故障是否
        // 源自 fork 陷阱帧拷贝损坏（elr=0 / x30=0 应立即暴露）
        crate::kinfo! {
            crate::uart_send_str("[fork-frame] elr=");
            crate::print_hex(c_elr);
            crate::uart_send_str(" sp=");
            crate::print_hex(c_sp);
            crate::uart_send_str(" x30=");
            crate::print_hex(c_x30);
            crate::uart_send_str(" spsr=");
            crate::print_hex(c_spsr);
            crate::uart_send_str("\n");
        }
    } else {
        // 陷阱帧不在父栈活动区：归还已分配的子栈（与护栏失败路径一致，
        // 此前该路径漏释放，反复失败 fork 按 256KB/次泄漏内核物理页）。
        let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
        unsafe {
            for p in 0..pages {
                allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }

    // 创建子进程（完整复制地址空间 + fd 表复制）
    let parent_pid = sched_cfs::current_pid();
    let pid = alloc_pid();
    if pid == u32::MAX {
        // 进程表满：归还已分配的子栈（此前失败路径漏释放，反复失败 fork
        // 会按 256KB/次永久泄漏内核物理页）。
        let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
        unsafe {
            for p in 0..pages {
                allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }
    // 根页表 OOM：物理内存可被用户 brk/文件 mmap 耗尽后 fork 触发，必须
    // 干净失败（归还子栈）而非 panic。
    let mut child_pt = match vm::ProcPageTable::try_new_boxed() {
        Some(pt) => pt,
        None => {
            let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
            unsafe {
                for p in 0..pages {
                    allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
                }
            }
            proc_claim_release(pid);
            unsafe { asm!("msr daifclr, #2") };
            return -1;
        }
    };
    let (total_pages, copied_pages) = copy_address_space(parent_pid, &mut child_pt);
    let (child_pid, child_ttbr0) = create_child_process(parent_pid, pid, &mut child_pt);
    if child_pid == u32::MAX {
        // 子进程创建失败：回收已复制的子地址空间（用户数据页 + 私有页表
        // 结构）与子栈——此前漏释放，一次失败就泄漏整个地址空间。
        unsafe {
            child_pt.free_user_pages();
            vm::free_process_tables(child_pt.ttbr0);
        }
        let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
        unsafe {
            for p in 0..pages {
                allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        proc_claim_release(pid);
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }
    // 子进程继承父进程的 ELF 运行时信息（ASLR 栈基址/初始 sp/堆基址）：
    // 子进程地址空间是父进程的 COW 副本，用户栈布局与堆基址与父进程一致，
    // 信号帧边界校验/brk 初始化才能使用正确的随机化值。
    crate::elf::fork_runtime_info(parent_pid, child_pid);

    // 启动计时（诊断）：fork 总耗时（含地址空间复制与任务插入）
    let t1 = timer::now_us();
    crate::kinfo! {
        crate::uart_send_str("[perf] fork pid=");
        crate::print_dec(child_pid as u64);
        crate::uart_send_str(" 页数(total=");
        crate::print_dec(total_pages as u64);
        crate::uart_send_str(",拷贝=");
        crate::print_dec(copied_pages as u64);
        crate::uart_send_str(") 耗时=");
        crate::print_dec((t1 - t0) as u64);
        crate::uart_send_str("us\n");
    }

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
        // 任务槽位不足：回收子进程（地址空间/栈），返回失败。
        // ★ create_child_process 已对继承 fd 做引用 +1，必须先逐项释放再清
        // PCB，否则表满 fork 风暴下管道/队列/shm/文件引用永久泄漏（对象池
        // 耗尽、"文件仍打开"误报）。
        unsafe {
            for fd in 0..FD_MAX {
                let e = PROCESSES[child_pid as usize].fds[fd];
                match e.kind {
                    FdKind::File => crate::vfs::close_ref(e.index),
                    FdKind::Pipe => crate::ipc::pipe_drop_ref(e.index, e.flags & crate::ipc::FD_WRITE != 0),
                    FdKind::MsgQueue => crate::ipc::msgq_drop_ref(e.index),
                    FdKind::Shm => crate::ipc::shm_close_index(child_pid, e.index),
                    FdKind::Pair => crate::ipc::pair_drop_ref(e.index, e.flags as u32),
                    _ => {}
                }
            }
            PROCESSES[child_pid as usize].pt.free_user_pages();
            vm::free_process_tables(PROCESSES[child_pid as usize].pt.ttbr0);
                    {
            // 先归还 pt 的堆记录表（清零只写 null 不 drop，会泄漏 1.5MB）
            PROCESSES[child_pid as usize].pt.user_pages.release();
            let n = core::mem::size_of::<Process>() / 8;
            let p = &mut PROCESSES[child_pid as usize] as *mut Process as *mut u64;
            for j in 0..n {
                core::ptr::write_volatile(p.add(j), 0u64);
            }
        }
            let pages = stack_size.div_ceil(crate::allocator::PAGE_SIZE);
            for p in 0..pages {
                allocator::free_page(child_stack_base + p * crate::allocator::PAGE_SIZE);
            }
        }
        proc_claim_release(child_pid);
        unsafe { asm!("msr daifclr, #2") };
        return -1;
    }

    proc_claim_release(child_pid);
    unsafe { asm!("msr daifclr, #2") };
    child_pid as i32
}

// ==================== 公共接口 ====================

/// 初始化进程表：创建 pid 0（内核引导/init 父进程）并启动 init 线程。
pub fn init(init_entry: fn(usize) -> !) {
    crate::kinfo! { crate::uart_send_str("[pinit] A\n"); }
    unsafe {
        // 直接 u64 清零，避免 384KB×16 的 memcpy（TCG 下极慢）
        let n = core::mem::size_of::<Process>() / 8;
        for i in 0..MAX_PROC {
            let p = &mut PROCESSES[i] as *mut Process as *mut u64;
            for j in 0..n {
                core::ptr::write_volatile(p.add(j), 0u64);
            }
        }
    }
    crate::kinfo! { crate::uart_send_str("[pinit] B\n"); }
    let pt = vm::ProcPageTable::new_boxed();
    let ttbr0 = pt.ttbr0;
    crate::kinfo! { crate::uart_send_str("[pinit] C\n"); }
    unsafe {
        let dst = &mut PROCESSES[0] as *mut Process;
        crate::kinfo! {
            crate::uart_send_str("[pinit] dst\n");
            crate::uart_send_str("[pinit] dstaddr=");
            crate::print_hex(dst as u64);
            crate::uart_send_str(" sz=");
            crate::print_dec(core::mem::size_of::<Process>() as u64);
            crate::uart_send_str("\n");
        }
        let proc_bytes = core::mem::size_of::<Process>();
        let dstb = dst as *mut u8;
        for b in 0..proc_bytes {
            core::ptr::write_volatile(dstb.add(b), 0u8);
        }
        crate::kinfo! { crate::uart_send_str("[pinit] wb\n"); }
        (*dst).pid = 0;
        (*dst).state = ProcessState::Running;
        (*dst).parent = -1;
        (*dst).exit_code = 0;
        (*dst).waiters = [None; WAIT_WAITERS];
        (*dst).signals = 0;
        (*dst).name = [0; MAX_NAME];
        (*dst).name_len = 0;
        (*dst).cwd = DEFAULT_CWD;
        (*dst).cwd_len = 1;
        (*dst).fds = [EMPTY_FD; FD_MAX];
        crate::kinfo! { crate::uart_send_str("[pinit] fields\n"); }
        // 移动语义转入 PCB（pt 内含堆记录表 Box）：deref-move 消费 Box，
        // 防止 u64 字拷贝复制 Box 指针 + 随后 pt drop 造成的双释放。
        core::ptr::write(core::ptr::addr_of_mut!((*dst).pt), *pt);
        crate::kinfo! { crate::uart_send_str("[pinit] ptcopy\n"); }
    }
    crate::kinfo! { crate::uart_send_str("[pinit] D\n"); }
    set_name(0, "kernel");
    crate::kinfo! { crate::uart_send_str("[pinit] E\n"); }
    // init 线程：pid 0，普通优先级，地址空间为内核恒等映射
    sched_cfs::spawn(0, "kernel", init_entry, 0, PriorityClass::Normal, 0, ttbr0);
    crate::kinfo! { crate::uart_send_str("[pinit] F\n"); }
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
    // pid 槽位只有写回 PROCESSES 后才占用（state 仍为 Free），此处失败直接
    // 返回即可，槽位可复用。
    let pt = match vm::ProcPageTable::try_new_boxed() {
        Some(pt) => pt,
        None => {
            proc_claim_release(pid);
            return u32::MAX;
        }
    };
    let ttbr0 = pt.ttbr0;
    unsafe {
        let dst = &mut PROCESSES[pid as usize] as *mut Process;
        {
            let proc_q = core::mem::size_of::<Process>() / 8;
            let dstq = dst as *mut u64;
            for i in 0..proc_q {
                core::ptr::write_volatile(dstq.add(i), 0u64);
            }
        }
        (*dst).pid = pid;
        (*dst).state = ProcessState::Running;
        (*dst).parent = current_pid() as i32;
        (*dst).exit_code = 0;
        (*dst).waiters = [None; WAIT_WAITERS];
        (*dst).signals = 0;
        (*dst).name = [0; MAX_NAME];
        (*dst).name_len = 0;
        (*dst).cwd = DEFAULT_CWD;
        (*dst).cwd_len = 1;
        (*dst).fds = [EMPTY_FD; FD_MAX];
        // 移动语义转入 PCB（pt 内含堆记录表 Box，deref-move 消费 Box 防双释放）
        core::ptr::write(core::ptr::addr_of_mut!((*dst).pt), *pt);
    }
    set_name(pid, name);
    if sched_cfs::spawn(pid, name, entry, arg, class, nice, ttbr0) == u32::MAX {
        // 任务槽位耗尽：回收该进程的页表后返回失败（不泄漏）
        unsafe {
            vm::free_process_tables(ttbr0);
                    {
            // 先归还 pt 的堆记录表（清零循环只写 null 不 drop，会泄漏 1.5MB）
            PROCESSES[pid as usize].pt.user_pages.release();
            let n = core::mem::size_of::<Process>() / 8;
            let p = &mut PROCESSES[pid as usize] as *mut Process as *mut u64;
            for j in 0..n {
                core::ptr::write_volatile(p.add(j), 0u64);
            }
        }
        }
        proc_claim_release(pid);
        return u32::MAX;
    }
    proc_claim_release(pid);
    pid
}

/// 用给定的地址空间创建新进程（不创建线程），返回 pid。
///
/// 供 ELF 加载器使用：先构建好用户地址空间（代码 + 栈 + 数据映射），
/// 再注册进程并自行 spawn 用户线程。
pub fn create_process(mut pt: alloc::boxed::Box<vm::ProcPageTable>, name: &str) -> u32 {
    let pid = alloc_pid();
    if pid == u32::MAX {
        // ★ 进程表满：Box 只会被 drop 释放 24KB 结构体本身——已加载的用户
        // 页与派生页表（可达数 MB）全部泄漏，进程表反复打满即 OOM。这里
        // 显式回收整个新地址空间后再返回。
        pt.free_user_pages();
        vm::free_process_tables(pt.ttbr0);
        return u32::MAX;
    }
    unsafe {
        // 逐字段写入 + 直接 memcpy 页表：避免 Process 结构体构造在栈上
        // 产生 96KB+ 临时（叠加 execve/spawn 深调用链会溢出内核任务栈）。
        let dst = &mut PROCESSES[pid as usize] as *mut Process;
        {
            let proc_q = core::mem::size_of::<Process>() / 8;
            let dstq = dst as *mut u64;
            for i in 0..proc_q {
                core::ptr::write_volatile(dstq.add(i), 0u64);
            }
        }
        (*dst).pid = pid;
        (*dst).state = ProcessState::Running;
        (*dst).parent = current_pid() as i32;
        (*dst).exit_code = 0;
        (*dst).waiters = [None; WAIT_WAITERS];
        (*dst).signals = 0;
        (*dst).name = [0; MAX_NAME];
        (*dst).name_len = 0;
        (*dst).cwd = DEFAULT_CWD;
        (*dst).cwd_len = 1;
        (*dst).fds = [EMPTY_FD; FD_MAX];
        // 移动语义转入 PCB（pt 内含堆记录表 Box，deref-move 消费 Box 防双释放）
        core::ptr::write(core::ptr::addr_of_mut!((*dst).pt), *pt);
    }
    set_name(pid, name);
    proc_claim_release(pid);
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
///
/// 同进程同 VA 的重复记录（同一映射被多次登记）不属于双重分配，只做一次
/// 汇总提示（安静模式不上串口）；重复登记本身意味着该页退出时会被
/// `free_user_pages` 释放两次，需排查登记来源。
pub fn check_double_allocations() {
    // 1) 全进程对（含自身）私有页重复物理地址检查
    for a in 0..MAX_PROC {
        let pa = unsafe { &PROCESSES[a] };
        if pa.pt.ttbr0 == 0 {
            continue;
        }
        // 同进程内同 VA 重复登记计数（同一映射多条记录 → 退出时双重释放隐患）
        let mut dup_records = 0usize;
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
                        && !(a == b && q.vaddr == p.vaddr)
                        && !(vm::is_user_ro(q.flags) && !vm::is_cow(q.flags))
                    {
                        crate::uart_send_str("\n[DOUBLE-ALLOC] paddr=");
                        crate::print_hex(p.paddr as u64);
                        crate::uart_send_str(" proc ");
                        crate::print_dec(a as u64);
                        crate::uart_send_str(" va=");
                        crate::print_hex(p.vaddr as u64);
                        crate::uart_send_str(" 与 proc ");
                        crate::print_dec(b as u64);
                        crate::uart_send_str(" va=");
                        crate::print_hex(q.vaddr as u64);
                        crate::uart_send_str("\n");
                    }
                }
            }
        }
        // 同 VA 重复登记：只汇总一条（安静模式不上串口），避免刷屏
        for i in 0..pa.pt.count {
            let p = pa.pt.user_pages[i];
            if p.paddr == 0 || p.paddr == vm::LAZY_PAGE {
                continue;
            }
            for j in 0..i {
                let q = pa.pt.user_pages[j];
                if q.vaddr == p.vaddr && q.paddr != vm::LAZY_PAGE {
                    dup_records += 1;
                    break;
                }
            }
        }
        if dup_records > 0 {
            crate::kinfo! {
                crate::uart_send_str("[dbl-track] proc ");
                crate::print_dec(a as u64);
                crate::uart_send_str(" 存在 ");
                crate::print_dec(dup_records as u64);
                crate::uart_send_str(" 条同 VA 重复页记录（退出时会双重释放，需排查登记来源）\n");
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
                crate::uart_send_str(" 在内核区 [");
                crate::print_hex(kstart as u64);
                crate::uart_send_str(",");
                crate::print_hex(kend as u64);
                crate::uart_send_str(")\n");
            }
        }
    }
}

/// 按根表物理地址反查进程（供 vm 层做记录级映射校验）。
///
/// 用户映射的权威来源是 `ProcPageTable::user_pages` 记录表：所有用户页表
/// 继承内核 L1[0] 的 1GB 设备块，纯页表漫游会把任意低地址（含设备区）误判
/// 为已映射。`user_range_mapped` 等校验改为先由本函数定位进程，再查记录。
fn pid_by_root(root: usize) -> i32 {
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state != ProcessState::Free && PROCESSES[i].pt.ttbr0 == root {
                return i as i32;
            }
        }
    }
    -1
}

/// 校验根表 `root` 对应进程的记录表中 `page` 页已建立真实映射
/// （有记录且物理页已分配：`paddr != 0` 且非 `LAZY_PAGE` 预留）。
pub fn root_page_recorded(root: usize, page: usize) -> bool {
    let pid = pid_by_root(root);
    if pid < 0 {
        return false;
    }
    unsafe {
        let pt = &PROCESSES[pid as usize].pt;
        for i in 0..pt.count {
            let r = pt.user_pages[i];
            if r.vaddr == page && r.paddr != 0 && r.paddr != vm::LAZY_PAGE {
                return true;
            }
        }
    }
    false
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
pub fn replace_address_space(pid: u32, new_pt: alloc::boxed::Box<vm::ProcPageTable>) {
    let new_ttbr0 = new_pt.ttbr0;
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        let old_ttbr0 = p.pt.ttbr0;
        // 1) 先释放旧地址空间的**用户数据页**（物理页归还）。
        //    此刻仍运行在旧/新根表共有的内核恒等映射上，不触碰用户区，安全。
        p.pt.free_user_pages();
        // 2) 把新页表整体移入进程表（含新 ttbr0）。移动语义：旧值（其堆记录
        //    表 Box 在 free_user_pages 清空记录后仍占 1.5MB）先 read 出来，
        //    新值 deref-move 写入（消费 new_pt 的 Box，不复制指针），再 drop
        //    旧值归还其堆内存 —— u64 字拷贝会复制 Box 指针，导致旧表泄漏 +
        //    新表双释放。
        {
            let old = core::ptr::read(core::ptr::addr_of!(p.pt));
            core::ptr::write(core::ptr::addr_of_mut!(p.pt), *new_pt);
            drop(old);
        }
        // 3) **先把硬件 TTBR0 切到新根表**（新根表同样继承内核恒等映射，指令
        //    取指/数据访问在新根表上继续成立），再做 TLB 全清。此后内核不再
        //    通过旧根表做任何翻译。
        sched_cfs::set_current_ttbr0(new_ttbr0);
        // 4) 最后释放旧地址空间的**私有页表结构**（旧 L2/L3/根表）。
        //    ★顺序修正：此前先释放旧根表再切 TTBR0，窗口内 TTBR0 仍指向已
        //    归还的页表页——任何 IRQ/内存分配都会让翻译命中已复用内存（悬空
        //    页表 → data abort / 随机翻译错误）。先切后释放消除该窗口。
        vm::free_process_tables(old_ttbr0);
    }
}

/// 复制当前进程：父进程返回子 pid，子进程返回 0。
///
/// `frame` 为当前系统调用陷阱帧指针（用于定位子进程陷阱帧副本，置其 x[0]=0）。
pub fn fork(frame: usize) -> i32 {
    let pid = unsafe { do_fork(frame) };
    // black_box 阻止尾调用优化，保证 do_fork 的返回地址存在（子进程在此返回 0）
    core::hint::black_box(pid)
}

/// 诊断：在用户页表记录中查找 `va` 页（供故障定位）。
pub fn processes_pub_find(pid: u32, va: usize) -> i64 {
    if (pid as usize) >= MAX_PROC {
        return -1;
    }
    let page = va & !(vm::PAGE_SIZE - 1);
    unsafe {
        let p = &PROCESSES[pid as usize];
        for i in 0..p.pt.count {
            if p.pt.user_pages[i].vaddr == page {
                return p.pt.user_pages[i].paddr as i64;
            }
        }
    }
    -1
}

/// 公开指定进程的页表根（供故障诊断使用）。
pub fn processes_pub_ttbr0(pid: u32) -> usize {
    if (pid as usize) < MAX_PROC {
        unsafe { PROCESSES[pid as usize].pt.ttbr0 }
    } else {
        0
    }
}

/// 强制回收一个「刚创建即失败」的进程（如任务槽位耗尽）：释放地址空间、
/// fd 引用与进程槽位。进程从未被调度（无线程），无需任务清理。
pub fn reap_forced(pid: u32) {
    if (pid as usize) >= MAX_PROC {
        return;
    }
    // SMP：与 alloc_pid/reap 互斥（清零期间他核不得认领复用）。非阻塞：
    // 认领失败即放弃（本函数仅「刚创建即失败」的单一调用路径）。
    if PROC_CLAIM[pid as usize].compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    unsafe {
        let ttbr0 = PROCESSES[pid as usize].pt.ttbr0;
        for fd in 0..FD_MAX {
            let e = PROCESSES[pid as usize].fds[fd];
            match e.kind {
                FdKind::File => crate::vfs::close_ref(e.index),
                FdKind::Pipe => crate::ipc::pipe_drop_ref(e.index, e.flags & crate::ipc::FD_WRITE != 0),
                FdKind::MsgQueue => crate::ipc::msgq_drop_ref(e.index),
                FdKind::Shm => crate::ipc::shm_close_index(pid, e.index),
                FdKind::Pair => crate::ipc::pair_drop_ref(e.index, e.flags as u32),
                _ => {}
            }
        }
        PROCESSES[pid as usize].pt.free_user_pages();
        vm::free_process_tables(ttbr0);
        // 复位该 pid 槽位的用户堆断点/mmap 游标：槽位会被复用（如 OOBE 退出后
        // launcher 复用同一 pid），遗留的未对齐断点会让新进程堆分配写越界。
        crate::bionic_shim::reset_brk_state(pid as usize);
        // 清 ELF 运行时信息：pid 复用给非 ELF 进程时，陈旧的 ASLR 栈基址/brk
        // 会被 current_stack_base()/current_brk() 当作新进程的值使用。
        crate::elf::release(pid);
        {
            // 先归还 pt 的堆记录表（清零只写 null 不 drop，会泄漏 1.5MB）
            PROCESSES[pid as usize].pt.user_pages.release();
            let n = core::mem::size_of::<Process>() / 8;
            let p = &mut PROCESSES[pid as usize] as *mut Process as *mut u64;
            for j in 0..n {
                core::ptr::write_volatile(p.add(j), 0u64);
            }
        }
    }
    proc_claim_release(pid);
}

/// 回收指定进程：释放用户数据页与私有页表，清空 fd 表，槽位标记 Free。
///
/// 调用前提：目标进程已是 Zombie 且不再被调度（TTBR0 已指向其它进程）。
/// 返回 `Some(退出码)` = 本调用完成回收；`None` = 未回收（并发竞争失败
/// 或 SMP 护栏命中，槽位留待 wait 重试）。
///
/// ★ 防重入：多线程父进程的两个线程可被同一次 exit 的 wake_pid 同时唤醒、
/// 双方都扫到同一僵尸。先在关中断窗口内把状态从 Zombie 摘除，只有摘除
/// 成功者继续释放 —— 此前无此防护，双重 reap = 双重 free_process_tables
/// （把已复用的页二次归还分配器，后续 alloc 会把同一物理页发给两个进程）
/// + fd 引用错减。
fn reap(pid: u32) -> Option<i32> {
    unsafe {
        // 原子认领：Zombie → Free（释放期间并发扫描看不到它）。
        // ★ SMP：先取槽位认领位再置 Free——否则 alloc_pid 可在他核看到
        // Free 即认领并写入新 PCB，与本函数后续的 fd/页表释放及 PCB 清零
        // 交错（新页表被释放、新 PCB 被清零）。认领持有至 PCB 清零完成。
        if PROC_CLAIM[pid as usize]
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None; // 他核正在分配/回收此槽位：wait 循环稍后重试
        }
        let saved = crate::sched_cfs::read_daif();
        core::arch::asm!("msr daifset, #2");
        if PROCESSES[pid as usize].state != ProcessState::Zombie {
            crate::sched_cfs::write_daif(saved);
            proc_claim_release(pid);
            return None;
        }
        PROCESSES[pid as usize].state = ProcessState::Free;
        crate::sched_cfs::write_daif(saved);
        let code = PROCESSES[pid as usize].exit_code;
        let ttbr0 = PROCESSES[pid as usize].pt.ttbr0;
        crate::kinfo! {
            crate::uart_send_str("[reap] pid=");
            crate::print_dec(pid as u64);
            crate::uart_send_str(" slot.pid=");
            crate::print_dec(PROCESSES[pid as usize].pid as u64);
            crate::uart_send_str(" ttbr0=");
            crate::print_hex(ttbr0 as u64);
            crate::uart_send_str("\n");
        }
        // 释放该进程全部 fd 资源（文件引用计数 -1、管道/队列/共享内存/套接字对
        // 槽位释放），防止对象池耗尽 panic 与文件表项泄漏。
        // ★ SMP 护栏必须先行：目标仍有存活任务（他核 kill_req 待兑现）时其
        // 地址空间正在被使用，先释放页表/fd = use-after-free。恢复 Zombie
        // 并放弃（wait 循环重试），只返回退出码。
        if crate::sched_cfs::has_live_tasks(pid) {
            let saved2 = crate::sched_cfs::read_daif();
            core::arch::asm!("msr daifset, #2");
            PROCESSES[pid as usize].state = ProcessState::Zombie;
            crate::sched_cfs::write_daif(saved2);
            proc_claim_release(pid);
            return None;
        }
        for fd in 0..FD_MAX {
            let e = PROCESSES[pid as usize].fds[fd];
            match e.kind {
FdKind::File => crate::vfs::close_ref(e.index),
                FdKind::Pipe => crate::ipc::pipe_drop_ref(e.index, e.flags & crate::ipc::FD_WRITE != 0),
                FdKind::MsgQueue => crate::ipc::msgq_drop_ref(e.index),
                FdKind::Shm => crate::ipc::shm_close_index(pid, e.index),
                FdKind::Pair => crate::ipc::pair_drop_ref(e.index, e.flags as u32),
                _ => {}
            }
        }
        PROCESSES[pid as usize].pt.free_user_pages();
        vm::free_process_tables(ttbr0);
        // 回收该进程全部已退出线程的内核栈与 TCB 槽位（防 alloc_tid 耗尽）
        crate::sched_cfs::reap_pid(pid);
        // 复位该 pid 槽位的用户堆断点/mmap 游标：槽位会被复用（如 OOBE 退出后
        // launcher 复用同一 pid），遗留的未对齐断点会让新进程堆分配写越界。
        crate::bionic_shim::reset_brk_state(pid as usize);
        // 清 ELF 运行时信息与信号 handler 表（与 reap_forced 对齐）：pid
        // 复用给非 ELF 进程时陈旧的 ASLR 栈基址/brk 会被 current_stack_base()
        // /current_brk() 当作新进程的值使用；SIG handler 残留则会把本应默认
        // 终止的信号跳转到新地址空间里的垃圾地址。
        crate::elf::release(pid);
        SIG_TABLES[pid as usize] = EMPTY_SIG;
        {
            // 先归还 pt 的堆记录表（清零只写 null 不 drop，会泄漏 1.5MB）
            PROCESSES[pid as usize].pt.user_pages.release();
            let n = core::mem::size_of::<Process>() / 8;
            let p = &mut PROCESSES[pid as usize] as *mut Process as *mut u64;
            for j in 0..n {
                core::ptr::write_volatile(p.add(j), 0u64);
            }
        }
        proc_claim_release(pid);
        Some(code)
    }
}

/// 孤儿收养：`pid` 进程退出时，将其全部存活子进程重新挂到 init（pid 1）名下。
///
/// 此前无收养逻辑：父退出后子进程的 `parent` 仍指向已回收的槽位，任何进程
/// （包括 init）的 `wait(-1)` 都扫描不到它们 —— 子进程退出后成为无人收割的
/// 僵尸，永久占用 `MAX_PROC` 槽位（反复 fork/退出的服务会慢慢耗尽进程表）。
/// 收养后子进程退出时 `exit` 会唤醒 init（parent = 1），init 的 wait 循环可
/// 正常收割。
fn adopt_orphans(pid: u32) {
    unsafe {
        for i in 0..MAX_PROC {
            if i == pid as usize {
                continue;
            }
            if PROCESSES[i].state != ProcessState::Free && PROCESSES[i].parent == pid as i32 {
                PROCESSES[i].parent = 1;
            }
        }
    }
}

/// 当前进程退出（唤醒等待者，回收当前线程，不再返回）。
pub fn exit(code: i32) -> ! {
    // 屏蔽 IRQ，保证「终止兄弟线程 → 标记僵尸 → 唤醒等待者 → 切走」整个过程原子。
    unsafe { asm!("msr daifset, #2") };
    let pid = current_pid();
    unsafe {
        let ttbr: u64;
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr);
        crate::kinfo! {
            crate::uart_send_str("[exit] pid=");
            crate::print_dec(pid as u64);
            crate::uart_send_str(" pt.ttbr0=");
            crate::print_hex(PROCESSES[pid as usize].pt.ttbr0 as u64);
            crate::uart_send_str(" reg=");
            crate::print_hex(ttbr);
            crate::uart_send_str("\n");
        }
    }
    // 终止同进程的所有线程：多线程进程退出时，兄弟线程必须在此之前停止，
    // 否则父进程 reap 释放页表后它们会以悬空 TTBR0 继续运行。
    sched_cfs::kill_pid(pid);
    // 孤儿收养：本进程退出后其子进程改挂到 init（pid 1）下。否则这些子进程
    // 保持孤儿身份：父槽位被回收后无人能 wait 它们，僵尸永占 MAX_PROC 槽位。
    adopt_orphans(pid);
    unsafe {
        PROCESSES[pid as usize].state = ProcessState::Zombie;
        PROCESSES[pid as usize].exit_code = code;
        // 唤醒全部等待本进程的线程（多等待者：并发 wait 各自登记，逐个唤醒
        // 并清槽——防止只唤醒一个，其余等待线程永久阻塞）
        for i in 0..WAIT_WAITERS {
            if let Some(wt) = PROCESSES[pid as usize].waiters[i] {
                PROCESSES[pid as usize].waiters[i] = None;
                sched_cfs::wake_irq(wt);
            }
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

/// 线程级退出（Linux `exit(93)` 语义）：仅退出当前线程；只有当本线程是
/// 进程最后一个存活任务时才升级为完整进程退出。CLONE_THREAD 工作线程的
/// 收尾（pthread_exit / 线程函数返回）走此处 —— 若误走 [`exit`]，任一线程
/// 结束都会 kill_pid 整个线程组，pthread_join 永远等不到、主线程被杀。
pub fn exit_thread(code: i32) -> ! {
    // CLONE_CHILD_CLEARTID：清 tid + futex 唤醒 join 方（须在地址空间仍
    // 映射时执行；最后一线程走 exit() 整体退出，无 join 方需要唤醒）
    crate::bionic_shim::exit_thread_notify();
    if !sched_cfs::exit_thread_current() {
        exit(code);
    }
    // exit_thread_current 返回 true 时已切换走，不会到达
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

/// 强制终止任意进程（SIGKILL 语义，供 lifd/LMK 使用）。
///
/// 与 `exit`（仅当前进程）不同：本函数在**发送方上下文**执行，可终止
/// 任意 pid 的进程——包括 SIGSTOP 停止（后台/挂起）的进程。停止态进程
/// 不会再次进入内核处理待处理信号，因此 SIGKILL 不能走"置位+唤醒"路径，
/// 必须在此直接终止其全部任务、标记僵尸并唤醒父进程/等待者。
///
/// 调用方须已屏蔽 IRQ 或处于 syscall 上下文（`sys_kill` 路径）。
pub fn terminate(pid: u32) {
    if (pid as usize) >= MAX_PROC {
        return;
    }
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        if p.state == ProcessState::Free || p.state == ProcessState::Zombie {
            return;
        }
    }
    sched_cfs::kill_pid(pid);
    adopt_orphans(pid);
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        p.state = ProcessState::Zombie;
        p.exit_code = 137; // 128 + SIGKILL(9)
        // 唤醒全部等待本进程的线程（多等待者：并发 wait 各自登记，逐个唤醒）
        for i in 0..WAIT_WAITERS {
            if let Some(wt) = p.waiters[i] {
                p.waiters[i] = None;
                sched_cfs::wake_irq(wt);
            }
        }
        let parent = p.parent;
        if parent >= 0 {
            sched_cfs::wake_pid(parent as u32);
            if parent < MAX_PROC as i32 && sig_handler(parent as u32, 17) != 0 {
                signal_send(parent as u32, 17);
            }
        }
    }
    // 终止目标包含当前任务（kill(getpid(), SIGKILL) 自杀路径）：立即走
    // exit_current 等价路径（置 Exited 并切走），不等下一 tick 兑现 kill_req，
    // 消除「已标记 Zombie 的进程仍运行至多 10ms」的窗口。父进程随后 reap 时
    // 目标任务必然已切走（单核安全，reap_pid 另有存活任务护栏兜底 SMP）。
    if sched_cfs::current_pid() == pid {
        sched_cfs::exit_current();
    }
}

/// 判断当前进程是否存在已退出的子进程（Zombie，等待回收）。
///
/// 供 `wait4(WNOHANG)` 使用：存在则 `wait` 可立即回收，否则调用方
/// 不阻塞直接返回 0。`pid = -1`/`0` 查任意子进程，`pid > 0` 查指定子进程。
pub fn has_zombie_child(pid: i32) -> bool {
    let cur = current_pid();
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state != ProcessState::Zombie {
                continue;
            }
            if PROCESSES[i].parent != cur as i32 {
                continue;
            }
            if pid == -1 || pid == 0 || pid as usize == i {
                return true;
            }
        }
    }
    false
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
                if let Some(code) = reap(z) {
                    return (z as i32, code);
                }
                // 被并发线程抢先收走（或 SMP 护栏命中）：让出后重扫
                sched_cfs::yield_now();
                continue;
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
            // ★ 丢唤醒修复：「查无僵尸」与 block_current 之间被 tick 抢占时，
            // 子进程 exit 的 wake_pid 只唤醒 Blocked 任务（此时父进程是 Ready）
            // → 父进程随后阻塞且再无人唤醒。把检查到阻塞包进 IRQ 屏蔽窗口。
            let saved = unsafe { sched_cfs::read_daif() };
            unsafe { core::arch::asm!("msr daifset, #2") };
            let mut zombie2 = None;
            unsafe {
                for i in 0..MAX_PROC {
                    if PROCESSES[i].state == ProcessState::Zombie
                        && PROCESSES[i].parent == cur as i32
                    {
                        zombie2 = Some(i as u32);
                        break;
                    }
                }
            }
            match zombie2 {
                Some(z) => {
                    unsafe { sched_cfs::write_daif(saved) };
                    if let Some(code) = reap(z) {
                        return (z as i32, code);
                    }
                    // 与首路径一致让出重扫（reap 开中断窗口内的退出唤醒
                    // 只唤醒 Blocked 任务，盲目 block 会丢唤醒）。
                    sched_cfs::yield_now();
                    continue;
                }
                None => {
                    sched_cfs::block_current();
                    unsafe { sched_cfs::write_daif(saved) };
                }
            }
        }
    } else if pid > 0 && (pid as usize) < MAX_PROC {
        loop {
            // ★ 丢唤醒修复：登记 waiter 与 block_current 之间被 tick 抢占时，
            // 子进程 exit 会清掉 waiter 槽且 wake_irq 只唤醒 Blocked 任务
            //（此时父进程是 Ready）→ 父进程随后阻塞且再无人唤醒。
            // 把「状态检查 + 登记 + 阻塞」包进 IRQ 屏蔽窗口。
            let saved = unsafe { sched_cfs::read_daif() };
            unsafe { core::arch::asm!("msr daifset, #2") };
            let action = unsafe {
                let p = &mut PROCESSES[pid as usize];
                if p.state == ProcessState::Free {
                    unsafe { sched_cfs::write_daif(saved) };
                    return (-1, 0);
                }
                // ★ wait(pid) 只允许等待**自己的子进程**：pid 不是当前进程的子进程
                // （别人的兄弟/祖先/其它进程）立即 ECHILD，不阻塞等待、不收割他人
                // 僵尸——此前缺失亲子校验，任意进程可永久阻塞在别人的进程上或
                // 抢先回收他人已退出子进程（wait() 与真正父进程的 reap 竞态）。
                if p.parent != cur as i32 {
                    unsafe { sched_cfs::write_daif(saved) };
                    return (-1, 0);
                }
match p.state {
                    ProcessState::Zombie => Some(()),
                    _ => {
                        // 多等待者：登记到第一个空闲槽位（防并发 wait 覆写单槽
                        // 导致先等待的线程在退出唤醒时被漏掉、永久阻塞）。槽位
                        // 满的极端场景（多线程并发 wait 同一子进程）不登记直接
                        // 阻塞——唤醒由 wait(-1) 路径的 wake_pid 兜底覆盖。
                        for w in p.waiters.iter_mut() {
                            if w.is_none() {
                                *w = Some(sched_cfs::current_tid());
                                break;
                            }
                        }
                        None
                    }
                }
            };
            if action.is_some() {
                unsafe { sched_cfs::write_daif(saved) };
                if let Some(code) = reap(pid as u32) {
                    return (pid, code);
                }
                // 被并发抢先收走（state 已 Free → 下一轮 ECHILD）或护栏
                // 命中（恢复 Zombie → 重试）：让出后重扫（不在此阻塞 ——
                // 本路径 Zombies 场景下未登记 waiter）。
                sched_cfs::yield_now();
                continue;
            }
            sched_cfs::block_current();
            unsafe { sched_cfs::write_daif(saved) };
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

/// 进程级统计（LMK / procfs 用）：对每个存活进程调用
/// `f(pid, 进程名, 用户物理页数, 是否 SIGSTOP 停止)`。
///
/// 用户页数来自地址空间映射记录（`pt.count`），是 LMK 评分的内存口径；
/// 停止态供诊断显示（/proc/processes 的 T 标记）。
pub fn proc_stats(f: &mut dyn FnMut(u32, &str, usize, bool)) {
    unsafe {
        for i in 0..MAX_PROC {
            if PROCESSES[i].state == ProcessState::Free {
                continue;
            }
            let name = core::str::from_utf8(&PROCESSES[i].name[..PROCESSES[i].name_len])
                .unwrap_or("?");
            let pages = PROCESSES[i].pt.count;
            let stopped = crate::sched_cfs::process_stopped(PROCESSES[i].pid);
            f(PROCESSES[i].pid, name, pages, stopped);
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
///
/// ★ 位图读-改-写屏蔽 IRQ：handle_pending 的 clear 与他进程 kill 的 send
/// 并发交错会丢新置的位（含 SIGKILL 静默丢弃）。
pub fn signal_clear(pid: u32, sig: u32) {
    if (pid as usize) < MAX_PROC && (sig as usize) < 64 {
        let saved = unsafe { sched_cfs::read_daif() };
        unsafe { core::arch::asm!("msr daifset, #2") };
        unsafe {
            PROCESSES[pid as usize].signals &= !(1u64 << sig);
        }
        unsafe { sched_cfs::write_daif(saved) };
    }
}

pub fn signal_send(pid: u32, sig: u32) {
    if sig >= 64 || (pid as usize) >= MAX_PROC {
        return;
    }
    let saved = unsafe { sched_cfs::read_daif() };
    unsafe { core::arch::asm!("msr daifset, #2") };
    unsafe {
        PROCESSES[pid as usize].signals |= 1u64 << sig;
    }
    unsafe { sched_cfs::write_daif(saved) };
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
        if !vm::map_page_in(root, vaddr, paddr, vm::FLAG_USER_DATA) {
            // 页表 OOM：映射缺失，进程访问该页将 SIGSEGV（比内核 panic 好）。
            crate::uart_send_str("[map] map_user_page OOM vaddr=");
            crate::print_hex(vaddr as u64);
            crate::uart_send_str("\n");
        }
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
                // 页表 OOM：归还物理页并返回 false（SIGSEGV），
                // 避免映射缺失却更新记录导致缺页无限重入。
                if !vm::map_page_in(p.pt.ttbr0, page, pa, rec.flags) {
                    allocator::free_page(pa);
                    crate::uart_send_str("[fault] 页表 OOM\n");
                    return false;
                }
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
                // 页表 OOM：归还新页并返回 false（SIGSEGV），
                // 避免记录指向新页而 PTE 仍是旧共享页的不一致状态。
                if !vm::map_page_in(p.pt.ttbr0, page, new_pa, vm::cow_pte_flags(new_flags)) {
                    allocator::free_page(new_pa);
                    crate::uart_send_str("[cow] 页表 OOM\n");
                    return false;
                }
                p.pt.user_pages[i] = vm::UserPage { vaddr: page, paddr: new_pa, flags: new_flags };
                // 3) 共享页引用 -1，归零释放旧页
                // 注意：不在此处逐页打印 —— 每次 COW break 都做阻塞串口输出
                // 会让启动动画期间每帧写显存/back buffer 被串口拖慢数秒，且
                // 阻塞 UART RX 导致注入的触摸帧丢失；总量由 COW_BREAKS 汇总，
                // 引导结束时统一打印。
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
/// COW 共享页 break 出私有副本、共享只读页（ELF 代码/rodata、信号蹦床，
/// `AP_USER_RO`=0b11 在 **EL1 同样只读**）复制出私有可写副本。
///
/// 供内核态**写入**用户缓冲区前调用（`copy_to_user` 等路径）。COW 页在页表
/// 中映射为只读，EL1 直接写会触发 permission fault → 同步异常处理器 panic
/// （整机挂死）；共享只读池页同理。因此写前必须把两类页全部处理为私有可写。
///
/// 返回是否成功；范围非法、任一页落地失败或复制失败（OOM）返回 `false`
/// （调用方按 EFAULT/SIGSEGV 语义处理，**绝不能**带着只读映射继续写）。
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
        // 查记录权限：COW 共享页 → break；AP_USER_RO 页（ELF text/rodata、
        // 蹦床）→ 复制私有副本。两者都走 mprotect_page（其内部对 COW/共享
        // 只读/普通页分支处理完备，OOM 返回 false）。
        // 注意：无记录的共享映射（SHM/framebuffer，经页表漫游放行）无法在
        // 此处理——它们映射为 RW，EL1 写安全，跳过即可。
        if let Some(fl) = current_page_flags(cur) {
            if vm::is_cow(fl) || vm::is_user_ro(fl) {
                if !mprotect_page(cur, vm::AP_USER_RW) {
                    return false;
                }
            }
        }
        cur += vm::PAGE_SIZE;
    }
    true
}

/// 查询当前进程 `vaddr` 所在页的记录 flags；不在记录表返回 `None`。
fn current_page_flags(vaddr: usize) -> Option<vm::MapFlags> {
    let page = vaddr & !(vm::PAGE_SIZE - 1);
    let pid = current_pid();
    unsafe {
        let p = &PROCESSES[pid as usize];
        for i in 0..p.pt.count {
            if p.pt.user_pages[i].vaddr == page {
                return Some(p.pt.user_pages[i].flags);
            }
        }
    }
    None
}

/// `mprotect` 单页实现：修改 `vaddr` 所在页的访问权限为 `ap`
/// （`vm::AP_USER_RW` 或 `vm::AP_USER_RO`），同步页表与地址空间记录。
///
/// 语义（与 syscall 层 W^X 策略配套）：
/// - LAZY 预留页：仅更新记录权限，落地时按新权限建映射（`page_fault_resolve`
///   用记录 flags 映射）；
/// - COW 共享页授予写权限：先 `cow_break` 出本进程私有副本（否则 EL1/EL0
///   写入会穿透只读页表污染其它进程）；
/// - 共享只读池页（ELF 代码/rodata，`AP_USER_RO` 无 COW）授予写权限：
///   复制出私有页并解除共享（`unshare_page` 引用 -1），防止写穿其它进程；
/// - 授予只读：清除 COW 标记、保持共享语义（退出时按共享池引用释放）。
///
/// 返回是否成功（页不在记录表中 / 内存不足返回 `false`）。
pub fn mprotect_page(vaddr: usize, ap: vm::MapFlags) -> bool {
    if !vm::user_va_range_ok(vaddr, vm::PAGE_SIZE) {
        return false;
    }
    let page = vaddr & !(vm::PAGE_SIZE - 1);
    let pid = current_pid();
    unsafe {
        let p = &mut PROCESSES[pid as usize];
        for i in 0..p.pt.count {
            if p.pt.user_pages[i].vaddr != page {
                continue;
            }
            // LAZY 预留页：只改记录，落地时生效
            if p.pt.user_pages[i].paddr == vm::LAZY_PAGE {
                let rec = &mut p.pt.user_pages[i];
                rec.flags = (rec.flags & !vm::AP_MASK & !vm::FLAG_COW) | ap;
                return true;
            }
            let wants_rw = ap == vm::AP_USER_RW;
            if wants_rw && (vm::is_cow(p.pt.user_pages[i].flags) || vm::is_user_ro(p.pt.user_pages[i].flags))
            {
                // COW 页：break 出私有副本（记录被更新为私有 RW）
                if vm::is_cow(p.pt.user_pages[i].flags) {
                    if !cow_break(page) {
                        return false;
                    }
                    // cow_break 后记录已是私有 RW，权限目标即达成
                    if ap == vm::AP_USER_RW {
                        return true;
                    }
                } else {
                    // 共享只读池页：手动复制出私有页并解除共享
                    let old_pa = p.pt.user_pages[i].paddr;
                    let Some(new_pa) = allocator::alloc_page() else { return false };
                    core::ptr::copy_nonoverlapping(old_pa as *const u8, new_pa as *mut u8, vm::PAGE_SIZE);
                    let new_flags = (p.pt.user_pages[i].flags & !vm::AP_MASK & !vm::FLAG_COW) | ap;
                    // 先映射新页（覆盖旧 PTE），成功后再解除旧页共享：
                    // 映射失败时旧页仍完好，不会留下指向已释放页的 PTE。
                    if !vm::map_page_in(p.pt.ttbr0, page, new_pa, vm::cow_pte_flags(new_flags)) {
                        allocator::free_page(new_pa);
                        crate::uart_send_str("[mprotect] 页表 OOM\n");
                        return false;
                    }
                    if allocator::unshare_page(old_pa) {
                        allocator::free_page(old_pa);
                    }
                    p.pt.user_pages[i] = vm::UserPage { vaddr: page, paddr: new_pa, flags: new_flags };
                    return true;
                }
            }
            // 普通路径：更新 AP（清除 COW 标记；只读页保持共享池语义）
            let rec = p.pt.user_pages[i];
            let new_flags = (rec.flags & !vm::AP_MASK & !vm::FLAG_COW) | ap;
            // 页表 OOM：保持旧映射与记录一致，mprotect 失败返回 false。
            if !vm::map_page_in(p.pt.ttbr0, page, rec.paddr, vm::cow_pte_flags(new_flags)) {
                crate::uart_send_str("[mprotect] 页表 OOM\n");
                return false;
            }
            p.pt.user_pages[i] = vm::UserPage { vaddr: page, paddr: rec.paddr, flags: new_flags };
            return true;
        }
    }
    false
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

/// 读取进程 `pid` 的第 `index` 条用户映射记录，返回 `(vaddr, paddr, flags)`。
///
/// 越界或进程无效时返回 `(u64::MAX, u64::MAX, u64::MAX)` 哨兵（供
/// crash tombstone 的内存映射转储与调试工具迭代）。只读，无需屏蔽 IRQ。
pub fn user_map_at(pid: u32, index: usize) -> (u64, u64, u64) {
    if (pid as usize) >= MAX_PROC {
        return (u64::MAX, u64::MAX, u64::MAX);
    }
    unsafe {
        let p = &PROCESSES[pid as usize];
        if p.state == ProcessState::Free || index >= p.pt.count {
            return (u64::MAX, u64::MAX, u64::MAX);
        }
        let pg = p.pt.user_pages[index];
        (pg.vaddr as u64, pg.paddr as u64, pg.flags as u64)
    }
}
