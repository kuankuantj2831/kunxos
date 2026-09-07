//! virtio-rng 熵源驱动（virtio-mmio，DeviceID = 4）。
//!
//! 单 virtqueue：驱动投递「设备写入」描述符，设备填充随机字节，used ring
//! 完成。无特性协商（legacy 需写 GuestPageSize），同步轮询，一次取一个缓冲。
//!
//! 注册为字符设备主设备号 10，`/dev/urandom` / `/dev/random` 两个节点都指向
//! 同一个熵源（演示系统不做阻塞熵池区分）。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

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

// ==================== MMIO 原语 ====================

fn r32(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}
fn w32(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}

#[inline]
fn mb() {
    unsafe { asm!("dmb sy") }
}
#[inline]
fn dsb() {
    unsafe { asm!("dsb sy") }
}

/// 按缓存行清洗数据缓存，使设备 DMA 可读到。
#[inline]
unsafe fn dcache_clean(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("dc cvac, {}", in(reg) a);
        a += 64;
    }
    asm!("dsb sy");
}

/// 按缓存行作废数据缓存，使设备写入的数据可见。
#[inline]
unsafe fn dcache_invalidate(va: usize, len: usize) {
    let mut a = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while a < end {
        asm!("dc ivac, {}", in(reg) a);
        a += 64;
    }
    asm!("dsb sy");
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
        RING = allocator::alloc_pages_contig(2).expect("virtio_rng: ring pages");
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

/// 同步从熵源取 `len` 字节写入 `buf`，返回实际字节数；失败 -1。
///
/// 单飞行模型：一次只投递一个「设备写入」描述符并轮询 used ring。
pub fn get_random(buf: &mut [u8]) -> i32 {
    if !unsafe { READY } {
        return -1;
    }
    let len = buf.len().min(4096);
    if len == 0 {
        return 0;
    }
    unsafe {
        let ai = avail_idx();
        let slot = (ai % QUEUE_SIZE as u16) as usize;

        *desc_addr(0) = buf.as_ptr() as u64;
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
            tries += 1;
            if tries > 100_000_000 {
                crate::uart_send_str("[rng] get_random 超时\n");
                return -1;
            }
            core::hint::spin_loop();
        }
        dcache_invalidate(buf.as_ptr() as usize, len);
        len as i32
    }
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
