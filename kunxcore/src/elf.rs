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
use crate::bionic_shim;
use crate::process;
use crate::sched_cfs::{self, PriorityClass};
use crate::syscall_table::TrapFrame;
use crate::timer;
use crate::vfs;
use crate::vm;

/// 用户进程数量上限（与 `process` 进程表一致）。
///
/// 随 `process::MAX_PROC` 扩到 24：进程表扩大后若 ELF 运行时表仍是 16，
/// pid ≥ 16 的 fork 子进程无法继承 ASLR 栈基址/brk，且 `elf::spawn_from_path`
/// 会直接拒绝 pid ≥ 16 的新进程（见 `spawn_from_path`/`execve` 边界检查）。
const MAX_ELF_PROCS: usize = 24;

/// argv/envp 元素上限。
const MAX_ARGC: usize = 16;
/// execve 读入缓冲的安全上限：缓冲按文件实际大小分配（64KB→256KB 固定上限
/// 已两次被应用体积增长撞爆，表现为 execve 静默失败、桌面起不来），固定值
/// 只用于拦截损坏文件系统的异常大小，不再作为正常路径的容量约束。
const EXEC_MAX_CAP: usize = 4 * 1024 * 1024;

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
///
/// `pt` 用 Box 承载：`ProcPageTable` 含 `user_pages[4096]`（约 96KB），
/// 内联进 `ElfImage` 会让 `load` 的返回值与 `execve`/`spawn` 的栈帧
/// 超过内核任务栈（256KB）的安全余量。
pub struct ElfImage {
    pub entry: usize,
    pub pt: alloc::boxed::Box<vm::ProcPageTable>,
    /// 初始程序断点：所有 PT_LOAD 段 (vaddr + memsz) 的最大值按页对齐。
    pub brk: usize,
    /// 程序头表在用户空间的虚拟地址（供 auxv AT_PHDR；musl 静态启动必需）。
    pub phdr_va: usize,
    pub phnum: usize,
    pub phentsize: usize,
}

// ==================== 用户进程注册表 ====================

/// 已启动用户进程的运行时信息（供 trampoline / brk / 信号帧边界读取）。
struct ElfProc {
    used: bool,
    entry: usize,
    /// 初始用户栈指针（argv/envp 压栈后，`sp_el0` 初值）。
    stack_top: usize,
    /// 用户栈区基址（ASLR 随机化后的栈底；区顶 = `stack_base + USER_STACK_SIZE`）。
    stack_base: usize,
    /// 程序断点（ASLR 随机化后的堆基址）。
    brk: usize,
}

impl ElfProc {
    const fn empty() -> Self {
        ElfProc { used: false, entry: 0, stack_top: 0, stack_base: 0, brk: 0 }
    }
}

static mut ELF_PROCS: [ElfProc; MAX_ELF_PROCS] = [const { ElfProc::empty() }; MAX_ELF_PROCS];

/// 当前进程 ASLR 随机化后的用户栈区基址（供信号帧边界校验等）。
/// 非 ELF 进程（内核线程/fork 后未 exec 的进程）回退固定基址。
pub fn current_stack_base() -> usize {
    let pid = process::current_pid();
    unsafe {
        if (pid as usize) < MAX_ELF_PROCS && ELF_PROCS[pid as usize].used {
            let b = ELF_PROCS[pid as usize].stack_base;
            if b != 0 {
                return b;
            }
        }
    }
    vm::USER_STACK_BASE
}

/// 当前进程用户栈区顶（= 栈底 + 1MB；ASLR 后可能低于固定 `USER_STACK_TOP`）。
pub fn current_stack_top() -> usize {
    current_stack_base() + vm::USER_STACK_SIZE
}

