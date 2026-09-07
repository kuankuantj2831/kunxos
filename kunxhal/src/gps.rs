//! 定位框架：GNSS/GPS 接口（NMEA 0183 解析）、WGS84 坐标换算。
//!
//! - [`GpsParser`]：按行（`\n` 结尾）喂入 NMEA 语句，解析 GGA / RMC，
//!   校验 `*HH` 校验和，输出 [`GpsFix`]。
//! - [`GpsFix`]：经纬度（WGS84 十进制度）、海拔、速度、航向、卫星数、
//!   UTC 时间、定位质量。
//! - WGS84 换算：`deg ↔ rad`、`lla → ecef`（A-GPS 预留：辅助星历 /
//!   网络定位需要 ECEF 坐标系）、haversine 距离（地图就近显示）。
//!
//! A-GPS 预留：`A_GPS_RESERVED` 常量与 `ecef_from_lla` 供后续网络辅助
//! （星历注入、粗定位种子）使用；当前版本 GPS 为纯被动接收。

use crate::math;

/// A-GPS 预留标记：当前版本无网络辅助，仅被动 NMEA 解析。
pub const A_GPS_RESERVED: bool = true;

/// 定位质量（GGA 字段 6）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixQuality {
    /// 无定位。
    #[default]
    None = 0,
    Gps = 1,
    Dgps = 2,
    Pps = 3,
    Rtk = 4,
    FloatRtk = 5,
    /// 估算（航位推算等）。
    Estimated = 6,
    Manual = 7,
    Simulated = 8,
}

impl FixQuality {
    pub fn from_u8(v: u8) -> FixQuality {
        match v {
            1 => FixQuality::Gps,
            2 => FixQuality::Dgps,
            3 => FixQuality::Pps,
            4 => FixQuality::Rtk,
            5 => FixQuality::FloatRtk,
            6 => FixQuality::Estimated,
            7 => FixQuality::Manual,
            8 => FixQuality::Simulated,
            _ => FixQuality::None,
        }
    }
    pub fn has_fix(self) -> bool {
        !matches!(self, FixQuality::None)
    }
}

/// 一次定位结果（WGS84）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpsFix {
    /// UTC 时分秒（hh, mm, ss）。
    pub utc: (u32, u32, u32),
    /// UTC 日期（日, 月, 年-2000）。
    pub date: (u32, u32, u32),
    /// 纬度（十进制度，北正）。
    pub lat_deg: f32,
    /// 经度（十进制度，东正）。
    pub lon_deg: f32,
    /// 海拔（米，WGS84 椭球高）。
    pub alt_m: f32,
    /// 对地速度（km/h；RMC 字段，节 × 1.852）。
    pub speed_kmh: f32,
    /// 航向（度，0-360 真北）。
    pub course_deg: f32,
    /// 定位质量。
    pub quality: FixQuality,
    /// 使用卫星数。
    pub num_sats: u32,
    /// 水平精度因子。
    pub hdop: f32,
    /// 语句时间戳（解析时递增，用于新鲜度判断）。
    pub seq: u32,
}

impl GpsFix {
    pub fn has_fix(&self) -> bool {
        self.quality.has_fix()
    }

    /// 距 `(lat2, lon2)` 的大圆距离（米，haversine）。
    pub fn distance_to(&self, lat2: f32, lon2: f32) -> f32 {
        distance_m(self.lat_deg, self.lon_deg, lat2, lon2)
    }
}

// ==================== WGS84 坐标换算 ====================

/// WGS84 长半轴（米）。
pub const WGS84_A: f32 = 6_378_137.0;
/// WGS84 第一偏心率平方。
pub const WGS84_E2: f32 = 0.006_694_379_990_14;

/// 十进制度 → 弧度。
#[inline]
pub fn deg_to_rad(d: f32) -> f32 {
    math::to_rad(d)
}

/// 弧度 → 十进制度。
#[inline]
pub fn rad_to_deg(r: f32) -> f32 {
    math::to_deg(r)
}

