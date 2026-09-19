//! virtio-input 驱动（virtio-mmio）：键盘 + 鼠标（指针）。
//!
//! QEMU 的 `virtio-keyboard-device` / `virtio-mouse-device` 把 Linux
//! `input_event` 的 type/code/value 三元组写入各自的事件队列。驱动初始化
//! 全部 virtio-input 设备（最多 [`MAX_INPUT_DEVS`] 个），把事件转发到统一的
//! `/dev/input/event0`，与 UART 触摸事件共享同一事件流，用户态可同时处理
//! 触摸、键盘与鼠标。

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

use crate::allocator;
use crate::plic;
use crate::input::{self, InputEvent};
use crate::interrupt;
use crate::net_virtio::MmioDev;

const VIRTIO_MAGIC: u32 = 0x7472_6976;
const DEV_ID_INPUT: u32 = 18;

/// 同时支持的 virtio-input 设备数（键盘 + 鼠标）。
const MAX_INPUT_DEVS: usize = 2;

const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
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
const REG_IRQ_STATUS: usize = 0x060;
const REG_IRQ_ACK: usize = 0x064;
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
const IRQ_USED: u32 = 1;

const QUEUE_SIZE: usize = 64;
const EVENT_SIZE: usize = 8;
const DESC_OFF: usize = 0;
const AVAIL_OFF: usize = QUEUE_SIZE * 16;
const USED_OFF: usize = (AVAIL_OFF + 4 + 2 * QUEUE_SIZE + 3) & !3;
const DESC_F_WRITE: u16 = 2;

/// 单个 virtio-input 设备的状态。
struct InputDev {
    base: usize,
    irq: u32,
    modern: bool,
    ring: usize,
    event_page: usize,
    used_seen: u16,
    active: bool,
}

const EMPTY_DEV: InputDev = InputDev {
    base: 0,
    irq: 0,
    modern: false,
    ring: 0,
    event_page: 0,
    used_seen: 0,
    active: false,
};

static mut DEVS: [InputDev; MAX_INPUT_DEVS] = [EMPTY_DEV; MAX_INPUT_DEVS];

#[inline]
unsafe fn r32(idx: usize, off: usize) -> u32 {
    read_volatile((DEVS[idx].base + off) as *const u32)
}

#[inline]
unsafe fn w32(idx: usize, off: usize, value: u32) {
    write_volatile((DEVS[idx].base + off) as *mut u32, value)
}

#[inline]
fn mb() {
    unsafe { asm!("fence rw, rw") }
}

#[inline]
fn dsb() {
    unsafe { asm!("fence iorw, iorw") }
}

unsafe fn dcache_clean(va: usize, len: usize) {
    let mut addr = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while addr < end {
        asm!("fence iorw, iorw");
        addr += 64;
    }
    asm!("fence iorw, iorw");
}

unsafe fn dcache_invalidate(va: usize, len: usize) {
    let mut addr = va & !63usize;
    let end = (va + len + 63) & !63usize;
    while addr < end {
        asm!("fence iorw, iorw");
        addr += 64;
    }
    asm!("fence iorw, iorw");
}

#[inline]
unsafe fn desc_addr(idx: usize, i: usize) -> *mut u64 {
    (DEVS[idx].ring + DESC_OFF + i * 16) as *mut u64
}

#[inline]
unsafe fn desc_len(idx: usize, i: usize) -> *mut u32 {
    (DEVS[idx].ring + DESC_OFF + i * 16 + 8) as *mut u32
}

#[inline]
unsafe fn desc_flags(idx: usize, i: usize) -> *mut u16 {
    (DEVS[idx].ring + DESC_OFF + i * 16 + 12) as *mut u16
}

#[inline]
unsafe fn desc_next(idx: usize, i: usize) -> *mut u16 {
    (DEVS[idx].ring + DESC_OFF + i * 16 + 14) as *mut u16
}

#[inline]
unsafe fn avail_idx(idx: usize) -> u16 {
    read_volatile((DEVS[idx].ring + AVAIL_OFF + 2) as *const u16)
}

#[inline]
unsafe fn set_avail_idx(idx: usize, value: u16) {
    write_volatile((DEVS[idx].ring + AVAIL_OFF + 2) as *mut u16, value)
}

#[inline]
unsafe fn avail_ring(idx: usize, i: usize) -> *mut u16 {
    (DEVS[idx].ring + AVAIL_OFF + 4 + i * 2) as *mut u16
}

