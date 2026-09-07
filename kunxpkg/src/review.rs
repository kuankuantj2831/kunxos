//! 审核流程：对 .kunx 包做「签名验证 + 证书链验证 + 权限审查」，
//! 产出可打印的审查结果，支持批准记录（`pkg review --approve`）。

use crate::cert;
use crate::ed25519;
use crate::format::{self, KunxPkg, Manifest};
use crate::perms;
use crate::trust;

/// 审查结果。
#[derive(Clone, Copy)]
pub struct Review<'a> {
    pub pkg: KunxPkg,
    pub manifest: Manifest<'a>,
    /// 证书链是否有效（对 ROOT_ANCHOR）。
    pub chain_valid: bool,
    /// 链验证出的 leaf 公钥（chain_valid 时有效）。
    pub leaf_key: [u8; 32],
    /// 包签名是否有效（leaf 公钥对 signed_data）。
    pub sig_valid: bool,
    /// 时间是否有效（now=0 视为跳过时间校验，仍为 true）。
    pub time_ok: bool,
    /// 危险权限数。
    pub dangerous: usize,
    /// API 级别是否兼容（manifest.min_api ≤ API_LEVEL）。
    pub api_ok: bool,
    /// 证书数。
    pub cert_count: usize,
}

/// 对包字节做完整审查。`now`：Unix 秒；0 = 跳过时间校验。
pub fn review(data: &[u8], now: u64) -> Option<Review<'_>> {
    let pkg = format::parse(data)?;
    let manifest = format::parse_manifest(pkg.manifest(data))?;

    // 解析证书链（META-INF 内连续证书，leaf 在前）
    let meta = pkg.meta(data);
    let mut certs: [&[u8]; cert::CHAIN_MAX] = [&[]; cert::CHAIN_MAX];
    let mut cert_count = 0usize;
    let mut off = 0usize;
    while off < meta.len() && cert_count < cert::CHAIN_MAX {
        // 证书长度：先从头部的 subject_len 算出本张证书的精确字节数再切片
        // ——cert::parse 要求传入数据恰好等于一张证书，把「off 到 META-INF
        // 末尾」的整段传入时，≥2 张证书（leaf + 中间/根）的第一张必然因
        // 长度不匹配解析失败 → 链恒无效，多级证书链功能整体不可用。
        if meta.len() - off < cert::CERT_HEAD {
            break;
        }
        let slen = u16::from_le_bytes([meta[off + 13], meta[off + 14]]) as usize;
        if slen > cert::SUBJECT_MAX {
            break;
        }
        let clen = cert::CERT_HEAD + slen + 64;
        if meta.len() - off < clen {
            break;
        }
        if cert::parse(&meta[off..off + clen]).is_none() {
            break;
        }
        certs[cert_count] = &meta[off..off + clen];
        cert_count += 1;
        off += clen;
    }

    let chain_key = cert::verify_chain(&certs[..cert_count], &trust::ROOT_ANCHOR, now);
    let chain_valid = chain_key.is_some();
    let leaf_key = chain_key.unwrap_or([0u8; 32]);

    let sig_valid = if pkg.signed() && chain_valid {
        let sig: [u8; 64] = pkg.signature(data).try_into().unwrap_or([0u8; 64]);
        ed25519::verify(&leaf_key, pkg.signed_data(data), &sig)
    } else {
        false
    };

    let dangerous = perms::count_dangerous(&manifest.permissions, manifest.perm_count);
    let api_ok = manifest.min_api <= trust::API_LEVEL;

    Some(Review {
        pkg,
        manifest,
        chain_valid,
        leaf_key,
        sig_valid,
        time_ok: now == 0 || certs.iter().take(cert_count).all(|c| {
            cert::parse(c).map(|cc| cert::valid_time(&cc, now)).unwrap_or(false)
        }),
        dangerous,
        api_ok,
        cert_count,
    })
}
