//! KunxOS 时区数据库（tzdb 子集）与本地时间换算。
//!
//! 时区规则以 POSIX TZ 字符串表达（紧凑、无外部数据文件）：
//!   - `CST-8`                 固定偏移 UTC+8（POSIX 符号与习惯相反：正数 = 格林尼治以西）
//!   - `EST5EDT,M3.2.0,M11.1.0` 标准 EST(UTC-5)、夏令时 EDT(UTC-4)；
//!                              3 月第 2 个周日 02:00 开始，11 月第 1 个周日 02:00 结束
//!   - `GMT0BST,M3.5.0/1,M10.5.0` 带时刻的规则（/1 = 01:00）
//!
//! 覆盖 OOBE 提供的 8 个时区 + 常用补充。宿主可测（无系统调用）。

/// 解析后的时区规则。
#[derive(Clone, Copy, Debug)]
pub struct TzRule {
    /// IANA 时区名。
    pub name: &'static str,
    /// POSIX TZ 字符串。
    pub posix: &'static str,
}

/// 内置时区表（覆盖 OOBE 8 个 + 常用补充）。
pub static TZDB: &[TzRule] = &[
    TzRule { name: "Asia/Shanghai", posix: "CST-8" },
    TzRule { name: "Asia/Tokyo", posix: "JST-9" },
    TzRule { name: "Asia/Seoul", posix: "KST-9" },
    TzRule { name: "Asia/Singapore", posix: "SGT-8" },
    TzRule { name: "Asia/Dubai", posix: "GST-4" },
    TzRule { name: "Asia/Kolkata", posix: "IST-5:30" },
    TzRule { name: "Europe/London", posix: "GMT0BST,M3.5.0/1,M10.5.0" },
    TzRule { name: "Europe/Paris", posix: "CET-1CEST,M3.5.0/2,M10.5.0/3" },
    TzRule { name: "Europe/Moscow", posix: "MSK-3" },
    TzRule { name: "America/New_York", posix: "EST5EDT,M3.2.0/2,M11.1.0/2" },
    TzRule { name: "America/Chicago", posix: "CST6CDT,M3.2.0/2,M11.1.0/2" },
    TzRule { name: "America/Los_Angeles", posix: "PST8PDT,M3.2.0/2,M11.1.0/2" },
    TzRule { name: "America/Sao_Paulo", posix: "BRT3BRST,M10.3.0/0,M2.3.0/0" },
    TzRule { name: "Australia/Sydney", posix: "AEST-10AEDT,M10.1.0/2,M4.1.0/3" },
    TzRule { name: "UTC", posix: "UTC0" },
];

/// 本地时间结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalTime {
    pub y: i32,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub min: u32,
    pub s: u32,
    /// 相对 UTC 的偏移（分钟，东为正）。
    pub offset_min: i32,
    /// 是否夏令时。
    pub dst: bool,
}

// ==================== 公历/儒略日换算（Howard Hinnant 算法） ====================

/// days since 1970-01-01 → 公历 (y, m, d)。
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

/// 公历 (y, m, d) → days since 1970-01-01。
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn weekday(days: i64) -> u32 {
    // 1970-01-01 是周四（4）。(days + 4) mod 7，0=周日
    let w = (days + 4).rem_euclid(7);
    w as u32
}

/// 一年中某月第 w 个星期 d 的日序（w=5 表示最后一个）。返回 (days, day_of_month)。
fn nth_weekday_of_month(y: i32, m: u32, w: u32, d: u32) -> (i64, u32) {
    let first = days_from_civil(y, m, 1);
    let first_wd = weekday(first);
    // 第一个 d 的偏移
    let off = (d + 7 - first_wd) % 7;
    let mut day = 1 + off;
    if w >= 2 {
        day += (w - 1) * 7;
    }
    // w=5：最后一个（若超出月天数则回退一周）
    if w == 5 {
        let days_in_month = {
            let (y2, m2, _) = if m == 12 { (y + 1, 1, 0) } else { (y, m + 1, 0) };
            days_from_civil(y2, m2, 1) - days_from_civil(y, m, 1)
        };
        if (day as i64) > days_in_month {
            day -= 7;
        }
    }
    (days_from_civil(y, m, day), day)
}

// ==================== POSIX TZ 解析 ====================

