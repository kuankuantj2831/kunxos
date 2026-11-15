//! QEMU virtio-mmio GPU, fenced 2D resources with damage uploads.
//!
//! This is a software-rendered display backend, not a VirGL/3D accelerator.
//! Queue and DMA storage live for the entire boot, including after a timeout.
//! No caller stack is exposed to DMA. No optional GPU/virtqueue features used.

use core::arch::asm;
use core::ptr::{copy_nonoverlapping, read_volatile, write_bytes, write_volatile};
use crate::{allocator, timer};
use crate::net_virtio::MmioDev;
use crate::virtio_gpu_protocol::{self as wire, Command, Damage};

const QUEUE_SIZE: usize = 8;
const AVAIL: usize = QUEUE_SIZE * 16;
const USED: usize = allocator::PAGE_SIZE;
const RESPONSE_LEN: usize = 512;
const RESPONSE_OFF: usize = 512;

static mut BASE: usize = 0;
static mut RING: usize = 0;
static mut DMA: usize = 0;
static mut SEEN: u16 = 0;
static mut FENCE: u64 = 0;
static mut READY: bool = false;
static mut SCANOUT: u32 = 0;
static mut RESOURCE: u32 = 0;
static mut BINDING: (usize, u32, u32) = (0, 0, 0);
static mut COMMANDS: u32 = 0;
static mut FAILURES: u32 = 0;

// All queue operations are bounded and serialized with local interrupts masked.
// KunxOS currently runs its compositor on one hart (SMP is experimental).
struct IrqGuard(usize);
impl IrqGuard {
    fn new() -> Self {
        let saved: usize;
        unsafe { asm!("csrrci {}, sstatus, 2", out(reg) saved, options(nostack)); }
        Self(saved)
    }
}
impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.0 & 2 != 0 {
            unsafe { asm!("csrsi sstatus, 2", options(nostack)); }
        }
    }
}

unsafe fn r32(off: usize) -> u32 { read_volatile((BASE + off) as *const u32) }
unsafe fn w32(off: usize, value: u32) { write_volatile((BASE + off) as *mut u32, value); }
fn barrier() { unsafe { asm!("fence iorw, iorw", options(nostack)); } }

unsafe fn failed(reason: &str) -> bool {
    READY = false;
    FAILURES = FAILURES.saturating_add(1);
    w32(0x70, r32(0x70) | 128); // FAILED: never reuse a timed-out DMA buffer.
    crate::uart_send_str("[gpu] virtio-gpu failed: ");
    crate::uart_send_str(reason);
    crate::uart_send_str("\n");
    false
}

/// Returns response length only after validating the used element and fence.
unsafe fn command(mut cmd: Command, expected: u32, min_len: usize) -> bool {
    if !READY { return false; }
    FENCE = FENCE.wrapping_add(1);
    cmd.fence(FENCE);
    copy_nonoverlapping(cmd.bytes.as_ptr(), DMA as *mut u8, cmd.len);
    write_bytes((DMA + RESPONSE_OFF) as *mut u8, 0, RESPONSE_LEN);
    // Two descriptors: request (read-only), then response (device-writable).
    write_volatile(RING as *mut u64, DMA as u64);
    write_volatile((RING + 8) as *mut u32, cmd.len as u32);
    write_volatile((RING + 12) as *mut u16, 1); // NEXT
    write_volatile((RING + 14) as *mut u16, 1);
    write_volatile((RING + 16) as *mut u64, (DMA + RESPONSE_OFF) as u64);
    write_volatile((RING + 24) as *mut u32, RESPONSE_LEN as u32);
    write_volatile((RING + 28) as *mut u16, 2); // WRITE
    write_volatile((RING + 30) as *mut u16, 0);
    let idx = read_volatile((RING + AVAIL + 2) as *const u16);
    write_volatile((RING + AVAIL + 4 + (idx as usize % QUEUE_SIZE) * 2) as *mut u16, 0);
    barrier();
    write_volatile((RING + AVAIL + 2) as *mut u16, idx.wrapping_add(1));
    barrier();
    w32(0x50, 0);
    COMMANDS = COMMANDS.wrapping_add(1);
    let start = timer::read_time();
    let budget = (timer::frequency() / 10).max(1); // 100ms, bounded even with IRQs off.
    while read_volatile((RING + USED + 2) as *const u16) == SEEN {
        if timer::read_time().wrapping_sub(start) >= budget {
            return failed("control queue timeout");
        }
        core::hint::spin_loop();
    }
    barrier();
    let used_idx = read_volatile((RING + USED + 2) as *const u16);
    let used = RING + USED + 4 + (SEEN as usize % QUEUE_SIZE) * 8;
    let id = read_volatile(used as *const u32);
    let len = read_volatile((used + 4) as *const u32) as usize;
    if used_idx != SEEN.wrapping_add(1) || id != 0 || len < min_len || len > RESPONSE_LEN {
        return failed("invalid used element");
    }
    SEEN = used_idx;
    let response = DMA + RESPONSE_OFF;
    if read_volatile(response as *const u32) != expected
        || read_volatile((response + 4) as *const u32) & 1 == 0
        || read_volatile((response + 8) as *const u64) != FENCE {
        return failed("device error or invalid response fence");
    }
    let irq = r32(0x60);
    if irq != 0 { w32(0x64, irq); }
    true
}

