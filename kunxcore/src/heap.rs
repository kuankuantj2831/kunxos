//! 内核堆分配器：slab 算法 + 大块直连页分配。
//!
//! 小对象按大小分级（8..2048），每个 slab 为一整页，划分为若干等大对象，
//! 空闲对象以链表组织。整页 slab 释放完后归还物理页分配器（无泄漏）。
//! 超过 2048 字节的分配按整页（带头部）直接分配，释放时按头记录归还页。
//!
//! 并发安全：`kmalloc`/`kfree` 全程屏蔽 IRQ（读取/恢复 DAIF），避免任务侧
//! 分配与 IRQ 侧 `kfree`（如 `net_virtio` used-ring 回收）并发改写同一 slab
//! 槽的空闲链表/物理页位图 —— IRQ 保存-恢复式屏蔽可安全嵌套。
//!
//! 释放安全：每物理页维护「所有者」标签（`PAGE_TAG`），`kfree` 先按标签
//! 精确判定对象归属；整页 slab 被回收并重新用作其它分级/大块后，陈旧指针
//! 的二次释放会被标签/魔数拦截（防双释放破坏新分配）。大块头部直接放在
//! 返回指针之前，释放可 O(1) 反推基址与页数。

use crate::allocator::{self, PAGE_SIZE};
use core::arch::asm;

/// 大小分级（字节）
pub const CLASS_SIZES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];
/// slab 数量上限（fontdue 解析 cn.ttf 字体子集时小对象分配密集，需要较大余量）
const MAX_SLABS: usize = 4096;

/// 大块分配头部魔数，用于识别/校验大块释放。
const LARGE_MAGIC: u64 = 0x4B_4D_41_4C_4C_4F_43; // "KMALLOC"

/// 每物理页的「所有者」标签（.bss，u16 表）：
/// - `0`：不属于堆（空闲页 / 尚未登记）；
/// - `slot+1`：该页属于 slab 槽位 `slot`；
/// - `0xFFFF`：该页属于某大块分配。
///
/// 用于 `kfree` 精确判定对象归属：整页 slab 释放后被重新用作其它 class 的
/// slab 或大块时，陈旧指针的二次释放经标签即被识别为非法，不再被新宿主
/// 的错误路径接受（防止双释放破坏新分配）。
const MAX_PAGES: usize = allocator::TOTAL_PAGES;
const PAGE_TAG_LARGE: u16 = 0xFFFF;
static mut PAGE_TAG: [u16; MAX_PAGES] = [0; MAX_PAGES];

/// 保存 DAIF 并屏蔽 IRQ（嵌套安全：恢复的是调用前的完整 DAIF 值）。
unsafe fn irq_save() -> u64 {
    let d: u64;
    asm!("mrs {}, daif", out(reg) d);
    asm!("msr daifset, #2");
    d
}

unsafe fn irq_restore(saved: u64) {
    asm!("msr daif, {}", in(reg) saved);
}

/// 单个 slab 的描述。
#[derive(Clone, Copy)]
struct Slab {
    /// slab 页基地址（`base == 0` 表示该槽位空闲）
    base: usize,
    /// 所属大小分级索引
    class: usize,
    /// 空闲对象链表头（0 = 空）
    free_head: usize,
    /// 当前空闲对象数
    free_count: usize,
    /// 本 slab 对象总数
    total: usize,
}

const EMPTY_SLAB: Slab = Slab { base: 0, class: 0, free_head: 0, free_count: 0, total: 0 };

/// 全局 slab 表（位于 .bss）。
static mut SLABS: [Slab; MAX_SLABS] = [EMPTY_SLAB; MAX_SLABS];

/// 把 `size` 向上取整到最小满足的分级，返回分级索引；超过 2048 返回 `None`。
fn size_to_class(size: usize) -> Option<usize> {
    CLASS_SIZES.iter().position(|&s| s >= size)
}

/// 查找一个有空闲对象且属于 `class` 的 slab，返回其槽位索引。
fn find_slab_with_free(class: usize) -> Option<usize> {
    unsafe {
        for i in 0..MAX_SLABS {
            let s = SLABS[i];
            if s.base != 0 && s.class == class && s.free_count > 0 {
                return Some(i);
            }
        }
    }
    None
}

