//! 最小 X.509（DER）证书解析与链验证 —— 浏览器 TLS 客户端信任模型。
//!
//! 只支持验证所需的子集：
//! - 签名算法仅 `sha256WithRSAEncryption`（1.2.840.113549.1.1.11）；
//! - 公钥仅 RSA；
//! - 链验证：叶子开始逐级找签发者（内嵌根优先，其次服务器送来的中间
//!   证书），按 issuer/subject DN 字节匹配，用签发者公钥做 PKCS#1 v1.5
//!   验签，抵达内嵌根即成功（根本身不验签——信任锚以公钥为定义）；
//! - 主机名：叶子 SAN dNSName（支持 `*.example.com` 单标签通配），
//!   无 SAN 时回退 CN；
//! - 有效期：notBefore/notAfter（UTCTime/GeneralizedTime），与「当前时间」
//!   比较（由调用方给：来宾时钟不可信时以构建时间为下限）。

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::rsa::RsaPub;
use super::roots::ROOTS;
use crate::kxsec::sha256;

// ==================== DER 基础 ====================

const TAG_SEQ: u8 = 0x30;
const TAG_INT: u8 = 0x02;
const TAG_BITSTR: u8 = 0x03;
const TAG_OCTET: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_UTC: u8 = 0x17;
const TAG_GENTIME: u8 = 0x18;

/// 读一个 TLV：返回 (tag, content 区间, 下一元素偏移)。
fn read_tlv(b: &[u8], pos: usize) -> Option<(u8, usize, usize, usize)> {
    if pos + 2 > b.len() {
        return None;
    }
    let tag = b[pos];
    let first = b[pos + 1];
    let (len, hdr) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 || pos + 2 + n > b.len() {
            return None;
        }
        let mut v = 0usize;
        for i in 0..n {
            v = (v << 8) | b[pos + 2 + i] as usize;
        }
        (v, 2 + n)
    };
    let start = pos + hdr;
    let end = start.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((tag, start, end, end))
}

/// OID 内容字节相等（1.2.840.113549.1.1.11 = sha256WithRSA）。
fn oid_is(oid: &[u8], expect: &[u8]) -> bool {
    oid == expect
}

const OID_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OID_SHA256_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];

