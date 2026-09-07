//! OTP / eFUSE 模拟（QEMU 无真实 eFUSE，用磁盘 misc 原始区 + 写一次语义模拟）。
//!
//! 语义与真实 eFUSE 一致：
//! - 每字节初值 0xFF（未熔断）；仅当目标字节仍为 0xFF 时允许写入（1→0 或覆写真实值后不可逆）；
//! - 熔丝计数器（防回滚楼层）只能增加：用 64 字节计数域中“熔断位数（1→0）”表示楼层，
//!   楼层只升不降，符合 eFUSE 计数器只能烧的能力；
//! - 设备唯一密钥在首启烧入，此后不可读回替换（丢失 = 永久丢失）。
//!
//! 布局（4KB / 8 扇区，@0 为 OTP 区起始）：
//! ```text
//! @0    magic[8]  = "KXOTPv10"
//! @8    version u32 = 1
//! @12   flags u32    （bit0 = secure_boot 使能熔丝，熔断后不可逆）
//! @16   boot_pk_sha256[32]  （授权启动公钥摘要；0xFF=未熔断）
//! @48   device_key[32]      （设备唯一密钥；0xFF=未生成）
//! @80   reserved[32]
//! @112  floor_fuses[64]     （防回滚熔丝计数域：bits 1→0）
//! @176..2048 保留
//! ```

use crate::{ct_eq, le_u32, put_le_u32};

/// OTP 区大小（8 扇区）。
pub const OTP_SIZE: usize = 2048;
pub const MAGIC: &[u8; 8] = b"KXOTPv10";
pub const VERSION: u32 = 1;

pub const FLAG_SECURE_BOOT: u32 = 1 << 0;

pub const OFF_FLAGS: usize = 12;
pub const OFF_BOOT_PK_SHA256: usize = 16;
pub const OFF_DEVICE_KEY: usize = 48;
pub const OFF_FLOOR_FUSES: usize = 112;
pub const FLOOR_FUSES_LEN: usize = 64;
/// 楼层可用最大位（64×8）。楼层 0 = 无位熔断。
pub const FLOOR_MAX: u64 = (FLOOR_FUSES_LEN * 8) as u64;

/// 解析 OTP 区；magic/版本不符 → None。
///
/// 注意：未熔断字段（全 0xFF）在解析时归一化为 0，保证
/// `parse(fresh())` 的 flags 为 0（而不是 0xFFFFFFFF）。
pub fn parse(raw: &[u8]) -> Option<Otp> {
    if raw.len() < OTP_SIZE || &raw[0..8] != MAGIC {
        return None;
    }
    let ver = le_u32(&raw[8..12]);
    if ver != VERSION {
        return None;
    }
    let mut boot_pk_sha = [0u8; 32];
    boot_pk_sha.copy_from_slice(&raw[16..48]);
    let mut dev_key = [0u8; 32];
    dev_key.copy_from_slice(&raw[48..80]);
    let flags = if raw[12..16].iter().all(|&b| b == 0xFF) {
        0
    } else {
        le_u32(&raw[12..16]) & FLAG_SECURE_BOOT
    };
    Some(Otp {
        flags,
        boot_pk_sha256: boot_pk_sha,
        device_key: dev_key,
        floor: count_fused_bits(&raw[112..176]),
    })
}

/// 已解析 OTP 视图。
#[derive(Clone, Copy, Debug)]
pub struct Otp {
    pub flags: u32,
    pub boot_pk_sha256: [u8; 32],
    pub device_key: [u8; 32],
    /// 当前防回滚楼层（熔断位数）。
    pub floor: u64,
}

impl Otp {
    pub fn secure_boot_enabled(&self) -> bool {
        self.flags & FLAG_SECURE_BOOT != 0
    }

    /// 授权公钥是否已熔断（全 0xFF = 未熔断）。
    pub fn boot_pk_fused(&self) -> bool {
        !self.boot_pk_sha256.iter().all(|&b| b == 0xFF)
    }

    /// 设备密钥是否已生成。
    pub fn device_key_fused(&self) -> bool {
        !self.device_key.iter().all(|&b| b == 0xFF)
    }

