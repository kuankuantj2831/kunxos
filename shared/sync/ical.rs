//! iCalendar（RFC 5545）解析与生成：VEVENT / VALARM / RRULE 子集。
//!
//! 初始范围（与 docs/sync.md 一致）：
//!   - 解析：VERSION/PRODID/UID/DTSTART/DTEND/DURATION/SUMMARY/LOCATION/
//!     DESCRIPTION/CATEGORIES/STATUS/TRANSP/SEQUENCE/RRULE/EXDATE/RDATE(忽略)/
//!     VALARM(TRIGGER/ACTION)/VTIMEZONE(取首个 TZOFFSETTO 作为固定偏移)
//!   - 生成：规范化的 VCALENDAR + VEVENT（本地新建/修改对象使用）
//!   - RRULE：FREQ=DAILY/WEEKLY/MONTHLY/YEARLY + INTERVAL + COUNT/UNTIL +
//!     BYDAY(周/月)；BYMONTH/BYSETPOS/BYHOUR 等不在初始范围（解析时忽略）。
//!   - 时区：`Z` 后缀=UTC；TZID 命中内置常用时区表或日历内 VTIMEZONE 的
//!     固定偏移；未知 TZID 与浮动时间按 UTC 处理（KunxOS 墙钟为 UTC）。

use crate::sync::model::*;
use crate::sync::util;

// ==================== 行结构 ====================

/// 展开后的一条 iCalendar 属性（借用原始文本切片）。
#[derive(Clone, Copy)]
pub struct Prop<'a> {
    pub name: &'a [u8],
    pub params: &'a [u8],
    pub value: &'a [u8],
}

/// 就地展开折叠行（RFC 5545 §3.1：续行以空格/制表开头），
/// 折叠与换行被压成单个 `\n` 分隔。返回新长度。
pub fn unfold_in_place(text: &mut [u8], len: usize) -> usize {
    let mut o = 0usize;
    let mut i = 0usize;
    while i < len {
        let c = text[i];
        if c == b'\r' || c == b'\n' {
            // 跳过换行；若紧跟空格/制表则一并跳过（续行折叠）
            let mut j = i;
            if c == b'\r' && j + 1 < len && text[j + 1] == b'\n' {
                j += 1;
            }
            if j + 1 < len && (text[j + 1] == b' ' || text[j + 1] == b'\t') {
                i = j + 2;
                continue;
            }
            text[o] = b'\n';
            o += 1;
            i = j + 1;
        } else {
            text[o] = c;
            o += 1;
            i += 1;
        }
    }
    o
}

/// 在 value 中查找第一个不在双引号内的 `:`。
fn find_colon(value: &[u8]) -> Option<usize> {
    let mut in_q = false;
    for (i, &c) in value.iter().enumerate() {
        match c {
            b'"' => in_q = !in_q,
            b':' if !in_q => return Some(i),
            _ => {}
        }
    }
    None
}

/// 把展开后的文本切分为属性数组，返回属性数。
pub fn parse_props<'a>(text: &'a [u8], props: &mut [Prop<'a>]) -> usize {
    let mut n = 0usize;
    let mut start = 0usize;
    while start <= text.len() {
        let mut end = start;
        while end < text.len() && text[end] != b'\n' {
            end += 1;
        }
        let line = &text[start..end];
        if !line.is_empty() {
            if n < props.len() {
                if let Some(ci) = find_colon(line) {
                    let head = &line[..ci];
                    let (name, params) = match util::find_byte(head, b';') {
                        Some(si) => (&head[..si], &head[si + 1..]),
                        None => (head, &[][..]),
                    };
                    props[n] = Prop { name, params, value: &line[ci + 1..] };
                    n += 1;
                }
            }
        }
        if end >= text.len() {
            break;
        }
        start = end + 1;
    }
    n
}

/// 查找属性（大小写不敏感），返回索引。
pub fn find_prop<'a>(props: &[Prop<'a>], name: &[u8]) -> Option<usize> {
    props.iter().position(|p| util::eq_ignore_case(p.name, name))
}

/// 从参数串中取指定参数值（去引号）。如 `TZID=Asia/Shanghai`。
pub fn param_get<'a>(params: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    for part in params.split(|&c| c == b';') {
        let (k, v) = util::split2(part, b'=');
        if util::eq_ignore_case(k, key) {
            let v = util::trim(v);
            let v = if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
                &v[1..v.len() - 1]
            } else {
                v
            };
            return Some(v);
        }
    }
    None
}

