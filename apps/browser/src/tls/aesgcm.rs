//! AES-128（仅加密方向）+ GCM（NIST SP 800-38D）—— TLS 1.2
//! `ECDHE_RSA_AES128_GCM_SHA256` 的记录层加密。
//!
//! GCM 只需要 AES 加密函数（解密同向），故不实现解密轮。
//! GHASH 用 32×16 的 4-bit 查表（H·x^{4k}·n，共 8KB，连接级构建一次），
//! 大页面（数百 KB）吞吐优于逐位乘。

use alloc::boxed::Box;

/// GF(2^8) S-box（FIPS-197；生成见 tools/ 注释：逆元 + 仿射变换）。
const SBOX: [u8; 256] = [
    0x63, 0x7C, 0x77, 0x7B, 0xF2, 0x6B, 0x6F, 0xC5,
    0x30, 0x01, 0x67, 0x2B, 0xFE, 0xD7, 0xAB, 0x76,
    0xCA, 0x82, 0xC9, 0x7D, 0xFA, 0x59, 0x47, 0xF0,
    0xAD, 0xD4, 0xA2, 0xAF, 0x9C, 0xA4, 0x72, 0xC0,
    0xB7, 0xFD, 0x93, 0x26, 0x36, 0x3F, 0xF7, 0xCC,
    0x34, 0xA5, 0xE5, 0xF1, 0x71, 0xD8, 0x31, 0x15,
    0x04, 0xC7, 0x23, 0xC3, 0x18, 0x96, 0x05, 0x9A,
    0x07, 0x12, 0x80, 0xE2, 0xEB, 0x27, 0xB2, 0x75,
    0x09, 0x83, 0x2C, 0x1A, 0x1B, 0x6E, 0x5A, 0xA0,
    0x52, 0x3B, 0xD6, 0xB3, 0x29, 0xE3, 0x2F, 0x84,
    0x53, 0xD1, 0x00, 0xED, 0x20, 0xFC, 0xB1, 0x5B,
    0x6A, 0xCB, 0xBE, 0x39, 0x4A, 0x4C, 0x58, 0xCF,
    0xD0, 0xEF, 0xAA, 0xFB, 0x43, 0x4D, 0x33, 0x85,
    0x45, 0xF9, 0x02, 0x7F, 0x50, 0x3C, 0x9F, 0xA8,
    0x51, 0xA3, 0x40, 0x8F, 0x92, 0x9D, 0x38, 0xF5,
    0xBC, 0xB6, 0xDA, 0x21, 0x10, 0xFF, 0xF3, 0xD2,
    0xCD, 0x0C, 0x13, 0xEC, 0x5F, 0x97, 0x44, 0x17,
    0xC4, 0xA7, 0x7E, 0x3D, 0x64, 0x5D, 0x19, 0x73,
    0x60, 0x81, 0x4F, 0xDC, 0x22, 0x2A, 0x90, 0x88,
    0x46, 0xEE, 0xB8, 0x14, 0xDE, 0x5E, 0x0B, 0xDB,
    0xE0, 0x32, 0x3A, 0x0A, 0x49, 0x06, 0x24, 0x5C,
    0xC2, 0xD3, 0xAC, 0x62, 0x91, 0x95, 0xE4, 0x79,
    0xE7, 0xC8, 0x37, 0x6D, 0x8D, 0xD5, 0x4E, 0xA9,
    0x6C, 0x56, 0xF4, 0xEA, 0x65, 0x7A, 0xAE, 0x08,
    0xBA, 0x78, 0x25, 0x2E, 0x1C, 0xA6, 0xB4, 0xC6,
    0xE8, 0xDD, 0x74, 0x1F, 0x4B, 0xBD, 0x8B, 0x8A,
    0x70, 0x3E, 0xB5, 0x66, 0x48, 0x03, 0xF6, 0x0E,
    0x61, 0x35, 0x57, 0xB9, 0x86, 0xC1, 0x1D, 0x9E,
    0xE1, 0xF8, 0x98, 0x11, 0x69, 0xD9, 0x8E, 0x94,
    0x9B, 0x1E, 0x87, 0xE9, 0xCE, 0x55, 0x28, 0xDF,
    0x8C, 0xA1, 0x89, 0x0D, 0xBF, 0xE6, 0x42, 0x68,
    0x41, 0x99, 0x2D, 0x0F, 0xB0, 0x54, 0xBB, 0x16,
];

/// Rcon（轮常数）。
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1B, 0x36];

// ==================== AES-128 ====================

