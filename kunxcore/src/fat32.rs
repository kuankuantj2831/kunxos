//! FAT32 文件系统（读 + 写），作为 VFS 的第二种文件系统。
//!
//! 特性与简化：
//! - 短文件名（8.3）：读写均只支持 8.3；长文件名（LFN）目录项在遍历时跳过，
//!   写入时不生成 LFN（含中文等非 ASCII 的文件名会以 `_` 落入 8.3 短名）。
//! - ino 不是磁盘概念：FAT32 没有 inode 表。本模块用一张静态
//!   「打开文件上下文表」把 `(dev, 父目录簇, 目录项槽号)` 映射为 ino，
//!   供 VFS 在 open 期间稳定引用；文件被 unlink / rename 时同步更新表项。
//! - 根目录 ino 固定为 1；其余 ino = 表索引 + 1。
//! - 簇链、目录项、FAT 表（FAT1 与 FAT2 同步写）均正确维护，簇可跨扇区、
//!   跨簇读写；文件大小更新写回目录项。
//! - 扇区大小固定 512；每簇扇区数取自 BPB（须为 2 的幂）。

use crate::blkdev::{self, SECTOR_SIZE};

/// FAT 链结束标记（>= 0x0FFFFFF8 均视为结束）。
const EOC: u32 = 0x0FFF_FFF8;
/// FAT 表项掩码（FAT32 高 4 位保留）。
const FAT_MASK: u32 = 0x0FFF_FFFF;
/// 簇号保留值 0（未分配）。
const FREE_CLUSTER: u32 = 0;

// ---- 目录项属性位 ----
const ATTR_READONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;

// ---- VFS 类型码（与 fs_simple 保持一致） ----
const T_REG: u16 = 1;
const T_DIR: u16 = 2;

/// 同时挂载的 FAT32 卷上限。
const MAX_VOLUMES: usize = 2;
/// 打开文件上下文表容量（决定可同时引用的文件数）。
const MAX_ENTRIES: usize = 256;

/// 一个已挂载的 FAT32 卷（按块设备 major 区分）。
#[derive(Clone, Copy)]
struct FatVol {
    dev: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_size: u32,
    root_cluster: u32,
    data_base: u64,
    total_clusters: u32,
}

/// 一个被引用的文件/目录上下文。
#[derive(Clone, Copy)]
struct FatEntry {
    used: bool,
    dev: u32,
    /// 父目录簇（根目录父为 0，无目录项可更新）。
    parent: u32,
    /// 目录项槽号（32 字节项序号，相对父目录首项）。
    slot: u32,
    /// 起始簇（0 = 空文件）。
    cluster: u32,
    size: u32,
    attr: u8,
    /// 簇位置缓存（续走优化）：上次定位的簇及其簇内基偏移。
    /// 顺序读写大文件时从缓存续走，避免每次操作从头走整条簇链（O(n²)）。
    cache_off: u32,
    cache_cluster: u32,
}

static mut VOLS: [Option<FatVol>; MAX_VOLUMES] = [None; MAX_VOLUMES];
static mut ENTRIES: [FatEntry; MAX_ENTRIES] = [FatEntry {
    used: false,
    dev: 0,
    parent: 0,
    slot: 0,
    cluster: 0,
    size: 0,
    attr: 0,
    cache_off: 0,
    cache_cluster: 0,
}; MAX_ENTRIES];
/// 每个卷的「下一个空闲簇」游标：簇分配从游标处续扫，避免每次从头
/// 全表扫描（cp 大文件时 O(n²) → 数十秒；游标化后接近 O(n)）。
static mut NEXT_FREE: [u32; MAX_VOLUMES] = [2; MAX_VOLUMES];

// ==================== 小端辅助 ====================

#[inline]
fn le16(b: &[u8]) -> u16 {
    b[0] as u16 | (b[1] as u16) << 8
}

#[inline]
fn le32(b: &[u8]) -> u32 {
    b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16 | (b[3] as u32) << 24
}

#[inline]
fn put_le16(b: &mut [u8], v: u16) {
    b[0] = (v & 0xFF) as u8;
    b[1] = (v >> 8) as u8;
}

#[inline]
fn put_le32(b: &mut [u8], v: u32) {
    b[0] = (v & 0xFF) as u8;
    b[1] = ((v >> 8) & 0xFF) as u8;
    b[2] = ((v >> 16) & 0xFF) as u8;
    b[3] = ((v >> 24) & 0xFF) as u8;
}

// ==================== 卷查找 ====================

fn vol_of(major: u32) -> Option<FatVol> {
    unsafe {
        for v in VOLS.iter() {
            if let Some(vol) = v {
                if vol.dev == major {
                    return Some(*vol);
                }
            }
        }
    }
    None
}

fn register_vol(vol: FatVol) -> bool {
    unsafe {
        for v in VOLS.iter_mut() {
            if v.is_none() {
                *v = Some(vol);
                return true;
            }
        }
    }
    false
}

// ==================== 条目表 ====================

/// 根目录 ino（固定 1）。
pub const ROOT_INO: u32 = 1;

