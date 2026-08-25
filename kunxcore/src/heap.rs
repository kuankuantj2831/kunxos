//! 内核堆分配器：slab 算法 + 大块直连页分配。
//!
//! 小对象按大小分级（8..2048），每个 slab 为一整页，划分为若干等大对象，
//! 空闲对象以链表组织。整页 slab 释放完后归还物理页分配器（无泄漏）。
//! 超过 2048 字节的分配按整页（带 16 字节头）直接分配，释放时按头记录归还页。

use crate::allocator::{self, PAGE_SIZE};

/// 大小分级（字节）
pub const CLASS_SIZES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];
/// slab 数量上限
const MAX_SLABS: usize = 512;

/// 大块分配头部魔数，用于识别/校验大块释放。
const LARGE_MAGIC: u64 = 0x4B_4D_41_4C_4C_4F_43; // "KMALLOC"

/// 大块分配头：位于返回指针之前，记录页数与魔数。
#[repr(C)]
struct LargeHeader {
    magic: u64,
    pages: usize,
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
unsafe fn find_slab_with_free(class: usize) -> Option<usize> {
    for i in 0..MAX_SLABS {
        let s = SLABS[i];
        if s.base != 0 && s.class == class && s.free_count > 0 {
            return Some(i);
        }
    }
    None
}

/// 新建一个 `class` 分级的 slab：分配一整页并初始化空闲链表。
unsafe fn slab_create(class: usize) -> usize {
    let slot = (0..MAX_SLABS).find(|&i| SLABS[i].base == 0)
        .expect("heap: out of slab slots");
    let base = allocator::alloc_page().expect("heap: out of physical memory for slab");
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
    slot
}

/// 从 `class` 分级分配一个对象。
unsafe fn slab_alloc(class: usize) -> usize {
    let slot = find_slab_with_free(class).unwrap_or_else(|| slab_create(class));
    let s = &mut SLABS[slot];
    let obj = s.free_head;
    s.free_head = *(obj as *const usize);
    s.free_count -= 1;
    obj
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
    *(ptr as *mut usize) = s.free_head;
    s.free_head = ptr;
    s.free_count += 1;
    if s.free_count == s.total {
        allocator::free_page(s.base);
        *s = EMPTY_SLAB;
    }
}

/// 大块分配：`size` 字节 + 头部，按整页向上取整分配（物理连续）。
unsafe fn large_alloc(size: usize) -> usize {
    let total = size + core::mem::size_of::<LargeHeader>();
    let pages = total.div_ceil(PAGE_SIZE);
    let start = allocator::alloc_pages_contig(pages)
        .expect("heap: out of physical memory for large alloc");
    let hdr = start as *mut LargeHeader;
    (*hdr).magic = LARGE_MAGIC;
    (*hdr).pages = pages;
    start + core::mem::size_of::<LargeHeader>()
}

/// 大块释放：读取头部并归还所有页。
///
/// 防御性校验：头部地址必须页对齐（大块从页首开始）、魔数必须匹配；
/// 非法/重复释放静默忽略，避免写坏任意内存。
unsafe fn large_free(ptr: usize) {
    let start = ptr - core::mem::size_of::<LargeHeader>();
    // 头部 16 字节 + 数据必须落在同一页内且页对齐（大块分配从页首开始）
    if start & (PAGE_SIZE - 1) != 0 {
        return;
    }
    let hdr = start as *const LargeHeader;
    if (*hdr).magic != LARGE_MAGIC {
        return; // 非本分配器产物，忽略
    }
    let pages = (*hdr).pages;
    if pages == 0 || pages > 1024 {
        return; // 页数明显异常（>4MB 单块分配不可能）：忽略
    }
    for i in 0..pages {
        allocator::free_page(start + i * PAGE_SIZE);
    }
}

/// 分配 `size` 字节堆内存，返回对齐到对象大小的指针。
///
/// 大小向上取整到最近分级；超过 2048 字节按整页大块分配。
pub fn kmalloc(size: usize) -> *mut u8 {
    let size = size.max(1);
    unsafe {
        match size_to_class(size) {
            Some(class) => slab_alloc(class) as *mut u8,
            None => large_alloc(size) as *mut u8,
        }
    }
}

/// 释放 `kmalloc` 返回的指针。
///
/// # Safety
/// `ptr` 必须由 `kmalloc` 返回且未被释放过。
pub unsafe fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let p = ptr as usize;
    // 先在 slab 表中查找所属 slab
    for i in 0..MAX_SLABS {
        let s = SLABS[i];
        if s.base != 0 && p >= s.base && p < s.base + PAGE_SIZE {
            slab_free(i, p);
            return;
        }
    }
    // 未命中 slab → 按大块释放
    large_free(p);
}
