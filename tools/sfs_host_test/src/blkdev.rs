//! 块设备桩：RAM 磁盘 + 掉电故障注入。
//!
//! 与内核 `kunxcore/src/blkdev.rs` 的接口签名保持一致（fs_simple 只用到
//! BLK_SIZE / get_blkdev / blk_read / blk_write / blk_flush）。
//!
//! 故障注入：
//! - `FAULT_MODE=Drop`：第 `FAULT_AT` 次写之后的写全部失败（设备断电，
//!   已完成的写保留）。
//! - `FAULT_MODE=Tear`：第 `FAULT_AT` 次写只写半个块（撕裂写），之后写失败。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

pub const BLK_SIZE: usize = 4096;
pub const SECTOR_SIZE: usize = 512;
pub const SECTORS_PER_BLOCK: usize = BLK_SIZE / SECTOR_SIZE;

pub struct BlkOps {
    pub num_sectors: usize,
}
pub struct BlkDev {
    pub ops: BlkOps,
}

static DISK: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static DISK_SECTORS: AtomicUsize = AtomicUsize::new(0);

/// 全局写计数（含故障注入前）。
static WRITE_COUNT: AtomicUsize = AtomicUsize::new(0);
/// 故障模式：0=无，1=Drop，2=Tear。
static FAULT_MODE: AtomicUsize = AtomicUsize::new(0);
/// 触发故障的第几次写。
static FAULT_AT: AtomicUsize = AtomicUsize::new(usize::MAX);

pub fn set_disk(bytes: Vec<u8>) {
    *DISK.lock().unwrap() = bytes;
    DISK_SECTORS.store(DISK.lock().unwrap().len() / SECTOR_SIZE, Ordering::SeqCst);
}

pub fn disk_bytes() -> Vec<u8> {
    DISK.lock().unwrap().clone()
}

pub fn set_fault(mode: usize, at: usize) {
    FAULT_MODE.store(mode, Ordering::SeqCst);
    FAULT_AT.store(at, Ordering::SeqCst);
    WRITE_COUNT.store(0, Ordering::SeqCst);
}

pub fn clear_fault() {
    set_fault(0, usize::MAX);
}

pub fn write_count() -> usize {
    WRITE_COUNT.load(Ordering::SeqCst)
}

pub fn get_blkdev(_major: u32) -> Option<BlkDev> {
    Some(BlkDev {
        ops: BlkOps {
            num_sectors: DISK_SECTORS.load(Ordering::SeqCst),
        },
    })
}

pub fn blk_read(major: u32, block: usize, buf: &mut [u8]) -> i32 {
    let _ = major;
    let disk = DISK.lock().unwrap();
    let start = block * BLK_SIZE;
    if start + BLK_SIZE > disk.len() || buf.len() < BLK_SIZE {
        return -1;
    }
    buf[..BLK_SIZE].copy_from_slice(&disk[start..start + BLK_SIZE]);
    BLK_SIZE as i32
}

pub fn set_trace(enable: bool) {
    TRACE.store(enable, Ordering::SeqCst);
    WRITE_LOG.with(|l| l.borrow_mut().clear());
}

pub fn trace_len() -> usize {
    WRITE_LOG.with(|l| l.borrow().len())
}

pub fn trace_block(i: usize) -> Option<(usize, bool)> {
    WRITE_LOG.with(|l| l.borrow().get(i).copied())
}

thread_local! {
    static WRITE_LOG: std::cell::RefCell<Vec<(usize, bool)>> = std::cell::RefCell::new(Vec::new());
}
static TRACE: AtomicBool = AtomicBool::new(false);

fn log_write(block: usize, ok: bool) {
    if TRACE.load(Ordering::SeqCst) {
        WRITE_LOG.with(|l| l.borrow_mut().push((block, ok)));
    }
}

pub fn blk_write(major: u32, block: usize, buf: &[u8]) -> i32 {
    let _ = major;
    let n = WRITE_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    let mode = FAULT_MODE.load(Ordering::SeqCst);
    let at = FAULT_AT.load(Ordering::SeqCst);
    let start = block * BLK_SIZE;
    let _ = start;
    let success = if mode != 0 && n >= at {
        if mode == 1 {
            log_write(block, false);
            return -1; // Drop：本次及以后写失败
        }
        if mode == 2 {
            // Tear：写半个块
            let mut disk = DISK.lock().unwrap();
            let start = block * BLK_SIZE;
            let ok = start + BLK_SIZE <= disk.len() && buf.len() >= BLK_SIZE;
            if ok {
                disk[start..start + BLK_SIZE / 2].copy_from_slice(&buf[..BLK_SIZE / 2]);
            }
            FAULT_MODE.store(1, Ordering::SeqCst); // 之后全部失败
            log_write(block, ok);
            if ok { BLK_SIZE as i32 } else { -1 }
        } else {
            -1
        }
    } else {
        let mut disk = DISK.lock().unwrap();
        let start = block * BLK_SIZE;
        let ok = start + BLK_SIZE <= disk.len() && buf.len() >= BLK_SIZE;
        if ok {
            disk[start..start + BLK_SIZE].copy_from_slice(&buf[..BLK_SIZE]);
        }
        log_write(block, ok);
        if ok { BLK_SIZE as i32 } else { -1 }
    };
    success
}

pub fn blk_flush(_major: u32) -> i32 {
    0
}