// ==================== 时间 ====================

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 解析 UTCTime / GeneralizedTime（Z 后缀）为 unix 秒。
fn parse_time(tag: u8, c: &[u8]) -> Option<i64> {
    let s = core::str::from_utf8(c).ok()?;
    let digits = &s[..s.len().saturating_sub(1)];
    if !s.ends_with('Z') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = if tag == TAG_UTC {
        if digits.len() < 10 {
            return None;
        }
        let yy: i64 = digits[..2].parse().ok()?;
        let y = if yy >= 50 { 1900 + yy } else { 2000 + yy };
        (y, &digits[2..])
    } else if tag == TAG_GENTIME {
        if digits.len() < 12 {
            return None;
        }
        (digits[..4].parse().ok()?, &digits[4..])
    } else {
        return None;
    };
    if rest.len() < 10 {
        return None;
    }
    let mo: i64 = rest[0..2].parse().ok()?;
    let d: i64 = rest[2..4].parse().ok()?;
    let h: i64 = rest[4..6].parse().ok()?;
    let mi: i64 = rest[6..8].parse().ok()?;
    let sec: i64 = rest[8..10].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(year, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

// ==================== 证书 ====================

pub struct Cert {
    /// tbsCertificate 的完整 TLV（验签输入）。
    pub tbs_tlv: Vec<u8>,
    /// 签名算法 OID 内容。
    pub sig_alg: Vec<u8>,
    /// 签名值（BIT STRING 内容，去掉 unused-bits 字节）。
    pub sig: Vec<u8>,
    /// issuer / subject Name 的完整 TLV。
    pub issuer: Vec<u8>,
    pub subject: Vec<u8>,
    pub not_before: i64,
    pub not_after: i64,
    /// RSA 公钥 (modulus, exponent)（DER INTEGER 内容，大端）。
    pub rsa_key: Option<(Vec<u8>, Vec<u8>)>,
    /// SAN dNSName（小写化）。
    pub san: Vec<String>,
    /// subject CN（回退匹配用，小写化）。
    pub cn: Option<String>,
    /// basicConstraints cA（无该扩展 = false）。链验证要求中间签发者
    /// CA=true：否则持有效叶子证书者可用其私钥自签任意域名的伪造叶子
    /// 冒充他人（MITM 冒充完整绕过）。
    pub is_ca: bool,
}

fn parse_integer_content(b: &[u8], tag: u8, s: usize, e: usize) -> Option<&[u8]> {
    if tag != TAG_INT {
        return None;
    }
    let mut c = &b[s..e];
    // 去符号填充的前导 0（最多 1 个）
    while c.len() > 1 && c[0] == 0 {
        c = &c[1..];
    }
    Some(c)
}

/// 解析 RSA SPKI 内容（s 指向 SPKI SEQ 的内容起点，即 AlgorithmIdentifier）→ (modulus, exponent)。
fn parse_rsa_spki(b: &[u8], s: usize, e: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    // AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY }
    let (t1, s1, e1, next1) = read_tlv(b, s)?;
    if t1 != TAG_SEQ || next1 > e {
        return None;
    }
    let (t2, s2, e2, _) = read_tlv(b, s1)?;
    if t2 != TAG_OID || !oid_is(&b[s2..e2], OID_RSA) {
        return None;
    }
    // subjectPublicKey BIT STRING
    let (t3, s3, e3, _) = read_tlv(b, next1)?;
    if t3 != TAG_BITSTR || e3 > e || (e3 - s3) < 1 || b[s3] != 0 {
        return None;
    }
    // 内层 SEQUENCE { INTEGER n, INTEGER e }
    let inner = &b[s3 + 1..e3];
    let (t4, s4, e4, next4) = read_tlv(inner, 0)?;
    if t4 != TAG_SEQ || next4 != inner.len() {
        return None;
    }
    let (tn, sn, en, next_n) = read_tlv(inner, s4)?;
    let (te, se, ee, _) = read_tlv(inner, next_n)?;
    if tn != TAG_INT || te != TAG_INT || ee != e4 {
        return None;
    }
    let n = parse_integer_content(inner, tn, sn, en)?.to_vec();
    let exp = parse_integer_content(inner, te, se, ee)?.to_vec();
    Some((n, exp))
}

/// 解析证书。
pub fn parse_cert(der: &[u8]) -> Option<Cert> {
    let (t0, s0, e0, next0) = read_tlv(der, 0)?;
    if t0 != TAG_SEQ || next0 != der.len() {
        return None;
    }
    // tbsCertificate
    let (t1, s1, e1, next1) = read_tlv(der, s0)?;
    if t1 != TAG_SEQ || next1 > e0 {
        return None;
    }
    // 签名覆盖 tbsCertificate 的完整 TLV 编码（含 tag 与长度头）
    let tbs_tlv = der[s0..e1].to_vec();
    // signatureAlgorithm
    let (t2, s2, e2, next2) = read_tlv(der, next1)?;
    if t2 != TAG_SEQ || next2 > e0 {
        return None;
    }
    let (ta, sa, ea, _) = read_tlv(der, s2)?;
    if ta != TAG_OID {
        return None;
    }
    let sig_alg = der[sa..ea].to_vec();
    // signatureValue BIT STRING
    let (t3, s3, e3, _) = read_tlv(der, next2)?;
    if t3 != TAG_BITSTR || e3 > e0 || e3 - s3 < 1 || der[s3] != 0 {
        return None;
    }
    let sig = der[s3 + 1..e3].to_vec();

    // ---- tbs 内部字段 ----
    let mut pos = s1;
    // [0] version（可选，context-specific tag 0xA0）
    let (tv, _, _, nextv) = read_tlv(der, pos)?;
    if tv == 0xA0 {
        pos = nextv;
    }
    // serialNumber
    let (ts, _, _, nexts) = read_tlv(der, pos)?;
    if ts != TAG_INT {
        return None;
    }
    pos = nexts;
    // signature（AlgorithmIdentifier，忽略——以证书外层为准）
    let (tsg, _, _, nextsg) = read_tlv(der, pos)?;
    if tsg != TAG_SEQ {
        return None;
    }
    pos = nextsg;
    // issuer
    let (ti, si, ei, nexti) = read_tlv(der, pos)?;
    if ti != TAG_SEQ {
        return None;
    }
    let issuer = der[si..ei].to_vec();
    pos = nexti;
    // validity
    let (tv2, sv2, ev2, nextv2) = read_tlv(der, pos)?;
    if tv2 != TAG_SEQ {
        return None;
    }
    let (tt1, st1, et1, nt1) = read_tlv(der, sv2)?;
    let (tt2, st2, et2, _) = read_tlv(der, nt1)?;
    if nt1 > ev2 {
        return None;
    }
    let not_before = parse_time(tt1, &der[st1..et1])?;
    let not_after = parse_time(tt2, &der[st2..et2])?;
    pos = nextv2;
    // subject
    let (tj, sj, ej, nextj) = read_tlv(der, pos)?;
    if tj != TAG_SEQ {
        return None;
    }
    let subject = der[sj..ej].to_vec();
    pos = nextj;
    // subjectPublicKeyInfo
    let (tp, sp, ep, nextp) = read_tlv(der, pos)?;
    if tp != TAG_SEQ || nextp > e1 {
        return None;
    }
    let rsa_key = parse_rsa_spki(der, sp, ep);
    if rsa_key.is_none() {
    } else {
        let (n, e) = rsa_key.as_ref().unwrap();
    }
    pos = nextp;

    // 可选字段与扩展
    let mut san = Vec::new();
    let mut is_ca = false;
    let mut cn = None;
    while pos < e1 {
        let (tx, sx, ex, nextx) = read_tlv(der, pos)?;
        pos = nextx;
        if tx == 0xA3 {
            // extensions ::= SEQUENCE OF Extension
            let (ty, sy, ey, _) = read_tlv(der, sx)?;
            if ty != TAG_SEQ {
                break;
            }
            let mut p = sy;
            while p < ey {
                let (te, se, ee, nx) = read_tlv(der, p)?;
                p = nx;
                if te != TAG_SEQ {
                    continue;
                }
                // OID
                let (to, so, eo, no) = read_tlv(der, se)?;
                if to != TAG_OID {
                    continue;
                }
                // critical BOOLEAN（可选）→ value OCTET STRING。
                // ★ critical 扩展（basicConstraints 等）前置 01 01 FF：跳过
                // BOOLEAN 后必须用**新读到的** TLV 的 tag —— 此前保留 BOOLEAN
                // 的 0x01，tval2 != OCTET 恒成立，critical 扩展全部被静默
                // 跳过（basicConstraints 永远解析不出来）。
                let Some((tval, sval, eval_, _)) = read_tlv(der, no) else {
                    continue;
                };
                let (tval2, sval2, eval2) = if tval == 0x01 {
                    let Some((t2, s2, e2, _)) = read_tlv(der, eval_) else {
                        continue;
                    };
                    (t2, s2, e2)
                } else {
                    (tval, sval, eval_)
                };
                if tval2 != TAG_OCTET {
                    continue;
                }
                if oid_is(&der[so..eo], OID_SAN) {
                    // GeneralNames ::= SEQUENCE OF GeneralName；dNSName = [2] 原文
                    let (tg, sg, eg, _) = read_tlv(der, sval2)?;
                    if tg != TAG_SEQ {
                        continue;
                    }
                    let mut q = sg;
                    while q < eg {
                        let (tn2, sn2, en2, nq) = read_tlv(der, q)?;
                        q = nq;
                        if tn2 == 0x82 {
                            if let Ok(name) = core::str::from_utf8(&der[sn2..en2]) {
                                san.push(name.to_ascii_lowercase());
                            }
                        }
                    }
                }
                // basicConstraints（OID 2.5.29.19 = 55 1D 13）：
                // value 内是 SEQUENCE { BOOLEAN cA DEFAULT FALSE, ... }；
                // 空 SEQUENCE / 缺省 = 非 CA，cA=TRUE（0xFF）= CA。
                if oid_is(&der[so..eo], &[0x55, 0x1D, 0x13]) {
                    if let Some((tb, sb, eb, _)) = read_tlv(der, sval2) {
                        if tb == TAG_SEQ && eb > sb {
                            if let Some((tv, sv, ev, _)) = read_tlv(der, sb) {
                                is_ca = tv == 0x01 && ev == sv + 1 && der[sv] == 0xFF;
                            }
                        }
                    }
                }
            }
        }
    }
    // CN（subject 内 AttributeTypeAndValue；CN OID 2.5.4.3 = 55 04 03）
    {
        let mut p = sj;
        while p < ej {
            let (tr, sr, er, nr) = read_tlv(der, p)?;
            if tr != TAG_SEQ || nr > ej {
                break;
            }
            p = nr;
            let (tat, sat, eat, _) = read_tlv(der, sr)?;
            if tat != TAG_SEQ {
                continue;
            }
            let (to, so, eo, no) = read_tlv(der, sat)?;
            if to == TAG_OID && oid_is(&der[so..eo], &[0x55, 0x04, 0x03]) {
                let (tv, sv, ev, _) = read_tlv(der, no)?;
                if matches!(tv, 0x0C | 0x13 | 0x16) {
                    if let Ok(v) = core::str::from_utf8(&der[sv..ev]) {
                        cn = Some(v.to_ascii_lowercase());
                    }
                }
            }
        }
    }
    Some(Cert {
        tbs_tlv,
        sig_alg,
        sig,
        issuer,
        subject,
        not_before,
        not_after,
        rsa_key,
        san,
        cn,
        is_ca,
    })
}

fn rsa_pub_of(cert: &Cert) -> Option<RsaPub> {
    let (n, e) = cert.rsa_key.as_ref()?;
    let pubk = RsaPub::parse(n, e);
    if pubk.is_some() {
    } else {
    }
    pubk
}

/// 主机名匹配：精确或单标签通配。
fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // 允许匹配 pattern 自身域（*.baidu.com 匹配 baidu.com 与单标签子域）
        if host == suffix {
            return true;
        }
        if let Some(rest) = host.strip_prefix(".") {
            return !rest.is_empty() && !rest.contains('.') && rest == suffix;
        }
        // host 形如 www.baidu.com：取首个 '.' 之后与 suffix 比较
        return match host.find('.') {
            Some(i) => !host[..i].is_empty() && &host[i + 1..] == suffix,
            None => false,
        };
    }
    false
}