/// 把物理页地址映射到页索引；越界返回 `None`。
fn page_index(p: usize) -> Option<usize> {
    if p >= allocator::RAM_START
        && p < allocator::RAM_END
        && p % PAGE_SIZE == 0
        && (p - allocator::RAM_START) / PAGE_SIZE < MAX_PAGES
    {
        Some((p - allocator::RAM_START) / PAGE_SIZE)
    } else {
        None
    }
}

/// 新建一个 `class` 分级的 slab：分配一整页并初始化空闲链表。失败返回 `None`。
unsafe fn slab_create(class: usize) -> Option<usize> {
    let slot = (0..MAX_SLABS).find(|&i| SLABS[i].base == 0)?;
    let base = allocator::alloc_page()?;
    let size = CLASS_SIZES[class];
    let total = PAGE_SIZE / size;

    // 链接空闲对象：每个空闲对象的前 8 字节存放下一个对象地址
    let mut head = 0usize;
    for i in (0..total).rev() {
        let obj = base + i * size;
        *(obj as *mut usize) = head;
        head = obj;
    }

    SLABS[slot] = Slab { base, class, free_head: head, free_count: total, total };
    if let Some(pi) = page_index(base) {
        PAGE_TAG[pi] = slot as u16 + 1;
    }
    Some(slot)
}

/// 从 `class` 分级分配一个对象。OOM 返回 `None`（不再 panic 挂内核）。
unsafe fn slab_alloc(class: usize) -> Option<usize> {
    let slot = find_slab_with_free(class).or_else(|| slab_create(class))?;
    let s = &mut SLABS[slot];
    let obj = s.free_head;
    s.free_head = *(obj as *const usize);
    s.free_count -= 1;
    Some(obj)
}

/// 把对象 `ptr` 归还到 `slot` 号 slab；整页空闲时归还物理页并回收槽位。
///
/// 释放前校验对象归属：`ptr` 必须落在 slab 页内、对齐到该分级对象大小，
/// 否则静默忽略（防御错误释放/重复释放破坏空闲链表）。
unsafe fn slab_free(slot: usize, ptr: usize) {
    let s = &mut SLABS[slot];
    let size = CLASS_SIZES[s.class];
    if ptr < s.base
        || ptr >= s.base + PAGE_SIZE
        || (ptr - s.base) % size != 0
        || s.free_count >= s.total
    {
        return; // 越界/未对齐/重复释放：忽略
    }
    // 半满 slab 的精确双重释放检测：对象已在空闲链表中 → 拒绝。此前只挡
    // 「整页已空」（free_count == total），半满状态下对同一对象二次释放会把
    // 空闲链表插坏（同一对象随后被分配两次，互相踩踏）。遍历上界 =
    // free_count ≤ total ≤ 512，IRQ 已屏蔽，最坏几微秒。
    {
        let mut cur = s.free_head;
        let mut n = s.free_count;
        while n > 0 {
            if cur == ptr {
                return; // 已在空闲链表：重复释放，忽略
            }
            cur = *(cur as *const usize);
            n -= 1;
        }
    }
    *(ptr as *mut usize) = s.free_head;
    s.free_head = ptr;
    s.free_count += 1;
    if s.free_count == s.total {
        allocator::free_page(s.base);
        if let Some(pi) = page_index(s.base) {
            PAGE_TAG[pi] = 0;
        }
        *s = EMPTY_SLAB;
    }
}

/// 大块分配头：**直接位于返回指针之前**（`ptr - header_size` 处）。按 `align`
/// 对齐的内部指针释放时可立即反推头部（O(1)），头部记录整块首地址与页数，
/// 释放边界一目了然。
#[repr(C)]
#[derive(Clone, Copy)]
struct LargeHeader {
    magic: u64,
    pages: usize,
    base: usize,
}

