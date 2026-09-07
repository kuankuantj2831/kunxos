//! HMAC（RFC 2104 / RFC 4231）：HMAC-SHA1 与 HMAC-SHA256。
//! 用于 WPA 802.11 PRF、EAPOL-Key MIC、PBKDF2、蓝牙 SMP f5 等。

use crate::sha1::Sha1;
use crate::sha2::Sha256;

const BLOCK_SHA1: usize = 64;
const BLOCK_SHA256: usize = 64;

/// HMAC-SHA1，输出 20 字节。
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut k = [0u8; BLOCK_SHA1];
    if key.len() > BLOCK_SHA1 {
        k[..20].copy_from_slice(&crate::sha1::sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_SHA1];
    let mut opad = [0x5cu8; BLOCK_SHA1];
    for i in 0..BLOCK_SHA1 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finalize()
}

/// HMAC-SHA256，输出 32 字节。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; BLOCK_SHA256];
    if key.len() > BLOCK_SHA256 {
        k[..32].copy_from_slice(&crate::sha2::sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_SHA256];
    let mut opad = [0x5cu8; BLOCK_SHA256];
    for i in 0..BLOCK_SHA256 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4231 第 2 条（SHA-256）与 RFC 2202 第 2 条（SHA-1）。
    #[test]
    fn hmac_sha1_rfc2202_case1() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hmac_sha1(&key, b"Hi There"),
            [
                0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb,
                0x37, 0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00
            ]
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        assert_eq!(
            hmac_sha256(&key, b"Hi There"),
            [
                0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf,
                0x0b, 0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9,
                0x37, 0x6c, 0x2e, 0x32, 0xcf, 0xf7
            ]
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_case2() {
        assert_eq!(
            hmac_sha256(b"Jefe", b"what do ya want for nothing?"),
            [
                0x5b, 0xdc, 0xc1, 0x46, 0xbf, 0x60, 0x75, 0x4e, 0x6a, 0x04, 0x24, 0x26, 0x08,
                0x95, 0x75, 0xc7, 0x5a, 0x00, 0x3f, 0x08, 0x9d, 0x27, 0x39, 0x83, 0x9d, 0xec,
                0x58, 0xb9, 0x64, 0xec, 0x38, 0x43
            ]
        );
    }
}
