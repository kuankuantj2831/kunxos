//! 十六进制编解码小工具（no_std）。

/// 把字节编码为小写十六进制，写入 `out`；返回写入长度（= 2×len）。
/// `out` 不足时截断（绝不越界写）。
pub fn encode(data: &[u8], out: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut n = 0usize;
    for &b in data {
        if n + 2 > out.len() {
            break;
        }
        out[n] = HEX[(b >> 4) as usize];
        out[n + 1] = HEX[(b & 0xf) as usize];
        n += 2;
    }
    n
}

/// 解码十六进制字符串；输入长度须为偶数且全部为合法字符，否则返回 None。
pub fn decode(s: &[u8], out: &mut [u8]) -> Option<usize> {
    if s.len() % 2 != 0 || s.len() / 2 > out.len() {
        return None;
    }
    fn nib(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    for i in 0..s.len() / 2 {
        let hi = nib(s[i * 2])?;
        let lo = nib(s[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(s.len() / 2)
}
