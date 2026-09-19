//! 命名空间节点：路径段 → 对象哈希 的 B-tree 节点格式。
//!
//! Phase 1 Step 1 只定义节点字节格式与序列化；B-tree 的插入、
//! 分裂、路径解析、Merkle 链验证属 Phase 2。
//!
//! 注意：设计文档未给出内部节点如何携带子节点指针（哈希），
//! 本结构按设计原文只含 `entries`；该问题列入未决问题。

use super::{
    crc32_skip_field, get_array, get_u16, get_u32, put_array, put_u16, put_u32, KOS_BLOCK_SIZE,
};
use crate::error::KosError;

/// 节点 magic：ASCII `"KNS\0"` 按小端 u32 解释。
pub const NAMESPACE_NODE_MAGIC: u32 = u32::from_le_bytes(*b"KNS\0");
/// 节点固定大小（等于一个块）。
pub const NAMESPACE_NODE_SIZE: usize = KOS_BLOCK_SIZE;
/// 单个 entry 的字节数。
pub const NAMESPACE_ENTRY_SIZE: usize = 52;
/// 每节点最大 entry 数：(4096 - 8 字节头 - 4 字节 CRC) / 52 = 78。
pub const NAMESPACE_MAX_ENTRIES: usize = 78;

/// `crc32` 字段在映像内的字节偏移。
const OFF_CRC: usize = 8 + NAMESPACE_MAX_ENTRIES * NAMESPACE_ENTRY_SIZE; // 4064

/// 节点类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NodeType {
    /// 叶子节点：entries 全部为路径段 → 对象。
    Leaf = 1,
    /// 内部节点（子指针表示方式未定，见模块文档）。
    Internal = 2,
}

impl NodeType {
    /// 数值形式。
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    /// 从磁盘数值解析。
    pub fn from_u8(v: u8) -> Result<Self, KosError> {
        match v {
            1 => Ok(NodeType::Leaf),
            2 => Ok(NodeType::Internal),
            _ => Err(KosError::UnknownNodeType(v)),
        }
    }
}

/// 条目类型（命名空间视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryType {
    /// 普通文件。
    File = 1,
    /// 目录。
    Dir = 2,
    /// 符号链接。
    Symlink = 3,
}

impl EntryType {
    /// 数值形式。
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    /// 从磁盘数值解析。
    pub fn from_u8(v: u8) -> Result<Self, KosError> {
        match v {
            1 => Ok(EntryType::File),
            2 => Ok(EntryType::Dir),
            3 => Ok(EntryType::Symlink),
            _ => Err(KosError::UnknownEntryType(v)),
        }
    }
}

/// 命名空间条目（52 字节）。本身无 CRC：篡改检测由所在节点的
/// CRC 承担（`from_bytes` 只校验长度与 entry_type 合法性）。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceEntry {
    /// 路径段哈希 `BLAKE3-128(segment)`，用于排序与查找。
    pub name_hash: [u8; 16],
    /// 指向目标对象的 32 字节哈希。
    pub obj_hash: [u8; 32],
    /// [`EntryType`] 数值形式。
    pub entry_type: u8,
    /// 显式对齐填充。
    pub _pad: u8,
    /// POSIX 风格权限位。
    pub mode: u16,
}

impl NamespaceEntry {
    /// 全零条目（entry_type=0，仅作未用槽位的规范填充，不可解析）。
    pub const ZERO: NamespaceEntry = NamespaceEntry {
        name_hash: [0u8; 16],
        obj_hash: [0u8; 32],
        entry_type: 0,
        _pad: 0,
        mode: 0,
    };

    /// 结构大小（字节）。
    pub const SIZE: usize = NAMESPACE_ENTRY_SIZE;

    /// 构造条目。
    pub fn new(name_hash: [u8; 16], obj_hash: [u8; 32], entry_type: EntryType, mode: u16) -> Self {
        NamespaceEntry {
            name_hash,
            obj_hash,
            entry_type: entry_type.as_u8(),
            _pad: 0,
            mode,
        }
    }

    /// 序列化为 52 字节（写入调用方提供的缓冲区）。
    pub(crate) fn write_to(&self, buf: &mut [u8], off: usize) {
        put_array(buf, off, &self.name_hash);
        put_array(buf, off + 16, &self.obj_hash);
        buf[off + 48] = self.entry_type;
        buf[off + 49] = self._pad;
        put_u16(buf, off + 50, self.mode);
    }

