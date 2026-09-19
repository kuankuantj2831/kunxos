//! 集成测试：Merkle 树构建/验证全矩阵（含 1 块、非满块、每层不满、
//! 三层大树），双哈希算法。

use kos::hash::{Blake3Hasher, Hasher, Sha256Hasher};
use kos::merkle::{verify_all, verify_chunk, MerkleTree, DATA_CHUNK_SIZE};

/// 与位置相关的伪随机填充：任何两个 chunk 内容都不同，
/// 「拿错块也能通过」类错误必然暴露。
fn make_data(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    for (i, chunk) in v.chunks_mut(8).enumerate() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state = state.wrapping_add(i as u64);
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    v
}

fn chunk_slice(data: &[u8], c: usize) -> &[u8] {
    let start = c * DATA_CHUNK_SIZE;
    let end = usize::min(start + DATA_CHUNK_SIZE, data.len());
    &data[start..end]
}

fn hashers() -> [(&'static str, &'static dyn Hasher); 2] {
    [("blake3", &Blake3Hasher), ("sha256", &Sha256Hasher)]
}

#[test]
fn build_verify_matrix() {
    // 字节数覆盖：空、1B、非满单块、整块、整块+1B、两个整块、
    // 恰满一层(127 块)、跨层(128 块)、层内不满(255 块)、多层(1000 块)
    let sizes: Vec<usize> = vec![
        0,
        1,
        DATA_CHUNK_SIZE - 1,
        DATA_CHUNK_SIZE,
        DATA_CHUNK_SIZE + 1,
        2 * DATA_CHUNK_SIZE,
        127 * DATA_CHUNK_SIZE,
        128 * DATA_CHUNK_SIZE,
        255 * DATA_CHUNK_SIZE,
        1000 * DATA_CHUNK_SIZE,
    ];
    for &size in &sizes {
        let data = make_data(size);
        let total_chunks = size.div_ceil(DATA_CHUNK_SIZE);
        for (name, h) in hashers() {
            let tree = MerkleTree::build(&data, h);
            assert_eq!(tree.chunk_count(), total_chunks, "{name} size={size}");
            // 全量验证
            verify_all(&tree.root_hash(), &data, h).unwrap();
            // 逐 chunk 路径验证
            for c in 0..total_chunks {
                let path: Vec<_> = tree.path_for_chunk(c).into_iter().cloned().collect();
                verify_chunk(&tree.root_hash(), total_chunks, c, chunk_slice(&data, c), &path, h)
                    .unwrap_or_else(|e| panic!("{name} size={size} chunk={c}: {e}"));
            }
        }
    }
}

/// 三层树：127*127+1 = 16130 chunks → 层数 [128, 2, 1]，
/// 每层都存在不满的块（宽树「每层不满」边界）。
#[test]
fn three_level_partially_filled_tree() {
    let chunks = 127 * 127 + 1;
    let data = make_data(chunks * DATA_CHUNK_SIZE);
    let h = &Blake3Hasher;
    let tree = MerkleTree::build(&data, h);
    assert_eq!(tree.level_count(), 3);
    assert_eq!(tree.level(0).unwrap().len(), 128);
    assert_eq!(tree.level(1).unwrap().len(), 2);
    assert_eq!(tree.level(2).unwrap().len(), 1);
    // 层尾块的 count 不满
    assert_eq!(tree.level(0).unwrap()[127].count, 1); // 16130 - 127*127 = 1
    assert_eq!(tree.level(1).unwrap()[1].count, 1); // 128 - 127 = 1
    verify_all(&tree.root_hash(), &data, h).unwrap();
    for &c in &[0, 63, 8192, 16128, 16129] {
        let path: Vec<_> = tree.path_for_chunk(c).into_iter().cloned().collect();
        verify_chunk(&tree.root_hash(), chunks, c, chunk_slice(&data, c), &path, h)
            .unwrap_or_else(|e| panic!("chunk={c}: {e}"));
    }
}

#[test]
fn any_byte_flip_in_data_breaks_verification() {
    let data = make_data(10 * DATA_CHUNK_SIZE);
    let h = &Blake3Hasher;
    let tree = MerkleTree::build(&data, h);
    for &pos in &[0usize, 4095, 5000, 40959] {
        let mut bad = data.clone();
        bad[pos] ^= 0x01;
        // 全量验证失败
        assert!(verify_all(&tree.root_hash(), &bad, h).is_err(), "pos={pos}");
        // 所在 chunk 的路径验证失败
        let c = pos / DATA_CHUNK_SIZE;
        let path: Vec<_> = tree.path_for_chunk(c).into_iter().cloned().collect();
        assert!(
            verify_chunk(&tree.root_hash(), 10, c, chunk_slice(&bad, c), &path, h).is_err(),
            "pos={pos}"
        );
    }
}

#[test]
fn swapping_chunks_breaks_verification() {
    let mut data = make_data(2 * DATA_CHUNK_SIZE);
    let h = &Blake3Hasher;
    let tree = MerkleTree::build(&data, h);
    // 交换两个不同内容的 chunk
    let (a, b) = data.split_at_mut(DATA_CHUNK_SIZE);
    a.swap_with_slice(&mut b[..DATA_CHUNK_SIZE]);
    for &c in &[0, 1] {
        let path: Vec<_> = tree.path_for_chunk(c).into_iter().cloned().collect();
        assert!(verify_chunk(&tree.root_hash(), 2, c, chunk_slice(&data, c), &path, h).is_err());
    }
}

#[test]
fn empty_data_root_is_well_defined() {
    // 空数据：一层 count=0 索引块；root 定义无歧义且可验证
    for (name, h) in hashers() {
        let tree = MerkleTree::build(&[], h);
        assert_eq!(tree.chunk_count(), 0);
        assert_eq!(tree.level_count(), 1);
        verify_all(&tree.root_hash(), &[], h).unwrap();
        let _ = name;
    }
}

#[test]
fn index_blocks_serialize_and_reparse() {
    // 树中每个索引块都能独立 to_bytes/from_bytes 往返
    let data = make_data(300 * DATA_CHUNK_SIZE);
    let tree = MerkleTree::build(&data, &Blake3Hasher);
    let n = tree.index_block_count();
    assert!(n >= 4);
    for blk in tree.index_blocks() {
        let img = blk.to_bytes();
        let parsed = kos::format::merkle::MerkleIndexBlock::from_bytes(&img).unwrap();
        assert_eq!(parsed.to_bytes(), img);
    }
}
