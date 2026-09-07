//! AES Key Wrap（RFC 3394）：WPA2 4-way handshake 中 AP 用 KEK 加密 GTK 的 KDE 载荷。

use crate::aes::encrypt_block;

const DEFAULT_IV: [u8; 8] = [0xa6, 0xa6, 0xa6, 0xa6, 0xa6, 0xa6, 0xa6, 0xa6];

/// 用 KEK 包装 `plain`（长度必须是 8 的倍数且 ≥ 16），输出 `plain.len() + 8` 字节。
pub fn aes_key_wrap(kek: &[u8], plain: &[u8]) -> Option<[u8; 48]> {
    if plain.len() < 16 || plain.len() % 8 != 0 || plain.len() + 8 > 48 {
        return None;
    }
    let n = plain.len() / 8;
    let mut a = DEFAULT_IV;
    let mut r = [0u8; 40]; // 最多 5 个 64 位块
    r[..plain.len()].copy_from_slice(plain);
    for j in 0..6 {
        for i in 1..=n {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(&r[(i - 1) * 8..i * 8]);
            let c = encrypt_block(kek, &b);
            let t = n as u32 * j as u32 + i as u32;
            a[..8].copy_from_slice(&c[..8]);
            // A = MSB64(C) ^ t
            let t_bytes = t.to_be_bytes();
            for k in 0..4 {
                a[4 + k] ^= t_bytes[k];
            }
            r[(i - 1) * 8..i * 8].copy_from_slice(&c[8..]);
        }
    }
    let mut out = [0u8; 48];
    out[..8].copy_from_slice(&a);
    out[8..8 + plain.len()].copy_from_slice(&r[..plain.len()]);
    Some(out)
}

/// 解包：输入 `cipher`（长度 ≥ 24 且为 8 的倍数），返回明文长度 = cipher.len() - 8。
/// 校验失败返回 `None`。
pub fn aes_key_unwrap(kek: &[u8], cipher: &[u8]) -> Option<[u8; 40]> {
    if cipher.len() < 24 || cipher.len() % 8 != 0 || cipher.len() > 48 {
        return None;
    }
    let n = cipher.len() / 8 - 1;
    let mut a = [0u8; 8];
    a.copy_from_slice(&cipher[..8]);
    let mut r = [0u8; 40];
    r[..cipher.len() - 8].copy_from_slice(&cipher[8..]);
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let t = n as u32 * j as u32 + i as u32;
            let t_bytes = t.to_be_bytes();
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            for k in 0..4 {
                b[4 + k] ^= t_bytes[k];
            }
            b[8..].copy_from_slice(&r[(i - 1) * 8..i * 8]);
            let p = crate::aes::decrypt_block(kek, &b);
            a.copy_from_slice(&p[..8]);
            r[(i - 1) * 8..i * 8].copy_from_slice(&p[8..]);
        }
    }
    if a != DEFAULT_IV {
        return None;
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 3394 4.1：KEK = 000102...1f，明文 = 001122...1f（16 字节）。
    #[test]
    fn rfc3394_example() {
        let kek: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plain: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let wrapped = aes_key_wrap(&kek, &plain).unwrap();
        assert_eq!(
            &wrapped[..24],
            &[
                0x1f, 0xa6, 0x8b, 0x0a, 0x81, 0x12, 0xb4, 0x47, 0xae, 0xf3, 0x4b, 0xd8, 0xfb,
                0x5a, 0x7b, 0x82, 0x9d, 0x3e, 0x86, 0x23, 0x71, 0xd2, 0xcf, 0xe5
            ]
        );
        let unwrapped = aes_key_unwrap(&kek, &wrapped[..24]).unwrap();
        assert_eq!(&unwrapped[..16], &plain);
    }

    #[test]
    fn roundtrip_gtk() {
        let kek = [0x42u8; 16];
        let gtk = [0x11u8; 32]; // 32 字节 GTK（AES-CCMP 128）
        let wrapped = aes_key_wrap(&kek, &gtk).unwrap();
        let unwrapped = aes_key_unwrap(&kek, &wrapped[..40]).unwrap();
        assert_eq!(&unwrapped[..32], &gtk);
    }
}
