//! 文件系统分派层：让 VFS 支持多种文件系统（SFS / FAT32）。
//!
//! VFS 的挂载表记录每个挂载点的块设备 major；本模块维护一张
//! `dev → 文件系统类型` 表（`vfs::mount` 探测后注册），把 VFS 的
//! 每个文件系统操作按类型分派到 `fs_simple`（SFS）或 `fat32`（FAT32）。
//! 这样 VFS 层无需关心具体文件系统，新增文件系统只需在这里加一个分支。

use crate::fat32;
use crate::fs_simple;

/// 文件系统类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsKind {
    /// SFS（简单文件系统）：ramdisk / /data 分区。
    Sfs,
    /// FAT32：boot 分区等。
    Fat32,
}

/// 类型码重导出（与 fs_simple 一致，供 VFS 使用）。
pub use crate::fs_simple::{T_BLK, T_CHR, T_DIR, T_FIFO, T_REG, T_SYM};

/// dev → 类型 表（与挂载点一一对应，容量与 VFS 挂载表一致）。
const MAX_FS: usize = 8;

static mut KINDS: [(u32, FsKind); MAX_FS] = [(0, FsKind::Sfs); MAX_FS];
static mut KINDS_USED: [bool; MAX_FS] = [false; MAX_FS];

/// 探测 `dev` 上的文件系统类型；无法识别返回 None。
pub fn probe(dev: u32) -> Option<FsKind> {
    if fs_simple::fs_probe(dev) {
        return Some(FsKind::Sfs);
    }
    if fat32::fs_probe(dev) {
        return Some(FsKind::Fat32);
    }
    None
}

/// 注册 `dev` 的文件系统类型（挂载成功后调用）。
pub fn register(dev: u32, kind: FsKind) {
    unsafe {
        // 更新已有项
        for i in 0..MAX_FS {
            if KINDS_USED[i] && KINDS[i].0 == dev {
                KINDS[i].1 = kind;
                return;
            }
        }
        // 分配新项
        for i in 0..MAX_FS {
            if !KINDS_USED[i] {
                KINDS[i] = (dev, kind);
                KINDS_USED[i] = true;
                return;
            }
        }
    }
}

/// 查询 `dev` 的文件系统类型（未注册按 SFS 处理，兼容旧代码路径）。
fn kind_of(dev: u32) -> FsKind {
    unsafe {
        for i in 0..MAX_FS {
            if KINDS_USED[i] && KINDS[i].0 == dev {
                return KINDS[i].1;
            }
        }
    }
    FsKind::Sfs
}

// ==================== 操作分派 ====================

/// 挂载：校验并初始化 `dev` 上的文件系统，返回 0；失败 -1。
pub fn mount(dev: u32) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::mount(dev),
        FsKind::Fat32 => fat32::mount(dev),
    }
}

/// 根 ino。
pub fn root_ino(dev: u32) -> u32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_root_ino(dev),
        FsKind::Fat32 => fat32::fs_root_ino(dev),
    }
}

/// 目录查找。
pub fn fs_lookup(dev: u32, dir_ino: u32, name: &str) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_lookup(dev, dir_ino, name),
        FsKind::Fat32 => fat32::fs_lookup(dev, dir_ino, name),
    }
}

/// 创建文件/设备节点。
pub fn fs_create(dev: u32, dir_ino: u32, name: &str, typ: u16, rdev: u32) -> i32 {
    crate::vfs::dcache_invalidate(); // 目录变更：dentry 缓存代际失效
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_create(dev, dir_ino, name, typ, rdev),
        FsKind::Fat32 => fat32::fs_create(dev, dir_ino, name, typ, rdev),
    }
}

/// 创建目录。
pub fn fs_mkdir(dev: u32, dir_ino: u32, name: &str) -> i32 {
    crate::vfs::dcache_invalidate(); // 目录变更：dentry 缓存代际失效
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_mkdir(dev, dir_ino, name),
        FsKind::Fat32 => fat32::fs_mkdir(dev, dir_ino, name),
    }
}

/// 创建符号链接（仅 SFS 支持；FAT32 返回 -1）。
pub fn fs_symlink(dev: u32, dir_ino: u32, name: &str, target: &str) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_symlink(dev, dir_ino, name, target),
        FsKind::Fat32 => -1,
    }
}

/// 读取符号链接目标（仅 SFS 支持；FAT32 返回 -1）。
pub fn fs_readlink(dev: u32, ino: u32, buf: &mut [u8]) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_readlink(dev, ino, buf),
        FsKind::Fat32 => -1,
    }
}

