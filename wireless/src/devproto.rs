//! 内核 `/dev/wifi`、`/dev/hci`、`/dev/gnss` 用户态 ↔ 内核态协议。
//!
//! 内核驱动与守护进程（wifid / btd / locationd）按本模块精确定义的常量与
//! 字节布局实现。约束：`no_std`、无堆、无 panic。

// ===== /dev/wifi 协议 =====

/// 命令包（用户 → 内核 write）：`[CMD, 参数...]`。
pub const WIFI_CMD_SCAN: u8 = 0x01;
pub const WIFI_CMD_CONNECT: u8 = 0x02; // 参数：ssid(≤32) 0x00 psk(≤64) 0x00 auth(1)
pub const WIFI_CMD_DISCONNECT: u8 = 0x03;
pub const WIFI_CMD_STATUS: u8 = 0x04;
pub const WIFI_CMD_SET_BACKEND: u8 = 0x10; // 参数：backend id (0=sim, 1=cyw43455)

/// 事件/结果包（内核 → 用户 read）：
pub const WIFI_EVT_SCAN_RESULT: u8 = 0x01; // 每条一个 AP
pub const WIFI_EVT_SCAN_DONE: u8 = 0x02;
pub const WIFI_EVT_CONNECTING: u8 = 0x03;
pub const WIFI_EVT_CONNECTED: u8 = 0x04;
pub const WIFI_EVT_DISCONNECTED: u8 = 0x05;
pub const WIFI_EVT_HANDSHAKE_MSG: u8 = 0x06; // EAPOL-Key 帧载荷（4-way 中继）
pub const WIFI_EVT_ERROR: u8 = 0x07;

/// 扫描结果记录布局（`WIFI_EVT_SCAN_RESULT` 的载荷）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WifiScanRecord {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i8,
    pub auth: u8,  // 0=open 1=wpa2-psk 2=wpa3-sae 3=8021x
    pub flags: u8, // 位0: 隐藏
}

pub const WIFI_AUTH_OPEN: u8 = 0;
pub const WIFI_AUTH_WPA2_PSK: u8 = 1;
pub const WIFI_AUTH_WPA3_SAE: u8 = 2;
pub const WIFI_AUTH_8021X: u8 = 3;

/// 状态记录布局（`WIFI_EVT_CONNECTED` / `WIFI_EVT_DISCONNECTED` 等）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WifiStateRecord {
    pub state: u8, // 0=idle 1=scanning 2=connecting 3=handshake 4=connected
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub bssid: [u8; 6],
    pub rssi: i8,
}

pub const WIFI_STATE_IDLE: u8 = 0;
pub const WIFI_STATE_SCANNING: u8 = 1;
pub const WIFI_STATE_CONNECTING: u8 = 2;
pub const WIFI_STATE_HANDSHAKE: u8 = 3;
pub const WIFI_STATE_CONNECTED: u8 = 4;

/// ioctl 请求码（第三参数：扫描参数 0=主动 1=被动）。
pub const WIFI_IOCTL_TRIGGER_SCAN: u32 = 0x5700;
pub const WIFI_IOCTL_GET_STATE: u32 = 0x5701;

// ===== /dev/hci 协议（H4 帧） =====

/// H4 类型字节（与 src/hci.rs 一致）。
pub const HCI_H4_CMD: u8 = 0x01;
pub const HCI_H4_ACL: u8 = 0x02;
pub const HCI_H4_SCO: u8 = 0x03;
pub const HCI_H4_EVT: u8 = 0x04;

/// /dev/hci 的 ioctl：
pub const HCI_IOCTL_SET_BACKEND: u32 = 0x5800; // 参数：0=sim 1=uart
pub const HCI_IOCTL_GET_BACKEND: u32 = 0x5801;
pub const HCI_IOCTL_SIM_INJECT: u32 = 0x5802; // 模拟控制器注入事件（测试用）

// ===== /dev/gnss 协议 =====

/// 读返回的定位记录（内核 → 用户）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GnssFixRecord {
    pub lat: i32, // 纬度 ×1e7（整数微度）
    pub lon: i32, // 经度 ×1e7
    pub alt_m: i32,
    pub num_sats: u8,
    pub fix_quality: u8,
    pub timestamp_ms: u32,
}

pub const GNSS_IOCTL_SET_SIM_TRACK: u32 = 0x5900; // 参数：轨迹 id（0=静止 1=圆周）

