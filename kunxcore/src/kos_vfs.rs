//! KOS 对象存储内核集成（Phase 1 双跑：SFS 之外的第二个卷）。
//!
//! 职责：
//! - [`KosBlkDev`]：把内核 [`crate::blkdev`] 的一个设备适配成
//!   `kos::block::BlockDevice`（4KB 块语义直通）；
//! - 全局挂载槽 + 复用 `fs_lock` 大锁串行化（KOS 引擎要求同一设备
//!   同时只有一个活跃实例）；
//! - 系统调用面（号段 460..=466，见 `linux_compat`）：
//!   mount / read / write / mkdir / unlink / list / stat。
//!
//! 与 SFS 的关系：本阶段双写并存（DESIGN.md Phase 1），KOS 卷默认
//! 挂在第二个磁盘（`-drive file=kos.img`），路径以 `/kos/` 前缀的
//! 私有命名空间存在于 KOS 卷内部，不与 SFS 路径混用。

use crate::blkdev;
use kos::block::BlockDevice;
use kos::error::KosError;
use kos::store::{KosStore, VerifyLevel};

// ==================== 系统调用号（私有号段） ====================

pub const SYS_KOS_MOUNT: usize = 460;
pub const SYS_KOS_READ: usize = 461;
pub const SYS_KOS_WRITE: usize = 462;
pub const SYS_KOS_MKDIR: usize = 463;
pub const SYS_KOS_UNLINK: usize = 464;
pub const SYS_KOS_LIST: usize = 465;
pub const SYS_KOS_STAT: usize = 466;
pub const SYS_KOS_APPEND: usize = 467;

// ==================== 块设备适配 ====================

/// 内核块设备适配器：`major` 设备按 4KB 块读写。
pub struct KosBlkDev {
    pub major: u32,
    pub blocks: u64,
}

impl BlockDevice for KosBlkDev {
    fn block_size(&self) -> usize {
        kos::format::KOS_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        // blk_read 成功返回拷贝字节数（正数），失败返回负数。
        if blkdev::blk_read(self.major, lba as usize, buf) >= 0 {
            Ok(())
        } else {
            Err(KosError::Io(5))
        }
    }

    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        if blkdev::blk_write(self.major, lba as usize, data) >= 0 {
            Ok(())
        } else {
            Err(KosError::Io(5))
        }
    }

    fn flush(&mut self) -> Result<(), KosError> {
        if blkdev::blk_flush(self.major) >= 0 {
            Ok(())
        } else {
            Err(KosError::Io(5))
        }
    }
}

// ==================== 全局挂载槽 ====================

/// 已挂载的 KOS 卷。访问一律在 fs_lock 大锁内（与 SFS 共用一把锁，
/// 避免两套存储各自为政造成的锁序问题）。
static mut KOS_STORE: Option<KosStore<KosBlkDev>> = None;

fn with_store<T>(f: impl FnOnce(&mut KosStore<KosBlkDev>) -> T) -> Option<T> {
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 是任务身份可重入大锁，KOS_STORE 只在此临界区
    // 内访问；SMP 下与 SFS 路径互斥。
    let r = unsafe { KOS_STORE.as_mut().map(f) };
    crate::fs_simple::fs_unlock();
    r
}

