//! 用户态信号处理（sigaction handler + 信号帧 + sigreturn 蹦床）。
//!
//! 机制：
//! - 每进程两张表：`sig_handlers[64]`（0 = 默认动作）与 `sig_masks[64]`。
//! - 信号投递时机：syscall 入口（`linux_compat::dispatch`）检查待处理位图，
//!   对注册了 handler 的信号，把用户现场压入**用户栈**上的信号帧
//!   （304 字节，布局 = TrapFrame 头 + signum），然后：
//!     - `elr` = handler 入口，`a0` = signum，`ra` = 蹦床地址
//!   handler 返回时经蹦床 `rt_sigreturn` 恢复现场并继续原断点。
//! - 蹦床：所有用户地址空间在固定 VA（`SIG_TRAMP_VA`）映射同一份只读代码页
//!   （`li a7, #139; ecall; j .`），fork/exec 经页表继承。
//!
//! 信号帧布局（内核写入 / `rt_sigreturn` 读取；与 TrapFrame 头同构）：
//! ```text
//! 0x000  x0..x31（256 字节）
//! 0x100  sp（信号打断时的用户栈指针）
//! 0x108  elr（sepc，信号打断时的用户 PC）
//! 0x110  sstatus
//! 0x118  signum
//! 0x128  填充到 16 对齐
//! 0x130  帧总大小 304
//! ```

use crate::allocator;
use crate::process;
use crate::syscall_table::TrapFrame;
use crate::vm;

/// 信号蹦床固定用户地址（低于 fb mmap 0x20000000，与 ELF/堆无冲突）。
pub const SIG_TRAMP_VA: usize = 0x1FF0_0000;
/// 信号帧大小（304 字节，16 对齐）。
pub const SIG_FRAME_SIZE: usize = 0x130;
/// 帧内偏移（与 TrapFrame 头同构）。
const OFF_SP: usize = 0x100;
const OFF_ELR: usize = 0x108;
const OFF_SPSR: usize = 0x110;
const OFF_SIG: usize = 0x118;
/// 信号帧头大小（寄存器现场 + sp/elr/sstatus，sigreturn 恢复范围）。
const SIG_FRAME_HEADER: usize = 0x118;

/// 蹦床代码：`li a7, #139(SYS_RT_SIGRETURN); ecall; j .`（12 字节，RISC-V）。
const TRAMP_CODE: [u8; 12] = [
    0x93, 0x08, 0xB0, 0x08, // addi a7, zero, 139（li a7, #139）
    0x73, 0x00, 0x00, 0x00, // ecall
    0x6F, 0x00, 0x00, 0x00, // jal zero, 0（j .）
];

/// 蹦床物理页（启动时分配一次，只读映射到所有用户空间）。
static mut TRAMP_PA: usize = 0;

/// 初始化蹦床：分配一页并写入代码。返回 0 成功。
pub fn trampoline_init() -> i32 {
    if unsafe { TRAMP_PA != 0 } {
        return 0;
    }
    let pa = match allocator::alloc_page() {
        Some(p) => p,
        None => return -1,
    };
    unsafe {
        core::ptr::copy_nonoverlapping(TRAMP_CODE.as_ptr(), pa as *mut u8, TRAMP_CODE.len());
        TRAMP_PA = pa;
    }
    // ★ 关键：蹦床页是跨进程共享的只读页，但共享引用计数无法在「所有
    // 进程经 fork 派生、各自独立退出」的生命周期里正确维护（init 退出即归零）。
    // 因此标记为永久页：free_page 永不释放，杜绝「蹦床 VA 指向被复用内存 →
    // 未定义指令 / 跳垃圾地址」。
    allocator::mark_permanent(pa);
    allocator::mark_shared_page(pa);
    0
}

