//! KunxOS 同步栈数据模型：日历事件 / 联系人 / 账户 / 集合 / 同步状态。
//!
//! 全部为固定尺寸结构体（无堆），可直接嵌入索引记录并整体序列化。
//! 同步状态使用位标志（flags）表达：本地脏、墓碑、冲突、服务器已知等。

// ==================== 尺寸上限 ====================

/// iCalendar/vCard 内的 UID（事件 / 联系人身份，区别于存储键 href）。
pub const UID_LEN: usize = 64;
/// 存储记录键 = 服务器资源路径（href），如 /dav/cal/xxx.ics。
pub const KEY_LEN: usize = 128;
pub const ETAG_LEN: usize = 48;
pub const SUMMARY_LEN: usize = 160;
pub const LOCATION_LEN: usize = 128;
pub const DESC_LEN: usize = 256;
pub const CATEGORIES_LEN: usize = 64;
pub const TZID_LEN: usize = 32;
pub const NAME_LEN: usize = 128;
pub const TEL_LEN: usize = 40;
pub const EMAIL_LEN: usize = 64;
pub const GROUP_LEN: usize = 40;
pub const NOTE_LEN: usize = 256;
pub const PHOTO_LEN: usize = 4096; // 解码后照片字节上限
pub const MIME_LEN: usize = 16;
pub const SERVER_LEN: usize = 160;
pub const PRINCIPAL_LEN: usize = 192;
pub const USERNAME_LEN: usize = 64;
pub const SECRET_LEN: usize = 128;
pub const COLL_PATH_LEN: usize = 192;
pub const COLL_NAME_LEN: usize = 48;
pub const TOKEN_LEN: usize = 96;
pub const CTAG_LEN: usize = 64;
pub const SECRET_NAME_LEN: usize = 48;

pub const MAX_TELS: usize = 4;
pub const MAX_EMAILS: usize = 4;
pub const MAX_GROUPS: usize = 8;

/// 每个存储（事件 / 联系人）的最大记录数。
pub const MAX_RECORDS: usize = 512;
/// 索引记录固定尺寸（与 store.rs 的布局一致）。
pub const REC_SIZE: usize = 256;

// ==================== 同步标志位 ====================

/// 本地已修改，待推送（同步队列的“在途”标记）。
pub const FLAG_DIRTY: u32 = 1 << 0;
/// 本地已删除（墓碑）：待向服务器发送 DELETE。
pub const FLAG_DELETED: u32 = 1 << 1;
/// 冲突待解决（本地与服务器同时修改）。
pub const FLAG_CONFLICT: u32 = 1 << 2;
/// 服务器已知该对象（已确认的远程 ETag 存在）。
pub const FLAG_ON_SERVER: u32 = 1 << 3;
/// 仅在本地创建、从未推送到服务器。
pub const FLAG_LOCAL_ONLY: u32 = 1 << 4;

// ==================== 集合 / 账户类型 ====================

pub const COLL_CALENDAR: u8 = 0;
pub const COLL_ADDRBOOK: u8 = 1;

pub const ACCT_LOCAL: u8 = 0;
pub const ACCT_GOOGLE: u8 = 1;
pub const ACCT_APPLE: u8 = 2;
pub const ACCT_ICLOUD: u8 = 3;
pub const ACCT_GENERIC: u8 = 4;

pub const AUTH_BASIC: u8 = 0;
pub const AUTH_OAUTH2: u8 = 1;

// ==================== 日期时间 ====================

/// 本地日期时间。`tz_min` 为 UTC 偏移（分钟）；`has_tz=false` 表示
/// iCalendar 浮动时间（floating），按本地墙钟处理（KunxOS 当前墙钟为 UTC，
/// 简化策略：浮动时间按 UTC 存储，见 docs/sync.md 时区一节）。
#[derive(Clone, Copy, PartialEq)]
pub struct DateTime {
    pub y: i32,
    pub mo: u32, // 1-12
    pub d: u32,  // 1-31
    pub h: u32,
    pub mi: u32,
    pub s: u32,
    pub tz_min: i32,
    pub has_tz: bool,
}