    /// 检查 `sha256(boot_pk)` 是否与熔断摘要一致。
    pub fn boot_pk_matches(&self, sha_of_pk: &[u8; 32]) -> bool {
        self.boot_pk_sha256[..] == sha_of_pk[..] || ct_eq(&self.boot_pk_sha256[..], &sha_of_pk[..])
    }
}

/// 计数域中已熔断（=0）的位数。
fn count_fused_bits(fuses: &[u8]) -> u64 {
    let mut n = 0u64;
    for &b in fuses {
        n += (8 - b.count_ones()) as u64;
    }
    n
}

/// 把 OTP 区初始化为全新（全 0xFF + 写 magic/版本）。
pub fn fresh() -> [u8; OTP_SIZE] {
    let mut raw = [0xFFu8; OTP_SIZE];
    raw[0..8].copy_from_slice(MAGIC);
    put_le_u32(VERSION, &mut raw[8..12]);
    raw
}

/// 写一次语义写入：目标字节必须仍为 0xFF 才允许熔断为 `v`。
/// 返回是否写入成功。
pub fn burn_byte(raw: &mut [u8], off: usize, v: u8) -> bool {
    if off >= OTP_SIZE || raw[off] != 0xFF {
        return false;
    }
    raw[off] = v;
    true
}

/// 设置 flags（含 secure-boot 使能），已设置的部分保留；仅允许 0xFF→真实值。
pub fn burn_flags(raw: &mut [u8], flags: u32) -> bool {
    let mut f = [0u8; 4];
    f.copy_from_slice(&raw[OFF_FLAGS..OFF_FLAGS + 4]);
    let cur = if f.iter().all(|&b| b == 0xFF) {
        0
    } else {
        le_u32(&f) & FLAG_SECURE_BOOT
    };
    // flags 只有 bit0 使用；保证其它位为 0 且允许首次写入
    let set = flags & !FLAG_SECURE_BOOT == 0;
    if flags != 0 && !set {
        return false;
    }
    if cur & flags != 0 {
        return true; // 已熔断
    }
    put_le_u32(cur | flags, &mut raw[OFF_FLAGS..OFF_FLAGS + 4]);
    true
}

/// 熔断 boot 公钥摘要（写一次）。返回是否成功。
pub fn burn_boot_pk(raw: &mut [u8], sha256_of_pk: &[u8; 32]) -> bool {
    if !boot_pk_region_fusable(raw) {
        return false;
    }
    raw[OFF_BOOT_PK_SHA256..OFF_BOOT_PK_SHA256 + 32].copy_from_slice(sha256_of_pk);
    true
}

fn boot_pk_region_fusable(raw: &[u8]) -> bool {
    raw[OFF_BOOT_PK_SHA256..OFF_BOOT_PK_SHA256 + 32]
        .iter()
        .all(|&b| b == 0xFF)
}

/// 生成设备唯一密钥（首次调用熔断；已存在则返回 false）。
pub fn burn_device_key(raw: &mut [u8], key: &[u8; 32]) -> bool {
    if !raw[OFF_DEVICE_KEY..OFF_DEVICE_KEY + 32].iter().all(|&b| b == 0xFF) {
        return false;
    }
    raw[OFF_DEVICE_KEY..OFF_DEVICE_KEY + 32].copy_from_slice(key);
    true
}

/// 读取当前楼层。
pub fn floor(raw: &[u8]) -> u64 {
    count_fused_bits(&raw[OFF_FLOOR_FUSES..OFF_FLOOR_FUSES + FLOOR_FUSES_LEN])
}

/// 把楼层提升到 `n`（熔断足够多的位：从区首开始 1→0）。楼层只升不降。
/// 返回是否成功（n 必须 > 当前且 ≤ FLOOR_MAX）。
pub fn bump_floor(raw: &mut [u8], n: u64) -> bool {
    let cur = floor(raw);
    if n <= cur || n > FLOOR_MAX {
        return false;
    }
    let mut to_burn = (n - cur) as usize;
    for i in 0..FLOOR_FUSES_LEN {
        if to_burn == 0 {
            break;
        }
        for bit in 0..8u32 {
            if to_burn == 0 {
                break;
            }
            let mask = 1u8 << bit;
            let off = OFF_FLOOR_FUSES + i;
            if raw[off] & mask != 0 {
                raw[off] &= !mask; // 1→0 熔断
                to_burn -= 1;
            }
        }
    }
    to_burn == 0
}

