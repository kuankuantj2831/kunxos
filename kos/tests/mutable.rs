//! 可变记录（v2 引擎）专项测试：append / write_range / 非连续分配 /
//! 重挂载一致性 / 权限。

use kos::block::MemBlockDevice;
use kos::store::{perm, KosStore, VerifyLevel};

fn dev(blocks: u64) -> MemBlockDevice {
    MemBlockDevice::new(blocks, 4096)
}

#[test]
fn append_basic_and_after_remount() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    s.write_mutable("/log", b"hello", 0, perm::DEFAULT).unwrap();
    let mut expect = b"hello".to_vec();
    for i in 0..50 {
        let chunk = format_args!("line-{i:04}\n");
        let mut buf = String::new();
        use core::fmt::Write;
        let _ = write!(buf, "{chunk}");
        s.append("/log", buf.as_bytes(), 0).unwrap();
        expect.extend_from_slice(buf.as_bytes());
    }
    let mut got = s.open_path("/log", 0).unwrap();
    assert_eq!(got, expect);

    // 重挂载（扫描式恢复依赖区段表）：内容与长度一致。
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::Full).unwrap();
    got = s2.open_path("/log", 0).unwrap();
    assert_eq!(got, expect, "重挂载后 append 内容必须逐字节一致");
}

#[test]
fn append_large_crossing_merkle_levels() {
    // 追加到 127+ chunk：跨越 Merkle 第 0 层单块（127 fanout）。
    let mut s = KosStore::mkfs(dev(16384)).unwrap();
    s.write_mutable("/big", &vec![0x11u8; 4096 * 130], 0, perm::DEFAULT)
        .unwrap();
    s.append("/big", &vec![0x22u8; 4096 * 10], 0).unwrap();
    let data = s.open_path("/big", 0).unwrap();
    assert_eq!(data.len(), 4096 * 140);
    assert!(data[..4096 * 130].iter().all(|&b| b == 0x11));
    assert!(data[4096 * 130..].iter().all(|&b| b == 0x22));
}

#[test]
fn write_range_in_place_and_extend() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    let base = vec![0xAAu8; 4096 * 3];
    s.write_mutable("/f", &base, 0, perm::DEFAULT).unwrap();
    // 原地改（跨 chunk 边界）。
    s.write_range("/f", 4090, b"XY-Z-9876", 0).unwrap();
    let data = s.open_path("/f", 0).unwrap();
    assert_eq!(&data[4090..4099], b"XY-Z-9876");
    assert_eq!(data.len(), 4096 * 3);
    // 扩容写（超出现末尾 + 空隙补零）。
    s.write_range("/f", 4096 * 3 + 100, b"TAIL", 0).unwrap();
    let data = s.open_path("/f", 0).unwrap();
    assert_eq!(data.len(), 4096 * 3 + 104);
    assert_eq!(&data[4096 * 3 + 100..], b"TAIL");
    assert_eq!(&data[4096 * 3..4096 * 3 + 100], &[0u8; 100]);
}

#[test]
fn append_rejects_immutable_and_perms() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.write("/ro", b"immutable", 0, perm::DEFAULT).unwrap();
    assert!(matches!(
        s.append("/ro", b"x", 0),
        Err(kos::KosError::NotMutable)
    ));
    // 属主私有：其他用户不可追加。
    s.write_mutable("/priv", b"s", 0, perm::OWNER_R | perm::OWNER_W)
        .unwrap();
    assert!(matches!(
        s.append("/priv", b"x", 1),
        Err(kos::KosError::PermissionDenied)
    ));
}

#[test]
fn fragmented_allocation_still_works() {
    // 交错建/删制造碎片，之后放大对象（v1 连续布局会失败的场景）。
    let mut s = KosStore::mkfs(dev(2048)).unwrap();
    for i in 0..40 {
        s.write(&format!("/f{i:02}"), &[0u8; 4096 * 2], 0, perm::DEFAULT)
            .unwrap();
    }
    for i in (0..40).step_by(2) {
        s.unlink(&format!("/f{i:02}"), 0).unwrap();
    }
    // 需要一段大空间（40 chunk）：在碎片池上必须成功。
    s.write_mutable("/big", &[0x5Au8; 4096 * 40], 0, perm::DEFAULT)
        .unwrap();
    let data = s.open_path("/big", 0).unwrap();
    assert_eq!(data.len(), 4096 * 40);
    assert!(data.iter().all(|&b| b == 0x5A));
}

