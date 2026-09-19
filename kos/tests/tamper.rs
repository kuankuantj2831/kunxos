//! 集成测试：篡改检测——对每个带 CRC 结构的映像**逐字节**翻转，
//! `from_bytes` 必须失败。覆盖所有 4096/192/96 字节位置。

use kos::error::KosError;
use kos::format::manifest::KosRootManifest;
use kos::format::merkle::MerkleIndexBlock;
use kos::format::namespace::{EntryType, NamespaceEntry, NamespaceNode, NodeType};
use kos::format::object::{ObjectType, KosObject};
use kos::format::superblock::KosSuperblock;

fn superblock_image() -> Vec<u8> {
    KosSuperblock::mkfs_layout(1 << 12).to_bytes().to_vec()
}

fn object_image() -> Vec<u8> {
    let mut o = KosObject::empty(ObjectType::Blob);
    o.content_len = 4096;
    o.content_hash = [7u8; 32];
    o.owner_uid = 1;
    o.capability = 0o600;
    o.block_count = 1;
    o.indirect = 9;
    o.to_bytes().to_vec()
}

fn manifest_image() -> Vec<u8> {
    let mut m = KosRootManifest::genesis([3u8; 32], 1_758_000_000);
    m.generation = 5;
    m.signature = [8u8; 64];
    m.to_bytes().to_vec()
}

fn namespace_image() -> Vec<u8> {
    let mut node = NamespaceNode::new(NodeType::Internal);
    for i in 0..5u8 {
        node.push_entry(NamespaceEntry::new([i; 16], [i + 1; 32], EntryType::File, 0o644));
    }
    node.to_bytes().to_vec()
}

fn merkle_image() -> Vec<u8> {
    let children: Vec<[u8; 32]> = (0..100u32).map(|i| [(i % 251) as u8; 32]).collect();
    MerkleIndexBlock::new(1, 2, &children).to_bytes().to_vec()
}

/// 对 `img` 的每个字节位置做 XOR 0xFF 翻转，断言解析全部失败。
fn assert_every_tamper_detected<T>(
    img: &[u8],
    parse: impl Fn(&[u8]) -> Result<T, KosError>,
    ctx: &str,
) {
    for i in 0..img.len() {
        let mut tampered = img.to_vec();
        tampered[i] ^= 0xFF;
        assert!(
            parse(&tampered).is_err(),
            "{ctx}：翻转第 {i} 字节后仍解析成功"
        );
    }
}

#[test]
fn superblock_tamper_every_byte() {
    assert_every_tamper_detected(&superblock_image(), KosSuperblock::from_bytes, "superblock");
}

#[test]
fn object_tamper_every_byte() {
    assert_every_tamper_detected(&object_image(), KosObject::from_bytes, "object");
}

#[test]
fn manifest_tamper_every_byte() {
    assert_every_tamper_detected(&manifest_image(), KosRootManifest::from_bytes, "manifest");
}

#[test]
fn namespace_node_tamper_every_byte() {
    assert_every_tamper_detected(&namespace_image(), NamespaceNode::from_bytes, "namespace");
}

#[test]
fn merkle_index_tamper_every_byte() {
    assert_every_tamper_detected(&merkle_image(), MerkleIndexBlock::from_bytes, "merkle-index");
}

/// 篡改不落在 CRC 字段时，报错应为 CrcMismatch（可定位诊断信息）。
#[test]
fn tamper_outside_crc_field_reports_crc_mismatch() {
    // superblock：CRC 在 96..100，翻 500 号字节
    let mut img = superblock_image();
    img[500] ^= 0x01;
    assert!(matches!(
        KosSuperblock::from_bytes(&img).unwrap_err(),
        KosError::CrcMismatch { .. }
    ));
    // object：CRC 在 88..92，翻 content_hash 区
    let mut img = object_image();
    img[20] ^= 0x01;
    assert!(matches!(
        KosObject::from_bytes(&img).unwrap_err(),
        KosError::CrcMismatch { .. }
    ));
    // manifest：CRC 在 184..188，翻 signature 区
    let mut img = manifest_image();
    img[150] ^= 0x01;
    assert!(matches!(
        KosRootManifest::from_bytes(&img).unwrap_err(),
        KosError::CrcMismatch { .. }
    ));
}

/// NamespaceEntry 本身无 CRC（设计如此）：篡改检测由所在节点的
/// CRC 承担——条目区任何字节翻转必须让节点解析失败。
#[test]
fn entry_tamper_caught_by_node_crc() {
    let img = namespace_image();
    for &off in &[8usize, 8 + 26, 8 + 3 * 52 + 50] {
        // name_hash / obj_hash / mode 区各取一点
        let mut bad = img.clone();
        bad[off] ^= 0x80;
        assert!(
            NamespaceNode::from_bytes(&bad).is_err(),
            "翻转节点内 off={off} 未被检测"
        );
    }
}