/// WGS84 大地坐标 (lat, lon, alt) → ECEF 直角坐标（米）。
/// 供 A-GPS / 融合定位（Wi-Fi + 基站 + GPS 交叉解算）使用。
pub fn ecef_from_lla(lat_deg: f32, lon_deg: f32, alt_m: f32) -> (f32, f32, f32) {
    let lat = deg_to_rad(lat_deg);
    let lon = deg_to_rad(lon_deg);
    let n = WGS84_A / math::sqrt(1.0 - WGS84_E2 * math::sin(lat) * math::sin(lat));
    let x = (n + alt_m) * math::cos(lat) * math::cos(lon);
    let y = (n + alt_m) * math::cos(lat) * math::sin(lon);
    let z = (n * (1.0 - WGS84_E2) + alt_m) * math::sin(lat);
    (x, y, z)
}

/// 两点大圆距离（米，haversine；参数为十进制度）。
pub fn distance_m(lat1: f32, lon1: f32, lat2: f32, lon2: f32) -> f32 {
    let r = 6_371_000.0;
    let (p1, p2) = (deg_to_rad(lat1), deg_to_rad(lat2));
    let dp = deg_to_rad(lat2 - lat1);
    let dl = deg_to_rad(lon2 - lon1);
    let a = math::sin(dp / 2.0) * math::sin(dp / 2.0)
        + math::cos(p1) * math::cos(p2) * math::sin(dl / 2.0) * math::sin(dl / 2.0);
    let c = 2.0 * math::atan2(math::sqrt(a), math::sqrt(1.0 - a));
    r * c
}

// ==================== NMEA 解析 ====================

/// NMEA 语句解析器（状态：累积行缓冲）。
#[derive(Clone, Copy, Debug)]
pub struct GpsParser {
    line_buf: [u8; 128],
    line_len: usize,
    /// 语句序号（新鲜度）。
    seq: u32,
}

impl GpsParser {
    pub fn new() -> Self {
        GpsParser { line_buf: [0; 128], line_len: 0, seq: 0 }
    }

    /// 喂入原始字节流（可任意分块）。解析到一条完整语句时返回对应更新。
    pub fn feed(&mut self, bytes: &[u8]) -> Option<GpsFix> {
        let mut fix = None;
        for &c in bytes {
            if c == b'\n' {
                let line = &self.line_buf[..self.line_len];
                if let Some(f) = parse_nmea(line) {
                    self.seq = self.seq.wrapping_add(1);
                    let mut f = f;
                    f.seq = self.seq;
                    fix = Some(f);
                }
                self.line_len = 0;
            } else if self.line_len < self.line_buf.len() {
                self.line_buf[self.line_len] = c;
                self.line_len += 1;
            }
            // 超长行：丢弃该行（line_len 已满，等待 \n 复位）。
        }
        fix
    }

    /// 喂入一整行（不含换行符），立即解析。
    pub fn feed_line(&mut self, line: &[u8]) -> Option<GpsFix> {
        self.feed(line).or_else(|| self.feed(b"\n"))
    }
}

impl Default for GpsParser {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析一条 NMEA 语句（含 `$...*HH`），返回定位更新；无效返回 None。
pub fn parse_nmea(line: &[u8]) -> Option<GpsFix> {
    let line = trim_line(line);
    if line.len() < 7 || line[0] != b'$' {
        return None;
    }
    // 校验和：`*HH`。
    let star = line.iter().rposition(|&c| c == b'*')?;
    let (body, chk) = (&line[1..star], &line[star + 1..]);
    if chk.len() < 2 || !checksum_ok(body, chk) {
        return None;
    }
    // 语句名（前 5 字符，如 GPGGA / GPRMC）。
    if body.len() < 5 {
        return None;
    }
    let fields = alloc_fields(body);
    // fields[0] 为 talker 前缀（GPGGA 等），数据字段从 fields[1] 起。
    match &body[..5] {
        b"GPGGA" | b"GNGGA" | b"GLGGA" | b"GAGGA" => parse_gga(&fields[1..]),
        b"GPRMC" | b"GNRMC" => parse_rmc(&fields[1..]),
        _ => None,
    }
}

/// 去掉行尾 `\r` 与空白。
fn trim_line(mut line: &[u8]) -> &[u8] {
    while line.last() == Some(&b'\r') || line.last() == Some(&b' ') {
        line = &line[..line.len() - 1];
    }
    line
}

/// 校验和：body 所有字节异或 == chk 十六进制。
fn checksum_ok(body: &[u8], chk: &[u8]) -> bool {
    let mut x = 0u8;
    for &c in body {
        x ^= c;
    }
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'A'..=b'F' => Some(b - b'A' + 10),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    };
    match (hex(chk[0]), hex(chk[1])) {
        (Some(h), Some(l)) => (h << 4 | l) == x,
        _ => false,
    }
}