/// 把蹦床映射进指定地址空间（execve 后调用；fork 经页表复制继承）。
pub fn map_trampoline(pt: &mut vm::ProcPageTable) {
    let pa = unsafe { TRAMP_PA };
    if pa == 0 {
        return;
    }
    pt.add_page(SIG_TRAMP_VA, pa, vm::FLAG_USER_CODE);
}

/// 注册/查询信号动作（`rt_sigaction`）。
/// `act` 布局（aarch64 Linux sigaction，32 字节）：
///   handler@0, flags@8, restorer@16, mask@24
pub fn set_action(pid: u32, sig: usize, act: Option<*const u8>) -> i32 {
    if sig >= 64 {
        return -1;
    }
    if let Some(a) = act {
        unsafe {
            // PAN 下 EL1 读用户 sigaction 需临时放行（无 PAN 时为空操作）
            crate::mmu::uaccess_begin();
            let handler = *(a as *const u64);
            let mask = *((a as *const u8).add(24) as *const u64);
            crate::mmu::uaccess_end();
            process::set_sig_handler(pid, sig, handler, mask);
        }
    }
    0
}

/// 读取进程信号动作。
pub fn handler_of(pid: u32, sig: usize) -> u64 {
    process::sig_handler(pid, sig)
}

/// 在用户栈上构建信号帧并把陷阱帧改写成进入 handler。
/// 返回新栈指针（已写入 frame）。frame 写失败返回 None（不投递）。
fn deliver(f: &mut TrapFrame, sig: usize, handler: u64) -> Option<u64> {
    let sp = f.sp_el0;
    let frame = sp.checked_sub(SIG_FRAME_SIZE as u64)?;
    // 帧落在主线程用户栈内（[ASLR 随机化栈底, 栈顶)）时按栈边界快速通过。
    // CLONE_THREAD 工作线程的用户栈由用户态 mmap/堆分配，不在主栈 ASLR 区间
    // 内 —— 此前仅按主栈边界校验，工作线程收信号一律判「栈越界」→ 按默认
    // 动作误杀。放宽为：或全帧落在用户空间 [PAGE_SIZE, USER_TOP) 内（逐页
    // 已映射可写由下方 ensure_user_range_writable 强制，LAZY 落地/COW break
    // 亦由其完成）。内核完整性不受影响：帧只写用户页，USER_TOP 之上的内核
    // 恒等映射区仍然被拒。
    let stack_base = crate::elf::current_stack_base() as u64;
    let stack_top = crate::elf::current_stack_top() as u64;
    let in_main_stack = frame >= stack_base
        && frame.checked_add(SIG_FRAME_SIZE as u64).map_or(false, |e| e <= stack_top);
    let in_user_space = frame >= vm::PAGE_SIZE as u64
        && frame
            .checked_add(SIG_FRAME_SIZE as u64)
            .map_or(false, |e| e <= crate::linux_compat::USER_TOP as u64);
    if !in_main_stack && !in_user_space {
        return None;
    }
    // ★ 内核写用户帧前必须**逐页**校验「已映射且可写」（此前只检查帧落在栈区间内）：
    // 帧所在页若是 COW 共享页（fork 后页表映射只读）或 LAZY 未落地预留页，EL1 直接
    // 写会触发 permission/translation fault → 同步异常处理器死循环 → 整机挂死
    // （任意用户进程可经「注册 handler + 触发信号」路径单方面挂死内核）。
    // ensure_user_range_writable 会先落地 LAZY 页、break COW 页为私有可写、把共享
    // 只读页复制为私有副本；任一页失败则本次信号不投递（按默认动作处理）。
    if !crate::process::ensure_user_range_writable(frame as usize, SIG_FRAME_SIZE) {
        return None;
    }
    // 写入信号帧（逐字段拷贝，等效 copy_to_user；PAN 下需 uaccess 放行）
    let bytes = frame as usize;
    unsafe { crate::mmu::uaccess_begin() };
    for (i, v) in f.x.iter().enumerate() {
        unsafe { *((bytes + i * 8) as *mut u64) = *v };
    }
    unsafe {
        *((bytes + OFF_SP) as *mut u64) = sp;
        *((bytes + OFF_ELR) as *mut u64) = f.elr;
        *((bytes + OFF_SPSR) as *mut u64) = f.spsr;
        *((bytes + OFF_SIG) as *mut u64) = sig as u64;
    }
    unsafe { crate::mmu::uaccess_end() };
    // 改写陷阱帧：进入 handler
    f.sp_el0 = frame;
    f.elr = handler;
    f.x[10] = sig as u64; // a0 = signum
    f.x[11] = 0; // a1 = siginfo
    f.x[12] = 0; // a2 = ucontext
    f.x[1] = SIG_TRAMP_VA as u64; // ra：handler 返回 → 蹦床 → rt_sigreturn
    Some(frame)
}

