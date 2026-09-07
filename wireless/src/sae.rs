//! WPA3-SAE（Simultaneous Authentication of Equals，IEEE 802.11-2016 §12.4）。
//!
//! 基于 P-256（群 19），实现与 hostap `src/common/sae.c` 一致的算法：
//! - PWE 派生：pwd-seed = HMAC-SHA256(MAX||MIN MAC, password || counter)，
//!   pwd-value = KDF-z(pwd-seed, "SAE Hunting and Pecking", p)（hunting-and-pecking）
//! - Commit：scalar = (rand + mask) mod n；element = −(mask·PWE)
//! - K 派生：keyseed = HKDF-Extract(0, K)；KCK||PMK = KDF(keyseed, "SAE KCK and PMK",
//!   (own_scalar + peer_scalar) mod n)
//! - Confirm：HMAC-SHA256(KCK, send-confirm(2B LE) || own 组 || peer 组)

use crate::p256::{fe_mul, fe_sub, Point, ORDER, scalar_mul, to_be_bytes};

/// P-256 域 p。
const P: [u64; 4] = [
    0xffff_ffff_ffff_ffff,
    0x0000_0000_ffff_ffff,
    0x0000_0000_0000_0000,
    0xffff_ffff_0000_0001,
];

/// (p+1)/4（用于 sqrt：p ≡ 3 mod 4）。
const P_PLUS_1_OVER_4: [u64; 4] = [
    0x0000_0000_0000_0000,
    0x0000_0000_4000_0000,
    0x4000_0000_0000_0000,
    0x3fff_ffff_c000_0000,
];

/// P-256 曲线 b。
const B: [u64; 4] = [
    0x3bce_3c3e_27d2_604b,
    0x651d_06b0_cc53_b0f6,
    0xb3eb_bd55_7698_86bc,
    0x5ac6_35d8_aa3a_93e7,
];

fn be_to_fe(b: &[u8; 32]) -> [u64; 4] {
    let mut v = [0u64; 4];
    for i in 0..4 {
        let mut word: u64 = 0;
        for j in 0..8 {
            word = (word << 8) | b[i * 8 + j] as u64;
        }
        v[3 - i] = word;
    }
    v
}

fn fe_less_than(a: &[u64; 4], b: &[u64; 4]) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

fn p_be() -> [u8; 32] {
    to_be_bytes(&P)
}

/// NIST SP 800-108 KDF（counter 模式，大端 counter 与 length）：
/// K_i = HMAC-SHA256(key, counter(2B BE) || label || 0x00 || context || L(2B BE))。
pub fn kdf_z(key: &[u8], label: &[u8], context: &[u8], out_len: usize) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut produced = 0usize;
    let mut counter: u16 = 1;
    while produced < out_len {
        let mut msg = [0u8; 300];
        let mut n = 0usize;
        msg[n..n + 2].copy_from_slice(&counter.to_be_bytes());
        n += 2;
        let llen = label.len().min(64);
        msg[n..n + llen].copy_from_slice(&label[..llen]);
        n += llen;
        msg[n] = 0;
        n += 1;
        let clen = context.len().min(200);
        msg[n..n + clen].copy_from_slice(&context[..clen]);
        n += clen;
        msg[n..n + 2].copy_from_slice(&(out_len as u16).to_be_bytes());
        n += 2;
        let h = crate::hmac::hmac_sha256(key, &msg[..n]);
        let take = 32.min(out_len - produced);
        out[produced..produced + take].copy_from_slice(&h[..take]);
        produced += take;
        counter = counter.wrapping_add(1);
    }
    out
}

/// 曲线 y² = x³ − 3x + b 的平方根（p ≡ 3 mod 4）：y = a^((p+1)/4)。
fn sqrt_mod_p(a: &[u64; 4]) -> Option<[u64; 4]> {
    let y = crate::p256::pow_mod(a, &P_PLUS_1_OVER_4);
    let y2 = fe_mul(&y, &y);
    if y2 == *a {
        Some(y)
    } else {
        None
    }
}

