//! 物理页分配器：用位图管理 0x40000000 - 0x7FFFFFFF（1GB）物理内存。

use core::arch::asm;
use core::ptr::{addr_of_mut, read_volatile, write_volatile};

/// 管理的内存范围（1GB）
pub const RAM_START: usize = 0x4000_0000;
pub const RAM_END: usize = 0x8000_0000;

/// 页大小：4KB
pub const PAGE_SIZE: usize = 4096;

/// 总页数：1GB / 4KB = 262144 页（约 256K 页）
pub const TOTAL_PAGES: usize = (RAM_END - RAM_START) / PAGE_SIZE;

/// 保存 DAIF 并屏蔽 IRQ（嵌套安全：恢复调用前的完整 DAIF 值）。
/// 分配/释放全程屏蔽 IRQ，避免任务侧 `alloc_page`/`free_page` 的位图
/// 读-改-写与 IRQ 侧（如 `net_virtio` used-ring 回收) 的并发破坏空闲链表。
unsafe fn irq_save() -> u64 {
    let d: u64;
    asm!("mrs {}, daif", out(reg) d);
    asm!("msr daifset, #2");
    d
}

unsafe fn irq_restore(saved: u64) {
    asm!("msr daif, {}", in(reg) saved);
}

/// 位图：每个 u64 管理 64 页，1 = 已用，0 = 空闲
const BITMAP_WORDS: usize = TOTAL_PAGES / 64; // 4096 个 u64 = 32KB

/// 位图数组（位于 .bss，由启动代码清零）
static mut BITMAP: [u64; BITMAP_WORDS] = [0; BITMAP_WORDS];

/// 实际可分配页数上限：初始化时按真实内存（DTB 的 /memory size）钳制，
/// 缺省为位图全量（1GB）。分配/统计都以此为准，防止把不存在的物理内存
/// 分配出去（QEMU -m 512M 时若按 1GB 分配，触碰未映射页会 data abort）。
static mut LIMIT_PAGES: usize = TOTAL_PAGES;

/// 返回位图数组首元素的原始指针。
///
/// # 安全
/// 仅在开启 IRQ 之前的初始化阶段被调用，单线程，无并发访问。
unsafe fn bitmap_ptr() -> *mut u64 {
    addr_of_mut!(BITMAP).cast::<u64>()
}

/// 设置位图中第 `page` 位（标记已用）。
///
/// # 安全
/// 仅在内核初始化阶段（开启 IRQ 之前）被调用，无并发访问。
unsafe fn bitmap_set(page: usize) {
    let word = page / 64;
    let bit = page % 64;
    let ptr = bitmap_ptr().add(word);
    write_volatile(ptr, read_volatile(ptr) | (1u64 << bit));
}

/// 初始化分配器：清空位图，并把内核占用的页标记为已用。
///
/// `kernel_end` 为内核镜像末尾地址（`_kernel_end`），向上取整到整页。
/// `kernel_pages` 被钳制到 `TOTAL_PAGES`，防止链接脚本符号异常时
/// `bitmap_set` 越界写位图数组之后的内存。
pub fn init(kernel_end: usize, ram_size: usize) {
    // 按真实内存钳制可分配页数（QEMU -m 512M → 512MB 内可分配）
    let effective_end = RAM_START.saturating_add(ram_size).min(RAM_END);
    let limit = (effective_end - RAM_START) / PAGE_SIZE;
    unsafe {
        LIMIT_PAGES = limit.clamp(1, TOTAL_PAGES);
    }
    // 内核占用的页数（0x40000000 到 kernel_end 之间，向上取整）
    let kernel_pages = kernel_end
        .saturating_sub(RAM_START)
        .div_ceil(PAGE_SIZE)
        .min(TOTAL_PAGES);

    unsafe {
        // 清空位图：全部标记为空闲
        for i in 0..BITMAP_WORDS {
            write_volatile(bitmap_ptr().add(i), 0);
        }
        // 标记内核区域已用
        for p in 0..kernel_pages {
            bitmap_set(p);
        }
    }
}