fn entry_of(major: u32, ino: u32) -> Option<&'static mut FatEntry> {
    if ino == 0 || ino as usize > MAX_ENTRIES {
        return None;
    }
    let idx = (ino - 1) as usize;
    unsafe {
        let e = &mut ENTRIES[idx];
        if e.used && e.dev == major {
            Some(e)
        } else {
            None
        }
    }
}

/// 查找 (dev, parent, slot) 是否已有表项，返回 ino；无则分配新表项并返回。
fn entry_for(dev: u32, parent: u32, slot: u32, cluster: u32, size: u32, attr: u8) -> Option<u32> {
    unsafe {
        for (i, e) in ENTRIES.iter_mut().enumerate() {
            if e.used && e.dev == dev && e.parent == parent && e.slot == slot {
                e.cluster = cluster;
                e.size = size;
                e.attr = attr;
                return Some(i as u32 + 1);
            }
        }
        for (i, e) in ENTRIES.iter_mut().enumerate() {
            if !e.used {
                *e = FatEntry {
                    used: true,
                    dev,
                    parent,
                    slot,
                    cluster,
                    size,
                    attr,
                    cache_off: 0,
                    cache_cluster: 0,
                };
                return Some(i as u32 + 1);
            }
        }
    }
    None
}

/// 释放表项（unlink 后调用）。
fn entry_free(dev: u32, parent: u32, slot: u32) {
    unsafe {
        for e in ENTRIES.iter_mut() {
            if e.used && e.dev == dev && e.parent == parent && e.slot == slot {
                e.used = false;
                return;
            }
        }
    }
}

// ==================== 扇区 / FAT / 簇 ====================

fn read_sectors(major: u32, sector: u64, count: usize, buf: &mut [u8]) -> bool {
    blkdev::blk_read_sectors(major, sector as usize, count, buf) == (count * SECTOR_SIZE) as i32
}

fn write_sectors(major: u32, sector: u64, count: usize, buf: &[u8]) -> bool {
    blkdev::blk_write_sectors(major, sector as usize, count, buf) == (count * SECTOR_SIZE) as i32
}

/// 读取 FAT 表项（下一簇号），越界 / 出错按链结束处理。
fn fat_entry(vol: &FatVol, cluster: u32) -> u32 {
    let off = cluster as u64 * 4;
    let sector = vol.reserved_sectors as u64 + off / SECTOR_SIZE as u64;
    let fat_end = vol.reserved_sectors as u64 + vol.fat_size as u64;
    if sector >= fat_end {
        return EOC;
    }
    let mut tmp = [0u8; SECTOR_SIZE];
    if !read_sectors(vol.dev, sector, 1, &mut tmp) {
        return EOC;
    }
    let o = (off % SECTOR_SIZE as u64) as usize;
    le32(&tmp[o..o + 4]) & FAT_MASK
}

/// 写入 FAT 表项（FAT1 与 FAT2 同步，保证与宿主工具一致）。
fn set_fat(vol: &FatVol, cluster: u32, value: u32) -> bool {
    let off = cluster as u64 * 4;
    let sector = vol.reserved_sectors as u64 + off / SECTOR_SIZE as u64;
    let fat_end = vol.reserved_sectors as u64 + vol.fat_size as u64;
    if sector >= fat_end {
        return false;
    }
    let o = (off % SECTOR_SIZE as u64) as usize;
    let mut tmp = [0u8; SECTOR_SIZE];
    if !read_sectors(vol.dev, sector, 1, &mut tmp) {
        return false;
    }
    let cur = le32(&tmp[o..o + 4]) & FAT_MASK;
    // 组合保留高 4 位
    let new = (cur & !FAT_MASK) | (value & FAT_MASK);
    put_le32(&mut tmp[o..o + 4], new);
    if !write_sectors(vol.dev, sector, 1, &tmp) {
        return false;
    }
    // FAT2 同步
    let sec2 = vol.reserved_sectors as u64 + vol.fat_size as u64 + off / SECTOR_SIZE as u64;
    if sec2 < vol.reserved_sectors as u64 + 2 * vol.fat_size as u64 {
        let mut tmp2 = [0u8; SECTOR_SIZE];
        if read_sectors(vol.dev, sec2, 1, &mut tmp2) {
            put_le32(&mut tmp2[o..o + 4], new);
            let _ = write_sectors(vol.dev, sec2, 1, &tmp2);
        }
    }
    true
}

/// 簇号 → 分区内起始扇区。
#[inline]
fn cluster_sector(vol: &FatVol, cluster: u32) -> u64 {
    vol.data_base + (cluster as u64 - 2) * vol.sectors_per_cluster as u64
}

/// 每簇字节数。
#[inline]
fn cluster_bytes(vol: &FatVol) -> usize {
    vol.sectors_per_cluster as usize * SECTOR_SIZE
}

/// 卷在静态表中的索引（NEXT_FREE 游标按此索引）。
fn vol_index(vol: &FatVol) -> usize {
    unsafe {
        for (i, v) in VOLS.iter().enumerate() {
            if let Some(x) = v {
                if x.dev == vol.dev {
                    return i;
                }
            }
        }
    }
    0
}

