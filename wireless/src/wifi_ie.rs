//! IEEE 802.11 管理帧与信息元素（IE）解析/构造。
//!
//! 覆盖 wifid 客户端与内核模拟 AP 需要的子集：
//! - 管理帧头（frame control / duration / addresses / seq）
//! - Beacon / Probe Response / Probe Request
//! - Auth（开放系统 / SAE）与 Assoc Request/Response
//! - IE：SSID、Supported Rates、DS Parameter、RSN、RSNXE、Vendor（WPA）
//! - capability 位、status code、reason code

/// 帧控制：类型/子类型。
pub const FC_TYPE_MGMT: u8 = 0;
pub const FC_TYPE_CTRL: u8 = 1;
pub const FC_TYPE_DATA: u8 = 2;

pub const ST_ASSOC_REQ: u8 = 0;
pub const ST_ASSOC_RESP: u8 = 1;
pub const ST_PROBE_REQ: u8 = 4;
pub const ST_PROBE_RESP: u8 = 5;
pub const ST_AUTH: u8 = 11;
pub const ST_DEAUTH: u8 = 12;
pub const ST_BEACON: u8 = 8;

/// 管理帧固定字段后的信息元素迭代器（SSID 起）。
pub struct IeIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> IeIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        IeIter { data, pos: 0 }
    }
}

impl<'a> Iterator for IeIter<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + 2 > self.data.len() {
            return None;
        }
        let id = self.data[self.pos];
        let len = self.data[self.pos + 1] as usize;
        if self.pos + 2 + len > self.data.len() {
            return None; // 截断：停止迭代
        }
        let body = &self.data[self.pos + 2..self.pos + 2 + len];
        self.pos += 2 + len;
        Some((id, body))
    }
}

/// 常见 IE ID。
pub const IE_SSID: u8 = 0;
pub const IE_SUPP_RATES: u8 = 1;
pub const IE_DS_PARAM: u8 = 3;
pub const IE_TIM: u8 = 5;
pub const IE_RSN: u8 = 48;
pub const IE_EXT_CAP: u8 = 127;
pub const IE_VENDOR: u8 = 221;
pub const IE_RSNXE: u8 = 244;

/// 解析 RSN IE（802.11i）核心字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RsnInfo {
    /// 组密钥套件（如 4 = CCMP-128）。
    pub group_cipher: u16,
    /// 成对密钥套件列表（第一个）。
    pub pairwise_cipher: u16,
    /// AKM 套件列表（第一个）。
    pub akm: u16,
    /// RSN capabilities 原始值。
    pub capabilities: u16,
}

/// 密码套件 OUI。
pub const OUI_80211: [u8; 3] = [0x00, 0x0f, 0xac];

/// 套件类型（type 字节）。
pub const CIPHER_NONE: u8 = 0;
pub const CIPHER_WEP40: u8 = 1;
pub const CIPHER_TKIP: u8 = 2;
pub const CIPHER_CCMP: u8 = 4;
pub const CIPHER_GCMP: u8 = 8;

pub const AKM_8021X: u8 = 1;
pub const AKM_PSK: u8 = 2;
pub const AKM_FT_8021X: u8 = 3;
pub const AKM_FT_PSK: u8 = 4;
pub const AKM_PSK_SHA256: u8 = 6;
pub const AKM_SAE: u8 = 8;
pub const AKM_FT_SAE: u8 = 9;

/// 解析 RSN IE 主体（不含 id/len 两字节）。
/// 返回 None 若结构非法或非 802.11 OUI。
pub fn parse_rsn(body: &[u8]) -> Option<RsnInfo> {
    // version(2) + group(4) + pairwise count(2) + pairwise(4) + akm count(2) + akm(4) + caps(2)
    if body.len() < 20 {
        return None;
    }
    let version = u16::from_le_bytes([body[0], body[1]]);
    if version != 1 {
        return None;
    }
    let group_cipher = parse_suite(&body[2..6])?;
    let pairwise_count = u16::from_le_bytes([body[6], body[7]]) as usize;
    if pairwise_count == 0 || 8 + pairwise_count * 4 + 2 > body.len() {
        return None;
    }
    let pairwise_cipher = parse_suite(&body[8..12])?;
    let akm_count =
        u16::from_le_bytes([body[8 + pairwise_count * 4], body[9 + pairwise_count * 4]]) as usize;
    let akm_off = 10 + pairwise_count * 4;
    if akm_count == 0 || akm_off + akm_count * 4 + 2 > body.len() {
        return None;
    }
    let akm = parse_suite(&body[akm_off..akm_off + 4])?;
    let caps_off = akm_off + akm_count * 4;
    let capabilities = u16::from_le_bytes([body[caps_off], body[caps_off + 1]]);
    Some(RsnInfo { group_cipher, pairwise_cipher, akm, capabilities })
}

