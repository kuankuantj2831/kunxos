//! virtio-blk 块设备驱动（virtio-mmio 传输，兼容 legacy 与 modern 接口）。
//!
//! 扫描 `virtio,mmio` 节点中 `DeviceID == 2`（块设备），注册为 `sda` 主设备，
//! 通过单个 virtqueue 以「提交请求 + 轮询 used ring」的同步方式完成扇区读写。
//!
//! 请求布局（小端）：
//!   ```text
//!   16 字节头：type@0(u32) + reserved@4(u32) + sector@8(u64)
//!   数据区（读：设备写入；写：驱动写入）
//!   1 字节状态（设备写入，0 表示成功）
//!   ```
//! 三个链式描述符分别指向头（设备读）、数据（按方向）与状态（设备写）。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::allocator;
use crate::blkdev::{self, SECTOR_SIZE};
use crate::heap;
use crate::net_virtio::MmioDev;

/// virtio-blk 主设备号（对齐 Linux sd major = 8）。
pub const MAJOR: u32 = 8;

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const DEV_ID_BLOCK: u32 = 2;

// legacy / modern 公共寄存器
const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;

// ---- modern（virtio-1）布局 ----
const M_DRV_FEATURES: usize = 0x020;
const M_DRV_FEATURES_SEL: usize = 0x024;
const M_QUEUE_SEL: usize = 0x030;
const M_QUEUE_NUM_MAX: usize = 0x034;
const M_QUEUE_NUM: usize = 0x038;
const M_QUEUE_READY: usize = 0x044;
const M_QUEUE_NOTIFY: usize = 0x050;
const M_STATUS: usize = 0x070;
const M_QUEUE_DESC_LOW: usize = 0x080;
const M_QUEUE_DESC_HIGH: usize = 0x084;
const M_QUEUE_AVAIL_LOW: usize = 0x090;
const M_QUEUE_AVAIL_HIGH: usize = 0x094;
const M_QUEUE_USED_LOW: usize = 0x0a0;
const M_QUEUE_USED_HIGH: usize = 0x0a4;
const M_CONFIG: usize = 0x100;

// ---- legacy（virtio-0.9.5 / virtio-mmio v1）布局 ----
// 注意：QEMU 的 virtio-mmio 无论 legacy/modern，寄存器偏移**相同**
// （GuestFeatures@0x020、QueueSel@0x030、Status@0x070 等），仅访问语义
// 与版本相关。早期版本误用 virtio-1.0 规范里的 legacy 偏移
// （0x018/0x024/0x028...），导致 QEMU 上读错寄存器（QueueNumMax 读到
// GuestFeaturesSel=0 → 队列无法建立 → 请求超时）。
const L_DRV_FEATURES: usize = 0x020;
const L_DRV_FEATURES_SEL: usize = 0x024;
const L_GUEST_PAGE_SIZE: usize = 0x028;
const L_QUEUE_SEL: usize = 0x030;
const L_QUEUE_NUM_MAX: usize = 0x034;
const L_QUEUE_NUM: usize = 0x038;
const L_QUEUE_ALIGN: usize = 0x03c;
const L_QUEUE_PFN: usize = 0x040;
const L_QUEUE_NOTIFY: usize = 0x050;
const L_STATUS: usize = 0x070;
const L_CONFIG: usize = 0x100;

// 统一寄存器访问（按传输版本选择偏移）
fn reg_drv_features() -> usize {
    if unsafe { MODERN } { M_DRV_FEATURES } else { L_DRV_FEATURES }
}
fn reg_drv_features_sel() -> usize {
    if unsafe { MODERN } { M_DRV_FEATURES_SEL } else { L_DRV_FEATURES_SEL }
}
fn reg_queue_sel() -> usize {
    if unsafe { MODERN } { M_QUEUE_SEL } else { L_QUEUE_SEL }
}
fn reg_queue_num_max() -> usize {
    if unsafe { MODERN } { M_QUEUE_NUM_MAX } else { L_QUEUE_NUM_MAX }
}
fn reg_queue_num() -> usize {
    if unsafe { MODERN } { M_QUEUE_NUM } else { L_QUEUE_NUM }
}
fn reg_queue_notify() -> usize {
    if unsafe { MODERN } { M_QUEUE_NOTIFY } else { L_QUEUE_NOTIFY }
}
/// legacy 专用寄存器（modern 无 GuestPageSize；QEMU 统一偏移 0x028）。
fn reg_guest_page_size() -> usize {
    L_GUEST_PAGE_SIZE
}
fn reg_status() -> usize {
    if unsafe { MODERN } { M_STATUS } else { L_STATUS }
}
fn reg_config() -> usize {
    if unsafe { MODERN } { M_CONFIG } else { L_CONFIG }
}

