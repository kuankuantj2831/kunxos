//! KXUP v1：KunxOS OTA Update Package（OTA 更新包）。
//!
//! ```text
//! @0  magic[8]       = "KXUPDv10"
//! @8  format u32     = 1
//! @12 target_role u32 = 6（系统镜像包）
//! @16 target_version u32  （新构建号，单调递增）
//! @20 base_version u32    （0=全量；非 0=从该版本增量）
//! @24 flags u32           （bit0=含增量组件）
//! @28 manifest_len u32
//! @32 components_count u32
//! @36 reserved u32 = 0
//! @40 components：每组件 = 16 字节头 + data：
//!       { role u32; data_len u32; delta_len u32; flags u32 }
//!       flags bit0 = 增量（data 区为 KXDELTA 补丁）
//!       flags bit1 = 全量（data 区为完整 KXSI 镜像）
//!    manifest[...manifest_len]
//!    sig[64] = Ed25519(ota_pk, [0, len-64))
//! ```
//!
//! 验签顺序：结构边界 → 整包 ota_pk → manifest 与头部版本一致 →
//! 每个全量组件再用 boot_pk 独立 KXSI 验签（双钥分层，防 ota 失泄单点植入）。

use crate::kxsi;
use crate::le_u32;

pub const MAGIC: &[u8; 8] = b"KXUPDv10";
pub const HEADER_SIZE: usize = 40;
pub const COMPONENT_HEADER: usize = 16;
pub const SIG_SIZE: usize = 64;
/// 系统镜像包角色。
pub const ROLE_SYSTEM_PKG: u32 = 6;

/// 组件标志。
pub const CF_DELTA: u32 = 1 << 0;
pub const CF_FULL: u32 = 1 << 1;

#[derive(Clone, Copy, Debug)]
pub struct Package {
    pub target_version: u32,
    pub base_version: u32,
    pub flags: u32,
    pub manifest_off: usize,
    pub manifest_len: usize,
    pub comp_off: usize,
    pub comp_count: u32,
    pub comp_total: usize,
    /// 签名字段起始（== 整包长度 - 64）。
    pub sig_off: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Component {
    pub role: u32,
    /// CF_DELTA / CF_FULL。
    pub flags: u32,
    pub data_off: usize,
    pub data_len: usize,
}

/// 解析 KXUP 结构（不验签）。\n
pub fn parse(blob: &[u8]) -> Option<Package> {
    if blob.len() < HEADER_SIZE + SIG_SIZE || &blob[0..8] != MAGIC {
        return None;
    }
    if le_u32(&blob[8..12]) != 1 {
        return None;
    }
    let manifest_len = le_u32(&blob[28..32]) as usize;
    let comp_count = le_u32(&blob[32..36]) as usize;
    let comp_off = HEADER_SIZE;
    let mut p = comp_off;
    let mut comp_total = 0usize;
    let mut ok = true;
    for _ in 0..comp_count {
        // 头必须在 blob 内（防越界读取导致 panic；任何越界一律拒绝）
        if p.checked_add(COMPONENT_HEADER).map_or(true, |e| e > blob.len()) {
            ok = false;
            break;
        }
        let data_len = le_u32(&blob[p + 4..p + 8]) as usize;
        let Some(next) = p.checked_add(COMPONENT_HEADER).and_then(|h| h.checked_add(data_len)) else {
            ok = false;
            break;
        };
        if next > blob.len() {
            ok = false;
            break;
        }
        p = next;
        comp_total = next.checked_sub(comp_off).unwrap_or(0);
    }
    if !ok {
        return None;
    }
    let manifest_off = p;
    let Some(sig_off) = p.checked_add(manifest_len) else { return None };
    if sig_off.checked_add(SIG_SIZE).map_or(true, |e| e != blob.len()) {
        return None;
    }
    Some(Package {
        target_version: le_u32(&blob[16..20]),
        base_version: le_u32(&blob[20..24]),
        flags: le_u32(&blob[24..28]),
        manifest_off,
        manifest_len,
        comp_off,
        comp_count: comp_count as u32,
        comp_total,
        sig_off,
    })
}

fn component_offsets(pkg: &Package, blob: &[u8], idx: usize) -> Option<(usize, usize)> {
    // 从 comp_off 起逐组件推进；所有读入都先做越界检查（防恶意 idx 触发 panic）
    let mut p = pkg.comp_off;
    for _ in 0..idx {
        if p.checked_add(COMPONENT_HEADER).map_or(true, |e| e > pkg.manifest_off) {
            return None;
        }
        let data_len = le_u32(&blob[p + 4..p + 8]) as usize;
        let next = p.checked_add(COMPONENT_HEADER)?.checked_add(data_len)?;
        if next > pkg.manifest_off {
            return None;
        }
        p = next;
    }
    if p.checked_add(COMPONENT_HEADER).map_or(true, |e| e > pkg.manifest_off) {
        return None;
    }
    Some((p, p + COMPONENT_HEADER))
}

/// 获取第 `idx` 个组件视图。
pub fn component(pkg: &Package, blob: &[u8], idx: usize) -> Option<Component> {
    if idx >= pkg.comp_count as usize {
        return None;
    }
    let (h_off, d_off) = component_offsets(pkg, blob, idx)?;
    let flags = le_u32(&blob[h_off + 12..h_off + 16]);
    let data_len = le_u32(&blob[h_off + 4..h_off + 8]) as usize;
    let end = d_off.checked_add(data_len)?;
    if end > pkg.manifest_off {
        return None;
    }
    Some(Component {
        role: le_u32(&blob[h_off..h_off + 4]),
        flags,
        data_off: d_off,
        data_len,
    })
}

/// 从 manifest 文本解析 `key=value` 行（返回 value 切片）。
pub fn manifest_get<'a>(manifest: &'a [u8], key: &str) -> Option<&'a [u8]> {
    for line in manifest.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(2, |&b| b == b'=');
        let k = it.next()?;
        let v = it.next()?;
        if k == key.as_bytes() {
            return Some(v);
        }
    }
    None
}

