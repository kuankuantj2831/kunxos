//! Merkle 宽树验证。
//!
//! `verify_chunk`：给定 root、chunk 内容及其自底向上索引块路径，
//! 验证该 chunk 确实属于 root 所指的树（归属证明）。
//!
//! `verify_all`：对整段数据重建树并比对 root（全量验证，测试与
//! 低频审计用）。

use crate::error::KosError;
use crate::format::merkle::{MerkleIndexBlock, MERKLE_FANOUT};
use crate::hash::Hasher;
use crate::merkle::build::MerkleTree;
use crate::merkle::{level_block_counts, DATA_CHUNK_SIZE};

/// 验证 `chunk_index` 处的数据块属于 `root` 指向的 Merkle 树。
///
/// `path` 为自底向上的索引块序列（`MerkleTree::path_for_chunk`
/// 的产物），长度必须等于树深。除哈希链外还逐块核对 CRC、层号、
/// 层内序号与计数，防御手工构造的「几何合法但内容伪造」路径。
///
/// chunk 长度规则：非最后块必须恰为 [`DATA_CHUNK_SIZE`]；最后块
/// 为 1..=4096 字节（精确长度需整卷元数据才能判定，此处不做强假设）。
pub fn verify_chunk(
    root: &[u8; 32],
    total_chunks: usize,
    chunk_index: usize,
    chunk: &[u8],
    path: &[MerkleIndexBlock],
    hasher: &dyn Hasher,
) -> Result<(), KosError> {
    if chunk_index >= total_chunks {
        return Err(KosError::ChunkIndexOutOfRange {
            index: chunk_index,
            total_chunks,
        });
    }
    let counts = level_block_counts(total_chunks);
    let depth = counts.len();
    if path.len() != depth {
        return Err(KosError::PathDepthMismatch {
            expected: depth,
            got: path.len(),
        });
    }
    if chunk_index + 1 < total_chunks {
        if chunk.len() != DATA_CHUNK_SIZE {
            return Err(KosError::InvalidChunkLen { len: chunk.len() });
        }
    } else if chunk.len() == 0 || chunk.len() > DATA_CHUNK_SIZE {
        return Err(KosError::InvalidChunkLen { len: chunk.len() });
    }

    // 单遍自底向上：几何 + CRC 校验，同时滚动持有「已验证的子块
    // 映像」用于哈希链，每块只序列化一次
    let mut child_img: Option<[u8; 4096]> = None;
    let chunk_hash = hasher.hash32(chunk);
    let mut idx = chunk_index; // 本层孩子的序号（level 0 为 chunk 序号）
    for (k, blk) in path.iter().enumerate() {
        let expected_index = idx / MERKLE_FANOUT;
        let children_total = if k == 0 { total_chunks } else { counts[k - 1] };
        let expected_count = core::cmp::min(
            MERKLE_FANOUT,
            children_total.saturating_sub(expected_index * MERKLE_FANOUT),
        );
        if blk.level != k as u16 {
            return Err(KosError::LevelMismatch {
                expected: k as u16,
                got: blk.level,
            });
        }
        if blk.index as usize != expected_index {
            return Err(KosError::IndexMismatch {
                expected: expected_index as u32,
                got: blk.index,
            });
        }
        if blk.count as usize != expected_count {
            return Err(KosError::CountMismatch {
                expected: expected_count as u16,
                got: blk.count,
            });
        }
        let img = blk.verified_image()?;
        match k {
            0 => {
                // 数据哈希 → level-0 块
                let slot0 = chunk_index % MERKLE_FANOUT;
                if chunk_hash != blk.hashes[slot0] {
                    return Err(KosError::HashMismatch { context: "chunk" });
                }
            }
            _ => {
                // 子块映像哈希 → 本块对应槽位
                let slot = idx % MERKLE_FANOUT;
                if hasher.hash32(child_img.as_ref().unwrap()) != blk.hashes[slot] {
                    return Err(KosError::HashMismatch {
                        context: "index-block",
                    });
                }
            }
        }
        idx = expected_index;
        child_img = Some(img);
    }
    // 顶层块映像哈希 == root
    if &hasher.hash32(child_img.as_ref().unwrap()) != root {
        return Err(KosError::RootMismatch);
    }
    Ok(())
}