#[inline]
unsafe fn used_idx(idx: usize) -> u16 {
    read_volatile((DEVS[idx].ring + USED_OFF + 2) as *const u16)
}

#[inline]
unsafe fn used_id(idx: usize, i: usize) -> u32 {
    read_volatile((DEVS[idx].ring + USED_OFF + 4 + i * 8) as *const u32)
}

unsafe fn setup_queue(idx: usize) {
    w32(idx, REG_QUEUE_SEL, 0);
    let size = r32(idx, REG_QUEUE_NUM_MAX).min(QUEUE_SIZE as u32);
    if size < QUEUE_SIZE as u32 {
        return;
    }
    w32(idx, REG_QUEUE_NUM, QUEUE_SIZE as u32);
    if DEVS[idx].modern {
        w32(idx, REG_QUEUE_DESC_LOW, (DEVS[idx].ring + DESC_OFF) as u32);
        w32(idx, REG_QUEUE_DESC_HIGH, ((DEVS[idx].ring + DESC_OFF) >> 32) as u32);
        w32(idx, REG_QUEUE_AVAIL_LOW, (DEVS[idx].ring + AVAIL_OFF) as u32);
        w32(idx, REG_QUEUE_AVAIL_HIGH, ((DEVS[idx].ring + AVAIL_OFF) >> 32) as u32);
        w32(idx, REG_QUEUE_USED_LOW, (DEVS[idx].ring + USED_OFF) as u32);
        w32(idx, REG_QUEUE_USED_HIGH, ((DEVS[idx].ring + USED_OFF) >> 32) as u32);
        w32(idx, REG_QUEUE_READY, 1);
    } else {
        w32(idx, REG_QUEUE_ALIGN, 4);
        w32(idx, REG_QUEUE_PFN, (DEVS[idx].ring >> 12) as u32);
    }
}

unsafe fn fill_queue(idx: usize) {
    for i in 0..QUEUE_SIZE {
        *desc_addr(idx, i) = (DEVS[idx].event_page + i * EVENT_SIZE) as u64;
        *desc_len(idx, i) = EVENT_SIZE as u32;
        *desc_flags(idx, i) = DESC_F_WRITE;
        *desc_next(idx, i) = 0;
        *avail_ring(idx, i) = i as u16;
    }
    dcache_clean(DEVS[idx].ring, allocator::PAGE_SIZE);
    dcache_invalidate(DEVS[idx].event_page, QUEUE_SIZE * EVENT_SIZE);
    mb();
    set_avail_idx(idx, QUEUE_SIZE as u16);
    dcache_clean(DEVS[idx].ring + AVAIL_OFF, 4 + 2 * QUEUE_SIZE);
    DEVS[idx].used_seen = 0;
    dsb();
    w32(idx, REG_QUEUE_NOTIFY, 0);
}

unsafe fn drain(idx: usize) {
    dcache_invalidate(DEVS[idx].ring + USED_OFF, 4 + QUEUE_SIZE * 8);
    loop {
        let current = used_idx(idx);
        if DEVS[idx].used_seen == current {
            break;
        }
        let raw_id = used_id(idx, (DEVS[idx].used_seen % QUEUE_SIZE as u16) as usize) as usize;
        // 坏 id（设备异常返回未配置的 desc 序号）也钳制到有效范围重投递，
        // 防止该 desc 槽位不再回收 → 输入队列逐步耗尽
        let id = raw_id % QUEUE_SIZE;
        let addr = DEVS[idx].event_page + id * EVENT_SIZE;
        dcache_invalidate(addr, EVENT_SIZE);
        let etype = read_volatile(addr as *const u16);
        let code = read_volatile((addr + 2) as *const u16);
        let mut value = read_volatile((addr + 4) as *const u32) as i32;
        // virtio-tablet（绝对定位）：ABS_X/ABS_Y 范围 0..0x7FFF，缩放到屏幕像素，
        // 使宿主鼠标位置与客机光标 1:1 精确对应（相对鼠标会漂移）。
        if etype == crate::input::EV_ABS {
            if code == crate::input::ABS_X || code == crate::input::ABS_Y {
                let screen = if code == crate::input::ABS_X {
                    crate::gpu_simple::width()
                } else {
                    crate::gpu_simple::height()
                };
                if screen > 0 {
                    let scaled = ((value.max(0) as u32) * screen) / 0x7FFF;
                    value = scaled.min(screen as u32 - 1) as i32;
                }
            }
        }
        // 事件时间戳：virtio-input 硬件不带时间，取内核单调时钟填充
        let us = crate::timer::now_us();
        let mut ev = InputEvent::new(etype, code, value);
        ev.tv_sec = (us / 1_000_000) as i64;
        ev.tv_usec = (us % 1_000_000) as i64;
        input::push_event(ev);

        let next = avail_idx(idx);
        *avail_ring(idx, (next % QUEUE_SIZE as u16) as usize) = id as u16;
        mb();
        set_avail_idx(idx, next.wrapping_add(1));
        dcache_clean(DEVS[idx].ring + AVAIL_OFF, 4 + 2 * QUEUE_SIZE);
        DEVS[idx].used_seen = DEVS[idx].used_seen.wrapping_add(1);
    }
    dsb();
    w32(idx, REG_QUEUE_NOTIFY, 0);
}

