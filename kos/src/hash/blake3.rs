//! BLAKE3-256 自研标量实现，按 BLAKE3 规范实现压缩、chunk 与二叉树。
//!
//! 仅提供 KOS 所需的无密钥 32 字节摘要，不提供 keyed hash / derive key / XOF。
//! 无堆分配、无 unsafe；递归深度为 chunk 数的对数。

use super::Hasher;

/// BLAKE3，默认 256 位输出。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Blake3Hasher;

impl Hasher for Blake3Hasher {
    fn hash32(&self, data: &[u8]) -> [u8; 32] {
        let output = subtree(data, 0);
        // 根输出使用输出块计数器 0，而不是叶子的 chunk 计数器。
        let words = compress(output.cv, output.block, 0, output.len, output.flags | ROOT);
        let mut hash = [0u8; 32];
        for (bytes, word) in hash.chunks_exact_mut(4).zip(words) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        hash
    }

    fn hash128(&self, data: &[u8]) -> [u8; 16] {
        let h = self.hash32(data);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h[..16]);
        out
    }

    fn name(&self) -> &'static str {
        "blake3"
    }
}

const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
const PERMUTE: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];
const CHUNK_START: u32 = 1;
const CHUNK_END: u32 = 2;
const PARENT: u32 = 4;
const ROOT: u32 = 8;

struct Output {
    cv: [u32; 8],
    block: [u32; 16],
    counter: u64,
    len: u32,
    flags: u32,
}

impl Output {
    fn chaining_value(&self) -> [u32; 8] {
        let words = compress(self.cv, self.block, self.counter, self.len, self.flags);
        let mut cv = [0; 8];
        cv.copy_from_slice(&words[..8]);
        cv
    }
}

fn subtree(data: &[u8], chunk_counter: u64) -> Output {
    if data.len() <= 1024 {
        return chunk(data, chunk_counter);
    }
    // Largest power of two strictly below the rounded-up chunk count.
    // Written without len + 1023 to avoid overflow on very large slices.
    let chunks_minus_one = (data.len() - 1) / 1024;
    let left_chunks = 1usize << (usize::BITS - 1 - chunks_minus_one.leading_zeros());
    let (left, right) = data.split_at(left_chunks * 1024);
    let left_cv = subtree(left, chunk_counter).chaining_value();
    let right_cv = subtree(right, chunk_counter + left_chunks as u64).chaining_value();
    let mut block = [0; 16];
    block[..8].copy_from_slice(&left_cv);
    block[8..].copy_from_slice(&right_cv);
    Output {
        cv: IV,
        block,
        counter: 0,
        len: 64,
        flags: PARENT,
    }
}

fn chunk(mut data: &[u8], counter: u64) -> Output {
    let mut cv = IV;
    let mut first = true;
    loop {
        let len = data.len().min(64);
        let last = data.len() <= 64;
        let mut block = [0u32; 16];
        for (i, &byte) in data[..len].iter().enumerate() {
            block[i / 4] |= (byte as u32) << (8 * (i % 4));
        }
        let flags = (if first { CHUNK_START } else { 0 })
            | (if last { CHUNK_END } else { 0 });
        let output = Output {
            cv,
            block,
            counter,
            len: len as u32,
            flags,
        };
        if last {
            return output;
        }
        cv = output.chaining_value();
        data = &data[len..];
        first = false;
    }
}

#[inline]
fn mix(v: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize, x: u32, y: u32) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(12);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(8);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(7);
}

fn compress(cv: [u32; 8], mut m: [u32; 16], counter: u64, len: u32, flags: u32) -> [u32; 16] {
    let mut v = [0; 16];
    v[..8].copy_from_slice(&cv);
    v[8..12].copy_from_slice(&IV[..4]);
    v[12] = counter as u32;
    v[13] = (counter >> 32) as u32;
    v[14] = len;
    v[15] = flags;
    for _ in 0..7 {
        mix(&mut v, 0, 4, 8, 12, m[0], m[1]);
        mix(&mut v, 1, 5, 9, 13, m[2], m[3]);
        mix(&mut v, 2, 6, 10, 14, m[4], m[5]);
        mix(&mut v, 3, 7, 11, 15, m[6], m[7]);
        mix(&mut v, 0, 5, 10, 15, m[8], m[9]);
        mix(&mut v, 1, 6, 11, 12, m[10], m[11]);
        mix(&mut v, 2, 7, 8, 13, m[12], m[13]);
        mix(&mut v, 3, 4, 9, 14, m[14], m[15]);
        let previous = m;
        for i in 0..16 {
            m[i] = previous[PERMUTE[i]];
        }
    }
    for i in 0..8 {
        v[i] ^= v[i + 8];
        v[i + 8] ^= cv[i];
    }
    v
}