/// fork 时把父进程的 ELF 运行时信息（栈基址/初始 sp/brk）复制给子进程：
/// 子进程继承父进程地址空间（COW），其用户栈布局与堆基址与父进程一致，
/// 信号帧边界/brk 初始化才能用正确的随机化值。父进程非 ELF 进程时跳过。
pub fn fork_runtime_info(parent_pid: u32, child_pid: u32) {
    unsafe {
        if (parent_pid as usize) < MAX_ELF_PROCS
            && (child_pid as usize) < MAX_ELF_PROCS
            && ELF_PROCS[parent_pid as usize].used
        {
            // 逐字段拷贝（ElfProc 非 Copy，静态数组不能整体 move）
            let p_entry = ELF_PROCS[parent_pid as usize].entry;
            let p_stack_top = ELF_PROCS[parent_pid as usize].stack_top;
            let p_stack_base = ELF_PROCS[parent_pid as usize].stack_base;
            let p_brk = ELF_PROCS[parent_pid as usize].brk;
            ELF_PROCS[child_pid as usize] = ElfProc {
                used: true,
                entry: p_entry,
                stack_top: p_stack_top,
                stack_base: p_stack_base,
                brk: p_brk,
            };
        }
    }
}

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
    // 长度检查必须先于任何 data 索引：kinfo! 块在 verbose 开启时必然求值，
    // 若先 rd_u32(data,0)，execve 1-3 字节的文件会切片越界 panic 杀死内核。
    // 下界 72 而非 64：诊断读取 read_volatile(data.as_ptr().add(64)) 取 8 字节
    // 需要 len ≥ 72，否则读入 slab 对象未初始化尾部（切片越界语义）。
    if data.len() < 72 {
        return Err(-1);
    }
    crate::kinfo! {
        crate::uart_send_str("[diag] 加载 len=");
        crate::print_dec(data.len() as u64);
        crate::uart_send_str(" magic=");
        crate::print_hex(rd_u32(data, 0) as u64);
        crate::uart_send_str("\n");
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

    let v64_entry = unsafe { core::ptr::read_volatile(data.as_ptr().add(64)) };
    crate::kinfo! {
        crate::uart_send_str("[diag] data ptr=");
        crate::print_hex(data.as_ptr() as u64);
        crate::uart_send_str(" v64-entry=");
        crate::print_hex(v64_entry as u64);
        crate::uart_send_str("\n");
    }
    // 根页表 OOM：物理内存耗尽时干净失败（无已分配资源需要回滚）。
    let mut pt = match vm::ProcPageTable::try_new_boxed() {
        Some(pt) => pt,
        None => return Err(-1),
    };
    let v64_after = unsafe { core::ptr::read_volatile(data.as_ptr().add(64)) };
    let boxa = (&*pt as *const vm::ProcPageTable) as usize;
    crate::kinfo! {
        crate::uart_send_str("[diag] pt boxa=");
        crate::print_hex(boxa as u64);
        crate::uart_send_str(" datap=");
        crate::print_hex(data.as_ptr() as u64);
        crate::uart_send_str(" v64-after=");
        crate::print_hex(v64_after as u64);
        crate::uart_send_str("\n");
    }
    let mut brk = 0usize;
    let mut mapped = 0usize;
    // 程序头表落在哪个 PT_LOAD 段内（auxv AT_PHDR 用；找不到则置 0，
    // musl 单线程静态程序在 phnum=0 时仍能完成最小 TLS 初始化）
    let mut phdr_va = 0usize;

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
        // ★ 程序头偏移用 checked 运算：构造的 ELF 可使 e_phoff / e_phentsize 接近
        // usize::MAX，未校验的 `phoff + i*phentsize` 与 `off + 56` 会整数回绕通过
        // 长度检查，随后 `rd_u32` 越界切片 → panic → 内核崩溃（用户态可触发的 DoS）。
        let Some(off) = phoff.checked_add(i * phentsize) else { fail!() };
        if off.checked_add(56).map_or(true, |e| e > data.len()) {
            fail!();
        }
        let p_type = rd_u32(data, off);
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = rd_u32(data, off + 4);
        // ★ W^X：拒绝「可写且可执行」段（PF_W | PF_X）。此前 `seg_flags` 接受
        // W+X 段并映射为可执行可写文本页，与内核声明的 W^X 策略相矛盾（ELF 内
        // 注入代码的路径）。执行与可写互斥：只读可执行或可写不可执行。
        if (p_flags & (PF_W | PF_X)) == (PF_W | PF_X) {
            fail!();
        }
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
        // 程序头表位于本段文件映像内 → 记录其虚拟地址
        if phdr_va == 0
            && phoff >= p_offset
            && phoff.checked_add(phnum * phentsize).map_or(false, |e| e <= p_offset + p_filesz)
        {
            phdr_va = p_vaddr + (phoff - p_offset);
        }

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
                        // add_page 失败：记录表只覆盖 base..base+i，整段
                        // 连续块的剩余页（含本页）必须就地归还，否则 fail!
                        // 的逐记录释放回收不到它们 → 物理页泄漏。
                        for k in i..seg_pages {
                            allocator::free_page(base + k * vm::PAGE_SIZE);
                        }
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
    crate::kinfo! {
        crate::uart_send_str("[perf] ELF 加载 pages=");
        crate::print_dec(mapped as u64);
        crate::uart_send_str(" 耗时=");
        crate::print_dec((t1 - t0) as u64);
        crate::uart_send_str("us\n");
    }
    // ASLR：堆基址在 ELF 断点之上随机上移 4MB（页对齐）。打破固定 brk 布局，
    // 与栈随机化共同构成地址空间布局随机化（mmap 区 0x30000000 与 fb 0x20000000
    // 在堆区上方，余量充足，无碰撞风险）。
    //
    // 堆下限钳制到 `vm::USER_HEAP_BASE`（0x0C000000，设备窗口之上）：应用堆
    // 若从 BSS 尾（~0x600000）起涨，大堆（fontdue 解析 1.7MB 字体膨胀 ~62MB/
    // 个）会跨过 0x08000000 的 GIC/UART/virtio 设备区，用户页覆盖设备寄存器
    // → 内核读 GIC 全 0 → 中断风暴冻结（见 vm::USER_HEAP_BASE 注释）。
    let heap_base = ((brk + vm::PAGE_SIZE - 1) & !(vm::PAGE_SIZE - 1)).max(vm::USER_HEAP_BASE);
    // 堆基址（含 4MB ASLR）必须留在匿名 mmap 区（0x30000000）之下、用户区
    // 上界之内：段尾靠近 0x3FF00000 的 ELF 会派生出 ≥ 0x40000000 的 brk
    // （brk_set 拒绝一切增长 → 堆永久损坏）或与 mmap 区重叠。
    const BRK_LIMIT: usize = bionic_shim::MMAP_BASE - 8 * 1024 * 1024;
    let brk_init = (heap_base + process::aslr_pages(1024)).min(BRK_LIMIT);
    Ok(ElfImage {
        entry,
        pt,
        brk: brk_init,
        phdr_va,
        phnum,
        phentsize,
    })
}

