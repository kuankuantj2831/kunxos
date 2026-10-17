//! 超级块（Block 0）：全局布局 + 双 Root 槽指针。

use super::{crc32_skip_field, get_array, get_u32, get_u64, put_array, put_u32, put_u64};
use crate::error::KosError;

/// 超级块 magic：ASCII `"KOSV0001"` 按小端 u64 解释。
pub const SUPERBLOCK_MAGIC: u64 = u64::from_le_bytes(*b"KOSV0001");
/// 本 crate 读写 的超级块版本。
pub const SUPERBLOCK_VERSION: u32 = 1;
/// 块 0 为超级块，块 1/2 为 Root Manifest 双槽，块 3..16 预留。
pub const RESERVED_BLOCKS_BEFORE_POOLS: u64 = 16;

/// 超级块固定大小（等于一个块）。
pub const SUPERBLOCK_SIZE: usize = 4096;

/// `crc32` 字段在映像内的字节偏移。
const OFF_CRC: usize = 96;

/// 超级块：磁盘全局元数据，位于 Block 0。
///
/// 字节布局见 `to_bytes`；`repr(C)` 下自然大小恰为 4096（无隐式填充，
/// 尾部由 `reserved` 显式补齐）。
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KosSuperblock {
    /// 必须等于 [`SUPERBLOCK_MAGIC`]（`"KOSV0001"`）。
    pub magic: u64,
    /// 格式版本，本版本为 1。
    pub version: u32,
    /// 块大小，本版本固定 4096。
    pub block_size: u32,
    /// 设备总块数。
    pub total_blocks: u64,
    /// 空闲块数。
    pub free_blocks: u64,
    /// Object Pool 起始块号。
    pub obj_pool_start: u64,
    /// Object Pool 块数。
    pub obj_pool_blocks: u64,
    /// Object Pool 空闲链表头（块号）。
    pub obj_free_head: u64,
    /// Record Pool 起始块号。
    pub rec_pool_start: u64,
    /// Record Pool 块数。
    pub rec_pool_blocks: u64,
    /// Record Pool 已分配 inode 数（动态扩容，无 256 硬上限）。
    pub rec_inode_count: u32,
    /// Record Pool 空闲 inode 数。
    pub rec_inode_free: u32,
    /// Root Manifest 槽 A 块号。
    pub root_slot_a: u64,
    /// Root Manifest 槽 B 块号。
    pub root_slot_b: u64,
    /// 全映像 CRC32（本字段自身不参与计算）。`to_bytes` 总是重算。
    pub crc32: u32,
    /// 显式补齐/预留，建盘时置零，解析后原样保留。
    pub reserved: [u8; 3996],
}

impl KosSuperblock {
    /// 结构大小（字节）。
    pub const SIZE: usize = SUPERBLOCK_SIZE;

    /// 生成一份测试用的参考布局：
    /// 块 0 超级块、块 1/2 Root 双槽、块 3..16 预留，
    /// 其余可用块按 80% / 20% 划分给 Object / Record Pool。
    ///
    /// 这是格式层的字段填充辅助，不执行任何块设备写入；
    /// 真正的 mkfs 属于后续步骤。
    pub fn mkfs_layout(total_blocks: u64) -> Self {
        assert!(total_blocks > RESERVED_BLOCKS_BEFORE_POOLS, "总块数必须大于预留块数");
        let usable = total_blocks - RESERVED_BLOCKS_BEFORE_POOLS;
        let obj_blocks = usable * 4 / 5;
        let rec_blocks = usable - obj_blocks;
        KosSuperblock {
            magic: SUPERBLOCK_MAGIC,
            version: SUPERBLOCK_VERSION,
            block_size: KOS_BLOCK_SIZE_U32,
            total_blocks,
            free_blocks: usable,
            obj_pool_start: RESERVED_BLOCKS_BEFORE_POOLS,
            obj_pool_blocks: obj_blocks,
            obj_free_head: RESERVED_BLOCKS_BEFORE_POOLS,
            rec_pool_start: RESERVED_BLOCKS_BEFORE_POOLS + obj_blocks,
            rec_pool_blocks: rec_blocks,
            rec_inode_count: 0,
            rec_inode_free: 0,
            root_slot_a: 1,
            root_slot_b: 2,
            crc32: 0,
            reserved: [0u8; 3996],
        }
    }