/// 分配一个新簇：FAT 中找空闲簇（值 0），置为链结束；`prev` 非 0 时把
/// `prev` 的链尾接到新簇。返回新簇号；失败 0。
///
/// 从本卷的「下一个空闲簇」游标续扫（先游标后，再从头扫到游标），
/// 顺序写大文件时接近线性时间。
fn alloc_cluster(vol: &FatVol, prev: u32) -> u32 {
    // 可分配簇号上限：FAT 表能表达的簇数（fat_size 扇区 × 每扇区 128 项）。
    // 超过该范围的簇在 FAT 中无对应项（fat_entry 越界返回 EOC），不可分配。
    let fat_clusters = vol.fat_size.saturating_mul(128);
    let limit = vol.total_clusters.min(fat_clusters).saturating_add(2);
    let idx = vol_index(vol);
    let mut c = unsafe { NEXT_FREE[idx] }.max(2);
    let mut found = 0u32;
    // 第一遍：游标 → 上限
    while c < limit {
        if c == vol.root_cluster {
            c += 1;
            continue;
        }
        if fat_entry(vol, c) == FREE_CLUSTER {
            found = c;
            break;
        }
        c += 1;
    }
    // 第二遍：2 → 游标（回收释放的簇）
    if found == 0 {
        let mut c2 = 2u32;
        let cur = unsafe { NEXT_FREE[idx] }.max(2);
        while c2 < cur && c2 < limit {
            if c2 == vol.root_cluster {
                c2 += 1;
                continue;
            }
            if fat_entry(vol, c2) == FREE_CLUSTER {
                found = c2;
                break;
            }
            c2 += 1;
        }
    }
    if found == 0 {
        return 0; // 卷满
    }
    if !set_fat(vol, found, EOC) {
        return 0;
    }
    if prev >= 2 {
        if !set_fat(vol, prev, found) {
            let _ = set_fat(vol, found, FREE_CLUSTER);
            return 0;
        }
    }
    unsafe {
        NEXT_FREE[idx] = found + 1;
    }
    found
}

/// 释放整条簇链（FAT 项清零）。
fn free_chain(vol: &FatVol, mut c: u32) {
    let mut seen = 0u32;
    // 释放的簇可被游标回收：把游标重置到链首（取 min）
    let idx = vol_index(vol);
    unsafe {
        if NEXT_FREE[idx] > c {
            NEXT_FREE[idx] = c;
        }
    }
    while c >= 2 && c < EOC {
        let next = fat_entry(vol, c);
        let _ = set_fat(vol, c, FREE_CLUSTER);
        c = next;
        seen += 1;
        if seen > 0x10000 {
            break; // 防成环死循环
        }
    }
}

// ==================== 目录项 ====================

/// 读取目录 `dir_cluster` 的第 `slot` 个目录项（跨簇链）。
fn dir_entry_at(vol: &FatVol, dir_cluster: u32, slot: u32) -> Option<[u8; 32]> {
    let items_per_cluster = cluster_bytes(vol) / 32;
    let mut cluster = dir_cluster;
    let mut steps = slot / items_per_cluster as u32;
    let mut seen = 0u32;
    while steps > 0 {
        let next = fat_entry(vol, cluster);
        if next < 2 || next >= EOC {
            return None;
        }
        cluster = next;
        steps -= 1;
        seen += 1;
        if seen > 0x10000 {
            return None;
        }
    }
    let in_cluster = (slot % items_per_cluster as u32) as usize;
    let sector_off = in_cluster * 32 / SECTOR_SIZE;
    let in_sector = in_cluster * 32 % SECTOR_SIZE;
    let mut tmp = [0u8; SECTOR_SIZE];
    if !read_sectors(
        vol.dev,
        cluster_sector(vol, cluster) + sector_off as u64,
        1,
        &mut tmp,
    ) {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&tmp[in_sector..in_sector + 32]);
    Some(out)
}

/// 写目录项：读-改-写所在扇区。
fn write_dir_entry(vol: &FatVol, dir_cluster: u32, slot: u32, ent: &[u8; 32]) -> bool {
    let items_per_cluster = cluster_bytes(vol) / 32;
    let mut cluster = dir_cluster;
    let mut steps = slot / items_per_cluster as u32;
    let mut seen = 0u32;
    while steps > 0 {
        let next = fat_entry(vol, cluster);
        if next < 2 || next >= EOC {
            return false;
        }
        cluster = next;
        steps -= 1;
        seen += 1;
        if seen > 0x10000 {
            return false;
        }
    }
    let in_cluster = (slot % items_per_cluster as u32) as usize;
    let sector_off = in_cluster * 32 / SECTOR_SIZE;
    let in_sector = in_cluster * 32 % SECTOR_SIZE;
    let mut tmp = [0u8; SECTOR_SIZE];
    if !read_sectors(
        vol.dev,
        cluster_sector(vol, cluster) + sector_off as u64,
        1,
        &mut tmp,
    ) {
        return false;
    }
    tmp[in_sector..in_sector + 32].copy_from_slice(ent);
    write_sectors(vol.dev, cluster_sector(vol, cluster) + sector_off as u64, 1, &tmp)
}