// ==================== 文本转义 ====================

/// TEXT 值反转义：`\\` `\;` `\,` `\n` `\N` → 字面字符。
pub fn unescape_text(value: &[u8], out: &mut [u8]) -> usize {
    let mut o = 0usize;
    let mut i = 0usize;
    while i < value.len() {
        if value[i] == b'\\' && i + 1 < value.len() {
            let n = value[i + 1];
            let ch = match n {
                b'n' | b'N' => b'\n',
                b'\\' => b'\\',
                b';' => b';',
                b',' => b',',
                _ => {
                    if o < out.len() {
                        out[o] = b'\\';
                    }
                    o += 1;
                    n
                }
            };
            if ch == b'\n' {
                if o < out.len() {
                    out[o] = b'\n';
                }
                o += 1;
            } else {
                if o < out.len() {
                    out[o] = ch;
                }
                o += 1;
            }
            i += 2;
        } else {
            if o < out.len() {
                out[o] = value[i];
            }
            o += 1;
            i += 1;
        }
    }
    o.min(out.len())
}

/// TEXT 值转义（生成侧）。
pub fn escape_text(src: &[u8], out: &mut [u8]) -> usize {
    let mut o = 0usize;
    for &c in src {
        let esc = match c {
            b'\\' => b'\\',
            b';' => b';',
            b',' => b',',
            b'\n' => b'n',
            _ => 0,
        };
        if esc != 0 {
            if o + 1 < out.len() {
                out[o] = b'\\';
                out[o + 1] = esc;
            }
            o += 2;
        } else {
            if o < out.len() {
                out[o] = c;
            }
            o += 1;
        }
    }
    o.min(out.len())
}

// ==================== 时间 / 时长解析 ====================

/// 常用时区固定标准偏移表（分钟；仅简化初始范围，DST 不处理）。
/// 匹配失败返回 None。
pub fn tzid_known_offset(tzid: &[u8]) -> Option<i32> {
    const TZ: &[(&[u8], i32)] = &[
        (b"UTC", 0),
        (b"Etc/UTC", 0),
        (b"GMT", 0),
        (b"Asia/Shanghai", 480),
        (b"Asia/Hong_Kong", 480),
        (b"Asia/Taipei", 480),
        (b"Asia/Tokyo", 540),
        (b"Asia/Seoul", 540),
        (b"Asia/Singapore", 480),
        (b"Asia/Kolkata", 330),
        (b"Europe/Berlin", 60),
        (b"Europe/Paris", 60),
        (b"Europe/London", 0),
        (b"Europe/Moscow", 180),
        (b"America/New_York", -300),
        (b"America/Chicago", -360),
        (b"America/Denver", -420),
        (b"America/Los_Angeles", -480),
        (b"America/Sao_Paulo", -180),
        (b"Australia/Sydney", 600),
        (b"Australia/Perth", 480),
        (b"Pacific/Auckland", 720),
    ];
    for (name, off) in TZ {
        if *name == tzid {
            return Some(*off);
        }
    }
    None
}

/// 在日历文本中查找 TZID 对应 VTIMEZONE 的固定偏移（取首个
/// TZOFFSETTO，即标准时间偏移；DST 简化为不处理）。
/// 需要文本已展开（unfold_in_place 之后）。
pub fn find_tz_offset(text: &[u8], tzid: &[u8]) -> Option<i32> {
    let mut k = 0usize;
    let mut in_tz = false;
    let mut tz_ok = false;
    while k < text.len() {
        // 逐行扫描（文本已展开，行以 \r\n 分隔）
        let mut end = k;
        while end < text.len() && text[end] != b'\n' {
            end += 1;
        }
        let line = &text[k..end];
        let (name, params, value) = split_line(line);
        let _ = params;
        if util::eq_ignore_case(name, b"BEGIN") && util::eq_ignore_case(util::trim(value), b"VTIMEZONE") {
            in_tz = true;
            tz_ok = false;
        } else if in_tz && util::eq_ignore_case(name, b"TZID") {
            tz_ok = util::trim(value) == tzid;
        } else if in_tz && util::eq_ignore_case(name, b"END") && util::eq_ignore_case(util::trim(value), b"VTIMEZONE") {
            in_tz = false;
            tz_ok = false;
        } else if tz_ok && util::eq_ignore_case(name, b"TZOFFSETTO") {
            if let Some(off) = parse_utc_offset(util::trim(value)) {
                return Some(off);
            }
        }
        k = end + 1;
    }
    None
}