impl DateTime {
    pub fn new(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, tz_min: i32, has_tz: bool) -> Self {
        DateTime { y, mo, d, h, mi, s, tz_min, has_tz }
    }

    /// 是否为全天（日期型）值。
    pub fn is_date(&self) -> bool {
        self.h == 0 && self.mi == 0 && self.s == 0 && !self.has_tz
    }

    /// 转 epoch 秒（按 tz_min 折算 UTC）。
    pub fn to_epoch(&self) -> i64 {
        let utc = crate::sync::util::epoch_from_civil(self.y, self.mo, self.d, self.h, self.mi, self.s);
        if self.has_tz {
            utc - (self.tz_min as i64) * 60
        } else {
            utc
        }
    }

    /// 从 epoch 秒构造（UTC）。
    pub fn from_epoch(e: i64) -> Self {
        let (y, mo, d, h, mi, s) = crate::sync::util::civil_from_epoch(e);
        DateTime { y, mo, d, h, mi, s, tz_min: 0, has_tz: true }
    }

    /// 该日期的 UTC 午夜 epoch 秒（用于全天事件边界与查询）。
    pub fn day_epoch(&self) -> i64 {
        crate::sync::util::days_from_civil(self.y, self.mo, self.d) * 86400
    }

    /// 文本表示 YYYYMMDDTHHMMSS（本地字段，不折算，零填充）。
    pub fn to_ical_text(&self, buf: &mut [u8], with_time: bool) -> usize {
        let mut n = crate::sync::util::push_dec_pad(buf, 0, self.y as u64, 4);
        n = crate::sync::util::push_dec_pad(buf, n, self.mo as u64, 2);
        n = crate::sync::util::push_dec_pad(buf, n, self.d as u64, 2);
        if with_time {
            n = crate::sync::util::push_str(buf, n, b"T");
            n = crate::sync::util::push_dec_pad(buf, n, self.h as u64, 2);
            n = crate::sync::util::push_dec_pad(buf, n, self.mi as u64, 2);
            n = crate::sync::util::push_dec_pad(buf, n, self.s as u64, 2);
        }
        n
    }

    pub fn weekday(&self) -> u32 {
        crate::sync::util::weekday_from_days(crate::sync::util::days_from_civil(self.y, self.mo, self.d))
    }
}

// ==================== 重复规则（RRULE，初始子集） ====================

pub const FREQ_NONE: u8 = 0;
pub const FREQ_DAILY: u8 = 1;
pub const FREQ_WEEKLY: u8 = 2;
pub const FREQ_MONTHLY: u8 = 3;
pub const FREQ_YEARLY: u8 = 4;

/// 重复规则。BYDAY 为 7 位掩码（bit0=周日 … bit6=周六），
/// 仅周重复 / 月重复（按星期几）使用；COUNT/UNTIL 二选一或皆无（无限）。
/// EXDATE（排除实例）随事件存储，见 Event。
#[derive(Clone, Copy, PartialEq)]
pub struct Rrule {
    pub freq: u8,
    pub interval: u32,
    pub count: u32,
    pub until: i64,
    pub has_count: bool,
    pub has_until: bool,
    pub byday: u32,
}

impl Rrule {
    pub fn none() -> Self {
        Rrule { freq: FREQ_NONE, interval: 1, count: 0, until: 0, has_count: false, has_until: false, byday: 0 }
    }

    pub fn is_none(&self) -> bool {
        self.freq == FREQ_NONE
    }