/// 验证证书链并返回叶子公钥。
///
/// `chain`：服务器送来的证书（DER），叶子在前。`now`：当前 unix 秒。
/// 调试用：置 true 时跳过链验证（hosttest 对本地 s_server）。
pub static VERIFY_DISABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn verify_chain(chain: &[Vec<u8>], host: &str, now: i64) -> Result<RsaPub, &'static str> {
    let host = host.to_ascii_lowercase();
    if VERIFY_DISABLED.load(core::sync::atomic::Ordering::Relaxed) {
        let der = chain.first().ok_or("服务器未送证书")?;
        let cert = parse_cert(der).ok_or("证书解析失败")?;
        return rsa_pub_of(&cert).ok_or("叶子证书无 RSA 公钥");
    }
    if chain.is_empty() {
        return Err("服务器未送证书");
    }
    let mut certs = Vec::new();
    for der in chain {
        certs.push(parse_cert(der).ok_or("证书解析失败")?);
    }
    let leaf = &certs[0];
    // 有效期
    if now < leaf.not_before {
        return Err("证书尚未生效");
    }
    if now > leaf.not_after {
        return Err("证书已过期");
    }
    // 主机名
    let ok_name = leaf.san.iter().any(|p| host_matches(p, &host))
        || leaf.cn.as_deref().map(|c| host_matches(c, &host)).unwrap_or(false);
    if !ok_name {
        return Err("证书主机名不匹配");
    }

    // 链验证：current 从叶子出发；issuer 优先在内嵌根中找，其次服务器链
    let mut used = vec![false; certs.len()];
    used[0] = true;
    let mut current = 0usize;
    for i in 0..4 {
        let cur = &certs[current];
        // 根优先
        for (j, root) in ROOTS.iter().enumerate() {
            let rc = match parse_cert(root.der) {
                Some(c) => c,
                None => {
                    continue;
                }
            };
            if rc.subject == cur.issuer {
                if verify_sig(cur, &rc) {
                    return rsa_pub_of(leaf).ok_or("叶子证书无 RSA 公钥");
                } else {
                    return Err("根证书验签失败");
                }
            }
        }
        // 服务器送的中间证书
        let mut found = None;
        for (j, c) in certs.iter().enumerate() {
            if !used[j] && c.subject == cur.issuer {
                found = Some(j);
                break;
            }
        }
        match found {
            Some(i) => {
                // 签发者必须是 CA（basicConstraints cA=TRUE）：否则持有效
                // 叶子证书者可用其私钥自签任意域名的伪造叶子，逐级验签
                // 全部通过 → MITM 冒充任何站点。签发者证书自身也必须在
                // 有效期内（叶子查了、中间此前没查）。
                if !certs[i].is_ca {
                    return Err("中间证书非 CA（basicConstraints）");
                }
                if now < certs[i].not_before || now > certs[i].not_after {
                    return Err("中间证书不在有效期内");
                }
                if !verify_sig(&certs[current], &certs[i]) {
                    return Err("中间证书验签失败");
                }
                used[i] = true;
                current = i;
            }
            None => {
                return Err("找不到签发者证书（根缺失）");
            }
        }
    }
    Err("证书链过长")
}

/// 用 `issuer` 的 RSA 公钥验证 `cert` 的签名（tbs + sha256WithRSA）。
fn verify_sig(cert: &Cert, issuer: &Cert) -> bool {
    if !oid_is(&cert.sig_alg, OID_SHA256_RSA) {
        return false; // 链签名算法不在支持集内
    }
    let key = match rsa_pub_of(issuer) {
        Some(k) => {
            k
        },
        None => {
            return false;
        }
    };
    let hash = sha256::sha256(&cert.tbs_tlv);
    let result = key.verify_pkcs1_sha256(&hash, &cert.sig);
    result
}

/// 写入 DER TLV 格式。
pub fn write_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    if content.len() < 128 {
        vec![tag, content.len() as u8]
    } else {
        // 长格式：大端长度字节压栈（低位在前）再整体反转，前缀 0x80|字节数。
        let mut len_bytes = Vec::new();
        let mut n = content.len();
        while n > 0 {
            len_bytes.push((n & 0xFF) as u8);
            n >>= 8;
        }
        len_bytes.reverse();
        let mut result = vec![tag, 0x80 | len_bytes.len() as u8];
        result.extend(len_bytes);
        result.extend(content);
        result
    }
}