/// 读文件。
pub fn fs_read(dev: u32, ino: u32, offset: usize, buf: &mut [u8]) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_read(dev, ino, offset, buf),
        FsKind::Fat32 => fat32::fs_read(dev, ino, offset, buf),
    }
}

/// 写文件。
pub fn fs_write(dev: u32, ino: u32, offset: usize, buf: &[u8]) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_write(dev, ino, offset, buf),
        FsKind::Fat32 => fat32::fs_write(dev, ino, offset, buf),
    }
}

/// O_APPEND 原子追加（SFS：取尾与写入同临界区；FAT32：取尾+写入两步，
/// 残余竞态——FAT32 仅用于 boot 分区挂载，文件信箱不落在其上）。
pub fn fs_write_append(dev: u32, ino: u32, buf: &[u8]) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_write_append(dev, ino, buf),
        FsKind::Fat32 => {
            let off = fat32::fs_size(dev, ino) as usize;
            fat32::fs_write(dev, ino, off, buf)
        }
    }
}

/// 截断文件为 0。
pub fn fs_truncate(dev: u32, ino: u32) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_truncate(dev, ino),
        FsKind::Fat32 => fat32::fs_truncate(dev, ino),
    }
}

/// 删除文件/目录。
pub fn fs_unlink(dev: u32, dir_ino: u32, name: &str) -> i32 {
    crate::vfs::dcache_invalidate(); // 目录变更：dentry 缓存代际失效
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_unlink(dev, dir_ino, name),
        FsKind::Fat32 => fat32::fs_unlink(dev, dir_ino, name),
    }
}

/// 重命名。
pub fn fs_rename(dev: u32, old_dir: u32, old_name: &str, new_dir: u32, new_name: &str) -> i32 {
    crate::vfs::dcache_invalidate(); // 目录变更：dentry 缓存代际失效
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_rename(dev, old_dir, old_name, new_dir, new_name),
        FsKind::Fat32 => fat32::fs_rename(dev, old_dir, old_name, new_dir, new_name),
    }
}

/// 读目录。
pub fn fs_readdir(dev: u32, ino: u32, index: usize, name: &mut [u8]) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_readdir(dev, ino, index, name),
        FsKind::Fat32 => fat32::fs_readdir(dev, ino, index, name),
    }
}

/// 空闲块数。
pub fn fs_free_blocks(dev: u32) -> u32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_free_blocks(dev),
        FsKind::Fat32 => fat32::fs_free_blocks(dev),
    }
}

/// 总块数。
pub fn fs_total_blocks(dev: u32) -> u32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_total_blocks(dev),
        FsKind::Fat32 => fat32::fs_total_blocks(dev),
    }
}

/// 空闲 inode 数（FAT32 无 inode 概念，返回 None）。
pub fn fs_free_inodes(dev: u32) -> Option<u32> {
    match kind_of(dev) {
        FsKind::Sfs => Some(fs_simple::fs_free_inodes(dev)),
        FsKind::Fat32 => None,
    }
}

/// inode 总数（FAT32 返回 None）。
pub fn fs_total_inodes(dev: u32) -> Option<u32> {
    match kind_of(dev) {
        FsKind::Sfs => Some(fs_simple::fs_total_inodes(dev)),
        FsKind::Fat32 => None,
    }
}

/// 块大小（字节）：fs_total_blocks / fs_free_blocks 的计数单位。
pub fn fs_block_size(dev: u32) -> u32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_block_size(dev),
        FsKind::Fat32 => fat32::fs_block_size(dev),
    }
}

/// 类型码。
pub fn fs_type(dev: u32, ino: u32) -> u16 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_type(dev, ino),
        FsKind::Fat32 => fat32::fs_type(dev, ino),
    }
}

/// 文件大小。
pub fn fs_size(dev: u32, ino: u32) -> u64 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_size(dev, ino),
        FsKind::Fat32 => fat32::fs_size(dev, ino) as u64,
    }
}

/// 设备号。
pub fn fs_rdev(dev: u32, ino: u32) -> u32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_rdev(dev, ino),
        FsKind::Fat32 => fat32::fs_rdev(dev, ino),
    }
}

/// fsync：把 `dev` 上未落盘的元数据刷盘并冲刷设备（SFS 检查点；FAT32 no-op）。
pub fn fs_fsync(dev: u32) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::fs_checkpoint(dev),
        FsKind::Fat32 => 0,
    }
}

/// fsck 语义一致性检查（仅 SFS；FAT32 返回 0 未实现）。
pub fn fs_check(dev: u32) -> i32 {
    match kind_of(dev) {
        FsKind::Sfs => fs_simple::sfs_check(dev),
        FsKind::Fat32 => 0,
    }
}