    /// 计算下一次重复的起始 epoch 秒（严格 > after）。
    /// 返回 None 表示没有更多实例（或超出迭代上限）。
    pub fn next_after(&self, start: i64, after: i64) -> Option<i64> {
        if self.is_none() {
            return None;
        }
        let mut out = [0i64; 1];
        // 上界：UNTIL 或 after + 100 年（expand 内部还有迭代上限兜底）
        let hi = if self.has_until { self.until } else { after + 100 * 366 * 86400 };
        if self.expand(start, after + 1, hi, &mut out) > 0 {
            Some(out[0])
        } else {
            None
        }
    }

    /// 生成 [from, to]（含两端，epoch 秒）内的所有实例起始时间，
    /// 写入 out，返回数量（截断到 out 容量）。
    pub fn expand(&self, start: i64, from: i64, to: i64, out: &mut [i64]) -> usize {
        let mut n = 0usize;
        if self.is_none() {
            return 0;
        }
        let interval = self.interval.max(1) as i64;
        let tod = start.rem_euclid(86400);
        let start_day = start - tod;
        let start_dow = crate::sync::util::weekday_from_days(start_day / 86400) as i64; // 0=周日
        let mut emitted: u64 = 0;
        let cap: u64 = 1_000_000;
        let push = |cand: i64, n: &mut usize, out: &mut [i64]| -> bool {
            if *n >= out.len() {
                return false;
            }
            out[*n] = cand;
            *n += 1;
            true
        };
        match self.freq {
            FREQ_DAILY => {
                let mut i = 0i64;
                loop {
                    if emitted >= cap {
                        break;
                    }
                    if self.has_count && emitted >= self.count as u64 {
                        break;
                    }
                    let cand = start + i * interval * 86400;
                    if self.has_until && cand > self.until {
                        break;
                    }
                    if cand > to {
                        break;
                    }
                    if cand >= from && !push(cand, &mut n, out) {
                        break;
                    }
                    emitted += 1;
                    i += 1;
                }
            }
            FREQ_WEEKLY => {
                if self.byday == 0 {
                    let mut i = 0i64;
                    loop {
                        if emitted >= cap {
                            break;
                        }
                        if self.has_count && emitted >= self.count as u64 {
                            break;
                        }
                        let cand = start + i * interval * 7 * 86400;
                        if self.has_until && cand > self.until {
                            break;
                        }
                        if cand > to {
                            break;
                        }
                        if cand >= from && !push(cand, &mut n, out) {
                            break;
                        }
                        emitted += 1;
                        i += 1;
                    }
                } else {
                    // 按周推进：每周内按星期几顺序产出
                    let mut week = 0i64;
                    'weeks: loop {
                        if emitted >= cap {
                            break;
                        }
                        let week_start0 = start_day - start_dow * 86400 + week * interval * 7 * 86400;
                        for b in 0..7i64 {
                            if self.byday & (1 << b) == 0 {
                                continue;
                            }
                            let cand = week_start0 + b * 86400 + tod;
                            if week == 0 && cand < start {
                                continue;
                            }
                            if self.has_until && cand > self.until {
                                break 'weeks;
                            }
                            if self.has_count && emitted >= self.count as u64 {
                                break 'weeks;
                            }
                            if cand > to {
                                break 'weeks;
                            }
                            if cand >= from && !push(cand, &mut n, out) {
                                break 'weeks;
                            }
                            emitted += 1;
                        }
                        week += 1;
                    }
                }
            }
            FREQ_MONTHLY => {
                let (y0, m0, d0, h, mi, s) = crate::sync::util::civil_from_epoch(start);
                let mut k = 0i64;
                loop {
                    if emitted >= cap {
                        break;
                    }
                    if self.has_count && emitted >= self.count as u64 {
                        break;
                    }
                    let total = (y0 as i64) * 12 + (m0 as i64 - 1) + k * interval;
                    let ny = total.div_euclid(12) as i32;
                    let nm = total.rem_euclid(12) as u32 + 1;
                    let nd = d0.min(crate::sync::util::days_in_month(ny, nm));
                    let cand = crate::sync::util::epoch_from_civil(ny, nm, nd, h, mi, s);
                    if self.has_until && cand > self.until {
                        break;
                    }
                    if cand > to {
                        break;
                    }
                    if cand >= from && !push(cand, &mut n, out) {
                        break;
                    }
                    emitted += 1;
                    k += 1;
                }
            }
            FREQ_YEARLY => {
                let (y0, mo0, d0, h, mi, s) = crate::sync::util::civil_from_epoch(start);
                let mut k = 0i64;
                loop {
                    if emitted >= cap {
                        break;
                    }
                    if self.has_count && emitted >= self.count as u64 {
                        break;
                    }
                    let ny = y0 + (k * interval) as i32;
                    let nd = d0.min(crate::sync::util::days_in_month(ny, mo0));
                    let cand = crate::sync::util::epoch_from_civil(ny, mo0, nd, h, mi, s);
                    if self.has_until && cand > self.until {
                        break;
                    }
                    if cand > to {
                        break;
                    }
                    if cand >= from && !push(cand, &mut n, out) {
                        break;
                    }
                    emitted += 1;
                    k += 1;
                }
            }
            _ => {}
        }
        n
    }
}

