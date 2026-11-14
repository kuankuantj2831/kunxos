//! 虚拟内存管理：页表漫游、`map_page`/`unmap_page`、描述符构造与用户页表创建。
//!
//! 页表结构（RISC-V Sv39，4KB 粒度，satp.MODE=8，三级页表）：
//!   L1（根表，512 项 × 1GB）→ L2（512 项 × 2MB）→ L3（512 项 × 4KB）
//!
//! 与 aarch64（T0SZ=25 三级表）结构同构：L1/L2 支持 1GB/2MB 大页（superpage），
//! `map_page_in` 按需分配中间级表；遇到大页先拆表（split block）保证任意 4KB
//! 页可独立映射。内核恒等映射位于 0x80000000 起（L1[2]），用户空间整体落在
//! L1[0]（<1GB），与内核共享表彻底分离。
//!
//! 描述符位（Sv39 PTE）：V=0、R=1、W=2、X=3、U=4、G=5、A=6、D=7、
//! RSW=[9:8]（软件位，COW 标记用 bit8）；PPN 位于 bits[53:10]。
//! A/D 恒置 1（QEMU svadu 也会硬件置位，显式置上避免访问权限 fault）。

use core::arch::asm;
use core::ptr::addr_of;

use crate::allocator;

#[path = "page_records.rs"]
mod page_records;

/// 页大小
pub const PAGE_SIZE: usize = 4096;
/// L2 大页大小（2MB）
const L2_BLOCK: usize = 1 << 21;
/// RAM 物理起始（用于恒等映射构造；QEMU virt RISC-V DRAM 基址 0x80000000）
pub const RAM_START: usize = 0x8000_0000;

// ==================== 地址类型与用户内存布局 ====================

/// 物理地址。
pub type PhysAddr = usize;
/// 虚拟地址。
pub type VirtAddr = usize;

/// 用户代码段起始（与各应用链接脚本 `ENTRY`/`.`=0x400000` 保持一致）。
pub const USER_CODE_BASE: VirtAddr = 0x0040_0000;
/// 用户数据段起始（紧随代码段之后）。
pub const USER_DATA_BASE: VirtAddr = 0x0050_0000;
/// 用户堆（brk）下限：置于 QEMU virt RISC-V 设备窗口之上。
///
/// ⚠️ 低 1GB 中的 [0x0C00_0000, 0x1100_0000) 是内核设备区（PLIC 0x0C000000
/// 起 6MB、UART/virtio-mmio/fw_cfg 0x10000000-0x10110000、留余量到
/// 0x11000000）。应用堆（brk bump 分配器）跨入该窗口时会拆分继承的 1GB
/// 设备大页、以普通内存页覆盖设备寄存器（aarch64 版本实测教训：GIC 被
/// 覆盖 → 中断风暴 → 全系统冻结）。堆下限放到设备窗口之上，向
/// [0x1100_0000, 0x3D00_0000)（file-mmap 区下界）增长，约 720MB 余量。
pub const USER_HEAP_BASE: VirtAddr = 0x1100_0000;

/// 内核设备窗口（用户映射禁区）：QEMU virt RISC-V 平台 PLIC / UART /
/// virtio-mmio / fw_cfg / goldfish-RTC 寄存器区。用户 brk/mmap/ELF/
/// mmap(MAP_FIXED) 不得映射该区间，`user_va_range_ok` 对其返回 false
/// （add_page 层硬断言兜底）。
pub const USER_DEVICE_WINDOW_BASE: VirtAddr = 0x0C00_0000;
pub const USER_DEVICE_WINDOW_END: VirtAddr = 0x1100_0000;

/// 用户栈顶：置于内核恒等映射（0x80000000）之下 1GB 内，使整个用户地址
/// 空间落在 L1[0]（0x0000_0000-0x3FFF_FFFF）区域，从而与共享的内核 RAM
/// 恒等映射（L1[2]）彻底分离，保证多进程各自持有独立的用户页表而不互相污染。
pub const USER_STACK_TOP: VirtAddr = 0x3F00_0000;
/// 用户栈大小：2MB（向下增长，execve 时饿式映射全部页）。
///
/// 历史意图是 8MB（NetSurf 深递归）+ 按需落地，但 setup_stack 是饿式
/// 映射：8MB × MAX_PROC=24 并发上限 = 192MB 物理页，512MB 机型不可承受。
/// 实测 2MB 下 nsmonkey 冒烟通过；8MB 需先做栈区按需落地（guard 页 +
/// 缺页处理）再放开。ASLR 随机化窗口已预留 16MB 区间，未来扩容只改此值。
pub const USER_STACK_SIZE: usize = 0x0020_0000;
/// 用户栈底。
pub const USER_STACK_BASE: VirtAddr = USER_STACK_TOP - USER_STACK_SIZE;

/// ASLR 用户栈随机化窗口（页数）：栈区顶在 `[USER_STACK_TOP - 16MB,
/// USER_STACK_TOP)` 内随机（16MB = 4096 页熵）。栈区下方保留 15MB 隔离带
/// （匿名/文件 mmap 游标上限 `bionic_shim::MMAP_CAP = 0x3D00_0000`，
/// 最低栈底 0x3D90_0000 之下还有守护空间），栈溢出不会静默写进 mmap 区。
pub const STACK_ASLR_PAGES: usize = 0x1000;

/// 内核空间起始（恒等映射内核位于 0x80000000 起，此常量仅用于描述内核与
/// 用户空间的逻辑分界（用户指针上界）。
pub const KERNEL_SPACE_BASE: VirtAddr = 0x8000_0000_0000_0000;

/// 用户虚拟地址上界（1GB）：用户空间整体落在 L1[0]，与内核恒等映射
/// （L1[2]，0x80000000 起）彻底分离。所有用户映射必须严格落于其下，
/// 否则会经继承的内核共享 L2/L3 页表改写内核全局页表（安全关键），
/// 且 `free_process_tables` 只回收 L1[0] 派生的页表。
pub const USER_VA_TOP: VirtAddr = 0x4000_0000;

/// 校验 `[va, va+len)` 是否完整落在用户地址空间内（不含 0 地址，不含内核恒等映射，
/// 不与内核设备窗口重叠）。
pub fn user_va_range_ok(va: VirtAddr, len: usize) -> bool {
    if !(va > 0 && va.checked_add(len).map_or(false, |e| e <= USER_VA_TOP)) {
        return false;
    }
    // 内核设备窗口（PLIC/UART/virtio/fw_cfg 寄存器区）禁止任何用户映射：
    // 用户页表继承的 L1[0] 是 1GB 设备大页，向该区间建映射会拆分设备页、
    // 以普通内存页覆盖设备寄存器（中断风暴 / 内核外设失效）。
    let end = va + len;
    !(va < USER_DEVICE_WINDOW_END && end > USER_DEVICE_WINDOW_BASE)
}