fn split_line(line: &[u8]) -> (&[u8], &[u8], &[u8]) {
    match find_colon(line) {
        Some(ci) => {
            let head = &line[..ci];
            match util::find_byte(head, b';') {
                Some(si) => (&head[..si], &head[si + 1..], &line[ci + 1..]),
                None => (head, &[], &line[ci + 1..]),
            }
        }
        None => (line, &[], &[]),
    }
}

/// 解析 `+0800` / `-0530` / `Z` 形式的 UTC 偏移 → 分钟。
pub fn parse_utc_offset(v: &[u8]) -> Option<i32> {
    if v.len() == 1 && (v[0] == b'Z' || v[0] == b'z') {
        return Some(0);
    }
    if v.len() != 5 || (v[0] != b'+' && v[0] != b'-') {
        return None;
    }
    let h = util::parse_u32(&v[1..3])?;
    let m = util::parse_u32(&v[3..5])?;
    if h > 23 || m > 59 {
        return None;
    }
    let total = (h * 60 + m) as i32;
    Some(if v[0] == b'-' { -total } else { total })
}

/// 解析 iCalendar 日期/时间值 → DateTime。
/// `tz_offset` 为 TZID 解析出的固定偏移（分钟）；若 value 带 `Z` 则忽略。
pub fn parse_ical_dt(value: &[u8], tz_offset: Option<i32>) -> Option<DateTime> {
    let v = util::trim(value);
    // 基本格式：YYYYMMDD 或 YYYYMMDDTHHMMSS[Z]
    if v.len() < 8 || !v[..8].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y = util::parse_i64(&v[0..4])? as i32;
    let mo = util::parse_u32(&v[4..6])?;
    let d = util::parse_u32(&v[6..8])?;
    if mo < 1 || mo > 12 || d < 1 || d > util::days_in_month(y, mo) {
        return None;
    }
    if v.len() == 8 {
        // 全天（DATE 值）
        return Some(DateTime { y, mo, d, h: 0, mi: 0, s: 0, tz_min: 0, has_tz: false });
    }
    if v.len() >= 15 && v[8] == b'T' && v[13..15].iter().all(|c| c.is_ascii_digit()) {
        let h = util::parse_u32(&v[9..11])?;
        let mi = util::parse_u32(&v[11..13])?;
        let s = util::parse_u32(&v[13..15])?;
        if h > 23 || mi > 59 || s > 60 {
            return None;
        }
        let utc = v.len() >= 16 && (v[15] == b'Z' || v[15] == b'z');
        let tz = if utc { 0 } else { tz_offset.unwrap_or(0) };
        return Some(DateTime { y, mo, d, h, mi, s, tz_min: tz, has_tz: true });
    }
    None
}

/// 解析 DURATION（RFC 5545 §3.3.6）：`P[n]W` / `P[n]D[T[n]H[n]M[n]S]` → 秒。
/// 允许前导 `-`（TRIGGER 的“提前”语义，如 `-PT15M`）。
pub fn parse_duration(v: &[u8]) -> Option<i64> {
    let v = util::trim(v);
    if v.is_empty() {
        return None;
    }
    let (neg, rest) = if v[0] == b'-' {
        (true, &v[1..])
    } else if v[0] == b'+' {
        (false, &v[1..])
    } else {
        (false, v)
    };
    if rest.len() < 2 || (rest[0] != b'P' && rest[0] != b'p') {
        return None;
    }
    let mut i = 1;
    let mut total: i64 = 0;
    let mut num: Option<i64> = None;
    let mut saw = false;
    while i < rest.len() {
        let c = rest[i];
        if c.is_ascii_digit() {
            let d = (c - b'0') as i64;
            num = Some(num.unwrap_or(0) * 10 + d);
        } else {
            let n = num.take().unwrap_or(0);
            match c.to_ascii_uppercase() {
                b'W' => total += n * 7 * 86400,
                b'D' => total += n * 86400,
                b'H' => total += n * 3600,
                b'M' => total += n * 60,
                b'S' => total += n,
                b'T' => {}
                _ => return None,
            }
            saw = true;
        }
        i += 1;
    }
    if !saw {
        return None;
    }
    Some(if neg { -total } else { total })
}