// ==================== 提醒（VALARM，初始子集） ====================

pub const ALARM_NONE: i32 = i32::MAX;

// ==================== 日历事件 ====================

#[derive(Clone, Copy)]
pub struct Event {
    pub uid: [u8; UID_LEN],
    pub uid_len: usize,
    pub summary: [u8; SUMMARY_LEN],
    pub summary_len: usize,
    pub location: [u8; LOCATION_LEN],
    pub location_len: usize,
    pub desc: [u8; DESC_LEN],
    pub desc_len: usize,
    pub categories: [u8; CATEGORIES_LEN],
    pub categories_len: usize,
    pub dtstart: DateTime,
    pub dtend: DateTime,
    pub all_day: bool,
    pub rrule: Rrule,
    /// 提前提醒分钟数；ALARM_NONE 表示无提醒。
    pub alarm_min: i32,
    pub tzid: [u8; TZID_LEN],
    pub tzid_len: usize,
    /// 被排除的重复实例起始时间（EXDATE，epoch 秒）。
    pub exdates: [i64; 16],
    pub exdates_len: usize,
    pub etag: [u8; ETAG_LEN],
    pub etag_len: usize,
    pub coll: u8,
    pub flags: u32,
    pub lrev: u32,
    pub rrev: u32,
    pub seq: u32,
}

impl Event {
    pub fn new() -> Self {
        Event {
            uid: [0; UID_LEN],
            uid_len: 0,
            summary: [0; SUMMARY_LEN],
            summary_len: 0,
            location: [0; LOCATION_LEN],
            location_len: 0,
            desc: [0; DESC_LEN],
            desc_len: 0,
            categories: [0; CATEGORIES_LEN],
            categories_len: 0,
            dtstart: DateTime::new(1970, 1, 1, 0, 0, 0, 0, true),
            dtend: DateTime::new(1970, 1, 1, 0, 0, 0, 0, true),
            all_day: false,
            rrule: Rrule::none(),
            alarm_min: ALARM_NONE,
            tzid: [0; TZID_LEN],
            tzid_len: 0,
            exdates: [0; 16],
            exdates_len: 0,
            etag: [0; ETAG_LEN],
            etag_len: 0,
            coll: 0,
            flags: 0,
            lrev: 0,
            rrev: 0,
            seq: 0,
        }
    }

