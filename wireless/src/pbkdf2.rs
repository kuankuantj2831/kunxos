//! PBKDF2（RFC 8018）：WPA/WPA2-PSK 的密钥派生（PBKDF2-HMAC-SHA1，4096 轮）。
//! 输出 32 字节 PMK。

use crate::hmac::hmac_sha1;

/// PBKDF2-HMAC-SHA1：从口令 `password` 与盐 `salt` 派生 `dk_len` 字节。
/// WPA2-PSK 惯例：`password` = 8..63 字节 ASCII，`salt` = SSID，4096 轮，输出 32 字节。
pub fn pbkdf2_sha1(password: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> [u8; 64] {
    let mut out = [0u8; 64];
    let n_blocks = dk_len.div_ceil(20);
    debug_assert!(n_blocks <= 4, "PBKDF2-SHA1 最多输出 64 字节");
    for block in 1..=n_blocks {
        // U1 = PRF(P, S || INT(block_index))
        let mut salt_block = [0u8; 256];
        let mut salt_len = salt.len().min(salt_block.len() - 4);
        salt_block[..salt_len].copy_from_slice(&salt[..salt_len]);
        salt_block[salt_len..salt_len + 4].copy_from_slice(&(block as u32).to_be_bytes());
        salt_len += 4;

        let mut u = hmac_sha1(password, &salt_block[..salt_len]);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha1(password, &u);
            for i in 0..20 {
                t[i] ^= u[i];
            }
        }
        let dst = (block - 1) * 20;
        let n = 20.min(dk_len - dst);
        out[dst..dst + n].copy_from_slice(&t[..n]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6070 第 2 条（iterations=1）与第 3 条（iterations=2）。
    #[test]
    fn pbkdf2_rfc6070() {
        let dk = pbkdf2_sha1(b"password", b"salt", 1, 20);
        assert_eq!(
            &dk[..20],
            &[
                0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf,
                0x60, 0x12, 0x06, 0x2f, 0xe0, 0x37, 0xa6
            ]
        );
        let dk2 = pbkdf2_sha1(b"password", b"salt", 2, 20);
        assert_eq!(
            &dk2[..20],
            &[
                0xea, 0x6c, 0x01, 0x4d, 0xc7, 0x2d, 0x6f, 0x8c, 0xcd, 0x1e, 0xd9, 0x2a, 0xce,
                0x1d, 0x41, 0xf0, 0xd8, 0xde, 0x89, 0x57
            ]
        );
    }

    // 经典 WPA2-PSK 测试向量：SSID="test", 口令="password" → PMK（32 字节）。
    // 该值经宿主 Python hashlib.pbkdf2_hmac('sha1', b'password', b'test', 4096, 32) 独立验证。
    #[test]
    fn wpa2_psk_pmk_vector() {
        let pmk = pbkdf2_sha1(b"password", b"test", 4096, 32);
        let expected = [
            0x9a, 0x15, 0xed, 0x29, 0xa9, 0xb8, 0x0e, 0x5d, 0x52, 0x32, 0xa0, 0x64, 0x4c, 0xfd,
            0x40, 0x4b, 0x83, 0x97, 0x9b, 0x57, 0xaf, 0x83, 0x05, 0x80, 0x6d, 0xd4, 0xd4, 0x86,
            0x50, 0x06, 0xed, 0x7d,
        ];
        assert_eq!(&pmk[..32], &expected);
    }
}