// ==================== 用户栈 ====================

/// 用户栈页数。
const STACK_PAGES: usize = vm::USER_STACK_SIZE / vm::PAGE_SIZE;

/// ASLR：每次 exec 随机化用户栈区顶（16MB 熵窗口，页对齐）。
///
/// 栈区顶 ∈ [USER_STACK_TOP - 16MB, USER_STACK_TOP)，栈区（1MB）随之整体
/// 下移；栈区下方保留 15MB 隔离带（mmap 游标上限 `MMAP_CAP`），栈底之下
/// 不映射任何页（栈溢出 → 干净的 EL0 fault/SIGSEGV，而非写穿 mmap 区）。
/// 与 Linux `mmap_rnd_bits` 思路一致：布局随机化打破固定地址攻击面。
fn aslr_stack_top() -> usize {
    vm::USER_STACK_TOP - process::aslr_pages((vm::STACK_ASLR_PAGES) as u32)
}

/// 把数据写入用户栈对应的物理内存（栈 VA → 物理页翻译后写入）。
///
/// ★ 跨页安全：写入可能跨越两个物理页（栈物理页不保证相邻），必须按页
/// 切分 —— 此前一次性 copy_nonoverlapping 会越过第一页的物理帧边界写进
/// **紧随其后的任意物理内存**（其它进程页/内核数据）。argv/envp 字符串
/// 总长超过一页且恰跨边界时触发（当前调用方串短，属防御性修复）。
fn stack_write(pages: &[usize; STACK_PAGES], base: usize, va: usize, data: &[u8]) {
    let mut cur_va = va;
    let mut done = 0usize;
    while done < data.len() {
        let idx = (cur_va - base) / vm::PAGE_SIZE;
        let off = (cur_va - base) % vm::PAGE_SIZE;
        let chunk = (vm::PAGE_SIZE - off).min(data.len() - done);
        if idx >= STACK_PAGES {
            return;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                data[done..].as_ptr(),
                (pages[idx] + off) as *mut u8,
                chunk,
            );
        }
        done += chunk;
        cur_va += chunk;
    }
}