// ==================== RRULE ====================

fn byday_bit(name: &[u8]) -> Option<u32> {
    let day = match name {
        b"SU" => 0,
        b"MO" => 1,
        b"TU" => 2,
        b"WE" => 3,
        b"TH" => 4,
        b"FR" => 5,
        b"SA" => 6,
        _ => return None,
    };
    Some(1 << day)
}

/// 解析 RRULE 值（初始子集）。
pub fn parse_rrule(value: &[u8]) -> Rrule {
    let mut r = Rrule::none();
    let mut freq_seen = false;
    for part in value.split(|&c| c == b';') {
        let (k, v) = util::split2(part, b'=');
        let v = util::trim(v);
        if util::eq_ignore_case(k, b"FREQ") {
            freq_seen = true;
            r.freq = if util::eq_ignore_case(v, b"DAILY") {
                FREQ_DAILY
            } else if util::eq_ignore_case(v, b"WEEKLY") {
                FREQ_WEEKLY
            } else if util::eq_ignore_case(v, b"MONTHLY") {
                FREQ_MONTHLY
            } else if util::eq_ignore_case(v, b"YEARLY") {
                FREQ_YEARLY
            } else {
                FREQ_NONE
            };
        } else if util::eq_ignore_case(k, b"INTERVAL") {
            if let Some(iv) = util::parse_u32(v) {
                if iv >= 1 {
                    r.interval = iv;
                }
            }
        } else if util::eq_ignore_case(k, b"COUNT") {
            if let Some(c) = util::parse_u32(v) {
                r.count = c;
                r.has_count = true;
            }
        } else if util::eq_ignore_case(k, b"UNTIL") {
            if let Some(dt) = parse_ical_dt(v, Some(0)) {
                r.until = dt.to_epoch();
                r.has_until = true;
            }
        } else if util::eq_ignore_case(k, b"BYDAY") {
            let mut mask = 0u32;
            for day in v.split(|&c| c == b',') {
                // 允许 "MO" / "2MO"（序数忽略，初始范围）
                let name = if day.len() >= 2 && (day[0].is_ascii_digit() || day[0] == b'+' || day[0] == b'-') {
                    &day[day.len() - 2..]
                } else {
                    day
                };
                if let Some(b) = byday_bit(name) {
                    mask |= b;
                }
            }
            r.byday = mask;
        }
        // BYMONTH/BYSETPOS/BYHOUR 等：初始范围忽略
    }
    if !freq_seen {
        r.freq = FREQ_NONE;
    }
    r
}

// ==================== 事件解析 ====================