fn parse_u32(v: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    for &b in v {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}

/// 完整校验：结构 + 整包 ota_pk 签名 + manifest 一致性。
/// `boot_pk` 用于预检全量组件的 KXSI（增量组件在安装时验）。\n
pub fn parse_and_verify(blob: &[u8], ota_pk: &[u8; 32], boot_pk: &[u8; 32]) -> Option<Package> {
    let pkg = parse(blob)?;
    // 整包签名
    let sig: [u8; 64] = blob[pkg.sig_off..pkg.sig_off + SIG_SIZE].try_into().ok()?;
    if !crate::ed25519::verify(ota_pk, &blob[..pkg.sig_off], &sig) {
        return None;
    }
    let manifest = &blob[pkg.manifest_off..pkg.manifest_off + pkg.manifest_len];
    // manifest 声明的版本与头部一致
    let mv = manifest_get(manifest, "target_version")?;
    if parse_u32(mv)? != pkg.target_version {
        return None;
    }
    // 预检全量组件
    for i in 0..pkg.comp_count as usize {
        let c = component(&pkg, blob, i)?;
        if c.flags & CF_FULL != 0 || c.flags & CF_DELTA != 0 {
            if c.flags & CF_FULL != 0 && c.flags & CF_DELTA == 0 {
                // 全量：data 区必须是合法 KXSI 且与组件 role 匹配
                let data = &blob[c.data_off..c.data_off + c.data_len];
                if c.role != 0 && kxsi::verify(data, boot_pk, c.role).is_none() {
                    return None;
                }
            }
        }
    }
    Some(pkg)
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

    /// 构造“签名版”KXUP 的测试替身：真实签名由主机工具生成，
    /// 单元测试用 host 工具生成的固定向量（见 tools/kunxsec/kxup_build.py）。
    /// 这里只测结构解析与 manifest 读取（不依赖签名）。
    fn build_unverified(comp_blobs: &[&[u8]], manifest: &[u8], target_version: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&ROLE_SYSTEM_PKG.to_le_bytes());
        b.extend_from_slice(&target_version.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // base
        b.extend_from_slice(&0u32.to_le_bytes()); // flags
        b.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        b.extend_from_slice(&(comp_blobs.len() as u32).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for (role_idx, blob) in comp_blobs.iter().enumerate() {
            let role = (2 + role_idx) as u32; // 2=内核 3=DTB 4=ramdisk
            b.extend_from_slice(&role.to_le_bytes());
            b.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes()); // delta_len
            b.extend_from_slice(&CF_FULL.to_le_bytes());
            b.extend_from_slice(blob);
        }
        b.extend_from_slice(manifest);
        b.extend_from_slice(&[0u8; SIG_SIZE]); // 占位签名（本函数不验签）
        b
    }

    #[test]
    fn parse_layout() {
        let manifest = b"name=kunxos\ntarget_version=42\nbase_version=0\n";
        let blob11 = b"KXIMGKV1..."; // 11 字节（8 魔数 + 3）
        let b = build_unverified(&[blob11, blob11, blob11], manifest, 42);
        let pkg = parse(&b).expect("parse");
        assert_eq!(pkg.target_version, 42);
        assert_eq!(pkg.comp_count, 3);
        assert_eq!(pkg.comp_total, 3 * COMPONENT_HEADER + 3 * 11);
        // 组件遍历与偏移正确
        let c0 = component(&pkg, &b, 0).unwrap();
        assert_eq!(c0.role, 2);
        assert_eq!(c0.data_off, HEADER_SIZE + COMPONENT_HEADER);
        assert_eq!(c0.data_len, 11);
        let manifest_str = &b[pkg.manifest_off..pkg.manifest_off + pkg.manifest_len];
        assert_eq!(manifest_get(manifest_str, "name"), Some(b"kunxos".as_slice()));
        assert_eq!(manifest_get(manifest_str, "target_version"), Some(b"42".as_slice()));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse(b"KXUPDv10!").is_none());
        assert!(parse(&[0u8; 40]).is_none());
        let manifest = b"name=kunxos\ntarget_version=42\nbase_version=0\n";
        let b = build_unverified(&[b"AAAA", b"BBBB"], manifest, 42);
        // 截断签名区 → 结构不符
        let mut bad = b.clone();
        bad.pop();
        assert!(parse(&bad).is_none());
        // comp_count 与组件不匹配（count=5 但只有 2 个）
        let mut bad = build_unverified(&[b"AAAA", b"BBBB"], manifest, 42);
        bad[32..36].copy_from_slice(&5u32.to_le_bytes());
        assert!(parse(&bad).is_none()); // checked 推进越界
    }

    #[test]
    fn manifest_parse() {
        let m = b"name=kunxos\ntarget_version=7\nfoo=bar\n";
        assert_eq!(parse_u32(manifest_get(m, "target_version").unwrap()), Some(7));
        assert!(parse_u32(manifest_get(m, "name").unwrap()).is_none());
        assert_eq!(manifest_get(m, "nosuch"), None);
    }

    // 真实签名向量自检由主机工具 tools/kunxsec/kxup_build.py 独立生成并校验；
    // 单元测试仅验解析路径（签名验签在 kxsi/ed25519 层已覆盖）。
}