/// hunting-and-pecking 派生口令元素（hostap sae_derive_pwe 等价）。
/// `addr1`/`addr2` 为双方 MAC；`password` 口令。
/// 返回 (PWE, counter)。
pub fn derive_pwe(addr1: &[u8; 6], addr2: &[u8; 6], password: &[u8]) -> Option<(Point, u32)> {
    // addrs = MAX(addr1, addr2) || MIN(addr1, addr2)
    let mut addrs = [0u8; 12];
    if addr1 > addr2 {
        addrs[..6].copy_from_slice(addr1);
        addrs[6..].copy_from_slice(addr2);
    } else {
        addrs[..6].copy_from_slice(addr2);
        addrs[6..].copy_from_slice(addr1);
    }
    let p_bytes = p_be();
    for counter in 1u32..=200 {
        // pwd-seed = HMAC-SHA256(addrs, password || counter(1B))
        let mut seed_msg = [0u8; 100];
        let plen = password.len().min(64);
        seed_msg[..plen].copy_from_slice(&password[..plen]);
        seed_msg[plen] = counter as u8;
        let pwd_seed = crate::hmac::hmac_sha256(&addrs, &seed_msg[..plen + 1]);
        // pwd-value = KDF-z(pwd-seed, "SAE Hunting and Pecking", p)
        let pv = kdf_z(&pwd_seed, b"SAE Hunting and Pecking", &p_bytes, 32);
        let mut pv32 = [0u8; 32];
        pv32.copy_from_slice(&pv[..32]);
        let x = be_to_fe(&pv32);
        if !fe_less_than(&x, &P) {
            continue;
        }
        // y² = x³ − 3x + b
        let x2 = fe_mul(&x, &x);
        let x3 = fe_mul(&x2, &x);
        let three_x = fe_mul(&[3u64, 0, 0, 0], &x);
        let rhs = crate::p256::fe_add(&fe_sub(&x3, &three_x), &B);
        if let Some(mut y) = sqrt_mod_p(&rhs) {
            // y 的奇偶性与 pwd-seed 最低位对齐
            let y_lsb = pwd_seed[31] & 1;
            if to_be_bytes(&y)[31] & 1 != y_lsb {
                y = fe_sub(&P, &y);
            }
            let pwe = Point::new(x, y);
            if Point::on_curve(&x, &y) {
                return Some((pwe, counter));
            }
        }
    }
    None
}

/// SAE 会话（一方）。
#[derive(Clone, Copy)]
pub struct SaeSession {
    pub scalar: [u8; 32],
    pub element: Point,
    pub pwe: Point,
    pub kck: [u8; 32],
    pub pmk: [u8; 32],
    pub send_confirm: u16,
}

/// 生成 Commit：随机数 `rand`（私密），掩码 `mask`，口令元素 `pwe`。
/// 返回 (session, commit 96 字节：scalar || element_x || element_y)。
pub fn make_commit(rand: &[u8; 32], mask: &[u8; 32], pwe: &Point) -> (SaeSession, [u8; 96]) {
    let r = be_to_fe(rand);
    let m = be_to_fe(mask);
    // scalar = (r + m) mod n
    let mut scalar = crate::p256::fe_add(&r, &m);
    if !fe_less_than(&scalar, &ORDER) {
        scalar = fe_sub(&scalar, &ORDER);
    }
    if !fe_less_than(&scalar, &ORDER) {
        scalar = fe_sub(&scalar, &ORDER);
    }
    let scalar_bytes = to_be_bytes(&scalar);
    // element = −(mask·PWE)
    let mp = scalar_mul(&m, pwe);
    let (mx, my) = mp.to_affine().expect("mask·PWE == O");
    let neg_my = fe_sub(&P, &my);

    let mut commit = [0u8; 96];
    commit[..32].copy_from_slice(&scalar_bytes);
    commit[32..64].copy_from_slice(&to_be_bytes(&mx));
    commit[64..96].copy_from_slice(&to_be_bytes(&neg_my));

    let sess = SaeSession {
        scalar: scalar_bytes,
        element: Point::new(mx, neg_my),
        pwe: *pwe,
        kck: [0u8; 32],
        pmk: [0u8; 32],
        send_confirm: 0,
    };
    (sess, commit)
}