#[test]
fn mutable_record_rejects_hash_collision_semantics() {
    // 两个同内容的可变记录各自独立（不做 CAS 去重）。
    let mut s = KosStore::mkfs(dev(2048)).unwrap();
    s.write_mutable("/a", b"same", 0, perm::DEFAULT).unwrap();
    s.write_mutable("/b", b"same", 0, perm::DEFAULT).unwrap();
    s.append("/a", b"1", 0).unwrap();
    assert_eq!(s.open_path("/a", 0).unwrap(), b"same1");
    assert_eq!(s.open_path("/b", 0).unwrap(), b"same");
}

#[test]
fn gc_reclaims_orphans() {
    let mut s = KosStore::mkfs(dev(8192)).unwrap();
    s.write_mutable("/log", b"v0", 0, perm::DEFAULT).unwrap();
    for i in 0..30 {
        s.append("/log", format!("chunk-{i} ").as_bytes(), 0).unwrap();
    }
    let objs_before = s.object_count();
    let free_before = s.free_blocks();
    let (dead, freed) = s.gc().unwrap();
    assert!(dead >= 30, "至少 30 代孤儿目录+索引应被清扫（实际 {dead}）");
    assert!(freed > 0);
    assert_eq!(s.object_count(), objs_before - dead);
    assert_eq!(s.free_blocks(), free_before + freed);
    // 活数据完好。
    let d = s.open_path("/log", 0).unwrap();
    assert!(d.starts_with(b"v0chunk-0 "));
    assert!(d.ends_with(b"chunk-29 "));
    // 重挂载后状态一致（持久索引反映 GC 后世界）。
    let dev = s.take_device();
    let mut s2 = KosStore::open(dev, VerifyLevel::Full).unwrap();
    assert_eq!(s2.object_count(), objs_before - dead);
    assert_eq!(s2.open_path("/log", 0).unwrap(), d);
    // GC 后继续写入分配正常。
    s2.append("/log", b"post-gc", 0).unwrap();
    assert!(s2.open_path("/log", 0).unwrap().ends_with(b"post-gc"));
}

#[test]
fn write_range_fast_path_correctness() {
    // 大文件中部多点位补丁 + 空隙扩容（全部走快速路径窗口）。
    let mut s = KosStore::mkfs(dev(8192)).unwrap();
    let base: Vec<u8> = (0..4096 * 40).map(|i| (i % 251) as u8).collect();
    s.write_mutable("/big", &base, 0, perm::DEFAULT).unwrap();
    // 三处中部补丁。
    s.write_range("/big", 100, b"PATCH-1", 0).unwrap();
    s.write_range("/big", 4096 * 20 + 55, b"PATCH-2", 0).unwrap();
    s.write_range("/big", 4096 * 39, b"P3", 0).unwrap();
    let d = s.open_path("/big", 0).unwrap();
    assert_eq!(&d[100..107], b"PATCH-1");
    assert_eq!(&d[4096 * 20 + 55..4096 * 20 + 62], b"PATCH-2");
    assert_eq!(&d[4096 * 39..4096 * 39 + 2], b"P3");
    assert_eq!(d.len(), base.len());
    assert_eq!(d[4096 * 10], base[4096 * 10]); // 未触及区域原样
    // 空隙扩容（offset 超末尾 1 chunk 内）。
    s.write_range("/big", (base.len() + 100) as u64, b"TAIL", 0).unwrap();
    let d = s.open_path("/big", 0).unwrap();
    assert_eq!(&d[base.len()..base.len() + 100], &[0u8; 100]);
    assert_eq!(&d[base.len() + 100..], b"TAIL");
    // Full 校验通过（树/索引一致）。
    s.set_verify_level(VerifyLevel::Full);
    let mut d2 = s.open_path("/big", 0).unwrap();
    assert_eq!(d2, d);
    let _ = &mut d2;
    // 重挂载后一致。
    let dev = s.take_device();
    let mut s2 = KosStore::open(dev, VerifyLevel::Full).unwrap();
    assert_eq!(s2.open_path("/big", 0).unwrap(), d);
}