/// 校验 `[va, va+len)` 中每一页都已建立有效映射。
///
/// ⚠️ 此校验**不漫游页表**，而是查询进程地址空间映射记录（`ProcPageTable::user_pages`）：
/// 所有用户页表都继承内核根表的 L1[0] = 1GB 设备大页，漫游页表会把 0~1GB 内任意
/// 虚拟地址（包括未映射空洞、PLIC 0x0C000000 / UART 0x10000000 设备区）误判为
/// 「已映射」，等于只做范围检查——内核解引用任意低地址将直接读写设备寄存器，
/// 解引用未映射地址会在 S 模式触发 store/load access fault 挂死内核（用户态 DoS）。
/// 映射记录按页维护、由 `add_page/reserve_page/…` 在建立映射时同步写入，
/// 是用户地址空间唯一权威来源；`LAZY_PAGE` 预留页（未落地）视为未映射。
pub fn user_range_mapped(root: usize, va: VirtAddr, len: usize) -> bool {
    // root=0：无地址空间（内核任务 / 未初始化进程槽）——视为未映射，
    // 防止调用方在 tick/采样等内核上下文里解引用空根表打挂内核。
    if root == 0 {
        return false;
    }
    if !user_va_range_ok(va, len) {
        return false;
    }
    let end = va + len;
    let mut cur = va & !(PAGE_SIZE - 1);
    while cur < end {
        // 记录表是权威来源（不误判设备页），但 SHM / framebuffer 等共享映射
        // 不写记录。未命中记录时再做「U 位可访问」的页表漫游：两路合取
        // —— 共享映射不再被误拒（此前 fork/SHM 用户缓冲被 EFAULT），
        // 设备页（无 U 位）依然被拒绝，杜绝「把设备区当用户内存」误判。
        if !crate::process::root_page_recorded(root, cur) && !user_page_pte_u(root, cur) {
            return false;
        }
        cur += PAGE_SIZE;
    }
    true
}

/// 漫游页表：`va` 所在叶子/大页的描述符是否允许 U 模式访问（U 位置 1）。
///
/// 用于记录表未命中的共享映射（SHM / framebuffer）校验：必须真正映射到
/// 用户可访问的地址。继承的内核设备大页（L1[0] = PLIC/UART 等）无 U 位，
/// 此函数对它们返回 `false` —— 与记录表一致，不把设备寄存器区放行给
/// 用户指针。
pub fn user_page_pte_u(root: usize, va: usize) -> bool {
    let flags = user_page_pte_flags(root, va);
    flags & (U | R) == U | R
}

pub fn user_page_pte_writable(root: usize, va: usize) -> bool {
    let flags = user_page_pte_flags(root, va);
    flags & (U | W) == U | W
}

fn user_page_pte_flags(root: usize, va: usize) -> u64 {
    if root == 0 {
        return 0;
    }
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    unsafe {
        let l1 = (*(root as *const PageTable)).entries[l1_idx];
        if l1 & VALID == 0 {
            return 0;
        }
        if l1 & LEAF_MASK != 0 {
            // 1GB 大页：仅当 U 可访问才放行
            return l1;
        }
        let l2p = pte_to_addr(l1) as *const PageTable;
        let l2 = (*l2p).entries[l2_idx];
        if l2 & VALID == 0 {
            return 0;
        }
        if l2 & LEAF_MASK != 0 {
            return l2;
        }
        let l3p = pte_to_addr(l2) as *const PageTable;
        let l3 = (*l3p).entries[l3_idx];
        if l3 & VALID != 0 && l3 & LEAF_MASK != 0 { l3 } else { 0 }
    }
}

// ==================== 描述符位定义 ====================

/// 有效位（bit0）
const VALID: u64 = 1 << 0;
/// 读权限（bit1；非叶表项要求 R=W=X=0，叶表项任一置 1 即为叶）
const R: u64 = 1 << 1;
/// 写权限（bit2）
const W: u64 = 1 << 2;
/// 执行权限（bit3）
const X: u64 = 1 << 3;
/// 用户模式可访问（bit4）
const U: u64 = 1 << 4;
/// 已访问（bit6）
const A: u64 = 1 << 6;
/// 已脏（bit7）
const D: u64 = 1 << 7;
/// 叶表项判定掩码（R|W|X 任一置位 = 叶）
const LEAF_MASK: u64 = R | W | X;
/// PPN 字段掩码（bits[53:10]，即页物理地址 <<10）
const ADDR_MASK: u64 = 0x003F_FFFF_FFC00;

/// PPN 字段 → 物理地址（右移 2 位：字段位 [53:10] 对应地址位 [53:12]）。
/// ★ Sv39 与 aarch64 的关键差异：aarch64 描述符输出地址与地址同位
/// （直接 & 掩码即得），Sv39 的 PPN 在 [53:10]，提取/装入都要移位。
#[inline]
fn pte_to_addr(pte: u64) -> usize {
    (((pte >> 10) & 0x3F_FFFF_FFFF_FFFF) << 12) as usize
}

/// 物理地址 → PPN 字段（左移 2 位装入 [53:10]）。
#[inline]
fn addr_to_ppn(addr: usize) -> u64 {
    ((addr as u64) >> 12) << 10
}
/// 低属性位掩码（bits[9:0]：V/RSW/U/R/W/X/G/A/D），拆分大页描述符时原样保留
const LOW_ATTR_MASK: u64 = 0x3FF;

// ==================== 映射标志 ====================

/// `map_page` 的 flags 参数类型：各标志位按位或组合。
///
/// RISC-V Sv39 无独立 UXN/PXN（执行权限只有 X 一位，S/U 共用）、无
/// AttrIndx（内存类型由 PMA 决定）——aarch64 的 ATTR_*/XN 位族在此退化为
/// 无操作，仅保留 AP（U+R+W 组合）与 A/D。
pub type MapFlags = u64;

