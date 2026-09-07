//! AES-CMAC（RFC 4493 / NIST SP 800-38B）。用于蓝牙 LE SMP 的 f4/f6 函数与
//! (在 802.11 中 WPA3 亦使用) 相关 MAC 场景。

use crate::aes::encrypt_block;

fn left_shift_one_bit(input: &[u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut overflow: u8 = 0;
    for i in (0..16).rev() {
        out[i] = (input[i] << 1) | overflow;
        overflow = input[i] >> 7;
    }
    out
}

/// 生成子密钥 K1 / K2。
fn generate_subkeys(key: &[u8]) -> ([u8; 16], [u8; 16]) {
    let l = encrypt_block(key, &[0u8; 16]);
    let k1 = if l[0] & 0x80 != 0 {
        let mut v = left_shift_one_bit(&l);
        v[15] ^= 0x87;
        v
    } else {
        left_shift_one_bit(&l)
    };
    let k2 = if k1[0] & 0x80 != 0 {
        let mut v = left_shift_one_bit(&k1);
        v[15] ^= 0x87;
        v
    } else {
        left_shift_one_bit(&k1)
    };
    (k1, k2)
}

/// AES-CMAC-128：`key` 16 字节，`msg` 任意长度，输出 16 字节 MAC（RFC 4493）。
pub fn aes_cmac(key: &[u8], msg: &[u8]) -> [u8; 16] {
    let (k1, k2) = generate_subkeys(key);

    // 计算完整块数 n 与最后一块内容 M_last。
    let n = msg.len().div_ceil(16).max(1);
    let complete = msg.len() > 0 && msg.len() % 16 == 0;
    let mut m_last = [0u8; 16];
    if msg.len() == 0 {
        // M_len = 0：M_last = 0^127 || 1，使用 K2。
        m_last[0] = 0x80;
    } else if complete {
        m_last.copy_from_slice(&msg[msg.len() - 16..]);
    } else {
        let rem = msg.len() % 16;
        m_last[..rem].copy_from_slice(&msg[msg.len() - rem..]);
        m_last[rem] = 0x80;
    }

    // X = 0；依次处理前 n-1 个完整块。
    let mut x = [0u8; 16];
    for i in 0..n - 1 {
        let mut xored = [0u8; 16];
        for j in 0..16 {
            xored[j] = msg[i * 16 + j] ^ x[j];
        }
        x = encrypt_block(key, &xored);
    }

    // T = AES_K(X ⊕ M_last ⊕ K1/K2)。
    let mut final_block = [0u8; 16];
    let sub = if complete { k1 } else { k2 };
    for j in 0..16 {
        final_block[j] = x[j] ^ m_last[j] ^ sub[j];
    }
    encrypt_block(key, &final_block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    // RFC 4493 测试向量。
    #[test]
    fn cmac_rfc4493() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let empty: [u8; 0] = [];
        assert_eq!(
            aes_cmac(&key, &empty),
            [
                0xbb, 0x1d, 0x69, 0x29, 0xe9, 0x59, 0x37, 0x28, 0x7f, 0xa3, 0x7d, 0x12, 0x9b,
                0x75, 0x67, 0x46
            ]
        );
    }

    #[test]
    fn cmac_rfc4493_hex_message() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let msg: [u8; 16] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        assert_eq!(
            aes_cmac(&key, &msg),
            [
                0x07, 0x0a, 0x16, 0xb4, 0x6b, 0x4d, 0x41, 0x44, 0xf7, 0x9b, 0xdd, 0x9d, 0xd0,
                0x4a, 0x28, 0x7c
            ]
        );
        // RFC 4493 40 字节消息（0x6bc1bee2... 前 40 字节）→ MAC。
        let msg40: [u8; 40] = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a, 0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac,
            0x45, 0xaf, 0x8e, 0x51, 0x30, 0xc8, 0x1c, 0x46, 0xa3, 0x5c, 0xe4, 0x11,
        ];
        assert_eq!(
            aes_cmac(&key, &msg40),
            [
                0xdf, 0xa6, 0x67, 0x47, 0xde, 0x9a, 0xe6, 0x30, 0x30, 0xca, 0x32, 0x61, 0x14,
                0x97, 0xc8, 0x27
            ]
        );
    }
}