/// 把 body 按逗号切分为字段（no_std，固定最大字段数）。
fn alloc_fields(body: &[u8]) -> [&[u8]; 24] {
    let mut fields = [&[][..]; 24];
    let mut fi = 0usize;
    let mut start = 0usize;
    for (i, &c) in body.iter().enumerate() {
        if c == b',' {
            if fi < fields.len() {
                fields[fi] = &body[start..i];
                fi += 1;
            }
            start = i + 1;
        }
    }
    if fi < fields.len() {
        fields[fi] = &body[start..];
    }
    fields
}

/// 解析十进制浮点（含负号）。
fn parse_f32(s: &[u8]) -> Option<f32> {
    if s.is_empty() {
        return None;
    }
    let mut neg = false;
    let mut i = 0usize;
    if s[0] == b'-' {
        neg = true;
        i = 1;
    } else if s[0] == b'+' {
        i = 1;
    }
    let mut v = 0.0f32;
    let mut frac = 0.0f32;
    let mut scale = 1.0f32;
    let mut has_digit = false;
    while i < s.len() {
        let c = s[i];
        if c == b'.' {
            i += 1;
            break;
        }
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        has_digit = true;
        v = v * 10.0 + (c - b'0') as f32;
        i += 1;
    }
    while i < s.len() {
        let c = s[i];
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        has_digit = true;
        frac = frac * 10.0 + (c - b'0') as f32;
        scale *= 10.0;
        i += 1;
    }
    if !has_digit {
        return None;
    }
    let v = v + frac / scale;
    Some(if neg { -v } else { v })
}

/// 解析无符号整数。
fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0u32;
    for &c in s {
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
    }
    Some(v)
}

/// NMEA 经纬度 `ddmm.mmmm` → 十进制度。
fn nmea_to_deg(v: f32, hemi: &[u8]) -> Option<f32> {
    let deg = crate::math::floor(v / 100.0);
    let min = v - deg * 100.0;
    let mut d = deg + min / 60.0;
    if hemi.first() == Some(&b'S') || hemi.first() == Some(&b'W') {
        d = -d;
    }
    Some(d)
}

/// 解析 `hhmmss.ss` → (h, m, s)。
fn parse_utc(s: &[u8]) -> Option<(u32, u32, u32)> {
    let v = parse_f32(s)?;
    let h = (v / 10000.0) as u32;
    let m = ((v - h as f32 * 10000.0) / 100.0) as u32;
    let sec = v - h as f32 * 10000.0 - m as f32 * 100.0;
    Some((h % 24, m % 60, sec as u32))
}

/// 解析 `ddmmyy` → (日, 月, 年-2000)。
fn parse_date(s: &[u8]) -> Option<(u32, u32, u32)> {
    if s.len() != 6 {
        return None;
    }
    let d = parse_u32(&s[0..2])?;
    let m = parse_u32(&s[2..4])?;
    let y = parse_u32(&s[4..6])?;
    Some((d, m, y))
}

/// GGA：定位核心数据。
fn parse_gga(f: &[&[u8]]) -> Option<GpsFix> {
    if f.len() < 14 {
        return None;
    }
    let utc = parse_utc(f[0])?;
    let lat = nmea_to_deg(parse_f32(f[1])?, f[2])?;
    let lon = nmea_to_deg(parse_f32(f[3])?, f[4])?;
    let quality = FixQuality::from_u8(parse_u32(f[5]).unwrap_or(0) as u8);
    let num_sats = parse_u32(f[6]).unwrap_or(0);
    let hdop = parse_f32(f[7]).unwrap_or(0.0);
    let alt = parse_f32(f[8]).unwrap_or(0.0);
    Some(GpsFix {
        utc,
        date: (0, 0, 0),
        lat_deg: lat,
        lon_deg: lon,
        alt_m: alt,
        speed_kmh: 0.0,
        course_deg: 0.0,
        quality,
        num_sats,
        hdop,
        seq: 0,
    })
}