/// 在目录里找空闲目录项槽号（0xE5 或 0x00），并返回该槽；目录满返回 None。
/// 顺带可确认目录未被破坏。
fn find_free_slot(vol: &FatVol, dir_cluster: u32) -> Option<u32> {
    let mut slot = 0u32;
    loop {
        match dir_entry_at(vol, dir_cluster, slot) {
            Some(e) => {
                if e[0] == 0x00 || e[0] == 0xE5 {
                    return Some(slot);
                }
            }
            None => return None, // 目录簇链结束（目录满）
        }
        slot += 1;
        if slot > 0x10000 {
            return None;
        }
    }
}

// ==================== 8.3 短名 ====================

/// 把 "NAME.EXT" 转成 11 字节 FAT 短名（大写 + 空格填充；非 ASCII 转 `_`）。
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
            let up = if (b'a'..=b'z').contains(&b) {
                b - 32
            } else if b.is_ascii() && b != 0 && !matches!(b, b'/' | b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|') {
                b
            } else {
                b'_'
            };
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

/// 把 11 字节 FAT 短名还原为 "NAME.EXT" 字符串（去尾空格）。
fn long_name(short: &[u8; 11], out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut base_end = 8usize;
    while base_end > 0 && short[base_end - 1] == b' ' {
        base_end -= 1;
    }
    for i in 0..base_end {
        if n < out.len() {
            out[n] = short[i];
            n += 1;
        }
    }
    let mut ext_end = 11usize;
    while ext_end > 8 && short[ext_end - 1] == b' ' {
        ext_end -= 1;
    }
    if ext_end > 8 {
        if n < out.len() {
            out[n] = b'.';
            n += 1;
        }
        for i in 8..ext_end {
            if n < out.len() {
                out[n] = short[i];
                n += 1;
            }
        }
    }
    n
}

// ==================== 对外接口（与 fs_simple 平行） ====================

/// 探测 `major` 设备是否为 FAT32 卷（BPB 签名 + 根目录簇合法）。
pub fn fs_probe(major: u32) -> bool {
    let mut bpb = [0u8; SECTOR_SIZE];
    if blkdev::blk_read_sectors(major, 0, 1, &mut bpb) != SECTOR_SIZE as i32 {
        return false;
    }
    if bpb[510] != 0x55 || bpb[511] != 0xAA {
        return false;
    }
    let bps = le16(&bpb[11..13]);
    if bps != SECTOR_SIZE as u16 {
        return false;
    }
    let spc = bpb[13] as u32;
    if spc == 0 || !spc.is_power_of_two() {
        return false;
    }
    let fat_size = le32(&bpb[36..40]);
    if fat_size == 0 {
        return false; // FAT16 等（fat_size_16 非零）不支持
    }
    let root_cluster = le32(&bpb[44..48]);
    if root_cluster < 2 {
        return false;
    }
    // fs_type 字段（BPB 偏移 82）
    if bpb[82..90] != *b"FAT32   " {
        return false;
    }
    true
}

/// 挂载 `major` 设备：读 BPB 存卷参数并注册根目录表项，返回 0；失败 -1。
pub fn mount(major: u32) -> i32 {
    if vol_of(major).is_some() {
        return 0; // 已挂载
    }
    let mut bpb = [0u8; SECTOR_SIZE];
    if blkdev::blk_read_sectors(major, 0, 1, &mut bpb) != SECTOR_SIZE as i32 {
        return -1;
    }
    let spc = bpb[13] as u32;
    let reserved = le16(&bpb[14..16]) as u32;
    let num_fats = bpb[16] as u32;
    let fat_size = le32(&bpb[36..40]);
    let root_cluster = le32(&bpb[44..48]);
    if spc == 0 || !spc.is_power_of_two() || fat_size == 0 || root_cluster < 2 {
        return -1;
    }
    let data_base = reserved as u64 + num_fats as u64 * fat_size as u64;
    // 总簇数：从分区大小推算（分区相对：num_sectors - data_base）
    let num_sectors = match blkdev::get_blkdev(major) {
        Some(d) => d.ops.num_sectors,
        None => return -1,
    };
    let total_clusters = if data_base >= num_sectors as u64 {
        0
    } else {
        ((num_sectors as u64 - data_base) / spc as u64) as u32
    };
    let vol = FatVol {
        dev: major,
        sectors_per_cluster: spc,
        reserved_sectors: reserved,
        num_fats,
        fat_size,
        root_cluster,
        data_base,
        total_clusters,
    };
    if !register_vol(vol) {
        return -1; // 卷表满
    }
    // 注册根目录表项（ino = 1）
    unsafe {
        ENTRIES[0] = FatEntry {
            used: true,
            dev: major,
            parent: 0,
            slot: 0,
            cluster: root_cluster,
            size: 0,
            attr: ATTR_DIRECTORY,
            cache_off: 0,
            cache_cluster: 0,
        };
    }
    0
}

/// 根 ino（固定 1）。
pub fn fs_root_ino(_major: u32) -> u32 {
    ROOT_INO
}

