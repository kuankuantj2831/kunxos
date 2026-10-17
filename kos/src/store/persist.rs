//! 持久分配索引（P2）：快速挂载三件套。
//!
//! v2 引擎挂载原本要扫全池（O(池块数)，2TB 卷不可用）。本模块把挂载
//! 所需状态持久化到 Record Pool 区（超级块 `rec_pool_start` 起）：
//!
//! 1. **空闲位图**（块链，每块自描述）：池块 → 空闲/占用；
//! 2. **对象索引**（块链）：全部对象头块号列表；
//! 3. **挂载提示（hint）**：置于超级块 `reserved[0..32]`，记录位图/
//!    索引位置与 `seq`（= 写入时的 manifest generation）。
//!
//! 快速挂载：hint 合法且 `seq == 活动 manifest generation` → 读位图 +
//! 索引链 + 逐对象头/区段表 → 建内存索引。任一环节校验失败（含崩溃后
//! 位图滞后于 manifest）→ **回退全池扫描**（权威恢复路径）。
//!
//! 写序（commit 内）：manifest 切换 → flush → 脏位图块 → 索引链 →
//! hint + 超级块。崩溃在任何一步：seq 不匹配 → 回退扫描，正确性不受
//! 影响，只损失一次快速挂载。

use crate::error::KosError;
use alloc::vec::Vec;
use crate::format::superblock::KosSuperblock;

/// 位图块 magic。
pub const BITMAP_MAGIC: u32 = u32::from_le_bytes(*b"KBM\0");
/// 对象索引块 magic。
pub const INDEX_MAGIC: u32 = u32::from_le_bytes(*b"KIX\0");
/// hint magic。
pub const HINT_MAGIC: u32 = u32::from_le_bytes(*b"KMH\0");

/// 每个位图块的位容量：4096 - 16 头 = 4080 字节载荷。
pub const BITS_PER_BMP_BLOCK: usize = 4080 * 8;
/// 每个索引块的头块号容量：(4096 - 24) / 8 = 509。
pub const IDS_PER_IDX_BLOCK: usize = 509;

/// hint（超级块 reserved[0..32]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountHint {
    pub magic: u32,
    /// 写入时的 manifest generation（与活动槽一致才走快速路径）。
    pub seq: u64,
    /// 位图块数。
    pub bitmap_blocks: u32,
    /// 索引链首块号（绝对）。
    pub index_head: u64,
    /// 索引块数。
    pub index_blocks: u32,
    /// 保留。
    pub reserved: u32,
}

impl MountHint {
    pub const SIZE: usize = 32;

    pub fn to_bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12..16].copy_from_slice(&self.bitmap_blocks.to_le_bytes());
        b[16..24].copy_from_slice(&self.index_head.to_le_bytes());
        b[24..28].copy_from_slice(&self.index_blocks.to_le_bytes());
        b[28..32].copy_from_slice(&self.reserved.to_le_bytes());
        b
    }

    /// 从超级块 reserved 前 32 字节解析（magic 不符返回 None）。
    pub fn from_sb(sb: &KosSuperblock) -> Option<Self> {
        let b = &sb.reserved[..32];
        let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if magic != HINT_MAGIC {
            return None;
        }
        Some(MountHint {
            magic,
            seq: u64::from_le_bytes(b[4..12].try_into().ok()?),
            bitmap_blocks: u32::from_le_bytes(b[12..16].try_into().ok()?),
            index_head: u64::from_le_bytes(b[16..24].try_into().ok()?),
            index_blocks: u32::from_le_bytes(b[24..28].try_into().ok()?),
            reserved: u32::from_le_bytes(b[28..32].try_into().ok()?),
        })
    }

    pub fn write_to_sb(&self, sb: &mut KosSuperblock) {
        sb.reserved[..32].copy_from_slice(&self.to_bytes());
    }
}

/// 序列化一个位图块：`bits` 为覆盖池块 `[first_bit, first_bit+n)` 的
/// 位切片（true = 空闲）。每块头 16B {magic, first_bit, reserved, crc}。
pub fn bitmap_block_bytes(first_bit: u32, bits: &[bool]) -> [u8; 4096] {
    let mut b = [0u8; 4096];
    b[0..4].copy_from_slice(&BITMAP_MAGIC.to_le_bytes());
    b[4..8].copy_from_slice(&first_bit.to_le_bytes());
    b[8..12].copy_from_slice(&0u32.to_le_bytes());
    for (i, &f) in bits.iter().enumerate() {
        if f {
            b[16 + i / 8] |= 1 << (i % 8);
        }
    }
    let crc = crc32_skip_field_(&b, 12);
    b[12..16].copy_from_slice(&crc.to_le_bytes());
    b
}

/// 解析位图块。返回 (first_bit, bits)；magic/CRC 不符返回 None。
pub fn bitmap_block_parse(b: &[u8; 4096]) -> Option<(u32, Vec<bool>)> {
    if b[0..4] != BITMAP_MAGIC.to_le_bytes() {
        return None;
    }
    let stored = u32::from_le_bytes(b[12..16].try_into().ok()?);
    if crc32_skip_field_(b, 12) != stored {
        return None;
    }
    let first = u32::from_le_bytes(b[4..8].try_into().ok()?);
    let bits = (0..BITS_PER_BMP_BLOCK).map(|i| b[16 + i / 8] & (1 << (i % 8)) != 0).collect();
    Some((first, bits))
}

/// 序列化一个对象索引块。
pub fn index_block_bytes(next: u64, ids: &[u64]) -> [u8; 4096] {
    let mut b = [0u8; 4096];
    b[0..4].copy_from_slice(&INDEX_MAGIC.to_le_bytes());
    b[4..8].copy_from_slice(&(ids.len() as u32).to_le_bytes());
    b[8..16].copy_from_slice(&next.to_le_bytes());
    for (i, &id) in ids.iter().enumerate() {
        b[24 + i * 8..24 + i * 8 + 8].copy_from_slice(&id.to_le_bytes());
    }
    let crc = crc32_skip_field_(&b, 16);
    b[16..20].copy_from_slice(&crc.to_le_bytes());
    b
}

/// 解析对象索引块，返回 (next, ids)。
pub fn index_block_parse(b: &[u8; 4096]) -> Option<(u64, Vec<u64>)> {
    if b[0..4] != INDEX_MAGIC.to_le_bytes() {
        return None;
    }
    let stored = u32::from_le_bytes(b[16..20].try_into().ok()?);
    if crc32_skip_field_(b, 16) != stored {
        return None;
    }
    let count = u32::from_le_bytes(b[4..8].try_into().ok()?) as usize;
    if count > IDS_PER_IDX_BLOCK {
        return None;
    }
    let next = u64::from_le_bytes(b[8..16].try_into().ok()?);
    let ids = (0..count)
        .map(|i| u64::from_le_bytes(b[24 + i * 8..24 + i * 8 + 8].try_into().unwrap()))
        .collect();
    Some((next, ids))
}

/// CRC32（跳过 `off..off+4`，即 CRC 字段自身）。复用格式层实现。
fn crc32_skip_field_(b: &[u8], off: usize) -> u32 {
    crate::format::crc32_skip_field(b, off, 4)
}

/// 位图所需块数。
pub fn bitmap_blocks_for(pool_blocks: usize) -> u64 {
    pool_blocks.div_ceil(BITS_PER_BMP_BLOCK) as u64
}

/// 让 KosError 满足模块内部使用（占位，防未使用告警）。
#[allow(unused)]
fn _err(_: KosError) {}