/// 分配一页物理内存，返回页的起始物理地址；失败返回 `None`。
pub fn alloc_page() -> Option<usize> {
    let saved = unsafe { irq_save() };
    let mut found: Option<usize> = None;
    unsafe {
        // 线性扫描位图，找第一个空闲位（命中后必须走到尾部 irq_restore，
        // 不可在临界区内提前 return，否则屏蔽态泄漏到调用方）
        'outer: for word in 0..BITMAP_WORDS {
            let bits = read_volatile(bitmap_ptr().add(word));
            // 该字已全满则跳过
            if bits == u64::MAX {
                continue;
            }
            for bit in 0..64 {
                let page = word * 64 + bit;
                if page >= LIMIT_PAGES {
                    break;
                }
                if (bits >> bit) & 1 == 0 {
                    bitmap_set(page);
                    found = Some(RAM_START + page * PAGE_SIZE);
                    break 'outer;
                }
            }
        } // 内存耗尽时 found 保持 None
    };
    unsafe { irq_restore(saved) };
    found
}

/// 分配 N 个**物理连续**的页，返回第一页起始地址；失败返回 `None`。
///
/// 供内核栈、大块堆分配、帧缓冲、ramdisk 等需要连续物理内存的场景使用。
pub fn alloc_pages_contig(n: usize) -> Option<usize> {
    let saved = unsafe { irq_save() };
    let r = if n == 0 || n > unsafe { LIMIT_PAGES } {
        None
    } else {
        unsafe {
            let mut run = 0usize;
            let mut start_page = 0usize;
            for page in 0..LIMIT_PAGES {
                let word = page / 64;
                let bit = page % 64;
                let bits = read_volatile(bitmap_ptr().add(word));
                if (bits >> bit) & 1 == 0 {
                    if run == 0 {
                        start_page = page;
                    }
                    run += 1;
                    if run == n {
                        for p in start_page..start_page + n {
                            bitmap_set(p);
                        }
                        break;
                    }
                } else {
                    run = 0;
                }
            }
            if run == n {
                Some(RAM_START + start_page * PAGE_SIZE)
            } else {
                None
            }
        }
    };
    unsafe { irq_restore(saved) };
    r
}

/// 统计当前已分配（位图中置 1）的物理页数，用于泄漏检测。
pub fn used_pages() -> usize {
    let mut count = 0usize;
    unsafe {
        for word in 0..BITMAP_WORDS {
            count += read_volatile(bitmap_ptr().add(word)).count_ones() as usize;
        }
    }
    count
}

/// 当前空闲物理页数（供内存压力检测：`free = total - used`）。
pub fn free_pages() -> usize {
    total_pages().saturating_sub(used_pages())
}

/// 把 `[start, start+len)` 物理区间标记为已用，分配器不再分配这些页。
///
/// 供启动画面帧缓冲/交接信息等固定地址区域使用：内核接管 bootloader 渲染的
/// 画面后，必须防止后续分配覆写正在扫描输出的像素。
pub fn reserve_region(start: usize, len: usize) {
    if len == 0 || start < RAM_START {
        return;
    }
    // checked：`start + len - 1` 裸加法可回绕（len 无上界），回绕后 `last < first`
    // 会只预留部分甚至完全不预留本应保留的区域。改用 checked/saturating 计算末尾。
    let Some(end) = start.checked_add(len) else { return };
    let end = end.min(RAM_END);
    if end <= start {
        return;
    }
    let first = (start - RAM_START) / PAGE_SIZE;
    let last = (end - 1 - RAM_START) / PAGE_SIZE;
    let limit = unsafe { LIMIT_PAGES };
    let saved = unsafe { irq_save() };
    for p in first..=last.min(limit.saturating_sub(1)) {
        unsafe {
            bitmap_set(p);
        }
    }
    unsafe { irq_restore(saved) };
}

/// 物理内存总页数（分配器实际可分配的全部页，按真实内存钳制）。
pub fn total_pages() -> usize {
    unsafe { LIMIT_PAGES }
}

/// 释放一页物理内存。永久页（`mark_permanent`）直接忽略，永不释放。
pub fn free_page(addr: usize) {
    // 地址必须位于管理范围内且页对齐
    let bad = !(addr >= RAM_START && addr < RAM_END && addr % PAGE_SIZE == 0);
    if bad {
        let lr: u64;
        unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
        crate::uart_send_str("[free] BAD addr=");
        crate::print_hex(addr as u64);
        crate::uart_send_str(" lr=");
        crate::print_hex(lr);
        crate::uart_send_str("\n");
    }
    assert!(!bad);
    let page = (addr - RAM_START) / PAGE_SIZE;
    assert!(page < unsafe { LIMIT_PAGES }, "allocator：空闲页超出限制");
    let word = page / 64;
    let bit = page % 64;
    let saved = unsafe { irq_save() };
    unsafe {
        if read_volatile(PERMANENT.as_ptr().add(word)) & (1u64 << bit) != 0 {
            irq_restore(saved);
            return; // 永久页：不释放
        }
        let ptr = bitmap_ptr().add(word);
        write_volatile(ptr, read_volatile(ptr) & !(1u64 << bit));
    }
    unsafe { irq_restore(saved) };
}