/// AES-128 密钥调度 + 块加密（只加密）。
pub struct Aes128 {
    /// 轮密钥：44 个 u32（11×4 列主序，f[round][col]）。
    f: [u32; 44],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Aes128 {
        let mut w = [0u32; 44];
        for i in 0..4 {
            w[i] = u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
        }
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                // RotWord + SubWord + Rcon
                t = t.rotate_left(8);
                t = (u32::from(SBOX[(t >> 24) as usize]) << 24)
                    | (u32::from(SBOX[((t >> 16) & 0xFF) as usize]) << 16)
                    | (u32::from(SBOX[((t >> 8) & 0xFF) as usize]) << 8)
                    | u32::from(SBOX[(t & 0xFF) as usize]);
                t ^= u32::from(RCON[i / 4 - 1]) << 24;
            }
            w[i] = w[i - 4] ^ t;
        }
        Aes128 { f: w }
    }

    /// 加密单个 16 字节块。
    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        let mut s = [0u32; 4];
        for c in 0..4 {
            s[c] = u32::from_be_bytes([
                block[4 * c],
                block[4 * c + 1],
                block[4 * c + 2],
                block[4 * c + 3],
            ]) ^ self.f[c];
        }
        for round in 1..=10 {
            // SubBytes + ShiftRows
            let mut t = [0u32; 4];
            for c in 0..4 {
                let src = |row: usize| (c + row) & 3;
                let mut v = 0u32;
                for row in 0..4 {
                    let byte = (s[src(row)] >> (24 - 8 * row)) & 0xFF;
                    v |= u32::from(SBOX[byte as usize]) << (24 - 8 * row);
                }
                t[c] = v;
            }
            // MixColumns（末轮跳过）
            if round < 10 {
                for c in 0..4 {
                    let b = t[c].to_be_bytes();
                    let x = |i: usize| b[i] as u32;
                    let xtime = |v: u32| ((v << 1) ^ (((v >> 7) & 1) * 0x1B)) & 0xFF;
                    t[c] = (xtime(x(0)) ^ (xtime(x(1)) ^ x(1)) ^ x(2) ^ x(3)) << 24
                        | (x(0) ^ xtime(x(1)) ^ (xtime(x(2)) ^ x(2)) ^ x(3)) << 16
                        | (x(0) ^ x(1) ^ xtime(x(2)) ^ (xtime(x(3)) ^ x(3))) << 8
                        | ((xtime(x(0)) ^ x(0)) ^ x(1) ^ x(2) ^ xtime(x(3)));
                }
            }
            // AddRoundKey
            for c in 0..4 {
                s[c] = t[c] ^ self.f[round * 4 + c];
            }
        }
        for c in 0..4 {
            block[4 * c..4 * c + 4].copy_from_slice(&s[c].to_be_bytes());
        }
    }
}

// ==================== GHASH / GCM ====================

/// 128 位块按字节大端转 u128（GCM 位序：b0 = 最高位）。
fn be_u128(b: &[u8; 16]) -> u128 {
    let mut v = 0u128;
    for &x in b {
        v = (v << 8) | x as u128;
    }
    v
}

fn u128_be(v: u128) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = (v >> (8 * (15 - i))) as u8;
    }
    out
}

/// 块乘多项式 x（GCM 位序：u128 最高位 = x^0 系数）。
/// 乘 x：整体右移 1 位（系数 b_i → b_{i+1}），移出的 b_127（u128 最低位）
/// 即 x^128 项 ≡ R = 0xE1<<120（x^7+x^2+x+1 在 b_0..b_7，即最高字节 0xE1）。
fn gf_mulx(v: u128) -> u128 {
    (v >> 1) ^ if v & 1 != 0 { 0xE1u128 << 120 } else { 0 }
}

/// GCM 实例（同一 TLS 连接两方向共用一张 H 表）。
pub struct Gcm {
    aes: Aes128,
    /// h_tab[k][n] = H·n·x^{4k}，n ∈ [0,16)。
    h_tab: Box<[[u128; 16]; 32]>,
}

impl Gcm {
    pub fn new(key: &[u8; 16]) -> Gcm {
        let aes = Aes128::new(key);
        let mut h = [0u8; 16];
        aes.encrypt_block(&mut h);
        let h = be_u128(&h);
        let mut h_tab = Box::new([[0u128; 16]; 32]);
        // h_tab[0][n] = H·n（4 位窗口，移位-加）
        for n in 1..16usize {
            let mut acc = 0u128;
            for b in (0..4).rev() {
                acc = gf_mulx(acc);
                if (n >> b) & 1 == 1 {
                    acc ^= h;
                }
            }
            h_tab[0][n] = acc;
        }
        // h_tab[k][n] = h_tab[k-1][n]·x^4
        for k in 1..32usize {
            for n in 1..16usize {
                let mut v = h_tab[k - 1][n];
                for _ in 0..4 {
                    v = gf_mulx(v);
                }
                h_tab[k][n] = v;
            }
        }
        Gcm { aes, h_tab }
    }

