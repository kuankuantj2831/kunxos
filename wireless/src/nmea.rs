//! NMEA 0183 GPS 句子解析（GGA / RMC 子集）。
//!
//! 约束：`no_std`、无堆、无 panic——所有解析失败返回 `None`。
//!
//! 支持：带/不带 `$` 前缀、带/不带校验和（带则校验，失败返回 `None`；
//! 不带则容忍）。坐标 `DDDMM.mmm` 转十进制度：`deg = floor(v/100)`、
//! `min = v - deg*100`、`out = deg + min/60`。校验和 = `$` 与 `*` 之间
//! 全部字节的异或。

/// 一个 GPS 定位点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub lat: f64,          // 纬度（度，北正）
    pub lon: f64,          // 经度（度，东正）
    pub altitude_m: Option<f64>,
    pub time_utc: Option<(u8, u8, u8)>,   // (时,分,秒)
    pub date_utc: Option<(u8, u8, u16)>,  // (日,月,年)
    pub num_sats: Option<u8>,
    pub hdop: Option<f64>,
    pub fix_quality: Option<u8>,          // GGA 定位质量 0..8
}

/// 解析一行 NMEA 句子（含或不含 `$` 前缀与校验和）。
///
/// 返回 `Some(Fix)` 若该句是 GGA 或 RMC 且含有效定位；否则 `None`
/// （非定位句返回 `None`，调用方忽略）。
pub fn parse_nmea_line(line: &[u8]) -> Option<Fix> {
    let line = strip_dollar(trim_eol(line));
    let (body, cks) = split_checksum(line);
    if let Some(cks) = cks {
        if parse_hex2(cks)? != checksum(body) {
            return None; // 校验和不匹配
        }
    }
    let mut f = body.split(|c| *c == b',');
    let hdr = f.next()?;
    if hdr.len() < 3 {
        return None;
    }
    match &hdr[hdr.len() - 3..] {
        b"GGA" => parse_gga(f),
        b"RMC" => parse_rmc(f),
        _ => None,
    }
}

/// 解析并合并连续多行：维护一个状态（最后一次 RMC 日期 + GGA 高度），
/// 每次喂入一行返回最新完整 Fix。
#[derive(Clone, Copy, Debug, Default)]
pub struct NmeaParser {
    pub last: Option<Fix>,
    pub date: Option<(u8, u8, u16)>,
}

impl NmeaParser {
    pub fn new() -> Self {
        NmeaParser { last: None, date: None }
    }

    /// 喂入一行；若为有效定位句则与已记录状态（日期 / 高度）合并后
    /// 更新内部状态并返回最新完整 Fix，否则返回 `None` 且状态不变。
    pub fn feed(&mut self, line: &[u8]) -> Option<Fix> {
        let mut fix = parse_nmea_line(line)?;
        // GGA 无日期 → 补上最后一次 RMC 日期。
        if fix.date_utc.is_none() {
            fix.date_utc = self.date;
        }
        // RMC 无高度 → 补上最后一次 GGA 高度。
        if fix.altitude_m.is_none() {
            if let Some(prev) = self.last {
                fix.altitude_m = prev.altitude_m;
            }
        }
        // 更新状态：记录最新日期；把（可能合并过的高度）存回 last。
        if let Some(d) = fix.date_utc {
            self.date = Some(d);
        }
        self.last = Some(fix);
        Some(fix)
    }
}

// ===== 内部工具 =====

/// 去掉行尾的 `\r` / `\n` / 空白。
fn trim_eol(mut l: &[u8]) -> &[u8] {
    while matches!(l.last(), Some(&b'\r') | Some(&b'\n') | Some(&b' ') | Some(&b'\t')) {
        l = &l[..l.len() - 1];
    }
    l
}

/// 去掉可选的 `$` 前缀。
fn strip_dollar(l: &[u8]) -> &[u8] {
    if l.first() == Some(&b'$') {
        &l[1..]
    } else {
        l
    }
}

/// 按 `*` 拆分正文与校验和（校验和可为空/缺失）。
fn split_checksum(l: &[u8]) -> (&[u8], Option<&[u8]>) {
    match l.iter().position(|&b| b == b'*') {
        Some(pos) => (&l[..pos], Some(trim_eol(&l[pos + 1..]))),
        None => (l, None),
    }
}

