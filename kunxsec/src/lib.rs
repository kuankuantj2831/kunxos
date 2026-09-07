//! KunxOS 安全启动链共享库（`no_std`，零依赖）。
//!
//! 供 BootROM（kunxrom）、Bootloader（kunxboot）、内核（kunxcore）与
//! 用户态 OTA 客户端（otad）共用的密码学原语与二进制格式：
//!
//! - [`sha256`] / [`sha512`]：哈希（FIPS 180-4），支持流式增量。
//! - [`ed25519`]：Ed25519 验签（RFC 8032），纯 Rust 自研实现。
//! - [`kxsi`]：签名镜像格式（KXSI 头 + 载荷 + 签名）。
//! - [`bcb`]：A/B 启动控制块（BCB，misc 分区）。
//! - [`kxup`]：OTA 更新包格式（KXUP 清单 + 全量/增量组件）。
//! - [`bspatch`]：BSDIFF40 补丁应用（设备侧增量更新）。
//! - [`crc32`]：CRC-32（BCB / 包完整性冗余校验）。

#![no_std]
#![allow(dead_code)]

#[cfg(test)]
extern crate std;

pub mod bcb;
pub mod bspatch;
pub mod crc32;
pub mod ed25519;
pub mod keys;
pub mod kxsi;
pub mod kxup;
pub mod otp;
pub mod sha256;
pub mod sha512;

/// 把 4 字节小端切片读成 `u32`。
#[inline]
pub fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// 把 8 字节小端切片读成 `u64`。
#[inline]
pub fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// 把 `u32` 写为小端 4 字节。
#[inline]
pub fn put_le_u32(v: u32, b: &mut [u8]) {
    let x = v.to_le_bytes();
    b[0] = x[0];
    b[1] = x[1];
    b[2] = x[2];
    b[3] = x[3];
}

/// 把 `u64` 写为小端 8 字节。
#[inline]
pub fn put_le_u64(v: u64, b: &mut [u8]) {
    let x = v.to_le_bytes();
    for (i, c) in x.iter().enumerate() {
        b[i] = *c;
    }
}

/// 常量时间字节比较（防止侧信道时序差异；返回 `true` 表示相等）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d: u8 = 0;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}
