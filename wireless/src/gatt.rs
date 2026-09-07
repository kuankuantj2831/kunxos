//! GATT：服务/特征数据库与发现辅助函数。
//!
//! 16 位 UUID 常量按蓝牙规范以大端数值书写（如 0x2800 = Primary Service）；
//! ATT PDU 中传输时按小端编码，解析处用 `u16::from_le_bytes` 还原。
//! 无堆：数据库为静态数组，查询均为线性扫描。

/// 16 位 UUID 常量。
pub const UUID_PRIMARY_SERVICE: u16 = 0x2800;
pub const UUID_CHARACTERISTIC: u16 = 0x2803;
pub const UUID_CCCD: u16 = 0x2902;
pub const UUID_GAP_SERVICE: u16 = 0x1800;
pub const UUID_DEVICE_NAME_CHAR: u16 = 0x2A00;
pub const UUID_GATT_SERVICE: u16 = 0x1801;
pub const UUID_BATTERY_SERVICE: u16 = 0x180F;
pub const UUID_BATTERY_LEVEL: u16 = 0x2A19;
pub const UUID_HEART_RATE_SERVICE: u16 = 0x180D;
pub const UUID_HEART_RATE_MEASUREMENT: u16 = 0x2A37;

/// 特征属性位。
pub const CHAR_PROP_BROADCAST: u8 = 0x01;
pub const CHAR_PROP_READ: u8 = 0x02;
pub const CHAR_PROP_WRITE_WITHOUT_RESP: u8 = 0x04;
pub const CHAR_PROP_WRITE: u8 = 0x08;
pub const CHAR_PROP_NOTIFY: u8 = 0x10;
pub const CHAR_PROP_INDICATE: u8 = 0x20;

/// GATT 特征声明（Read By Type 0x2803 响应项）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharDecl {
    pub properties: u8,
    pub value_handle: u16,
    pub uuid16: Option<u16>, // 16 位 UUID（若为 128 位则为 None）
    pub uuid128: [u8; 16],
}

/// 解析 Read By Type 响应（0x2803 特征声明）的每一项。
///
/// 支持两种长度：5 字节（16 位 UUID）与 19 字节（128 位 UUID）。128 位 UUID 按 ATT
/// 传输顺序（小端）原样存入 `uuid128`。长度非法返回 `None`。
pub fn parse_char_decl(data: &[u8]) -> Option<CharDecl> {
    if data.len() < 5 {
        return None;
    }
    let properties = data[0];
    let value_handle = u16::from_le_bytes([data[1], data[2]]);
    let mut uuid128 = [0u8; 16];
    if data.len() == 5 {
        let uuid16 = u16::from_le_bytes([data[3], data[4]]);
        Some(CharDecl {
            properties,
            value_handle,
            uuid16: Some(uuid16),
            uuid128,
        })
    } else if data.len() == 19 {
        uuid128.copy_from_slice(&data[3..19]);
        Some(CharDecl {
            properties,
            value_handle,
            uuid16: None,
            uuid128,
        })
    } else {
        None
    }
}

/// 解析 Find Information 响应（句柄-UUID 对），返回条数（最多 8）。
///
/// 仅支持 format 1（16 位 UUID 对）：`[format(1)=0x01, (handle(2 LE), uuid(2 LE))...]`。
/// 数据截断时只统计完整对；format 2（128 位）返回 0。
pub fn parse_handle_uuid_pairs(data: &[u8], out: &mut [(u16, u16); 8]) -> usize {
    if data.len() < 1 || data[0] != 0x01 {
        return 0;
    }
    let mut count = 0;
    let mut pos = 1;
    while pos + 4 <= data.len() && count < 8 {
        let handle = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let uuid = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
        out[count] = (handle, uuid);
        count += 1;
        pos += 4;
    }
    count
}

/// 简单 GATT 服务数据库（静态数组，无堆）。
#[derive(Clone, Copy, Debug)]
pub struct GattDb {
    pub services: [(u16, u16, u16); 8], // (start_handle, end_handle, uuid16)
    pub chars: [(u16, u16, u8, u16); 16], // (decl_handle, value_handle, properties, uuid16)
    pub service_count: usize,
    pub char_count: usize,
}

