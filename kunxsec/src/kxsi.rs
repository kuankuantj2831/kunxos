//! KXSI v1：KunxOS Signed Image（签名镜像容器）。
//!
//! 内核 / DTB / ramdisk / 恢复 ramdisk 都以 KXSI 包装存放在 boot 分区中：
//!
//! ```text
//! @0   magic[8]    = "KXIMGKV1"
//! @8   pad u32     = 0
//! @12  role u32    = 2=内核 3=DTB 4=ramdisk 5=恢复ramdisk 6=系统镜像包
//! @16  version u32 = 构建号（单调递增，反降级比较用）
//! @20  timestamp u64
//! @28  reserved u32 = 0
//! @32  payload_off u32 = 80（v1 固定）
//! @36  payload_len u32
//! @40  payload_sha256[32]
//! @72  entry u64       （AArch64 内核入口；0 = 按角色默认）
//! @80  payload[...payload_len]
//!      sig[64]         = Ed25519(keys::BOOT_PK, [0, 80+payload_len))
//! ```
//!
//! 验签输入全部为公开数据；仅最终签名比对用常量时间（`crate::ct_eq`）。

use crate::sha256::Sha256;
use crate::le_u32;
use crate::le_u64;

/// KXSI 头大小（v1）。
pub const HEADER_SIZE: usize = 80;
/// 签名大小。
pub const SIG_SIZE: usize = 64;

/// 镜像角色。
pub const ROLE_KERNEL: u32 = 2;
pub const ROLE_DTB: u32 = 3;
pub const ROLE_RAMDISK: u32 = 4;
pub const ROLE_RECOVERY_RAMDISK: u32 = 5;
pub const ROLE_SYSTEM_IMAGE: u32 = 6;

/// 解析并验签 KXSI 镜像。`expected_role == 0` 表示不限制角色。
///
/// 返回成功时的视图（载荷区间、版本、时间戳、入口）。任何结构/哈希/签名
/// 不一致都返回 `None`。
pub fn verify(blob: &[u8], pk: &[u8; 32], expected_role: u32) -> Option<Kxsi> {
    let v = parse(blob)?;
    if expected_role != 0 && v.role != expected_role {
        return None;
    }
    // 载荷哈希
    let mut h = Sha256::new();
    h.update(&blob[v.payload_off..v.payload_off + v.payload_len]);
    if h.finalize() != v.payload_sha256 {
        return None;
    }
    // 签名覆盖 [0, payload_off+payload_len)
    let signed_end = v.payload_off + v.payload_len;
    let sig: [u8; 64] = blob[blob.len() - SIG_SIZE..].try_into().ok()?;
    if !crate::ed25519::verify(pk, &blob[..signed_end], &sig) {
        return None;
    }
    Some(v)
}

/// 仅解析 KXSI 结构（不验签）。用于边界检查/预览；验签用 `verify`。
pub fn parse(blob: &[u8]) -> Option<Kxsi> {
    if blob.len() < HEADER_SIZE + SIG_SIZE {
        return None;
    }
    if &blob[0..8] != b"KXIMGKV1" {
        return None;
    }
    let payload_off = le_u32(&blob[32..36]) as usize;
    let payload_len = le_u32(&blob[36..40]) as usize;
    // v1 固定偏移；payload 必须落在 [80, len-64) 内且不重叠头部/签名
    if payload_off != HEADER_SIZE {
        return None;
    }
    let signed_end = payload_off.checked_add(payload_len)?;
    if signed_end > blob.len() - SIG_SIZE {
        return None;
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&blob[40..72]);
    Some(Kxsi {
        role: le_u32(&blob[12..16]),
        version: le_u32(&blob[16..20]),
        timestamp: le_u64(&blob[20..28]),
        payload_off,
        payload_len,
        payload_sha256: sha,
        entry: le_u64(&blob[72..80]),
    })
}

/// 已解析 KXSI 的结构视图。
#[derive(Clone, Copy, Debug)]
pub struct Kxsi {
    pub role: u32,
    pub version: u32,
    pub timestamp: u64,
    pub payload_off: usize,
    pub payload_len: usize,
    pub payload_sha256: [u8; 32],
    /// 内核入口地址；0 表示按角色默认。
    pub entry: u64,
}

