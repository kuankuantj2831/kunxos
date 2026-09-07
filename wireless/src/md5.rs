//! MD5（RFC 1321）。用于 802.1X EAP-MD5（遗留方法）与部分认证场景。

#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const K: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
    0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
    0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
    0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
    0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
    0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
    0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
    0xeb86_d391,
];

impl Md5 {
    pub const fn new() -> Self {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buf: [0u8; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut off = 0usize;
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            off += take;
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while off + 64 <= data.len() {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[off..off + 64]);
            self.compress(&block);
            off += 64;
        }
        if off < data.len() {
            self.buf[..data.len() - off].copy_from_slice(&data[off..]);
            self.buf_len = data.len() - off;
        }
    }

    pub fn finalize(&self) -> [u8; 16] {
        let mut ctx = self.clone();
        let bit_len = ctx.total.wrapping_mul(8);
        ctx.update(&[0x80]);
        while ctx.buf_len != 56 {
            ctx.update(&[0]);
        }
        ctx.buf[56..64].copy_from_slice(&bit_len.to_le_bytes());
        let block = ctx.buf;
        ctx.compress(&block);
        let mut out = [0u8; 16];
        for (i, w) in ctx.state.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a
                .wrapping_add(f)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut ctx = Md5::new();
    ctx.update(data);
    ctx.finalize()
}

/// EAP-MD5 响应：MD5(id || password || challenge)（RFC 3748 7.1）。
pub fn eap_md5_response(id: u8, password: &[u8], challenge: &[u8]) -> [u8; 16] {
    let mut buf = [0u8; 256];
    let mut n = 0usize;
    buf[n] = id;
    n += 1;
    let plen = password.len().min(100);
    buf[n..n + plen].copy_from_slice(&password[..plen]);
    n += plen;
    let clen = challenge.len().min(100);
    buf[n..n + clen].copy_from_slice(&challenge[..clen]);
    n += clen;
    md5(&buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_vectors() {
        // RFC 1321 测试向量。
        assert_eq!(
            md5(b""),
            [0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
             0x42, 0x7e]
        );
        assert_eq!(
            md5(b"abc"),
            [0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1,
             0x7f, 0x72]
        );
        assert_eq!(
            md5(b"abcdefghijklmnopqrstuvwxyz"),
            [0xc3, 0xfc, 0xd3, 0xd7, 0x61, 0x92, 0xe4, 0x00, 0x7d, 0xfb, 0x49, 0x6c, 0xca, 0x67,
             0xe1, 0x3b]
        );
    }

    #[test]
    fn streaming_matches() {
        let data = b"KunxOS md5 streaming 0123456789 abcdef";
        let mut ctx = Md5::new();
        for c in data.chunks(7) {
            ctx.update(c);
        }
        assert_eq!(ctx.finalize(), md5(data));
    }
}