#[derive(Clone, Copy, Debug)]
struct ParsedTz {
    std_off_min: i32,
    dst_off_min: i32,
    has_dst: bool,
    /// DST 开始：(月, 周, 星期, 时刻分钟)（Mm.w.d 规则）
    start: (u32, u32, u32, u32),
    end: (u32, u32, u32, u32),
}

/// 解析 POSIX TZ 字符串（支持 `std offset[dst[offset][,start[/time],end[/time]]]`）。
fn parse_posix(posix: &str) -> Option<ParsedTz> {
    let b = posix.as_bytes();
    // 读取 std 缩写（3+ 字母）
    let mut i = 0usize;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    // 读取偏移
    let (off, ni) = parse_offset(b, i)?;
    let std_off_min = off; // POSIX 正=西，取反为东正分钟
    i = ni;
    // 读取可选 dst
    let mut has_dst = false;
    let mut dst_off_min = std_off_min + 60; // POSIX 默认：夏令时比标准早 1 小时
    let mut start = (3, 2, 0, 120);
    let mut end = (11, 1, 0, 120);
    if i < b.len() && b[i].is_ascii_alphabetic() {
        has_dst = true;
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        // 可选 dst 偏移
        if i < b.len() && (b[i] == b'+' || b[i] == b'-' || b[i].is_ascii_digit()) {
            let (o, ni2) = parse_offset(b, i)?;
            dst_off_min = o;
            i = ni2;
        }
        // 可选 ,start,end
        if i < b.len() && b[i] == b',' {
            i += 1;
            let (s, ni3) = parse_rule_date(b, i)?;
            start = s;
            i = ni3;
            if i < b.len() && b[i] == b',' {
                i += 1;
                let (e, ni4) = parse_rule_date(b, i)?;
                end = e;
                i = ni4;
            }
        }
    }
    let _ = i;
    Some(ParsedTz {
        std_off_min,
        dst_off_min,
        has_dst,
        start,
        end,
    })
}

/// 解析 [+|-]hh[:mm[:ss]]（返回分钟，POSIX 符号：正=西）。
fn parse_offset(b: &[u8], mut i: usize) -> Option<(i32, usize)> {
    let sign: i32 = if i < b.len() && b[i] == b'-' {
        i += 1;
        1
    } else if i < b.len() && b[i] == b'+' {
        i += 1;
        -1
    } else {
        -1
    };
    let mut hh = 0i32;
    while i < b.len() && b[i].is_ascii_digit() {
        hh = hh * 10 + (b[i] - b'0') as i32;
        i += 1;
    }
    let mut mm = 0i32;
    if i < b.len() && b[i] == b':' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            mm = mm * 10 + (b[i] - b'0') as i32;
            i += 1;
        }
    }
    let mut ss = 0i32;
    if i < b.len() && b[i] == b':' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            ss = ss * 10 + (b[i] - b'0') as i32;
            i += 1;
        }
    }
    // POSIX 正偏移 = 西；转为东正分钟
    Some((sign * (hh * 60 + mm + ss / 60), i))
}

/// 解析规则日期 `Mm.w.d[/time]`。
fn parse_rule_date(b: &[u8], i: usize) -> Option<((u32, u32, u32, u32), usize)> {
    if i + 2 < b.len() && b[i] == b'M' {
        let mut j = i + 1;
        let mut m = 0u32;
        while j < b.len() && b[j].is_ascii_digit() {
            m = m * 10 + (b[j] - b'0') as u32;
            j += 1;
        }
        if j < b.len() && b[j] == b'.' {
            j += 1;
        } else {
            return None;
        }
        let mut w = 0u32;
        while j < b.len() && b[j].is_ascii_digit() {
            w = w * 10 + (b[j] - b'0') as u32;
            j += 1;
        }
        if j < b.len() && b[j] == b'.' {
            j += 1;
        } else {
            return None;
        }
        let mut d = 0u32;
        while j < b.len() && b[j].is_ascii_digit() {
            d = d * 10 + (b[j] - b'0') as u32;
            j += 1;
        }
        let mut t = 120u32;
        if j < b.len() && b[j] == b'/' {
            j += 1;
            let (hh, jj) = parse_clock(b, j)?;
            t = hh;
            j = jj;
        }
        Some(((m, w, d, t), j))
    } else {
        None
    }
}

