//! BCB v1：A/B 启动控制块（Boot Control Block）。存放于磁盘 misc 原始区，
//! 主/备两个 512 字节副本 + CRC32 + 代次计数（写备→写主，重启取代次大者）。
//!
//! 布局：
//! ```text
//! @0  magic[8] = "KXBCBv10"
//! @8  version u32 = 1
//! @12 active_slot u8      0=A, 1=B
//! @13 tries_remaining u8  当前槽剩余尝试次数
//! @14 tries_total u8      切换/OTA 写入的初始尝试次数（默认 3）
//! @15 slot_ok[2] u8       各槽是否曾成功启动（0/1）
//! @17 boot_mode u8        0=normal 1=recovery
//! @18 ota_in_progress u8  1=OTA 已写入待重启切换
//! @19 reserved u8 = 0
//! @20 build_version u32   当前已确认的构建版本（反降级下限）
//! @24 antirollback_floor u32 OTP 熔断下限快照
//! @28 gen u32             代次计数（主/备交替写，重启取 gen 大者）
//! @32 reserved[472]
//! @504 crc32 u32          小端，crc32(bytes[0..504))
//! ```

use crate::crc32::crc32;
use crate::{le_u32, put_le_u32};

pub const BCB_SIZE: usize = 512;
pub const MAGIC: &[u8; 8] = b"KXBCBv10";
pub const VERSION: u32 = 1;
/// OTA 切换后新槽的初始尝试次数。
pub const DEFAULT_TRIES: u8 = 3;

/// 槽位常量。
pub const SLOT_A: u8 = 0;
pub const SLOT_B: u8 = 1;

/// 启动模式。
pub const BOOT_NORMAL: u8 = 0;
pub const BOOT_RECOVERY: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bcb {
    pub active_slot: u8,
    pub tries_remaining: u8,
    pub tries_total: u8,
    pub slot_ok: [bool; 2],
    pub boot_mode: u8,
    pub ota_in_progress: bool,
    pub build_version: u32,
    pub antirollback_floor: u32,
    pub gen: u32,
}

impl Bcb {
    /// 全新 BCB：槽 A 活动、normal 模式、无尝试、版本 0。
    pub fn new() -> Self {
        Bcb {
            active_slot: SLOT_A,
            tries_remaining: 0,
            tries_total: DEFAULT_TRIES,
            slot_ok: [false; 2],
            boot_mode: BOOT_NORMAL,
            ota_in_progress: false,
            build_version: 0,
            antirollback_floor: 0,
            gen: 0,
        }
    }

    /// 另一个槽位。
    pub fn other_slot(slot: u8) -> u8 {
        if slot == SLOT_A {
            SLOT_B
        } else {
            SLOT_A
        }
    }

    /// 槽名字符（'A'/'B'）。
    pub fn slot_name(slot: u8) -> u8 {
        if slot == SLOT_A {
            b'A'
        } else {
            b'B'
        }
    }

    /// 启动尝试开始：递减剩余次数（bootloader 在每次尝试新槽时调用前先检查）。
    /// 返回新的剩余次数。
    pub fn begin_try(&mut self) -> u8 {
        if self.tries_remaining > 0 {
            self.tries_remaining -= 1;
        }
        self.tries_remaining
    }

    /// 是否应回滚：当前槽未成功过且尝试次数已耗尽。
    pub fn should_rollback(&self) -> bool {
        !self.slot_ok[self.active_slot as usize] && self.tries_remaining == 0
    }

    /// 启动成功：标记当前槽成功、清零尝试、提升确认版本、结束 OTA 状态。
    pub fn mark_boot_success(&mut self, image_version: u32) {
        self.slot_ok[self.active_slot as usize] = true;
        self.tries_remaining = 0;
        if image_version > self.build_version {
            self.build_version = image_version;
        }
        self.ota_in_progress = false;
        self.gen = self.gen.wrapping_add(1);
    }

    /// OTA 写完后切换：活动槽 → 另一槽，重置尝试计数，标记 OTA 进行中。
    pub fn prepare_switch(&mut self) {
        self.active_slot = Self::other_slot(self.active_slot);
        self.tries_total = DEFAULT_TRIES;
        self.tries_remaining = DEFAULT_TRIES;
        self.ota_in_progress = true;
        self.gen = self.gen.wrapping_add(1);
    }

    /// 手动切到 `slot`（恢复菜单/调试用）。
    pub fn force_slot(&mut self, slot: u8) {
        if slot != self.active_slot {
            self.active_slot = slot;
            self.tries_remaining = DEFAULT_TRIES;
            self.gen = self.gen.wrapping_add(1);
        }
    }

    pub fn set_recovery(&mut self) {
        self.boot_mode = BOOT_RECOVERY;
        self.gen = self.gen.wrapping_add(1);
    }

