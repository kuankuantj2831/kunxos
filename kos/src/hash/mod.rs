//! 哈希封装：统一接口屏蔽算法选择（BLAKE3 vs SHA-256 选型是设计
//! 文档未决问题 №1，由基准数据说话）。
//!
//! 两个实现（`blake3`、`sha2` crate）均以 `default-features = false`
//! 引入，`no_std` 下可用。

pub mod blake3;
pub mod sha256;

pub use blake3::Blake3Hasher;
pub use sha256::Sha256Hasher;

/// 哈希算法统一接口（对象安全，可 `&dyn Hasher` 传递）。
pub trait Hasher {
    /// 32 字节摘要（BLAKE3-256 / SHA-256）。
    fn hash32(&self, data: &[u8]) -> [u8; 32];
    /// 16 字节截断摘要：取 `hash32` 结果前 16 字节。
    ///
    /// BLAKE3 的可扩展输出（XOF）性质保证「截断前缀」与「请求 16
    /// 字节输出」一致；SHA-256 截断同样取前 16 字节。命名空间
    /// entry 的 `name_hash` 字段即此格式。
    fn hash128(&self, data: &[u8]) -> [u8; 16];
    /// 算法名（`"blake3"` / `"sha256"`），用于诊断与基准标注。
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    /// 官方已知向量，防止实现接线错误。
    #[test]
    fn known_vectors() {
        let cases: &[(&dyn Hasher, &str, &str)] = &[
            (&Blake3Hasher, "", "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"),
            (&Blake3Hasher, "abc", "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"),
            (&Sha256Hasher, "", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (&Sha256Hasher, "abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ];
        for (h, input, want) in cases {
            assert_eq!(
                h.hash32(input.as_bytes()).to_vec(),
                hex(want),
                "{}({:?})",
                h.name(),
                input
            );
        }
    }

    /// hash128 必须是 hash32 的前 16 字节。
    #[test]
    fn hash128_is_prefix_of_hash32() {
        for h in [&Blake3Hasher as &dyn Hasher, &Sha256Hasher] {
            let full = h.hash32(b"prefix property");
            let short = h.hash128(b"prefix property");
            assert_eq!(&full[..16], &short[..]);
        }
    }

    /// 通过 trait 对象使用（Merkle 构建即如此调用）。
    #[test]
    fn object_safe() {
        fn dispatch(h: &dyn Hasher) -> usize {
            h.hash32(b"dyn").len()
        }
        assert_eq!(dispatch(&Blake3Hasher), 32);
        assert_eq!(dispatch(&Sha256Hasher), 32);
        assert_eq!(Blake3Hasher.name(), "blake3");
        assert_eq!(Sha256Hasher.name(), "sha256");
    }
}