/// 在目录 `dir_ino` 中查找 `name`，返回 ino；未找到 -1。
pub fn fs_lookup(major: u32, dir_ino: u32, name: &str) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let dir = match entry_of(major, dir_ino) {
        Some(e) => *e,
        None => return -1,
    };
    if dir.attr & ATTR_DIRECTORY == 0 {
        return -1;
    }
    let short = short_name(name);
    let mut slot = 0u32;
    loop {
        match dir_entry_at(&vol, dir.cluster, slot) {
            Some(e) => {
                if e[0] == 0x00 {
                    return -1; // 目录结束
                }
                if e[0] == 0xE5 || e[11] == ATTR_LFN {
                    slot += 1;
                    continue;
                }
                if e[11] & (ATTR_VOLUME) != 0 {
                    slot += 1;
                    continue;
                }
                if e[0..11] == short[..] {
                    let cluster = le16(&e[20..22]) as u32 | (le16(&e[26..28]) as u32) << 16;
                    let size = le32(&e[28..32]);
                    match entry_for(major, dir.cluster, slot, cluster, size, e[11]) {
                        Some(ino) => return ino as i32,
                        None => return -1, // 表满
                    }
                }
                slot += 1;
            }
            None => return -1, // 目录簇链异常/结束
        }
    }
}

/// 创建文件或设备节点（FAT32 只支持普通文件；设备节点以普通文件创建，
/// rdev 忽略），返回新文件 ino；失败 -1。
pub fn fs_create(major: u32, dir_ino: u32, name: &str, _typ: u16, _rdev: u32) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let dir = match entry_of(major, dir_ino) {
        Some(e) => *e,
        None => return -1,
    };
    if dir.attr & ATTR_DIRECTORY == 0 {
        return -1;
    }
    // 重名检查（FAT 不允许同名）
    if fs_lookup(major, dir_ino, name) >= 0 {
        return -1;
    }
    let slot = match find_free_slot(&vol, dir.cluster) {
        Some(s) => s,
        None => return -1, // 目录满
    };
    let mut ent = [0u8; 32];
    ent[0..11].copy_from_slice(&short_name(name));
    ent[11] = ATTR_ARCHIVE;
    put_le16(&mut ent[20..22], 0); // 首簇低 16 位（空文件）
    put_le16(&mut ent[26..28], 0); // 首簇高 16 位
    put_le32(&mut ent[28..32], 0); // 大小 0
    if !write_dir_entry(&vol, dir.cluster, slot, &ent) {
        return -1;
    }
    match entry_for(major, dir.cluster, slot, 0, 0, ATTR_ARCHIVE) {
        Some(ino) => ino as i32,
        None => -1,
    }
}

/// 创建子目录，返回 ino；失败 -1。
pub fn fs_mkdir(major: u32, dir_ino: u32, name: &str) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let dir = match entry_of(major, dir_ino) {
        Some(e) => *e,
        None => return -1,
    };
    if fs_lookup(major, dir_ino, name) >= 0 {
        return -1;
    }
    let cluster = alloc_cluster(&vol, 0);
    if cluster == 0 {
        return -1; // 无空闲簇
    }
    // 初始化目录内容：. 与 ..
    let mut dot = [0u8; 32];
    dot[0..11].copy_from_slice(b".          ");
    dot[11] = ATTR_DIRECTORY;
    put_le16(&mut dot[20..22], cluster as u16);
    put_le16(&mut dot[26..28], (cluster >> 16) as u16);
    let mut dotdot = [0u8; 32];
    dotdot[0..11].copy_from_slice(b"..         ");
    dotdot[11] = ATTR_DIRECTORY;
    // ".." 指向父目录（即 dir 所在的目录簇）
    let parent_cluster = dir.cluster;
    put_le16(&mut dotdot[20..22], parent_cluster as u16);
    put_le16(&mut dotdot[26..28], (parent_cluster >> 16) as u16);
    let base = cluster_sector(&vol, cluster);
    let mut tmp = [0u8; SECTOR_SIZE];
    if !read_sectors(major, base, 1, &mut tmp) {
        free_chain(&vol, cluster);
        return -1;
    }
    tmp[0..32].copy_from_slice(&dot);
    tmp[32..64].copy_from_slice(&dotdot);
    if !write_sectors(major, base, 1, &tmp) {
        free_chain(&vol, cluster);
        return -1;
    }
    // 父目录项
    let slot = match find_free_slot(&vol, dir.cluster) {
        Some(s) => s,
        None => {
            free_chain(&vol, cluster);
            return -1;
        }
    };
    let mut ent = [0u8; 32];
    ent[0..11].copy_from_slice(&short_name(name));
    ent[11] = ATTR_DIRECTORY;
    put_le16(&mut ent[20..22], cluster as u16);
    put_le16(&mut ent[26..28], (cluster >> 16) as u16);
    put_le32(&mut ent[28..32], 0);
    if !write_dir_entry(&vol, dir.cluster, slot, &ent) {
        free_chain(&vol, cluster);
        return -1;
    }
    match entry_for(major, dir.cluster, slot, cluster, 0, ATTR_DIRECTORY) {
        Some(ino) => ino as i32,
        None => -1,
    }
}

/// 读取文件内容（从 `offset` 开始，至多 `buf.len()` 字节），返回字节数；失败 -1。
/// 更新簇位置缓存：`cur` 是最后访问的簇，`cur_off` 是访问终点。
/// ★ cache_off 必须指向「cur 所在簇的起始偏移」——若终点恰在簇边界，
/// cur 是终点**之前**的最后一个满簇，其起始 = 终点 - 簇大小；否则 cur
/// 就是终点所在簇。错位 1 簇会导致下次读写从旧簇续写：数据重叠、
/// 簇链少分配（实测 cp 1.6MB 文件只建成 2830 簇）、读提前 EOF。
fn update_cache(e: &mut FatEntry, cur_off: usize, cur: u32, cb: usize) {
    let aligned = (cur_off / cb) * cb;
    let cache_off = if cur_off % cb == 0 && cur_off > 0 {
        aligned - cb
    } else {
        aligned
    };
    e.cache_off = cache_off as u32;
    e.cache_cluster = cur;
}