/// RMC：速度 / 航向 / 日期。
fn parse_rmc(f: &[&[u8]]) -> Option<GpsFix> {
    if f.len() < 10 {
        return None;
    }
    let utc = parse_utc(f[0])?;
    // f[1]：状态（A=有效 / V=无效）。
    let valid = f[1].first() == Some(&b'A');
    if !valid {
        return Some(GpsFix { quality: FixQuality::None, seq: 0, ..GpsFix::default() });
    }
    let lat = nmea_to_deg(parse_f32(f[2])?, f[3])?;
    let lon = nmea_to_deg(parse_f32(f[4])?, f[5])?;
    let speed_knots = parse_f32(f[6]).unwrap_or(0.0);
    let course = parse_f32(f[7]).unwrap_or(0.0);
    let date = parse_date(f[8]).unwrap_or((0, 0, 0));
    Some(GpsFix {
        utc,
        date,
        lat_deg: lat,
        lon_deg: lon,
        alt_m: 0.0,
        speed_kmh: speed_knots * 1.852,
        course_deg: course,
        quality: FixQuality::Gps,
        num_sats: 0,
        hdop: 0.0,
        seq: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// 生成带正确校验和的 NMEA 语句（body 不含 `$` 与 `*HH`）。
    fn mk(body: &str) -> Vec<u8> {
        let mut x = 0u8;
        for &c in body.as_bytes() {
            x ^= c;
        }
        let mut line = Vec::new();
        line.push(b'$');
        line.extend_from_slice(body.as_bytes());
        line.push(b'*');
        let hex = b"0123456789ABCDEF";
        line.push(hex[(x >> 4) as usize]);
        line.push(hex[(x & 0xF) as usize]);
        line
    }

    #[test]
    fn test_gga() {
        // 北京：39.9042N 116.4074E
        let line = mk("GPGGA,081230.00,3954.25200,N,11624.44400,E,1,08,0.9,54.6,M,0.0,M,,");
        let fix = parse_nmea(&line).unwrap();
        assert!(fix.has_fix());
        assert_eq!(fix.utc, (8, 12, 30));
        assert!((fix.lat_deg - 39.9042).abs() < 0.01, "lat {}", fix.lat_deg);
        assert!((fix.lon_deg - 116.4074).abs() < 0.01, "lon {}", fix.lon_deg);
        assert!((fix.alt_m - 54.6).abs() < 0.1);
        assert_eq!(fix.num_sats, 8);
    }

    #[test]
    fn test_rmc() {
        let line = mk("GPRMC,081230.00,A,3954.25200,N,11624.44400,E,2.5,120.3,150624,,,A");
        let fix = parse_nmea(&line).unwrap();
        assert!(fix.has_fix());
        assert!((fix.speed_kmh - 2.5 * 1.852).abs() < 0.01);
        assert!((fix.course_deg - 120.3).abs() < 0.01);
        assert_eq!(fix.date, (15, 6, 24));
    }

    #[test]
    fn test_checksum_reject() {
        let mut line = mk("GPGGA,081230.00,3954.25200,N,11624.44400,E,1,08,0.9,54.6,M,0.0,M,,");
        let n = line.len();
        line[n - 1] = b'0';
        assert!(parse_nmea(&line).is_none());
    }

    #[test]
    fn test_parser_streaming() {
        let mut p = GpsParser::new();
        let mut full = mk("GPRMC,081230.00,A,3954.25200,N,11624.44400,E,2.5,120.3,150624,,,A");
        full.push(b'\n');
        let (a, b) = full.split_at(20);
        assert!(p.feed(a).is_none());
        let fix = p.feed(b).unwrap();
        assert!(fix.has_fix());
    }

    #[test]
    fn test_distance() {
        // 北京 → 上海 约 1067 km。
        let d = distance_m(39.9042, 116.4074, 31.2304, 121.4737);
        assert!((d - 1_067_000.0).abs() < 50_000.0, "d={d}");
    }

    #[test]
    fn test_ecef() {
        // 赤道 0°N 0°E：x ≈ 6378137，y = z = 0。
        let (x, y, z) = ecef_from_lla(0.0, 0.0, 0.0);
        assert!((x - WGS84_A).abs() < 1.0);
        assert!(y.abs() < 1.0 && z.abs() < 1.0);
    }

    #[test]
    fn test_south_west() {
        let line = mk("GPGGA,081230.00,3351.43333,S,15112.28333,E,1,08,0.9,54.6,M,0.0,M,,");
        let fix = parse_nmea(&line).unwrap();
        assert!(fix.lat_deg < 0.0, "lat {}", fix.lat_deg);
        assert!(fix.lon_deg > 0.0);
    }
}