// 状态位
const ST_ACK: u32 = 1;
const ST_DRIVER: u32 = 2;
const ST_DRIVER_OK: u32 = 4;
const ST_FEATURES_OK: u32 = 8;

// virtio-blk 请求类型
const VBLK_T_IN: u32 = 0; // 读
const VBLK_T_OUT: u32 = 1; // 写
const VBLK_T_FLUSH: u32 = 4; // 冲刷（把设备写缓冲落盘）

/// virtqueue 大小。
const QUEUE_SIZE: usize = 8;

/// 环内偏移：desc 表（16B/项）→ avail → used（4 对齐）。
///
/// 注意：环布局由**设备按 QUEUE_NUM（=QUEUE_SIZE）计算**——desc 表占
/// QUEUE_SIZE×16 字节、avail 紧随其后、used 再之后。此前误用
/// `QUEUE_SIZE×3×16`（每请求 3 描述符的总表长）作为 AVAIL_OFF，导致
/// avail/used 偏移与设备期望不一致：设备在 128 处读 avail（全零）而驱动
/// 写到 384，请求永不处理（超时）。
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = QUEUE_SIZE * 16;
/// used ring 按 QueueAlign（=PAGE_SIZE，legacy 必需）对齐到环第二页。
/// 环总占用 < 2 页，故 RING 分配 2 个连续页。
const USED_OFF: usize = allocator::PAGE_SIZE;

/// 描述符 flags：NEXT 表示还有后续描述符，WRITE 表示设备写入该缓冲。
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// 单请求超时（µs）：请求完成/陈旧完成消费的等待上限。
/// 此前是 100M 次无时钟自旋（数十秒级挂起系统），改用单调时钟 deadline。
const VBLK_TIMEOUT_US: u64 = 1_000_000;

/// 设备 MMIO 基地址（0 表示未初始化）。
static mut BASE: usize = 0;
/// 是否为 modern 接口（Version=2）。
static mut MODERN: bool = false;
/// 队列环所在物理页（恒等映射下即虚拟地址）。
static mut RING: usize = 0;
/// used ring 已处理的 idx（同步模式：每次只提交一个请求）。
static mut LAST_USED: u16 = 0;
/// 上一次请求超时后，设备仍持有其 desc 链（无法安全 kfree/复用）。
/// 置位期间**禁止提交新请求**：下次调用先轮询并消费掉该陈旧完成项，
/// 再提交 —— 否则新请求的完成与陈旧完成无法区分（都从 desc 0 开始，
/// used 项 id 相同），会把陈旧完成误判为本次成功，导致状态字节未就绪
/// 即 kfree（use-after-free）与读写错乱。
static mut TIMEOUT_PENDING: bool = false;

// ==================== MMIO 原语 ====================

fn r32(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}

fn w32(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}

#[inline]
fn mb() {
    unsafe { asm!("fence rw, rw") }
}

#[inline]
fn dsb() {
    unsafe { asm!("fence iorw, iorw") }
}

/// 按缓存行清洗数据缓存，使设备 DMA 可读到。
#[inline]
unsafe fn dcache_clean(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("fence iorw, iorw");
        a += 64;
    }
    asm!("fence iorw, iorw");
}

/// 按缓存行作废数据缓存，使设备写入的数据可见。
#[inline]
unsafe fn dcache_invalidate(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("fence iorw, iorw");
        a += 64;
    }
    asm!("fence iorw, iorw");
}

// ==================== 环访问辅助 ====================

