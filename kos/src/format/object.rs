//! 对象头：Object Pool 中不可变对象的元数据。
//!
//! Phase 1 Step 1 只定义格式与序列化；对象头在块设备上的摆放
//! （与数据块、Merkle 索引块的相对位置）由后续步骤决定。

use super::{crc32_skip_field, get_array, get_u32, get_u64, put_array, put_u32, put_u64};
use crate::error::KosError;

/// 对象头 magic：ASCII `"KO\0\0"` 按小端 u32 解释。
pub const OBJECT_MAGIC: u32 = u32::from_le_bytes(*b"KO\0\0");
/// 对象头固定大小（字节）：92 字节字段 + 4 字节显式尾部补齐。
pub const OBJECT_HEADER_SIZE: usize = 96;

/// `header_crc` 字段在映像内的字节偏移。
const OFF_CRC: usize = 88;

/// 对象类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ObjectType {
    /// 普通文件内容。
    File = 1,
    /// 目录（目录内容本身也是对象）。
    Dir = 2,
    /// 符号链接（目标路径为内容）。
    Symlink = 3,
    /// 打包对象（小对象合并，类似 Git packfile）。
    Pack = 4,
    /// 无类型裸数据块。
    Blob = 5,
}

impl ObjectType {
    /// 数值形式。
    pub fn as_u16(self) -> u16 {
        self as u16
    }
    /// 从磁盘数值解析。
    pub fn from_u16(v: u16) -> Result<Self, KosError> {
        match v {
            1 => Ok(ObjectType::File),
            2 => Ok(ObjectType::Dir),
            3 => Ok(ObjectType::Symlink),
            4 => Ok(ObjectType::Pack),
            5 => Ok(ObjectType::Blob),
            _ => Err(KosError::UnknownObjectType(v)),
        }
    }
}

/// `flags` 位定义（按位或组合；未知位保留，解析时不拒绝）。
pub mod object_flags {
    /// 内容已压缩。
    pub const COMPRESSED: u16 = 0x0001;
    /// 内容已打包进 Pack 对象。
    pub const PACKED: u16 = 0x0002;
    /// 内容已加密。
    pub const ENCRYPTED: u16 = 0x0004;
    /// 可变记录（引擎 v2）：支持 append / write_range 原地更新；
    /// `content_hash` 存 Merkle 树根（非 SHA-256 内容哈希），不参与
    /// CAS 去重。
    pub const MUTABLE: u16 = 0x0008;
}

/// 对象头（96 字节）。
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KosObject {
    /// 必须等于 [`OBJECT_MAGIC`]（`"KO\0\0"`）。
    pub magic: u32,
    /// [`ObjectType`] 的数值形式。
    pub obj_type: u16,
    /// [`object_flags`] 位组合。
    pub flags: u16,
    /// 内容长度（字节）。
    pub content_len: u64,
    /// 整个内容的哈希（CAS 寻址/去重键；算法由所在卷的约定决定）。
    pub content_hash: [u8; 32],
    /// 属主（与 per-app UID 对齐）。
    pub owner_uid: u32,
    /// 能力位（open 时由内核检查）。
    pub capability: u32,
    /// 创建时间戳。
    pub created_at: u64,
    /// 对象版本号。
    pub version: u64,
    /// 内容占用的数据块数。
    pub block_count: u32,
    /// 显式对齐填充（设计文档 `_pad`）。
    pub _pad: u32,
    /// Merkle 间接指针：指向顶层间接索引块的块号。
    pub indirect: u64,
    /// 头部 CRC32（覆盖全部 96 字节，本字段自身不参与计算）。
    pub header_crc: u32,
    /// 显式尾部补齐，保证 `repr(C)` 大小恰为 96。
    pub reserved: [u8; 4],
}

impl KosObject {
    /// 结构大小（字节）。
    pub const SIZE: usize = OBJECT_HEADER_SIZE;

    /// 全零内容的占位对象（测试用；合法但无意义）。
    pub fn empty(obj_type: ObjectType) -> Self {
        KosObject {
            magic: OBJECT_MAGIC,
            obj_type: obj_type.as_u16(),
            flags: 0,
            content_len: 0,
            content_hash: [0u8; 32],
            owner_uid: 0,
            capability: 0,
            created_at: 0,
            version: 0,
            block_count: 0,
            _pad: 0,
            indirect: 0,
            header_crc: 0,
            reserved: [0u8; 4],
        }
    }