/// 解析对端 Commit：返回 (peer_scalar, peer_element)。
pub fn parse_commit(commit: &[u8]) -> Option<([u8; 32], Point)> {
    if commit.len() < 96 {
        return None;
    }
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&commit[..32]);
    let x = be_to_fe(&commit[32..64].try_into().ok()?);
    let y = be_to_fe(&commit[64..96].try_into().ok()?);
    if !Point::on_curve(&x, &y) {
        return None;
    }
    let elem = Point::new(x, y);
    if elem.is_infinity() {
        return None;
    }
    Some((scalar, elem))
}

/// 收到对端 Commit 后派生 KCK/PMK（hostap sae_process_commit 等价）。
/// `rand` 为本端 Commit 的私密随机数。
pub fn on_peer_commit(
    sess: &mut SaeSession,
    rand: &[u8; 32],
    peer: &[u8; 32],
    peer_elem: &Point,
) -> bool {
    // K = rand · (peer_scalar·PWE + peer_element)（hostap sae_derive_k_ecc 等价）
    let peer_scalar_fe = be_to_fe(peer);
    let t1 = scalar_mul(&peer_scalar_fe, &sess.pwe);
    let t2 = crate::p256::point_add(&t1, peer_elem);
    let r = be_to_fe(rand);
    let k = scalar_mul(&r, &t2);
    let (kx, _) = match k.to_affine() {
        Some(v) => v,
        None => return false,
    };
    let k_bytes = to_be_bytes(&kx);
    // keyseed = HKDF-Extract(0x00*32, K) = HMAC-SHA256(0x00*32, K)
    let keyseed = crate::hmac::hmac_sha256(&[0u8; 32], &k_bytes);
    // val = (own_scalar + peer_scalar) mod n
    let own = be_to_fe(&sess.scalar);
    let peer_fe = be_to_fe(peer);
    let mut val = crate::p256::fe_add(&own, &peer_fe);
    if !fe_less_than(&val, &ORDER) {
        val = fe_sub(&val, &ORDER);
    }
    if !fe_less_than(&val, &ORDER) {
        val = fe_sub(&val, &ORDER);
    }
    let val_bytes = to_be_bytes(&val);
    // KCK || PMK = KDF-Hash-Length(keyseed, "SAE KCK and PMK", val)
    let keys = kdf_z(&keyseed, b"SAE KCK and PMK", &val_bytes, 64);
    sess.kck.copy_from_slice(&keys[..32]);
    sess.pmk.copy_from_slice(&keys[32..64]);
    true
}

/// 生成 Confirm：HMAC-SHA256(KCK, send_confirm(2B LE) || own_scalar || own_element ||
/// peer_scalar || peer_element)。
pub fn make_confirm(sess: &SaeSession, peer: &[u8; 32], peer_elem: &Point) -> [u8; 32] {
    let (ex, ey) = sess.element.to_affine().expect("own element == O");
    let (peer_ex, peer_ey) = peer_elem.to_affine().expect("peer element == O");
    let mut buf = [0u8; 260];
    let mut n = 0usize;
    buf[n..n + 2].copy_from_slice(&sess.send_confirm.to_le_bytes());
    n += 2;
    buf[n..n + 32].copy_from_slice(&sess.scalar);
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&ex));
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&ey));
    n += 32;
    buf[n..n + 32].copy_from_slice(peer);
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&peer_ex));
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&peer_ey));
    n += 32;
    crate::hmac::hmac_sha256(&sess.kck, &buf[..n])
}

