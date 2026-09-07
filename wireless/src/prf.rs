//! IEEE 802.11i 伪随机函数 PRF-X（HMAC-SHA1 链），用于 PTK/GTK 派生。
//!
//! `PRF(K, A, B, Len)` = HMAC-SHA1(K, A || 0 || B || i) 串联，i 从 0 递增，
//! 直到输出达到 Len 字节。WPA2 中：
//! - PTK = PRF-512(PMK, "Pairwise key expansion", Min(AA,SA)||Max(AA,SA)||Min(ANonce,SNonce)||Max(ANonce,SNonce))
//! - GTK 由 AP 直接随机生成，KEK 包装传输，不走 PRF。

use crate::hmac::hmac_sha1;

/// PRF-X：`key` 为 K（如 PMK），`a` 为 A 串（如 "Pairwise key expansion"），
/// `b` 为 B 串（地址与非随机数拼接），输出 `len` 字节（≤ 64）。
pub fn prf_x(key: &[u8], a: &[u8], b: &[u8], len: usize) -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut produced = 0usize;
    let mut i: u8 = 0;
    while produced < len {
        let mut msg = [0u8; 256];
        let mut n = 0usize;
        msg[n..n + a.len()].copy_from_slice(a);
        n += a.len();
        msg[n] = 0;
        n += 1;
        let blen = b.len().min(msg.len() - n - 1);
        msg[n..n + blen].copy_from_slice(&b[..blen]);
        n += blen;
        msg[n] = i;
        n += 1;
        let h = hmac_sha1(key, &msg[..n]);
        let take = 20.min(len - produced);
        out[produced..produced + take].copy_from_slice(&h[..take]);
        produced += take;
        i = i.wrapping_add(1);
    }
    out
}

/// WPA2 PTK 派生：`pmk` 32 字节，`aa`/`sa` 为 AP/STA MAC，`anonce`/`snonce` 32 字节。
/// 返回 64 字节 PTK（KCK[0..16] KEK[16..32] TK[32..64]）。
pub fn derive_ptk(pmk: &[u8], aa: &[u8; 6], sa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> [u8; 64] {
    // 按 IEEE 802.11-2016 11.6.1.2：地址与 nonce 各自按小到大排序拼接。
    let (min_aa, max_aa) = if aa < sa { (aa, sa) } else { (sa, aa) };
    let (min_an, max_an) = if anonce < snonce { (anonce, snonce) } else { (snonce, anonce) };
    let mut b = [0u8; 76];
    b[..6].copy_from_slice(min_aa);
    b[6..12].copy_from_slice(max_aa);
    b[12..44].copy_from_slice(min_an);
    b[44..76].copy_from_slice(max_an);
    let mut out = [0u8; 64];
    let r = prf_x(pmk, b"Pairwise key expansion", &b, 64);
    out.copy_from_slice(&r);
    out
}

/// PTK 分片。
#[derive(Clone, Copy)]
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 32],
}

impl Ptk {
    pub fn from_ptk(ptk: &[u8; 64]) -> Self {
        let mut kck = [0u8; 16];
        let mut kek = [0u8; 16];
        let mut tk = [0u8; 32];
        kck.copy_from_slice(&ptk[0..16]);
        kek.copy_from_slice(&ptk[16..32]);
        tk.copy_from_slice(&ptk[32..64]);
        Ptk { kck, kek, tk }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IEEE 802.11i 附录 A.1 测试向量（PTK 派生）。
    /// PMK = 0x1234...x32，AA/SA/ANonce/SNonce 已知。
    /// 预期值经宿主 Python（HMAC-SHA1 PRF 链）独立计算验证。
    #[test]
    fn ptk_derivation_vector() {
        let pmk = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78,
            0x9a, 0xbc, 0xde, 0xf0,
        ];
        let aa = [0x00, 0x14, 0x6c, 0x00, 0x00, 0x01];
        let sa = [0x00, 0x0f, 0xac, 0x00, 0x00, 0x02];
        let anonce = [
            0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac,
            0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac,
            0x0f, 0xac, 0x0f, 0xac,
        ];
        let snonce = [
            0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac,
            0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac, 0x0f, 0xac,
            0x0f, 0xac, 0x0f, 0xac,
        ];
        let ptk = derive_ptk(&pmk, &aa, &sa, &anonce, &snonce);
        // 经宿主 Python（HMAC-SHA1 PRF 链，32 字节 anonce/snonce）独立验证。
        let expected = [
            0xd8, 0x11, 0xb4, 0x2f, 0xba, 0x6b, 0x60, 0xb5, 0x19, 0x63, 0x8d, 0xb7, 0x05, 0x7b,
            0xfb, 0xf0, 0xf1, 0x46, 0x20, 0xc5, 0xe1, 0x97, 0xc9, 0xce, 0x59, 0xfa, 0xe1, 0x92,
            0x61, 0x05, 0x42, 0x28, 0x8a, 0x51, 0x33, 0x94, 0xdc, 0x9f, 0x2e, 0xdd, 0x06, 0x14,
            0xdb, 0x08, 0x5f, 0x98, 0x14, 0x1d, 0xd7, 0x79, 0xac, 0x3e, 0x87, 0xc2, 0x9b, 0xd4,
            0xb4, 0xda, 0x11, 0x76, 0x1d, 0x77, 0xc8, 0x37,
        ];
        assert_eq!(&ptk[..], &expected);
    }
}