/// 正文校验和：所有字节异或。
fn checksum(body: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in body {
        c ^= b;
    }
    c
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// 解析两位十六进制校验和。
fn parse_hex2(b: &[u8]) -> Option<u8> {
    if b.len() != 2 {
        return None;
    }
    Some(hex_val(b[0])? * 16 + hex_val(b[1])?)
}

/// 手工解析十进制数（core 无浮点字符串解析），支持可选符号与一个小数点。
fn parse_decimal(b: &[u8]) -> Option<f64> {
    if b.is_empty() {
        return None;
    }
    let mut i = 0;
    let neg = match b[i] {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    if i >= b.len() {
        return None;
    }
    let mut int = 0.0f64;
    let mut frac = 0.0f64;
    let mut frac_div = 1.0f64;
    let mut digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        int = int * 10.0 + (b[i] - b'0') as f64;
        i += 1;
        digits += 1;
    }
    if digits == 0 && (i >= b.len() || b[i] != b'.') {
        return None;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            frac = frac * 10.0 + (b[i] - b'0') as f64;
            frac_div *= 10.0;
            i += 1;
        }
    }
    if i != b.len() {
        return None; // 残留非法字符
    }
    let v = int + frac / frac_div;
    Some(if neg { -v } else { v })
}

/// 解析无符号小整数。
fn parse_u8(b: &[u8]) -> Option<u8> {
    if b.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u32;
        if v > 255 {
            return None;
        }
    }
    Some(v as u8)
}

/// 解析 HHMMSS(.ss) → (时,分,秒)，校验范围。
fn parse_time(b: &[u8]) -> Option<(u8, u8, u8)> {
    if b.len() < 6 {
        return None;
    }
    let h = parse_u8(&b[0..2])?;
    let m = parse_u8(&b[2..4])?;
    let s = parse_u8(&b[4..6])?;
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some((h, m, s))
}

/// 解析 DDMMYY → (日,月,年)。两位年份：<80 → 20xx，否则 19xx。
fn parse_date(b: &[u8]) -> Option<(u8, u8, u16)> {
    if b.len() < 6 {
        return None;
    }
    let d = parse_u8(&b[0..2])?;
    let m = parse_u8(&b[2..4])?;
    let y = parse_u8(&b[4..6])?;
    if d < 1 || d > 31 || m < 1 || m > 12 {
        return None;
    }
    let yy = if y < 80 { 2000 + y as u16 } else { 1900 + y as u16 };
    Some((d, m, yy))
}

/// 解析 `DDDMM.mmm` 坐标为十进制度，并按半球符号取负。
///
/// 整数部分用 u64 精确切分（度 = 整/100，分 = 整%100 + 小数），
/// 避免依赖 core 未稳定的浮点取整函数。
fn parse_latlon(field: &[u8], hemi: &[u8]) -> Option<f64> {
    let (int_part, frac) = parse_int_frac(field)?;
    let deg = (int_part / 100) as f64;
    let min = (int_part % 100) as f64 + frac;
    let mut out = deg + min / 60.0;
    if hemi.first() == Some(&b'S') || hemi.first() == Some(&b'W') {
        out = -out;
    }
    Some(out)
}

/// 解析 `整数[.小数]`，返回 (整数部分, 小数部分)。坐标字段专用：无符号、无指数。
fn parse_int_frac(b: &[u8]) -> Option<(u64, f64)> {
    if b.is_empty() {
        return None;
    }
    let mut i = 0;
    let mut int: u64 = 0;
    let mut digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        int = int.checked_mul(10)?.checked_add((b[i] - b'0') as u64)?;
        i += 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    let mut frac = 0.0f64;
    if i < b.len() {
        if b[i] != b'.' {
            return None;
        }
        i += 1;
        let mut div = 1.0f64;
        while i < b.len() && b[i].is_ascii_digit() {
            frac = frac * 10.0 + (b[i] - b'0') as f64;
            div *= 10.0;
            i += 1;
        }
        frac /= div;
    }
    if i != b.len() {
        return None;
    }
    Some((int, frac))
}

