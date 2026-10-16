//! virtio-rng 熵源驱动（virtio-mmio，DeviceID = 4）。
//!
//! 单 virtqueue：驱动投递「设备写入」描述符，设备填充随机字节，used ring
//! 完成。无特性协商（legacy 需写 GuestPageSize），同步轮询，一次取一个缓冲。
//!
//! 注册为字符设备主设备号 10，`/dev/urandom` / `/dev/random` 两个节点都指向
//! 同一个熵源（演示系统不做阻塞熵池区分）。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::allocator;
use crate::devfs;
use crate::net_virtio::MmioDev;

/// 字符设备主设备号（Linux misc major = 10，未被本项目占用）。
pub const MAJOR: u32 = 10;
pub const MINOR_URANDOM: u32 = 0;

const VIRTIO_MAGIC: u32 = 0x7472_6976;
const DEV_ID_RNG: u32 = 4;
const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;

// modern / legacy 共用偏移（QEMU virtio-mmio 两种版本偏移相同）
const REG_DRV_FEATURES: usize = 0x020;
const REG_DRV_FEATURES_SEL: usize = 0x024;
const REG_GUEST_PAGE_SIZE: usize = 0x028;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_ALIGN: usize = 0x03c;
const REG_QUEUE_PFN: usize = 0x040;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_AVAIL_LOW: usize = 0x090;
const REG_QUEUE_AVAIL_HIGH: usize = 0x094;
const REG_QUEUE_USED_LOW: usize = 0x0a0;
const REG_QUEUE_USED_HIGH: usize = 0x0a4;

const ST_ACK: u32 = 1;
const ST_DRIVER: u32 = 2;
const ST_DRIVER_OK: u32 = 4;
const ST_FEATURES_OK: u32 = 8;

const QUEUE_SIZE: usize = 8;
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = QUEUE_SIZE * 16;
const USED_OFF: usize = allocator::PAGE_SIZE;

const DESC_F_WRITE: u16 = 2;

static mut BASE: usize = 0;
static mut MODERN: bool = false;
static mut RING: usize = 0;
static mut LAST_USED: u16 = 0;
static mut READY: bool = false;

/// 单飞行互斥：[`get_random`] / [`get_random_timed`] 共用。
///
/// 此前两任务并发取熵会同时改写描述符 0 与 avail/used 游标（无任何锁），
/// 可能彼此误判完成、读到对方请求的残数据。占用失败直接返回 -1：调用方
/// （rng_read → EOF、熵池 → 回退时间混合）都有降级路径，绝不等锁。
static RING_BUSY: AtomicBool = AtomicBool::new(false);

/// [`get_random_timed`] 的 DMA 目的地：模块常驻 64 字节暂存区。
///
/// 限时放弃后设备迟到的完成只覆写这块永久有效的内存，绝不 DMA 进可能已
/// 回收的调用者栈缓冲（消除超时路径悬空描述符的栈破坏风险）。
static mut TIMED_BUF: [u8; 64] = [0; 64];

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

// ==================== 环访问 ====================