/// 启动期挂载策略（P1 持久化）：
/// 1. 优先扫描 virtio-blk 磁盘（sdb..，major 9..=11；sda 留给 SFS），
///    块 0 为 KOS 超级块者挂载为**持久卷**；
/// 2. 否则回退内嵌 kosrd（RAM 卷，写入不跨重启）。
/// 返回挂载描述（打印用）。
pub fn boot_mount() -> bool {
    // 全槽扫描：SFS 磁盘不会有 KOS 超级块（magic 不同），无冲突。
    for slot in 0..crate::virtio_blk::dev_count() {
        let major = crate::virtio_blk::MAJOR + slot as u32;
        if kos_superblock_at(major) && mount(major) {
            crate::uart_send_str("[kos] 持久对象存储卷已挂载（磁盘 major=");
            let mut tmp = [0u8; 4];
            let mut v = major;
            for i in (0..4).rev() {
                tmp[i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            crate::uart_send_str(core::str::from_utf8(&tmp).unwrap_or("?"));
            crate::uart_send_str("）\n");
            return true;
        }
    }
    // 回退：内嵌 kosrd（main.rs 早期已注册为 major 254）。
    if kos_superblock_at(254) && mount(254) {
        crate::uart_send_str("[kos] 对象存储卷已挂载（内嵌 RAM 卷，写入不跨重启）\n");
        return true;
    }
    crate::uart_send_str("[kos] 未找到可挂载的 KOS 卷\n");
    false
}

/// `major` 块 0 是否为合法 KOS 超级块（magic + version + 4096 块大小）。
fn kos_superblock_at(major: u32) -> bool {
    let mut buf = [0u8; 512];
    if blkdev::blk_read_sectors(major, 0, 1, &mut buf) < 0 {
        return false;
    }
    buf[0..8] == *b"KOSV0001"
}

/// 内核内部挂载（启动期用，无 uid 检查）。成功返回 true。
pub fn mount(major: u32) -> bool {
    let mut blocks = 0u64;
    blkdev::for_each_blkdev(|m, _name, b| {
        if m == major {
            blocks = b as u64;
        }
    });
    if blocks == 0 {
        return false;
    }
    let dev = KosBlkDev { major, blocks };
    match KosStore::open(dev, VerifyLevel::HashOnly) {
        Ok(s) => {
            crate::fs_simple::fs_lock();
            // SAFETY：fs_lock 临界区内（见 with_store）。
            unsafe {
                KOS_STORE = Some(s);
            }
            crate::fs_simple::fs_unlock();
            true
        }
        Err(e) => {
            crate::uart_send_str("[kos-dbg] open err code=");
            crate::uart_send_str(match e {
                KosError::InvalidMagic { .. } => "magic",
                KosError::CrcMismatch { .. } => "crc",
                KosError::UnsupportedVersion { .. } => "ver",
                KosError::InvalidBlockSize { .. } => "bsz",
                KosError::NoValidManifest => "nomanifest",
                KosError::BlockOutOfRange { .. } => "oor",
                KosError::InvalidBlockLen { .. } => "blen",
                KosError::Io(_) => "io",
                _ => "other",
            });
            crate::uart_send_str(" blocks=");
            let mut tmp = [0u8; 16];
            let mut v = blocks;
            for i in (0..16).rev() {
                tmp[i] = b'0' + (v & 0xF) as u8;
                v >>= 4;
            }
            crate::uart_send_str(core::str::from_utf8(&tmp).unwrap_or(""));
            crate::uart_send_str("\n");
            false
        }
    }
}

/// 挂载（root 专用）：`major` 为内核块设备主号。
pub fn sys_mount(major: u32) -> i64 {
    if crate::process::current_uid() != 0 {
        return -1;
    }
    let mut blocks = 0u64;
    blkdev::for_each_blkdev(|m, _name, b| {
        if m == major {
            blocks = b as u64;
        }
    });
    if blocks == 0 {
        return -1;
    }
    let dev = KosBlkDev { major, blocks };
    match KosStore::open(dev, VerifyLevel::HashOnly) {
        Ok(s) => {
            crate::fs_simple::fs_lock();
            // SAFETY：同上，fs_lock 临界区内。
            unsafe {
                KOS_STORE = Some(s);
            }
            crate::fs_simple::fs_unlock();
            0
        }
        Err(_) => -1,
    }
}

/// 是否已挂载。
pub fn mounted() -> bool {
    with_store(|_| true).unwrap_or(false)
}

// ==================== 数据面系统调用 ====================

/// 读对象：成功返回字节数，失败 -1。路径由调用方以 C 字符串给出。
pub fn sys_read(path: &str, buf: &mut [u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.open_path(path, uid)) {
        Some(Ok(data)) => {
            let n = core::cmp::min(data.len(), buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            n as i64
        }
        _ => -1,
    }
}

/// 写对象（新建或整体替换）：caps 为权限位（0 则用缺省）；
/// `mutable` 为真时建可变记录（支持 append）。
pub fn sys_write(path: &str, data: &[u8], caps: u32, mutable: bool) -> i64 {
    let uid = crate::process::current_uid();
    let caps = if caps == 0 { kos::store::perm::DEFAULT } else { caps };
    let r = with_store(|s| {
        if mutable {
            s.write_mutable(path, data, uid, caps).map(|_| ())
        } else {
            s.write(path, data, uid, caps)
        }
    });
    match r {
        Some(Ok(())) => data.len() as i64,
        Some(Err(e)) => {
            let msg = match e {
                KosError::PermissionDenied => "perm",
                KosError::NoSpace => "nospace",
                KosError::PathNotFound => "pathnf",
                KosError::AlreadyExists => "exists",
                KosError::NotADirectory => "notdir",
                KosError::ExtentTableCorrupt => "extcorr",
                KosError::CorruptNamespace => "ns",
                _ => "other",
            };
            crate::uart_send_str("[kos] write err: ");
            crate::uart_send_str(msg);
            crate::uart_send_str("\n");
            -1
        }
        None => {
            crate::uart_send_str("[kos] write err: not-mounted\n");
            -1
        }
    }
}

pub fn sys_mkdir(path: &str, caps: u32) -> i64 {
    let uid = crate::process::current_uid();
    let caps = if caps == 0 { kos::store::perm::DIR_DEFAULT } else { caps };
    match with_store(|s| s.mkdir(path, uid, caps)) {
        Some(Ok(())) => 0,
        _ => -1,
    }
}

pub fn sys_unlink(path: &str) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.unlink(path, uid)) {
        Some(Ok(())) => 0,
        _ => -1,
    }
}

/// 列目录：文本行 `type caps uid size name\n` 写入 buf，返回字节数。
pub fn sys_list(path: &str, buf: &mut [u8]) -> i64 {
    let lines = match with_store(|s| s.list(path)) {
        Some(Ok(entries)) => {
            let mut out = alloc::string::String::new();
            use core::fmt::Write;
            for e in entries {
                let t = match e.entry_type {
                    kos::format::object::ObjectType::Dir => 'd',
                    kos::format::object::ObjectType::Symlink => 'l',
                    _ => '-',
                };
                let _ = write!(
                    &mut out,
                    "{t} {:o} {} {}\n",
                    e.capability, e.owner_uid, e.name
                );
            }
            out
        }
        _ => return -1,
    };
    let b = lines.as_bytes();
    let n = core::cmp::min(b.len(), buf.len());
    buf[..n].copy_from_slice(&b[..n]);
    n as i64
}

/// 追加写（仅可变记录）：返回新长度。
pub fn sys_append(path: &str, data: &[u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.append(path, data, uid)) {
        Some(Ok(len)) => len as i64,
        _ => -1,
    }
}

/// 卷状态：返回 (generation, objects, free_blocks, pool_blocks)。
pub fn sys_stat() -> (u64, usize, u64, u64) {
    with_store(|s| (s.generation(), s.object_count(), s.free_blocks(), s.pool_blocks()))
        .unwrap_or((0, 0, 0, 0))
}