/// 全量验证：对 `data` 重建 Merkle 树，比对 root。相等返回 `Ok(())`。
pub fn verify_all(root: &[u8; 32], data: &[u8], hasher: &dyn Hasher) -> Result<(), KosError> {
    let tree = MerkleTree::build(data, hasher);
    if tree.root_hash() != *root {
        return Err(KosError::RootMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{Blake3Hasher, Sha256Hasher};
    use alloc::vec::Vec;

    fn owned_path(t: &MerkleTree, c: usize) -> Vec<MerkleIndexBlock> {
        t.path_for_chunk(c).into_iter().cloned().collect()
    }

    #[test]
    fn verify_ok_both_hashers() {
        for h in [&Blake3Hasher as &dyn Hasher, &Sha256Hasher] {
            let data = vec![0xC3u8; 3 * DATA_CHUNK_SIZE + 123];
            let t = MerkleTree::build(&data, h);
            assert!(verify_all(&t.root_hash(), &data, h).is_ok());
            for c in [0, 1, 2, 3] {
                let start = c * DATA_CHUNK_SIZE;
                let end = core::cmp::min(start + DATA_CHUNK_SIZE, data.len());
                assert!(verify_chunk(
                    &t.root_hash(),
                    t.chunk_count(),
                    c,
                    &data[start..end],
                    &owned_path(&t, c),
                    h
                )
                .is_ok());
            }
        }
    }

    #[test]
    fn tampered_chunk_rejected() {
        let h = &Blake3Hasher;
        let mut data = vec![0x11u8; DATA_CHUNK_SIZE];
        let t = MerkleTree::build(&data, h);
        data[4095] ^= 0x01;
        assert!(matches!(
            verify_chunk(&t.root_hash(), 1, 0, &data, &owned_path(&t, 0), h).unwrap_err(),
            KosError::HashMismatch { .. }
        ));
    }

    #[test]
    fn tampered_index_block_rejected() {
        let h = &Blake3Hasher;
        let data = vec![0x22u8; 300 * DATA_CHUNK_SIZE];
        let t = MerkleTree::build(&data, h);
        let mut path = owned_path(&t, 5);
        path[0].hashes[5][0] ^= 0x01; // 伪造 level-0 块里的哈希
        // CRC 先挂
        assert!(matches!(
            verify_chunk(&t.root_hash(), t.chunk_count(), 5, &data[5 * DATA_CHUNK_SIZE..6 * DATA_CHUNK_SIZE], &path, h).unwrap_err(),
            KosError::CrcMismatch { .. }
        ));
    }

    #[test]
    fn geometry_checks_reject_forged_blocks() {
        let h = &Blake3Hasher;
        // 200 chunks → counts=[2,1]，chunk 0 与 chunk 150 分属不同 level-0 块
        let data = vec![0x33u8; 200 * DATA_CHUNK_SIZE];
        let t = MerkleTree::build(&data, h);
        // 拿 chunk 0 的路径冒充 chunk 150 的
        assert!(matches!(
            verify_chunk(
                &t.root_hash(),
                t.chunk_count(),
                150,
                &data[150 * DATA_CHUNK_SIZE..151 * DATA_CHUNK_SIZE],
                &owned_path(&t, 0),
                h
            )
            .unwrap_err(),
            KosError::IndexMismatch { .. }
        ));
        // 路径截断
        let mut short = owned_path(&t, 150);
        short.pop();
        assert!(matches!(
            verify_chunk(
                &t.root_hash(),
                t.chunk_count(),
                150,
                &data[150 * DATA_CHUNK_SIZE..151 * DATA_CHUNK_SIZE],
                &short,
                h
            )
            .unwrap_err(),
            KosError::PathDepthMismatch { .. }
        ));
        // chunk 序号越界
        assert!(matches!(
            verify_chunk(
                &t.root_hash(),
                t.chunk_count(),
                200,
                &data[150 * DATA_CHUNK_SIZE..151 * DATA_CHUNK_SIZE],
                &owned_path(&t, 150),
                h
            )
            .unwrap_err(),
            KosError::ChunkIndexOutOfRange { .. }
        ));
        // 错 root
        let mut bad_root = t.root_hash();
        bad_root[0] ^= 0xFF;
        assert!(matches!(
            verify_chunk(
                &bad_root,
                t.chunk_count(),
                150,
                &data[150 * DATA_CHUNK_SIZE..151 * DATA_CHUNK_SIZE],
                &owned_path(&t, 150),
                h
            )
            .unwrap_err(),
            KosError::RootMismatch
        ));
    }
}