/// 大块分配：分配足够页数，把头部放在返回指针之前（`ptr - header_size`），
/// 返回按 `align`（≥16）对齐的内部指针。OOM 返回 `None`。
unsafe fn large_alloc(size: usize, align: usize) -> Option<usize> {
    let align = if align < 16 { 16 } else { align };
    let hdr_size = core::mem::size_of::<LargeHeader>();
    // checked：`size + 头部` 回绕时直接失败（此前裸加法可回绕 → 页数错误/越界）
    let base_need = size.checked_add(hdr_size)?;
    let mut pages = base_need.div_ceil(PAGE_SIZE);
    let Some(start) = allocator::alloc_pages_contig(pages) else { return None };
    let body = start + hdr_size;
    let aligned = ((body + align - 1) / align) * align;
    // 头部在 `aligned - hdr_size`，必须在块起始（start）之上；数据区必须在块内
    if aligned < start + hdr_size {
        for i in 0..pages {
            allocator::free_page(start + i * PAGE_SIZE);
        }
        return None;
    }
    let end_need = aligned.checked_add(size)?;
    // 页数必须是「从块起始到数据末尾」的相对跨度，而不是绝对地址除以页大小：
    // 此前用 `end_need.div_ceil(PAGE_SIZE)`（绝对地址/4K）会在 start 较高时算出
    // 数十万页的荒谬页数 → 重分配必然失败（OOM 误报）。
    let need_pages = end_need.saturating_sub(start).div_ceil(PAGE_SIZE);
    if need_pages > pages {
        // 对齐把数据推出已分配的页：释放后按实际页数重分配
        for i in 0..pages {
            allocator::free_page(start + i * PAGE_SIZE);
        }
        pages = need_pages;
        let Some(start2) = allocator::alloc_pages_contig(pages) else { return None };
        let body2 = start2 + hdr_size;
        let aligned2 = ((body2 + align - 1) / align) * align;
        if aligned2 < start2 + hdr_size {
            for i in 0..pages {
                allocator::free_page(start2 + i * PAGE_SIZE);
            }
            return None;
        }
        // ★ 复核第二次对齐后的跨度：need_pages 是按第一次对齐算的，若新的
        // 对齐偏移更大（pad2 > pad1），数据区尾部可能越过已分配页写入相邻
        // 分配的内存。越界时释放并按 OOM 处理（安全失败优于静默堆破坏）。
        let end2 = aligned2.checked_add(size);
        let need2 = end2.map(|e| e.saturating_sub(start2).div_ceil(PAGE_SIZE));
        if need2.map_or(true, |n| n > pages) {
            for i in 0..pages {
                allocator::free_page(start2 + i * PAGE_SIZE);
            }
            return None;
        }
        let hdr = (aligned2 - hdr_size) as *mut LargeHeader;
        (*hdr).magic = LARGE_MAGIC;
        (*hdr).pages = pages;
        (*hdr).base = start2;
        for p in 0..pages {
            if let Some(pi) = page_index(start2 + p * PAGE_SIZE) {
                PAGE_TAG[pi] = PAGE_TAG_LARGE;
            }
        }
        return Some(aligned2);
    }
    let hdr = (aligned - hdr_size) as *mut LargeHeader;
    (*hdr).magic = LARGE_MAGIC;
    (*hdr).pages = pages;
    (*hdr).base = start;
    for p in 0..pages {
        if let Some(pi) = page_index(start + p * PAGE_SIZE) {
            PAGE_TAG[pi] = PAGE_TAG_LARGE;
        }
    }
    Some(aligned)
}

/// 大块释放：由返回指针反推头部（`ptr - header_size`），校验后归还所有页。
///
/// 防御性校验：魔数必须匹配、页数与基址合理、`ptr` 必须落在
/// `[base, base + pages*PAGE)` 内；非法/重复释放静默忽略。成功后清除标签
/// 并抹掉魔数（旧指针二次释放不再命中）。
unsafe fn large_free(ptr: usize) {
    let hdr_size = core::mem::size_of::<LargeHeader>();
    if ptr < hdr_size {
        return;
    }
    let hdr = (ptr - hdr_size) as *const LargeHeader;
    if (*hdr).magic != LARGE_MAGIC {
        return; // 非本分配器产物，忽略
    }
    let pages = (*hdr).pages;
    let base = (*hdr).base;
    let total_pages = allocator::total_pages();
    if pages == 0 || pages > total_pages {
        return; // 页数明显异常：忽略
    }
    if base < allocator::RAM_START
        || base % PAGE_SIZE != 0
        || base + pages * PAGE_SIZE > allocator::RAM_END
    {
        return; // 基址/终止越界：忽略
    }
    if ptr < base || ptr >= base + pages * PAGE_SIZE {
        return; // 指针不在块范围内：忽略
    }
    for i in 0..pages {
        allocator::free_page(base + i * PAGE_SIZE);
        if let Some(pi) = page_index(base + i * PAGE_SIZE) {
            PAGE_TAG[pi] = 0;
        }
    }
    let hm = (ptr - hdr_size) as *mut LargeHeader;
    (*hm).magic = 0;
}