/// 检查并投递待处理信号。返回 `true` 表示已投递（调用方应跳过本次 syscall）。
pub fn handle_pending(f: &mut TrapFrame) -> bool {
    let pid = process::current_pid();
    let pending = process::signal_pending(pid);
    if pending == 0 {
        return false;
    }
    for sig in 1..64usize {
        if pending & (1u64 << sig) == 0 {
            continue;
        }
        process::signal_clear(pid, sig as u32);
        // 不可捕获/忽略的信号：
        // - SIGKILL(9)：直接终止
        // - SIGSTOP(19)：停止进程（应用后台/挂起语义），直到 SIGCONT。
        //   停止发生在信号投递点（syscall 入口）：当前任务置为 Stopped
        //   并切走，进程不再被调度；SIGCONT 由 sys_kill 调用
        //   sched_cfs::cont_pid 唤醒（Stopped → Ready），随后从 stop_current
        //   返回，本 syscall 继续执行。
        if sig == 9 {
            process::exit(-1);
        }
        if sig == 19 {
            crate::sched_cfs::stop_current();
            // SIGCONT 后从 stop_current 返回：清除 SIGSTOP 位（已在上方
            // signal_clear），继续处理其余待处理信号（若 SIGCONT 也待处理，
            // 其默认动作为继续，无需额外动作）。
            continue;
        }
        // SIGCONT(18) 的恢复动作在发送方（sys_kill → cont_pid）完成；
        // 若进程持有 SIGCONT 的 handler，此处按普通信号投递。
        let handler = process::sig_handler(pid, sig);
        if handler != 0 {
            if deliver(f, sig, handler).is_some() {
                return true;
            }
            // 帧写失败（栈越界）：按默认动作处理
        }
        // 默认动作：SIGINT(2)/SIGTERM(15)/SIGQUIT(3)/SIGSEGV(11) 终止，其余忽略
        match sig {
            2 | 3 | 11 | 15 => process::exit(-1),
            _ => {}
        }
    }
    false
}

