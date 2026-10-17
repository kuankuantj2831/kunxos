//! Merkle 宽树构建。

use crate::format::merkle::{MerkleIndexBlock, MERKLE_FANOUT};
use crate::hash::Hasher;
use crate::merkle::{ceil_div, DATA_CHUNK_SIZE};
use alloc::vec::Vec;

/// 一棵已构建的 Merkle 宽树：root 哈希 + 各层索引块。
#[derive(Debug, Clone)]
pub struct MerkleTree {
    root: [u8; 32],
    chunk_count: usize,
    levels: Vec<Vec<MerkleIndexBlock>>,
}

impl MerkleTree {
    /// 从数据构建：按 4KB 分块、逐块哈希、每 127 个哈希一层索引块，
    /// 逐层向上直到单块。
    ///
    /// - 最后一个 chunk 可以短于 4KB（按实际长度哈希）；
    /// - 空数据（0 chunk）产生一层：一个 `count=0` 的索引块；
    /// - root = 顶层索引块**序列化字节**的哈希（含头与 CRC）。
    pub fn build(data: &[u8], hasher: &dyn Hasher) -> Self {
        let chunk_count = ceil_div(data.len(), DATA_CHUNK_SIZE);
        let mut cur: Vec<[u8; 32]> = (0..chunk_count)
            .map(|i| {
                let start = i * DATA_CHUNK_SIZE;
                let end = core::cmp::min(start + DATA_CHUNK_SIZE, data.len());
                hasher.hash32(&data[start..end])
            })
            .collect();
        let mut levels: Vec<Vec<MerkleIndexBlock>> = Vec::new();
        loop {
            let level = levels.len() as u16;
            let mut blocks = Vec::new();
            if cur.is_empty() {
                blocks.push(MerkleIndexBlock::new(level, 0, &[]));
            } else {
                for (bi, group) in cur.chunks(MERKLE_FANOUT).enumerate() {
                    blocks.push(MerkleIndexBlock::new(level, bi as u32, group));
                }
            }
            cur = blocks.iter().map(|b| hasher.hash32(&b.to_bytes())).collect();
            levels.push(blocks);
            if cur.len() == 1 {
                break;
            }
        }
        let root = cur[0];
        MerkleTree {
            root,
            chunk_count,
            levels,
        }
    }

    /// 从既有叶子哈希列表构建（追加写路径：旧 chunk 哈希来自磁盘上的
    /// 第 0 层索引块，新 chunk 哈希现算，合流后重建整棵树）。
    pub fn build_from_chunk_hashes(chunk_hashes: &[[u8; 32]], hasher: &dyn Hasher) -> Self {
        let chunk_count = chunk_hashes.len();
        let mut cur: Vec<[u8; 32]> = chunk_hashes.to_vec();
        let mut levels: Vec<Vec<MerkleIndexBlock>> = Vec::new();
        loop {
            let level = levels.len() as u16;
            let mut blocks = Vec::new();
            if cur.is_empty() {
                blocks.push(MerkleIndexBlock::new(level, 0, &[]));
            } else {
                for (bi, group) in cur.chunks(MERKLE_FANOUT).enumerate() {
                    blocks.push(MerkleIndexBlock::new(level, bi as u32, group));
                }
            }
            cur = blocks.iter().map(|b| hasher.hash32(&b.to_bytes())).collect();
            levels.push(blocks);
            if cur.len() == 1 {
                break;
            }
        }
        let root = cur[0];
        MerkleTree {
            root,
            chunk_count,
            levels,
        }
    }

    /// 根哈希（顶层索引块序列化字节的哈希）。
    pub fn root_hash(&self) -> [u8; 32] {
        self.root
    }

    /// 数据 chunk 总数。
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// 索引块层数（恒 ≥ 1）。
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// 全部索引块数。
    pub fn index_block_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }

    /// 按层访问索引块切片。
    pub fn level(&self, level: usize) -> Option<&[MerkleIndexBlock]> {
        self.levels.get(level).map(|v| v.as_slice())
    }

    /// 展平遍历全部索引块（level 0 在前，层内按序）。
    pub fn index_blocks(&self) -> impl Iterator<Item = &MerkleIndexBlock> {
        self.levels.iter().flatten()
    }

    /// 第 `chunk` 个数据块的自底向上验证路径：
    /// `path[0]` 为直接覆盖该 chunk 的 level-0 块，最后一项为顶层块。
    ///
    /// `chunk` 越界时 panic（与切片索引同语义）。
    pub fn path_for_chunk(&self, chunk: usize) -> Vec<&MerkleIndexBlock> {
        assert!(chunk < self.chunk_count, "chunk 序号越界");
        let mut path = Vec::new();
        let mut idx = chunk;
        for level_blocks in &self.levels {
            path.push(&level_blocks[idx / MERKLE_FANOUT]);
            idx /= MERKLE_FANOUT;
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{Blake3Hasher, Sha256Hasher};

    #[test]
    fn empty_data_single_count0_block() {
        let t = MerkleTree::build(&[], &Blake3Hasher);
        assert_eq!(t.chunk_count(), 0);
        assert_eq!(t.level_count(), 1);
        assert_eq!(t.level(0).unwrap()[0].count, 0);
    }

    #[test]
    fn level_shape_matches_geometry() {
        for n_chunks in [1usize, 127, 128, 255, 1000] {
            let data = vec![0x5Au8; n_chunks * DATA_CHUNK_SIZE];
            let t = MerkleTree::build(&data, &Blake3Hasher);
            assert_eq!(t.chunk_count(), n_chunks);
            let want = crate::merkle::level_block_counts(n_chunks);
            assert_eq!(t.level_count(), want.len());
            for (k, &blocks) in want.iter().enumerate() {
                assert_eq!(t.level(k).unwrap().len(), blocks, "n={n_chunks} level={k}");
            }
        }
    }

    #[test]
    fn root_depends_on_hasher_and_data() {
        let d = b"same bytes";
        let a = MerkleTree::build(d, &Blake3Hasher);
        let b = MerkleTree::build(d, &Sha256Hasher);
        assert_ne!(a.root_hash(), b.root_hash());
        let mut d2 = *d;
        d2[0] ^= 1;
        let c = MerkleTree::build(&d2, &Blake3Hasher);
        assert_ne!(a.root_hash(), c.root_hash());
    }

    #[test]
    fn path_depth_covers_all_levels() {
        // 128 chunks → 两层
        let data = vec![1u8; 128 * DATA_CHUNK_SIZE];
        let t = MerkleTree::build(&data, &Blake3Hasher);
        for c in [0, 126, 127] {
            let path = t.path_for_chunk(c);
            assert_eq!(path.len(), 2);
            assert_eq!(path[0].level, 0);
            assert_eq!(path[1].level, 1);
            assert_eq!(path[0].index as usize, c / 127);
        }
    }
}
