//! 内核镜像加载：从 FAT32 读取 KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG 到约定地址。
//!
//! 内核为裸二进制（QEMU virt 链接于 0x40080000），直接复制即可。

use crate::fs_fat::FatVolume;
use crate::uart::{print_dec, print_hex};

/// 内核加载地址（KXSI 载荷解包后落点；裸二进制链接脚本入口 0x40080000）。
pub const KERNEL_LOAD_ADDR: usize = 0x4008_0000;
/// 设备树加载地址。
pub const DTB_LOAD_ADDR: usize = 0x4400_0000;
/// ramdisk 加载地址（0x46FF0000：0x46FF0000 + 16MB+64KB 文件上限 =
/// 0x48000000，恰好用满 128MB RAM 末尾，不越界；上方避开 bootloader
/// 0x46000000、splash 帧缓冲 0x46A00000 与握手区 0x46F00000）。
pub const RAMDISK_LOAD_ADDR: usize = 0x46FF_0000;
/// 恢复 ramdisk 加载地址（0x44800000：DTB 区（≤0x44090000）之上、bootloader
/// 0x46000000 之下的空闲带；0x44800000 + 16MB+64KB = 0x45810000，不触及
/// splash 帧缓冲 0x46A00000。原 0x4A000000 落在 128MB RAM 之外，恢复模式
/// DMA 写入未映射地址必然失败）。
pub const RECOVERY_RAMDISK_LOAD_ADDR: usize = 0x4480_0000;
/// misc 原始区起始 LBA（磁盘尾 3MB，与 tools/mkdisk.py 一致）。
pub const MISC_START_LBA: u64 = 1_042_432;
/// BCB 主 / 备扇区 LBA。
pub const BCB_PRIMARY_LBA: u64 = MISC_START_LBA;
pub const BCB_BACKUP_LBA: u64 = MISC_START_LBA + 1;
/// OTP 模拟区 LBA（8 扇区 = 4KB）。
pub const OTP_LBA: u64 = MISC_START_LBA + 2;
pub const OTP_SECTORS: usize = 8;

/// 各镜像的最大允许大小（防止损坏/恶意的 FAT 目录项 size 导致对任意内存的覆写）。
/// KERNEL_MAX 与 sec_boot::MAX_KERNEL_FILE 对齐（内核含内嵌 ramdisk 已约 58MB，
/// 受 DTB_LOAD_ADDR 下界约束最大 63.5MB）。
const KERNEL_MAX: usize = 0x03F8_0000; // 63.5MB
const DTB_MAX: usize = 0x0008_0000; // 512KB
const RAMDISK_MAX: usize = 0x0080_0000; // 8MB（此前注释误写 128MB；0x80_0000 = 8MB）

/// 加载结果：三个镜像的实际大小（供跳转前缓存维护使用）。
pub struct Loaded {
    pub kernel_size: usize,
    pub dtb_size: usize,
    pub ramdisk_size: usize,
}

/// 打印字节数（以 KB 为单位，不足 1KB 按字节打印）。
fn print_size(bytes: usize) {
    if bytes >= 1024 {
        print_dec((bytes / 1024) as u64);
        crate::print!("KB");
    } else {
        print_dec(bytes as u64);
        crate::print!("B");
    }
}

/// 从 FAT32 加载三个镜像，成功返回 `Some(Loaded)`（含各镜像大小）。
pub fn load_all(vol: &FatVolume) -> Option<Loaded> {
    // ---- 内核 ----
    let kernel = vol.find("KERNEL.IMG")?;
    crate::print!("[boot] KERNEL.IMG  ");
    print_size(kernel.size as usize);
    crate::print!(" @ 0x");
    print_hex(KERNEL_LOAD_ADDR as u64);
    crate::print!("\n");
    if kernel.size as usize > KERNEL_MAX {
        crate::print!("[boot] KERNEL.IMG 过大，中止\n");
        return None;
    }
    if !vol.read_file(&kernel, KERNEL_LOAD_ADDR as *mut u8, KERNEL_MAX) {
        return None;
    }

    // ---- 设备树 ----
    let dtb = vol.find("KUNXOS.DTB")?;
    crate::print!("[boot] KUNXOS.DTB  ");
    print_size(dtb.size as usize);
    crate::print!(" @ 0x");
    print_hex(DTB_LOAD_ADDR as u64);
    crate::print!("\n");
    if dtb.size as usize > DTB_MAX {
        crate::print!("[boot] KUNXOS.DTB 过大，中止\n");
        return None;
    }
    if !vol.read_file(&dtb, DTB_LOAD_ADDR as *mut u8, DTB_MAX) {
        return None;
    }

    // ---- ramdisk ----
    let ramdisk = vol.find("RAMDISK.IMG")?;
    crate::print!("[boot] RAMDISK.IMG ");
    print_size(ramdisk.size as usize);
    crate::print!(" @ 0x");
    print_hex(RAMDISK_LOAD_ADDR as u64);
    crate::print!("\n");
    if ramdisk.size as usize > RAMDISK_MAX {
        crate::print!("[boot] RAMDISK.IMG 过大，中止\n");
        return None;
    }
    if !vol.read_file(&ramdisk, RAMDISK_LOAD_ADDR as *mut u8, RAMDISK_MAX) {
        return None;
    }

    Some(Loaded {
        kernel_size: kernel.size as usize,
        dtb_size: dtb.size as usize,
        ramdisk_size: ramdisk.size as usize,
    })
}
