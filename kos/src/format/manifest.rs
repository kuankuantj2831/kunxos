//! Root Manifest：版本链根，双槽交替写。
//!
//! Phase 1 Step 1 中 Ed25519 签名区为不透明字节（依赖白名单不含
//! ed25519 实现）；`from_bytes` 校验 CRC 与 magic，签名真伪由
//! kunxsec 集成步骤负责。

use super::{crc32_skip_field, get_array, get_u32, get_u64, put_array, put_u32, put_u64};
use crate::error::KosError;

/// Root Manifest magic：ASCII `"KOSROOT\0"` 按小端 u64 解释。
pub const MANIFEST_MAGIC: u64 = u64::from_le_bytes(*b"KOSROOT\0");
/// 结构固定大小（字节）：188 字节字段 + 4 字节显式尾部补齐。
///
/// 磁盘上 Manifest 占据一个完整 4096 字节槽（`root_slot_a/b` 所指
/// 块），本结构序列化后置于槽首；块内包装属后续步骤。
pub const MANIFEST_SIZE: usize = 192;

/// `crc32` 字段在映像内的字节偏移。
const OFF_CRC: usize = 184;

/// Root Manifest：命名空间与版本链的受信根。
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KosRootManifest {
    /// 必须等于 [`MANIFEST_MAGIC`]（`"KOSROOT\0"`）。
    pub magic: u64,
    /// 单调递增代数。崩溃恢复时取「generation 更大且验证通过」的槽。
    pub generation: u64,
    /// 提交时间戳。
    pub timestamp: u64,
    /// 命名空间 B-tree 根哈希。
    pub namespace_root_hash: [u8; 32],
    /// 父代 manifest 哈希（版本链回溯；创世代为全零）。
    pub parent_manifest_hash: [u8; 32],
    /// 签名者 Ed25519 公钥。
    pub signer_pk: [u8; 32],
    /// Ed25519 签名，覆盖 generation + timestamp + 两个哈希字段。
    pub signature: [u8; 64],
    /// 结构 CRC32（覆盖全部 192 字节，本字段自身不参与计算）。
    pub crc32: u32,
    /// 显式尾部补齐，保证 `repr(C)` 大小恰为 192。
    pub reserved: [u8; 4],
}

impl KosRootManifest {
    /// 结构大小（字节）。
    pub const SIZE: usize = MANIFEST_SIZE;

    /// 创世代（无父代、未签名）的占位 manifest，测试用。
    pub fn genesis(namespace_root_hash: [u8; 32], timestamp: u64) -> Self {
        KosRootManifest {
            magic: MANIFEST_MAGIC,
            generation: 1,
            timestamp,
            namespace_root_hash,
            parent_manifest_hash: [0u8; 32],
            signer_pk: [0u8; 32],
            signature: [0u8; 64],
            crc32: 0,
            reserved: [0u8; 4],
        }
    }

    /// 序列化为 192 字节映像（小端，CRC 重算后写入）。
    pub fn to_bytes(&self) -> [u8; MANIFEST_SIZE] {
        let mut b = [0u8; MANIFEST_SIZE];
        put_u64(&mut b, 0, self.magic);
        put_u64(&mut b, 8, self.generation);
        put_u64(&mut b, 16, self.timestamp);
        put_array(&mut b, 24, &self.namespace_root_hash);
        put_array(&mut b, 56, &self.parent_manifest_hash);
        put_array(&mut b, 88, &self.signer_pk);
        put_array(&mut b, 120, &self.signature);
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b[188..192].copy_from_slice(&self.reserved);
        b
    }

    /// 从字节映像解析。校验：长度、CRC、magic。签名区本步骤不验。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != MANIFEST_SIZE {
            return Err(KosError::BufferTooShort {
                need: MANIFEST_SIZE,
                got: bytes.len(),
            });
        }
        let stored = get_u32(bytes, OFF_CRC);
        let computed = crc32_skip_field(bytes, OFF_CRC, 4);
        if stored != computed {
            return Err(KosError::CrcMismatch {
                context: "root-manifest",
                expected: stored,
                actual: computed,
            });
        }
        let magic = get_u64(bytes, 0);
        if magic != MANIFEST_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "root-manifest",
            });
        }
        Ok(KosRootManifest {
            magic,
            generation: get_u64(bytes, 8),
            timestamp: get_u64(bytes, 16),
            namespace_root_hash: get_array(bytes, 24),
            parent_manifest_hash: get_array(bytes, 56),
            signer_pk: get_array(bytes, 88),
            signature: get_array(bytes, 120),
            crc32: stored,
            reserved: get_array(bytes, 188),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> KosRootManifest {
        let mut m = KosRootManifest::genesis([9u8; 32], 1_758_000_000);
        m.generation = 42;
        m.parent_manifest_hash = [1u8; 32];
        m.signer_pk = [2u8; 32];
        m.signature = [3u8; 64];
        m
    }

    #[test]
    fn size_matches_design() {
        assert_eq!(core::mem::size_of::<KosRootManifest>(), KosRootManifest::SIZE);
        assert_eq!(KosRootManifest::SIZE, 192);
    }

    #[test]
    fn roundtrip() {
        let img = sample().to_bytes();
        assert_eq!(&img[0..8], b"KOSROOT\0");
        let parsed = KosRootManifest::from_bytes(&img).unwrap();
        assert_eq!(parsed.generation, 42);
        assert_eq!(parsed.signer_pk, [2u8; 32]);
        assert_eq!(parsed.signature, [3u8; 64]);
        assert_eq!(parsed.to_bytes(), img);
    }

    #[test]
    fn reject_wrong_length_and_crc() {
        let img = sample().to_bytes();
        assert!(matches!(
            KosRootManifest::from_bytes(&img[..191]).unwrap_err(),
            KosError::BufferTooShort { .. }
        ));
        let mut bad = img;
        bad[100] ^= 0x80; // signature 区
        assert!(matches!(
            KosRootManifest::from_bytes(&bad).unwrap_err(),
            KosError::CrcMismatch { .. }
        ));
    }
}