/// 已访问/已脏（A|D）：恒置 1，避免访问标志 fault
pub const AF: MapFlags = A | D;
/// 可执行位（X）：EL0 调用方（ELF 段加载）显式置 1；未置即不可执行
pub const EXEC: MapFlags = X;
/// aarch64 UXN/PXN 的占位空操作（Sv39 无对应位）
#[allow(dead_code)]
pub const UXN: MapFlags = 0;
/// aarch64 PXN 的占位空操作
#[allow(dead_code)]
pub const PXN: MapFlags = 0;
/// 共享属性 SH 的占位空操作（单核 ASID=0 无全局/共享区分）
#[allow(dead_code)]
pub const SH_NON_SHAREABLE: MapFlags = 0;
#[allow(dead_code)]
pub const SH_OUTER: MapFlags = 0;
#[allow(dead_code)]
pub const SH_INNER: MapFlags = 0;

/// 内核读写（S 模式 R+W，无 X 不可执行，无 U）
pub const AP_EL1_RW: MapFlags = R | W;
/// 内核只读（S 模式 R）
pub const AP_EL1_RO: MapFlags = R;
/// 用户/内核均读写（U|R|W）
pub const AP_USER_RW: MapFlags = U | R | W;
/// 用户只读（U|R，EL0 写触发 store page fault → COW）
pub const AP_USER_RO: MapFlags = U | R;

/// 内存属性索引占位空操作（Sv39 由 PMA 决定，PTE 无属性字段）
pub const ATTR_NORMAL: MapFlags = 0;
pub const ATTR_DEVICE: MapFlags = 0;

// ==================== 常用标志组合 ====================

/// 内核数据页：S 读写、不可执行
pub const FLAG_KERNEL_DATA: MapFlags = AF | AP_EL1_RW;
/// 内核代码页：S 只读可执行
#[allow(dead_code)]
pub const FLAG_KERNEL_CODE: MapFlags = AF | AP_EL1_RO | X;
/// 用户数据页：U/S 读写、不可执行
pub const FLAG_USER_DATA: MapFlags = AF | AP_USER_RW;
/// 用户代码页：U 只读可执行
pub const FLAG_USER_CODE: MapFlags = AF | AP_USER_RO | X;

/// 访问权限 AP 字段掩码（U|R|W）。
pub const AP_MASK: MapFlags = U | R | W;

/// 是否为用户只读映射（AP 字段精确等于 `AP_USER_RO`）。
///
/// 注意不能写 `flags & AP_USER_RO != 0`：AP_USER_RO=U|R 与 AP_USER_RW=U|R|W
/// 共享 U|R 位，位与非零判断会把读写页误判为只读。
#[inline]
pub fn is_user_ro(flags: MapFlags) -> bool {
    (flags & AP_MASK) == AP_USER_RO
}

// ==================== 写时复制（COW）标记 ====================

/// COW 标记位：表示该用户页正被多个进程共享（fork 后尚未写）。
///
/// 语义：
/// - **记录层**（`UserPage.flags`）：置 `FLAG_COW` 表示"共享待复制"，
///   原始权限仍为 RW（`FLAG_USER_DATA` 的 AP 部分），fork 复用、写时复制。
/// - **页表层**：COW 页必须映射为**只读**（`AP_USER_RO`），使 U 模式首次
///   写入触发 store page fault → 内核 `cow_break` 复制后才恢复可写。
///
/// 位选择：bit 8 是 Sv39 PTE 的 RSW 软件可用位（bits[9:8]），硬件忽略，
/// 可安全写入页表描述符而不会改变翻译行为。
pub const FLAG_COW: MapFlags = 1 << 8;
/// Software-only PROT_NONE marker. Keep original permissions for page ownership
/// and COW reference accounting, but publish an invalid PTE until restored.
pub const FLAG_NO_ACCESS: MapFlags = 1 << 9;

pub fn user_readable(flags: MapFlags) -> bool {
    flags & FLAG_NO_ACCESS == 0 && flags & (U | R) == U | R
}

pub fn user_writable(flags: MapFlags) -> bool {
    flags & FLAG_NO_ACCESS == 0 && flags & (U | W) == U | W
}

/// Rebuild data permissions, explicitly discarding the old executable bit.
pub fn data_permissions(flags: MapFlags, ap: MapFlags) -> MapFlags {
    (flags & !AP_MASK & !EXEC & !FLAG_COW & !FLAG_NO_ACCESS) | ap
}

/// Any overlap, including lazy reservations and PROT_NONE records, is a
/// MAP_FIXED_NOREPLACE collision.
pub fn user_range_occupied(root: usize, va: usize, len: usize) -> bool {
    if !user_va_range_ok(va, len) { return true; }
    let mut page = va & !(PAGE_SIZE - 1);
    let end = va + len;
    while page < end {
        if crate::process::root_page_reserved(root, page) || user_page_pte_u(root, page) {
            return true;
        }
        page += PAGE_SIZE;
    }
    false
}

/// 是否为 COW 共享页（fork 后待复制）。
#[inline]
pub fn is_cow(flags: MapFlags) -> bool {
    flags & FLAG_COW != 0
}

/// 把记录层 flags 转换成**页表映射用** flags：
/// - COW 页：AP 强制为只读（`AP_USER_RO`），保留 COW 标记位（页表描述符
///   的 RSW 位，硬件忽略），使 U 模式写触发 store page fault。
/// - 非 COW 页：原样返回。
#[inline]
pub fn cow_pte_flags(record_flags: MapFlags) -> MapFlags {
    if is_cow(record_flags) {
        (record_flags & !AP_MASK) | AP_USER_RO
    } else {
        record_flags
    }
}

// ==================== 页表（静态分配，4KB 对齐，位于 .bss） ====================

/// 页表：512 个 64 位描述符。
#[repr(align(4096))]
pub struct PageTable {
    pub entries: [u64; 512],
}