/// 解析 4 字节套件选择器（3 字节 OUI + 1 字节 type）。
fn parse_suite(s: &[u8]) -> Option<u16> {
    if s.len() < 4 || s[0..3] != OUI_80211 {
        return None;
    }
    Some(s[3] as u16)
}

/// 构造 RSN IE 主体（802.11i 单套件）。
pub fn build_rsn(pairwise: u8, akm: u8, caps: u16) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..2].copy_from_slice(&1u16.to_le_bytes()); // version
    b[2..5].copy_from_slice(&OUI_80211);
    b[5] = CIPHER_CCMP; // group
    b[6..8].copy_from_slice(&1u16.to_le_bytes()); // pairwise count
    b[8..11].copy_from_slice(&OUI_80211);
    b[11] = pairwise;
    b[12..14].copy_from_slice(&1u16.to_le_bytes()); // akm count
    b[14..17].copy_from_slice(&OUI_80211);
    b[17] = akm;
    b[18..20].copy_from_slice(&caps.to_le_bytes());
    b
}

/// 解析 Vendor IE（WPA）的 OUI。
pub fn vendor_oui(body: &[u8]) -> Option<[u8; 3]> {
    if body.len() < 4 {
        return None;
    }
    Some([body[0], body[1], body[2]])
}

/// 管理帧头。
#[derive(Clone, Copy, Debug)]
pub struct MgmtHeader {
    pub frame_control: u16,
    pub duration: u16,
    pub addr1: [u8; 6],
    pub addr2: [u8; 6],
    pub addr3: [u8; 6],
    pub seq: u16,
}

/// 解析管理帧：返回 (header, payload)。
pub fn parse_mgmt(frame: &[u8]) -> Option<(MgmtHeader, &[u8])> {
    if frame.len() < 24 {
        return None;
    }
    let mut addr1 = [0u8; 6];
    let mut addr2 = [0u8; 6];
    let mut addr3 = [0u8; 6];
    addr1.copy_from_slice(&frame[4..10]);
    addr2.copy_from_slice(&frame[10..16]);
    addr3.copy_from_slice(&frame[16..22]);
    let hdr = MgmtHeader {
        frame_control: u16::from_le_bytes([frame[0], frame[1]]),
        duration: u16::from_le_bytes([frame[2], frame[3]]),
        addr1,
        addr2,
        addr3,
        seq: u16::from_le_bytes([frame[22], frame[23]]) >> 4,
    };
    Some((hdr, &frame[24..]))
}

/// 解析 Beacon/Probe Response 固定字段：返回 (timestamp, beacon_interval, capabilities, ie_data)。
pub fn parse_beacon_body(body: &[u8]) -> Option<(u64, u16, u16, &[u8])> {
    if body.len() < 12 {
        return None;
    }
    let ts = u64::from_le_bytes(body[0..8].try_into().ok()?);
    let interval = u16::from_le_bytes([body[8], body[9]]);
    let caps = u16::from_le_bytes([body[10], body[11]]);
    Some((ts, interval, caps, &body[12..]))
}

/// 从 IE 流中提取 SSID（可能为空 SSID，隐藏网络）。
pub fn find_ssid<'a>(ies: &'a [u8]) -> Option<&'a [u8]> {
    for (id, body) in IeIter::new(ies) {
        if id == IE_SSID {
            return Some(body);
        }
    }
    None
}

/// 从 IE 流中提取 RSN。
pub fn find_rsn(ies: &[u8]) -> Option<RsnInfo> {
    for (id, body) in IeIter::new(ies) {
        if id == IE_RSN {
            return parse_rsn(body);
        }
    }
    None
}

/// 判断是否为 WPA（vendor IE）而非 WPA2（RSN IE）。
pub fn find_wpa_vendor(ies: &[u8]) -> bool {
    for (id, body) in IeIter::new(ies) {
        if id == IE_VENDOR && body.len() >= 4 && body[0] == 0x00 && body[1] == 0x50 && body[2] == 0xf2
        {
            return true;
        }
    }
    false
}

