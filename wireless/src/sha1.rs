//! SHA-1（RFC 3174）流式哈希。用于 WPA-PBKDF2、802.11 PRF、EAP-MD5 无关、HMAC-SHA1。

/// SHA-1 上下文：支持分块 update + 一次性 finalize。
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Sha1 {
    pub const fn new() -> Self {
        Sha1 {
            h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            buf: [0u8; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut off = 0usize;
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
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

    /// 一次性完成哈希；之后可继续 update（SHA-1 流式语义）。
    pub fn finalize(&self) -> [u8; 20] {
        let mut ctx = self.clone();
        let bit_len = ctx.total.wrapping_mul(8);
        // 填充：0x80 + 零 + 64 位大端长度。
        ctx.update(&[0x80]);
        while ctx.buf_len != 56 {
            ctx.update(&[0]);
        }
        ctx.buf[56..64].copy_from_slice(&bit_len.to_be_bytes());
        // 直接压缩最后一块（buf 恰好 64 字节）。
        let block = ctx.buf;
        ctx.compress(&block);
        let mut out = [0u8; 20];
        for (i, w) in ctx.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }
}

/// 一次性 SHA-1。
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut ctx = Sha1::new();
    ctx.update(data);
    ctx.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_vectors() {
        // RFC 3174 / FIPS 180-1 测试向量。
        assert_eq!(
            sha1(b""),
            [0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
             0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09]
        );
        assert_eq!(
            sha1(b"abc"),
            [0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
             0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d]
        );
        assert_eq!(
            sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            [0x84, 0x98, 0x3e, 0x44, 0x1c, 0x3b, 0xd2, 0x6e, 0xba, 0xae, 0x4a, 0xa1, 0xf9, 0x51,
             0x29, 0xe5, 0xe5, 0x46, 0x70, 0xf1]
        );
    }

    #[test]
    fn sha1_streaming_matches_once() {
        let data = b"KunxOS wireless protocol stack sha1 streaming test 0123456789";
        let mut ctx = Sha1::new();
        for chunk in data.chunks(7) {
            ctx.update(chunk);
        }
        assert_eq!(ctx.finalize(), sha1(data));
    }
}