    pub fn clear_recovery(&mut self) {
        self.boot_mode = BOOT_NORMAL;
        self.gen = self.gen.wrapping_add(1);
    }

    pub fn serialize(&self) -> [u8; BCB_SIZE] {
        let mut b = [0u8; 512];
        b[0..8].copy_from_slice(MAGIC);
        put_le_u32(1, &mut b[8..12]);
        b[12] = self.active_slot;
        b[13] = self.tries_remaining;
        b[14] = self.tries_total;
        b[15] = (self.slot_ok[0] as u8) | ((self.slot_ok[1] as u8) << 1);
        b[17] = self.boot_mode;
        b[18] = self.ota_in_progress as u8;
        b[19] = 0;
        put_le_u32(self.build_version, &mut b[20..24]);
        put_le_u32(self.antirollback_floor, &mut b[24..28]);
        put_le_u32(self.gen, &mut b[28..32]);
        // b[32..504] 保留为 0
        put_le_u32(crc32(&b[0..504]), &mut b[504..508]);
        b
    }

    /// 解析单个 512 字节 BCB 副本；magic/version/CRC 任一不符 → None。
    pub fn parse(b: &[u8]) -> Option<Bcb> {
        if b.len() < BCB_SIZE || &b[0..8] != MAGIC {
            return None;
        }
        let ver = le_u32(&b[8..12]);
        if ver != VERSION {
            return None;
        }
        if le_u32(&b[504..508]) != crc32(&b[0..504]) {
            return None;
        }
        let okb = b[15];
        Some(Bcb {
            active_slot: b[12] & 1,
            tries_remaining: b[13],
            tries_total: b[14],
            slot_ok: [(okb & 1) != 0, (okb & 2) != 0],
            boot_mode: b[17],
            ota_in_progress: b[18] != 0,
            build_version: le_u32(&b[20..24]),
            antirollback_floor: le_u32(&b[24..28]),
            gen: le_u32(&b[28..32]),
        })
    }

    /// 从主/备两个副本中取有效且代次最新者；两个都无效 → None。
    pub fn parse_double(primary: &[u8], backup: &[u8]) -> Option<Bcb> {
        let p = Bcb::parse(primary);
        let q = Bcb::parse(backup);
        match (p, q) {
            (Some(a), Some(b)) => Some(if b.gen > a.gen { b } else { a }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_crc() {
        let mut b = Bcb::new();
        b.active_slot = SLOT_B;
        b.prepare_switch();
        b.mark_boot_success(42);
        let raw = b.serialize();
        let c = Bcb::parse(&raw).expect("parse");
        assert_eq!(c.active_slot, b.active_slot);
        assert_eq!(c.build_version, 42);
        assert_eq!(c.gen, b.gen);
        assert_eq!(c.slot_ok[c.active_slot as usize], true);
        // 篡改 → 无效
        let mut bad = raw;
        bad[13] ^= 1;
        assert!(Bcb::parse(&bad).is_none());
        // magic 错 → 无效
        let mut bad = raw;
        bad[0] = b'X';
        assert!(Bcb::parse(&bad).is_none());
    }

    #[test]
    fn rollback_decision() {
        let mut b = Bcb::new();
        b.active_slot = SLOT_B;
        b.tries_total = DEFAULT_TRIES;
        b.tries_remaining = DEFAULT_TRIES;
        // 新槽 B 连续崩溃尝试 3 次后应回滚
        assert!(!b.should_rollback());
        b.begin_try();
        b.begin_try();
        assert!(!b.should_rollback());
        b.begin_try();
        assert!(b.should_rollback(), "tries exhausted, slot B not confirmed");
        assert_eq!(Bcb::other_slot(SLOT_B), SLOT_A);
        assert_eq!(Bcb::slot_name(SLOT_B), b'B');
        // 成功标记后不应回滚
        b.mark_boot_success(1);
        assert!(!b.should_rollback());
    }

    #[test]
    fn double_copy_recovery() {
        let b = Bcb { ..Bcb::new() };
        let q = b.serialize();
        // 主更新
        let mut b2 = b.clone();
        b2.mark_boot_success(7);
        let p2 = b2.serialize();
        assert_eq!(Bcb::parse_double(&p2, &q).unwrap().build_version, 7);
        // 主损坏 → 用备
        let mut m = b2.serialize();
        m[20..24].copy_from_slice(&[0xEE; 4]); // CRC 失效
        assert_eq!(Bcb::parse_double(&m, &q).unwrap().gen, b.gen);
        // 都损坏 → None
        let mut z = q;
        z[0] = 0;
        assert!(Bcb::parse_double(&m, &z).is_none());
    }
}