/// 校验对端 Confirm：用对端的 send_confirm 与（对端组在前）重算。
pub fn verify_confirm(
    sess: &SaeSession,
    peer_sc: u16,
    peer: &[u8; 32],
    peer_elem: &Point,
    peer_confirm: &[u8; 32],
) -> bool {
    let (ex, ey) = sess.element.to_affine().expect("own element == O");
    let (peer_ex, peer_ey) = peer_elem.to_affine().expect("peer element == O");
    let mut buf = [0u8; 260];
    let mut n = 0usize;
    buf[n..n + 2].copy_from_slice(&peer_sc.to_le_bytes());
    n += 2;
    buf[n..n + 32].copy_from_slice(peer);
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&peer_ex));
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&peer_ey));
    n += 32;
    buf[n..n + 32].copy_from_slice(&sess.scalar);
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&ex));
    n += 32;
    buf[n..n + 32].copy_from_slice(&to_be_bytes(&ey));
    n += 32;
    let expected = crate::hmac::hmac_sha256(&sess.kck, &buf[..n]);
    expected == *peer_confirm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn macs() -> ([u8; 6], [u8; 6]) {
        ([0x00, 0x11, 0x22, 0x33, 0x44, 0x55], [0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb])
    }

    #[test]
    fn pwe_derived_on_curve() {
        let (a, b) = macs();
        let (pwe, counter) = derive_pwe(&a, &b, b"password").expect("pwe");
        assert!(Point::on_curve(&pwe.x, &pwe.y));
        assert!(counter >= 1);
        let (pwe2, _) = derive_pwe(&a, &b, b"password").expect("pwe2");
        assert_eq!(pwe.x, pwe2.x);
        assert_eq!(pwe.y, pwe2.y);
    }

    /// 完整 SAE 双方握手：双方独立派生 PWE，交换 Commit/Confirm，PMK 一致。
    #[test]
    fn sae_full_handshake() {
        let (mac_a, mac_b) = macs();
        let (pwe_a, _) = derive_pwe(&mac_a, &mac_b, b"password").unwrap();
        let (pwe_b, _) = derive_pwe(&mac_a, &mac_b, b"password").unwrap();
        assert_eq!(pwe_a.x, pwe_b.x);

        let rand_a = [0x01u8; 32];
        let mask_a = [0x02u8; 32];
        let rand_b = [0x03u8; 32];
        let mask_b = [0x04u8; 32];

        let (mut sess_a, commit_a) = make_commit(&rand_a, &mask_a, &pwe_a);
        let (mut sess_b, commit_b) = make_commit(&rand_b, &mask_b, &pwe_b);
        sess_a.send_confirm = 1;
        sess_b.send_confirm = 2;

        let (scalar_b, elem_b) = parse_commit(&commit_b).unwrap();
        let (scalar_a, elem_a) = parse_commit(&commit_a).unwrap();

        assert!(on_peer_commit(&mut sess_a, &rand_a, &scalar_b, &elem_b));
        assert!(on_peer_commit(&mut sess_b, &rand_b, &scalar_a, &elem_a));

        // 双方 PMK / KCK 必须一致
        assert_eq!(sess_a.pmk, sess_b.pmk);
        assert_eq!(sess_a.kck, sess_b.kck);

        let conf_a = make_confirm(&sess_a, &scalar_b, &elem_b);
        let conf_b = make_confirm(&sess_b, &scalar_a, &elem_a);

        assert!(verify_confirm(&sess_a, sess_b.send_confirm, &scalar_b, &elem_b, &conf_b));
        assert!(verify_confirm(&sess_b, sess_a.send_confirm, &scalar_a, &elem_a, &conf_a));
        // 篡改检测
        let mut bad = conf_b;
        bad[0] ^= 1;
        assert!(!verify_confirm(&sess_a, sess_b.send_confirm, &scalar_b, &elem_b, &bad));
    }
}