    /// 事件在 [from, to]（epoch 秒）内是否有实例（含全天边界处理：
    /// 全天事件结束日按"次日晚"折算，使 [当天0点, 次日0点) 覆盖整天）。
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        let start = if self.all_day { self.dtstart.day_epoch() } else { self.dtstart.to_epoch() };
        let end = if self.all_day {
            // DTEND 为独占边界（排除结束日），转成包含式
            self.dtend.day_epoch() - 1
        } else {
            self.dtend.to_epoch()
        };
        if self.rrule.is_none() {
            return start <= to && end >= from;
        }
        // 重复事件：在该范围前的最后一次实例开始时间之后的所有实例
        let mut occs = [0i64; 64];
        let n = self.rrule.expand(start, from - 366 * 86400, to, &mut occs);
        for i in 0..n {
            if self.is_excluded(occs[i]) {
                continue;
            }
            let s = occs[i];
            let e = s + (end - start).max(0);
            if s <= to && e >= from {
                return true;
            }
        }
        false
    }

    /// 该实例起始时间是否被 EXDATE 排除。
    pub fn is_excluded(&self, start: i64) -> bool {
        for i in 0..self.exdates_len {
            if self.exdates[i] == start {
                return true;
            }
        }
        false
    }

    /// 输出该事件在 [from,to] 内所有实例的 (起始, 结束) 到 out，
    /// 返回实例数（截断）。
    pub fn instances(&self, from: i64, to: i64, out: &mut [(i64, i64); 64]) -> usize {
        let start = if self.all_day { self.dtstart.day_epoch() } else { self.dtstart.to_epoch() };
        let end = if self.all_day {
            self.dtend.day_epoch() - 1
        } else {
            self.dtend.to_epoch()
        };
        let dur = (end - start).max(0);
        if self.rrule.is_none() {
            if start <= to && end >= from {
                out[0] = (start, end);
                return 1;
            }
            return 0;
        }
        let mut occs = [0i64; 64];
        let n = self.rrule.expand(start, from - 366 * 86400, to, &mut occs);
        let mut k = 0usize;
        for i in 0..n {
            if self.is_excluded(occs[i]) {
                continue;
            }
            let s = occs[i];
            let e = s + dur;
            if s <= to && e >= from {
                if k < out.len() {
                    out[k] = (s, e);
                    k += 1;
                } else {
                    break;
                }
            }
        }
        k
    }

    pub fn is_dirty(&self) -> bool {
        self.flags & FLAG_DIRTY != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
    pub fn is_conflict(&self) -> bool {
        self.flags & FLAG_CONFLICT != 0
    }
}

// ==================== 联系人 ====================

/// 电话类型。
pub const TEL_VOICE: u8 = 0;
pub const TEL_CELL: u8 = 1;
pub const TEL_HOME: u8 = 2;
pub const TEL_WORK: u8 = 3;
pub const TEL_FAX: u8 = 4;
pub const TEL_OTHER: u8 = 5;

/// 邮箱类型。
pub const EMAIL_HOME: u8 = 0;
pub const EMAIL_WORK: u8 = 1;
pub const EMAIL_OTHER: u8 = 2;

#[derive(Clone, Copy)]
pub struct Tel {
    pub value: [u8; TEL_LEN],
    pub value_len: usize,
    pub kind: u8,
}

#[derive(Clone, Copy)]
pub struct Email {
    pub value: [u8; EMAIL_LEN],
    pub value_len: usize,
    pub kind: u8,
}

#[derive(Clone, Copy)]
pub struct Group {
    pub name: [u8; GROUP_LEN],
    pub name_len: usize,
}

#[derive(Clone, Copy)]
pub struct Contact {
    pub uid: [u8; UID_LEN],
    pub uid_len: usize,
    pub fn_: [u8; NAME_LEN],
    pub fn_len: usize,
    pub n_family: [u8; NAME_LEN],
    pub n_family_len: usize,
    pub n_given: [u8; NAME_LEN],
    pub n_given_len: usize,
    pub org: [u8; NAME_LEN],
    pub org_len: usize,
    pub title: [u8; NAME_LEN],
    pub title_len: usize,
    pub note: [u8; NOTE_LEN],
    pub note_len: usize,
    pub tels: [Tel; MAX_TELS],
    pub tels_len: usize,
    pub emails: [Email; MAX_EMAILS],
    pub emails_len: usize,
    pub groups: [Group; MAX_GROUPS],
    pub groups_len: usize,
    pub photo: [u8; PHOTO_LEN],
    pub photo_len: usize,
    pub photo_mime: [u8; MIME_LEN],
    pub photo_mime_len: usize,
    pub etag: [u8; ETAG_LEN],
    pub etag_len: usize,
    pub coll: u8,
    pub flags: u32,
    pub lrev: u32,
    pub rrev: u32,
}

