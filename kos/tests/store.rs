//! 存储引擎集成测试：mkfs → put/get → 命名空间 → 双槽提交 → 崩溃/篡改。

use kos::block::{BlockDevice, MemBlockDevice};
use kos::format::object::ObjectType;
use kos::hash::{Hasher, Sha256Hasher};
use kos::store::{check_access, perm, KosStore, VerifyLevel};

fn dev(blocks: u64) -> MemBlockDevice {
    MemBlockDevice::new(blocks, 4096)
}

#[test]
fn mkfs_open_roundtrip() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    assert_eq!(s.generation(), 1); // genesis
    assert_eq!(s.object_count(), 1); // 空根目录
    // 重挂载：状态一致。
    let d = s.take_device();
    let s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    assert_eq!(s2.generation(), 1);
    assert_eq!(s2.object_count(), 1);
}

#[test]
fn write_read_nested() {
    let mut s = KosStore::mkfs(dev(4096)).unwrap();
    s.mkdir("/etc", 0, perm::DIR_DEFAULT).unwrap();
    s.mkdir("/etc/conf", 0, perm::DIR_DEFAULT).unwrap();
    s.write("/etc/conf/passwd", b"root:x:0:0", 0, perm::DEFAULT).unwrap();
    s.write("/hello.txt", b"hello kos", 100, perm::DEFAULT).unwrap();

    assert_eq!(s.open_path("/hello.txt", 100).unwrap(), b"hello kos");
    assert_eq!(s.open_path("/etc/conf/passwd", 0).unwrap(), b"root:x:0:0");

    // 树形列表。
    let root = s.list("/").unwrap();
    assert_eq!(root.len(), 2);
    let etc = s.list("/etc").unwrap();
    assert_eq!(etc[0].name, "conf");

    // 覆盖写。
    s.write("/hello.txt", b"v2", 100, perm::DEFAULT).unwrap();
    assert_eq!(s.open_path("/hello.txt", 100).unwrap(), b"v2");

    // 重挂载后仍可读（双槽 + 扫描恢复）。
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::Full).unwrap();
    assert_eq!(s2.open_path("/etc/conf/passwd", 0).unwrap(), b"root:x:0:0");
}

#[test]
fn cas_dedup() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.mkdir("/a", 0, perm::DIR_DEFAULT).unwrap();
    s.mkdir("/b", 0, perm::DIR_DEFAULT).unwrap();
    s.write("/a/x", b"same content", 0, perm::DEFAULT).unwrap();
    let before = (s.object_count(), s.free_blocks());
    s.write("/b/y", b"same content", 0, perm::DEFAULT).unwrap();
    // 数据对象被去重：新增的只有 b 的新目录对象 + 新根目录（CoW 链）。
    assert_eq!(s.object_count(), before.0 + 2);
    // 直接对同内容再 put：计数不变（纯 CAS 层去重）。
    let n = s.object_count();
    s.put(b"same content", ObjectType::File, 0, perm::DEFAULT).unwrap();
    assert_eq!(s.object_count(), n);
    let h = Sha256Hasher.hash32(b"same content");
    assert_eq!(s.stat(&h).unwrap().content_len, 12);
}

#[test]
fn unlink_recycles() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.write("/f", &[0xABu8; 100_000], 0, perm::DEFAULT).unwrap();
    let used_before = s.free_blocks();
    s.unlink("/f", 0).unwrap();
    assert!(s.free_blocks() > used_before);
    assert!(s.open_path("/f", 0).is_err());
    // 重挂载后条目依然不存在；旧根目录对象是 CoW 孤儿（留待 GC），
    // 但 /f 的数据对象头已清零，其块在重挂载扫描后同样回收。
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    assert!(s2.open_path("/f", 0).is_err());
    assert_eq!(s2.object_count(), 2); // 新旧两个根目录
}

#[test]
fn tamper_detected() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.write("/data", &[0x5Au8; 8192], 0, perm::DEFAULT).unwrap();
    let mut d = s.take_device();
    // 篡改 /data 的第一个数据块（blk16 根v1 + blk17 数据 + blk18 索引
    // + blk19 /data 头 → 数据在 blk20）。
    let mut buf = [0u8; 4096];
    d.read_block(20, &mut buf).unwrap();
    buf[0] ^= 0xFF;
    d.write_block(20, &buf).unwrap();
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    assert!(s2.open_path("/data", 0).is_err()); // 内容哈希兜底
}

#[test]
fn crash_before_manifest_commit() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.write("/ok", b"committed", 0, perm::DEFAULT).unwrap();
    // 模拟崩溃：直接拿走设备（未走任何部分写路径的挂载必须干净）。
    let d = s.take_device();
    let mut s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    assert_eq!(s2.open_path("/ok", 0).unwrap(), b"committed");
}

#[test]
fn permissions() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.mkdir("/secret", 0, perm::OWNER_R | perm::OWNER_W).unwrap(); // 属主私有
    s.write("/secret/key", b"k", 0, perm::OWNER_R).unwrap();
    assert!(s.open_path("/secret/key", 0).is_ok());
    assert!(s.open_path("/secret/key", 1).is_err()); // 其他用户无读权限
    // 覆盖他人条目需要写权限。
    assert!(s.write("/secret/key", b"x", 1, perm::DEFAULT).is_err());
    // 单元：位判读。
    assert!(check_access(0, 0, perm::EXEC, perm::OWNER_X));
    assert!(check_access(1, 0, perm::EXEC, perm::OTHER_X)); // EXEC 含 other-x
    assert!(!check_access(1, 0, perm::OWNER_R | perm::OWNER_W, perm::OTHER_R));
    assert!(check_access(1, 0, perm::ALL_R, perm::OTHER_R));
}

#[test]
fn symlink_rw() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    s.write("/target", b"T", 0, perm::DEFAULT).unwrap();
    s.symlink("/link", b"/target", 0).unwrap();
    assert_eq!(s.readlink("/link").unwrap(), b"/target");
}

#[test]
fn generation_monotonic_across_remount() {
    let mut s = KosStore::mkfs(dev(1024)).unwrap();
    let g0 = s.generation();
    s.write("/x", b"1", 0, perm::DEFAULT).unwrap();
    s.write("/x", b"2", 0, perm::DEFAULT).unwrap();
    let g1 = s.generation();
    assert!(g1 > g0);
    let d = s.take_device();
    let s2 = KosStore::open(d, VerifyLevel::HashOnly).unwrap();
    assert_eq!(s2.generation(), g1);
}
