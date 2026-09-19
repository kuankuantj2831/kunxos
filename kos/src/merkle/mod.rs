//! Merkle 宽树：构建（`build`）与验证（`verify`）。
//!
//! 树形：数据按 4KB 分块 → 每块单独哈希 → 每 127 个哈希装进一个
//! 4096 字节索引块 → 索引块序列化字节再哈希、再分组，逐层向上，
//! 直至单块；root = 顶层索引块序列化字节的哈希。
//!
//! 几何不变量（构建与验证共用，[`level_block_counts`] 是唯一定义）：
//! `counts[k]` 为第 k 层索引块数；第 k 层块 `b` 的 `count` 字段
//! 必须等于 `min(127, children(k) - b*127)`，其中 `children(0)`
//! 为 chunk 总数、`children(k) = counts[k-1]`。

pub mod build;
pub mod verify;

pub use build::MerkleTree;
pub use verify::{verify_all, verify_chunk};

use alloc::vec::Vec;

/// 数据分块大小（字节），与全局块大小一致。
pub const DATA_CHUNK_SIZE: usize = crate::format::KOS_BLOCK_SIZE;

/// 上取整除法（`b` 恒非零）。
pub(crate) fn ceil_div(a: usize, b: usize) -> usize {
    (a + b - 1) / b
}

/// 给定 chunk 总数，返回每层索引块数。恒至少一层：0 个 chunk 也有
/// 一个 `count=0` 的顶层块（空数据根哈希无歧义）。
pub(crate) fn level_block_counts(total_chunks: usize) -> Vec<usize> {
    let fanout = crate::format::merkle::MERKLE_FANOUT;
    let mut counts = Vec::new();
    let mut cur = total_chunks.max(1);
    loop {
        let blocks = ceil_div(cur, fanout).max(1);
        counts.push(blocks);
        if blocks == 1 {
            break;
        }
        cur = blocks;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry() {
        assert_eq!(level_block_counts(0), vec![1]); // 空数据：一层 count=0
        assert_eq!(level_block_counts(1), vec![1]); // 单块
        assert_eq!(level_block_counts(127), vec![1]); // 恰好一层满
        assert_eq!(level_block_counts(128), vec![2, 1]); // 两层，层0不满
        assert_eq!(level_block_counts(255), vec![3, 1]); // 127+127+1
        assert_eq!(level_block_counts(1000), vec![8, 1]);
        // 127*127+1：三层，每层都有不满的块
        assert_eq!(level_block_counts(127 * 127 + 1), vec![128, 2, 1]);
    }

    #[test]
    fn ceil() {
        assert_eq!(ceil_div(0, 127), 0);
        assert_eq!(ceil_div(1, 127), 1);
        assert_eq!(ceil_div(127, 127), 1);
        assert_eq!(ceil_div(128, 127), 2);
        assert_eq!(ceil_div(16130, 127), 128);
    }
}
