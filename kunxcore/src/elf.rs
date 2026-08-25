//! ELF64 加载器（多进程）：解析、加载与 `execve`。
//!
//! 解析静态链接的 ELF64（ET_EXEC 优先，兼容 ET_DYN），把每个 `PT_LOAD` 段
//! 按页加载到**进程私有**用户地址空间：分配物理页、按段权限建立映射、拷贝
//! 文件内容、其余部分清零（BSS）。随后为新进程准备用户栈（argc/argv/envp）。
//!
//! 多进程模型说明：
//! - 每个进程持有一个 [`vm::ProcPageTable`]（独立 TTBR0 根表 + 用户映射记录），
//!   内核恒等映射经 `inherit_kernel_mapping` 跨进程共享。
//! - 用户地址空间整体位于 `0x0000_0000 - 0x3FFF_FFFF`（L1[0]），与共享的
//!   内核 RAM 恒等映射（L1[1]，0x4000_0000+）彻底分离，互不污染。
//! - 加载期间通过「物理地址」访问内存（RAM 恒等映射），无需切换 TTBR0。
//! - 调度器在上下文切换时保存/恢复 TTBR0_EL1 并做 `tlbi vmalle1`，
//!   因此多个 EL0 用户进程可以并发运行。
//!
//! 启动（`spawn`）与替换（`execve`）两套路径共享 `load` + `setup_stack`；
//! `spawn` 创建新进程并让 `user_trampoline` 首次下陷到 EL0，`execve` 则
//! 释放旧地址空间、安装新地址空间，并改写系统调用陷阱帧直接 `eret` 到新入口。

use core::arch::asm;

use crate::allocator;
use crate::process;
use crate::sched_cfs::{self, PriorityClass};
use crate::syscall_table::TrapFrame;
use crate::timer;
use crate::vfs;
use crate::vm;

/// 用户进程数量上限（与 `process` 进程表一致）。
const MAX_ELF_PROCS: usize = 16;

/// argv/envp 元素上限。
const MAX_ARGC: usize = 16;
/// execve 读取 ELF 文件的缓冲区上限（launcher 等大应用 ELF 可达 25KB+，留足余量）。
const EXEC_BUF_CAP: usize = 65536;

// ==================== ELF 常量 ====================

/// ELF 魔数（`\x7fELF` 小端）。
const ELF_MAGIC: u32 = 0x464C_457F;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

// ==================== 字节解析 ====================

fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off], b[off + 1], b[off + 2], b[off + 3],
        b[off + 4], b[off + 5], b[off + 6], b[off + 7],
    ])
}

// ==================== 加载结果 ====================

/// 一次 ELF 加载的结果：入口、进程私有页表与程序断点（brk）。
pub struct ElfImage {
    pub entry: usize,
    pub pt: vm::ProcPageTable,
    /// 初始程序断点：所有 PT_LOAD 段 (vaddr + memsz) 的最大值按页对齐。
    pub brk: usize,
}

// ==================== 用户进程注册表 ====================

/// 已启动用户进程的运行时信息（供 trampoline / brk 读取）。
struct ElfProc {
    used: bool,
    entry: usize,
    stack_top: usize,
    brk: usize,
}

impl ElfProc {
    const fn empty() -> Self {
        ElfProc { used: false, entry: 0, stack_top: 0, brk: 0 }
    }
}

static mut ELF_PROCS: [ElfProc; MAX_ELF_PROCS] = [const { ElfProc::empty() }; MAX_ELF_PROCS];

// ==================== 段权限 ====================

fn seg_flags(pf: u32) -> vm::MapFlags {
    let mut f = vm::AF | vm::SH_INNER | vm::ATTR_NORMAL;
    if pf & PF_W != 0 {
        f |= vm::AP_USER_RW;
    } else {
        f |= vm::AP_USER_RO;
    }
    if pf & PF_X != 0 {
        // 可执行：EL0 可执行（UXN 清 0）、EL1 不可执行（PXN 置 1）
        f |= vm::PXN;
    } else {
        f |= vm::UXN | vm::PXN;
    }
    f
}

// ==================== 加载 ====================

