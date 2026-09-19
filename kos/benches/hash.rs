//! BLAKE3 vs SHA-256 吞吐基准（KOS 设计文档未决问题 №1 的数据依据）。

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kos::hash::{Blake3Hasher, Hasher, Sha256Hasher};
use std::hint::black_box;
use std::time::Duration;

fn bench_hash32(c: &mut Criterion) {
    let mut g = c.benchmark_group("hash32");
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(2));
    for &size in &[4096usize, 65536, 1 << 20, 8 << 20] {
        let data = vec![0xA5u8; size];
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::new("blake3", size), &data, |b, d| {
            b.iter(|| Blake3Hasher.hash32(black_box(d)))
        });
        g.bench_with_input(BenchmarkId::new("sha256", size), &data, |b, d| {
            b.iter(|| Sha256Hasher.hash32(black_box(d)))
        });
    }
    g.finish();
}

fn bench_hash128(c: &mut Criterion) {
    let mut g = c.benchmark_group("hash128");
    g.warm_up_time(Duration::from_millis(500));
    g.measurement_time(Duration::from_secs(2));
    let data = vec![0xA5u8; 1 << 20];
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("blake3/1MiB", |b| b.iter(|| Blake3Hasher.hash128(black_box(&data))));
    g.bench_function("sha256/1MiB", |b| b.iter(|| Sha256Hasher.hash128(black_box(&data))));
    g.finish();
}

criterion_group!(benches, bench_hash32, bench_hash128);
criterion_main!(benches);