/// 序列化视图（保留未熔断区为 0xFF）。
pub fn serialize(otp: &Otp) -> [u8; OTP_SIZE] {
    let mut raw = fresh();
    put_le_u32(otp.flags, &mut raw[OFF_FLAGS..OFF_FLAGS + 4]);
    raw[OFF_BOOT_PK_SHA256..OFF_BOOT_PK_SHA256 + 32].copy_from_slice(&otp.boot_pk_sha256);
    raw[OFF_DEVICE_KEY..OFF_DEVICE_KEY + 32].copy_from_slice(&otp.device_key);
    // floor 还原（按序熔断）
    let mut remaining = otp.floor as usize;
    for i in 0..FLOOR_FUSES_LEN {
        if remaining == 0 {
            break;
        }
        for bit in 0..8u32 {
            if remaining == 0 {
                break;
            }
            raw[OFF_FLOOR_FUSES + i] &= !(1u8 << bit);
            remaining -= 1;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_and_parse() {
        let raw = fresh();
        assert!(parse(&raw).is_some());
        let o = parse(&raw).unwrap();
        assert!(!o.secure_boot_enabled());
        assert!(!o.boot_pk_fused());
        assert!(!o.device_key_fused());
        assert_eq!(o.floor, 0);
    }

    #[test]
    fn burn_once_semantics() {
        let mut raw = fresh();
        assert!(burn_boot_pk(&mut raw, &[0xAB; 32]));
        assert!(!burn_boot_pk(&mut raw, &[0xCD; 32]), "二次熔断被拒");
        let o = parse(&raw).unwrap();
        assert!(o.boot_pk_fused());
        assert!(o.boot_pk_matches(&[0xAB; 32]));
        assert!(!o.boot_pk_matches(&[0xCD; 32]));

        assert!(burn_device_key(&mut raw, &[0x11; 32]));
        assert!(!burn_device_key(&mut raw, &[0x22; 32]), "密钥写一次");
        assert!(parse(&raw).unwrap().device_key_fused());

        // 任意普通字节写一次
        assert!(burn_byte(&mut raw, 200, 0x5A));
        assert!(!burn_byte(&mut raw, 200, 0x6B));
    }

    #[test]
    fn secure_boot_flag_irreversible() {
        let mut raw = fresh();
        assert!(burn_flags(&mut raw, FLAG_SECURE_BOOT));
        let o = parse(&raw).unwrap();
        assert!(o.secure_boot_enabled());
        // 使能后保持
        assert!(burn_flags(&mut raw, FLAG_SECURE_BOOT));
        assert!(parse(&raw).unwrap().secure_boot_enabled());
    }

    #[test]
    fn floor_monotonic() {
        let mut raw = fresh();
        assert!(!bump_floor(&mut raw, 0), "0 不提升");
        assert!(bump_floor(&mut raw, 5));
        assert_eq!(floor(&raw), 5);
        assert!(!bump_floor(&mut raw, 3), "降级被拒");
        assert!(bump_floor(&mut raw, 40));
        assert_eq!(floor(&raw), 40);
        assert!(!bump_floor(&mut raw, 39));
        // 最大上限
        assert!(!bump_floor(&mut raw, FLOOR_MAX + 1));
        assert!(bump_floor(&mut raw, FLOOR_MAX));
        assert_eq!(floor(&raw), FLOOR_MAX);
    }

    #[test]
    fn serialize_restore() {
        let mut raw = fresh();
        burn_flags(&mut raw, FLAG_SECURE_BOOT);
        burn_boot_pk(&mut raw, &[0xAB; 32]);
        burn_device_key(&mut raw, &[0x11; 32]);
        bump_floor(&mut raw, 12);
        let o = parse(&raw).unwrap();
        let raw2 = serialize(&o);
        let o2 = parse(&raw2).unwrap();
        assert_eq!(o.flags, o2.flags);
        assert_eq!(o.boot_pk_sha256, o2.boot_pk_sha256);
        assert_eq!(o.device_key, o2.device_key);
        assert_eq!(o.floor, o2.floor);
    }
}