impl Contact {
    pub fn new() -> Self {
        Contact {
            uid: [0; UID_LEN],
            uid_len: 0,
            fn_: [0; NAME_LEN],
            fn_len: 0,
            n_family: [0; NAME_LEN],
            n_family_len: 0,
            n_given: [0; NAME_LEN],
            n_given_len: 0,
            org: [0; NAME_LEN],
            org_len: 0,
            title: [0; NAME_LEN],
            title_len: 0,
            note: [0; NOTE_LEN],
            note_len: 0,
            tels: [Tel { value: [0; TEL_LEN], value_len: 0, kind: TEL_VOICE }; MAX_TELS],
            tels_len: 0,
            emails: [Email { value: [0; EMAIL_LEN], value_len: 0, kind: EMAIL_HOME }; MAX_EMAILS],
            emails_len: 0,
            groups: [Group { name: [0; GROUP_LEN], name_len: 0 }; MAX_GROUPS],
            groups_len: 0,
            photo: [0; PHOTO_LEN],
            photo_len: 0,
            photo_mime: [0; MIME_LEN],
            photo_mime_len: 0,
            etag: [0; ETAG_LEN],
            etag_len: 0,
            coll: 0,
            flags: 0,
            lrev: 0,
            rrev: 0,
        }
    }

    /// 显示名：FN 优先，否则 姓+名 组合。
    pub fn display_name<'a>(&'a self, buf: &'a mut [u8; NAME_LEN + 2]) -> &'a [u8] {
        if self.fn_len > 0 {
            return &self.fn_[..self.fn_len];
        }
        let mut n = 0usize;
        if self.n_family_len > 0 {
            n += crate::sync::util::copy_into(&mut buf[n..], &self.n_family[..self.n_family_len]);
        }
        if self.n_given_len > 0 {
            if n > 0 {
                n += crate::sync::util::copy_into(&mut buf[n..], b" ");
            }
            n += crate::sync::util::copy_into(&mut buf[n..], &self.n_given[..self.n_given_len]);
        }
        &buf[..n]
    }

