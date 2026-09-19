//! 集成测试：全部磁盘结构往返、字节大小与设计一致、块设备基本行为。

use kos::block::{BlockDevice, MemBlockDevice};
use kos::format::manifest::KosRootManifest;
use kos::format::merkle::MerkleIndexBlock;
use kos::format::namespace::{
    EntryType, NamespaceEntry, NamespaceNode, NodeType, NAMESPACE_MAX_ENTRIES,
};
use kos::format::object::{object_flags, ObjectType, KosObject};
use kos::format::superblock::KosSuperblock;

/// 结构体字节大小必须与设计完全一致（repr(C) 实测）。
#[test]
fn struct_sizes_match_design() {
    assert_eq!(core::mem::size_of::<KosSuperblock>(), 4096);
    assert_eq!(core::mem::size_of::<KosObject>(), 96);
    assert_eq!(core::mem::size_of::<KosRootManifest>(), 192);
    assert_eq!(core::mem::size_of::<NamespaceNode>(), 4096);
    assert_eq!(core::mem::size_of::<NamespaceEntry>(), 52);
    assert_eq!(core::mem::size_of::<MerkleIndexBlock>(), 4096);
    // 布局常量
    assert_eq!(NAMESPACE_MAX_ENTRIES, 78);
    assert_eq!(kos::format::merkle::MERKLE_FANOUT, 127);
    assert_eq!(kos::merkle::DATA_CHUNK_SIZE, 4096);
}

#[test]
fn superblock_roundtrip_and_layout() {
    let sb = KosSuperblock::mkfs_layout(1 << 16);
    let img = sb.to_bytes();
    assert_eq!(img.len(), 4096);
    assert_eq!(&img[0..8], b"KOSV0001"); // magic 明文可读（小端打包字符串）
    let parsed = KosSuperblock::from_bytes(&img).unwrap();
    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.block_size, 4096);
    assert_eq!(parsed.root_slot_a, 1);
    assert_eq!(parsed.root_slot_b, 2);
    assert_eq!(parsed.obj_pool_blocks + parsed.rec_pool_blocks, (1 << 16) - 16);
    assert_eq!(parsed.to_bytes(), img); // 字节级往返
}

#[test]
fn object_header_roundtrip() {
    let mut o = KosObject::empty(ObjectType::File);
    o.flags = object_flags::COMPRESSED;
    o.content_len = 123_456;
    o.content_hash = [9u8; 32];
    o.owner_uid = 1001;
    o.capability = 0o755;
    o.created_at = 1_758_123_456;
    o.version = 9;
    o.block_count = 31;
    o.indirect = 777;
    let img = o.to_bytes();
    assert_eq!(img.len(), 96);
    assert_eq!(&img[0..4], b"KO\0\0");
    let parsed = KosObject::from_bytes(&img).unwrap();
    assert_eq!(parsed.content_hash, [9u8; 32]);
    assert_eq!(parsed.owner_uid, 1001);
    assert_eq!(parsed.capability, 0o755);
    assert_eq!(parsed.block_count, 31);
    assert_eq!(parsed.indirect, 777);
    assert_eq!(parsed.to_bytes(), img);
}

#[test]
fn manifest_roundtrip() {
    let mut m = KosRootManifest::genesis([0xAA; 32], 1_758_000_000);
    m.generation = 7;
    m.parent_manifest_hash = [0xBB; 32];
    m.signer_pk = [0xCC; 32];
    m.signature = [0xDD; 64];
    let img = m.to_bytes();
    assert_eq!(img.len(), 192);
    assert_eq!(&img[0..8], b"KOSROOT\0");
    let parsed = KosRootManifest::from_bytes(&img).unwrap();
    assert_eq!(parsed.generation, 7);
    assert_eq!(parsed.namespace_root_hash, [0xAA; 32]);
    assert_eq!(parsed.signature, [0xDD; 64]);
    assert_eq!(parsed.to_bytes(), img);
}

#[test]
fn namespace_node_roundtrip() {
    let mut node = NamespaceNode::new(NodeType::Leaf);
    for i in 0..10u8 {
        assert!(node.push_entry(NamespaceEntry::new(
            [i; 16],
            [i + 100; 32],
            if i % 3 == 0 { EntryType::Dir } else { EntryType::File },
            0o644,
        )));
    }
    let img = node.to_bytes();
    assert_eq!(img.len(), 4096);
    assert_eq!(&img[0..4], b"KNS\0");
    let parsed = NamespaceNode::from_bytes(&img).unwrap();
    assert_eq!(parsed.entry_count, 10);
    assert_eq!(parsed.node_type, NodeType::Leaf.as_u8());
    assert_eq!(parsed.entries[9].obj_hash, [109u8; 32]);
    assert_eq!(parsed.to_bytes(), img);
}

#[test]
fn merkle_index_block_roundtrip() {
    let children: Vec<[u8; 32]> = (0..50u8).map(|i| [i; 32]).collect();
    let blk = MerkleIndexBlock::new(2, 5, &children);
    let img = blk.to_bytes();
    assert_eq!(img.len(), 4096);
    assert_eq!(&img[0..4], b"KMIX");
    let parsed = MerkleIndexBlock::from_bytes(&img).unwrap();
    assert_eq!(parsed.level, 2);
    assert_eq!(parsed.index, 5);
    assert_eq!(parsed.count, 50);
    assert_eq!(parsed.hashes[49], [49u8; 32]);
    assert_eq!(parsed.to_bytes(), img);
}

#[test]
fn block_device_write_read_superblock() {
    let mut dev = MemBlockDevice::new(64, 4096);
    let sb = KosSuperblock::mkfs_layout(64);
    dev.write_block(0, &sb.to_bytes()).unwrap();
    dev.flush().unwrap();
    let mut buf = [0u8; 4096];
    dev.read_block(0, &mut buf).unwrap();
    let parsed = KosSuperblock::from_bytes(&buf).unwrap();
    assert_eq!(parsed.total_blocks, 64);
    // 块 1/2 预留给 Root 槽：写入 manifest 再读回
    let m = KosRootManifest::genesis([1; 32], 42);
    // manifest 192 字节置于 4096 槽首（块内包装的约定形式）
    let mut slot = [0u8; 4096];
    slot[..192].copy_from_slice(&m.to_bytes());
    dev.write_block(1, &slot).unwrap();
    dev.read_block(1, &mut buf).unwrap();
    let pm = KosRootManifest::from_bytes(&buf[..192]).unwrap();
    assert_eq!(pm.timestamp, 42);
}

#[test]
fn block_device_error_paths() {
    let mut dev = MemBlockDevice::new(8, 512);
    assert!(dev.write_block(8, &[0u8; 512]).is_err()); // 越界
    assert!(dev.write_block(0, &[0u8; 511]).is_err()); // 长度不符
    assert!(dev.read_block(0, &mut [0u8; 513]).is_err());
}
