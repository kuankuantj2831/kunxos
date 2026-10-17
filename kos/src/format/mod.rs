//! 磁盘格式定义：纯数据结构 + 定长序列化。
//!
//! 所有结构：
//! - `#[repr(C)]`、固定大小（`SIZE` 常量），显式补齐字段，无隐式填充；
//! - 多字节标量按小端序读写；
//! - `to_bytes` 总是重算 CRC（`crc32` 字段为权威值仅在 `from_bytes`
//!   之后成立），`from_bytes` 校验长度、magic、CRC 与枚举合法性；
//! - 不使用 unsafe：序列化按字段逐段 `copy_from_slice`，不 transmute，
//!   因此 `repr(C)` 只用于把内存布局声明为文档，字节布局完全由本
//!   模块的偏移常量决定。

pub mod extent;
pub mod manifest;
pub mod merkle;
pub mod namespace;
pub mod object;
pub mod superblock;

/// 全局块大小（字节）。本版本固定 4096，超级块校验该值。
pub const KOS_BLOCK_SIZE: usize = 4096;

/// 在偏移处写入小端 u16。
pub(crate) fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

/// 在偏移处写入小端 u32。
pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// 在偏移处写入小端 u64。
pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// 在偏移处写入定长字节数组。
pub(crate) fn put_array<const N: usize>(buf: &mut [u8], off: usize, v: &[u8; N]) {
    buf[off..off + N].copy_from_slice(v);
}

/// 从偏移处读小端 u16。调用方保证 `off + 2 <= buf.len()`。
pub(crate) fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

/// 从偏移处读小端 u32。调用方保证 `off + 4 <= buf.len()`。
pub(crate) fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// 从偏移处读小端 u64。调用方保证 `off + 8 <= buf.len()`。
pub(crate) fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

/// 从偏移处读定长字节数组。
pub(crate) fn get_array<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[off..off + N]);
    out
}

/// 计算整幅映像的 CRC32：`[field_off, field_off+field_len)` 区间
/// （即 CRC 字段自身）**不参与字节流**。
///
/// 生成与校验用同一函数：新建映像时缓冲区零初始化、CRC 字段位置
/// 最后才写入；校验时直接跳过磁盘上存储的 CRC 字节。
pub(crate) fn crc32_skip_field(image: &[u8], field_off: usize, field_len: usize) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&image[..field_off]);
    h.update(&image[field_off + field_len..]);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let mut buf = [0u8; 32];
        put_u16(&mut buf, 0, 0x1234);
        put_u32(&mut buf, 2, 0x5678_9ABC);
        put_u64(&mut buf, 6, 0xDEAD_BEEF_CAFE_F00D);
        put_array(&mut buf, 14, b"hello kos!!");
        assert_eq!(get_u16(&buf, 0), 0x1234);
        assert_eq!(get_u32(&buf, 2), 0x5678_9ABC);
        assert_eq!(get_u64(&buf, 6), 0xDEAD_BEEF_CAFE_F00D);
        assert_eq!(get_array::<10>(&buf, 14), *b"hello kos!");
    }

    #[test]
    fn little_endian_byte_order() {
        let mut buf = [0u8; 4];
        put_u32(&mut buf, 0, 0x0403_0201);
        assert_eq!(&buf, &[1, 2, 3, 4]);
    }

    #[test]
    fn crc_skip_field_equals_crc_of_stream_without_region() {
        let image = [1u8; 64];
        let mut without = [0u8; 60];
        without[..16].copy_from_slice(&image[..16]);
        without[16..].copy_from_slice(&image[20..]);
        let skipped = crc32_skip_field(&image, 16, 4);
        let plain = {
            let mut h = crc32fast::Hasher::new();
            h.update(&without);
            h.finalize()
        };
        assert_eq!(skipped, plain);
    }
}
