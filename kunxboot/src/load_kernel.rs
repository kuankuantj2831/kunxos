//! 内核镜像加载：从 FAT32 读取 KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG 到约定地址。
//!
//! 内核为裸二进制（QEMU virt 链接于 0x40080000），直接复制即可。

use crate::fs_fat::FatVolume;
use crate::uart::{print_dec, print_hex};

/// 内核加载地址（裸二进制，链接脚本入口 0x40080000）。
pub const KERNEL_LOAD_ADDR: usize = 0x4008_0000;
/// 设备树加载地址。
pub const DTB_LOAD_ADDR: usize = 0x4400_0000;
/// ramdisk 加载地址。
pub const RAMDISK_LOAD_ADDR: usize = 0x4500_0000;

/// 各镜像的最大允许大小（防止损坏/恶意的 FAT 目录项 size 导致对任意内存的覆写）。
const KERNEL_MAX: usize = 0x0018_0000; // 1.5MB（bootloader 自身代码约在 0x40200000+）
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
        crate::print!("[boot] KERNEL.IMG too large, abort\n");
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
        crate::print!("[boot] KUNXOS.DTB too large, abort\n");
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
        crate::print!("[boot] RAMDISK.IMG too large, abort\n");
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