/// 解析 GGA 字段（时间起）。
fn parse_gga<'a>(mut f: impl Iterator<Item = &'a [u8]>) -> Option<Fix> {
    let time = f.next()?;
    let lat_f = f.next()?;
    let lat_h = f.next()?;
    let lon_f = f.next()?;
    let lon_h = f.next()?;
    let qual = f.next()?;
    let sats = f.next()?;
    let hdop = f.next()?;
    let alt = f.next()?;
    // 其余（高度单位/大地水准面等）忽略。

    let quality = parse_u8(qual)?;
    if quality == 0 || quality > 8 {
        return None; // 无有效定位
    }
    let lat = parse_latlon(lat_f, lat_h)?;
    let lon = parse_latlon(lon_f, lon_h)?;
    Some(Fix {
        lat,
        lon,
        altitude_m: if alt.is_empty() { None } else { parse_decimal(alt) },
        time_utc: if time.is_empty() { None } else { parse_time(time) },
        date_utc: None, // GGA 不含日期
        num_sats: if sats.is_empty() { None } else { parse_u8(sats) },
        hdop: if hdop.is_empty() { None } else { parse_decimal(hdop) },
        fix_quality: Some(quality),
    })
}

/// 解析 RMC 字段（时间起）。
fn parse_rmc<'a>(mut f: impl Iterator<Item = &'a [u8]>) -> Option<Fix> {
    let time = f.next()?;
    let status = f.next()?;
    let lat_f = f.next()?;
    let lat_h = f.next()?;
    let lon_f = f.next()?;
    let lon_h = f.next()?;
    let _speed = f.next();
    let _course = f.next();
    let date = f.next()?;

    if status.first() != Some(&b'A') {
        return None; // 状态 'V'（无效）等
    }
    let lat = parse_latlon(lat_f, lat_h)?;
    let lon = parse_latlon(lon_f, lon_h)?;
    Some(Fix {
        lat,
        lon,
        altitude_m: None,
        time_utc: if time.is_empty() { None } else { parse_time(time) },
        date_utc: if date.is_empty() { None } else { parse_date(date) },
        num_sats: None,
        hdop: None,
        fix_quality: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GGA: &[u8] = b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47";
    const RMC: &[u8] = b"$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A";

    #[test]
    fn parse_real_gga() {
        let f = parse_nmea_line(GGA).expect("GGA should parse");
        assert!((f.lat - 48.1173).abs() < 1e-9, "lat = {}", f.lat);
        assert!((f.lon - 11.516_666_666_7).abs() < 1e-9, "lon = {}", f.lon);
        assert_eq!(f.time_utc, Some((12, 35, 19)));
        assert_eq!(f.num_sats, Some(8));
        assert!((f.hdop.unwrap() - 0.9).abs() < 1e-12);
        assert!((f.altitude_m.unwrap() - 545.4).abs() < 1e-9);
        assert_eq!(f.fix_quality, Some(1));
        assert_eq!(f.date_utc, None); // GGA 无日期
    }

    #[test]
    fn parse_real_rmc() {
        let f = parse_nmea_line(RMC).expect("RMC should parse");
        assert!((f.lat - 48.1173).abs() < 1e-9);
        assert!((f.lon - 11.516_666_666_7).abs() < 1e-9);
        assert_eq!(f.time_utc, Some((12, 35, 19)));
        assert_eq!(f.date_utc, Some((23, 3, 1994)));
        assert_eq!(f.altitude_m, None);
        assert_eq!(f.fix_quality, None);
    }

    #[test]
    fn checksum_validation() {
        // 正确校验和 → 解析成功。
        assert!(parse_nmea_line(GGA).is_some());
        assert!(parse_nmea_line(RMC).is_some());
        // 校验和写错 → None。
        let mut bad = GGA.to_vec();
        let n = bad.len();
        bad[n - 2] = b'0'; // *47 → *07
        assert!(parse_nmea_line(&bad).is_none());
        // 正文被篡改但校验和不变 → None。
        let mut bad2 = GGA.to_vec();
        bad2[8] = b'1'; // 时间 123519 → 113519
        assert!(parse_nmea_line(&bad2).is_none());
        // 小写十六进制校验和同样接受。
        let mut low = RMC.to_vec();
        let m = low.len();
        low[m - 2] = b'6';
        low[m - 1] = b'a'; // *6A → *6a
        assert!(parse_nmea_line(&low).is_some());
    }

    #[test]
    fn tolerates_missing_checksum_and_dollar() {
        // 去掉 *47 校验和仍可解析。
        let no_cks = &GGA[..GGA.len() - 3];
        let f = parse_nmea_line(no_cks).expect("missing checksum tolerated");
        assert_eq!(f.fix_quality, Some(1));
        // 去掉 $ 前缀仍可解析。
        let no_dollar = &GGA[1..];
        assert!(parse_nmea_line(no_dollar).is_some());
        // 带 \r\n 行尾。
        let mut crlf = GGA.to_vec();
        crlf.extend_from_slice(b"\r\n");
        assert!(parse_nmea_line(&crlf).is_some());
    }

    #[test]
    fn invalid_or_foreign_sentences() {
        // GGA 定位质量 0 → 无有效定位。
        let q0 = b"$GPGGA,123519,4807.038,N,01131.000,E,0,08,0.9,545.4,M,,,,";
        assert!(parse_nmea_line(q0).is_none());
        // RMC 状态 V → 无效。
        let v = b"$GPRMC,123519,V,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W";
        assert!(parse_nmea_line(v).is_none());
        // 非定位句（GLL / 空 / 乱码）→ None。
        assert!(parse_nmea_line(b"$GPGLL,4807.038,N,01131.000,E,123519,A").is_none());
        assert!(parse_nmea_line(b"").is_none());
        assert!(parse_nmea_line(b"GARBAGE").is_none());
        // 坐标缺失 → None。
        let no_coord = b"$GPRMC,123519,A,,,,,,,230394,,W";
        assert!(parse_nmea_line(no_coord).is_none());
        // 校验和存在但非法 → None。
        assert!(parse_nmea_line(b"$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,,,,*ZZ").is_none());
    }

    #[test]
    fn coordinate_conversion() {
        // 南/西半球取负。
        let s = parse_nmea_line(b"$GPGGA,123519,4807.038,S,01131.000,W,1,08,0.9,545.4,M,,,,").unwrap();
        assert!((s.lat - (-48.1173)).abs() < 1e-9);
        assert!((s.lon - (-11.516_666_666_7)).abs() < 1e-9);
        // 0 分、整数度。
        let z = parse_nmea_line(b"$GPGGA,000000,0000.000,N,00000.000,E,1,01,1.0,0.0,M,,,,").unwrap();
        assert!(z.lat.abs() < 1e-9);
        assert!(z.lon.abs() < 1e-9);
    }

    #[test]
    fn parser_merges_date_and_altitude() {
        let mut p = NmeaParser::new();
        // 先 RMC：有日期、无高度。
        let rmc = p.feed(RMC).expect("feed RMC");
        assert_eq!(rmc.date_utc, Some((23, 3, 1994)));
        assert_eq!(rmc.altitude_m, None);
        assert_eq!(p.date, Some((23, 3, 1994)));
        // 再 GGA：无日期 → 补 RMC 日期；记录高度。
        let gga = p.feed(GGA).expect("feed GGA");
        assert_eq!(gga.date_utc, Some((23, 3, 1994)));
        assert!((gga.altitude_m.unwrap() - 545.4).abs() < 1e-9);
        assert_eq!(p.last.unwrap().altitude_m, Some(545.4));
        // 再一条新 RMC（不同日期）→ 应带上前次 GGA 高度、更新日期。
        let rmc2 = b"$GPRMC,000100,A,4807.038,N,01131.000,E,0.0,0.0,240394,003.1,W*6D";
        let f = p.feed(rmc2).expect("feed RMC 2");
        assert_eq!(f.date_utc, Some((24, 3, 1994)));
        assert!((f.altitude_m.unwrap() - 545.4).abs() < 1e-9);
        // 无效行：状态不变。
        assert!(p.feed(b"$GPRMC,000100,V,,,,,,,,").is_none());
        assert_eq!(p.date, Some((24, 3, 1994)));
    }
}
