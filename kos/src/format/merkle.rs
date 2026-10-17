//! Merkle 宽树索引块。
//!
//! 设计文档 3.3 画的是「每块 512 个 hash」，但 512 × 32B = 16KB，
//! 与 4KB 块模型自相矛盾。本实现取 4KB 索引块：127 个 32 字节
//! 哈希 + 12 字节头 + 4 字节 CRC + 16 字节预留 = 4096（偏差已记入
//! README）。
//!
//! 父块对子块的哈希覆盖**整块序列化字节**（含头与 CRC），因此
//! 逐块 CRC 只是快速垃圾过滤，真正的完整性由上层哈希链承担。

use super::{
    crc32_skip_field, get_array, get_u16, get_u32, put_array, put_u32, KOS_BLOCK_SIZE,
};
use crate::error::KosError;

/// 索引块 magic：ASCII `"KMIX"` 按小端 u32 解释。
pub const MERKLE_INDEX_MAGIC: u32 = u32::from_le_bytes(*b"KMIX");
/// 哈希长度（BLAKE3 / SHA-256 摘要长度）。
pub const HASH_LEN: usize = 32;
/// 宽树分支因子：每索引块容纳的哈希数。
///
/// (4096 - 12 字节头 - 4 字节 CRC - 16 字节预留) / 32 = 127。
pub const MERKLE_FANOUT: usize = 127;
/// 索引块固定大小（等于一个块）。
pub const MERKLE_INDEX_BLOCK_SIZE: usize = KOS_BLOCK_SIZE;

/// `crc32` 字段在映像内的字节偏移。
const OFF_CRC: usize = 12 + MERKLE_FANOUT * HASH_LEN; // 4076

/// Merkle 宽树的一个索引块（4096 字节，最多 127 个子哈希）。
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleIndexBlock {
    /// 必须等于 [`MERKLE_INDEX_MAGIC`]（`"KMIX"`）。
    pub magic: u32,
    /// 层号：0 = 直接位于数据块之上。
    pub level: u16,
    /// 有效哈希数（0..=127）。
    pub count: u16,
    /// 层内序号（该块在本层的下标）。
    pub index: u32,
    /// 子哈希槽位；`count` 之后必须为零。
    pub hashes: [[u8; HASH_LEN]; MERKLE_FANOUT],
    /// 全块 CRC32（本字段自身不参与计算）。
    pub crc32: u32,
    /// 显式尾部补齐到 4096。
    pub reserved: [u8; 16],
}

impl MerkleIndexBlock {
    /// 结构大小（字节）。
    pub const SIZE: usize = MERKLE_INDEX_BLOCK_SIZE;

    /// 构造索引块：写入 `children`（长度必须 ≤ 127），其余槽位置零。
    ///
    /// CRC 在构造时就算好，因此 `check_integrity` 对新建块立即通过。
    pub fn new(level: u16, index: u32, children: &[[u8; HASH_LEN]]) -> Self {
        assert!(
            children.len() <= MERKLE_FANOUT,
            "children 超过分支因子 {}",
            MERKLE_FANOUT
        );
        let mut hashes = [[0u8; HASH_LEN]; MERKLE_FANOUT];
        hashes[..children.len()].copy_from_slice(children);
        let mut blk = MerkleIndexBlock {
            magic: MERKLE_INDEX_MAGIC,
            level,
            count: children.len() as u16,
            index,
            hashes,
            crc32: 0,
            reserved: [0u8; 16],
        };
        let img = blk.to_bytes(); // to_bytes 重算 CRC，回填保持一致
        blk.crc32 = get_u32(&img, OFF_CRC);
        blk
    }

    /// 结构自检：magic、count 上限、CRC。
    ///
    /// `verify` 流程在每个路径块上先调用它，防御调用方绕过
    /// `from_bytes` 手工构造的块。
    pub fn check_integrity(&self) -> Result<(), KosError> {
        self.verified_image().map(|_| ())
    }