/// 解析并加载 ELF64 到新的进程私有地址空间，返回入口、页表与断点。
#[inline(never)]
pub fn load(data: &[u8]) -> Result<ElfImage, i32> {
    let t0 = timer::now_us();
    if data.len() < 64 {
        return Err(-1);
    }
    if rd_u32(data, 0) != ELF_MAGIC {
        return Err(-1);
    }
    // ident[4]=ELFCLASS64, ident[5]=ELFDATA2LSB
    if data[4] != 2 || data[5] != 1 {
        return Err(-1);
    }
    let e_type = rd_u16(data, 16);
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(-1);
    }
    let e_machine = rd_u16(data, 18);
    if e_machine != EM_AARCH64 {
        return Err(-1);
    }
    let entry = rd_u64(data, 24) as usize;
    // 入口必须在用户地址空间内（< 0x4000_0000），否则 eret 到未映射/内核区
    // 会触发取指异常死循环或经共享页表执行内核代码。
    if entry == 0 || entry >= vm::USER_VA_TOP {
        return Err(-1);
    }
    let phoff = rd_u64(data, 32) as usize;
    let phentsize = rd_u16(data, 54) as usize;
    let phnum = rd_u16(data, 56) as usize;
    if phentsize < 56 {
        return Err(-1);
    }

    let mut pt = vm::ProcPageTable::new();
    let mut brk = 0usize;
    let mut mapped = 0usize;

    // 所有失败路径统一清理已分配的页与页表，防止恶意/损坏 ELF 反复
    // 加载耗尽物理内存（内核 DoS）。
    macro_rules! fail {
        () => {{
            pt.free_user_pages();
            vm::free_process_tables(pt.ttbr0);
            return Err(-1);
        }};
    }

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + 56 > data.len() {
            fail!();
        }
        let p_type = rd_u32(data, off);
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = rd_u32(data, off + 4);
        let p_offset = rd_u64(data, off + 8) as usize;
        let p_vaddr = rd_u64(data, off + 16) as usize;
        let p_filesz = rd_u64(data, off + 32) as usize;
        let p_memsz = rd_u64(data, off + 40) as usize;
        if p_filesz > p_memsz {
            fail!();
        }
        if p_offset.checked_add(p_filesz).map_or(true, |e| e > data.len()) {
            fail!();
        }
        // 段必须完整落在用户地址空间内（< 0x4000_0000），否则会改写共享内核页表
        if !vm::user_va_range_ok(p_vaddr, p_memsz) {
            fail!();
        }

        // 程序断点 = 数据段（含 BSS）末尾，取所有段的最大值
        let seg_end = match p_vaddr.checked_add(p_memsz) {
            Some(e) => e,
            None => fail!(),
        };
        brk = brk.max(seg_end);

        let flags = seg_flags(p_flags);
        let start = p_vaddr & !(vm::PAGE_SIZE - 1);
        let end = p_vaddr + p_memsz;
        let file_end = p_vaddr + p_filesz;

        // 段整块加载：优先连续分配整段物理页 + 一次整块 memcpy（省去逐页
        // 分配/清零/拷贝循环）；连续分配失败时回退逐页加载。段内文件范围
        // [p_offset, p_offset+p_filesz) 对应 [p_vaddr, p_vaddr+p_filesz)。
        // 向上取整：end 可能未页对齐（段尾跨页），逐页路径的 `while va < end`
        // 天然处理最后不完整页，块路径必须显式取整否则漏映射尾页（用户翻译错误）
        let seg_pages = (end - start).div_ceil(vm::PAGE_SIZE);
        let seg_pa = allocator::alloc_pages_contig(seg_pages);
        match seg_pa {
            Some(base) => {
                // 整段清零（覆盖 BSS 与页对齐间隙），再一次性拷贝文件内容
                unsafe {
                    core::ptr::write_bytes(base as *mut u8, 0, seg_pages * vm::PAGE_SIZE);
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(p_offset),
                        (base + (p_vaddr - start)) as *mut u8,
                        p_filesz,
                    );
                }
                let mut va = start;
                for i in 0..seg_pages {
                    let pa = base + i * vm::PAGE_SIZE;
                    if vm::is_user_ro(flags) {
                        allocator::mark_shared_page(pa);
                    }
                    if !pt.add_page(va, pa, flags) {
                        fail!();
                    }
                    mapped += 1;
                    va += vm::PAGE_SIZE;
                }
            }
            None => {
                // 回退：逐页分配 + 逐页拷贝（保持原语义）
                let mut va = start;
                while va < end {
                    let pa = match allocator::alloc_page() {
                        Some(p) => p,
                        None => fail!(),
                    };
                    unsafe { core::ptr::write_bytes(pa as *mut u8, 0, vm::PAGE_SIZE) };

                    let copy_start = p_vaddr.max(va);
                    let copy_end = file_end.min(va + vm::PAGE_SIZE);
                    if copy_start < copy_end {
                        let file_off = p_offset + (copy_start - p_vaddr);
                        let n = copy_end - copy_start;
                        let page_off = copy_start - va;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                data.as_ptr().add(file_off),
                                (pa + page_off) as *mut u8,
                                n,
                            );
                        }
                    }

                    if vm::is_user_ro(flags) {
                        allocator::mark_shared_page(pa);
                    }

                    if !pt.add_page(va, pa, flags) {
                        fail!();
                    }
                    mapped += 1;
                    va += vm::PAGE_SIZE;
                }
            }
        }
    }

    if mapped == 0 {
        fail!();
    }
    let t1 = timer::now_us();
    crate::uart_send_str("[perf] elf load pages=");
    crate::print_dec(mapped as u64);
    crate::uart_send_str(" took=");
    crate::print_dec((t1 - t0) as u64);
    crate::uart_send_str("us\n");
    Ok(ElfImage {
        entry,
        pt,
        brk: (brk + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1),
    })
}