unsafe fn no_data(cmd: Command) -> bool { command(cmd, wire::OK_NODATA, wire::HEADER_LEN) }

/// Probe virtio-mmio DeviceID 16. Both legacy and modern split rings work;
/// modern transport negotiates VIRTIO_F_VERSION_1 and checks FEATURES_OK.
pub fn init(devs: &[MmioDev]) -> bool {
    let _irq = IrqGuard::new();
    unsafe {
        for dev in devs {
            if read_volatile(dev.base as *const u32) == 0x7472_6976
                && read_volatile((dev.base + 8) as *const u32) == 16 {
                BASE = dev.base;
                break;
            }
        }
        if BASE == 0 { return false; }
        let version = r32(4);
        if version != 1 && version != 2 { return failed("unsupported MMIO version"); }
        w32(0x70, 0);
        w32(0x70, 1);
        w32(0x70, 3);
        w32(0x24, 0);
        w32(0x20, 0); // no VIRGL, EDID, blob, EVENT_IDX or indirect descriptors
        if version == 2 {
            w32(0x14, 1);
            if r32(0x10) & 1 == 0 { return failed("missing VERSION_1"); }
            w32(0x24, 1);
            w32(0x20, 1); // VIRTIO_F_VERSION_1 (bit 32)
            w32(0x70, 11);
            if r32(0x70) & 8 == 0 { return failed("FEATURES_OK rejected"); }
        } else {
            w32(0x28, allocator::PAGE_SIZE as u32);
        }
        w32(0x30, 0);
        if r32(0x34) < QUEUE_SIZE as u32 { return failed("control queue too small"); }
        let Some(pages) = allocator::alloc_pages_contig(3) else {
            return failed("DMA allocation failed");
        };
        // Permanent allocation: a late completion can never target reclaimed memory.
        RING = pages;
        DMA = pages + 2 * allocator::PAGE_SIZE;
        write_bytes(pages as *mut u8, 0, 3 * allocator::PAGE_SIZE);
        // Poll completions without asking for used-buffer interrupts.
        write_volatile((RING + AVAIL) as *mut u16, 1);
        w32(0x38, QUEUE_SIZE as u32);
        if version == 2 {
            for (off, addr) in [(0x80, RING), (0x90, RING + AVAIL), (0xa0, RING + USED)] {
                w32(off, addr as u32);
                w32(off + 4, (addr >> 32) as u32);
            }
            barrier();
            w32(0x44, 1);
        } else {
            w32(0x3c, allocator::PAGE_SIZE as u32);
            barrier();
            w32(0x40, (RING >> 12) as u32);
        }
        w32(0x70, if version == 2 { 15 } else { 7 });
        READY = true;
        if !command(Command::new(wire::GET_DISPLAY_INFO, &[]),
                wire::OK_DISPLAY_INFO, wire::DISPLAY_INFO_LEN) { return false; }
        for i in 0..16 {
            if read_volatile((DMA + RESPONSE_OFF + 24 + i * 24 + 16) as *const u32) != 0 {
                SCANOUT = i as u32;
                crate::kinfo! { crate::uart_send_str("[gpu] virtio-gpu 2D control queue ready\n"); }
                return true;
            }
        }
        failed("no enabled scanout")
    }
}

pub fn stats() -> (u32, u32) {
    let _irq = IrqGuard::new();
    unsafe { (COMMANDS, FAILURES) }
}

/// Upload one clipped bounding rectangle per frame. A mode/address change
/// creates a new resource, uploads it in full, binds it, then releases the old
/// resource. Guest backing pages and fb0 mappings do not change during resize.
pub fn present(pa: usize, width: u32, height: u32, damage: Option<Damage>) -> Option<bool> {
    let _irq = IrqGuard::new();
    unsafe {
        if !READY { return None; }
        if pa == 0 || width == 0 || height == 0 || width > 1920 || height > 1200 {
            failed("invalid backing geometry");
            return None;
        }
        let changed = BINDING != (pa, width, height);
        let full = Damage { x: 0, y: 0, w: width, h: height };
        if changed {
            let next = if RESOURCE == 1 { 2 } else { 1 };
            if !no_data(wire::create(next, width, height))
                || !no_data(wire::attach(next, pa as u64, width * height * 4))
                || !no_data(full.transfer(next, width * 4))
                || !no_data(full.scanout(SCANOUT, next))
                || !no_data(full.flush(next)) { return None; }
            if RESOURCE != 0 && !no_data(Command::new(wire::UNREF, &[RESOURCE, 0])) { return None; }
            RESOURCE = next;
            BINDING = (pa, width, height);
        } else if let Some(d) = damage {
            if !no_data(d.transfer(RESOURCE, width * 4)) || !no_data(d.flush(RESOURCE)) { return None; }
        }
        Some(changed)
    }
}
