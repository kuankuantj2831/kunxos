//! CRC-32（IEEE 802.3，多项式 0xEDB88320），用于 BCB / 更新包的冗余校验。
//! 注意：CRC 只用于检测传输/写入损坏，不做认证（认证由 Ed25519 完成）。

/// 计算 `data` 的 CRC-32（标准反射算法，初始值 0xFFFFFFFF，异或输出 0xFFFFFFFF）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut h = Crc32::new();
    h.update(data);
    h.finalize()
}

const fn table() -> [u32; 256] {
    let mut entries = [0; 256];
    let mut i = 0;
    while i < entries.len() {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            bit += 1;
        }
        entries[i] = crc;
        i += 1;
    }
    entries
}

const TABLE: [u32; 256] = table();

/// CRC-32 增量上下文；分段输入与拼接后一次输入的结果相同。
#[derive(Clone)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// 创建 IEEE CRC-32 上下文。
    pub const fn new() -> Self {
        Self(0xffff_ffff)
    }

    /// 将下一段数据加入校验，不分配内存。
    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 = (self.0 >> 8) ^ TABLE[((self.0 ^ byte as u32) & 255) as usize];
        }
    }

    /// 完成校验（最终按位取反）。
    pub fn finalize(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
fn bitwise_reference(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
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

    #[test]
    fn table_and_streaming_match_bitwise() {
        let data: std::vec::Vec<u8> = (0..4097).map(|i| (i % 251) as u8).collect();
        assert_eq!(crc32(&data), bitwise_reference(&data));
        for split in 0..=data.len() {
            let mut h = Crc32::new();
            h.update(&data[..split]);
            h.update(&[]);
            h.update(&data[split..]);
            assert_eq!(h.finalize(), crc32(&data));
        }
    }
}