impl GattDb {
    /// 内置「KunxSim」外设数据库：
    ///
    /// | handle | 属性 | UUID |
    /// |--------|------|------|
    /// | 1      | 主服务 GAP | 0x1800 |
    /// | 2      | 特征声明 设备名（读） | 0x2803 → 0x2A00 |
    /// | 3      | 设备名值 | 0x2A00 |
    /// | 4      | 主服务 电池（Battery） | 0x180F |
    /// | 5      | 特征声明 电量（读+通知） | 0x2803 → 0x2A19 |
    /// | 6      | 电量值 | 0x2A19 |
    /// | 7      | CCCD | 0x2902 |
    /// | 8      | 主服务 心率（HeartRate） | 0x180D |
    /// | 9      | 特征声明 心率（读+通知） | 0x2803 → 0x2A37 |
    /// | 10     | 心率值 | 0x2A37 |
    /// | 11     | CCCD | 0x2902 |
    pub fn kunx_sim() -> Self {
        let mut db = GattDb {
            services: [(0, 0, 0); 8],
            chars: [(0, 0, 0, 0); 16],
            service_count: 0,
            char_count: 0,
        };
        db.services[0] = (1, 3, UUID_GAP_SERVICE);
        db.chars[0] = (2, 3, CHAR_PROP_READ, UUID_DEVICE_NAME_CHAR);
        db.services[1] = (4, 7, UUID_BATTERY_SERVICE);
        db.chars[1] = (5, 6, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_BATTERY_LEVEL);
        db.services[2] = (8, 11, UUID_HEART_RATE_SERVICE);
        db.chars[2] = (9, 10, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_HEART_RATE_MEASUREMENT);
        db.service_count = 3;
        db.char_count = 3;
        db
    }

    /// 按句柄查找特征：命中值句柄本身；对支持通知/指示的特征，其紧随的 CCCD
    /// 句柄（值句柄 + 1）同样命中（便于把 CCCD 写操作归属到特征）。
    /// 返回 `(value_handle, properties, uuid16)`。
    pub fn find_char_by_value_handle(&self, handle: u16) -> Option<(u16, u8, u16)> {
        for &(_, vh, props, uuid16) in self.chars.iter().take(self.char_count) {
            if handle == vh {
                return Some((vh, props, uuid16));
            }
            if (props & (CHAR_PROP_NOTIFY | CHAR_PROP_INDICATE)) != 0 && handle == vh + 1 {
                return Some((vh, props, uuid16));
            }
        }
        None
    }