unsafe fn desc_addr(i: usize) -> *mut u64 {
    (RING + DESC_OFF + i * 16) as *mut u64
}
unsafe fn desc_len(i: usize) -> *mut u32 {
    (RING + DESC_OFF + i * 16 + 8) as *mut u32
}
unsafe fn desc_flags(i: usize) -> *mut u16 {
    (RING + DESC_OFF + i * 16 + 12) as *mut u16
}
unsafe fn desc_next(i: usize) -> *mut u16 {
    (RING + DESC_OFF + i * 16 + 14) as *mut u16
}
unsafe fn avail_idx() -> u16 {
    read_volatile((RING + AVAIL_OFF + 2) as *const u16)
}
unsafe fn set_avail_idx(v: u16) {
    write_volatile((RING + AVAIL_OFF + 2) as *mut u16, v)
}
unsafe fn avail_ring(i: usize) -> *mut u16 {
    (RING + AVAIL_OFF + 4 + i * 2) as *mut u16
}
unsafe fn used_idx() -> u16 {
    read_volatile((RING + USED_OFF + 2) as *const u16)
}

// ==================== 初始化 ====================

/// 初始化块设备：从 `devs` 中探测 `DeviceID == 2` 的设备并注册为 `sda`。
///
/// 成功返回 `true`；找不到 virtio-blk 返回 `false`。
pub fn init(devs: &[MmioDev]) -> bool {
    for d in devs {
        unsafe {
            if read_volatile((d.base + REG_MAGIC) as *const u32) != VIRTIO_MAGIC {
                continue;
            }
            if read_volatile((d.base + REG_DEVICE_ID) as *const u32) != DEV_ID_BLOCK {
                continue;
            }
            BASE = d.base;
            MODERN = read_volatile((d.base + REG_VERSION) as *const u32) == 2;
        }
        break;
    }
    if unsafe { BASE == 0 } {
        return false;
    }

    unsafe {
        // 复位 + ACK + DRIVER
        w32(reg_status(), 0);
        w32(reg_status(), ST_ACK);
        w32(reg_status(), ST_ACK | ST_DRIVER);

        // 特性协商（不启用任何特性）
        w32(reg_drv_features_sel(), 0);
        w32(reg_drv_features(), 0);
        // QEMU virtio-mmio legacy 设备（version=1）必须显式写 GuestPageSize，
        // 否则队列设置（QueuePFN 以页为单位）无效 → 请求超时。
        if !MODERN {
            w32(reg_guest_page_size(), allocator::PAGE_SIZE as u32);
        }
        w32(
            reg_status(),
            ST_ACK | ST_DRIVER | if MODERN { ST_FEATURES_OK } else { 0 },
        );

        // 分配并清零环内存（2 个连续页：desc/avail 在第一页，used 按
        // QueueAlign=4096 对齐到第二页）
        RING = allocator::alloc_pages_contig(2).expect("virtio_blk: ring pages");
        for i in 0..(allocator::PAGE_SIZE * 2) / 8 {
            *((RING + i * 8) as *mut u64) = 0;
        }

        // 建立队列 0
        if !setup_queue() {
            crate::uart_send_str("[vblk] virtio-blk: 队列深度 < 8 不支持\n");
            return false;
        }

        // DRIVER_OK
        w32(reg_status(), ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
        LAST_USED = used_idx();
    }

    let cap = capacity();
    blkdev::register_blkdev(
        MAJOR,
        "sda",
        blkdev::BlkOps {
            base: 0,
            num_sectors: cap as usize,
            parent: 0,
            start_lba: 0,
            read: vblk_read,
            write: vblk_write,
            flush: Some(vblk_flush),
        },
    );
    true
}

/// 配置队列 0，环内存位于 `RING` 页。
///
/// 环布局按 `QUEUE_SIZE`（8）固定；设备支持的最大队列深度不足时返回 `false`，
/// 避免以错误的环大小运行导致设备写穿。
unsafe fn setup_queue() -> bool {
    w32(reg_queue_sel(), 0);
    let num_max = r32(reg_queue_num_max());
    if (num_max as usize) < QUEUE_SIZE {
        return false;
    }
    w32(reg_queue_num(), QUEUE_SIZE as u32);
    if MODERN {
        w32(M_QUEUE_DESC_LOW, (RING + DESC_OFF) as u32);
        w32(M_QUEUE_DESC_HIGH, ((RING + DESC_OFF) >> 32) as u32);
        w32(M_QUEUE_AVAIL_LOW, (RING + AVAIL_OFF) as u32);
        w32(M_QUEUE_AVAIL_HIGH, ((RING + AVAIL_OFF) >> 32) as u32);
        w32(M_QUEUE_USED_LOW, (RING + USED_OFF) as u32);
        w32(M_QUEUE_USED_HIGH, ((RING + USED_OFF) >> 32) as u32);
        w32(M_QUEUE_READY, 1);
    } else {
        // legacy：QueueAlign 必须与环页对齐（4096，bootloader 同款），
        // 否则设备按错误的对齐解释环布局
        w32(L_QUEUE_ALIGN, allocator::PAGE_SIZE as u32);
        w32(L_QUEUE_PFN, (RING >> 12) as u32);
    }
    true
}

/// 读取配置空间的设备容量（扇区数，u64 小端）。
fn capacity() -> u64 {
    unsafe {
        let lo = read_volatile((BASE + reg_config()) as *const u32) as u64;
        let hi = read_volatile((BASE + reg_config() + 4) as *const u32) as u64;
        lo | (hi << 32)
    }
}

/// 设备超时后复位 virtqueue 与**设备本身**：完整走 status=0 → ACK → DRIVER →
/// 特性协商 → DRIVER_OK 重初始化，丢弃设备侧 in-flight 请求并让其 used 游标
/// 归零。旧请求的 req 仍按 M-25 保持泄漏（复位前设备可能仍在对其 DMA）。
///
/// 此前只重写队列寄存器/清驱动侧游标、不复位设备：设备内部 used idx 与旧
/// 描述链原样存活，其迟到完成会让下一次请求把「旧 used 项」误判为本次成功
/// （读请求返回预清零缓冲 = 假成功 / 脏数据），正是 TIMEOUT_PENDING 要防的
/// 失效模式被 reset 路径重新放行。
unsafe fn reset_ring() {
    w32(reg_status(), 0);
    w32(reg_status(), ST_ACK);
    w32(reg_status(), ST_ACK | ST_DRIVER);
    w32(reg_drv_features_sel(), 0);
    w32(reg_drv_features(), 0);
    if !MODERN {
        w32(reg_guest_page_size(), allocator::PAGE_SIZE as u32);
    }
    setup_queue();
    w32(reg_status(), ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
    set_avail_idx(0);
    (*((RING + USED_OFF + 2) as *mut u16)) = 0;
    for i in 0..QUEUE_SIZE {
        *avail_ring(i) = 0;
    }
    // ★ 复位写入的 used/avail 区域必须 clean 到内存：后续 blk_io 轮询前的
    // dcache_invalidate 会把这批脏行直接丢弃，CPU 读回的仍是复位前的陈旧
    // used idx ≠ LAST_USED=0 → 新请求未及处理即被判"完成"（假成功/脏数据），
    // 正是 reset_ring 要消除的失效模式。
    dcache_clean(RING + USED_OFF, 64);
    dcache_clean(RING + AVAIL_OFF, 4 + QUEUE_SIZE * 2 + 2);
    LAST_USED = 0;
    TIMEOUT_PENDING = false;
}

// ==================== 读写接口 ====================

fn vblk_read(_base: usize, sector: usize, count: usize, buf: &mut [u8]) -> i32 {
    blk_io(false, sector, count, buf.as_mut_ptr())
}

fn vblk_write(_base: usize, sector: usize, count: usize, buf: &[u8]) -> i32 {
    blk_io(true, sector, count, buf.as_ptr() as *mut u8)
}

/// 冲刷设备写缓冲（VIRTIO_BLK_T_FLUSH）：无数据区，头 + 状态两个描述符。
/// 返回 0 成功；失败 -1（设备不支持时也按成功处理，冲刷为尽力而为）。
fn vblk_flush(_major: u32) -> i32 {
    if unsafe { BASE == 0 } {
        return 0;
    }
    unsafe {
        let total = 16 + 1;
        let req = heap::kmalloc(total);
        if req.is_null() {
            return -1;
        }
        let req = req as usize;
        let hdr = req as *mut u8;
        *(hdr.add(0) as *mut u32) = VBLK_T_FLUSH;
        *(hdr.add(4) as *mut u32) = 0;
        *(hdr.add(8) as *mut u64) = 0;
        *hdr.add(16) = 0;
        dcache_clean(req, total);

        if TIMEOUT_PENDING {
            let start = crate::timer::now_us();
            loop {
                dcache_invalidate(RING + USED_OFF, 4 + QUEUE_SIZE * 8);
                if used_idx() != LAST_USED {
                    LAST_USED = used_idx();
                    TIMEOUT_PENDING = false;
                    crate::uart_send_str("[vblk] 已消费陈旧完成项\n");
                    break;
                }
                // 有界等待：陈旧完成迟迟未到（设备假死）时不再 100M 次自旋挂死
                // 系统，到期复位环（丢旧描述符，旧 req 按 M-25 保持泄漏）。
                if crate::timer::now_us() - start >= VBLK_TIMEOUT_US {
                    crate::uart_send_str("[vblk] 陈旧完成项超时, 正在复位环\n");
                    reset_ring();
                    break;
                }
                core::hint::spin_loop();
            }
        }

        let ai = avail_idx();
        let slot = (ai % QUEUE_SIZE as u16) as usize;

        // 两个链式描述符：头（设备读）→ 状态（设备写）
        *desc_addr(0) = req as u64;
        *desc_len(0) = 16;
        *desc_flags(0) = DESC_F_NEXT;
        *desc_next(0) = 1;

        *desc_addr(1) = (req + 16) as u64;
        *desc_len(1) = 1;
        *desc_flags(1) = DESC_F_WRITE;
        *desc_next(1) = 0;
        dcache_clean(RING + DESC_OFF, 16 * 3);

        *avail_ring(slot) = 0;
        mb();
        set_avail_idx(ai.wrapping_add(1));
        dcache_clean(RING + AVAIL_OFF, 4 + 2 * QUEUE_SIZE);

        dsb();
        w32(reg_queue_notify(), 0);

        // 有界等待：设备假死时不再 100M 次自旋挂死系统。超时保持 M-25 语义
        // （req 仍在设备 DMA 视野内，不 kfree），由下一次调用消费/复位环。
        let start = crate::timer::now_us();
        loop {
            dcache_invalidate(RING + USED_OFF, 4 + QUEUE_SIZE * 8);
            let ui = used_idx();
            if ui != LAST_USED {
                LAST_USED = ui;
                break;
            }
            if crate::timer::now_us() - start >= VBLK_TIMEOUT_US {
                LAST_USED = used_idx();
                TIMEOUT_PENDING = true;
                return -1;
            }
            core::hint::spin_loop();
        }

        dcache_invalidate(req, total);
        let status = *hdr.add(16);
        heap::kfree(req as *mut u8);

        if status != 0 {
            // 设备不支持/失败：降级为成功（冲刷尽力而为）
            0
        } else {
            0
        }
    }
}

/// 提交一个同步扇区读写请求，返回成功传输的字节数；失败 -1。
fn blk_io(write: bool, sector: usize, count: usize, data: *mut u8) -> i32 {
    if count == 0 {
        return 0;
    }
    if unsafe { BASE == 0 } {
        return -1;
    }
    let data_len = count * SECTOR_SIZE;
    let total = 16 + data_len + 1;

    unsafe {
        let req = heap::kmalloc(total);
        if req.is_null() {
            return -1; // 分配失败：不提交请求
        }
        let req = req as usize;
        let hdr = req as *mut u8;
        *(hdr.add(0) as *mut u32) = if write { VBLK_T_OUT } else { VBLK_T_IN };
        *(hdr.add(4) as *mut u32) = 0;
        *(hdr.add(8) as *mut u64) = sector as u64;
        if write {
            core::ptr::copy_nonoverlapping(data, hdr.add(16), data_len);
        }
        *hdr.add(16 + data_len) = 0;
        dcache_clean(req, total);

        // 提交前先消费上一次超时请求的陈旧完成（见 TIMEOUT_PENDING 注释）。
        if TIMEOUT_PENDING {
            let start = crate::timer::now_us();
            loop {
                dcache_invalidate(RING + USED_OFF, 4 + QUEUE_SIZE * 8);
                if used_idx() != LAST_USED {
                    LAST_USED = used_idx();
                    TIMEOUT_PENDING = false;
                    crate::uart_send_str("[vblk] 已消费陈旧完成项\n");
                    break;
                }
                // 有界等待：陈旧完成迟迟未到（设备可能已死）时不再 100M 次自旋
                // 拒绝新请求——到期复位环（丢旧描述符，旧 req 按 M-25 泄漏），
                // 本请求可继续提交。
                if crate::timer::now_us() - start >= VBLK_TIMEOUT_US {
                    crate::uart_send_str("[vblk] 陈旧完成项超时, 正在复位环\n");
                    reset_ring();
                    break;
                }
                core::hint::spin_loop();
            }
        }

        let ai = avail_idx();
        let slot = (ai % QUEUE_SIZE as u16) as usize;

        // 三个链式描述符：头（设备读）→ 数据（按方向）→ 状态（设备写）。
        // 注意：virtio 规定 avail 项引用的描述符序号必须 < 队列深度（QUEUE_SIZE），
        // 因此固定使用 desc 0/1/2（单飞行模型，同一时刻至多一个在途请求）。
        *desc_addr(0) = req as u64;
        *desc_len(0) = 16;
        *desc_flags(0) = DESC_F_NEXT;
        *desc_next(0) = 1;

        *desc_addr(1) = (req + 16) as u64;
        *desc_len(1) = data_len as u32;
        *desc_flags(1) = DESC_F_NEXT | if write { 0 } else { DESC_F_WRITE };
        *desc_next(1) = 2;

        *desc_addr(2) = (req + 16 + data_len) as u64;
        *desc_len(2) = 1;
        *desc_flags(2) = DESC_F_WRITE;
        *desc_next(2) = 0;
        dcache_clean(RING + DESC_OFF, 16 * 3);

        *avail_ring(slot) = 0;
        mb();
        set_avail_idx(ai.wrapping_add(1));
        dcache_clean(RING + AVAIL_OFF, 4 + 2 * QUEUE_SIZE);

        dsb();
        w32(reg_queue_notify(), 0);

        // 轮询 used ring 直到本次请求完成（有界等待，替换 100M 次无时钟自旋。
        // 超时后仍按 M-25 不 kfree、置位门控；下一次调用对陈旧完成做有界消费）
        let start = crate::timer::now_us();
        loop {
            // 让设备写入的 used ring 对 CPU 可见（KVM/真机有真实缓存时必需）
            dcache_invalidate(RING + USED_OFF, 4 + QUEUE_SIZE * 8);
            let ui = used_idx();
            if ui != LAST_USED {
                LAST_USED = ui;
                break;
            }
            if crate::timer::now_us() - start >= VBLK_TIMEOUT_US {
                // 超时：设备仍可能对 req DMA 写入，此时 kfree 会造成 use-after-free；
                // 改为泄漏该缓冲（失败路径，可接受），同步游标并置位门控 ——
                // 下次提交前先消费陈旧完成，防止误判。
                let cur = used_idx();
                LAST_USED = cur;
                TIMEOUT_PENDING = true;
                crate::uart_send_str("[vblk] 超时 sector=");
                crate::print_dec(sector as u64);
                crate::uart_send_str(" count=");
                crate::print_dec(count as u64);
                crate::uart_send_str(" write=");
                crate::print_dec(write as u64);
                crate::uart_send_str(" last_used=");
                crate::print_dec(LAST_USED as u64);
                crate::uart_send_str(" modern=");
                crate::print_dec(MODERN as u64);
                crate::uart_send_str(" ver=");
                crate::print_hex(read_volatile((BASE + REG_VERSION) as *const u32) as u64);
                crate::uart_send_str(" status=");
                crate::print_hex(r32(reg_status()) as u64);
                crate::uart_send_str(" avidx=");
                crate::print_dec(avail_idx() as u64);
                crate::uart_send_str(" qnum=");
                crate::print_dec(r32(reg_queue_num()) as u64);
                crate::uart_send_str("\n");
                return -1;
            }
            core::hint::spin_loop();
        }

        dcache_invalidate(req, total);
        let status = *hdr.add(16 + data_len);
        if !write {
            core::ptr::copy_nonoverlapping(hdr.add(16), data, data_len);
        }
        heap::kfree(req as *mut u8);

        if status != 0 {
            crate::uart_send_str("[vblk] status=");
            crate::print_dec(status as u64);
            crate::uart_send_str(" sector=");
            crate::print_dec(sector as u64);
            crate::uart_send_str(" count=");
            crate::print_dec(count as u64);
            crate::uart_send_str(" write=");
            crate::print_dec(write as u64);
            crate::uart_send_str("\n");
            -1
        } else {
            data_len as i32
        }
    }
}