    /// 从缓冲区偏移处读取 52 字节。
    pub(crate) fn read_from(buf: &[u8], off: usize) -> Self {
        NamespaceEntry {
            name_hash: get_array(buf, off),
            obj_hash: get_array(buf, off + 16),
            entry_type: buf[off + 48],
            _pad: buf[off + 49],
            mode: get_u16(buf, off + 50),
        }
    }

    /// 序列化为 52 字节数组。
    pub fn to_bytes(&self) -> [u8; NAMESPACE_ENTRY_SIZE] {
        let mut b = [0u8; NAMESPACE_ENTRY_SIZE];
        self.write_to(&mut b, 0);
        b
    }

    /// 解析 52 字节。校验：长度、entry_type。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != NAMESPACE_ENTRY_SIZE {
            return Err(KosError::BufferTooShort {
                need: NAMESPACE_ENTRY_SIZE,
                got: bytes.len(),
            });
        }
        let e = NamespaceEntry::read_from(bytes, 0);
        EntryType::from_u8(e.entry_type)?;
        Ok(e)
    }
}

/// 命名空间 B-tree 节点（4096 字节，最多 78 个 entry）。
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceNode {
    /// 必须等于 [`NAMESPACE_NODE_MAGIC`]（`"KNS\0"`）。
    pub magic: u32,
    /// [`NodeType`] 数值形式。
    pub node_type: u8,
    /// 有效 entry 数（0..=78）。
    pub entry_count: u16,
    /// 条目槽位；`entry_count` 之后的槽位必须为零。
    pub entries: [NamespaceEntry; NAMESPACE_MAX_ENTRIES],
    /// 全节点 CRC32（本字段自身不参与计算）。
    pub crc32: u32,
    /// 显式尾部补齐到 4096。
    pub reserved: [u8; 28],
}

impl NamespaceNode {
    /// 结构大小（字节）。
    pub const SIZE: usize = NAMESPACE_NODE_SIZE;

    /// 构造指定类型的空节点。未用槽位保持全零（规范形式）。
    pub fn new(node_type: NodeType) -> Self {
        NamespaceNode {
            magic: NAMESPACE_NODE_MAGIC,
            node_type: node_type.as_u8(),
            entry_count: 0,
            entries: [NamespaceEntry::ZERO; NAMESPACE_MAX_ENTRIES],
            crc32: 0,
            reserved: [0u8; 28],
        }
    }

    /// 追加一个条目；返回是否成功（节点已满则失败）。
    pub fn push_entry(&mut self, entry: NamespaceEntry) -> bool {
        if self.entry_count as usize >= NAMESPACE_MAX_ENTRIES {
            return false;
        }
        self.entries[self.entry_count as usize] = entry;
        self.entry_count += 1;
        true
    }

    /// 序列化为 4096 字节映像（小端，CRC 重算后写入）。
    pub fn to_bytes(&self) -> [u8; NAMESPACE_NODE_SIZE] {
        let mut b = [0u8; NAMESPACE_NODE_SIZE];
        put_u32(&mut b, 0, self.magic);
        b[4] = self.node_type;
        put_u16(&mut b, 6, self.entry_count);
        for (i, e) in self.entries.iter().enumerate() {
            e.write_to(&mut b, 8 + i * NAMESPACE_ENTRY_SIZE);
        }
        let crc = crc32_skip_field(&b, OFF_CRC, 4);
        put_u32(&mut b, OFF_CRC, crc);
        b[OFF_CRC + 4..NAMESPACE_NODE_SIZE].copy_from_slice(&self.reserved);
        b
    }

