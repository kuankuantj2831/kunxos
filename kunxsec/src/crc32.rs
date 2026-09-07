//! CRC-32（IEEE 802.3，多项式 0xEDB88320），用于 BCB / 更新包的冗余校验。
//! 注意：CRC 只用于检测传输/写入损坏，不做认证（认证由 Ed25519 完成）。

/// 计算 `data` 的 CRC-32（标准反射算法，初始值 0xFFFFFFFF，异或输出 0xFFFFFFFF）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"abc"), 0x3524_41C2);
    }
}
