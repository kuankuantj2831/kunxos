//! KunxOS 同步栈通用工具：无堆、core-only 字节处理。
//!
//! 该模块被两类 crate 经 `#[path]` 引入：
//!   - 应用（no_std / aarch64-unknown-none）：日历、通讯录
//!   - 宿主测试 crate（std）：单元 / 集成测试
//! 因此本文件只用 `core`，不触碰任何平台 API；文件系统与网络传输
//! 均由调用方注入（见 fs.rs / http.rs 的 trait）。

// ==================== 字节查找 / 裁剪 ====================

/// 在 hay 中查找 needle 首次出现的位置。
pub fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    'outer: for i in 0..=last {
        for j in 0..needle.len() {
            if hay[i + j] != needle[j] {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// 在 hay 中查找字节 b 首次出现的位置。
pub fn find_byte(hay: &[u8], b: u8) -> Option<usize> {
    hay.iter().position(|&c| c == b)
}

/// 在 hay 中从 start 起查找字节 b。
pub fn find_byte_from(hay: &[u8], b: u8, start: usize) -> Option<usize> {
    let mut i = start;
    while i < hay.len() {
        if hay[i] == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 去除首尾空白（空格 / 制表 / CR / LF）。
pub fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && matches!(b[s], b' ' | b'\t' | b'\r' | b'\n') {
        s += 1;
    }
    while e > s && matches!(b[e - 1], b' ' | b'\t' | b'\r' | b'\n') {
        e -= 1;
    }
    &b[s..e]
}

/// 在 b 的 sep 首次出现处切成两段（不含 sep）。找不到时返回 (b, 空)。
pub fn split2(b: &[u8], sep: u8) -> (&[u8], &[u8]) {
    match find_byte(b, sep) {
        Some(i) => (&b[..i], &b[i + 1..]),
        None => (b, &[]),
    }
}

/// 在 b 的 sep 首次出现处切成两段，**包含** sep 之后的一个字节
/// （用于解析 `KEY=value;PARAM=x` 时保留参数串）。
pub fn split2_keep(b: &[u8], sep: u8) -> (&[u8], &[u8]) {
    match find_byte(b, sep) {
        Some(i) => (&b[..i], &b[i + 1..]),
        None => (b, &[]),
    }
}

/// 大小写不敏感比较。
pub fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for i in 0..a.len() {
        if a[i].to_ascii_lowercase() != b[i].to_ascii_lowercase() {
            return false;
        }
    }
    true
}

/// 判断 b 是否以 prefix 开头（大小写不敏感）。
pub fn starts_with_ci(b: &[u8], prefix: &[u8]) -> bool {
    b.len() >= prefix.len() && eq_ignore_case(&b[..prefix.len()], prefix)
}

/// 复制切片到缓冲，返回写入长度（截断到 dst 容量）。
pub fn copy_into(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// 把字符串追加到 buf[at..]，返回新的 at（越界即截断）。
pub fn push_str(buf: &mut [u8], at: usize, s: &[u8]) -> usize {
    let start = at.min(buf.len());
    at + copy_into(&mut buf[start..], s)
}

/// 把无符号数十进制追加到 buf[at..]，返回新的 at。
pub fn push_dec(buf: &mut [u8], at: usize, mut v: u64) -> usize {
    if v == 0 {
        return push_str(buf, at, b"0");
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    push_str(buf, at, &tmp[i..])
}

/// 把无符号数十进制追加到 buf[at..]，不足 width 位左补零，返回新的 at。
pub fn push_dec_pad(buf: &mut [u8], at: usize, mut v: u64, width: usize) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if v == 0 {
        i -= 1;
        tmp[i] = b'0';
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    let digits = tmp.len() - i;
    let mut a = at;
    let mut pad = width.saturating_sub(digits);
    while pad > 0 {
        a = push_str(buf, a, b"0");
        pad -= 1;
    }
    a = push_str(buf, a, &tmp[i..]);
    a
}

/// 把有符号数十进制追加到 buf[at..]，返回新的 at。
pub fn push_dec_i(buf: &mut [u8], at: usize, v: i64) -> usize {
    if v < 0 {
        let mut a = push_str(buf, at, b"-");
        a = push_dec(buf, a, v.unsigned_abs());
        a
    } else {
        push_dec(buf, at, v as u64)
    }
}

// ==================== 数值解析 ====================

/// 解析十进制无符号整数（允许前导空白 / 尾随空白）。失败返回 None。
pub fn parse_u64(b: &[u8]) -> Option<u64> {
    let t = trim(b);
    if t.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in t {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

/// 解析十进制无符号 32 位整数。
pub fn parse_u32(b: &[u8]) -> Option<u32> {
    parse_u64(b).and_then(|v| u32::try_from(v).ok())
}

/// 解析十进制有符号 64 位整数。
pub fn parse_i64(b: &[u8]) -> Option<i64> {
    let t = trim(b);
    if t.is_empty() {
        return None;
    }
    let (neg, digits) = if t[0] == b'-' {
        (true, &t[1..])
    } else if t[0] == b'+' {
        (false, &t[1..])
    } else {
        (false, t)
    };
    if digits.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
    }
    Some(if neg { -v } else { v })
}

// ==================== 哈希 ====================

/// FNV-1a 32 位哈希（与内核 / 云服务的既有实现一致，用于
/// 稳定 ID 派生、密钥混淆与轻量指纹，**非密码学安全**）。
pub fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ==================== Base64（RFC 4648） ====================

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 编码（含填充 `=`），返回写入 dst 的字节数。
pub fn base64_encode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut o = 0usize;
    let mut i = 0usize;
    while i + 3 <= src.len() && o + 4 <= dst.len() {
        let n = ((src[i] as u32) << 16) | ((src[i + 1] as u32) << 8) | (src[i + 2] as u32);
        dst[o] = B64[((n >> 18) & 63) as usize];
        dst[o + 1] = B64[((n >> 12) & 63) as usize];
        dst[o + 2] = B64[((n >> 6) & 63) as usize];
        dst[o + 3] = B64[(n & 63) as usize];
        o += 4;
        i += 3;
    }
    let rem = src.len() - i;
    if rem > 0 {
        let mut n = (src[i] as u32) << 16;
        if rem > 1 {
            n |= (src[i + 1] as u32) << 8;
        }
        let mut need = 4;
        if o + 4 > dst.len() {
            need = dst.len() - o;
        }
        let pairs = match need {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if pairs >= 1 && o < dst.len() {
            dst[o] = B64[((n >> 18) & 63) as usize];
        }
        if pairs >= 2 && o + 1 < dst.len() {
            dst[o + 1] = B64[((n >> 12) & 63) as usize];
        }
        if pairs >= 3 && o + 2 < dst.len() {
            dst[o + 2] = if rem > 1 { B64[((n >> 6) & 63) as usize] } else { b'=' };
        }
        if pairs >= 4 && o + 3 < dst.len() {
            dst[o + 3] = b'=';
        }
        o += need;
    }
    o
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Base64 解码，返回写入 dst 的字节数；非法输入返回 None。
pub fn base64_decode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    let mut o = 0usize;
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &c in src {
        if c == b'\r' || c == b'\n' || c == b' ' || c == b'\t' {
            continue;
        }
        if c == b'=' {
            break;
        }
        let v = b64_val(c)?;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            let byte = ((acc >> nbits) & 0xFF) as u8;
            if o >= dst.len() {
                return None;
            }
            dst[o] = byte;
            o += 1;
        }
    }
    Some(o)
}

// ==================== Hex ====================

/// 十六进制编码（小写）。
pub fn hex_encode(src: &[u8], dst: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let n = src.len().min(dst.len() / 2);
    for i in 0..n {
        dst[i * 2] = HEX[(src[i] >> 4) as usize];
        dst[i * 2 + 1] = HEX[(src[i] & 0xF) as usize];
    }
    n * 2
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 十六进制解码，返回写入 dst 的字节数；非法输入返回 None。
pub fn hex_decode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    if src.len() % 2 != 0 {
        return None;
    }
    let n = src.len() / 2;
    if n > dst.len() {
        return None;
    }
    for i in 0..n {
        let hi = hex_val(src[i * 2])?;
        let lo = hex_val(src[i * 2 + 1])?;
        dst[i] = (hi << 4) | lo;
    }
    Some(n)
}

// ==================== URL 编解码 ====================

/// URL 编码（保留 RFC 3986 unreserved + 少量安全字符）。
pub fn url_encode(src: &[u8], dst: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut o = 0usize;
    for &c in src {
        let safe = c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'/' | b'@');
        if safe {
            if o < dst.len() {
                dst[o] = c;
            }
            o += 1;
        } else {
            if o + 2 < dst.len() {
                dst[o] = b'%';
                dst[o + 1] = HEX[(c >> 4) as usize];
                dst[o + 2] = HEX[(c & 0xF) as usize];
            }
            o += 3;
        }
    }
    o.min(dst.len())
}

/// URL 解码（`%XX` → 字节），返回写入 dst 的字节数；非法输入返回 None。
pub fn url_decode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    let mut o = 0usize;
    let mut i = 0usize;
    while i < src.len() {
        if src[i] == b'%' {
            if i + 2 >= src.len() {
                return None;
            }
            let hi = hex_val(src[i + 1])?;
            let lo = hex_val(src[i + 2])?;
            if o >= dst.len() {
                return None;
            }
            dst[o] = (hi << 4) | lo;
            o += 1;
            i += 3;
        } else {
            if o >= dst.len() {
                return None;
            }
            dst[o] = src[i];
            o += 1;
            i += 1;
        }
    }
    Some(o)
}

/// 简易生成式 UUID：基于时间 + 熵字节（内核熵不可用时退化为时间+计数器），
/// 输出 36 字节 "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"。
/// 熵来源：调用方提供 `entropy`（如 getrandom 或地址位）。
pub fn gen_uid(now_us: u64, counter: u32, entropy: u32, dst: &mut [u8]) -> usize {
    let mut raw = [0u8; 16];
    let t = now_us.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1234_5678);
    raw[0..8].copy_from_slice(&t.to_le_bytes());
    raw[8..12].copy_from_slice(&counter.to_le_bytes());
    raw[12..16].copy_from_slice(&entropy.to_le_bytes());
    // 置版本 4 / 变体 10 位（形式上遵循 UUIDv4 布局）
    raw[6] = (raw[6] & 0x0F) | 0x40;
    raw[8] = (raw[8] & 0x3F) | 0x80;
    let hex = [
        b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7',
        b'8', b'9', b'a', b'b', b'c', b'd', b'e', b'f',
    ];
    let mut o = 0usize;
    for i in 0..16 {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            if o < dst.len() {
                dst[o] = b'-';
            }
            o += 1;
        }
        if o + 1 < dst.len() {
            dst[o] = hex[(raw[i] >> 4) as usize];
            dst[o + 1] = hex[(raw[i] & 0xF) as usize];
        }
        o += 2;
    }
    o.min(dst.len())
}