/// 分配 `size` 字节堆内存，返回对齐到对象大小的指针；OOM/参数非法返回空指针。
///
/// 大小向上取整到最近分级；超过 2048 字节按整页大块分配。全程屏蔽 IRQ。
pub fn kmalloc(size: usize) -> *mut u8 {
    kmalloc_aligned(size, 16)
}

/// 分配 `size` 字节、对齐到 `align` 的堆内存；OOM/参数非法返回空指针。
///
/// 小对象取 `max(size, align)` 分级（分级为 2 的幂，自动满足对齐）；
/// 大对象经 `large_alloc` 做内部对齐。全程屏蔽 IRQ。
pub fn kmalloc_aligned(size: usize, align: usize) -> *mut u8 {
    let size = size.max(1);
    let saved = unsafe { irq_save() };
    let ptr = unsafe {
        // align 超过最大 slab 类（2048）时不能压进 slab：页内槽位按
        // `base + i*cls` 排布，奇数槽位仅 cls 对齐，无法保证更大对齐
        // （GlobalAlloc Layout 违约 = UB），必须走 large_alloc。
        if align > 2048 {
            large_alloc(size.max(align), align).map(|p| p as *mut u8)
        } else {
            let cls_size = size.max(align).max(1);
            match size_to_class(cls_size) {
                Some(class) => slab_alloc(class).map(|p| p as *mut u8),
                None => large_alloc(size.max(align), align).map(|p| p as *mut u8),
            }
        }
    };
    unsafe { irq_restore(saved) };
    ptr.unwrap_or(core::ptr::null_mut())
}

/// 释放 `kmalloc` 返回的指针。全程屏蔽 IRQ。
///
/// # 安全
/// `ptr` 必须由 `kmalloc` 返回且未被释放过。
pub unsafe fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let p = ptr as usize;
    let saved = irq_save();
    // 按页标签精确判定归属，避免旧指针在页被重新用作其它 slab/大块后
    // 按「页范围扫描」误判进新宿主（双释放破坏新分配）。
    if let Some(pi) = page_index(p) {
        match PAGE_TAG[pi] {
            0 => {
                irq_restore(saved);
                return; // 页不属于堆：陈旧指针，忽略
            }
            0xFFFF => {
                large_free(p);
                irq_restore(saved);
                return;
            }
            tag => {
                let slot = tag as usize - 1;
                if slot < MAX_SLABS {
                    let s = SLABS[slot];
                    if s.base != 0 && p >= s.base && p < s.base + PAGE_SIZE {
                        slab_free(slot, p);
                        irq_restore(saved);
                        return;
                    }
                }
            }
        }
    }
    // 页标签缺失但落在已登记 slab 页内（历史分配）：按范围兜底
    for i in 0..MAX_SLABS {
        let s = SLABS[i];
        if s.base != 0 && p >= s.base && p < s.base + PAGE_SIZE {
            slab_free(i, p);
            irq_restore(saved);
            return;
        }
    }
    large_free(p);
    irq_restore(saved);
}

// ==================== Rust alloc 桥接 ====================

use core::alloc::{GlobalAlloc, Layout};

/// 把 Rust 的 `alloc` 接到内核 slab/大块堆。对齐通过 `kmalloc_aligned` 满足。
pub struct KernelAlloc;

unsafe impl GlobalAlloc for KernelAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        kmalloc_aligned(layout.size(), layout.align())
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        unsafe { kfree(ptr) }
    }
}

#[global_allocator]
pub static KERNEL_ALLOC: KernelAlloc = KernelAlloc;