unsafe fn desc_addr(i: usize) -> *mut u64 {
    (RING + DESC_OFF + i * 16) as *mut u64
}
unsafe fn desc_len(i: usize) -> *mut u32 {
    (RING + DESC_OFF + i * 16 + 8) as *mut u32
}
unsafe fn desc_flags(i: usize) -> *mut u16 {
    (RING + DESC_OFF + i * 16 + 12) as *mut u16
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

/// 从 `devs` 中探测 `DeviceID == 4`（熵源）并建立队列；成功返回 true。
pub fn init(devs: &[MmioDev]) -> bool {
    for d in devs {
        unsafe {
            if read_volatile((d.base + REG_MAGIC) as *const u32) != VIRTIO_MAGIC {
                continue;
            }
            if read_volatile((d.base + REG_DEVICE_ID) as *const u32) != DEV_ID_RNG {
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
        w32(REG_STATUS, 0);
        w32(REG_STATUS, ST_ACK);
        w32(REG_STATUS, ST_ACK | ST_DRIVER);

        // 特性协商（不启用任何特性）
        w32(REG_DRV_FEATURES_SEL, 0);
        w32(REG_DRV_FEATURES, 0);
        if !MODERN {
            w32(REG_GUEST_PAGE_SIZE, allocator::PAGE_SIZE as u32);
        }
        w32(REG_STATUS, ST_ACK | ST_DRIVER | if MODERN { ST_FEATURES_OK } else { 0 });

        // 环内存：2 个连续页（desc/avail 第一页，used 按 QueueAlign 对齐第二页）
        // 启动期降级：环内存不足返回 false（随机源缺席，回退时间戳种子）
        let Some(ring) = allocator::alloc_pages_contig(2) else {
            crate::health::report(
                crate::health::SEV_INFO,
                "rng",
                "virtio-rng 环内存不足，硬件随机源已禁用",
                "随机数质量略降（回退时间戳种子）；重启可恢复",
            );
            return false;
        };
        RING = ring;
        for i in 0..(allocator::PAGE_SIZE * 2) / 8 {
            *((RING + i * 8) as *mut u64) = 0;
        }

        // 队列 0
        w32(REG_QUEUE_SEL, 0);
        let num_max = r32(REG_QUEUE_NUM_MAX);
        if (num_max as usize) < QUEUE_SIZE {
            crate::uart_send_str("[rng] virtio-rng：队列深度 < 8\n");
            return false;
        }
        w32(REG_QUEUE_NUM, QUEUE_SIZE as u32);
        if MODERN {
            w32(REG_QUEUE_DESC_LOW, (RING + DESC_OFF) as u32);
            w32(REG_QUEUE_DESC_HIGH, ((RING + DESC_OFF) >> 32) as u32);
            w32(REG_QUEUE_AVAIL_LOW, (RING + AVAIL_OFF) as u32);
            w32(REG_QUEUE_AVAIL_HIGH, ((RING + AVAIL_OFF) >> 32) as u32);
            w32(REG_QUEUE_USED_LOW, (RING + USED_OFF) as u32);
            w32(REG_QUEUE_USED_HIGH, ((RING + USED_OFF) >> 32) as u32);
            w32(REG_QUEUE_READY, 1);
        } else {
            w32(REG_QUEUE_ALIGN, allocator::PAGE_SIZE as u32);
            w32(REG_QUEUE_PFN, (RING >> 12) as u32);
        }

        w32(REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK);
        LAST_USED = used_idx();
        READY = true;
    }
    crate::kinfo! { crate::uart_send_str("[rng] virtio-rng 就绪\n"); }
    true
}

// ==================== 取随机数 ====================

/// 尝试占用单飞行互斥（另一任务正在操作环时返回 false）。
fn ring_acquire() -> bool {
    RING_BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
}

fn ring_release() {
    RING_BUSY.store(false, Ordering::Release);
}

/// 投递单个「设备写入」描述符并轮询 used ring。
///
/// `deadline == None`：legacy 语义（100_000_000 次自旋上限，超时打印）；
/// `deadline == Some(t)`：`time` CSR 计数到达 `t` 即静默返回 -1（不打印——
/// 熵池可能高频调用，刷屏比缺一次硬件熵更糟；描述符留环内，迟到完成由
/// 下一调用消费）。
///
/// # 安全
/// 调用方须持有 `RING_BUSY` 且已确认 `READY`；`dst` 指向的缓冲在设备完成
/// 前必须保持有效（deadline 超时路径除外——见 [`TIMED_BUF`]）。
unsafe fn get_random_inner(dst: usize, len: usize, deadline: Option<u64>) -> i32 {
    let ai = avail_idx();
    let slot = (ai % QUEUE_SIZE as u16) as usize;

    *desc_addr(0) = dst as u64;
    *desc_len(0) = len as u32;
    *desc_flags(0) = DESC_F_WRITE;
    dcache_clean(RING + DESC_OFF, 16);

    *avail_ring(slot) = 0;
    mb();
    set_avail_idx(ai.wrapping_add(1));
    dcache_clean(RING + AVAIL_OFF, 4 + 2 * QUEUE_SIZE);

    dsb();
    w32(REG_QUEUE_NOTIFY, 0);

    let mut tries = 0usize;
    loop {
        dcache_invalidate(RING + USED_OFF, 4 + QUEUE_SIZE * 8);
        if used_idx() != LAST_USED {
            LAST_USED = used_idx();
            break;
        }
        match deadline {
            Some(d) => {
                if crate::timer::read_time() >= d {
                    return -1;
                }
            }
            None => {
                tries += 1;
                if tries > 100_000_000 {
                    crate::uart_send_str("[rng] get_random 超时\n");
                    return -1;
                }
            }
        }
        core::hint::spin_loop();
    }
    dcache_invalidate(dst, len);
    len as i32
}

/// 同步从熵源取 `len` 字节写入 `buf`，返回实际字节数；失败 -1。
///
/// 单飞行模型：一次只投递一个「设备写入」描述符并轮询 used ring。
/// 并发占用（另一任务取熵中）返回 -1，`rng_read` 按 EOF 处理。
pub fn get_random(buf: &mut [u8]) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    let len = buf.len().min(4096);
    if len == 0 {
        return 0;
    }
    if !ring_acquire() {
        return -1;
    }
    let r = unsafe { get_random_inner(buf.as_mut_ptr() as usize, len, None) };
    ring_release();
    r
}

/// 限时非阻塞取熵（getrandom 熵池专用）。
///
/// 与 [`get_random`] 的差异：
/// - DMA 目的地固定为模块静态 [`TIMED_BUF`]（超时后迟到的完成不会写入
///   调用者栈缓冲）；
/// - 以 `time` CSR 设 `max_us` 微秒量级的截止时刻，设备忙/超时静默返回 -1
///   （调用方回退「时间 + pid」混合熵，绝不等设备）；
/// - 与 [`get_random`] 共享单飞行互斥，并发直接失败。
///
/// 返回值：`>0` 写入 buf 的字节数（≤ min(buf.len(), 64)）；`-1` 设备未
/// 就绪/忙/超时；`0` 请求长度为 0。
pub fn get_random_timed(buf: &mut [u8], max_us: u64) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    let want = buf.len().min(64);
    if want == 0 {
        return 0;
    }
    if !ring_acquire() {
        return -1;
    }
    let r = unsafe {
        // 限时预算（time CSR 计数）：正常平台 10MHz × 1000µs = 10000 计数；
        // freq < 1MHz 时 max(1) 防零预算（0 预算 = 立即假超时）。
        let budget = (max_us.saturating_mul(crate::timer::frequency()) / 1_000_000).max(1);
        let deadline = crate::timer::read_time().saturating_add(budget);
        let dst = core::ptr::addr_of_mut!(TIMED_BUF) as usize;
        get_random_inner(dst, want, Some(deadline))
    };
    ring_release();
    if r > 0 {
        let n = r as usize;
        unsafe {
            core::ptr::copy_nonoverlapping(core::ptr::addr_of!(TIMED_BUF) as *const u8, buf.as_mut_ptr(), n);
        }
    }
    r
}

// ==================== devfs ====================

fn rng_read(minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    if minor != MINOR_URANDOM {
        return -1;
    }
    let n = get_random(buf);
    if n < 0 {
        return 0; // 熵源暂不可用：按 EOF 处理（读 0 字节）
    }
    n
}

fn rng_write(_minor: u32, buf: &[u8]) -> i32 {
    buf.len() as i32 // 写 /dev/urandom 是合法空操作（熵池加盐语义），返回成功
}

fn rng_ioctl(_minor: u32, _request: u32, _out: &mut [u8]) -> i32 {
    -25 // ENOTTY
}

/// 探测 virtio-rng 并注册 `/dev/urandom` / `/dev/random`。
pub fn rng_boot(devs: &[MmioDev]) {
    if !init(devs) {
        return;
    }
    devfs::register_chrdev(
        MAJOR,
        "urandom",
        devfs::ChrOps {
            read: rng_read,
            write: rng_write,
            ioctl: rng_ioctl,
        },
    );
    let _ = crate::vfs::mknod("/dev/urandom", crate::vfs::FileType::Chr, MAJOR, MINOR_URANDOM);
    let _ = crate::vfs::mknod("/dev/random", crate::vfs::FileType::Chr, MAJOR, MINOR_URANDOM);
}