impl Kxsi {
    /// 角色默认入口：内核 0x40080000；其余角色 0（无入口语义）。
    pub fn resolved_entry(&self) -> u64 {
        if self.entry != 0 {
            return self.entry;
        }
        if self.role == ROLE_KERNEL {
            0x4008_0000
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// 由工具 tools/kunxsec/kxsi_build.py 用 demo 密钥生成的合法签名镜像向量。
    fn valid_ramdisk_v7() -> Vec<u8> {
        // 头 80 字节 + 载荷 256 字节（0xAA 重复） + 签名 64 字节
        let mut b = Vec::new();
        b.extend_from_slice(&[
            0x4b, 0x58, 0x49, 0x4d, 0x47, 0x4b, 0x56, 0x31, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x07, 0x00, 0x00, 0x00, 0x00, 0xd2, 0x49, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        b.extend_from_slice(&[
            0x50, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0xfd, 0x4c, 0x55, 0xf0, 0xc4, 0x80, 0x8b, 0x05,
            0x02, 0xe8, 0xd8, 0x8b, 0x84, 0xc8, 0x4f, 0x80, 0xe3, 0x8b, 0x4c, 0x8c, 0xd3, 0x54, 0x1c, 0x5a,
        ]);
        b.extend_from_slice(&[
            0x7a, 0x32, 0x8c, 0x41, 0xb9, 0x24, 0xf9, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        b.extend_from_slice(&[0xAA; 256]);
        b.extend_from_slice(&[
            0xe6, 0xb7, 0x2a, 0x67, 0x14, 0x9c, 0x16, 0xeb, 0xd1, 0xd1, 0xe4, 0xd0, 0xbd, 0x6b, 0x88, 0x54,
            0xcc, 0x9c, 0x36, 0x82, 0x0a, 0xd7, 0x12, 0x7c, 0xda, 0xb3, 0xee, 0xcb, 0xe2, 0x2c, 0xfd, 0x62,
            0xb7, 0x3f, 0x9f, 0x57, 0x2f, 0x80, 0x6a, 0xec, 0xce, 0x59, 0xfa, 0x64, 0x62, 0x2f, 0x63, 0x40,
            0x5e, 0x6e, 0x9f, 0x4d, 0x7d, 0x02, 0xf2, 0xc3, 0xc7, 0xf2, 0xda, 0xbd, 0xac, 0x97, 0xaf, 0x0f,
        ]);
        b
    }

    fn valid_kernel_v42() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[
            0x4b, 0x58, 0x49, 0x4d, 0x47, 0x4b, 0x56, 0x31, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            0x2a, 0x00, 0x00, 0x00, 0x00, 0xd2, 0x49, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        b.extend_from_slice(&[
            0x50, 0x00, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x57, 0x1c, 0x1e, 0xde, 0x2b, 0x1e, 0xf4, 0x32,
            0x47, 0x1a, 0x77, 0x75, 0xb3, 0x0a, 0x55, 0x24, 0x5c, 0xf1, 0x92, 0x27, 0x21, 0x88, 0xed, 0x95,
        ]);
        b.extend_from_slice(&[
            0x35, 0xcc, 0x79, 0x19, 0x5c, 0xb3, 0x08, 0x43, 0x00, 0x00, 0x08, 0x40, 0x00, 0x00, 0x00, 0x00,
            0x6b, 0x65, 0x72, 0x6e, 0x65, 0x6c, 0x20, 0x76, 0x34, 0x32, 0x20, 0x62, 0x6c, 0x6f, 0x62, 0x08,
        ]);
        b.extend_from_slice(&[
            0xdb, 0x68, 0x43, 0xb1, 0x4b, 0x34, 0x3a, 0x08, 0x50, 0x86, 0xde, 0x2c, 0xfd, 0xa7, 0x50, 0x8f,
            0x90, 0xca, 0x9e, 0x06, 0x74, 0x6e, 0xaf, 0xfb, 0x10, 0x64, 0x63, 0x2b, 0x0d, 0x86, 0xa8, 0xeb,
        ]);
        b.extend_from_slice(&[
            0x9f, 0x70, 0x36, 0x00, 0x67, 0x48, 0x71, 0x20, 0x3a, 0x38, 0x7a, 0xa5, 0xab, 0x93, 0x87, 0xb9,
            0xdd, 0x66, 0x1c, 0x2f, 0x64, 0x26, 0xa4, 0xcd, 0x59, 0xc7, 0x20, 0xa2, 0xc2, 0x5a, 0x0c,
        ]);
        b
    }

    #[test]
    fn valid_vectors_verify() {
        let r = verify(&valid_ramdisk_v7(), &crate::keys::BOOT_PK, ROLE_RAMDISK);
        assert!(r.is_some(), "ramdisk v7 must verify");
        assert_eq!(r.unwrap().version, 7);
        let k = verify(&valid_kernel_v42(), &crate::keys::BOOT_PK, ROLE_KERNEL);
        assert!(k.is_some(), "kernel v42 must verify");
        assert_eq!(k.unwrap().entry, 0x4008_0000);
        assert_eq!(k.unwrap().resolved_entry(), 0x4008_0000);
    }

    #[test]
    fn tamper_fails() {
        let mut b = valid_kernel_v42();
        let n = b.len();
        b[128] ^= 1; // 载荷字节
        assert!(verify(&b, &crate::keys::BOOT_PK, ROLE_KERNEL).is_none());
        b[n - 1] ^= 1; // 签名字节
        assert!(verify(&b, &crate::keys::BOOT_PK, ROLE_KERNEL).is_none());
    }

    #[test]
    fn wrong_role_or_key_fails() {
        let b = valid_kernel_v42();
        assert!(verify(&b, &crate::keys::BOOT_PK, ROLE_RAMDISK).is_none());
        let wrong = [0x7Au8; 32];
        assert!(verify(&b, &wrong, ROLE_KERNEL).is_none());
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse(b"KXIMGKV1!").is_none()); // magic 错误
        assert!(parse(&[0u8; 80]).is_none()); // 长度不足
        let mut b = Vec::new();
        b.extend_from_slice(b"KXIMGKV1");
        b.extend_from_slice(&[0u8; 24]);
        b.extend_from_slice(&80u32.to_le_bytes());
        b.extend_from_slice(&(1u32 << 28).to_le_bytes()); // payload_len 巨大
        b.extend_from_slice(&[0u8; 32 + 32]);
        assert!(parse(&b).is_none()); // checked 越界
    }

    #[test]
    fn host_compatible_parse() {
        let b = valid_ramdisk_v7();
        let v = parse(&b).expect("parse valid");
        assert_eq!(v.role, ROLE_RAMDISK);
        assert_eq!(v.version, 7);
        assert_eq!(v.payload_off, 80);
        assert_eq!(v.payload_len, 256);
        assert_eq!(v.payload_sha256, crate::sha256::sha256(&[0xAAu8; 256]));
        assert_eq!(v.resolved_entry(), 0); // 非内核角色 → 无入口语义
    }
}