/// 简单字节流读取工具：从 `buf` 解析一个以 `0x00` 结尾的字符串。
///
/// 字符串内容长度必须 ≤ `max`（不含结尾 `0x00`）。成功返回
/// `Some((字符串切片, 含结尾 0x00 的已消费长度))`；找不到结尾或
/// 超过 `max` 返回 `None`。
pub fn parse_cstr(buf: &[u8], max: usize) -> Option<(&[u8], usize)> {
    let limit = buf.len().min(max + 1);
    for i in 0..limit {
        if buf[i] == 0 {
            return Some((&buf[..i], i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn constants_are_exact() {
        // /dev/wifi 命令。
        assert_eq!(WIFI_CMD_SCAN, 0x01);
        assert_eq!(WIFI_CMD_CONNECT, 0x02);
        assert_eq!(WIFI_CMD_DISCONNECT, 0x03);
        assert_eq!(WIFI_CMD_STATUS, 0x04);
        assert_eq!(WIFI_CMD_SET_BACKEND, 0x10);
        // /dev/wifi 事件。
        assert_eq!(WIFI_EVT_SCAN_RESULT, 0x01);
        assert_eq!(WIFI_EVT_SCAN_DONE, 0x02);
        assert_eq!(WIFI_EVT_CONNECTING, 0x03);
        assert_eq!(WIFI_EVT_CONNECTED, 0x04);
        assert_eq!(WIFI_EVT_DISCONNECTED, 0x05);
        assert_eq!(WIFI_EVT_HANDSHAKE_MSG, 0x06);
        assert_eq!(WIFI_EVT_ERROR, 0x07);
        // 认证 / 状态。
        assert_eq!(WIFI_AUTH_OPEN, 0);
        assert_eq!(WIFI_AUTH_WPA2_PSK, 1);
        assert_eq!(WIFI_AUTH_WPA3_SAE, 2);
        assert_eq!(WIFI_AUTH_8021X, 3);
        assert_eq!(WIFI_STATE_IDLE, 0);
        assert_eq!(WIFI_STATE_CONNECTED, 4);
        // ioctl。
        assert_eq!(WIFI_IOCTL_TRIGGER_SCAN, 0x5700);
        assert_eq!(WIFI_IOCTL_GET_STATE, 0x5701);
        // H4 类型。
        assert_eq!(HCI_H4_CMD, 0x01);
        assert_eq!(HCI_H4_ACL, 0x02);
        assert_eq!(HCI_H4_SCO, 0x03);
        assert_eq!(HCI_H4_EVT, 0x04);
        assert_eq!(HCI_IOCTL_SET_BACKEND, 0x5800);
        assert_eq!(HCI_IOCTL_GET_BACKEND, 0x5801);
        assert_eq!(HCI_IOCTL_SIM_INJECT, 0x5802);
        assert_eq!(GNSS_IOCTL_SET_SIM_TRACK, 0x5900);
    }

    #[test]
    fn struct_sizes_and_layouts() {
        // 全 u8 字段、无填充：43 = 32 + 1 + 6 + 1 + 1 + 1 + 1。
        assert_eq!(size_of::<WifiScanRecord>(), 43);
        assert_eq!(align_of::<WifiScanRecord>(), 1);
        // 41 = 1 + 32 + 1 + 6 + 1。
        assert_eq!(size_of::<WifiStateRecord>(), 41);
        assert_eq!(align_of::<WifiStateRecord>(), 1);
        // i32×3 + u8×2 + (2 填充) + u32 = 20，4 字节对齐。
        assert_eq!(size_of::<GnssFixRecord>(), 20);
        assert_eq!(align_of::<GnssFixRecord>(), 4);
        // 字段偏移（repr(C) 保证）。
        let rec = GnssFixRecord {
            lat: 0,
            lon: 0,
            alt_m: 0,
            num_sats: 0,
            fix_quality: 0,
            timestamp_ms: 0,
        };
        let base = &rec as *const _ as usize;
        assert_eq!(&rec.lat as *const _ as usize - base, 0);
        assert_eq!(&rec.lon as *const _ as usize - base, 4);
        assert_eq!(&rec.alt_m as *const _ as usize - base, 8);
        assert_eq!(&rec.num_sats as *const _ as usize - base, 12);
        assert_eq!(&rec.fix_quality as *const _ as usize - base, 13);
        assert_eq!(&rec.timestamp_ms as *const _ as usize - base, 16);
    }

    #[test]
    fn parse_cstr_basic() {
        // 普通：内容不含结尾符。
        let buf = b"hello\0world";
        let (s, n) = parse_cstr(buf, 32).expect("cstr");
        assert_eq!(s, b"hello");
        assert_eq!(n, 6);
        // 空字符串。
        let (s, n) = parse_cstr(b"\0x", 32).expect("empty cstr");
        assert_eq!(s, b"");
        assert_eq!(n, 1);
    }

    #[test]
    fn parse_cstr_bounds() {
        // 无结尾符 → None。
        assert!(parse_cstr(b"hello", 32).is_none());
        assert!(parse_cstr(b"", 32).is_none());
        // 内容超过 max → None。
        assert!(parse_cstr(b"0123456789\0", 5).is_none());
        // 内容恰好等于 max → 允许。
        let (s, n) = parse_cstr(b"12345\0", 5).expect("exactly max");
        assert_eq!(s, b"12345");
        assert_eq!(n, 6);
        // max = 0：仅空字符串允许。
        let (s, _) = parse_cstr(b"\0", 0).expect("empty at max 0");
        assert_eq!(s, b"");
        assert!(parse_cstr(b"a\0", 0).is_none());
    }

    #[test]
    fn scan_record_field_roundtrip() {
        // 构造一条记录并核对原始字节视图（布局一致性）。
        let mut rec = WifiScanRecord {
            ssid: [0u8; 32],
            ssid_len: 0,
            bssid: [0u8; 6],
            channel: 0,
            rssi: 0,
            auth: 0,
            flags: 0,
        };
        rec.ssid[..5].copy_from_slice(b"kunx1");
        rec.ssid_len = 5;
        rec.bssid = [0xaa; 6];
        rec.channel = 6;
        rec.rssi = -45;
        rec.auth = WIFI_AUTH_WPA2_PSK;
        rec.flags = 1;
        let bytes = unsafe {
            core::slice::from_raw_parts(&rec as *const _ as *const u8, size_of::<WifiScanRecord>())
        };
        assert_eq!(bytes[0], b'k');
        assert_eq!(bytes[4], b'1');
        assert_eq!(bytes[32], 5); // ssid_len 紧随 32 字节 ssid
        assert_eq!(&bytes[33..39], &[0xaa; 6]); // bssid
        assert_eq!(bytes[39], 6); // channel
        assert_eq!(bytes[40] as i8, -45); // rssi
        assert_eq!(bytes[41], WIFI_AUTH_WPA2_PSK);
        assert_eq!(bytes[42], 1); // flags
    }
}