#[test]
fn block_cache_no_stale_reads() {
    // 块复用 + 缓存一致性回归：append → gc → 再写 → Full 全校验。
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    for r in 0..10 {
        s.write_mutable(&format!("/f{r}"), &[r as u8; 4096], 0, perm::DEFAULT)
            .unwrap();
    }
    for r in 0..10 {
        s.append(&format!("/f{r}"), &[0xEE; 512], 0).unwrap();
    }
    s.gc().unwrap();
    for r in 0..10 {
        s.write_range(&format!("/f{r}"), 128, &[r as u8 + 7; 64], 0).unwrap();
    }
    s.set_verify_level(VerifyLevel::Full);
    for r in 0..10 {
        let d = s.open_path(&format!("/f{r}"), 0).unwrap();
        assert_eq!(&d[128..192], &[r + 7; 64]);
    }
}

#[test]
fn truncate_shrink_extend_and_mtime() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    s.set_clock(1_700_000_000_000_000); // 2023-11-14T22:13:20Z
    let base = vec![0x5Au8; 4096 * 10];
    s.write_mutable("/t", &base, 0, perm::DEFAULT).unwrap();
    // 收缩到 3.5 chunk。
    s.truncate("/t", 4096 * 3 + 2048, 0).unwrap();
    let d = s.open_path("/t", 0).unwrap();
    assert_eq!(d.len(), 4096 * 3 + 2048);
    assert!(d.iter().all(|&b| b == 0x5A));
    // 扩展到 8 chunk（零填充）。
    s.truncate("/t", 4096 * 8, 0).unwrap();
    let d = s.open_path("/t", 0).unwrap();
    assert_eq!(d.len(), 4096 * 8);
    assert!(d[4096 * 3 + 2048..].iter().all(|&b| b == 0));
    // mtime 记录在头块 reserved。
    let e = s.list("/").unwrap().into_iter().find(|e| e.name == "t").unwrap();
    let h = s.stat(&e.content_hash).unwrap();
    let secs = u32::from_le_bytes([h.reserved[0], h.reserved[1], h.reserved[2], h.reserved[3]]);
    assert!(secs >= 1_700_000_000, "mtime 应已记录（{secs}）");
    // 收缩释放的空间可复用。
    let before = s.free_blocks();
    s.truncate("/t", 0, 0).unwrap();
    assert!(s.free_blocks() > before);
    assert_eq!(s.open_path("/t", 0).unwrap(), b"");
    // 不可变对象拒绝 truncate。
    s.write("/ro", b"x", 0, perm::DEFAULT).unwrap();
    assert!(matches!(s.truncate("/ro", 0, 0), Err(kos::KosError::NotMutable)));
    // 分页。
    s.mkdir("/pg", 0, perm::DIR_DEFAULT).unwrap();
    for i in 0..10 {
        s.write(&format!("/pg/e{i:02}"), b"x", 0, perm::DEFAULT).unwrap();
    }
    assert_eq!(s.list_skip("/pg", 8).unwrap().len(), 2);
    assert_eq!(s.list_skip("/pg", 99).unwrap().len(), 0);
}