/// 定位到 `offset` 所在簇（读路径，不分配簇）。
/// 优先从 entry 的簇位置缓存续走，否则从起始簇走；返回簇号，失败 None。
/// 成功后更新缓存。
fn locate_cluster(vol: &FatVol, e: &mut FatEntry, offset: usize) -> Option<u32> {
    let cb = cluster_bytes(vol);
    let base_off = (offset / cb * cb) as u32;
    let (mut c, mut off) = if e.cache_cluster >= 2
        && offset >= e.cache_off as usize
        && offset - e.cache_off as usize <= 0x10000
    {
        (e.cache_cluster, e.cache_off)
    } else {
        (e.cluster, 0u32)
    };
    if c < 2 {
        return None;
    }
    let mut steps = (base_off - off) / cb as u32;
    let mut guard = 0u32;
    while steps > 0 {
        let next = fat_entry(vol, c);
        if next < 2 || next >= EOC {
            return None;
        }
        c = next;
        steps -= 1;
        guard += 1;
        if guard > 0x10000 {
            return None;
        }
    }
    e.cache_off = base_off;
    e.cache_cluster = c;
    Some(c)
}

/// 读取文件内容（从 `offset` 开始，至多 `buf.len()` 字节），返回字节数；失败 -1。
pub fn fs_read(major: u32, ino: u32, offset: usize, buf: &mut [u8]) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let e = match entry_of(major, ino) {
        Some(e) => e,
        None => return -1,
    };
    if e.cluster == 0 || offset >= e.size as usize || buf.is_empty() {
        return 0; // EOF
    }
    let cb = cluster_bytes(&vol);
    let mut cur = match locate_cluster(&vol, e, offset) {
        Some(c) => c,
        None => return 0,
    };
    let mut done = 0usize;
    let mut cur_off = offset;
    let mut guard = 0u32;
    while done < buf.len() && cur_off < e.size as usize {
        let in_cl = cur_off % cb;
        let remain = (e.size as usize - cur_off).min(buf.len() - done);
        let n = (cb - in_cl).min(remain);
        // 逐扇区读取 [in_cl, in_cl+n)
        let base = cluster_sector(&vol, cur);
        let mut got = 0usize;
        while got < n {
            let sec = (in_cl + got) / SECTOR_SIZE;
            let so = (in_cl + got) % SECTOR_SIZE;
            let take = (SECTOR_SIZE - so).min(n - got);
            let mut tmp = [0u8; SECTOR_SIZE];
            if !read_sectors(major, base + sec as u64, 1, &mut tmp) {
                return done as i32;
            }
            buf[done + got..done + got + take].copy_from_slice(&tmp[so..so + take]);
            got += take;
        }
        done += n;
        cur_off += n;
        if in_cl + n == cb && done < buf.len() {
            let next = fat_entry(&vol, cur);
            if next < 2 || next >= EOC {
                break;
            }
            cur = next;
        }
        guard += 1;
        if guard > 0x10000 {
            break;
        }
    }
    // 更新缓存（★ cache_off 须为 cur 所在簇的起始偏移，见 update_cache）
    update_cache(e, cur_off, cur, cb);
    done as i32
}