/// 每个进程可跟踪的用户映射页数上限（vaddr/paddr/flags 三元组）。
///
/// 需要容纳最坏情况：launcher 含两个 2MB 帧缓冲（约 1017 页）+ 1MB 栈
/// （256 页）= 1273 页。接入 cn.ttf 平滑字体后，应用 bss 段膨胀（launcher
/// 约 4.2MB = 1062 页）+ 字体读取/解析堆（数 MB）可达 3000+ 页，
/// 16384 项（ProcPageTable 约 384KB）提供余量；超过则拒绝映射
/// （`add_page` 返回 `false`），调用方回滚，避免「只建映射不记录」
/// 导致的泄漏与 fork 不一致。字节 bump 下 brk ≈ fontdue 实际分配
/// ~48MB = 12300 页，16384 提供足够余量。
///
/// 2026-08：cn.ttf 子集约 3.1MB（3465 码点）经 fontdue 解析时用户堆实际
/// 增长 ~62MB（heap 从 0x600000 到 0x4395000 = 15765 页），加上 OOBE/launcher
/// 自身的代码/BSS 页（launcher bss 2379 页）会立刻耗尽 16384 记录 → brk
/// 映射失败 → alloc PANIC（首次开机 OOBE 崩溃的根因）。提高到 32768（128MB）
/// 后两者共需约 18144 页，仍留一半余量。
///
/// 2026-08-30：uiputong.ttf（cn 子集 1.68MB）与 bold.ttf（子集 1.68MB）由
/// `ttf_text::font_init()` 在同进程内一并解析，fontdue 每个字体约增长
/// 15765 页（实测同 3465 码点），双字体 ≈ 31530 页 + launcher OOBE bss
/// ~2400 页 ≈ 33900 页，已逼近 32768 → 提高到 65536（ProcPageTable 约 1.5MB），
/// 双字体加载与普通应用（bss 约 200-1000 页）继续留一半余量。
pub const MAX_USER_PAGES: usize = 65536;

/// 一条用户地址空间映射记录。
#[derive(Clone, Copy)]
pub struct UserPage {
    pub vaddr: VirtAddr,
    pub paddr: PhysAddr,
    pub flags: MapFlags,
}

/// 空映射记录。
pub const EMPTY_USER_PAGE: UserPage = UserPage { vaddr: 0, paddr: 0, flags: 0 };

/// 按需分页的"预留但未分配"标记：`UserPage.paddr` 取该值时表示地址空间已
/// 预留（mmap/brk 已返回该范围）、但物理页尚未分配、页表尚无映射。
///
/// 用户态首次访问该地址会触发 U 模式 load/store page fault，内核在缺页
/// 处理器中落地（分配物理页 + 清零 + 建映射 + 更新记录）后重试故障指令，
/// 实现按需分页。`usize::MAX` 不是合法物理地址，任何"释放物理页"路径
/// 都必须先判断 `paddr == LAZY_PAGE` 跳过，避免把哨兵值当地址释放。
pub const LAZY_PAGE: PhysAddr = usize::MAX;

/// 进程私有页表：根表物理地址 + 用户映射记录表。
///
/// - `ttbr0`：本进程页表根（satp 装载值，物理地址）。
/// - `user_pages`：本进程用户空间已建立的页映射（用于 fork 完整复制、
///   exit/exec 时释放物理页）。内核恒等映射不在其中（跨进程共享）。
///
/// ★ 内存布局（1.0 内核镜像治理）：`user_pages` 的 65536×24B ≈ 1.5MB 记录
/// 数组放在**堆**（`Pages` = `Option<Box<UserPageArray>>`），`ProcPageTable`
/// 本体仅 24B——此前每个 PCB 内嵌 1.5MB 数组，`PROCESSES[24]` 在 .data 撑出
/// 37.6MB、kernel.img 达 62MB。记录表惰性分配：从未登记用户映射的进程
/// （内核线程、纯内核态任务）零堆占用；首次 `add_page/reserve_page` 落地。
/// 经 `Deref/DerefMut` 保持 `pt.user_pages[i]` 既有访问语法。
pub struct ProcPageTable {
    pub ttbr0: PhysAddr,
    pub user_pages: Pages,
    pub count: usize,
    /// 单槽位置缓存（自证式，优化轮 033）：记录表 vaddr→idx 的最近命中。
    /// 命中时现读记录复核（idx<count 且 vaddr 相等）——追加不迁移既有
    /// 记录；换位删除/截断/清表破坏复核即自然失效退回全表扫描；
    /// flags/paddr 每次从记录现读，永不陈旧。usize::MAX = 冷缓存。
    pub cache_idx: usize,
}

/// 用户映射记录数组（`[UserPage; MAX_USER_PAGES]`，约 1.5MB）的堆承载。
pub struct UserPageArray {
    pub entries: [UserPage; MAX_USER_PAGES],
}

/// 记录表句柄：堆上数组的单一所有权（`Option<Box<UserPageArray>>`）。
///
/// - `None`：从未登记用户映射（空表，读侧返回空切片）。
/// - `Some`：已分配；`count > 0` 恒蕴含 `Some`。
/// - `Option<Box<_>>` 利用 null niche，大小 8B；全零字节即合法 `None`——
///   进程槽位的 u64 清零循环天然产出合法空值。
pub struct Pages(Option<alloc::boxed::Box<UserPageArray>>);

impl Pages {
    /// 空句柄（const：供 `ProcPageTable::empty`）。
    pub const NONE: Pages = Pages(None);

    /// 分配并清零记录表。走裸 `alloc_zeroed` + null 检查而非 `Box::new_uninit`：
    /// 后者在 OOM 时触发 `handle_alloc_error`（内核默认 panic），而记录表
    /// 分配失败必须按「可回滚的 false」上报——用户 brk/mmap 在内存压力下
    /// 可以随时触达本路径。
    fn alloc_zeroed() -> Option<alloc::boxed::Box<UserPageArray>> {
        use core::alloc::Layout;
        let layout = Layout::new::<UserPageArray>();
        let raw = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut UserPageArray;
        if raw.is_null() {
            return None;
        }
        // GlobalAlloc::alloc_zeroed 默认实现保证 null 之外的区域已清零。
        Some(unsafe { alloc::boxed::Box::from_raw(raw) })
    }

    /// 惰性分配记录表（push 路径首次调用）。OOM 返回 false，调用方回滚。
    fn ensure(&mut self) -> bool {
        if self.0.is_none() {
            match Pages::alloc_zeroed() {
                Some(b) => self.0 = Some(b),
                None => return false,
            }
        }
        true
    }

    /// 归还堆上记录表（None 化）。进程槽位 u64 清零**之前**必须调用：
    /// 清零只把 Box 指针写成 null，不会释放 —— 跳过本调用 = 每次 1.5MB 堆泄漏。
    pub fn release(&mut self) {
        self.0 = None;
    }
}

impl core::ops::Deref for Pages {
    type Target = [UserPage];
    fn deref(&self) -> &[UserPage] {
        match &self.0 {
            Some(b) => &b.entries,
            None => &[],
        }
    }
}

impl core::ops::DerefMut for Pages {
    fn deref_mut(&mut self) -> &mut [UserPage] {
        match self.0.as_deref_mut() {
            Some(b) => &mut b.entries,
            // 写侧只在 `index < count`（蕴含 Some）时触达；空表写 = 调用方 bug。
            None => panic!("vm：user_pages 写访问未分配的记录表"),
        }
    }
}