/// 认证算法。
pub const AUTH_OPEN: u16 = 0;
pub const AUTH_SAE: u16 = 3;

/// 状态码（IEEE 802.11-2016 Table 9-50）。
pub const STATUS_SUCCESS: u16 = 0;
pub const STATUS_REJECT: u16 = 1;
pub const STATUS_AP_FULL: u16 = 17;

/// 解析 Auth 帧固定字段：(算法, 事务序号, 状态码)。
pub fn parse_auth(body: &[u8]) -> Option<(u16, u16, u16)> {
    if body.len() < 6 {
        return None;
    }
    let algo = u16::from_le_bytes([body[0], body[1]]);
    let seq = u16::from_le_bytes([body[2], body[3]]);
    let status = u16::from_le_bytes([body[4], body[5]]);
    Some((algo, seq, status))
}

/// 解析 Assoc Response 固定字段：(capabilities, status, aid)。
pub fn parse_assoc_resp(body: &[u8]) -> Option<(u16, u16, u16)> {
    if body.len() < 6 {
        return None;
    }
    let caps = u16::from_le_bytes([body[0], body[1]]);
    let status = u16::from_le_bytes([body[2], body[3]]);
    let aid = u16::from_le_bytes([body[4], body[5]]) & 0x3fff;
    Some((caps, status, aid))
}

/// 构造 Auth 帧体（开放系统）。
pub fn build_auth_body(algo: u16, seq: u16, status: u16) -> [u8; 6] {
    let mut b = [0u8; 6];
    b[0..2].copy_from_slice(&algo.to_le_bytes());
    b[2..4].copy_from_slice(&seq.to_le_bytes());
    b[4..6].copy_from_slice(&status.to_le_bytes());
    b
}

/// 构造 Beacon/Probe Response 固定字段（12 字节）。
pub fn build_beacon_fixed(interval: u16, caps: u16, ts: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..8].copy_from_slice(&ts.to_le_bytes());
    b[8..10].copy_from_slice(&interval.to_le_bytes());
    b[10..12].copy_from_slice(&caps.to_le_bytes());
    b
}

/// 构造 SSID IE（含 id/len）。
pub fn build_ssid_ie(ssid: &[u8]) -> [u8; 34] {
    let mut b = [0u8; 34];
    b[0] = IE_SSID;
    let n = ssid.len().min(32);
    b[1] = n as u8;
    b[2..2 + n].copy_from_slice(&ssid[..n]);
    b
}

/// 构造 RSN IE（含 id/len）。
pub fn build_rsn_ie(pairwise: u8, akm: u8, caps: u16) -> [u8; 22] {
    let mut b = [0u8; 22];
    b[0] = IE_RSN;
    b[1] = 20;
    let body = build_rsn(pairwise, akm, caps);
    b[2..].copy_from_slice(&body);
    b
}

/// capability 位。
pub const CAP_ESS: u16 = 0x0001;
pub const CAP_IBSS: u16 = 0x0002;
pub const CAP_PRIVACY: u16 = 0x0010;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssid_and_rsn_roundtrip() {
        let ssid = build_ssid_ie(b"KunxHome");
        let rsn = build_rsn_ie(CIPHER_CCMP, AKM_PSK, 0);
        let mut ies = [0u8; 60];
        ies[..34].copy_from_slice(&ssid);
        ies[34..56].copy_from_slice(&rsn);
        assert_eq!(find_ssid(&ies[..56]), Some(&b"KunxHome"[..]));
        let info = find_rsn(&ies[..56]).unwrap();
        assert_eq!(info.pairwise_cipher, CIPHER_CCMP as u16);
        assert_eq!(info.akm, AKM_PSK as u16);
    }

    #[test]
    fn parse_beacon() {
        let mut body = [0u8; 64];
        let fixed = build_beacon_fixed(100, CAP_ESS | CAP_PRIVACY, 12345);
        body[..12].copy_from_slice(&fixed);
        let ssid = build_ssid_ie(b"net");
        body[12..46].copy_from_slice(&ssid);
        let (ts, interval, caps, ies) = parse_beacon_body(&body).unwrap();
        assert_eq!(ts, 12345);
        assert_eq!(interval, 100);
        assert_eq!(caps, CAP_ESS | CAP_PRIVACY);
        assert_eq!(find_ssid(ies), Some(&b"net"[..]));
    }
}