/// 解析一个 iCalendar 文本（VCALENDAR 或裸 VEVENT）为 Event。
/// `text` 会被原地展开（折叠消除），因此调用方需传入可变的 blob 缓冲。
/// 返回是否成功。
pub fn parse_event(text: &mut [u8], len: usize, ev: &mut Event) -> bool {
    let new_len = unfold_in_place(text, len);
    let text = &text[..new_len];
    let mut props = [Prop { name: &[], params: &[], value: &[] }; 64];
    let n = parse_props(text, &mut props);

    let mut in_alarm = false;
    let mut dtstart_raw: Option<(&[u8], Option<&[u8]>)> = None; // (value, tzid)
    let mut dtend_raw: Option<(&[u8], Option<&[u8]>)> = None;
    let mut dur_raw: Option<&[u8]> = None;

    // 收集 EXDATE
    let mut exdates: [i64; 16] = [0; 16];
    let mut ex_n = 0usize;

    for i in 0..n {
        let p = &props[i];
        if util::eq_ignore_case(p.name, b"BEGIN") {
            if util::eq_ignore_case(util::trim(p.value), b"VALARM") {
                in_alarm = true;
            }
            continue;
        }
        if util::eq_ignore_case(p.name, b"END") {
            if util::eq_ignore_case(util::trim(p.value), b"VALARM") {
                in_alarm = false;
            }
            continue;
        }
        if in_alarm {
            if util::eq_ignore_case(p.name, b"TRIGGER") {
                let v = util::trim(p.value);
                if let Some(d) = parse_duration(v) {
                    // TRIGGER:-PT15M → 提前 15 分钟；正值/绝对值取正
                    ev.alarm_min = if d < 0 { (-d / 60) as i32 } else { (d / 60) as i32 };
                } else if let Some(dt) = parse_ical_dt(v, None) {
                    let lead = dt.to_epoch() - ev.dtstart.to_epoch();
                    ev.alarm_min = (lead / 60) as i32;
                }
            }
            continue;
        }
        match p.name {
            n if util::eq_ignore_case(n, b"UID") => {
                let v = util::trim(p.value);
                ev.uid_len = util::copy_into(&mut ev.uid, v);
            }
            n if util::eq_ignore_case(n, b"SUMMARY") => {
                ev.summary_len = unescape_text(p.value, &mut ev.summary);
            }
            n if util::eq_ignore_case(n, b"LOCATION") => {
                ev.location_len = unescape_text(p.value, &mut ev.location);
            }
            n if util::eq_ignore_case(n, b"DESCRIPTION") => {
                ev.desc_len = unescape_text(p.value, &mut ev.desc);
            }
            n if util::eq_ignore_case(n, b"CATEGORIES") => {
                ev.categories_len = unescape_text(p.value, &mut ev.categories);
            }
            n if util::eq_ignore_case(n, b"SEQUENCE") => {
                if let Some(s) = util::parse_u32(util::trim(p.value)) {
                    ev.seq = s;
                }
            }
            n if util::eq_ignore_case(n, b"DTSTART") => {
                let tzid = param_get(p.params, b"TZID");
                dtstart_raw = Some((util::trim(p.value), tzid));
            }
            n if util::eq_ignore_case(n, b"DTEND") => {
                let tzid = param_get(p.params, b"TZID");
                dtend_raw = Some((util::trim(p.value), tzid));
            }
            n if util::eq_ignore_case(n, b"DURATION") => {
                dur_raw = Some(util::trim(p.value));
            }
            n if util::eq_ignore_case(n, b"RRULE") => {
                ev.rrule = parse_rrule(p.value);
            }
            n if util::eq_ignore_case(n, b"EXDATE") => {
                let tzid = param_get(p.params, b"TZID");
                let off = tzid.and_then(|t| tzid_known_offset(t)).or_else(|| tzid.and_then(|t| find_tz_offset(text, t)));
                for part in p.value.split(|&c| c == b',') {
                    if ex_n < exdates.len() {
                        if let Some(dt) = parse_ical_dt(part, off) {
                            exdates[ex_n] = dt.to_epoch();
                            ex_n += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 组装 DTSTART / DTEND
    let resolve_tz = |tzid: Option<&[u8]>| -> Option<i32> {
        let tzid = tzid?;
        tzid_known_offset(tzid)
            .or_else(|| find_tz_offset(text, tzid))
    };
    let (raw, tzid) = match dtstart_raw {
        Some(x) => x,
        None => return false,
    };
    let dt = match parse_ical_dt(raw, resolve_tz(tzid)) {
        Some(d) => d,
        None => return false,
    };
    ev.dtstart = dt;
    ev.all_day = dt.is_date();
    if let Some(tzidv) = tzid {
        ev.tzid_len = util::copy_into(&mut ev.tzid, tzidv);
    }

    ev.dtend = dt; // 默认与开始相同（零时长）
    if let Some((raw, tzid)) = dtend_raw {
        if let Some(dte) = parse_ical_dt(raw, resolve_tz(tzid)) {
            ev.dtend = dte;
        }
    } else if let Some(dv) = dur_raw {
        if let Some(secs) = parse_duration(dv) {
            let e = ev.dtstart.to_epoch() + secs;
            ev.dtend = DateTime::from_epoch(e);
        }
    }
    if ev.all_day && ev.dtend == ev.dtstart {
        // 全天事件缺省结束 = 次日
        let e = ev.dtstart.day_epoch() + 86400;
        ev.dtend = DateTime::from_epoch(e);
    }
    ev.exdates = exdates;
    ev.exdates_len = ex_n;
    true
}

// ==================== 事件生成 ====================

/// 生成规范的 VCALENDAR+VEVENT 文本（本地创建 / 修改对象用）。
/// 返回总长度；不足则截断。
pub fn event_to_ical(ev: &Event, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n = prop_ical(out, b"BEGIN", b"VCALENDAR", n);
    n = prop_ical(out, b"VERSION", b"2.0", n);
    n = prop_ical(out, b"PRODID", b"-//KunxOS//KunxOS Sync//CN", n);
    n = prop_ical(out, b"CALSCALE", b"GREGORIAN", n);
    n = prop_ical(out, b"BEGIN", b"VEVENT", n);
    n = prop_ical(out, b"UID", &ev.uid[..ev.uid_len], n);
    if ev.all_day {
        // 全天：DATE 值；DTEND 减一天（独占边界）
        let mut buf = [0u8; 32];
        let m = ev.dtstart.to_ical_text(&mut buf, false);
        n = prop_ical(out, b"DTSTART;VALUE=DATE", &buf[..m], n);
        // DTEND 为独占边界（DATE 值），直接写 dtend 所在日
        let end_dt = if ev.dtend.day_epoch() > ev.dtstart.day_epoch() { ev.dtend } else { DateTime::from_epoch(ev.dtstart.day_epoch() + 86400) };
        let m = end_dt.to_ical_text(&mut buf, false);
        n = prop_ical(out, b"DTEND;VALUE=DATE", &buf[..m], n);
        let _ = m;
    } else {
        let with_tz = ev.dtstart.has_tz && ev.dtstart.tz_min != 0;
        let mut buf = [0u8; 96];
        if with_tz && ev.tzid_len > 0 {
            let mut m = util::push_str(&mut buf, 0, b"TZID=");
            m = util::push_str(&mut buf, m, &ev.tzid[..ev.tzid_len]);
            m = util::push_str(&mut buf, m, b":");
            let t = ev.dtstart.to_ical_text(&mut buf, true);
            n = prop_ical(out, b"DTSTART;", &buf[..t], n);
            let _ = m;
        } else {
            let mut m = ev.dtstart.to_ical_text(&mut buf, true);
            if ev.dtstart.has_tz && ev.dtstart.tz_min == 0 {
                m = util::push_str(&mut buf, m, b"Z");
            }
            n = prop_ical(out, b"DTSTART", &buf[..m], n);
        }
        let mut m = ev.dtend.to_ical_text(&mut buf, true);
        if ev.dtend.has_tz && ev.dtend.tz_min == 0 {
            m = util::push_str(&mut buf, m, b"Z");
        }
        n = prop_ical(out, b"DTEND", &buf[..m], n);
    }
    if ev.summary_len > 0 {
        let mut esc = [0u8; SUMMARY_LEN * 2 + 16];
        let e = escape_text(&ev.summary[..ev.summary_len], &mut esc);
        n = prop_ical(out, b"SUMMARY", &esc[..e], n);
    }
    if ev.location_len > 0 {
        let mut esc = [0u8; LOCATION_LEN * 2 + 16];
        let e = escape_text(&ev.location[..ev.location_len], &mut esc);
        n = prop_ical(out, b"LOCATION", &esc[..e], n);
    }
    if ev.desc_len > 0 {
        let mut esc = [0u8; DESC_LEN * 2 + 16];
        let e = escape_text(&ev.desc[..ev.desc_len], &mut esc);
        n = prop_ical(out, b"DESCRIPTION", &esc[..e], n);
    }
    if ev.categories_len > 0 {
        n = prop_ical(out, b"CATEGORIES", &ev.categories[..ev.categories_len], n);
    }
    if !ev.rrule.is_none() {
        let mut buf = [0u8; 128];
        let mut m = util::push_str(&mut buf, 0, b"FREQ=");
        m = util::push_str(&mut buf, m, freq_name(ev.rrule.freq));
        if ev.rrule.interval > 1 {
            m = util::push_str(&mut buf, m, b";INTERVAL=");
            m = util::push_dec(&mut buf, m, ev.rrule.interval as u64);
        }
        if ev.rrule.byday != 0 {
            m = util::push_str(&mut buf, m, b";BYDAY=");
            let mut first = true;
            for b in 0..7u32 {
                if ev.rrule.byday & (1 << b) != 0 {
                    if !first {
                        m = util::push_str(&mut buf, m, b",");
                    }
                    m = util::push_str(&mut buf, m, byday_name(b));
                    first = false;
                }
            }
        }
        if ev.rrule.has_count {
            m = util::push_str(&mut buf, m, b";COUNT=");
            m = util::push_dec(&mut buf, m, ev.rrule.count as u64);
        } else if ev.rrule.has_until {
            m = util::push_str(&mut buf, m, b";UNTIL=");
            let mut ub = [0u8; 32];
            let mut u = DateTime::from_epoch(ev.rrule.until).to_ical_text(&mut ub, true);
            u = util::push_str(&mut ub, u, b"Z");
            m = util::push_str(&mut buf, m, &ub[..u]);
        }
        n = prop_ical(out, b"RRULE", &buf[..m], n);
    }
    if ev.alarm_min != ALARM_NONE {
        n = prop_ical(out, b"BEGIN", b"VALARM", n);
        n = prop_ical(out, b"ACTION", b"DISPLAY", n);
        n = prop_ical(out, b"DESCRIPTION", b"Reminder", n);
        let mut buf = [0u8; 32];
        let mut m = util::push_str(&mut buf, 0, b"-PT");
        m = util::push_dec(&mut buf, m, ev.alarm_min.max(0) as u64);
        m = util::push_str(&mut buf, m, b"M");
        n = prop_ical(out, b"TRIGGER", &buf[..m], n);
        n = prop_ical(out, b"END", b"VALARM", n);
    }
    n = prop_ical(out, b"END", b"VEVENT", n);
    n = prop_ical(out, b"END", b"VCALENDAR", n);
    n.min(out.len())
}

/// 追加 "NAME:value\r\n"。（结束标记为 CRLF 行尾）
fn prop_ical(out: &mut [u8], name: &[u8], value: &[u8], at: usize) -> usize {
    let mut a = at;
    a = util::push_str(out, a, name);
    a = util::push_str(out, a, b":");
    a = util::push_str(out, a, value);
    a = util::push_str(out, a, b"\r\n");
    a
}

fn freq_name(f: u8) -> &'static [u8] {
    match f {
        FREQ_DAILY => b"DAILY",
        FREQ_WEEKLY => b"WEEKLY",
        FREQ_MONTHLY => b"MONTHLY",
        FREQ_YEARLY => b"YEARLY",
        _ => b"DAILY",
    }
}

fn byday_name(bit: u32) -> &'static [u8] {
    match bit {
        0 => b"SU",
        1 => b"MO",
        2 => b"TU",
        3 => b"WE",
        4 => b"TH",
        5 => b"FR",
        6 => b"SA",
        _ => b"MO",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &[u8]) -> (Event, Vec<u8>) {
        let mut buf = text.to_vec();
        let mut ev = Event::new();
        let bl = buf.len();
        assert!(parse_event(&mut buf, bl, &mut ev));
        (ev, buf)
    }

    #[test]
    fn parse_basic_event() {
        let (ev, _) = parse(
            b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
              UID:ev-1\r\nDTSTART:20250601T090000Z\r\nDTEND:20250601T100000Z\r\n\
              SUMMARY:Team meeting\r\nLOCATION:Room 3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(&ev.uid[..ev.uid_len], b"ev-1");
        assert_eq!(&ev.summary[..ev.summary_len], b"Team meeting");
        assert_eq!(&ev.location[..ev.location_len], b"Room 3");
        assert_eq!(ev.dtstart.to_epoch(), super::super::util::epoch_from_civil(2025, 6, 1, 9, 0, 0));
        assert_eq!(ev.dtend.to_epoch(), super::super::util::epoch_from_civil(2025, 6, 1, 10, 0, 0));
        assert!(!ev.all_day);
        assert!(ev.rrule.is_none());
        assert_eq!(ev.alarm_min, ALARM_NONE);
    }

    #[test]
    fn parse_all_day_and_fold() {
        let (ev, _) = parse(
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:all\r\n\
              DTSTART;VALUE=DATE:20250310\r\nDTEND;VALUE=DATE:20250311\r\n\
              SUMMARY:holiday with a very long title that gets \r\n folded\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert!(ev.all_day);
        assert_eq!(ev.dtstart.day_epoch(), super::super::util::days_from_civil(2025, 3, 10) * 86400);
        assert_eq!(&ev.summary[..ev.summary_len], b"holiday with a very long title that gets folded");
    }

    #[test]
    fn parse_rrule_until() {
        let (ev, _) = parse(
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:r\r\nDTSTART:20250106T080000Z\r\n\
              RRULE:FREQ=WEEKLY;BYDAY=MO,WE;UNTIL=20250120T235959Z\r\n\
              EXDATE:20250108T080000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(ev.rrule.freq, FREQ_WEEKLY);
        assert_eq!(ev.rrule.byday, (1 << 1) | (1 << 3));
        assert!(ev.rrule.has_until);
        assert_eq!(ev.exdates_len, 1);
    }

    #[test]
    fn tzid_resolution() {
        // 内建表
        assert_eq!(tzid_known_offset(b"Asia/Shanghai"), Some(480));
        assert_eq!(tzid_known_offset(b"America/New_York"), Some(-300));
        assert_eq!(tzid_known_offset(b"Nowhere/Zone"), None);
        // VTIMEZONE 扫描
        let text = b"BEGIN:VTIMEZONE\r\nTZID:Custom/Zone\r\nBEGIN:STANDARD\r\n\
                     TZOFFSETFROM:+0600\r\nTZOFFSETTO:+0530\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
        assert_eq!(find_tz_offset(text, b"Custom/Zone"), Some(330));
        assert_eq!(find_tz_offset(text, b"Other/Zone"), None);
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration(b"PT1H30M"), Some(5400));
        assert_eq!(parse_duration(b"P1D"), Some(86400));
        assert_eq!(parse_duration(b"P1W"), Some(604800));
        assert_eq!(parse_duration(b"PT15M"), Some(900));
        assert_eq!(parse_duration(b"P1DT2H"), Some(93600));
        assert_eq!(parse_duration(b"nope"), None);
    }

    #[test]
    fn tzid_dtstart() {
        let (ev, _) = parse(
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:t\r\n\
              DTSTART;TZID=Asia/Shanghai:20250601T090000\r\n\
              DTEND;TZID=Asia/Shanghai:20250601T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(ev.dtstart.to_epoch(), super::super::util::epoch_from_civil(2025, 6, 1, 1, 0, 0));
        assert_eq!(&ev.tzid[..ev.tzid_len], b"Asia/Shanghai");
    }

    #[test]
    fn generate_roundtrip() {
        let mut ev = Event::new();
        ev.uid_len = util::copy_into(&mut ev.uid, b"local-1");
        ev.summary_len = util::copy_into(&mut ev.summary, b"Buy milk, eggs; and bread");
        ev.location_len = util::copy_into(&mut ev.location, b"Cafe\\Corner");
        ev.dtstart = DateTime::new(2025, 7, 1, 18, 30, 0, 0, true);
        ev.dtend = DateTime::new(2025, 7, 1, 19, 0, 0, 0, true);
        ev.rrule = Rrule { freq: FREQ_WEEKLY, interval: 1, count: 4, until: 0, has_count: true, has_until: false, byday: 1 << 2 };
        ev.alarm_min = 15;
        let mut out = [0u8; 1024];
        let n = event_to_ical(&ev, &mut out);
        let mut ev2 = Event::new();
        assert!(parse_event(&mut out, n, &mut ev2));
        assert_eq!(&ev2.summary[..ev2.summary_len], b"Buy milk, eggs; and bread");
        assert_eq!(&ev2.location[..ev2.location_len], b"Cafe\\Corner");
        assert_eq!(ev2.rrule.freq, FREQ_WEEKLY);
        assert_eq!(ev2.rrule.count, 4);
        assert_eq!(ev2.alarm_min, 15);
        assert_eq!(ev2.dtstart.to_epoch(), ev.dtstart.to_epoch());
    }

    #[test]
    fn generate_all_day_roundtrip() {
        let mut ev = Event::new();
        ev.uid_len = util::copy_into(&mut ev.uid, b"a1");
        ev.summary_len = util::copy_into(&mut ev.summary, b"Trip");
        ev.all_day = true;
        ev.dtstart = DateTime::new(2025, 8, 20, 0, 0, 0, 0, false);
        ev.dtend = DateTime::new(2025, 8, 23, 0, 0, 0, 0, false);
        let mut out = [0u8; 1024];
        let n = event_to_ical(&ev, &mut out);
        let mut ev2 = Event::new();
        assert!(parse_event(&mut out, n, &mut ev2));
        assert!(ev2.all_day);
        assert_eq!(ev2.dtstart.day_epoch(), ev.dtstart.day_epoch());
        assert_eq!(ev2.dtend.day_epoch(), ev.dtend.day_epoch());
    }

    #[test]
    fn escaping() {
        let mut out = [0u8; 64];
        let n = escape_text(b"a,b;c\\d\ne", &mut out);
        assert_eq!(&out[..n], b"a\\,b\\;c\\\\d\\ne");
        let mut back = [0u8; 64];
        let m = unescape_text(&out[..n], &mut back);
        assert_eq!(&back[..m], b"a,b;c\\d\ne");
    }
}
