//! tmpfs：内存文件系统（挂载于 /tmp）。
//!
//! 复用既有基础设施：`blkdev::ramdisk_create` 分配一块连续物理内存作为
//! 块设备，`fs_simple::mkfs` 就地格式化为 SFS，`vfs::mount` 挂载到 /tmp。
//! 由于底层是 RAM，读写即内存操作；文件/目录的生命周期随系统运行（不持久化）。

use crate::blkdev;
use crate::fs_simple;
use crate::vfs;

/// tmpfs 块设备主号（ramdisk=1，磁盘=2，tmpfs=3）。
pub const TMPFS_MAJOR: u32 = 3;
/// tmpfs 容量（块数，每块 4KB；256KB 足够 /tmp 临时文件）。
pub const TMPFS_BLOCKS: usize = 64;

/// 初始化 tmpfs：创建内存块设备 → mkfs → 挂载到 /tmp。
/// 返回 0 成功；失败 -1（不致命，/tmp 退化为 rootfs 目录）。
pub fn init() -> i32 {
    if blkdev::ramdisk_create("tmpfs", TMPFS_MAJOR, TMPFS_BLOCKS) != 0 {
        return -1;
    }
    if fs_simple::mkfs(TMPFS_MAJOR) != 0 {
        return -1;
    }
    if vfs::mount(TMPFS_MAJOR, "/tmp") != 0 {
        return -1;
    }
    crate::uart_send_str("[fs] tmpfs mounted at /tmp (");
    crate::print_dec((TMPFS_BLOCKS * 4) as u64);
    crate::uart_send_str("KB)\n");
    0
}