/// 写入文件内容（从 `offset` 开始），必要时分配簇并更新大小，返回字节数；失败 -1。
pub fn fs_write(major: u32, ino: u32, offset: usize, buf: &[u8]) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let cb = cluster_bytes(&vol);
    if buf.is_empty() {
        return 0;
    }
    let e = match entry_of(major, ino) {
        Some(e) => e,
        None => return -1,
    };
    if e.attr & ATTR_DIRECTORY != 0 {
        return -1; // 目录不可写
    }
    let mut cur = 0u32;
    // 定位 / 补足 offset 所在簇（写路径：链断时分配扩展）
    if e.cluster == 0 {
        cur = alloc_cluster(&vol, 0);
        if cur == 0 {
            return -1;
        }
        e.cluster = cur;
        e.cache_off = 0;
        e.cache_cluster = cur;
        // 更新目录项首簇（空文件 → 首个簇）
        update_dir_cluster(&vol, e);
    } else {
        let mut c = if e.cache_cluster >= 2
            && offset >= e.cache_off as usize
            && offset - e.cache_off as usize <= 0x10000
        {
            e.cache_cluster
        } else {
            e.cluster
        };
        let base_off = (offset / cb * cb) as u32;
        let start_off = if offset >= e.cache_off as usize
            && offset - e.cache_off as usize <= 0x10000
        {
            e.cache_off
        } else {
            0
        };
        let mut steps = (base_off - start_off) / cb as u32;
        let mut guard = 0u32;
        while steps > 0 {
            let next = fat_entry(&vol, c);
            if next < 2 || next >= EOC {
                let nc = alloc_cluster(&vol, c);
                if nc == 0 {
                    return -1;
                }
                c = nc;
            } else {
                c = next;
            }
            steps -= 1;
            guard += 1;
            if guard > 0x10000 {
                return -1;
            }
        }
        e.cache_off = base_off;
        e.cache_cluster = c;
        cur = c;
    }
    let mut done = 0usize;
    let mut cur_off = offset;
    let mut guard = 0u32;
    while done < buf.len() {
        let in_cl = cur_off % cb;
        let n = (cb - in_cl).min(buf.len() - done);
        let base = cluster_sector(&vol, cur);
        let mut wrote = 0usize;
        while wrote < n {
            let sec = (in_cl + wrote) / SECTOR_SIZE;
            let so = (in_cl + wrote) % SECTOR_SIZE;
            let take = (SECTOR_SIZE - so).min(n - wrote);
            if take == SECTOR_SIZE {
                // 整扇区直写
                if !write_sectors(major, base + sec as u64, 1, &buf[done + wrote..done + wrote + take]) {
                    return done as i32;
                }
            } else {
                // 部分扇区：读-改-写
                let mut tmp = [0u8; SECTOR_SIZE];
                if !read_sectors(major, base + sec as u64, 1, &mut tmp) {
                    return done as i32;
                }
                tmp[so..so + take].copy_from_slice(&buf[done + wrote..done + wrote + take]);
                if !write_sectors(major, base + sec as u64, 1, &tmp) {
                    return done as i32;
                }
            }
            wrote += take;
        }
        done += n;
        cur_off += n;
        if in_cl + n == cb && done < buf.len() {
            let next = fat_entry(&vol, cur);
            if next < 2 || next >= EOC {
                let nc = alloc_cluster(&vol, cur);
                if nc == 0 {
                    break;
                }
                cur = nc;
            } else {
                cur = next;
            }
        }
        guard += 1;
        if guard > 0x10000 {
            break;
        }
    }
    // 同步内存 entry 与磁盘目录项：大小（首簇更新已在首次分配时完成）。
    let new_size = e.size.max(cur_off as u32);
    if new_size != e.size {
        e.size = new_size;
        update_dir_size(&vol, e);
    }
    // 更新缓存（★ cache_off 须为 cur 所在簇的起始偏移，见 update_cache）
    update_cache(e, cur_off, cur, cb);
    done as i32
}

/// 更新目录项中的首簇字段（空文件首次分配簇后）。
fn update_dir_cluster(vol: &FatVol, e: &FatEntry) {
    if e.parent == 0 {
        return; // 根目录无目录项
    }
    if let Some(mut ent) = dir_entry_at(vol, e.parent, e.slot) {
        put_le16(&mut ent[20..22], e.cluster as u16);
        put_le16(&mut ent[26..28], (e.cluster >> 16) as u16);
        let _ = write_dir_entry(vol, e.parent, e.slot, &ent);
    }
}

/// 更新目录项中的大小字段。
fn update_dir_size(vol: &FatVol, e: &FatEntry) {
    if e.parent == 0 {
        return;
    }
    if let Some(mut ent) = dir_entry_at(vol, e.parent, e.slot) {
        put_le32(&mut ent[28..32], e.size);
        let _ = write_dir_entry(vol, e.parent, e.slot, &ent);
    }
}

/// 截断文件为 0：释放全部簇，更新目录项与上下文。
pub fn fs_truncate(major: u32, ino: u32) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let e = match entry_of(major, ino) {
        Some(e) => *e,
        None => return -1,
    };
    if e.attr & ATTR_DIRECTORY != 0 {
        return -1;
    }
    if e.cluster != 0 {
        free_chain(&vol, e.cluster);
    }
    if let Some(se) = entry_of(major, ino) {
        se.cluster = 0;
        se.size = 0;
    }
    if e.parent != 0 {
        if let Some(mut ent) = dir_entry_at(&vol, e.parent, e.slot) {
            put_le16(&mut ent[20..22], 0);
            put_le16(&mut ent[26..28], 0);
            put_le32(&mut ent[28..32], 0);
            let _ = write_dir_entry(&vol, e.parent, e.slot, &ent);
        }
    }
    0
}

/// 删除目录（须为空）或文件：释放簇链、标记目录项 0xE5、释放上下文表项。
pub fn fs_unlink(major: u32, dir_ino: u32, name: &str) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let dir = match entry_of(major, dir_ino) {
        Some(e) => *e,
        None => return -1,
    };
    let ino = fs_lookup(major, dir_ino, name);
    if ino < 0 {
        return -1;
    }
    let e = match entry_of(major, ino as u32) {
        Some(e) => *e,
        None => return -1,
    };
    // 目录非空不可删（不含 . 与 .. 之外的有效项）
    if e.attr & ATTR_DIRECTORY != 0 {
        let mut slot = 2usize; // 跳过 . 和 ..
        loop {
            match dir_entry_at(&vol, e.cluster, slot as u32) {
                Some(ent) => {
                    if ent[0] == 0x00 {
                        break;
                    }
                    if ent[0] != 0xE5 && ent[11] != ATTR_LFN && ent[11] & ATTR_VOLUME == 0 {
                        return -1; // 目录非空
                    }
                }
                None => break,
            }
            slot += 1;
        }
    }
    if e.cluster != 0 {
        free_chain(&vol, e.cluster);
    }
    // 标记删除
    if let Some(mut ent) = dir_entry_at(&vol, dir.cluster, e.slot) {
        ent[0] = 0xE5;
        let _ = write_dir_entry(&vol, dir.cluster, e.slot, &ent);
    }
    entry_free(major, dir.cluster, e.slot);
    0
}