    /// GHASH 吸收一块（与累加器异或后乘 H）。
    fn ghash_block(&self, acc: &mut u128, blk: &[u8; 16]) {
        let z = *acc ^ be_u128(blk);
        let bytes = z.to_be_bytes();
        let mut r = 0u128;
        for k in 0..32 {
            let nib = (if k % 2 == 0 { bytes[k / 2] >> 4 } else { bytes[k / 2] & 0xF }) as usize;
            // nibble k 覆盖系数 x^{4k}..x^{4k+3}，但 nibble 的整数值里
            // bit3 是最低系数（x^{4k}），与 h_tab[0][n] 的 n（bit_j ↔ x^j）
            // 位序相反，需先做 4 位反转。
            let n = ((nib & 8) >> 3) | ((nib & 4) >> 1) | ((nib & 2) << 1) | ((nib & 1) << 3);
            r ^= self.h_tab[k][n];
        }
        *acc = r;
    }

    /// GHASH 流式吸收（不足块零填充；跨调用状态在 `acc`）。
    pub fn ghash_update(&self, acc: &mut u128, data: &[u8]) {
        let mut chunks = data.chunks_exact(16);
        for c in &mut chunks {
            let mut blk = [0u8; 16];
            blk.copy_from_slice(c);
            self.ghash_block(acc, &blk);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut blk = [0u8; 16];
            blk[..rem.len()].copy_from_slice(rem);
            self.ghash_block(acc, &blk);
        }
    }

    /// 认证加密（TLS 记录）：`iv` 12B、`aad`、明文就地变密文，写 16B tag。
    pub fn seal(&self, iv: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; 16] {
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        let mut tag_base = j0;
        self.aes.encrypt_block(&mut tag_base);
        // CTR 加密（从 J0+1 起）
        let mut ctr = u128::from_be_bytes(j0) + 1;
        for chunk in buf.chunks_mut(16) {
            let mut k = u128_be(ctr);
            self.aes.encrypt_block(&mut k);
            for (i, b) in chunk.iter_mut().enumerate() {
                *b ^= k[i];
            }
            ctr += 1;
        }
        // GHASH(A || C || len)
        let mut acc = 0u128;
        self.ghash_update(&mut acc, aad);
        self.ghash_update(&mut acc, buf);
        let mut lenblk = [0u8; 16];
        lenblk[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        lenblk[8..].copy_from_slice(&((buf.len() as u64) * 8).to_be_bytes());
        self.ghash_block(&mut acc, &lenblk);
        let tag_bytes = u128_be(acc);
        let mut tag = [0u8; 16];
        for i in 0..16 {
            tag[i] = tag_bytes[i] ^ tag_base[i];
        }
        tag
    }

    /// 认证解密（就地）；tag 不符返回 false（缓冲内容不可信）。
    pub fn open(&self, iv: &[u8; 12], aad: &[u8], buf: &mut [u8], tag: &[u8; 16]) -> bool {
        // 先算期望 tag（GHASH 用密文），再解密
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        let mut tag_base = j0;
        self.aes.encrypt_block(&mut tag_base);
        let mut acc = 0u128;
        self.ghash_update(&mut acc, aad);
        self.ghash_update(&mut acc, buf);
        let mut lenblk = [0u8; 16];
        lenblk[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        lenblk[8..].copy_from_slice(&((buf.len() as u64) * 8).to_be_bytes());
        self.ghash_block(&mut acc, &lenblk);
        let tag_bytes = u128_be(acc);
        let mut diff = 0u8;
        for i in 0..16 {
            diff |= tag_bytes[i] ^ tag_base[i] ^ tag[i];
        }
        if diff != 0 {
            return false;
        }
        let mut ctr = u128::from_be_bytes(j0) + 1;
        for chunk in buf.chunks_mut(16) {
            let mut k = u128_be(ctr);
            self.aes.encrypt_block(&mut k);
            for (i, b) in chunk.iter_mut().enumerate() {
                *b ^= k[i];
            }
            ctr += 1;
        }
        true
    }
}
