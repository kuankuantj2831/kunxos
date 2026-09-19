//! # KOS — KunxOS Object Storage
//!
//! KUNXOS 的对象存储层。本 crate 是 **Phase 1 Step 1** 的交付物：
//! 只包含磁盘格式定义（纯数据结构 + 序列化）、哈希封装、Merkle 树
//! 构建/验证、以及块设备抽象。不包含 Object Pool 读写、命名空间
//! B-tree 逻辑、GC、VFS 集成。
//!
//! 设计文档见 `DESIGN.md`。
//!
//! ## 布局常量
//!
//! | 结构 | 大小（字节） | 说明 |
//! |------|--------------|------|
//! | `KosSuperblock` | 4096 | Block 0，magic `KOSV0001` |
//! | `KosObject` | 96 | 对象头，magic `KO\0\0` |
//! | `KosRootManifest` | 192 | 版本链根，magic `KOSROOT\0`（块内包装属后续步骤） |
//! | `NamespaceNode` | 4096 | 命名空间 B-tree 节点，最多 78 个 entry |
//! | `NamespaceEntry` | 52 | 节点内条目（本身无 CRC，由所在节点 CRC 覆盖） |
//! | `MerkleIndexBlock` | 4096 | 宽树索引块，127 个 32 字节哈希 |
//!
//! 所有多字节字段均为小端序（RISC-V RV64 与 ARM64 主机字节序一致，
//! 磁盘格式与 CPU 架构解耦）。
//! 每个带 CRC 字段的结构：`to_bytes` 重算 CRC，`from_bytes` 校验
//! CRC（以及 magic / 枚举合法性），任何单字节篡改都会使 `from_bytes`
//! 返回 `Err`。
//!
//! ## no_std
//!
//! `default` feature 开启 `std`；`cargo build --no-default-features`
//! 时整个 crate 以 `no_std` 编译（通过 `alloc` 使用 `Vec`）。
//!
//! ```
//! use kos::hash::Blake3Hasher;
//! use kos::merkle::{verify_chunk, MerkleTree};
//!
//! let hasher = Blake3Hasher;
//! let data = vec![0xABu8; 100_000];
//! let tree = MerkleTree::build(&data, &hasher);
//! let chunk = &data[0..kos::merkle::DATA_CHUNK_SIZE];
//! let path: Vec<_> = tree.path_for_chunk(0).into_iter().cloned().collect();
//! assert!(verify_chunk(&tree.root_hash(), tree.chunk_count(), 0, chunk, &path, &hasher).is_ok());
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
// 全库禁止 unsafe：所有 repr(C) 结构的序列化均按字段逐一小端读写，
// 不做 transmute，因此布局只由显式偏移常量决定。
#![forbid(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;

pub mod block;
pub mod error;
pub mod format;
pub mod hash;
pub mod merkle;

pub use error::KosError;