// ==================== 用户栈 ====================

/// 用户栈页数。
const STACK_PAGES: usize = vm::USER_STACK_SIZE / vm::PAGE_SIZE;

/// 把数据写入用户栈对应的物理内存（栈 VA → 物理页翻译后写入）。
fn stack_write(pages: &[usize; STACK_PAGES], va: usize, data: &[u8]) {
    let idx = (va - vm::USER_STACK_BASE) / vm::PAGE_SIZE;
    let off = (va - vm::USER_STACK_BASE) % vm::PAGE_SIZE;
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), (pages[idx] + off) as *mut u8, data.len());
    }
}

fn stack_write_u64(pages: &[usize; STACK_PAGES], va: usize, v: u64) {
    stack_write(pages, va, &v.to_le_bytes());
}

/// 分配并映射用户栈，写入 argc/argv/envp 初始布局，返回初始 `sp`。
///
/// 映射记录表满（`add_page` 失败）时回滚已分配栈页并返回 `None`，
/// 由调用方清理地址空间（避免泄漏 + fork 不一致）。
#[inline(never)]
fn setup_stack(pt: &mut vm::ProcPageTable, argv: &[&str], envp: &[&str]) -> Option<usize> {
    let mut pages: [usize; STACK_PAGES] = [0; STACK_PAGES];
    for i in 0..STACK_PAGES {
        let va = vm::USER_STACK_BASE + i * vm::PAGE_SIZE;
        let pa = allocator::alloc_page().expect("elf: out of memory for user stack");
        if !pt.add_page(va, pa, vm::FLAG_USER_DATA) {
            // 回滚已分配栈页
            for j in 0..i {
                allocator::free_page(pages[j]);
            }
            allocator::free_page(pa);
            return None;
        }
        pages[i] = pa;
    }

    let argc = argv.len().min(MAX_ARGC);
    let envc = envp.len().min(MAX_ARGC);

    // 1. 字符串区（自顶向下）：先 argv，后 envp
    let mut sp = vm::USER_STACK_TOP;
    let mut str_addrs = [0usize; MAX_ARGC];
    for i in 0..argc {
        let s = argv[i].as_bytes();
        sp -= s.len() + 1;
        stack_write(&pages, sp, s);
        stack_write(&pages, sp + s.len(), &[0]);
        str_addrs[i] = sp;
    }
    let mut env_addrs = [0usize; MAX_ARGC];
    for i in 0..envc {
        let s = envp[i].as_bytes();
        sp -= s.len() + 1;
        stack_write(&pages, sp, s);
        stack_write(&pages, sp + s.len(), &[0]);
        env_addrs[i] = sp;
    }

    // 2. 16 字节对齐
    sp &= !0xFusize;

    // 3. envp 终止符（NULL）
    sp -= 8;
    stack_write_u64(&pages, sp, 0);

    // 4. envp 指针（倒序）
    for i in (0..envc).rev() {
        sp -= 8;
        stack_write_u64(&pages, sp, env_addrs[i] as u64);
    }

    // 5. argv 终止符（NULL）
    sp -= 8;
    stack_write_u64(&pages, sp, 0);

    // 6. argv 指针（倒序）
    for i in (0..argc).rev() {
        sp -= 8;
        stack_write_u64(&pages, sp, str_addrs[i] as u64);
    }

    // 7. argc
    sp -= 8;
    stack_write_u64(&pages, sp, argc as u64);

    Some(sp)
}

