//! 块设备抽象：块设备注册表、统一的块读写接口、分区支持，以及 RAM 磁盘实现。
//!
//! 所有块设备统一以 512 字节扇区寻址；上层文件系统（SFS）通过 4KB 块接口
//! `blk_read` / `blk_write` 访问（每块 = 8 个扇区）。设备以 `(major, ops)` 标识：
//! - 整盘设备：`ops.parent == 0`，`ops.num_sectors` 为整盘扇区数，读写直接调用 `ops`。
//! - 分区设备：`ops.parent != 0`，读写时把扇区号加上 `ops.start_lba` 后转发父设备。
//!
//! RAM 磁盘把一段连续物理内存当作磁盘，读写即内存拷贝；virtio-blk 等真实设备
//! 则由各自的驱动注册为整盘设备。

use core::ptr;

use crate::allocator;

/// 块大小：4KB。
pub const BLK_SIZE: usize = 4096;
/// 扇区大小：512 字节。
pub const SECTOR_SIZE: usize = 512;
/// 每个 4KB 块包含的扇区数。
pub const SECTORS_PER_BLOCK: usize = BLK_SIZE / SECTOR_SIZE;

/// 块设备表容量。
const MAX_BLKDEV: usize = 8;

/// 扇区读取函数：从 `base + sector * 512` 起读取 `count` 个扇区到 `buf`，返回字节数。
pub type SectorReadFn = fn(base: usize, sector: usize, count: usize, buf: &mut [u8]) -> i32;
/// 扇区写入函数：把 `buf` 写入 `base + sector * 512` 起的 `count` 个扇区。
pub type SectorWriteFn = fn(base: usize, sector: usize, count: usize, buf: &[u8]) -> i32;

/// 块设备操作集：设备基址/句柄 + 总扇区数 + 可选分区信息 + 读写函数。
#[derive(Clone, Copy)]
pub struct BlkOps {
    /// 设备数据基地址（RAM 磁盘为物理内存基址，virtio-blk 忽略）
    pub base: usize,
    /// 总扇区数
    pub num_sectors: usize,
    /// 父设备 major（分区用，0 表示整盘）
    pub parent: u32,
    /// 分区起始扇区（LBA，分区用）
    pub start_lba: usize,
    pub read: SectorReadFn,
    pub write: SectorWriteFn,
}

/// 块设备描述。
#[derive(Clone, Copy)]
pub struct BlkDev {
    pub major: u32,
    pub name: &'static str,
    pub ops: BlkOps,
}

/// 分区描述：起始 LBA 与扇区数。
#[derive(Clone, Copy)]
pub struct Partition {
    pub start_lba: usize,
    pub sector_count: usize,
}

fn nop_read(_base: usize, _sector: usize, _count: usize, _buf: &mut [u8]) -> i32 {
    -1
}
fn nop_write(_base: usize, _sector: usize, _count: usize, _buf: &[u8]) -> i32 {
    -1
}

const EMPTY_BLKDEV: BlkDev = BlkDev {
    major: 0,
    name: "",
    ops: BlkOps {
        base: 0,
        num_sectors: 0,
        parent: 0,
        start_lba: 0,
        read: nop_read,
        write: nop_write,
    },
};

/// 块设备表（位于 .bss，name 为空表示槽位空闲）。
static mut BLKDEVS: [BlkDev; MAX_BLKDEV] = [EMPTY_BLKDEV; MAX_BLKDEV];

/// 分区名缓冲（注册分区时格式化 "sda3" 之类名字，返回 &'static str）。
static mut NAME_POOL: [[u8; 8]; MAX_BLKDEV] = [[0; 8]; MAX_BLKDEV];
static mut NAME_POOL_USED: [bool; MAX_BLKDEV] = [false; MAX_BLKDEV];

/// 注册块设备 `major`，返回 0 成功、-1 表满。
pub fn register_blkdev(major: u32, name: &'static str, ops: BlkOps) -> i32 {
    unsafe {
        for i in 0..MAX_BLKDEV {
            if BLKDEVS[i].name.is_empty() {
                BLKDEVS[i] = BlkDev { major, name, ops };
                return 0;
            }
        }
    }
    -1
}

/// 按主设备号查找块设备。
pub fn get_blkdev(major: u32) -> Option<BlkDev> {
    unsafe {
        for i in 0..MAX_BLKDEV {
            let d = BLKDEVS[i];
            if !d.name.is_empty() && d.major == major {
                return Some(d);
            }
        }
    }
    None
}

/// 读取 `major` 设备从 `sector` 起的 `count` 个扇区到 `buf`，返回字节数；失败 -1。
pub fn blk_read_sectors(major: u32, sector: usize, count: usize, buf: &mut [u8]) -> i32 {
    let dev = match get_blkdev(major) {
        Some(d) => d,
        None => return -1,
    };
    // 边界检查：防 usize 回绕绕过（sector+count 与 count*SECTOR_SIZE 用 checked 运算）
    if count == 0
        || sector.checked_add(count).map_or(true, |e| e > dev.ops.num_sectors)
        || count.checked_mul(SECTOR_SIZE).map_or(true, |n| buf.len() < n)
    {
        return -1;
    }
    if dev.ops.parent != 0 {
        let parent = match get_blkdev(dev.ops.parent) {
            Some(p) => p,
            None => return -1,
        };
        // start_lba 来自分区表，checked 加法防回绕
        let Some(psector) = dev.ops.start_lba.checked_add(sector) else { return -1 };
        if psector.checked_add(count).map_or(true, |e| e > parent.ops.num_sectors) {
            return -1;
        }
        (parent.ops.read)(parent.ops.base, psector, count, buf)
    } else {
        (dev.ops.read)(dev.ops.base, sector, count, buf)
    }
}