    /// 自检（同 [`check_integrity` 的检查项]）并返回序列化映像，
    /// 供验证流程直接哈希，避免同一块序列化两次。
    pub(crate) fn verified_image(&self) -> Result<[u8; MERKLE_INDEX_BLOCK_SIZE], KosError> {
        if self.magic != MERKLE_INDEX_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "merkle-index",
            });
        }
        if self.count as usize > MERKLE_FANOUT {
            return Err(KosError::CountMismatch {
                expected: MERKLE_FANOUT as u16,
                got: self.count,
            });
        }
        let img = self.to_bytes();
        let computed = crc32_skip_field(&img, OFF_CRC, 4);
        if self.crc32 != computed {
            return Err(KosError::CrcMismatch {
                context: "merkle-index",
                expected: self.crc32,
                actual: computed,
            });
        }
        Ok(img)
    }

    /// 序列化为 4096 字节映像（小端，CRC 重算后写入）。
    pub fn to_bytes(&self) -> [u8; MERKLE_INDEX_BLOCK_SIZE] {
        let mut b = [0u8; MERKLE_INDEX_BLOCK_SIZE];
        put_u32(&mut b, 0, self.magic);
        b[4..6].copy_from_slice(&self.level.to_le_bytes());
        b[6..8].copy_from_slice(&self.count.to_le_bytes());
        b[8..12].copy_from_slice(&self.index.to_le_bytes());
        for (i, h) in self.hashes.iter().enumerate() {
            put_array(&mut b, 12 + i * HASH_LEN, h);
        }
        b[OFF_CRC + 4..MERKLE_INDEX_BLOCK_SIZE].copy_from_slice(&self.reserved);
        // reserved 先于 CRC（同 object.rs 顺序 bug 预防性修正）。
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b
    }

    /// 从字节映像解析。校验：长度、CRC、magic、count 上限、
    /// 未用槽位全零（规范形式）。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != MERKLE_INDEX_BLOCK_SIZE {
            return Err(KosError::BufferTooShort {
                need: MERKLE_INDEX_BLOCK_SIZE,
                got: bytes.len(),
            });
        }
        let stored = get_u32(bytes, OFF_CRC);
        let computed = crc32_skip_field(bytes, OFF_CRC, 4);
        if stored != computed {
            return Err(KosError::CrcMismatch {
                context: "merkle-index",
                expected: stored,
                actual: computed,
            });
        }
        let magic = get_u32(bytes, 0);
        if magic != MERKLE_INDEX_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "merkle-index",
            });
        }
        let count = get_u16(bytes, 6);
        if count as usize > MERKLE_FANOUT {
            return Err(KosError::CountMismatch {
                expected: MERKLE_FANOUT as u16,
                got: count,
            });
        }
        for i in count as usize..MERKLE_FANOUT {
            let off = 12 + i * HASH_LEN;
            if bytes[off..off + HASH_LEN].iter().any(|&b| b != 0) {
                return Err(KosError::UnusedSlotNonZero { index: i as u16 });
            }
        }
        let mut hashes = [[0u8; HASH_LEN]; MERKLE_FANOUT];
        for (i, h) in hashes.iter_mut().enumerate() {
            *h = get_array(bytes, 12 + i * HASH_LEN);
        }
        Ok(MerkleIndexBlock {
            magic,
            level: get_u16(bytes, 4),
            count,
            index: get_u32(bytes, 8),
            hashes,
            crc32: stored,
            reserved: get_array(bytes, OFF_CRC + 4),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_design() {
        assert_eq!(core::mem::size_of::<MerkleIndexBlock>(), MerkleIndexBlock::SIZE);
        assert_eq!(MerkleIndexBlock::SIZE, 4096);
        assert_eq!(MERKLE_FANOUT, 127);
        assert_eq!(OFF_CRC, 4076);
    }

    #[test]
    fn roundtrip() {
        let children: Vec<[u8; 32]> = (0..MERKLE_FANOUT)
            .map(|i| [(i % 251) as u8 + 1; 32])
            .collect();
        let blk = MerkleIndexBlock::new(3, 42, &children);
        let img = blk.to_bytes();
        assert_eq!(&img[0..4], b"KMIX");
        let parsed = MerkleIndexBlock::from_bytes(&img).unwrap();
        assert_eq!(parsed.level, 3);
        assert_eq!(parsed.index, 42);
        assert_eq!(parsed.count as usize, MERKLE_FANOUT);
        assert_eq!(parsed.hashes[126], [127u8; 32]);
        assert_eq!(parsed.to_bytes(), img);
        assert!(blk.check_integrity().is_ok());
        assert!(parsed.check_integrity().is_ok());
    }

    #[test]
    fn empty_block_roundtrip() {
        let blk = MerkleIndexBlock::new(0, 0, &[]);
        let parsed = MerkleIndexBlock::from_bytes(&blk.to_bytes()).unwrap();
        assert_eq!(parsed.count, 0);
        assert!(parsed.check_integrity().is_ok());
    }

    #[test]
    fn reject_wrong_length_and_count() {
        let blk = MerkleIndexBlock::new(0, 0, &[]);
        let img = blk.to_bytes();
        assert!(matches!(
            MerkleIndexBlock::from_bytes(&img[..4095]).unwrap_err(),
            KosError::BufferTooShort { .. }
        ));
        let mut b2 = blk.clone();
        b2.count = MERKLE_FANOUT as u16 + 1;
        let img2 = b2.to_bytes(); // CRC 覆盖 128，只可能挂在 count 校验
        assert!(matches!(
            MerkleIndexBlock::from_bytes(&img2).unwrap_err(),
            KosError::CountMismatch { .. }
        ));
    }
}
