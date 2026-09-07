//! SHA-512（FIPS 180-4）：纯 Rust、no_std、零依赖、增量式实现。
//!
//! 供 Ed25519 验签（RFC 8032）与索引哈希使用。常量（IV 与 80 轮 K）为标准值，
//! 已在主机侧与 Python hashlib 交叉验证（见 tools/kunxverify 自测）。

/// 摘要长度（字节）。
pub const DIGEST_LEN: usize = 64;
/// 块大小（字节）。
const BLOCK_LEN: usize = 128;

/// 初始哈希值 H0（前 8 个素数平方根小数部分前 64 位）。
const IV: [u64; 8] = [
    0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
    0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
];

/// 80 轮常数 K（前 80 个素数立方根小数部分前 64 位）。
const K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

/// 增量式 SHA-512 上下文（栈上分配，约 224 字节）。
#[derive(Clone, Copy)]
pub struct Sha512 {
    h: [u64; 8],
    buf: [u8; BLOCK_LEN],
    buf_len: usize,
    /// 已处理的总字节数（低 128 位足够表示任意实际输入长度）。
    total: u128,
}

fn rotr(x: u64, n: u32) -> u64 {
    (x >> n) | (x << (64 - n))
}

fn compress(h: &mut [u64; 8], block: &[u8; BLOCK_LEN]) {
    let mut w = [0u64; 80];
    for i in 0..16 {
        let off = i * 8;
        w[i] = u64::from_be_bytes([
            block[off], block[off + 1], block[off + 2], block[off + 3],
            block[off + 4], block[off + 5], block[off + 6], block[off + 7],
        ]);
    }
    for i in 16..80 {
        let s0 = rotr(w[i - 15], 1) ^ rotr(w[i - 15], 8) ^ (w[i - 15] >> 7);
        let s1 = rotr(w[i - 2], 19) ^ rotr(w[i - 2], 61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let mut a = h[0];
    let mut b = h[1];
    let mut c = h[2];
    let mut d = h[3];
    let mut e = h[4];
    let mut f = h[5];
    let mut g = h[6];
    let mut hh = h[7];

    for i in 0..80 {
        let s1 = rotr(e, 14) ^ rotr(e, 18) ^ rotr(e, 41);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = rotr(a, 28) ^ rotr(a, 34) ^ rotr(a, 39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

impl Sha512 {
    pub fn new() -> Sha512 {
        Sha512 { h: IV, buf: [0; BLOCK_LEN], buf_len: 0, total: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u128);
        let mut off = 0usize;
        // 先填满残余缓冲
        if self.buf_len > 0 {
            let need = BLOCK_LEN - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            off += take;
            if self.buf_len == BLOCK_LEN {
                compress(&mut self.h, &self.buf);
                self.buf_len = 0;
            }
        }
        // 整块处理
        while off + BLOCK_LEN <= data.len() {
            let mut blk = [0u8; BLOCK_LEN];
            blk.copy_from_slice(&data[off..off + BLOCK_LEN]);
            compress(&mut self.h, &blk);
            off += BLOCK_LEN;
        }
        // 剩余
        let rest = data.len() - off;
        if rest > 0 {
            self.buf[..rest].copy_from_slice(&data[off..]);
            self.buf_len = rest;
        }
    }

    /// 收尾并输出 64 字节摘要（不改变上下文，可重复调用）。
    pub fn finalize(&self) -> [u8; DIGEST_LEN] {
        let mut st = *self;
        // 填充：0x80 + 0x00... + 128 位大端长度
        let bit_len = st.total.wrapping_mul(8);
        st.buf[st.buf_len] = 0x80;
        let zero_from = st.buf_len + 1;
        if zero_from <= BLOCK_LEN {
            for i in zero_from..BLOCK_LEN {
                st.buf[i] = 0;
            }
        }
        if st.buf_len + 1 > BLOCK_LEN - 16 {
            // 当前块放不下长度：补满并压缩，另起一块
            compress(&mut st.h, &st.buf);
            st.buf = [0; BLOCK_LEN];
        }
        let len_bytes = bit_len.to_be_bytes(); // 16 字节
        st.buf[BLOCK_LEN - 16..].copy_from_slice(&len_bytes);
        compress(&mut st.h, &st.buf);

        let mut out = [0u8; DIGEST_LEN];
        for (i, w) in st.h.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_be_bytes());
        }
        out
    }
}

/// 一次性 SHA-512。
pub fn sha512(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut st = Sha512::new();
    st.update(data);
    st.finalize()
}