fn stack_write_u64(pages: &[usize; STACK_PAGES], base: usize, va: usize, v: u64) {
    stack_write(pages, base, va, &v.to_le_bytes());
}

/// 分配并映射用户栈（ASLR 随机化的 `stack_top` 起 1MB），写入
/// argc/argv/envp 初始布局，返回初始 `sp` 与栈区基址。
///
/// 映射记录表满（`add_page` 失败）时回滚已分配栈页并返回 `None`，
/// 由调用方清理地址空间（避免泄漏 + fork 不一致）。
/// auxv 构造所需的启动信息（musl/glibc 静态程序必需）。
pub struct AuxInit<'a> {
    pub phdr_va: usize,
    pub phnum: usize,
    pub phentsize: usize,
    pub entry: usize,
    pub exec_path: &'a str,
    pub uid: u64,
}

#[inline(never)]
fn setup_stack(
    pt: &mut vm::ProcPageTable,
    argv: &[&str],
    envp: &[&str],
    stack_top: usize,
    aux: Option<&AuxInit>,
) -> Option<(usize, usize)> {
    let stack_base = stack_top - vm::USER_STACK_SIZE;
    let mut pages: [usize; STACK_PAGES] = [0; STACK_PAGES];
    for i in 0..STACK_PAGES {
        let va = stack_base + i * vm::PAGE_SIZE;
        // 用户栈页 OOM：回滚已分配页并返回 None（此前 panic，exec 路径
        // 可被用户触发；调用方走既有 fail! 清理路径）。
        let Some(pa) = allocator::alloc_page() else {
            for j in 0..i {
                allocator::free_page(pages[j]);
            }
            crate::uart_send_str("[elf] 用户栈内存不足\n");
            return None;
        };
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
    let mut sp = stack_top;
    let mut str_addrs = [0usize; MAX_ARGC];
    for i in 0..argc {
        let s = argv[i].as_bytes();
        sp -= s.len() + 1;
        stack_write(&pages, stack_base, sp, s);
        stack_write(&pages, stack_base, sp + s.len(), &[0]);
        str_addrs[i] = sp;
    }
    let mut env_addrs = [0usize; MAX_ARGC];
    for i in 0..envc {
        let s = envp[i].as_bytes();
        sp -= s.len() + 1;
        stack_write(&pages, stack_base, sp, s);
        stack_write(&pages, stack_base, sp + s.len(), &[0]);
        env_addrs[i] = sp;
    }

    // 1.5 auxv 附属数据：AT_RANDOM 16 字节随机数 + AT_EXECFN 路径串
    // （位于指针区上方；musl 经 AT_EXECFN/AT_RANDOM 指针读取）
    let mut random_addr = 0usize;
    let mut execfn_addr = 0usize;
    if aux.is_some() {
        let mut rb = [0u8; 16];
        if crate::virtio_rng::get_random(&mut rb) <= 0 {
            // 熵源不可用：时间回退（仅影响 stack canary 强度）
            let t = crate::timer::now_us().to_le_bytes();
            for i in 0..16 {
                rb[i] = t[i % 8] ^ (i as u8).wrapping_mul(0x9E);
            }
        }
        sp -= 16;
        stack_write(&pages, stack_base, sp, &rb);
        random_addr = sp;
        let pth = aux.map(|a| a.exec_path).unwrap_or("");
        sp -= pth.len() + 1;
        stack_write(&pages, stack_base, sp, pth.as_bytes());
        stack_write(&pages, stack_base, sp + pth.len(), &[0]);
        execfn_addr = sp;
    }

    // 2. 16 字节对齐（argv/envp/auxv 指针区与 argc 共偶数个 8 字节槽则
    // 天然对齐，奇数时补一空槽在指针区顶）
    sp &= !0xFusize;

    // 2.5 对齐垫片（按下述压栈总数奇偶决定，先占位判断）
    // 压栈时间顺序（自顶向下）：垫片 → auxv(AT_NULL 起倒序) → envp NULL →
    // envp 指针 → argv NULL → argv 指针 → argc。
    // 向上读出的最终布局：argc | argv | NULL | envp | NULL | auxv..AT_NULL。
    let auxv = aux.map(|ai| {
        build_auxv(
            ai.phdr_va,
            ai.phnum,
            ai.phentsize,
            ai.entry,
            random_addr,
            execfn_addr,
            ai.uid,
        )
    });
    let aux_cnt = auxv.as_ref().map_or(0, |v| v.len());
    let qwords = 2 * aux_cnt + 1 + envc + 1 + argc + 1;
    if qwords % 2 == 1 {
        sp -= 8;
        stack_write_u64(&pages, stack_base, sp, 0);
    }

    // auxv 对（含末尾 AT_NULL）：先压 AT_NULL（最高位），再倒序压其余对
    if let Some(v) = &auxv {
        sp -= 8;
        stack_write_u64(&pages, stack_base, sp, 0);
        sp -= 8;
        stack_write_u64(&pages, stack_base, sp, AT_NULL);
        for &(k, val) in v.iter().rev().skip(1) {
            sp -= 8;
            stack_write_u64(&pages, stack_base, sp, val);
            sp -= 8;
            stack_write_u64(&pages, stack_base, sp, k);
        }
    }

    // 3. envp 终止符（NULL）
    sp -= 8;
    stack_write_u64(&pages, stack_base, sp, 0);

    // 4. envp 指针（倒序）
    for i in (0..envc).rev() {
        sp -= 8;
        stack_write_u64(&pages, stack_base, sp, env_addrs[i] as u64);
    }

    // 5. argv 终止符（NULL）
    sp -= 8;
    stack_write_u64(&pages, stack_base, sp, 0);

    // 6. argv 指针（倒序）
    for i in (0..argc).rev() {
        sp -= 8;
        stack_write_u64(&pages, stack_base, sp, str_addrs[i] as u64);
    }

    // 7. argc
    sp -= 8;
    stack_write_u64(&pages, stack_base, sp, argc as u64);

    Some((sp, stack_base))
}

/// auxv 键（Linux ABI）。
const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_HWCAP: u64 = 16;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// 为 musl/glibc 风格启动构造 auxv 数组（AT_RANDOM/AT_EXECFN 数据已由
/// setup_stack 写到栈上，此处只填键值对）。
fn build_auxv(
    phdr_va: usize,
    phnum: usize,
    phentsize: usize,
    entry: usize,
    random_ptr: usize,
    execfn_ptr: usize,
    uid: u64,
) -> alloc::vec::Vec<(u64, u64)> {
    let mut v = alloc::vec::Vec::with_capacity(20);
    v.push((AT_PHDR, phdr_va as u64));
    v.push((AT_PHENT, phentsize as u64));
    v.push((AT_PHNUM, phnum as u64));
    v.push((AT_PAGESZ, vm::PAGE_SIZE as u64));
    v.push((AT_ENTRY, entry as u64));
    v.push((AT_UID, uid));
    v.push((AT_EUID, uid));
    v.push((AT_GID, uid));
    v.push((AT_EGID, uid));
    v.push((AT_HWCAP, 0));
    v.push((AT_CLKTCK, 100));
    v.push((AT_SECURE, 0));
    v.push((AT_RANDOM, random_ptr as u64));
    v.push((AT_EXECFN, execfn_ptr as u64));
    v.push((AT_NULL, 0));
    v
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
/// 兼容入口：无环境变量（等价 `spawn_env(name, data, argv, &[])`）。
pub fn spawn(name: &'static str, data: &[u8], argv: &[&str]) -> u32 {
    spawn_env(name, data, argv, &[])
}

/// 带环境变量创建进程（envp 元素为 "NAME=VALUE" 形式；压入初始用户栈，
/// musl __init_libc 经 argv/envp 指针数组读取）。
pub fn spawn_env(name: &'static str, data: &[u8], argv: &[&str], envp: &[&str]) -> u32 {
    let mut img = match load(data) {
        Ok(i) => i,
        Err(e) => {
            crate::uart_send_str("[diag] 加载失败 err=");
            crate::print_dec(e as u64);
            crate::uart_send_str("\n");
            return u32::MAX;
        }
    };
    let stack_top = aslr_stack_top();
    let aux = AuxInit {
        phdr_va: img.phdr_va,
        phnum: img.phnum,
        phentsize: img.phentsize,
        entry: img.entry,
        exec_path: name,
        uid: 0,
    };
    let (initial_sp, stack_base) = match setup_stack(&mut img.pt, argv, envp, stack_top, Some(&aux)) {
        Some((sp, base)) => (sp, base),
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
            stack_top: initial_sp,
            stack_base,
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
    // 大小按文件实际尺寸分配（EXEC_MAX_CAP 只拦截异常文件）。
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        crate::uart_send_str("[diag] 启动进程打开失败: ");
        crate::uart_send_str(path);
        crate::uart_send_str("\n");
        return -1;
    }
    let fsize = vfs::file_size(fd);
    if fsize <= 0 || fsize as usize > EXEC_MAX_CAP {
        crate::uart_send_str("[diag] execve 文件大小异常 (");
        crate::uart_send_str(path);
        crate::uart_send_str(" size=");
        crate::print_dec(fsize as u64);
        crate::uart_send_str(")\n");
        vfs::close(fd);
        return -1;
    }
    let buf = crate::heap::kmalloc(fsize as usize);
    if buf.is_null() {
        crate::uart_send_str("[diag] execve 缓冲分配失败 (");
        crate::uart_send_str(path);
        crate::uart_send_str(" size=");
        crate::print_dec(fsize as u64);
        crate::uart_send_str(")\n");
        vfs::close(fd);
        return -1;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 4096];
        let n = vfs::read(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        if total + n as usize > fsize as usize {
            break; // 文件被并发截断：按已读量交给 load() 头部校验兜底
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
            crate::uart_send_str("[diag] execve 加载失败 (");
            crate::uart_send_str(path);
            crate::uart_send_str(" err=");
            crate::print_dec(e as u64);
            crate::uart_send_str(")\n");
            unsafe { crate::heap::kfree(buf) };
            return e;
        }
    };
    unsafe { crate::heap::kfree(buf) };

    // 3. 准备新用户栈（ASLR 随机化区顶；失败则回滚新地址空间，旧进程不变）
    let stack_top = aslr_stack_top();
    let aux = AuxInit {
        phdr_va: img.phdr_va,
        phnum: img.phnum,
        phentsize: img.phentsize,
        entry: img.entry,
        exec_path: path,
        uid: 0,
    };
    let (initial_sp, stack_base) = match setup_stack(&mut img.pt, argv, envp, stack_top, Some(&aux)) {
        Some((sp, base)) => (sp, base),
        None => {
            crate::uart_send_str("[diag] execve 栈映射失败 (");
            crate::uart_send_str(path);
            crate::uart_send_str(")\n");
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
    // ★ 终止同进程全部兄弟线程（必须在释放旧页表前）：CLONE_THREAD 线程共享
    // 地址空间，replace_address_space 归还旧根表后，兄弟线程的 TTBR0 悬空，
    // 下次被调度即命中已复用内存（与 exit 路径的 kill_pid 同族）。execve
    // 调用者自身必须继续运行，故用排除当前任务的变体。
    sched_cfs::kill_siblings_of(pid);
    process::replace_address_space(pid, img.pt);
    process::rename(pid, basename(path));
    // ★ execve 复位堆断点/mmap 游标：BRK_CUR 仅在 reap 时清零，同 pid 先
    // brk 再 execve（如 "malloc 后 re-exec"）会以旧断点为堆基线 —— 旧断点
    // 低于新映像段尾时，brk_set 的全新零页映射会静默覆盖新映像的
    // text/data（PTE 被同名 VA 覆盖）→ 新进程必然崩溃。以新映像的 brk
    // 重新初始化（brk_init 首次 brk(0) 时取 current_brk）。
    crate::bionic_shim::reset_brk_state(pid as usize);
    // 5. GUI 前台进程调度优待：launcher/oobe 及其拉起的应用提升权重，
    //    降低动画期间被后台进程（networkd 等）抢占的概率（CFS 语义不变）
    let name = basename(path);
    if is_gui_app(name) {
        sched_cfs::set_current_nice(-3);
    }

    // 启动计时（诊断）
    let t1 = timer::now_us();
    crate::kinfo! {
        crate::uart_send_str("[perf] execve ");
        crate::uart_send_str(path);
        crate::uart_send_str(" uid=");
        crate::print_dec(process::current_uid() as u64);
        crate::uart_send_str(" 耗时=");
        crate::print_dec((t1 - t0) as u64);
        crate::uart_send_str("us\n");
    }

    unsafe {
        if (pid as usize) < MAX_ELF_PROCS {
            ELF_PROCS[pid as usize] = ElfProc {
                used: true,
                entry,
                stack_top: initial_sp,
                stack_base,
                brk,
            };
        }
    }

    // 5. 改写陷阱帧：返回时 eret 到新入口
    f.sp_el0 = initial_sp as u64;
    f.elr = entry as u64;
    f.spsr = SPSR_EL0T;
    f.x[0] = 0;
    // 清零陷阱帧的 FP/SIMD 区（v0-v31 + fpsr/fpcr，@0x110..0x320）：execve 后
    // 新进程无浮点状态，避免内核栈残留的浮点常量（0.01 等）污染用户 v 寄存器
    // （残留值会被 syscall 返回路径恢复进用户态 → 浮点计算错乱 → 跳垃圾地址）。
    let fb = (f as *mut TrapFrame) as *mut u8;
    unsafe { core::ptr::write_bytes(fb.add(0x110), 0, 0x320 - 0x110) };
    // [诊断] 记录 execve 设置的初始栈顶与入口（核对 PC=0 故障中 sp 是否错位）
    crate::kinfo! {
        crate::uart_send_str("[execve-diag] entry=");
        crate::print_hex(entry as u64);
        crate::uart_send_str(" stack_top=");
        crate::print_hex(stack_top as u64);
        crate::uart_send_str("\n");
    }
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
    spawn_from_path_env(path, argv, &[])
}

/// 同 [`spawn_from_path`]，但把 `envp`（"NAME=VALUE"）传入初始栈。
pub fn spawn_from_path_env(path: &'static str, argv: &[&str], envp: &[&str]) -> i32 {
    let t0 = timer::now_us();
    // 读入缓冲按文件实际大小堆分配（此前固定 64KB：launcher 等大应用被静默
    // 截断后 load 必然失败，shell `run` 只能看到笼统的"加载失败"）。
    let fd = vfs::open(path, vfs::O_RDONLY);
    if fd < 0 {
        return -1;
    }
    let fsize = vfs::file_size(fd);
    if fsize <= 0 || fsize as usize > EXEC_MAX_CAP {
        vfs::close(fd);
        return -1;
    }
    let buf = crate::heap::kmalloc(fsize as usize);
    if buf.is_null() {
        vfs::close(fd);
        return -1;
    }
    let mut total = 0usize;
    loop {
        let mut tmp = [0u8; 4096];
        let n = vfs::read(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        if total + n as usize > fsize as usize {
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
    let pid = spawn_env(
        name,
        unsafe { core::slice::from_raw_parts(buf as *const u8, total) },
        argv,
        envp,
    );
    unsafe { crate::heap::kfree(buf) };
    if is_gui_app(name) {
        sched_cfs::set_current_nice(-3);
    }
    if pid != u32::MAX {
        crate::kinfo! {
            crate::uart_send_str("[perf] 启动进程 ");
            crate::uart_send_str(path);
            crate::uart_send_str(" 耗时=");
            crate::print_dec((timer::now_us() - t0) as u64);
            crate::uart_send_str("us\n");
        }
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
