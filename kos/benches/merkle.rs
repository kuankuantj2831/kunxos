//! Merkle 树构建 / 单块路径验证 / 全量验证开销基准。

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kos::hash::{Blake3Hasher, Hasher, Sha256Hasher};
use kos::merkle::{verify_all, verify_chunk, MerkleTree};
use std::hint::black_box;
use std::time::Duration;

/// 伪随机填充（xorshift64），避免全零数据的极端分支。
fn make_data(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for chunk in v.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    v
}

fn bench_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("merkle_build");
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(2));
    let hashers: [(&str, &dyn Hasher); 2] = [("blake3", &Blake3Hasher), ("sha256", &Sha256Hasher)];
    for &mib in &[1usize, 8] {
        let data = make_data(mib << 20);
        g.throughput(Throughput::Bytes(data.len() as u64));
        for (name, h) in hashers {
            g.bench_with_input(BenchmarkId::new(name, format!("{mib}MiB")), &data, |b, d| {
                b.iter(|| MerkleTree::build(black_box(d), h))
            });
        }
    }
    g.finish();
}

fn bench_verify_chunk(c: &mut Criterion) {
    let mut g = c.benchmark_group("merkle_verify_chunk");
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(2));
    let data = make_data(8 << 20);
    let h = &Blake3Hasher;
    let tree = MerkleTree::build(&data, h);
    let chunk_idx = 1234;
    let start = chunk_idx * kos::merkle::DATA_CHUNK_SIZE;
    let chunk = data[start..start + kos::merkle::DATA_CHUNK_SIZE].to_vec();
    let path: Vec<_> = tree.path_for_chunk(chunk_idx).into_iter().cloned().collect();
    let root = tree.root_hash();
    let total = tree.chunk_count();
    g.throughput(Throughput::Bytes(kos::merkle::DATA_CHUNK_SIZE as u64));
    for (name, hh) in [("blake3", &Blake3Hasher as &dyn Hasher), ("sha256", &Sha256Hasher as &dyn Hasher)] {
        g.bench_function(BenchmarkId::new(name, "8MiB/tree"), |b| {
            b.iter(|| {
                verify_chunk(
                    black_box(&root),
                    black_box(total),
                    black_box(chunk_idx),
                    black_box(&chunk),
                    black_box(&path),
                    hh,
                )
            })
        });
    }
    g.finish();
}

fn bench_verify_all(c: &mut Criterion) {
    let mut g = c.benchmark_group("merkle_verify_all");
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(2));
    let data = make_data(1 << 20);
    let root = MerkleTree::build(&data, &Blake3Hasher).root_hash();
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("blake3/1MiB", |b| {
        b.iter(|| verify_all(black_box(&root), black_box(&data), &Blake3Hasher))
    });
    g.finish();
}

criterion_group!(benches, bench_build, bench_verify_chunk, bench_verify_all);
criterion_main!(benches);