#[test]
fn read_range_random_access() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    // 10MB 大对象 + 模式化内容。
    let n = 4096 * 2560;
    let content: Vec<u8> = (0..n).map(|i| ((i / 4096) % 251) as u8).collect();
    s.write_mutable("/big", &content, 0, perm::DEFAULT).unwrap();
    // 单点读（跨 chunk 边界）。
    let mut b = [0u8; 8];
    assert_eq!(s.read_range("/big", 4090, &mut b, 0).unwrap(), 8);
    assert_eq!(&b, &content[4090..4098]);
    // 深处读 1KB。
    let mut b = vec![0u8; 1024];
    assert_eq!(s.read_range("/big", 8_000_000, &mut b, 0).unwrap(), 1024);
    assert_eq!(b, content[8_000_000..8_000_000 + 1024]);
    // 末尾截断。
    let mut b = vec![0u8; 4096];
    assert_eq!(s.read_range("/big", n as u64 - 10, &mut b, 0).unwrap(), 10);
    // 越界返回 0。
    assert_eq!(s.read_range("/big", n as u64 + 1, &mut b, 0).unwrap(), 0);
    // 权限：私有对象其他 uid 拒绝。
    s.write_mutable("/priv", b"x", 0, perm::OWNER_R | perm::OWNER_W).unwrap();
    let mut b = [0u8; 4];
    assert!(matches!(
        s.read_range("/priv", 0, &mut b, 1),
        Err(kos::KosError::PermissionDenied)
    ));
}

#[test]
fn rename_same_and_cross_dir() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    s.mkdir("/a", 0, perm::DIR_DEFAULT).unwrap();
    s.mkdir("/b", 0, perm::DIR_DEFAULT).unwrap();
    s.write_mutable("/a/f", b"data-v1", 0, perm::DEFAULT).unwrap();
    // 同目录改名。
    s.rename("/a/f", "/a/g", 0).unwrap();
    assert!(s.open_path("/a/f", 0).is_err());
    assert_eq!(s.open_path("/a/g", 0).unwrap(), b"data-v1");
    // 跨目录移动。
    s.rename("/a/g", "/b/moved", 0).unwrap();
    assert_eq!(s.open_path("/b/moved", 0).unwrap(), b"data-v1");
    assert!(s.open_path("/a/g", 0).is_err());
    // 覆盖已存在目标。
    s.write_mutable("/b/other", b"OLD", 0, perm::DEFAULT).unwrap();
    s.rename("/b/moved", "/b/other", 0).unwrap();
    assert_eq!(s.open_path("/b/other", 0).unwrap(), b"data-v1");
    // 改名后追加仍工作（对象未动，条目哈希正确）。
    s.append("/b/other", b"+v2", 0).unwrap();
    assert_eq!(s.open_path("/b/other", 0).unwrap(), b"data-v1+v2");
    // 权限：他人对象拒绝改名。
    assert!(matches!(
        s.rename("/b/other", "/b/x", 1),
        Err(kos::KosError::PermissionDenied)
    ));
}

#[test]
fn inline_table_saves_block_per_object() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    let free0 = s.free_blocks();
    for i in 0..20 {
        s.write(&format!("/tiny{i:02}"), &[7u8; 16], 0, perm::DEFAULT).unwrap();
    }
    let used = free0 - s.free_blocks();
    // 20 个小对象 + 1 个新根目录对象。小对象应各占 3 块
    // （头块[含内联表] + 数据 + 索引），根目录 3 块：共 ≤ 63。
    assert!(
        used <= 20 * 3 + 3,
        "内联区段表未生效：20 小对象 + 根目录用了 {used} 块（期望 ≤63）"
    );
    // 内联对象读回完好。
    assert_eq!(s.open_path("/tiny07", 0).unwrap(), &[7u8; 16]);
    // 追加可让表膨胀到链式（跨内联/链式边界仍正确）。
    s.write_mutable("/grow", &[1u8; 4096 * 5], 0, perm::DEFAULT).unwrap();
    for r in 0..300 {
        s.append("/grow", &[2u8; 4096], 0).unwrap(); // 产生 300+ 数据区段
        let _ = r;
        if s.object_count() > 0 && r == 299 {
            let d = s.open_path("/grow", 0).unwrap();
            assert_eq!(d.len(), 4096 * 5 + 4096 * 300);
        }
    }
    // 重挂载后全部可读。
    let dev = s.take_device();
    let mut s2 = KosStore::open(dev, VerifyLevel::Full).unwrap();
    assert_eq!(s2.open_path("/tiny07", 0).unwrap(), &[7u8; 16]);
    assert_eq!(s2.open_path("/grow", 0).unwrap().len(), 4096 * 305);
}