// ==================== 时间（公历 ↔ epoch 天） ====================

/// epoch 天数 → 公历 (年, 月, 日)，月/日 1 基（Howard Hinnant 算法）。
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

/// 公历 (年, 月, 日) → epoch 天数；月/日 1 基。
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = ((153 * mp + 2) / 5) as i64 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// epoch 天数 → 星期（0=周日 … 6=周六）。
pub fn weekday_from_days(days: i64) -> u32 {
    let w = (days + 4) % 7;
    if w < 0 { (w + 7) as u32 } else { w as u32 }
}

/// 星期几 → 0=周日 … 6=周六 掩码位（BYDAY 用）。
pub fn weekday_bit(dow: u32) -> u32 {
    1 << dow
}

/// 每月天数（闰年修正）。
pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// epoch 秒 → 公历 (年,月,日,时,分,秒)（UTC）。
pub fn civil_from_epoch(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    (y, m, d, h, mi, s)
}

/// 公历（UTC）→ epoch 秒。
pub fn epoch_from_civil(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    days_from_civil(y, m, d) * 86400 + (h as i64) * 3600 + (mi as i64) * 60 + s as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_and_trim() {
        assert_eq!(find_sub(b"hello world", b"world"), Some(6));
        assert_eq!(find_sub(b"abc", b"zz"), None);
        assert_eq!(trim(b"  \tfoo\r\n "), b"foo");
        assert_eq!(split2(b"a=b;c=d", b'='), (b"a".as_slice(), b"b;c=d".as_slice()));
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_u32(b"123"), Some(123));
        assert_eq!(parse_u32(b" 42 "), Some(42));
        assert_eq!(parse_u32(b"12a"), None);
        assert_eq!(parse_i64(b"-17"), Some(-17));
        assert_eq!(parse_i64(b"+8"), Some(8));
        assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64(b"18446744073709551616"), None);
    }

    #[test]
    fn b64_roundtrip() {
        let cases: &[&[u8]] = &[b"", b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", b"\x00\x01\x02\xff"];
        for s in cases {
            let mut enc = [0u8; 64];
            let n = base64_encode(s, &mut enc);
            let mut dec = [0u8; 64];
            let m = base64_decode(&enc[..n], &mut dec).unwrap();
            assert_eq!(&dec[..m], *s, "roundtrip {:?}", s);
        }
        assert_eq!(base64_encode(b"foobar", &mut [0u8; 16]), 8);
        // 标准向量
        let mut e = [0u8; 8];
        let n = base64_encode(b"foobar", &mut e);
        assert_eq!(&e[..n], b"Zm9vYmFy");
        let mut e2 = [0u8; 4];
        let n2 = base64_encode(b"f", &mut e2);
        assert_eq!(&e2[..n2], b"Zg==");
    }

    #[test]
    fn hex_roundtrip() {
        let mut enc = [0u8; 32];
        let n = hex_encode(b"\xde\xad\xbe\xef", &mut enc);
        assert_eq!(&enc[..n], b"deadbeef");
        let mut dec = [0u8; 16];
        let m = hex_decode(b"DEadBEEF", &mut dec).unwrap();
        assert_eq!(&dec[..m], b"\xde\xad\xbe\xef");
    }

    #[test]
    fn fnv1a_known() {
        assert_eq!(fnv1a(b""), 0x811c9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c292c);
    }

    #[test]
    fn calendar_math() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2024, 2, 29), days_from_civil(2024, 3, 1) - 1);
        assert_eq!(weekday_from_days(0), 4); // 1970-01-01 是周四
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(epoch_from_civil(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(civil_from_epoch(86400), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn uid_gen_shape() {
        let mut u = [0u8; 64];
        let n = gen_uid(1_700_000_000_000_000, 7, 0xdeadbeef, &mut u);
        assert_eq!(n, 36);
        assert_eq!(u[8], b'-');
        assert_eq!(u[13], b'-');
        assert_eq!(u[14], b'4');
        assert!(matches!(u[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn url_codec() {
        let mut e = [0u8; 64];
        let n = url_encode(b"/dav/cal/a b&c", &mut e);
        assert_eq!(&e[..n], b"/dav/cal/a%20b%26c");
        let mut d = [0u8; 64];
        let m = url_decode(&e[..n], &mut d).unwrap();
        assert_eq!(&d[..m], b"/dav/cal/a b&c");
    }
}