/// 重命名（可跨目录）：复制目录项到新目录后删除旧目录项。
pub fn fs_rename(major: u32, old_dir: u32, old_name: &str, new_dir: u32, new_name: &str) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let od = match entry_of(major, old_dir) {
        Some(e) => *e,
        None => return -1,
    };
    let nd = match entry_of(major, new_dir) {
        Some(e) => *e,
        None => return -1,
    };
    let ino = fs_lookup(major, old_dir, old_name);
    if ino < 0 {
        return -1;
    }
    if fs_lookup(major, new_dir, new_name) >= 0 {
        return -1; // 目标已存在
    }
    let e = match entry_of(major, ino as u32) {
        Some(e) => *e,
        None => return -1,
    };
    // 读取旧目录项 → 改名 → 写入新目录空闲槽
    let mut ent = match dir_entry_at(&vol, od.cluster, e.slot) {
        Some(x) => x,
        None => return -1,
    };
    let new_short = short_name(new_name);
    ent[0..11].copy_from_slice(&new_short);
    let new_slot = match find_free_slot(&vol, nd.cluster) {
        Some(s) => s,
        None => return -1,
    };
    if !write_dir_entry(&vol, nd.cluster, new_slot, &ent) {
        return -1;
    }
    // 删除旧目录项
    if !(od.cluster == nd.cluster && e.slot == new_slot) {
        if let Some(mut old) = dir_entry_at(&vol, od.cluster, e.slot) {
            old[0] = 0xE5;
            let _ = write_dir_entry(&vol, od.cluster, e.slot, &old);
        }
    }
    // 更新上下文表项
    unsafe {
        for en in ENTRIES.iter_mut() {
            if en.used && en.dev == major && en.parent == od.cluster && en.slot == e.slot {
                en.parent = nd.cluster;
                en.slot = new_slot;
            }
        }
    }
    0
}

/// 读取目录第 `index` 个有效条目的名字，返回 ino；结束 -1。
pub fn fs_readdir(major: u32, ino: u32, index: usize, name: &mut [u8]) -> i32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return -1,
    };
    let e = match entry_of(major, ino) {
        Some(e) => *e,
        None => return -1,
    };
    if e.attr & ATTR_DIRECTORY == 0 {
        return -1;
    }
    let mut seen = 0usize;
    let mut slot = 0u32;
    loop {
        match dir_entry_at(&vol, e.cluster, slot) {
            Some(ent) => {
                if ent[0] == 0x00 {
                    return -1; // 目录结束
                }
                if ent[0] != 0xE5 && ent[11] != ATTR_LFN && ent[11] & ATTR_VOLUME == 0 {
                    if seen == index {
                        let n = long_name(&{
                            let mut s = [0u8; 11];
                            s.copy_from_slice(&ent[0..11]);
                            s
                        }, name);
                        if n < name.len() {
                            name[n] = 0;
                        }
                        let cluster = le16(&ent[20..22]) as u32 | (le16(&ent[26..28]) as u32) << 16;
                        let size = le32(&ent[28..32]);
                        return match entry_for(major, e.cluster, slot, cluster, size, ent[11]) {
                            Some(i) => i as i32,
                            None => -1,
                        };
                    }
                    seen += 1;
                }
            }
            None => return -1,
        }
        slot += 1;
        if slot > 0x10000 {
            return -1;
        }
    }
}

/// 空闲簇数（扫描 FAT，慢但准确）。
pub fn fs_free_blocks(major: u32) -> u32 {
    let vol = match vol_of(major) {
        Some(v) => v,
        None => return 0,
    };
    let mut free = 0u32;
    let fat_clusters = vol.fat_size.saturating_mul(128);
    let limit = vol.total_clusters.min(fat_clusters).saturating_add(2);
    let mut c = 2u32;
    while c < limit {
        if fat_entry(&vol, c) == FREE_CLUSTER {
            free += 1;
        }
        c += 1;
    }
    free
}

/// 总簇数。
pub fn fs_total_blocks(major: u32) -> u32 {
    match vol_of(major) {
        Some(v) => v.total_clusters,
        None => 0,
    }
}

/// 类型码（T_REG / T_DIR）。
pub fn fs_type(major: u32, ino: u32) -> u16 {
    match entry_of(major, ino) {
        Some(e) if e.attr & ATTR_DIRECTORY != 0 => T_DIR,
        Some(_) => T_REG,
        None => T_REG,
    }
}

/// 文件大小（字节）。
pub fn fs_size(major: u32, ino: u32) -> u32 {
    match entry_of(major, ino) {
        Some(e) => e.size,
        None => 0,
    }
}

/// 设备号（FAT32 无设备文件，恒 0）。
pub fn fs_rdev(_major: u32, _ino: u32) -> u32 {
    0
}