    /// 返回 `(start, end, uuid)` 覆盖 handle 的服务（含边界）。
    pub fn service_for_handle(&self, handle: u16) -> Option<(u16, u16, u16)> {
        for &(s, e, uuid) in self.services.iter().take(self.service_count) {
            if handle >= s && handle <= e {
                return Some((s, e, uuid));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kunx_sim_structure() {
        let db = GattDb::kunx_sim();
        assert_eq!(db.service_count, 3);
        assert_eq!(db.char_count, 3);
        // 服务覆盖
        assert_eq!(db.service_for_handle(1), Some((1, 3, UUID_GAP_SERVICE)));
        assert_eq!(db.service_for_handle(6), Some((4, 7, UUID_BATTERY_SERVICE)));
        assert_eq!(db.service_for_handle(11), Some((8, 11, UUID_HEART_RATE_SERVICE)));
        assert_eq!(db.service_for_handle(0), None);
        assert_eq!(db.service_for_handle(100), None);
        // 特征：值句柄
        assert_eq!(
            db.find_char_by_value_handle(3),
            Some((3, CHAR_PROP_READ, UUID_DEVICE_NAME_CHAR))
        );
        // 电量：值句柄 + 通知 → CCCD 句柄 7 也命中
        assert_eq!(
            db.find_char_by_value_handle(6),
            Some((6, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_BATTERY_LEVEL))
        );
        assert_eq!(
            db.find_char_by_value_handle(7),
            Some((6, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_BATTERY_LEVEL))
        );
        // 心率
        assert_eq!(
            db.find_char_by_value_handle(10),
            Some((10, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_HEART_RATE_MEASUREMENT))
        );
        assert_eq!(
            db.find_char_by_value_handle(11),
            Some((10, CHAR_PROP_READ | CHAR_PROP_NOTIFY, UUID_HEART_RATE_MEASUREMENT))
        );
        // 只读特征（无 CCCD）的“值句柄+1”不得误命中服务声明句柄 4
        assert_eq!(db.find_char_by_value_handle(4), None);
    }

    #[test]
    fn parse_char_decl_16_and_128() {
        // 16 位：props=READ|NOTIFY, value_handle=6, uuid=0x2A19
        let d16 = [0x12, 0x06, 0x00, 0x19, 0x2A];
        let c = parse_char_decl(&d16).unwrap();
        assert_eq!(c.properties, CHAR_PROP_READ | CHAR_PROP_NOTIFY);
        assert_eq!(c.value_handle, 6);
        assert_eq!(c.uuid16, Some(UUID_BATTERY_LEVEL));
        assert_eq!(c.uuid128, [0u8; 16]);
        // 128 位：props=READ, value_handle=7, uuid 1..16（小端传输序）
        let mut d128 = [0u8; 19];
        d128[0] = CHAR_PROP_READ;
        d128[1..3].copy_from_slice(&7u16.to_le_bytes());
        for i in 0..16 {
            d128[3 + i] = (i as u8).wrapping_add(1);
        }
        let c2 = parse_char_decl(&d128).unwrap();
        assert_eq!(c2.uuid16, None);
        assert_eq!(c2.value_handle, 7);
        assert_eq!(c2.uuid128[0], 1);
        assert_eq!(c2.uuid128[15], 16);
        // 长度非法
        assert!(parse_char_decl(&[0x02, 0x00, 0x00]).is_none());
        assert!(parse_char_decl(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_none());
    }

    #[test]
    fn handle_uuid_pairs_roundtrip() {
        let mut data = [0u8; 33];
        data[0] = 0x01; // format 1
        let pairs = [
            (1u16, 0x2800u16),
            (3, 0x2803),
            (5, 0x2A00),
            (7, 0x2902),
            (9, 0x2A19),
        ];
        for (i, (h, u)) in pairs.iter().enumerate() {
            let off = 1 + i * 4;
            data[off..off + 2].copy_from_slice(&h.to_le_bytes());
            data[off + 2..off + 4].copy_from_slice(&u.to_le_bytes());
        }
        let mut out = [(0u16, 0u16); 8];
        let n = parse_handle_uuid_pairs(&data[..1 + 5 * 4], &mut out);
        assert_eq!(n, 5);
        assert_eq!(&out[..5], &pairs);
        // 截断输入：只解析完整对
        let n2 = parse_handle_uuid_pairs(&data[..1 + 5 * 4 - 2], &mut out);
        assert_eq!(n2, 4);
        assert_eq!(&out[..4], &pairs[..4]);
        // format 2 / 空 → 0
        assert_eq!(parse_handle_uuid_pairs(&[0x02, 0, 0, 0, 0], &mut out), 0);
        assert_eq!(parse_handle_uuid_pairs(&[], &mut out), 0);
        // 超过 8 对时截断到 8
        let mut big = [0u8; 1 + 12 * 4];
        big[0] = 0x01;
        for i in 0..12 {
            let off = 1 + i * 4;
            big[off..off + 2].copy_from_slice(&(i as u16 + 1).to_le_bytes());
            big[off + 2..off + 4].copy_from_slice(&UUID_CCCD.to_le_bytes());
        }
        let n3 = parse_handle_uuid_pairs(&big, &mut out);
        assert_eq!(n3, 8);
        assert_eq!(out[0], (1, UUID_CCCD));
        assert_eq!(out[7], (8, UUID_CCCD));
    }
}