// ==================== 标准输入输出 ====================

fn setup_stdio(pid: u32) {
    let stdin = vfs::open("/dev/tty", vfs::O_RDONLY);
    let stdout = vfs::open("/dev/tty", vfs::O_WRONLY);
    let stderr = vfs::open("/dev/tty", vfs::O_WRONLY);

    if stdin >= 0 {
        if let Some(e) = process::get_fd(stdin) {
            process::assign_fd(pid, 0, e);
        }
    }
    if stdout >= 0 {
        if let Some(e) = process::get_fd(stdout) {
            process::assign_fd(pid, 1, e);
        }
    }
    if stderr >= 0 {
        if let Some(e) = process::get_fd(stderr) {
            process::assign_fd(pid, 2, e);
        }
    }

    // 关闭父进程的临时 fd（打开文件表项仍被子进程 fd 0/1/2 引用）
    if stdin >= 0 {
        process::close_fd(stdin);
    }
    if stdout >= 0 {
        process::close_fd(stdout);
    }
    if stderr >= 0 {
        process::close_fd(stderr);
    }
}

// ==================== 下陷到 EL0 ====================

/// EL0 模式、DAIF 全开（用户态可被定时器抢占）。
const SPSR_EL0T: u64 = 0;

/// 用户线程入口：首次调度时运行，设置 EL0 栈/入口并 `eret` 到 EL0。
///
/// `pid` 由 `thread_trampoline` 经 x0 传入。TTBR0_EL1 已由调度器在
/// 上下文切换时设置为本进程页表根，此处无需再动页表。
fn user_trampoline(pid: usize) -> ! {
    let (entry, stack_top) = unsafe {
        if pid < MAX_ELF_PROCS {
            let p = &ELF_PROCS[pid];
            (p.entry, p.stack_top)
        } else {
            (0, 0)
        }
    };
    unsafe {
        asm!(
            "msr sp_el0, {sp}",
            "msr elr_el1, {pc}",
            "msr spsr_el1, {spsr}",
            "eret",
            sp = in(reg) stack_top,
            pc = in(reg) entry,
            spsr = in(reg) SPSR_EL0T,
            options(noreturn),
        );
    }
}

// ==================== 公共接口 ====================

