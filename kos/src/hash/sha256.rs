//! SHA-256 实现（no_std：`sha2` crate 关闭默认 feature）。

use super::Hasher;
use sha2::{Digest, Sha256};

/// SHA-256（FIPS 180-4）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sha256Hasher;

impl Hasher for Sha256Hasher {
    fn hash32(&self, data: &[u8]) -> [u8; 32] {
        let d = Sha256::digest(data);
        let mut out = [0u8; 32];
        out.copy_from_slice(&d);
        out
    }

    fn hash128(&self, data: &[u8]) -> [u8; 16] {
        let h = self.hash32(data);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h[..16]);
        out
    }

    fn name(&self) -> &'static str {
        "sha256"
    }
}
