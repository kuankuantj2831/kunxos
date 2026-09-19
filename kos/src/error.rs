//! KOS 错误类型。
//!
//! 不使用 `String`，保证 `no_std` 可用；`context` 字段均为
//! `&'static str`，指明错误发生的位置/阶段。

use core::fmt;

/// KOS 所有可恢复错误的统一类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KosError {
    /// 输入缓冲区长度不等于结构固定大小。
    BufferTooShort {
        /// 期望长度。
        need: usize,
        /// 实际长度。
        got: usize,
    },
    /// magic 不匹配（struct/字段名见 context）。
    InvalidMagic {
        /// 出错的结构或阶段。
        context: &'static str,
    },
    /// CRC32 校验失败。
    CrcMismatch {
        /// 出错的结构或阶段。
        context: &'static str,
        /// 磁盘上存储的 CRC。
        expected: u32,
        /// 对读到的字节重算的 CRC。
        actual: u32,
    },
    /// 超级块 version 不受支持。
    UnsupportedVersion {
        /// 磁盘上记录的版本号。
        version: u32,
    },
    /// 超级块 block_size 不是本版本支持的 4096。
    InvalidBlockSize {
        /// 磁盘上记录的块大小。
        block_size: u32,
    },
    /// `KosObject::obj_type` 不是合法的 `ObjectType`。
    UnknownObjectType(u16),
    /// `NamespaceEntry::entry_type` 不是合法的 `EntryType`。
    UnknownEntryType(u8),
    /// `NamespaceNode::node_type` 不是合法的 `NodeType`。
    UnknownNodeType(u8),
    /// 块设备访问越界。
    BlockOutOfRange {
        /// 访问的块号。
        lba: u64,
        /// 设备总块数。
        block_count: u64,
    },
    /// 读写缓冲区长度不等于 block_size。
    InvalidBlockLen {
        /// 期望长度。
        expected: usize,
        /// 实际长度。
        got: usize,
    },
    /// Merkle 验证时 chunk 序号越界。
    ChunkIndexOutOfRange {
        /// 请求的 chunk 序号。
        index: usize,
        /// 总 chunk 数。
        total_chunks: usize,
    },
    /// chunk 长度非法（非最后块必须恰为 `DATA_CHUNK_SIZE`）。
    InvalidChunkLen {
        /// 实际长度。
        len: usize,
    },
    /// 提供的索引块路径层数与树深不符。
    PathDepthMismatch {
        /// 期望层数。
        expected: usize,
        /// 实际层数。
        got: usize,
    },
    /// 索引块 `level` 字段与所在层不符。
    LevelMismatch {
        /// 期望层号。
        expected: u16,
        /// 实际层号。
        got: u16,
    },
    /// 索引块 `index`（层内序号）不符。
    IndexMismatch {
        /// 期望层内序号。
        expected: u32,
        /// 实际序号。
        got: u32,
    },
    /// 计数字段（entry_count / count）超出合法范围。
    CountMismatch {
        /// 期望值。
        expected: u16,
        /// 实际值。
        got: u16,
    },
    /// 命名空间节点 entry_count 之后的槽位非全零（非规范形式）。
    UnusedSlotNonZero {
        /// 第一个非零的未用槽位下标。
        index: u16,
    },
    /// Merkle 哈希链在某一环节断裂。
    HashMismatch {
        /// 断裂位置（"chunk"、"index-block"、"root" 等）。
        context: &'static str,
    },
    /// 顶层索引块哈希不等于给定 root。
    RootMismatch,
}

impl fmt::Display for KosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KosError::BufferTooShort { need, got } => {
                write!(f, "缓冲区长度错误：需要 {need} 字节，得到 {got} 字节")
            }
            KosError::InvalidMagic { context } => write!(f, "{context}：magic 不匹配"),
            KosError::CrcMismatch {
                context,
                expected,
                actual,
            } => write!(f, "{context}：CRC32 不匹配（存储 {expected:#010x}，重算 {actual:#010x}）"),
            KosError::UnsupportedVersion { version } => write!(f, "不支持的超级块版本 {version}"),
            KosError::InvalidBlockSize { block_size } => write!(f, "不支持的块大小 {block_size}"),
            KosError::UnknownObjectType(v) => write!(f, "未知的对象类型 {v}"),
            KosError::UnknownEntryType(v) => write!(f, "未知的命名空间条目类型 {v}"),
            KosError::UnknownNodeType(v) => write!(f, "未知的命名空间节点类型 {v}"),
            KosError::BlockOutOfRange { lba, block_count } => {
                write!(f, "块号 {lba} 越界（设备共 {block_count} 块）")
            }
            KosError::InvalidBlockLen { expected, got } => {
                write!(f, "块缓冲区长度错误：期望 {expected}，得到 {got}")
            }
            KosError::ChunkIndexOutOfRange { index, total_chunks } => {
                write!(f, "chunk 序号 {index} 越界（共 {total_chunks} 个）")
            }
            KosError::InvalidChunkLen { len } => write!(f, "chunk 长度非法：{len}"),
            KosError::PathDepthMismatch { expected, got } => {
                write!(f, "Merkle 路径层数错误：期望 {expected}，得到 {got}")
            }
            KosError::LevelMismatch { expected, got } => {
                write!(f, "索引块 level 错误：期望 {expected}，得到 {got}")
            }
            KosError::IndexMismatch { expected, got } => {
                write!(f, "索引块层内序号错误：期望 {expected}，得到 {got}")
            }
            KosError::CountMismatch { expected, got } => write!(f, "计数字段错误：期望 {expected}，得到 {got}"),
            KosError::UnusedSlotNonZero { index } => {
                write!(f, "命名空间节点的未用槽位 {index} 非全零")
            }
            KosError::HashMismatch { context } => write!(f, "Merkle 哈希链不匹配：{context}"),
            KosError::RootMismatch => write!(f, "顶层索引块哈希与 root 不符"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for KosError {}