impl ProcPageTable {
    /// 空页表（无根表，无用户映射；记录表不占堆）。
    pub const fn empty() -> Self {
        ProcPageTable {
            ttbr0: 0,
            cache_idx: usize::MAX,
        user_pages: Pages::NONE,
            count: 0,
        }
    }

    /// 新建页表：分配清零根表并继承内核恒等映射。
    /// 根表（单页）分配失败直接 panic——进程创建路径，非用户可直接触发的
    /// OOM 增长点（中间级页表/任务栈的 OOM 已改为错误上抛）。
    /// 记录表不在构造时分配（惰性，首次登记用户映射时落地）。
    pub fn new() -> Self {
        match ProcPageTable::try_new_boxed() {
            Some(b) => *b,
            None => panic!("vm：根页表物理内存不足"),
        }
    }

    /// 在堆上新建页表（`Box`），**不在栈上构造大临时对象**。
    ///
    /// `ProcPageTable` 本体现在仅 24B（记录表在堆上惰性分配），但保持本
    /// 接口与调用方不变；fork/execve 深调用链不再有任何栈上大对象。
    pub fn new_boxed() -> alloc::boxed::Box<ProcPageTable> {
        match ProcPageTable::try_new_boxed() {
            Some(b) => b,
            None => panic!("vm：根页表物理内存不足"),
        }
    }

    /// `new_boxed` 的可失败版本：根表分配失败返回 `None` 而非 panic
    /// （用户进程可通过 brk/文件 mmap 耗尽物理内存后 fork 触发 OOM，
    /// fork/elf 路径必须能干净失败而不是挂死整机）。
    pub fn try_new_boxed() -> Option<alloc::boxed::Box<ProcPageTable>> {
        use core::ptr;
        let mut b = alloc::boxed::Box::new_uninit();
        unsafe {
            let p: *mut ProcPageTable = b.as_mut_ptr();
            let root = new_table()?;
            ptr::write(&mut (*p).ttbr0, root);
            ptr::write(&mut (*p).user_pages, Pages::NONE);
            ptr::write(&mut (*p).count, 0usize);
            inherit_kernel_mapping((*p).ttbr0);
            // 安全性：所有字段均已初始化。
            Some(b.assume_init())
        }
    }

    /// 建立 `vaddr → paddr` 的用户页映射并记录，返回是否成功。
    ///
    /// 失败（地址越界、记录表已满或记录表堆分配 OOM）时**不建立映射**并返回
    /// `false`——若只建映射不记录，会导致：该页 exit/exec 永不释放（泄漏）、
    /// fork 父子地址空间不一致（子进程缺页）、回滚逻辑释放仍被映射的旧页
    /// （use-after-free）。拒绝映射到内核恒等映射区（`vaddr >= USER_VA_TOP`），
    /// 防止改写共享内核页表。
    pub fn add_page(&mut self, vaddr: VirtAddr, paddr: PhysAddr, flags: MapFlags) -> bool {
        if !user_va_range_ok(vaddr, PAGE_SIZE) || self.count >= MAX_USER_PAGES {
            return false;
        }
        // 记录表先落地（惰性）：映射建立后记录堆分配失败 = 有映射无记录的
        // 不一致状态，故在 map_page_in 之前分配。
        if !self.user_pages.ensure() {
            return false;
        }
        // 映射失败（中间级页表 OOM）时同样不记录，避免记录与映射不一致。
        if !map_page_in(self.ttbr0, vaddr, paddr, flags) {
            return false;
        }
        self.user_pages[self.count] = UserPage { vaddr, paddr, flags };
        self.count += 1;
        true
    }

    /// 建立用户页映射：页表用 `pte_flags`、记录用 `record_flags`，返回是否成功。
    ///
    /// 供 COW fork 使用：子进程复用父进程物理页时，**页表映射为只读**
    /// （`pte_flags = cow_pte_flags(record_flags)` 使 U 模式写触发 fault），
    /// 而记录层保留原 RW 权限 + `FLAG_COW` 标记（供 `cow_break` 恢复）。
    pub fn add_page_with_pte(
        &mut self,
        vaddr: VirtAddr,
        paddr: PhysAddr,
        record_flags: MapFlags,
        pte_flags: MapFlags,
    ) -> bool {
        if !user_va_range_ok(vaddr, PAGE_SIZE) || self.count >= MAX_USER_PAGES {
            return false;
        }
        if !self.user_pages.ensure() {
            return false;
        }
        // 映射失败（中间级页表 OOM）时不记录。
        if !map_page_in(self.ttbr0, vaddr, paddr, pte_flags) {
            return false;
        }
        self.user_pages[self.count] = UserPage { vaddr, paddr, flags: record_flags };
        self.count += 1;
        true
    }

    /// 查找 vaddr 的既有登记（非 LAZY，已落地映射），返回 (下标, 记录)。
    ///
    /// ELF 装载器段边界共享页检测用：LLD 未页对齐的数据段可落进前段页，
    /// 二次 add_page 会覆盖 PTE 丢执行位并产生同 VA 双记录。
    pub fn find_mapped_page(&self, vaddr: VirtAddr) -> Option<(usize, UserPage)> {
        for i in 0..self.count {
            let r = self.user_pages[i];
            if r.vaddr == vaddr && r.paddr != LAZY_PAGE {
                return Some((i, r));
            }
        }
        None
    }

    /// 原位更新第 idx 条记录的权限（配合 find_mapped_page 的共享页并入）。
    pub fn set_page_flags(&mut self, idx: usize, flags: MapFlags) {
        if idx < self.count {
            self.user_pages[idx].flags = flags;
        }
    }

    /// 预留 `vaddr` 一页（按需分页）：仅记录地址空间占用，**不分配物理页、
    /// 不建页表映射**，记录 `paddr = LAZY_PAGE` 作标记。
    ///
    /// 首次访问该页时由内核缺页处理器落地。失败（地址越界、记录表满
    /// 或记录表堆分配 OOM）时返回 `false`，调用方须回滚已预留的页。
    pub fn reserve_page(&mut self, vaddr: VirtAddr, flags: MapFlags) -> bool {
        if !user_va_range_ok(vaddr, PAGE_SIZE) || self.count >= MAX_USER_PAGES {
            return false;
        }
        if !self.user_pages.ensure() {
            return false;
        }
        self.user_pages[self.count] = UserPage { vaddr, paddr: LAZY_PAGE, flags };
        self.count += 1;
        true
    }

