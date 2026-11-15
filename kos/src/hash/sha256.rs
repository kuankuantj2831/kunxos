//! SHA-256：复用 kunxsec 的自研实现（no_std）。

use super::Hasher;

/// SHA-256（FIPS 180-4）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sha256Hasher;

impl Hasher for Sha256Hasher {
    fn hash32(&self, data: &[u8]) -> [u8; 32] {
        kunxsec::sha256::sha256(data)
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