    /// 序列化为 4096 字节映像（小端，CRC 重算后写入）。
    pub fn to_bytes(&self) -> [u8; SUPERBLOCK_SIZE] {
        let mut b = [0u8; SUPERBLOCK_SIZE];
        put_u64(&mut b, 0, self.magic);
        put_u32(&mut b, 8, self.version);
        put_u32(&mut b, 12, self.block_size);
        put_u64(&mut b, 16, self.total_blocks);
        put_u64(&mut b, 24, self.free_blocks);
        put_u64(&mut b, 32, self.obj_pool_start);
        put_u64(&mut b, 40, self.obj_pool_blocks);
        put_u64(&mut b, 48, self.obj_free_head);
        put_u64(&mut b, 56, self.rec_pool_start);
        put_u64(&mut b, 64, self.rec_pool_blocks);
        put_u32(&mut b, 72, self.rec_inode_count);
        put_u32(&mut b, 76, self.rec_inode_free);
        put_u64(&mut b, 80, self.root_slot_a);
        put_u64(&mut b, 88, self.root_slot_b);
        // reserved（含 P2 挂载 hint）先写入再算 CRC：from_bytes 对全块
        // 计算，两者必须覆盖同一字节集。reserved 全零时与旧版字节兼容。
        put_array(&mut b, OFF_CRC + 4, &self.reserved);
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b
    }

    /// 从字节映像解析。校验：长度、CRC、magic、version、block_size。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != SUPERBLOCK_SIZE {
            return Err(KosError::BufferTooShort {
                need: SUPERBLOCK_SIZE,
                got: bytes.len(),
            });
        }
        let stored = get_u32(bytes, OFF_CRC);
        let computed = crc32_skip_field(bytes, OFF_CRC, 4);
        if stored != computed {
            return Err(KosError::CrcMismatch {
                context: "superblock",
                expected: stored,
                actual: computed,
            });
        }
        let magic = get_u64(bytes, 0);
        if magic != SUPERBLOCK_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "superblock",
            });
        }
        let version = get_u32(bytes, 8);
        if version != SUPERBLOCK_VERSION {
            return Err(KosError::UnsupportedVersion { version });
        }
        let block_size = get_u32(bytes, 12);
        if block_size as usize != super::KOS_BLOCK_SIZE {
            return Err(KosError::InvalidBlockSize { block_size });
        }
        Ok(KosSuperblock {
            magic,
            version,
            block_size,
            total_blocks: get_u64(bytes, 16),
            free_blocks: get_u64(bytes, 24),
            obj_pool_start: get_u64(bytes, 32),
            obj_pool_blocks: get_u64(bytes, 40),
            obj_free_head: get_u64(bytes, 48),
            rec_pool_start: get_u64(bytes, 56),
            rec_pool_blocks: get_u64(bytes, 64),
            rec_inode_count: get_u32(bytes, 72),
            rec_inode_free: get_u32(bytes, 76),
            root_slot_a: get_u64(bytes, 80),
            root_slot_b: get_u64(bytes, 88),
            crc32: stored,
            reserved: get_array(bytes, OFF_CRC + 4),
        })
    }
}

/// `KOS_BLOCK_SIZE` 的 u32 形式（超级块字段用）。
const KOS_BLOCK_SIZE_U32: u32 = super::KOS_BLOCK_SIZE as u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_matches_design() {
        assert_eq!(core::mem::size_of::<KosSuperblock>(), KosSuperblock::SIZE);
        assert_eq!(KosSuperblock::SIZE, 4096);
    }

    #[test]
    fn roundtrip() {
        let sb = KosSuperblock::mkfs_layout(1 << 20);
        let img = sb.to_bytes();
        assert_eq!(&img[0..8], b"KOSV0001");
        let parsed = KosSuperblock::from_bytes(&img).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.root_slot_a, 1);
        assert_eq!(parsed.root_slot_b, 2);
        assert_eq!(parsed.obj_pool_blocks + parsed.rec_pool_blocks, (1 << 20) - 16);
        // 再序列化必须字节一致（CRC 与 reserved 均保真）
        assert_eq!(parsed.to_bytes(), img);
    }

    #[test]
    fn reject_wrong_length() {
        let img = KosSuperblock::mkfs_layout(1024).to_bytes();
        let e = KosSuperblock::from_bytes(&img[..4095]).unwrap_err();
        assert_eq!(
            e,
            KosError::BufferTooShort {
                need: 4096,
                got: 4095
            }
        );
    }

    #[test]
    fn reject_bad_magic_or_crc() {
        let mut img = KosSuperblock::mkfs_layout(1024).to_bytes();
        img[0] ^= 0xFF; // 破坏 magic（同时破坏 CRC）
        assert!(KosSuperblock::from_bytes(&img).is_err());

        let mut img = KosSuperblock::mkfs_layout(1024).to_bytes();
        img[500] ^= 0x01; // 只破坏 reserved 区
        assert!(matches!(
            KosSuperblock::from_bytes(&img).unwrap_err(),
            KosError::CrcMismatch { .. }
        ));
    }
}