    pub fn is_dirty(&self) -> bool {
        self.flags & FLAG_DIRTY != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
    pub fn is_conflict(&self) -> bool {
        self.flags & FLAG_CONFLICT != 0
    }
}

// ==================== 账户 / 集合 ====================

#[derive(Clone, Copy)]
pub struct Account {
    pub id: u8,
    pub name: [u8; COLL_NAME_LEN],
    pub name_len: usize,
    pub acct_type: u8,
    pub server: [u8; SERVER_LEN],
    pub server_len: usize,
    pub principal: [u8; PRINCIPAL_LEN],
    pub principal_len: usize,
    pub username: [u8; USERNAME_LEN],
    pub username_len: usize,
    /// 密钥在 keystore 中的条目名（secret_ref）。
    pub secret_ref: [u8; SECRET_NAME_LEN],
    pub secret_ref_len: usize,
    pub auth: u8,
    pub enabled: bool,
    // —— OAuth2 预留字段（初始不实现刷新流） ——
    pub token_url: [u8; SERVER_LEN],
    pub token_url_len: usize,
    pub client_id: [u8; USERNAME_LEN],
    pub client_id_len: usize,
    pub has_refresh_token: bool,
}

impl Account {
    pub fn new() -> Self {
        Account {
            id: 0,
            name: [0; COLL_NAME_LEN],
            name_len: 0,
            acct_type: ACCT_LOCAL,
            server: [0; SERVER_LEN],
            server_len: 0,
            principal: [0; PRINCIPAL_LEN],
            principal_len: 0,
            username: [0; USERNAME_LEN],
            username_len: 0,
            secret_ref: [0; SECRET_NAME_LEN],
            secret_ref_len: 0,
            auth: AUTH_BASIC,
            enabled: false,
            token_url: [0; SERVER_LEN],
            token_url_len: 0,
            client_id: [0; USERNAME_LEN],
            client_id_len: 0,
            has_refresh_token: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Collection {
    pub id: u8,
    pub acct: u8,
    pub kind: u8,
    pub name: [u8; COLL_NAME_LEN],
    pub name_len: usize,
    pub color: u32,
    /// 服务器集合 URL 路径（如 /dav/calendars/user/calendar1/）。
    pub path: [u8; COLL_PATH_LEN],
    pub path_len: usize,
    pub ctag: [u8; CTAG_LEN],
    pub ctag_len: usize,
    /// RFC 6578 sync-token；空表示尚未建立增量同步。
    pub sync_token: [u8; TOKEN_LEN],
    pub sync_token_len: usize,
    pub last_sync: u64,
    /// 上次同步失败（离线标记），恢复网络后自动重试。
    pub needs_sync: bool,
    /// 本地存在未推送改动（快速判空）。
    pub dirty: bool,
}

impl Collection {
    pub fn new() -> Self {
        Collection {
            id: 0,
            acct: 0,
            kind: COLL_CALENDAR,
            name: [0; COLL_NAME_LEN],
            name_len: 0,
            color: 0x0022C55E,
            path: [0; COLL_PATH_LEN],
            path_len: 0,
            ctag: [0; CTAG_LEN],
            ctag_len: 0,
            sync_token: [0; TOKEN_LEN],
            sync_token_len: 0,
            last_sync: 0,
            needs_sync: false,
            dirty: false,
        }
    }
}

// ==================== 同步结果统计 ====================

/// 一次同步会话的结果统计（供 UI 显示）。
#[derive(Clone, Copy)]
pub struct SyncStats {
    pub pushed: u32,
    pub pulled: u32,
    pub deleted: u32,
    pub conflicts: u32,
    pub errors: u32,
    pub ok: bool,
    pub msg: [u8; 96],
    pub msg_len: usize,
}

impl SyncStats {
    pub fn new() -> Self {
        SyncStats { pushed: 0, pulled: 0, deleted: 0, conflicts: 0, errors: 0, ok: true, msg: [0; 96], msg_len: 0 }
    }
    pub fn set_msg(&mut self, s: &[u8]) {
        self.msg_len = crate::sync::util::copy_into(&mut self.msg, s);
    }
    pub fn fail(&mut self, s: &[u8]) {
        self.ok = false;
        self.errors += 1;
        self.set_msg(s);
    }
}

// ==================== 冲突解决策略 ====================

pub const RESOLVE_SERVER: u8 = 0; // 服务器优先：丢弃本地改动
pub const RESOLVE_CLIENT: u8 = 1; // 客户端优先：用本地覆盖服务器
pub const RESOLVE_MANUAL: u8 = 2; // 手动：标记冲突，保留双方（由 UI 决策）

pub fn conflict_strategy_name(s: u8) -> &'static [u8] {
    match s {
        RESOLVE_SERVER => b"server",
        RESOLVE_CLIENT => b"client",
        _ => b"manual",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_roundtrip() {
        let t = DateTime::new(2025, 6, 15, 10, 30, 0, 480, true);
        // UTC+8 的 10:30 = UTC 02:30
        assert_eq!(t.to_epoch(), 1749954600);
        // from_epoch 是 UTC 视图（2:30）
        let back = DateTime::from_epoch(t.to_epoch());
        assert_eq!((back.y, back.mo, back.d, back.h, back.mi), (2025, 6, 15, 2, 30));
        // 无时区（浮动）：按墙钟直通
        let f = DateTime::new(2025, 6, 15, 10, 30, 0, 0, false);
        assert_eq!(f.to_epoch(), 1749983400);
    }

    #[test]
    fn rrule_daily() {
        let r = Rrule { freq: FREQ_DAILY, interval: 1, count: 5, until: 0, has_count: true, has_until: false, byday: 0 };
        let start = DateTime::new(2025, 1, 1, 9, 0, 0, 0, true).to_epoch();
        assert_eq!(r.next_after(start, start), Some(start + 86400));
        assert_eq!(r.next_after(start, start + 3 * 86400), Some(start + 4 * 86400));
        assert_eq!(r.next_after(start, start + 4 * 86400), None); // count 用尽
        let mut out = [0i64; 16];
        let n = r.expand(start, start, start + 10 * 86400, &mut out);
        assert_eq!(n, 5);
    }

    #[test]
    fn rrule_weekly_byday() {
        // 每周一/三，共 3 次
        let r = Rrule {
            freq: FREQ_WEEKLY,
            interval: 1,
            count: 3,
            until: 0,
            has_count: true,
            has_until: false,
            byday: (1 << 1) | (1 << 3), // 周一、周三
        };
        // 2025-06-09 是周一
        let start = DateTime::new(2025, 6, 9, 10, 0, 0, 0, true).to_epoch();
        let mut out = [0i64; 16];
        let n = r.expand(start, start, start + 20 * 86400, &mut out);
        assert_eq!(n, 3);
        let mut days = [0i64; 3];
        for i in 0..n {
            days[i] = out[i] / 86400;
        }
        assert_eq!(days, [days[0], days[0] + 2, days[0] + 7]);
        assert_eq!(crate::sync::util::weekday_from_days(days[0]), 1);
    }

    #[test]
    fn rrule_monthly() {
        let r = Rrule { freq: FREQ_MONTHLY, interval: 1, count: 0, until: 0, has_count: false, has_until: false, byday: 0 };
        let start = DateTime::new(2025, 1, 31, 8, 0, 0, 0, true).to_epoch();
        let mut out = [0i64; 4];
        let n = r.expand(start, start, start + 90 * 86400, &mut out);
        assert_eq!(n, 4); // 1/31, 2/28, 3/31, 4/30
        let mut months = [0i64; 4];
        for i in 0..n {
            months[i] = crate::sync::util::civil_from_epoch(out[i]).0 as i64 * 100 + crate::sync::util::civil_from_epoch(out[i]).1 as i64;
        }
        assert_eq!(months, [202501, 202502, 202503, 202504]);
    }

    #[test]
    fn event_overlaps_all_day() {
        let mut e = Event::new();
        e.all_day = true;
        e.dtstart = DateTime::new(2025, 3, 10, 0, 0, 0, 0, false);
        e.dtend = DateTime::new(2025, 3, 11, 0, 0, 0, 0, false);
        // 3/10 全天：与 [3/10 00:00, 3/10 23:59] 相交
        assert!(e.overlaps(crate::sync::util::epoch_from_civil(2025, 3, 10, 0, 0, 0), crate::sync::util::epoch_from_civil(2025, 3, 10, 12, 0, 0)));
        assert!(!e.overlaps(crate::sync::util::epoch_from_civil(2025, 3, 9, 0, 0, 0), crate::sync::util::epoch_from_civil(2025, 3, 9, 23, 0, 0)));
    }

    #[test]
    fn contact_display_name() {
        let mut c = Contact::new();
        c.fn_len = crate::sync::util::copy_into(&mut c.fn_, b"Alice");
        let mut buf = [0u8; NAME_LEN + 2];
        assert_eq!(c.display_name(&mut buf), b"Alice");
        c.fn_len = 0;
        c.n_family_len = crate::sync::util::copy_into(&mut c.n_family, b"Zhang");
        c.n_given_len = crate::sync::util::copy_into(&mut c.n_given, b"San");
        assert_eq!(c.display_name(&mut buf), b"Zhang San");
    }
}