/// 把 `buf` 写入 `major` 设备从 `sector` 起的 `count` 个扇区，返回字节数；失败 -1。
pub fn blk_write_sectors(major: u32, sector: usize, count: usize, buf: &[u8]) -> i32 {
    let dev = match get_blkdev(major) {
        Some(d) => d,
        None => return -1,
    };
    if count == 0
        || sector.checked_add(count).map_or(true, |e| e > dev.ops.num_sectors)
        || count.checked_mul(SECTOR_SIZE).map_or(true, |n| buf.len() < n)
    {
        return -1;
    }
    if dev.ops.parent != 0 {
        let parent = match get_blkdev(dev.ops.parent) {
            Some(p) => p,
            None => return -1,
        };
        let Some(psector) = dev.ops.start_lba.checked_add(sector) else { return -1 };
        if psector.checked_add(count).map_or(true, |e| e > parent.ops.num_sectors) {
            return -1;
        }
        (parent.ops.write)(parent.ops.base, psector, count, buf)
    } else {
        (dev.ops.write)(dev.ops.base, sector, count, buf)
    }
}

/// 读取 `major` 设备的 `block` 号 4KB 块到 `buf`（调用方保证 `buf` 为整块大小）。
pub fn blk_read(major: u32, block: usize, buf: &mut [u8]) -> i32 {
    // checked 乘法：block 来自磁盘 inode（不可信），直接相乘可能回绕后
    // 通过内层边界检查，读到合法范围内的错误扇区
    let Some(sector) = block.checked_mul(SECTORS_PER_BLOCK) else { return -1 };
    blk_read_sectors(major, sector, SECTORS_PER_BLOCK, buf)
}

/// 把 `buf` 写入 `major` 设备的 `block` 号 4KB 块。
pub fn blk_write(major: u32, block: usize, buf: &[u8]) -> i32 {
    let Some(sector) = block.checked_mul(SECTORS_PER_BLOCK) else { return -1 };
    blk_write_sectors(major, sector, SECTORS_PER_BLOCK, buf)
}

/// 注册 `disk` 设备的第 `part_num` 个分区（起始 LBA `start`，扇区数 `count`）。
///
/// 返回分区的新 major（编码为 `disk * 16 + part_num`）；失败返回 -1。
pub fn blkdev_register_partition(
    disk: u32,
    part_num: u32,
    start: usize,
    count: usize,
) -> i32 {
    let parent = match get_blkdev(disk) {
        Some(d) => d,
        None => return -1,
    };
    // start 来自磁盘分区表（不可信），checked 加法防回绕
    if count == 0 || start.checked_add(count).map_or(true, |e| e > parent.ops.num_sectors) {
        return -1;
    }
    let major = disk * 16 + part_num;
    if get_blkdev(major).is_some() {
        return -1; // 已注册
    }
    let name = make_partition_name(parent.name, part_num);
    if register_blkdev(
        major,
        name,
        BlkOps {
            base: 0,
            num_sectors: count,
            parent: disk,
            start_lba: start,
            read: nop_read,
            write: nop_write,
        },
    ) != 0
    {
        return -1;
    }
    major as i32
}

/// 把 `base`（父设备名）与 `suffix`（分区号）拼成分区名，存入静态缓冲并返回。
fn make_partition_name(base: &str, suffix: u32) -> &'static str {
    unsafe {
        for i in 0..MAX_BLKDEV {
            if NAME_POOL_USED[i] {
                continue;
            }
            NAME_POOL_USED[i] = true;
            let dst = &mut NAME_POOL[i];
            let mut n = 0usize;
            for &c in base.as_bytes() {
                if n + 1 >= dst.len() {
                    break;
                }
                dst[n] = c;
                n += 1;
            }
            // 十进制后缀
            let mut v = suffix;
            let mut digits = [0u8; 8];
            let mut nd = 0usize;
            if v == 0 {
                digits[nd] = b'0';
                nd += 1;
            } else {
                while v > 0 {
                    digits[nd] = b'0' + (v % 10) as u8;
                    nd += 1;
                    v /= 10;
                }
            }
            while nd > 0 && n + 1 < dst.len() {
                nd -= 1;
                dst[n] = digits[nd];
                n += 1;
            }
            dst[n] = 0;
            return core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                dst.as_ptr(),
                n,
            ));
        }
    }
    "sda?"
}

// ==================== RAM 磁盘 ====================

