//! 简化 FAT32 文件系统读取（MBR 分区表 + FAT32 根目录）。
//!
//! 仅支持：短文件名（8.3）、根目录查找、单簇链（连续簇链）文件读取。
//! 块大小固定 512 字节。

use crate::mmc;

/// 扇区大小。
pub const SECTOR_SIZE: usize = 512;

/// MBR 分区表项中 FAT32 的类型码。
const PART_TYPE_FAT32_LBA: u8 = 0x0C;
const PART_TYPE_FAT32_CHS: u8 = 0x0B;

/// FAT 结束簇链标记（>= 0x0FFFFFF8）。
const EOC: u32 = 0x0FFF_FFF8;

/// 已挂载的 FAT32 卷信息。
pub struct FatVolume {
    /// 分区起始 LBA。
    part_lba: u64,
    /// 每簇扇区数。
    sectors_per_cluster: u32,
    /// 保留扇区数（含 BPB）。
    reserved_sectors: u32,
    /// 每个 FAT 表的大小（扇区数）。
    fat_size: u32,
    /// 根目录首簇。
    root_cluster: u32,
    /// 数据区起始扇区（首簇簇 2 所在扇区）。
    first_data_sector: u64,
}

/// 目录项（文件）。
pub struct DirEntry {
    pub first_cluster: u32,
    pub size: u32,
}

/// 小端读取辅助。
#[inline]
fn le16(b: &[u8]) -> u16 {
    b[0] as u16 | (b[1] as u16) << 8
}

#[inline]
fn le32(b: &[u8]) -> u32 {
    b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16 | (b[3] as u32) << 24
}

/// 把 "NAME.EXT" 形式的文件名转换为 11 字节的 FAT32 短目录项名（大写 + 空格填充）。
fn short_name(name: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let bytes = name.as_bytes();
    let mut ni = 0usize;
    let mut ei = 0usize;
    let mut in_ext = false;
    for &b in bytes {
        if b == b'.' {
            in_ext = true;
        } else {
            let up = if (b'a'..=b'z').contains(&b) { b - 32 } else { b };
            if !in_ext {
                if ni < 8 {
                    out[ni] = up;
                    ni += 1;
                }
            } else if ei < 3 {
                out[8 + ei] = up;
                ei += 1;
            }
        }
    }
    out
}

impl FatVolume {
    /// 读取 MBR 并挂载第一个 FAT32 分区。
    pub fn mount() -> Option<FatVolume> {
        let mut mbr = [0u8; SECTOR_SIZE];
        if !mmc::read_block(0, mbr.as_mut_ptr()) {
            return None;
        }
        // 校验 MBR 签名
        if mbr[510] != 0x55 || mbr[511] != 0xAA {
            return None;
        }

        // 查找 FAT32 分区
        let mut part_lba = 0u64;
        for i in 0..4 {
            let e = &mbr[446 + i * 16..446 + i * 16 + 16];
            let ptype = e[4];
            let lba = le32(&e[8..12]) as u64;
            let nsectors = le32(&e[12..16]) as u64;
            if nsectors == 0 {
                continue;
            }
            if ptype == PART_TYPE_FAT32_LBA || ptype == PART_TYPE_FAT32_CHS {
                part_lba = lba;
                break;
            }
            // 兜底：第一个非空分区
            if part_lba == 0 {
                part_lba = lba;
            }
        }
        if part_lba == 0 {
            return None;
        }

        // 读取分区 BPB
        let mut bpb = [0u8; SECTOR_SIZE];
        if !mmc::read_block(part_lba, bpb.as_mut_ptr()) {
            return None;
        }
        if bpb[510] != 0x55 || bpb[511] != 0xAA {
            return None;
        }

        let bytes_per_sector = le16(&bpb[11..13]) as u32;
        if bytes_per_sector as usize != SECTOR_SIZE {
            return None;
        }
        let sectors_per_cluster = bpb[13] as u32;
        // 校验每簇扇区数非零且为 2 的幂（非法值会导致读取死循环/越界）
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return None;
        }
        let reserved_sectors = le16(&bpb[14..16]) as u32;
        let num_fats = bpb[16] as u32;
        let fat_size = le32(&bpb[36..40]) as u32;
        let root_cluster = le32(&bpb[44..48]) as u32;
        let first_data_sector =
            part_lba + reserved_sectors as u64 + (num_fats as u64) * (fat_size as u64);

