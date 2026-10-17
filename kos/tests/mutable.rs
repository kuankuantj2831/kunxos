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