    /// 释放全部用户数据页（归还物理内存），保留根表与内核恒等映射。
    ///
    /// LAZY 预留页（`paddr == LAZY_PAGE`）无物理页可释放，仅清除记录。
    /// 共享页（只读共享池 `AP_USER_RO` 或 COW `FLAG_COW`）引用计数 -1，
    /// 归零才真正归还物理页；私有 RW 数据页直接释放。
    pub fn free_user_pages(&mut self) {
        // 去重：同一 VA 以相同物理页重复登记的记录（同 VA 多次 add_page）
        // 只允许释放一次，否则 free_page 二次归还同一页会破坏空闲链。
        let duplicates = page_records::duplicate_indices(self.count, |i| {
            let p = self.user_pages[i];
            if p.paddr == 0 || p.paddr == LAZY_PAGE { None } else { Some((p.vaddr, p.paddr)) }
        });
        if let Ok(indices) = duplicates {
            for i in indices { self.user_pages[i].paddr = 0; }
        } else {
            // OOM fallback keeps the original allocation-free release semantics.
            for i in 0..self.count {
                let p = self.user_pages[i];
                if p.paddr == 0 || p.paddr == LAZY_PAGE {
                    continue;
                }
                for j in 0..i {
                    let q = self.user_pages[j];
                    if q.vaddr == p.vaddr && q.paddr == p.paddr {
                        self.user_pages[i].paddr = 0;
                        break;
                    }
                }
            }
        }
        for i in 0..self.count {
            let p = self.user_pages[i];
            if p.paddr != 0 && p.paddr != LAZY_PAGE {
                if is_user_ro(p.flags) || is_cow(p.flags) {
                    // 共享页（只读共享池 / COW）：引用计数 -1，归零才归还
                    if allocator::unshare_page(p.paddr) {
                        allocator::free_page(p.paddr);
                    }
                } else {
                    // 数据页：进程私有，直接释放
                    allocator::free_page(p.paddr);
                }
            }
            self.user_pages[i] = EMPTY_USER_PAGE;
        }
        self.count = 0;
    }
}

/// 一级页表（内核根表，satp 指向此处）
static mut L1: PageTable = PageTable { entries: [0; 512] };
/// 二级页表（RAM 区）
static mut L2: PageTable = PageTable { entries: [0; 512] };
/// 三级页表（内核前 2MB 精细映射）
static mut L3: PageTable = PageTable { entries: [0; 512] };

/// 返回内核根表物理地址（4KB 对齐）。
pub fn kernel_root_addr() -> usize {
    addr_of!(L1) as usize
}

// ==================== 描述符构造 ====================

/// 构造下一级表描述符（指向下一级页表；非叶：仅 V，R=W=X=0）。
fn table_descriptor(next: usize) -> u64 {
    addr_to_ppn(next) | VALID
}

/// 构造 1GB 大页描述符（L1 使用），`flags` 为 `MapFlags` 组合（含 AP→R/W/U/X）。
/// 1GB 叶只保留 PPN[2]（bits[53:28]）：(addr>>30)<<28。
fn block_1g(addr: usize, flags: MapFlags) -> u64 {
    (((addr as u64) >> 30) << 28) | flags | VALID
}

/// 构造 2MB 大页描述符（L2 使用），`flags` 为 `MapFlags` 组合。
/// 2MB 叶保留 PPN[2:1]（bits[53:19]）：(addr>>21)<<19。
fn block_2m(addr: usize, flags: MapFlags) -> u64 {
    (((addr as u64) >> 21) << 19) | flags | VALID
}

/// 构造 4KB 页描述符（L3 使用），`flags` 为 `MapFlags` 组合。
fn page_descriptor(paddr: usize, flags: MapFlags) -> u64 {
    if flags & FLAG_NO_ACCESS != 0 { return 0; }
    addr_to_ppn(paddr) | flags | VALID
}

// ==================== 页表页分配 ====================

/// 分配并清零一页作为页表，返回其物理地址。
/// 物理内存耗尽时返回 `None`（调用方把 OOM 作为错误上抛，而非 panic——
/// 此路径可被用户 mmap/COW/缺页直接触发）。
fn alloc_zero_table() -> Option<usize> {
    let p = allocator::alloc_page()?;
    let t = p as *mut u64;
    unsafe {
        for i in 0..512 {
            *t.add(i) = 0;
        }
    }
    Some(p)
}

/// 分配一个清零的页表根（供用户地址空间等使用）。
/// 物理内存耗尽返回 `None`。
pub fn new_table() -> Option<usize> {
    alloc_zero_table()
}

/// 把内核根表的现有映射（全部 512 个 L1 项）复制到 `root`，使其继承内核恒等映射。
///
/// 用于用户页表：Sv39 单一地址空间（无 TTBR0/TTBR1 之分），S 模式翻译
/// 同样使用 `satp` 指向的表，因此用户页表必须同时包含内核恒等映射
/// （否则陷入内核后处理代码不可达）。
pub fn inherit_kernel_mapping(root: usize) {
    let dst = root as *mut PageTable;
    unsafe {
        for i in 0..512 {
            (*dst).entries[i] = L1.entries[i];
        }
    }
}

// ==================== 页表漫游辅助 ====================

/// 确保 L1 项指向一个有效的 L2 表，返回 L2 表物理地址。
///
/// 若该 L1 项为 1GB 大页，则将其拆分为 L2 表（512 个 2MB 大页），保留原属性。
/// 分配失败返回 `None`（调用方将 OOM 上抛）。
unsafe fn ensure_l2(l1_entry: &mut u64) -> Option<usize> {
    if *l1_entry & VALID == 0 {
        // 无效项：分配新的 L2 表
        let t = alloc_zero_table()?;
        *l1_entry = table_descriptor(t);
        Some(t)
    } else if *l1_entry & LEAF_MASK == 0 {
        // 已指向下一级表
        Some(pte_to_addr(*l1_entry))
    } else {
        // 1GB 大页 → 拆分为 512 个 2MB 大页
        let desc = *l1_entry;
        let base = (desc >> 28) << 30;
        let low = desc & LOW_ATTR_MASK;
        let t = alloc_zero_table()?;
        let p = t as *mut PageTable;
        for i in 0..512usize {
            let a = base + (i as u64) * (L2_BLOCK as u64);
            (*p).entries[i] = (((a) >> 21) << 19) | VALID | low;
        }
        *l1_entry = table_descriptor(t);
        // 大页 → 表拆分必须在 TLB 中体现：该 1GB 范围此前若按大页被访问过，
        // 陈旧大页项仍会命中（大页项不细分属性）。对全 VA 失效（拆分是罕见
        // 粗粒度操作，全冲刷成本可接受），随后 barrier 保证观测顺序。
        asm!("fence rw, rw");
        asm!("sfence.vma");
        Some(t)
    }
}