        Some(FatVolume {
            part_lba,
            sectors_per_cluster,
            reserved_sectors,
            fat_size,
            root_cluster,
            first_data_sector,
        })
    }

    /// 读取簇号对应的 FAT 表项（下一簇）。
    fn fat_entry(&self, cluster: u32) -> u32 {
        let fat_offset = cluster as u64 * 4;
        let fat_sector = self.part_lba + self.reserved_sectors as u64 + fat_offset / SECTOR_SIZE as u64;
        // FAT 边界校验：损坏/恶意的簇号会使 fat_sector 超出 FAT 区（落到
        // 数据区甚至分区外），读到无关数据作为 FAT 项 → 簇链错乱。越界
        // 一律视为链结束。
        let fat_end = self.part_lba + self.reserved_sectors as u64 + self.fat_size as u64;
        if fat_sector >= fat_end {
            return EOC;
        }
        let mut tmp = [0u8; SECTOR_SIZE];
        if !mmc::read_block(fat_sector, tmp.as_mut_ptr()) {
            return EOC;
        }
        let off = (fat_offset % SECTOR_SIZE as u64) as usize;
        le32(&tmp[off..off + 4]) & 0x0FFF_FFFF
    }

    /// 簇号转扇区号。
    #[inline]
    fn cluster_to_sector(&self, cluster: u32) -> u64 {
        self.first_data_sector + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    /// 在根目录中查找短文件名 `name`（如 "KERNEL.IMG"）。
    ///
    /// 沿根目录簇链遍历全部簇（根目录跨簇时后续簇中的条目也能找到）。
    pub fn find(&self, name: &str) -> Option<DirEntry> {
        let short = short_name(name);
        let mut cluster = self.root_cluster;
        let mut clusters_seen = 0u32;
        loop {
            if cluster < 2 || cluster >= EOC {
                return None;
            }
            clusters_seen += 1;
            if clusters_seen > 0x10000 {
                return None; // 簇链异常（成环等），防死循环
            }
            let sector = self.cluster_to_sector(cluster);
            let dir_sectors = self.sectors_per_cluster as usize;

            for s in 0..dir_sectors {
                let mut tmp = [0u8; SECTOR_SIZE];
                if !mmc::read_block(sector + s as u64, tmp.as_mut_ptr()) {
                    return None;
                }
                for i in 0..(SECTOR_SIZE / 32) {
                    let e = &tmp[i * 32..i * 32 + 32];
                    if e[0] == 0x00 {
                        return None; // 目录结束
                    }
                    if e[0] == 0xE5 {
                        continue; // 已删除
                    }
                    if e[11] & 0x0F == 0x0F {
                        continue; // 长文件名项
                    }
                    if e[0..11] == short[..] {
                        let cluster_high = le16(&e[26..28]) as u32;
                        let cluster_low = le16(&e[20..22]) as u32;
                        let first_cluster = (cluster_high << 16) | cluster_low;
                        let size = le32(&e[28..32]);
                        return Some(DirEntry { first_cluster, size });
                    }
                }
            }
            cluster = self.fat_entry(cluster);
        }
    }

    /// 读取文件内容到 `dst`（跟随簇链），写入总量不超过 `capacity` 字节。
    ///
    /// `capacity` 为 `dst` 目的区域可容纳的字节数，防止磁盘上损坏/恶意的 size
    /// 字段导致对任意内存的越界写（安全关键）。
    pub fn read_file(&self, entry: &DirEntry, dst: *mut u8, capacity: usize) -> bool {
        let mut cluster = entry.first_cluster;
        let mut remaining = entry.size as usize;
        let mut written = 0usize;

        while remaining > 0 {
            if cluster < 2 || cluster >= EOC {
                return false;
            }
            let sector = self.cluster_to_sector(cluster);
            let cluster_bytes = self.sectors_per_cluster as usize * SECTOR_SIZE;
            let to_read = core::cmp::min(remaining, cluster_bytes);
            if to_read == 0 {
                return false; // 每簇扇区数非法，避免死循环
            }
            if written + to_read > capacity {
                return false; // 超出目的区域容量，拒绝写入
            }
            if !mmc::read_bytes(sector * SECTOR_SIZE as u64, unsafe { dst.add(written) }, to_read)
            {
                return false;
            }
            written += to_read;
            remaining -= to_read;
            cluster = self.fat_entry(cluster);
        }
        true
    }
}
