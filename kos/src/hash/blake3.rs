//! BLAKE3 实现（no_std：`blake3` crate 关闭默认 feature）。

use super::Hasher;

/// BLAKE3，默认 256 位输出。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Blake3Hasher;

impl Hasher for Blake3Hasher {
    fn hash32(&self, data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
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