// ==================== 共享只读页引用计数（fork 提速） ====================
//
// ELF 的只读段（代码 / rodata，AP_USER_RO）在进程间共享同一物理页：
// fork 时不再逐字节拷贝，而是把映射记录复制到子进程并引用计数 +1；
// 进程退出 / exec 释放地址空间时引用计数 -1，归零才真正归还物理页。
// 数据段（RW）仍按现状逐页拷贝，不参与计数。

/// 永久页位图：这些页一旦分配永不释放/复用（如全局共享的信号蹦床页）。
/// free_page 对永久页直接跳过，避免共享引用计数归零后把仍被其它进程
/// 映射的页释放并复用（表现为蹦床 VA 指向被复用的内存 → 未定义指令）。
static mut PERMANENT: [u64; BITMAP_WORDS] = [0; BITMAP_WORDS];

/// 把一页标记为永久页（此后 free_page 忽略它）。
pub fn mark_permanent(addr: usize) {
    if addr >= RAM_START && addr < RAM_END && addr % PAGE_SIZE == 0 {
        let page = (addr - RAM_START) / PAGE_SIZE;
        if page < unsafe { LIMIT_PAGES } {
            unsafe {
                PERMANENT[page / 64] |= 1u64 << (page % 64);
            }
        }
    }
}

/// 每物理页的共享引用计数（u16；0 = 普通私有页，>0 = 被 n 个进程共享）。
/// 位于 .bss，占用 512KB（262144 × 2B），不随内核镜像落盘。
static mut PAGE_REFS: [u16; TOTAL_PAGES] = [0; TOTAL_PAGES];

#[inline]
fn page_index(addr: usize) -> usize {
    assert!(addr >= RAM_START && addr < RAM_END && addr % PAGE_SIZE == 0);
    (addr - RAM_START) / PAGE_SIZE
}

/// 标记一页进入共享池（ELF 加载只读段时调用）：引用计数置 1。
pub fn mark_shared_page(addr: usize) {
    let saved = unsafe { irq_save() };
    unsafe {
        PAGE_REFS[page_index(addr)] = 1;
    }
    unsafe { irq_restore(saved) };
}

/// fork 共享：引用计数 +1（饱和，防溢出）。
pub fn share_page(addr: usize) {
    // RMW 必须 IRQ 屏蔽：进程上下文（IRQ 使能）与另一核的 exit→
    // free_user_pages 交错在 load/store 之间会丢一次递增/递减 —— 计数
    // 永不归零 = 4KB 泄漏（与 free_page 同款防护）。
    let saved = unsafe { irq_save() };
    unsafe {
        let r = &mut PAGE_REFS[page_index(addr)];
        *r = r.saturating_add(1);
    }
    unsafe { irq_restore(saved) };
}

/// COW fork 共享：把一页私有页（计数 0）转为共享并 +1。
///
/// fork 时父进程的 RW 数据页原本是私有页（`PAGE_REFS == 0`，仅父进程持有）。
/// 转为 COW 后父子各持有一份引用：计数 0 → 1（父）→ 2（父+子）。
/// 若该页此前已是共享/COW 页（计数 > 0），仅 +1（子进程加入）。
pub fn cow_share(addr: usize) {
    let saved = unsafe { irq_save() };
    unsafe {
        let r = &mut PAGE_REFS[page_index(addr)];
        if *r == 0 {
            *r = 1;
        }
        *r = r.saturating_add(1);
    }
    unsafe { irq_restore(saved) };
}

/// fork/退出释放共享引用：引用计数 -1。返回 `true` 表示归零，
/// 调用方应随后 `free_page` 归还物理页。
pub fn unshare_page(addr: usize) -> bool {
    let saved = unsafe { irq_save() };
    let zero = unsafe {
        let r = &mut PAGE_REFS[page_index(addr)];
        if *r > 0 {
            *r -= 1;
        }
        *r == 0
    };
    unsafe { irq_restore(saved) };
    zero
}
