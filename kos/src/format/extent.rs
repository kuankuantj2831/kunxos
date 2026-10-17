//! 区段表：对象数据/索引块的非连续布局（引擎 v2）。
//!
//! 每个对象紧跟头块之后是一串区段表块（链式），描述其全部数据区段
//! 与 Merkle 索引块区段。这解决了 v1 连续布局的两个问题：
//! - **碎片化**：对象不再要求连续空闲段，任意可用块都能用；
//! - **追加写**：新数据块挂新区段即可，不动旧数据。
//!
//! 区段表块布局（4096 字节，小端）：
//!
//! ```text
//! 偏移  大小  字段
//! 0     4    magic "KXT\0"
//! 4     4    flags（保留，0）
//! 8     32   merkle_root（整棵 Merkle 树根；首块权威，后续块忽略）
//! 40    4    total_chunks（数据 chunk 总数；首块权威）
//! 44    4    entry_count（本块内有效 entry 数）
//! 48    4    reserved
//! 52    4    crc32（本块 CRC，偏移处自跳过）
//! 56    8    next_table（下一区段表块号；0 = 链尾）
//! 64    4032 entries：240 × 16B { start u64, count u32, kind u32 }
//! ```
//!
//! `kind`：0 = 数据区段（`count` 为 chunk 数），1 = 索引块区段
//! （`count` 为块数，块序 = 第 0 层起逐层向上，与树几何一致）。

use super::{crc32_skip_field, get_u32, get_u64, put_u32, put_u64, KOS_BLOCK_SIZE};
use crate::error::KosError;

/// 区段表 magic：ASCII `"KXT\0"` 按小端 u32。
pub const EXTENT_TABLE_MAGIC: u32 = u32::from_le_bytes(*b"KXT\0");
/// 区段表块大小（= 全局块大小）。
pub const EXTENT_TABLE_SIZE: usize = KOS_BLOCK_SIZE;
/// 单块区段容量：(4096 - 64) / 16 = 253 → 取 240 留余量。
pub const EXTENTS_PER_TABLE: usize = 240;

/// 数据区段。
pub const KIND_DATA: u32 = 0;
/// Merkle 索引块区段。
pub const KIND_INDEX: u32 = 1;

/// `crc32` 字段偏移。
const OFF_CRC: usize = 52;
/// entry 区起始偏移。
const OFF_ENTRIES: usize = 64;
/// 单 entry 字节数。
const EXTENT_ENTRY_SIZE: usize = 16;

/// 内存形态的一个区段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// 起始块号（绝对）。
    pub start: u64,
    /// 数据：chunk 数；索引：块数。
    pub count: u32,
    /// [`KIND_DATA`] / [`KIND_INDEX`]。
    pub kind: u32,
}

/// 区段表块（解析形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtentTable {
    /// 必须等于 [`EXTENT_TABLE_MAGIC`]。
    pub magic: u32,
    /// 保留，0。
    pub flags: u32,
    /// Merkle 树根（仅链首块有意义）。
    pub merkle_root: [u8; 32],
    /// 数据 chunk 总数（仅链首块有意义）。
    pub total_chunks: u32,
    /// 本块内 entry 数。
    pub entry_count: u32,
    /// 下一表块号（0 = 链尾）。
    pub next_table: u64,
    /// 区段数组（前 `entry_count` 个有效）。
    pub entries: [Extent; EXTENTS_PER_TABLE],
}

impl ExtentTable {
    /// 结构大小（字节）。
    pub const SIZE: usize = EXTENT_TABLE_SIZE;

    /// 空表。
    pub fn empty(merkle_root: [u8; 32], total_chunks: u32) -> Self {
        ExtentTable {
            magic: EXTENT_TABLE_MAGIC,
            flags: 0,
            merkle_root,
            total_chunks,
            entry_count: 0,
            next_table: 0,
            entries: [Extent {
                start: 0,
                count: 0,
                kind: 0,
            }; EXTENTS_PER_TABLE],
        }
    }

    /// 序列化为 4096 字节映像（CRC 重算）。
    pub fn to_bytes(&self) -> [u8; EXTENT_TABLE_SIZE] {
        let mut b = [0u8; EXTENT_TABLE_SIZE];
        put_u32(&mut b, 0, self.magic);
        put_u32(&mut b, 4, self.flags);
        b[8..40].copy_from_slice(&self.merkle_root);
        put_u32(&mut b, 40, self.total_chunks);
        put_u32(&mut b, 44, self.entry_count);
        // 48..52 reserved 置零
        put_u32(&mut b, OFF_CRC, 0); // 先置零再重算
        put_u64(&mut b, 56, self.next_table);
        for i in 0..EXTENTS_PER_TABLE {
            let o = OFF_ENTRIES + i * EXTENT_ENTRY_SIZE;
            let e = self.entries[i];
            put_u64(&mut b, o, e.start);
            put_u32(&mut b, o + 8, e.count);
            put_u32(&mut b, o + 12, e.kind);
        }
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b
    }

    /// 从 4096 字节映像解析。校验长度、CRC、magic、entry_count 上限。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != EXTENT_TABLE_SIZE {
            return Err(KosError::BufferTooShort {
                need: EXTENT_TABLE_SIZE,
                got: bytes.len(),
            });
        }
        let magic = get_u32(bytes, 0);
        if magic != EXTENT_TABLE_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "ExtentTable",
            });
        }
        let stored_crc = get_u32(bytes, OFF_CRC);
        let mut probe = [0u8; EXTENT_TABLE_SIZE];
        probe.copy_from_slice(bytes);
        let expect_crc = crc32_skip_field(&probe, OFF_CRC, 4);
        if stored_crc != expect_crc {
            return Err(KosError::CrcMismatch {
                context: "ExtentTable",
                expected: stored_crc,
                actual: expect_crc,
            });
        }
        let entry_count = get_u32(bytes, 44);
        if entry_count as usize > EXTENTS_PER_TABLE {
            return Err(KosError::CountMismatch {
                expected: EXTENTS_PER_TABLE as u16,
                got: entry_count as u16,
            });
        }
        let mut merkle_root = [0u8; 32];
        merkle_root.copy_from_slice(&bytes[8..40]);
        let mut entries = [Extent {
            start: 0,
            count: 0,
            kind: 0,
        }; EXTENTS_PER_TABLE];
        for i in 0..EXTENTS_PER_TABLE {
            let o = OFF_ENTRIES + i * EXTENT_ENTRY_SIZE;
            entries[i] = Extent {
                start: get_u64(bytes, o),
                count: get_u32(bytes, o + 8),
                kind: get_u32(bytes, o + 12),
            };
        }
        Ok(ExtentTable {
            magic,
            flags: get_u32(bytes, 4),
            merkle_root,
            total_chunks: get_u32(bytes, 40),
            entry_count,
            next_table: get_u64(bytes, 56),
            entries,
        })
    }
}