fn ramdisk_read(base: usize, sector: usize, count: usize, buf: &mut [u8]) -> i32 {
    let n = buf.len().min(count * SECTOR_SIZE);
    unsafe {
        ptr::copy_nonoverlapping((base + sector * SECTOR_SIZE) as *const u8, buf.as_mut_ptr(), n);
    }
    n as i32
}

fn ramdisk_write(base: usize, sector: usize, count: usize, buf: &[u8]) -> i32 {
    let n = buf.len().min(count * SECTOR_SIZE);
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), (base + sector * SECTOR_SIZE) as *mut u8, n);
    }
    n as i32
}

/// 创建 RAM 磁盘：连续分配 `blocks` 个 4KB 物理页作为磁盘，注册为 `major` 号块设备。
pub fn ramdisk_create(name: &'static str, major: u32, blocks: usize) -> i32 {
    assert!(blocks > 0, "blkdev: ramdisk needs at least 1 block");
    let base = allocator::alloc_pages_contig(blocks).expect("blkdev: ramdisk out of physical memory");
    register_blkdev(
        major,
        name,
        BlkOps {
            base,
            num_sectors: blocks * SECTORS_PER_BLOCK,
            parent: 0,
            start_lba: 0,
            read: ramdisk_read,
            write: ramdisk_write,
        },
    )
}

// ==================== 磁盘加密（透明 XOR 包装） ====================
//
// 把分区包装成「加密块设备」：读 = 读密文 → XOR 解密；写 = XOR 加密 → 写密文。
// 上层文件系统（SFS/FAT32）完全无感知。MBR 分区类型 0x8E 标记加密卷，
// 内核探测到后自动包装。
//
// 密钥为演示固定密钥（真实系统应从 TPM / 用户口令派生）；流密码为
// key 按「扇区号 + 字节偏移」循环异或，同一扇区重复写使用相同流（可逆）。

/// 加密卷的 MBR 分区类型码。
pub const ENC_PART_TYPE: u8 = 0x8E;

/// 演示密钥（16 字节："KunxOS-Enc-Key13"）。
const ENC_KEY: [u8; 16] = [
    0x4B, 0x75, 0x6E, 0x78, 0x4F, 0x53, 0x2D, 0x45, 0x6E, 0x63, 0x2D, 0x4B, 0x65, 0x79, 0x31, 0x33,
];

/// 对 512 字节扇区做 XOR 加/解密（同一函数可逆）。
fn xor_sector(buf: &mut [u8], sector: usize) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b ^= ENC_KEY[(i + sector * SECTOR_SIZE) % 16];
    }
}

/// 加密设备的读：转发父设备读密文，解密后返回。
/// `base` 参数存父设备 major（见 `wrap_encryption`）。
fn enc_read(base: usize, sector: usize, count: usize, buf: &mut [u8]) -> i32 {
    let major = base as u32;
    let n = count * SECTOR_SIZE;
    let mut tmp = [0u8; 16384]; // 最多 32 扇区（16KB），覆盖 4KB 块（8 扇区）等
    if n > tmp.len() {
        return -1;
    }
    if blk_read_sectors(major, sector, count, &mut tmp[..n]) != n as i32 {
        return -1;
    }
    for s in 0..count {
        let off = s * SECTOR_SIZE;
        xor_sector(&mut tmp[off..off + SECTOR_SIZE], sector + s);
    }
    buf[..n].copy_from_slice(&tmp[..n]);
    n as i32
}

/// 加密设备的写：加密后转发父设备写密文。
fn enc_write(base: usize, sector: usize, count: usize, buf: &[u8]) -> i32 {
    let major = base as u32;
    let n = count * SECTOR_SIZE;
    let mut tmp = [0u8; 16384];
    if n > tmp.len() {
        return -1;
    }
    tmp[..n].copy_from_slice(&buf[..n]);
    for s in 0..count {
        let off = s * SECTOR_SIZE;
        xor_sector(&mut tmp[off..off + SECTOR_SIZE], sector + s);
    }
    blk_write_sectors(major, sector, count, &tmp[..n])
}

/// 把 `major` 分区包装为加密设备，返回新 major；失败 -1。
pub fn wrap_encryption(major: u32) -> i32 {
    let dev = match get_blkdev(major) {
        Some(d) => d,
        None => return -1,
    };
    let enc_major = major + 0x40; // 独立号段，避免与分区号冲突
    if get_blkdev(enc_major).is_some() {
        return enc_major as i32; // 已包装
    }
    // 复用分区名缓冲（父设备名 + 后缀 "e"）
    let mut nm = [0u8; 8];
    let mut k = 0usize;
    for &c in dev.name.as_bytes() {
        if k + 1 >= nm.len() {
            break;
        }
        nm[k] = c;
        k += 1;
    }
    nm[k] = b'e';
    k += 1;
    let name = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(nm.as_ptr(), k))
    };
    if register_blkdev(
        enc_major,
        name,
        BlkOps {
            base: major as usize, // 存父设备 major，供 enc_read/enc_write 转发
            num_sectors: dev.ops.num_sectors,
            parent: 0,
            start_lba: 0,
            read: enc_read,
            write: enc_write,
        },
    ) != 0
    {
        return -1;
    }
    enc_major as i32
}