/// 加载 ELF 并启动为用户进程，返回 pid；失败返回 `u32::MAX`。
#[inline(never)]
pub fn spawn(name: &'static str, data: &[u8], argv: &[&str]) -> u32 {
    let mut img = match load(data) {
        Ok(i) => i,
        Err(_) => return u32::MAX,
    };
    let stack_top = match setup_stack(&mut img.pt, argv, &[]) {
        Some(sp) => sp,
        None => {
            // 栈映射失败：回滚整个新地址空间（load 的页 + 页表）
            img.pt.free_user_pages();
            vm::free_process_tables(img.pt.ttbr0);
            return u32::MAX;
        }
    };
    let root = img.pt.ttbr0;
    let entry = img.entry;
    let brk = img.brk;

    let pid = process::create_process(img.pt, name);
    if (pid as usize) >= MAX_ELF_PROCS {
        return u32::MAX;
    }

    setup_stdio(pid);

    unsafe {
        ELF_PROCS[pid as usize] = ElfProc {
            used: true,
            entry,
            stack_top,
            brk,
        };
    }

    if sched_cfs::spawn(pid, name, user_trampoline, pid as usize, PriorityClass::Normal, 0, root) == u32::MAX {
        // 任务槽位耗尽：回收进程（地址空间 + fd + 运行时信息）后返回失败
        unsafe { ELF_PROCS[pid as usize] = ElfProc::empty(); }
        process::reap_forced(pid);
        return u32::MAX;
    }
    pid
}

/// `execve`：替换当前进程映像，返回 0；失败返回负 errno。
///
/// 入参 `path`/`argv`/`envp` 均为**内核缓冲**中的字符串（调用方须先从用户
/// 空间拷贝，因为旧地址空间随后会被释放）。成功后改写陷阱帧，使当前系统调用
/// 返回时直接 `eret` 到新程序入口。
#[inline(never)]
pub fn execve(path: &str, argv: &[&str], envp: &[&str], f: &mut TrapFrame) -> i32 {
    let t0 = timer::now_us();
    // 1. 打开并读取 ELF 文件。
    // 读入缓冲按需堆分配而非 static：syscall 期间可被定时器抢占，若另一任务
    // 并发 execve，共享 static 缓冲会被覆写导致本任务加载被污染的 ELF 数据。
    let buf = crate::heap::kmalloc(EXEC_BUF_CAP);
    if buf.is_null() {
        return -1;
    }
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        unsafe { crate::heap::kfree(buf) };
        return -1;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 4096];
        let n = vfs::read(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        if total + n as usize > EXEC_BUF_CAP {
            vfs::close(fd);
            unsafe { crate::heap::kfree(buf) };
            return -1;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf.add(total), n as usize);
        }
        total += n as usize;
    }
    vfs::close(fd);
    if total == 0 {
        unsafe { crate::heap::kfree(buf) };
        return -1;
    }

    // 2. 加载新映像（load 不保留 data 引用，返回后即可释放缓冲）
    let data = unsafe { core::slice::from_raw_parts(buf as *const u8, total) };
    let mut img = match load(data) {
        Ok(i) => i,
        Err(e) => {
            unsafe { crate::heap::kfree(buf) };
            return e;
        }
    };
    unsafe { crate::heap::kfree(buf) };

    // 3. 准备新用户栈（失败则回滚新地址空间，旧进程保持不变）
    let stack_top = match setup_stack(&mut img.pt, argv, envp) {
        Some(sp) => sp,
        None => {
            img.pt.free_user_pages();
            vm::free_process_tables(img.pt.ttbr0);
            return -1;
        }
    };
    let entry = img.entry;
    let brk = img.brk;

    // 3.5 映射信号蹦床页（rt_sigreturn 存根，只读共享；fork 经页表继承）
    crate::signal::map_trampoline(&mut img.pt);

    // 4. 替换当前进程地址空间（先切到新根表，再释放旧地址空间）
    let pid = process::current_pid();
    process::replace_address_space(pid, img.pt);
    process::rename(pid, basename(path));
    // 5. GUI 前台进程调度优待：launcher/oobe 及其拉起的应用提升权重，
    //    降低动画期间被后台进程（networkd 等）抢占的概率（CFS 语义不变）
    let name = basename(path);
    if is_gui_app(name) {
        sched_cfs::set_current_nice(-3);
    }

    // 启动计时（诊断）
    let t1 = timer::now_us();
    crate::uart_send_str("[perf] execve ");
    crate::uart_send_str(path);
    crate::uart_send_str(" took=");
    crate::print_dec((t1 - t0) as u64);
    crate::uart_send_str("us\n");

    unsafe {
        if (pid as usize) < MAX_ELF_PROCS {
            ELF_PROCS[pid as usize] = ElfProc {
                used: true,
                entry,
                stack_top,
                brk,
            };
        }
    }

    // 5. 改写陷阱帧：返回时 eret 到新入口
    f.sp_el0 = stack_top as u64;
    f.elr = entry as u64;
    f.spsr = SPSR_EL0T;
    f.x[0] = 0;
    // 清零陷阱帧的 FP/SIMD 区（v0-v31 + fpsr/fpcr，@0x110..0x320）：execve 后
    // 新进程无浮点状态，避免内核栈残留的浮点常量（0.01 等）污染用户 v 寄存器
    // （残留值会被 syscall 返回路径恢复进用户态 → 浮点计算错乱 → 跳垃圾地址）。
    let fb = (f as *mut TrapFrame) as *mut u8;
    unsafe { core::ptr::write_bytes(fb.add(0x110), 0, 0x320 - 0x110) };
    // [诊断] 记录 execve 设置的初始栈顶与入口（核对 PC=0 故障中 sp 是否错位）
    crate::uart_send_str("[execve-diag] entry=");
    crate::print_hex(entry as u64);
    crate::uart_send_str(" stack_top=");
    crate::print_hex(stack_top as u64);
    crate::uart_send_str("\n");
    0
}