/// 确保 L2 项指向一个有效的 L3 表，返回 L3 表物理地址。
///
/// 若该 L2 项为 2MB 大页，则将其拆分为 L3 表（512 个 4KB 页），保留原属性。
/// 分配失败返回 `None`（调用方将 OOM 上抛）。
unsafe fn ensure_l3(l2_entry: &mut u64) -> Option<usize> {
    if *l2_entry & VALID == 0 {
        let t = alloc_zero_table()?;
        *l2_entry = table_descriptor(t);
        Some(t)
    } else if *l2_entry & LEAF_MASK == 0 {
        Some(pte_to_addr(*l2_entry))
    } else {
        // 2MB 大页 → 拆分为 512 个 4KB 页
        let desc = *l2_entry;
        let base = (desc >> 19) << 21;
        let low = desc & LOW_ATTR_MASK;
        let t = alloc_zero_table()?;
        let p = t as *mut PageTable;
        for i in 0..512usize {
            let a = base + (i as u64) * (PAGE_SIZE as u64);
            (*p).entries[i] = addr_to_ppn(a as usize) | VALID | low;
        }
        *l2_entry = table_descriptor(t);
        // 大页 → 表拆分需让 TLB 失效：该 2MB 范围此前若按大页被访问过，
        // 陈旧大页项会掩盖新表的属性（如把用户页当成内核 only 大页）。
        // 对全 VA 失效（拆分是粗粒度罕见操作，全冲刷成本可接受）。
        asm!("fence rw, rw");
        asm!("sfence.vma");
        Some(t)
    }
}

// ==================== 映射 / 取消映射 ====================

/// 在指定根表中建立 `vaddr → paddr` 的 4KB 页映射。
///
/// 按需分配中间级页表，必要时拆分大页描述符；覆盖已有映射时做 TLB 失效，
/// 防止陈旧 TLB 项命中（重映射场景）。返回是否成功：中间级页表分配失败
/// （物理内存耗尽）返回 `false` 且**不建立映射**，调用方应把 OOM 上抛
/// （此前直接 panic，用户 mmap/COW/缺页可打挂内核）。
pub fn map_page_in(root: usize, vaddr: usize, paddr: usize, flags: MapFlags) -> bool {
    let l1_idx = (vaddr >> 30) & 0x1FF;
    let l2_idx = (vaddr >> 21) & 0x1FF;
    let l3_idx = (vaddr >> 12) & 0x1FF;

    unsafe {
        let root_ptr = root as *mut PageTable;
        let Some(l2) = ensure_l2(&mut (*root_ptr).entries[l1_idx]) else { return false };
        let l2_ptr = l2 as *mut PageTable;
        let Some(l3) = ensure_l3(&mut (*l2_ptr).entries[l2_idx]) else { return false };
        let l3_ptr = l3 as *mut PageTable;
        (*l3_ptr).entries[l3_idx] = page_descriptor(paddr, flags);
        if vaddr == 0x400000 {
            static mut DUMPED2: bool = false;
            unsafe {
                if !DUMPED2 {
                    DUMPED2 = true;
                    crate::uart_send_str("[map-diag] l2=");
                    crate::print_hex(l2 as u64);
                    crate::uart_send_str(" l3=");
                    crate::print_hex(l3 as u64);
                    crate::uart_send_str(" l1[0]after=");
                    crate::print_hex(*((root as *const u64)));
                    crate::uart_send_str(" l2[0]after=");
                    crate::print_hex(*(l2 as *const u64));
                    crate::uart_send_str(" l3e=");
                    crate::print_hex((*l3_ptr).entries[l3_idx]);
                    crate::uart_send_str("\n");
                }
            }
        }
        // 确保页表写入对页表遍历可见
        asm!("fence rw, rw");
        // RISC-V may cache invalid PTEs too (e.g. PROT_NONE restoration).
        asm!("sfence.vma {}, zero", in(reg) vaddr as u64);
    }
    true
}

/// 在内核根表（satp 当前指向）中建立 `vaddr → paddr` 映射。返回是否成功。
pub fn map_page(vaddr: usize, paddr: usize, flags: MapFlags) -> bool {
    map_page_in(kernel_root_addr(), vaddr, paddr, flags)
}

/// 从指定根表翻译用户虚拟地址 → 物理地址（诊断用；未映射返回 0）。
pub fn user_va_to_pa(root: usize, vaddr: usize) -> usize {
    if root == 0 {
        return 0;
    }
    let l1_idx = (vaddr >> 30) & 0x1FF;
    let l2_idx = (vaddr >> 21) & 0x1FF;
    let l3_idx = (vaddr >> 12) & 0x1FF;
    unsafe {
        let root_ptr = root as *const PageTable;
        let l1 = (*root_ptr).entries[l1_idx];
        // L1/L2 必须是表（LEAF_MASK==0）：叶 = 大页，此函数只认 4KB 页
        if l1 & VALID == 0 || l1 & LEAF_MASK != 0 {
            return 0;
        }
        let l2_ptr = pte_to_addr(l1) as *const PageTable;
        let l2 = (*l2_ptr).entries[l2_idx];
        if l2 & VALID == 0 || l2 & LEAF_MASK != 0 {
            return 0;
        }
        let l3_ptr = pte_to_addr(l2) as *const PageTable;
        let l3 = (*l3_ptr).entries[l3_idx];
        // L3 必须是有效叶
        if l3 & VALID == 0 || l3 & LEAF_MASK == 0 {
            return 0;
        }
        (pte_to_addr(l3) | (vaddr & 0xFFF)) as usize
    }
}

