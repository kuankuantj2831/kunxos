//! 开发者证书（KunxOS 自建 CA，非 X.509）：紧凑二进制格式 + 链式验证。
//!
//! ```text
//! offset 0:  magic[8]        = "KUNXCERT"
//! offset 8:  version         u32 = 1
//! offset 12: key_usage       u8（bit0=包签名、bit1=索引签名、bit2=签发下级证书）
//! offset 13: subject_len     u16
//! offset 15: valid_from      u64（Unix 秒）
//! offset 23: valid_to        u64
//! offset 31: pubkey[32]      Ed25519 公钥
//! offset 63: subject[subject_len]（UTF-8，CN 等）
//!            parent_sig[64]  Ed25519 签名（父密钥对 [0, parent_sig) 签名）
//! ```
//!
//! 链模型：根锚（内嵌于 kunxpkg 的信任锚公钥）→ [中间证书…] → 开发者证书。
//! META-INF 按 leaf 在前存储；验证时从链顶（最接近根的证书）向下逐级
//! 验证 parent_sig，最后用 leaf 公钥验证包签名。

use crate::ed25519;

pub const CERT_MAGIC: [u8; 8] = *b"KUNXCERT";
pub const CERT_VERSION: u32 = 1;
/// 证书头长度（不含 subject 与签名）。
pub const CERT_HEAD: usize = 63;
/// 证书总长上限（subject ≤ 64）。
pub const CERT_MAX: usize = 63 + 64 + 64;
/// subject 最长。
pub const SUBJECT_MAX: usize = 64;
/// 链上证书数上限（root → intermediate → dev，最多 3 级）。
pub const CHAIN_MAX: usize = 3;

/// key_usage 位。
pub const KU_PACKAGE: u8 = 1; // 可签应用包
pub const KU_INDEX: u8 = 2; // 可签软件源索引
pub const KU_CERT: u8 = 4; // 可签发下级证书

/// 解析后的证书视图。
#[derive(Clone, Copy)]
pub struct Cert<'a> {
    pub key_usage: u8,
    pub valid_from: u64,
    pub valid_to: u64,
    pub pubkey: [u8; 32],
    pub subject: &'a [u8],
    /// parent_sig 覆盖的字节数（= 该证书在链中的起始偏移 + 签名前长度）。
    pub signed_len: usize,
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off], b[off + 1], b[off + 2], b[off + 3], b[off + 4], b[off + 5], b[off + 6], b[off + 7],
    ])
}

/// 解析单张证书（`data` 为该证书的完整字节）。非法返回 None。
pub fn parse(data: &[u8]) -> Option<Cert<'_>> {
    if data.len() < CERT_HEAD + 64 {
        return None;
    }
    if &data[0..8] != &CERT_MAGIC {
        return None;
    }
    if get_u32(data, 8) != CERT_VERSION {
        return None;
    }
    let ku = data[12];
    let slen = get_u16(data, 13) as usize;
    if slen > SUBJECT_MAX || CERT_HEAD + slen + 64 != data.len() {
        return None;
    }
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&data[31..63]);
    Some(Cert {
        key_usage: ku,
        valid_from: get_u64(data, 15),
        valid_to: get_u64(data, 23),
        pubkey,
        subject: &data[63..63 + slen],
        signed_len: CERT_HEAD + slen,
    })
}

/// 证书时间是否有效。`now == 0` 表示系统时间不可用（跳过时间校验，
/// 用于无 RTC 的开发环境；文档明确该降级）。
pub fn valid_time(c: &Cert, now: u64) -> bool {
    now == 0 || (c.valid_from <= now && now <= c.valid_to)
}

/// 校验证书的 parent_sig：`data` 为该证书完整字节，`parent_key` 为父公钥。
pub fn verify_parent(data: &[u8], c: &Cert, parent_key: &[u8; 32]) -> bool {
    let sig: [u8; 64] = data[c.signed_len..c.signed_len + 64].try_into().unwrap();
    ed25519::verify(parent_key, &data[..c.signed_len], &sig)
}

/// 校验完整链并返回 leaf（包签名者）公钥。
///
/// `certs` = META-INF 证书字节列表（leaf 在前）。`root` = 系统信任锚。
/// `now`：0 = 跳过时间校验。
/// 规则：
/// 1. 链长 1..=CHAIN_MAX；
/// 2. 从链顶（最后一个证书）向 leaf 逐级验证 parent_sig（链顶由 root 验证）；
/// 3. 链顶与中间证书须具备 KU_CERT；leaf 须具备 KU_PACKAGE；
/// 4. 所有证书须在有效期内。
pub fn verify_chain(
    certs: &[&[u8]],
    root: &[u8; 32],
    now: u64,
) -> Option<[u8; 32]> {
    if certs.is_empty() || certs.len() > CHAIN_MAX {
        return None;
    }
    let mut key = *root;
    // 链顶 → leaf
    for i in (0..certs.len()).rev() {
        let c = parse(certs[i])?;
        if !valid_time(&c, now) {
            return None;
        }
        // 除 leaf（i == 0）外须可签发下级证书；leaf 须可签包
        if i == 0 {
            if c.key_usage & KU_PACKAGE == 0 {
                return None;
            }
        } else if c.key_usage & KU_CERT == 0 {
            return None;
        }
        if !verify_parent(certs[i], &c, &key) {
            return None;
        }
        key = c.pubkey;
    }
    Some(key)
}