/// 单个设备的 IRQ 处理（确认中断 + 排空事件队列）。
fn irq_handler_dev(idx: usize) {
    unsafe {
        let status = r32(idx, REG_IRQ_STATUS);
        if status != 0 {
            w32(idx, REG_IRQ_ACK, status);
        }
        if status & IRQ_USED != 0 {
            drain(idx);
        }
    }
}

// 每个设备一个独立的 IRQ 处理函数（register_handler 需要 fn 指针，不能带参数）。
fn irq_handler_0() {
    irq_handler_dev(0);
}

fn irq_handler_1() {
    irq_handler_dev(1);
}

/// 探测并初始化 QEMU 的全部 virtio 输入设备（键盘 / 鼠标）。
pub fn init(devs: &[MmioDev]) -> bool {
    let mut count = 0usize;
    for dev in devs {
        if count >= MAX_INPUT_DEVS {
            break;
        }
        unsafe {
            if read_volatile((dev.base + REG_MAGIC) as *const u32) == VIRTIO_MAGIC
                && read_volatile((dev.base + REG_DEVICE_ID) as *const u32) == DEV_ID_INPUT
            {
                DEVS[count].base = dev.base;
                DEVS[count].irq = dev.irq;
                DEVS[count].modern =
                    read_volatile((dev.base + REG_VERSION) as *const u32) == 2;
                DEVS[count].active = true;
                count += 1;
            }
        }
    }
    if count == 0 {
        return false;
    }

    for idx in 0..count {
        unsafe {
            w32(idx, REG_STATUS, 0);
            w32(idx, REG_STATUS, ST_ACK);
            w32(idx, REG_STATUS, ST_ACK | ST_DRIVER);
            if DEVS[idx].modern {
                w32(idx, REG_DRV_FEATURES_SEL, 0);
                w32(idx, REG_DRV_FEATURES, 0);
                w32(idx, REG_STATUS, ST_ACK | ST_DRIVER | ST_FEATURES_OK);
            } else {
                w32(idx, REG_GUEST_PAGE_SIZE, allocator::PAGE_SIZE as u32);
                w32(idx, REG_DRV_FEATURES_SEL, 0);
                w32(idx, REG_DRV_FEATURES, 0);
            }

            DEVS[idx].ring = allocator::alloc_page().expect("virtio_input: ring page");
            DEVS[idx].event_page = allocator::alloc_page().expect("virtio_input: event page");
            core::ptr::write_bytes(DEVS[idx].ring as *mut u8, 0, allocator::PAGE_SIZE);
            core::ptr::write_bytes(DEVS[idx].event_page as *mut u8, 0, allocator::PAGE_SIZE);
            setup_queue(idx);
            fill_queue(idx);

            let ready = if DEVS[idx].modern {
                ST_ACK | ST_DRIVER | ST_FEATURES_OK | ST_DRIVER_OK
            } else {
                ST_ACK | ST_DRIVER | ST_DRIVER_OK
            };
            w32(idx, REG_STATUS, ready);
        }

        let irq = unsafe { DEVS[idx].irq };
        match idx {
            0 => interrupt::register_handler(irq, irq_handler_0),
            1 => interrupt::register_handler(irq, irq_handler_1),
            _ => {}
        }
        plic::plic_enable_irq(irq);
        plic::plic_set_priority(irq, 0x90);
        plic::plic_set_target(irq, 0x01);
    }
    true
}