/// GUI 前台应用名单：这些进程及其 fork 子树获得调度优待（nice -3）。
fn is_gui_app(name: &str) -> bool {
    matches!(
        name,
        "launcher" | "oobe" | "calculator" | "settings" | "terminal" | "store" | "pkg"
    )
}

/// 取路径最后一级分量作为进程名（`/bin/foo` -> `foo`）。
fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) if i + 1 < path.len() => &path[i + 1..],
        _ => path,
    }
}

/// 从文件系统路径读取 ELF 并启动为用户进程，返回 pid；失败返回 -1。
#[inline(never)]
pub fn spawn_from_path(path: &'static str, argv: &[&str]) -> i32 {
    let t0 = timer::now_us();
    // 用户态 ELF 缓冲（堆分配，避免 64KB 局部数组压栈；launcher 等大应用可达 25KB+）。
    let buf = crate::heap::kmalloc(65536);
    if buf.is_null() {
        return -1;
    }
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        unsafe { crate::heap::kfree(buf) };
        return -1;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 4096];
        let n = vfs::read(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        if total + n as usize > 65536 {
            break;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf.add(total), n as usize);
        }
        total += n as usize;
    }
    vfs::close(fd);
    if total == 0 {
        unsafe { crate::heap::kfree(buf) };
        return -1;
    }
    // 进程名取路径最后一级（/bin/init -> "init"），供 `ps` 显示
    let name = match path.rfind('/') {
        Some(i) if i + 1 < path.len() => &path[i + 1..],
        _ => path,
    };
    let pid = spawn(name, unsafe { core::slice::from_raw_parts(buf as *const u8, total) }, argv);
    unsafe { crate::heap::kfree(buf) };
    if is_gui_app(name) {
        sched_cfs::set_current_nice(-3);
    }
    if pid != u32::MAX {
        crate::uart_send_str("[perf] spawn ");
        crate::uart_send_str(path);
        crate::uart_send_str(" took=");
        crate::print_dec((timer::now_us() - t0) as u64);
        crate::uart_send_str("us\n");
    }
    pid as i32
}

/// 清除某个用户进程的运行时信息（地址空间已由 `process::wait` 回收）。
pub fn release(pid: u32) {
    if (pid as usize) >= MAX_ELF_PROCS {
        return;
    }
    unsafe {
        ELF_PROCS[pid as usize] = ElfProc::empty();
    }
}

/// 返回当前 EL0 用户进程的初始程序断点（brk）。
///
/// 供 bionic 运行时 `brk(0)` 初始化堆使用；非用户进程返回默认用户堆基址。
pub fn current_brk() -> usize {
    let pid = process::current_pid();
    unsafe {
        if (pid as usize) < MAX_ELF_PROCS && ELF_PROCS[pid as usize].used {
            let b = ELF_PROCS[pid as usize].brk;
            if b != 0 {
                return b;
            }
        }
    }
    vm::USER_HEAP_BASE
}