fn parse_clock(b: &[u8], mut j: usize) -> Option<(u32, usize)> {
    let mut hh = 0u32;
    while j < b.len() && b[j].is_ascii_digit() {
        hh = hh * 10 + (b[j] - b'0') as u32;
        j += 1;
    }
    let mut mm = 0u32;
    if j < b.len() && b[j] == b':' {
        j += 1;
        while j < b.len() && b[j].is_ascii_digit() {
            mm = mm * 10 + (b[j] - b'0') as u32;
            j += 1;
        }
    }
    Some((hh * 60 + mm, j))
}

// ==================== UTC → 本地时间 ====================

/// 判断给定本地时间（年/月/日/分钟-of-day）是否处于 DST 区间。
/// 规则按"本地标准时刻"比较（与 tzdata 一致：DST 切换发生在本地时刻）。
fn in_dst(rule: &ParsedTz, y: i32, mo: u32, d: u32, minute_of_day: u32) -> bool {
    if !rule.has_dst {
        return false;
    }
    let (start_days, _start_day) = nth_weekday_of_month(y, rule.start.0, rule.start.1, rule.start.2);
    let (end_days, _end_day) = nth_weekday_of_month(y, rule.end.0, rule.end.1, rule.end.2);
    let start_min = start_days * 1440 + rule.start.3 as i64;
    let end_min = end_days * 1440 + rule.end.3 as i64;
    let cur = days_from_civil(y, mo, d) * 1440 + minute_of_day as i64;
    // 北半球（start < end）与南半球（start > end）统一处理
    if start_min < end_min {
        cur >= start_min && cur < end_min
    } else {
        cur >= start_min || cur < end_min
    }
}

/// 把 UTC 公历时间换算为某时区本地时间。`zone` 为 TZDB 索引。
pub fn utc_to_local(zone: usize, utc_y: i32, utc_mo: u32, utc_d: u32, h: u32, min: u32, s: u32) -> Option<LocalTime> {
    let rule = TZDB.get(zone)?;
    let parsed = parse_posix(rule.posix)?;
    let utc_min = days_from_civil(utc_y, utc_mo, utc_d) * 1440 + h as i64 * 60 + min as i64;
    // 第一遍：按标准偏移近似
    let local_min0 = utc_min + parsed.std_off_min as i64;
    let (y0, mo0, d0) = civil_from_days(local_min0.div_euclid(1440));
    let tod0 = local_min0.rem_euclid(1440) as u32;
    let dst = in_dst(&parsed, y0, mo0, d0, tod0);
    let off = if dst { parsed.dst_off_min } else { parsed.std_off_min };
    // 第二遍：用确定的偏移重算（处理跨日 DST 边界）
    let local_min = utc_min + off as i64;
    let (y, mo, d) = civil_from_days(local_min.div_euclid(1440));
    let tod = local_min.rem_euclid(1440) as u32;
    let dst2 = in_dst(&parsed, y, mo, d, tod);
    let off2 = if dst2 { parsed.dst_off_min } else { parsed.std_off_min };
    let local_min2 = utc_min + off2 as i64;
    let (y2, mo2, d2) = civil_from_days(local_min2.div_euclid(1440));
    let tod2 = local_min2.rem_euclid(1440) as u32;
    Some(LocalTime {
        y: y2,
        mo: mo2,
        d: d2,
        h: tod2 / 60,
        min: tod2 % 60,
        s,
        offset_min: off2,
        dst: dst2,
    })
}

/// 由 Unix 纪元秒换算为本地时间。
pub fn epoch_to_local(zone: usize, epoch_secs: i64) -> Option<LocalTime> {
    let days = epoch_secs.div_euclid(86400);
    let tod = epoch_secs.rem_euclid(86400) as u32;
    let (y, mo, d) = civil_from_days(days);
    utc_to_local(zone, y, mo, d, tod / 3600, (tod / 60) % 60, tod % 60)
}

/// 时区索引 → 当前（按给定 UTC 时刻）偏移分钟。
pub fn zone_offset_min(zone: usize, utc_epoch_secs: i64) -> i32 {
    epoch_to_local(zone, utc_epoch_secs)
        .map(|t| t.offset_min)
        .unwrap_or(0)
}

/// 按名称查时区索引（未找到返回 None）。
pub fn zone_by_name(name: &str) -> Option<usize> {
    TZDB.iter().position(|z| z.name == name)
}

/// 常见时区名（与 OOBE 一致，供显示）。
pub fn zone_name(zone: usize) -> &'static str {
    TZDB.get(zone).map(|z| z.name).unwrap_or("UTC")
}