    /// 从字节映像解析。校验：长度、CRC、magic、node_type、entry_count。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KosError> {
        if bytes.len() != NAMESPACE_NODE_SIZE {
            return Err(KosError::BufferTooShort {
                need: NAMESPACE_NODE_SIZE,
                got: bytes.len(),
            });
        }
        let stored = get_u32(bytes, OFF_CRC);
        let computed = crc32_skip_field(bytes, OFF_CRC, 4);
        if stored != computed {
            return Err(KosError::CrcMismatch {
                context: "namespace-node",
                expected: stored,
                actual: computed,
            });
        }
        let magic = get_u32(bytes, 0);
        if magic != NAMESPACE_NODE_MAGIC {
            return Err(KosError::InvalidMagic {
                context: "namespace-node",
            });
        }
        let node_type = NodeType::from_u8(bytes[4])?;
        let entry_count = get_u16(bytes, 6);
        if entry_count as usize > NAMESPACE_MAX_ENTRIES {
            return Err(KosError::CountMismatch {
                expected: NAMESPACE_MAX_ENTRIES as u16,
                got: entry_count,
            });
        }
        // 规范形式：有效区之外的槽位必须全零
        for i in entry_count as usize..NAMESPACE_MAX_ENTRIES {
            let off = 8 + i * NAMESPACE_ENTRY_SIZE;
            if bytes[off..off + NAMESPACE_ENTRY_SIZE].iter().any(|&b| b != 0) {
                return Err(KosError::UnusedSlotNonZero { index: i as u16 });
            }
        }
        let mut entries = [NamespaceEntry::ZERO; NAMESPACE_MAX_ENTRIES];
        for (i, e) in entries.iter_mut().enumerate() {
            *e = NamespaceEntry::read_from(bytes, 8 + i * NAMESPACE_ENTRY_SIZE);
        }
        // 有效区内的 entry_type 必须合法
        for e in entries.iter().take(entry_count as usize) {
            EntryType::from_u8(e.entry_type)?;
        }
        Ok(NamespaceNode {
            magic,
            node_type: node_type.as_u8(),
            entry_count,
            entries,
            crc32: stored,
            reserved: get_array(bytes, OFF_CRC + 4),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_design() {
        assert_eq!(core::mem::size_of::<NamespaceEntry>(), NamespaceEntry::SIZE);
        assert_eq!(core::mem::size_of::<NamespaceNode>(), NamespaceNode::SIZE);
        assert_eq!(NamespaceEntry::SIZE, 52);
        assert_eq!(NamespaceNode::SIZE, 4096);
        assert_eq!(NAMESPACE_MAX_ENTRIES, 78);
    }

    #[test]
    fn node_roundtrip_full() {
        let mut node = NamespaceNode::new(NodeType::Leaf);
        for i in 0..NAMESPACE_MAX_ENTRIES as u8 {
            let e = NamespaceEntry::new(
                [i; 16],
                [i * 3 + 1; 32],
                if i % 2 == 0 { EntryType::Dir } else { EntryType::File },
                0o644,
            );
            assert!(node.push_entry(e));
        }
        assert!(!node.push_entry(NamespaceEntry::new(
            [0; 16],
            [0; 32],
            EntryType::File,
            0
        ))); // 已满
        let img = node.to_bytes();
        assert_eq!(&img[0..4], b"KNS\0");
        let parsed = NamespaceNode::from_bytes(&img).unwrap();
        assert_eq!(parsed.entry_count as usize, NAMESPACE_MAX_ENTRIES);
        assert_eq!(parsed.entries[0].name_hash, [0u8; 16]);
        assert_eq!(parsed.entries[77].obj_hash, [77 * 3 + 1; 32]);
        assert_eq!(parsed.to_bytes(), img);
    }

    #[test]
    fn entry_roundtrip_and_validation() {
        let e = NamespaceEntry::new([5; 16], [6; 32], EntryType::Symlink, 0o777);
        let img = e.to_bytes();
        assert_eq!(NamespaceEntry::from_bytes(&img).unwrap(), e);
        let mut bad = img;
        bad[48] = 9; // 非法 entry_type
        assert_eq!(
            NamespaceEntry::from_bytes(&bad).unwrap_err(),
            KosError::UnknownEntryType(9)
        );
    }

    #[test]
    fn node_reject_bad_count() {
        let mut node = NamespaceNode::new(NodeType::Internal);
        node.push_entry(NamespaceEntry::new([1; 16], [2; 32], EntryType::File, 0o600));
        node.entry_count = NAMESPACE_MAX_ENTRIES as u16 + 1; // 79
        let img = node.to_bytes(); // CRC 覆盖 79，只可能挂在 count 校验
        assert!(matches!(
            NamespaceNode::from_bytes(&img).unwrap_err(),
            KosError::CountMismatch { .. }
        ));
    }

    #[test]
    fn node_reject_unused_slot_nonzero() {
        // entry_count=0 但第 5 个槽位被塞了数据 → 非规范形式
        let mut node = NamespaceNode::new(NodeType::Leaf);
        node.entries[5].entry_type = 77;
        let img = node.to_bytes();
        assert_eq!(
            NamespaceNode::from_bytes(&img).unwrap_err(),
            KosError::UnusedSlotNonZero { index: 5 }
        );
    }
}