/// `rt_sigreturn`：从信号帧恢复现场（帧头 280 字节 + signum）。
/// 恢复后由 syscall 返回路径按 `f.elr` 返回到信号打断点。
pub fn sigreturn(f: &mut TrapFrame) -> i64 {
    let frame = f.sp_el0 as usize;
    // 帧应指向信号帧所在处：主线程用户栈区间，或工作线程栈（用户态 mmap，
    // 不在主栈 ASLR 区间）—— 全帧须在用户空间 [0, USER_TOP) 内且已映射
    // （deliver 侧同理放宽，否则工作线程 handler 返回永远恢复失败）。
    let stack_base = crate::elf::current_stack_base();
    let stack_top = crate::elf::current_stack_top();
    let in_main_stack = frame >= stack_base && frame + SIG_FRAME_HEADER <= stack_top;
    let in_user_space = frame >= vm::PAGE_SIZE
        && frame
            .checked_add(SIG_FRAME_HEADER)
            .map_or(false, |e| e <= crate::linux_compat::USER_TOP as usize);
    if (!in_main_stack && !in_user_space)
        || !vm::user_range_mapped(process::current_root(), frame, SIG_FRAME_HEADER)
    {
        return -1;
    }
    // LAZY 页兜底：帧范围若仍含预留未落地页，先落地（记录级校验已通过但页表
    // 尚未映射时 EL1 读会触发 translation fault 挂死内核）。
    if !crate::process::ensure_user_range_mapped(frame, SIG_FRAME_HEADER) {
        return -1;
    }
    unsafe {
        // PAN 下 EL1 读用户信号帧需 uaccess 放行
        crate::mmu::uaccess_begin();
        // 恢复 x0..x31
        for (i, slot) in f.x.iter_mut().enumerate() {
            *slot = *((frame + i * 8) as *const u64);
        }
        f.sp_el0 = *((frame + OFF_SP) as *const u64);
        f.elr = *((frame + OFF_ELR) as *const u64);
        f.spsr = *((frame + OFF_SPSR) as *const u64);
        crate::mmu::uaccess_end();
    }
    // ★ sstatus 清洗：帧内容完全由用户控制。SPP（bit8，伪造 S 模式返回）与
    // SIE（bit1，吞定时器 tick）不得从用户帧恢复。trap_return 的白名单
    //（只保留 FS + 强制 SUM/MXR/SPIE）是硬件级兜底，此处先行清洗减小
    // 观测面。
    f.spsr &= !((1 << 8) | 2);
    // 注意：上面 `f.sp_el0 = *(frame + OFF_SP)` 恢复的就是被打断时的原始
    // 栈指针（deliver 把 sp 而非帧地址存进 OFF_SP），信号帧本身位于原始 sp
    // 下方、随 sp 恢复已被隐式"弹出"。此处绝不能再 `+= SIG_FRAME_SIZE`，
    // 否则每次信号处理返回用户栈就永久上漂 304 字节，叠加数次后
    // 被打断函数的尾声从错误槽位恢复 s0/ra → ret 到垃圾地址。
    0
}

/// 供 linux_compat 的 syscall 表使用。
pub fn rt_sigaction_syscall(f: &mut TrapFrame) -> i64 {
    let sig = f.x[10] as usize;
    let act = f.x[11];
    let old = f.x[12];
    if sig == 0 || sig >= 64 {
        return -22; // EINVAL
    }
    let pid = process::current_pid();
    if old != 0 {
        // old 是内核写入目标：只查"已映射"不够 —— RO 页/COW 未断页在 EL1
        // 裸写会权限故障 panic（或污染共享 COW 页）。必须走可写校验
        // （含 COW break）。
        if old >= crate::linux_compat::USER_TOP
            || old.checked_add(32).map_or(true, |e| e > crate::linux_compat::USER_TOP)
            || !process::ensure_user_range_writable(old as usize, 32)
            || !vm::user_range_mapped(process::current_root(), old as usize, 32)
        {
            return -14; // EFAULT
        }
        let h = process::sig_handler(pid, sig);
        let m = process::sig_mask(pid, sig);
        unsafe {
            // PAN 下写用户 old sigaction 槽位需 uaccess 放行
            crate::mmu::uaccess_begin();
            *((old as usize) as *mut u64) = h;
            *(((old as usize) + 8) as *mut u64) = 0; // flags
            *(((old as usize) + 16) as *mut u64) = 0; // restorer
            *(((old as usize) + 24) as *mut u64) = m;
            crate::mmu::uaccess_end();
        }
    }
    if act != 0 {
        if !vm::user_range_mapped(process::current_root(), act as usize, 32) {
            return -14;
        }
        set_action(pid, sig, Some(act as *const u8));
    }
    0
}

/// 供 syscall 表使用（编号校验）。
pub fn rt_sigreturn_syscall(f: &mut TrapFrame) -> i64 {
    sigreturn(f)
}