    /// 序列化为 96 字节映像（小端，CRC 重算后写入）。
    pub fn to_bytes(&self) -> [u8; OBJECT_HEADER_SIZE] {
        let mut b = [0u8; OBJECT_HEADER_SIZE];
        put_u32(&mut b, 0, self.magic);
        b[4..6].copy_from_slice(&self.obj_type.to_le_bytes());
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        put_u64(&mut b, 8, self.content_len);
        put_array(&mut b, 16, &self.content_hash);
        b[48..52].copy_from_slice(&self.owner_uid.to_le_bytes());
        b[52..56].copy_from_slice(&self.capability.to_le_bytes());
        put_u64(&mut b, 56, self.created_at);
        put_u64(&mut b, 64, self.version);
        b[72..76].copy_from_slice(&self.block_count.to_le_bytes());
        b[76..80].copy_from_slice(&self._pad.to_le_bytes());
        put_u64(&mut b, 80, self.indirect);
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b[92..96].copy_from_slice(&self.reserved);
        b
    }

    /// 从字节映像解析。校验：长度、CRC、magic、obj_type。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != OBJECT_HEADER_SIZE {
            return Err(KosError::BufferTooShort {
                need: OBJECT_HEADER_SIZE,
                got: bytes.len(),
            });
        }
        let stored = get_u32(bytes, OFF_CRC);
        let computed = crc32_skip_field(bytes, OFF_CRC, 4);
        if stored != computed {
            return Err(KosError::CrcMismatch {
                context: "object-header",
                expected: stored,
                actual: computed,
            });
        }
        let magic = get_u32(bytes, 0);
        if magic != OBJECT_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "object-header",
            });
        }
        let obj_type = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        ObjectType::from_u16(obj_type)?;
        Ok(KosObject {
            magic,
            obj_type,
            flags: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            content_len: get_u64(bytes, 8),
            content_hash: get_array(bytes, 16),
            owner_uid: get_u32(bytes, 48),
            capability: get_u32(bytes, 52),
            created_at: get_u64(bytes, 56),
            version: get_u64(bytes, 64),
            block_count: get_u32(bytes, 72),
            _pad: get_u32(bytes, 76),
            indirect: get_u64(bytes, 80),
            header_crc: stored,
            reserved: get_array(bytes, 92),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> KosObject {
        let mut o = KosObject::empty(ObjectType::Pack);
        o.flags = object_flags::PACKED | object_flags::COMPRESSED;
        o.content_len = 0x1_0000;
        o.content_hash = [7u8; 32];
        o.owner_uid = 1000;
        o.capability = 0o644;
        o.created_at = 1_758_000_000;
        o.version = 3;
        o.block_count = 16;
        o.indirect = 0xBEEF;
        o
    }

    #[test]
    fn size_matches_design() {
        assert_eq!(core::mem::size_of::<KosObject>(), KosObject::SIZE);
        assert_eq!(KosObject::SIZE, 96);
    }

    #[test]
    fn roundtrip() {
        let img = sample().to_bytes();
        assert_eq!(&img[0..4], b"KO\0\0");
        let parsed = KosObject::from_bytes(&img).unwrap();
        assert_eq!(parsed, {
            let mut s = sample();
            s.header_crc = parsed.header_crc; // to_bytes 重算，构造值无意义
            s
        });
        assert_eq!(parsed.to_bytes(), img);
    }

    #[test]
    fn reject_unknown_type() {
        let mut o = sample();
        o.obj_type = 999;
        let img = o.to_bytes(); // CRC 覆盖 999，因此只可能挂在类型校验
        assert_eq!(
            KosObject::from_bytes(&img).unwrap_err(),
            KosError::UnknownObjectType(999)
        );
    }

    #[test]
    fn reject_wrong_length() {
        let img = sample().to_bytes();
        assert!(matches!(
            KosObject::from_bytes(&img[..95]).unwrap_err(),
            KosError::BufferTooShort { .. }
        ));
    }
}
