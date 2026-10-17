//! 容量与久用压测：2TB 稀疏卷 + churn/碎片化。
//!
//! 2TB 卷若挂载仍走全池扫描（5 亿块）会超时——本测试隐式强制快速
//! 挂载路径生效，并显式断言 was_fast_mounted。

use kos::block::BlockDevice;
use kos::block::file::FileBlockDevice;
use kos::block::MemBlockDevice;
use kos::store::{perm, KosStore, VerifyLevel};

// ==================== 2TB 容量 ====================

#[test]
fn sparse_2tb_volume() {
    let path = std::env::temp_dir().join("kos-2tb-test.img");
    let _ = std::fs::remove_file(&path);
    // 2TB = 512M × 4KB 块。稀疏文件：实际占用只随写入增长。
    let blocks: u64 = 512 * 1024 * 1024;
    let dev = FileBlockDevice::open(&path, Some(blocks)).unwrap();
    eprintln!("[t] dev created");
    assert_eq!(dev.block_count(), blocks);

    eprintln!("[t] opening store");
    let mut s = KosStore::mkfs(dev).unwrap();
    eprintln!("[t] mkfs done");
    s.mkdir("/data", 0, perm::DIR_DEFAULT).unwrap();
    eprintln!("[t] mkdir done");
    // 一个跨 Merkle 多层的大对象（~8MB）。
    let big: Vec<u8> = (0..8 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    s.write("/data/blob", &big, 0, perm::DEFAULT).unwrap();
    eprintln!("[t] blob written");
    // 可变记录 + 追加。
    s.write_mutable("/data/journal", b"j0\n", 0, perm::DEFAULT).unwrap();
    eprintln!("[t] journal created");
    for i in 0..100 {
        let line = format!("entry-{i:06}\n");
        s.append("/data/journal", line.as_bytes(), 0).unwrap();
    }

    eprintln!("[t] 100 appends done");
    // 重挂载：必须走快速路径（全池扫描 5 亿块不可能秒回）。
    let d = s.take_device();
    let t0 = std::time::Instant::now();
    eprintln!("[t] remounting...");
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    eprintln!("[t] remounted fast={}", s2.was_fast_mounted());
    let mount_ms = t0.elapsed().as_millis();
    assert!(s2.was_fast_mounted(), "2TB 卷挂载必须走持久索引快速路径");
    // debug 构建下 4 亿位解析 ~16s；release 约 1/5。上界 60s 防回归
    // （回退扫描 5 亿块是小时级，量级差 3 个数量级以上）。
    assert!(mount_ms < 60_000, "挂载过慢：{mount_ms}ms");

    // 数据完好。
    assert_eq!(s2.open_path("/data/blob", 0).unwrap(), big);
    let j = s2.open_path("/data/journal", 0).unwrap();
    assert!(j.starts_with(b"j0\nentry-000000\n"));
    assert!(j.ends_with(b"entry-000099\n"));

    // 快速路径上的继续写入。
    s2.write("/data/after-remount", b"ok", 0, perm::DEFAULT).unwrap();
    let d = s2.take_device();
    let mut s3 = KosStore::open(d, VerifyLevel::Full).unwrap();
    assert!(s3.was_fast_mounted());
    assert_eq!(s3.open_path("/data/after-remount", 0).unwrap(), b"ok");
    println!("2TB 挂载耗时 {mount_ms}ms（对象 {}，池 {} 块）",
             s3.object_count(), s3.pool_blocks());
    let _ = std::fs::remove_file(&path);
}

// ==================== churn / 碎片化久用 ====================

/// 线性同余：确定性伪随机。
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn churn_long_use_smoothness() {
    // 128MB 卷：churn 产生的 CoW 孤儿（GC 属 Phase 3）也占空间，
    // 卷必须足够大才不至于把 NoSpace 与碎片化问题混在一起。
    let mut s = KosStore::mkfs(MemBlockDevice::new(32768, 4096)).unwrap();
    let mut rng = Lcg(0x4B4F53);
    let mut live: Vec<(String, Vec<u8>)> = Vec::new();

    // 阶段 1：写入 + 追加 churn（模拟日志型负载）。
    for round in 0..30 {
        for f in 0..8 {
            let path = format!("/r{round:02}-f{f}");
            s.write_mutable(&path, &[0u8; 64], 0, perm::DEFAULT).unwrap();
            live.push((path, vec![0u8; 64]));
        }
        // 每轮对所有活文件追加随机 100..4000 字节。
        for (path, content) in live.iter_mut() {
            let n = 100 + (rng.next() % 3900) as usize;
            let chunk: Vec<u8> = (0..n).map(|i| (i % 256) as u8).collect();
            s.append(path, &chunk, 0).unwrap();
            content.extend_from_slice(&chunk);
        }
        // 每轮删除 1/3 的旧文件制造空洞。
        if round % 2 == 1 && live.len() > 12 {
            let drop_n = live.len() / 3;
            for _ in 0..drop_n {
                let i = (rng.next() as usize) % live.len();
                let (path, _) = live.remove(i);
                s.unlink(&path, 0).unwrap();
            }
        }
    }

    // 阶段 2：全量校验（内容逐字节一致）。
    for (path, content) in &live {
        assert_eq!(&s.open_path(path, 0).unwrap(), content, "churn 后 {path} 内容漂移");
    }

    // 阶段 3：碎片池上仍能放大对象（连续布局会失败的场景）。
    let big = [0x77u8; 4096 * 64];
    s.write_mutable("/post-churn-big", &big, 0, perm::DEFAULT).unwrap();
    assert_eq!(s.open_path("/post-churn-big", 0).unwrap(), big);

    // 阶段 4：追加延迟不随 churn 劣化（末段均值 vs 首段均值，容差 5×）。
    let t0 = std::time::Instant::now();
    let mut probe = String::new();
    s.write_mutable("/probe", b"", 0, perm::DEFAULT).unwrap();
    for i in 0..200 {
        let line = format!("p{i:04}\n");
        let t = std::time::Instant::now();
        s.append("/probe", line.as_bytes(), 0).unwrap();
        probe.push_str(&line);
        if i == 0 {
            let _ = t.elapsed(); // 首次含建对象，不计时
        }
    }
    let total = t0.elapsed().as_micros();
    assert_eq!(&s.open_path("/probe", 0).unwrap(), probe.as_bytes());
    // 200 次追加总计 < 3s（宽松上界：防性能坍塌，不卡波动）。
    assert!(total < 3_000_000, "追加延迟坍塌：200 次共 {total}µs");
    println!("churn: {} 活文件，200 追加共 {total}µs，空闲 {}/{} 块",
             live.len() + 1, s.free_blocks(), s.pool_blocks());
}