/// 从指定根表中删除 `vaddr` 的映射，并使对应 TLB 项失效。
pub fn unmap_page_in(root: usize, vaddr: usize) {    let l1_idx = (vaddr >> 30) & 0x1FF;
    let l2_idx = (vaddr >> 21) & 0x1FF;
    let l3_idx = (vaddr >> 12) & 0x1FF;

    unsafe {
        let root_ptr = root as *mut PageTable;
        let l1_entry = (*root_ptr).entries[l1_idx];
        // L1 项必须是表（否则该地址不是按 4KB 页映射，无从取消）
        if l1_entry & VALID == 0 || l1_entry & LEAF_MASK != 0 {
            return;
        }
        let l2 = pte_to_addr(l1_entry);
        let l2_ptr = l2 as *mut PageTable;
        let l2_entry = (*l2_ptr).entries[l2_idx];
        if l2_entry & VALID == 0 || l2_entry & LEAF_MASK != 0 {
            return;
        }
        let l3 = pte_to_addr(l2_entry);
        let l3_ptr = l3 as *mut PageTable;
        (*l3_ptr).entries[l3_idx] = 0;

        // TLB 维护：使该虚拟地址的条目失效
        asm!("fence rw, rw");
        asm!("sfence.vma {}, zero", in(reg) vaddr as u64);
    }
}

/// 从内核根表中删除 `vaddr` 的映射。
pub fn unmap_page(vaddr: usize) {
    unmap_page_in(kernel_root_addr(), vaddr);
}

// ==================== satp / TLB 辅助 ====================

/// satp 装载值构造：MODE=8（Sv39）| PPN。
#[inline]
pub fn satp_value(root: PhysAddr) -> u64 {
    (8u64 << 60) | ((root as u64) >> 12)
}

/// 读取当前 satp 指向的根表物理地址。
///
/// # 安全
/// 需要 S 模式权限。
pub unsafe fn read_ttbr0_el1() -> u64 {
    let v: u64;
    asm!("csrr {}, satp", out(reg) v);
    (v & ((1u64 << 44) - 1)) << 12
}

/// 写 satp 切换地址空间并做完整 TLB 维护（sfence.vma 全量）。
///
/// # 安全
/// 需要 S 模式权限；base 必须为 4KB 对齐的页表物理地址，且已继承内核恒等映射。
pub unsafe fn write_ttbr0_el1(base: PhysAddr) {
    asm!(
        "csrw satp, {satp}",
        "sfence.vma",
        satp = in(reg) satp_value(base),
    );
}

/// 释放进程页表的中间级页表与根表（不含用户数据页，也不触碰共享的内核表）。
///
/// 用户地址空间整体落在 L1[0]（0x0000_0000-0x3FFF_FFFF）；该 L1 项要么仍是
/// 继承的 1GB 设备大页，要么已被用户映射拆分为私有 L2 表。L1[2]（内核 RAM
/// 恒等映射）始终共享，绝不能被释放。此函数只回收 L1[0] 派生出的 L2/L3 与
/// 根表页。
pub fn free_process_tables(root: usize) {
    if root == 0 {
        crate::uart_send_str("[fpt] root=0!\n");
        return;
    }
    unsafe {
        let l1 = root as *mut PageTable;
        let e0 = (*l1).entries[0];
        if e0 & VALID != 0 && e0 & LEAF_MASK == 0 {
            let l2 = pte_to_addr(e0);
            let l2p = l2 as *mut PageTable;
            for i in 0..512 {
                let e = (*l2p).entries[i];
                // 表描述符（L3）→ 回收；大页描述符（设备/内存）→ 跳过
                if e & VALID != 0 && e & LEAF_MASK == 0 {
                    let l3 = pte_to_addr(e);
                    if l3 == 0 {
                        crate::uart_send_str("[fpt] L3 表项为零，i=");
                        crate::print_dec(i as u64);
                        crate::uart_send_str(" l1e0=");
                        crate::print_hex(e0);
                        crate::uart_send_str("\n");
                    }
                    allocator::free_page(l3);
                }
            }
            allocator::free_page(l2);
        }
        allocator::free_page(root);
    }
}

// ==================== 恒等映射构造 ====================

/// 建立内核恒等映射（低 1GB 设备区 + RAM 0x80000000 起全量）。
///
/// `text_start`/`data_start` 由链接脚本导出，用于把内核前 2MB 细分为
/// 代码（RX）/数据（RW）等不同权限的 4KB 页。
///
/// 布局（Sv39，根表 L1）：
///   - L1[0]：低 1GB（PLIC 0x0C000000、UART/virtio/fw_cfg 0x10000000+、
///     PCIe ECAM 0x30000000）→ 1GB 设备大页（S RW、不可执行）
///   - L1[2]：RAM 1GB（0x80000000-0xBFFFFFFF）指向 L2
///     - L2[0]：OpenSBI 保护区（0x80000000-0x80200000）→ 2MB RW 大页
///       （内核不访问，仅为 DTB 兜底可见性）
///     - L2[1]：内核镜像前 2MB → L3（精细权限，含 DTB 兜底区）
///     - L2[2..511]：其余 RAM → 2MB RW 大页（数据/堆/ramdisk，不可执行）
pub fn build_identity_map(text_start: usize, data_start: usize) {
    unsafe {
        // ---- L1 根表 ----
        // entry0：低 1GB 设备区 → 1GB 大页（S 读写、不可执行：W^X 强化）
        L1.entries[0] = block_1g(0x0000_0000, AF | AP_EL1_RW);
        // entry2：RAM 1GB（0x80000000）指向 L2
        L1.entries[2] = table_descriptor(addr_of!(L2) as usize);

        // ---- L2 表 ----
        // entry0：OpenSBI 保护区 → 2MB RW 大页（通常不触达，兜底映射）
        L2.entries[0] = block_2m(RAM_START, AF | AP_EL1_RW);
        // entry1：内核镜像前 2MB 指向 L3（精细权限；0x80200000-0x80400000）
        L2.entries[1] = table_descriptor(addr_of!(L3) as usize);
        // entry2..511：其余 RAM 映射为 2MB RW 大页（加不可执行：堆/页表/
        // ramdisk 等数据区不可执行，防止在数据区执行代码的提权路径）
        for i in 2..512 {
            let a = RAM_START + i * L2_BLOCK;
            L2.entries[i] = block_2m(a, AF | AP_EL1_RW);
        }

        // ---- L3 表（内核前 2MB 按 4KB 页精细映射） ----
        for i in 0..512 {
            let a = RAM_START + L2_BLOCK + i * PAGE_SIZE;
            let flags = if a < text_start {
                AF | AP_EL1_RW // text 之前（对齐缝）：RW
            } else if a < data_start {
                AF | AP_EL1_RO | X // 代码 + 只读数据：RX（可执行）
            } else {
                AF | AP_EL1_RW // 数据 + BSS + bootstack：RW（不可执行）
            };
            L3.entries[i] = page_descriptor(a, flags);
        }
    }
}
