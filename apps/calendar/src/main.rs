// KunxOS 日历应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/calendar）。职责：
//   1. 月视图：星期表头 + 当月日期网格，今天高亮，有事件的日子显示圆点
//   2. 本地存储：事件以 iCalendar 格式存入 /data/sync/events.idx/.blob
//      （同步栈 shared/sync，见 docs/sync.md）
//   3. 日视图：点日期查看当天事件列表
//   4. 新建事件：标题 / 日期 / 时间 表单（键盘输入）
//   5. 同步：配置 CalDAV 账户（服务器 / 用户名 / 密码）→ 发现日历集合 →
//      推送本地改动 + 拉取远端（RFC 4791 / RFC 6578，冲突手动解决）
//   6. 离线：同步失败保留本地队列，应用启动或手动同步时自动重试

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_calendar.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

#[path = "../../../shared/sync/mod.rs"]
mod sync;

#[path = "../../../shared/sync/app_platform.rs"]
mod app_platform;

use app_platform::{SysFs, SysNet};
use kit::*;
use sync::accounts::MetaStore;
use sync::model::*;
use sync::store::Store;
use sync::sync::SyncEnv;
use sync::model::SyncStats;

// ==================== 布局与配色 ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_CELL: u32 = 0x00_FF_FF_FF;
const C_CELL_BORDER: u32 = 0x00_D9_DE_E6;
const C_TODAY: u32 = 0x00_00_B9_6B;
/// 今天格子用强调绿玻璃（彩色表面），日期文字保持白色。
const C_TODAY_TEXT: u32 = 0x00_FF_FF_FF;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_WEEKEND: u32 = 0x00_E7_4C_3C;
const C_DOT: u32 = 0x00_4F_A8_FF;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_WARN: u32 = 0x00_E2_96_06;

const GRID_X: i32 = 12;
const GRID_Y: i32 = 210;
const CELL_W: i32 = 72;
const CELL_H: i32 = 72;
const CELL_GAP: i32 = 6;

static WEEKDAY_NAMES: [&[u8]; 7] = [
    "日".as_bytes(), "一".as_bytes(), "二".as_bytes(), "三".as_bytes(),
    "四".as_bytes(), "五".as_bytes(), "六".as_bytes(),
];

static MONTH_NAMES: [&[u8]; 12] = [
    "一月".as_bytes(), "二月".as_bytes(), "三月".as_bytes(), "四月".as_bytes(),
    "五月".as_bytes(), "六月".as_bytes(), "七月".as_bytes(), "八月".as_bytes(),
    "九月".as_bytes(), "十月".as_bytes(), "十一月".as_bytes(), "十二月".as_bytes(),
];

/// 设备密钥（占位级：KeyStore 预留，见 docs/sync.md「安全」）。
const DEV_KEY: u32 = 0x4B58_5359;

// ==================== 全局状态 ====================

const PAGE_MONTH: u8 = 0;
const PAGE_DAY: u8 = 1;
const PAGE_NEW: u8 = 2;
const PAGE_SYNC: u8 = 3;

static mut PAGE: u8 = PAGE_MONTH;
static mut CUR_Y: i32 = 2025;
static mut CUR_M: u32 = 1;
static mut SEL_D: u32 = 1;
static mut TODAY_Y: i32 = 2025;
static mut TODAY_M: u32 = 1;
static mut TODAY_D: u32 = 1;

/// 事件存储 Blob 缓冲。
static mut EV_BLOB: [u8; 262144] = [0u8; 262144];
/// 同步缓冲。
static mut RESP_BUF: [u8; 65536] = [0u8; 65536];
static mut REQ_BUF: [u8; 8192] = [0u8; 8192];
static mut XML_BUF: [u8; 65536] = [0u8; 65536];
/// 表单缓冲。
static mut F_TITLE: [u8; 64] = [0u8; 64];
static mut F_TITLE_LEN: usize = 0;
static mut F_DATE: [u8; 16] = [0u8; 16];
static mut F_DATE_LEN: usize = 0;
static mut F_TIME: [u8; 16] = [0u8; 16];
static mut F_TIME_LEN: usize = 0;
static mut F_FOCUS: u8 = 0;
/// 同步面板表单。
static mut F_SERVER: [u8; 160] = [0u8; 160];
static mut F_SERVER_LEN: usize = 0;
static mut F_USER: [u8; 64] = [0u8; 64];
static mut F_USER_LEN: usize = 0;
static mut F_PASS: [u8; 64] = [0u8; 64];
static mut F_PASS_LEN: usize = 0;
static mut SYNC_MSG: [u8; 96] = [0u8; 96];
static mut SYNC_MSG_LEN: usize = 0;
static mut SYNC_BUSY: bool = false;

static mut SYS_FS: SysFs = SysFs;
static mut SYS_NET: SysNet = SysNet;

// ==================== 存储访问 ====================

fn store<'a>(blob: &'a mut [u8]) -> Store<'a> {
    let mut s = Store::new(blob, b"/data/sync/events.idx", b"/data/sync/events.blob");
    unsafe { s.load(&mut SYS_FS); }
    s
}

fn meta() -> MetaStore {
    let mut m = MetaStore::new(b"/data/sync/meta.d");
    unsafe { m.load(&mut SYS_FS); }
    m
}

/// 当前月份有事件的日期掩码（1..=31）。
fn month_dots(y: i32, m: u32, dots: &mut [bool; 32]) {
    for d in dots.iter_mut() {
        *d = false;
    }
    let from = sync::util::epoch_from_civil(y, m, 1, 0, 0, 0);
    let to = sync::util::epoch_from_civil(y, m, sync::util::days_in_month(y, m), 23, 59, 59);
    unsafe {
        let s = store(&mut EV_BLOB);
        let mut recs = [0usize; MAX_RECORDS];
        let n = s.query_coll(1, &mut recs);
        let mut ev = Event::new();
        for k in 0..n {
            let i = recs[k];
            let mut buf = [0u8; 4096];
            if let Some(blob) = s.blob_of(i) {
                let bl = sync::util::copy_into(&mut buf, blob);
                if sync::ical::parse_event(&mut buf, bl, &mut ev) {
                    let mut occs = [(0i64, 0i64); 64];
                    let o = ev.instances(from, to, &mut occs);
                    for j in 0..o {
                        let (yy, mm, dd, _, _, _) = sync::util::civil_from_epoch(occs[j].0);
                        if yy == y && mm == m && (dd as usize) < dots.len() {
                            dots[dd as usize] = true;
                        }
                    }
                }
            }
        }
    }
}

/// 某天的事件列表（summary + 实例起止），返回数量。
fn day_events(y: i32, m: u32, d: u32, out_sum: &mut [[u8; 160]], out_starts: &mut [i64], out_ends: &mut [i64]) -> usize {
    let from = sync::util::epoch_from_civil(y, m, d, 0, 0, 0);
    let to = from + 86399;
    let mut n = 0usize;
    unsafe {
        let s = store(&mut EV_BLOB);
        let mut recs = [0usize; MAX_RECORDS];
        let nr = s.query_coll(1, &mut recs);
        let mut ev = Event::new();
        for k in 0..nr {
            let i = recs[k];
            let mut buf = [0u8; 4096];
            if let Some(blob) = s.blob_of(i) {
                let bl = sync::util::copy_into(&mut buf, blob);
                if sync::ical::parse_event(&mut buf, bl, &mut ev) {
                    let mut occs = [(0i64, 0i64); 64];
                    let o = ev.instances(from, to, &mut occs);
                    for j in 0..o {
                        if n < out_sum.len() {
                            out_sum[n] = [0u8; 160];
                            sync::util::copy_into(&mut out_sum[n], &ev.summary[..ev.summary_len]);
                            out_starts[n] = occs[j].0;
                            out_ends[n] = occs[j].1;
                            n += 1;
                        }
                    }
                }
            }
        }
    }
    n
}

// ==================== 渲染 ====================

fn render_month() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("日历".as_bytes());

    let cy = unsafe { CUR_Y };
    let cm = unsafe { CUR_M };
    let mut title = [0u8; 16];
    let mut n = 0;
    n = push_num(&mut title, n, cy as u32);
    title[n] = b' ';
    n += 1;
    let mname = MONTH_NAMES[(cm - 1) as usize];
    title[n..n + mname.len()].copy_from_slice(mname);
    n += mname.len();
    draw_text(16, 70, &title[..n], C_TEXT);

    // 操作按钮
    draw_glass_button(16, 110, 84, 36, "新建".as_bytes(), true);
    draw_glass_button(104, 110, 84, 36, "同步".as_bytes(), false);

    // 星期表头
    draw_glass(GRID_X, GRID_Y - 46, 7 * (CELL_W + CELL_GAP) - CELL_GAP, 28, 8);
    for (i, w) in WEEKDAY_NAMES.iter().enumerate() {
        let x = GRID_X + i as i32 * (CELL_W + CELL_GAP);
        let color = if i == 0 || i == 6 { C_WEEKEND } else { C_DIM };
        draw_text(x + (CELL_W - text_width(w)) / 2, GRID_Y - 30, w, color);
    }

    // 当月事件点
    let mut dots = [false; 32];
    month_dots(cy, cm, &mut dots);

    let first_dow = weekday_from_days(days_from_civil(cy, cm, 1));
    let dim = sync::util::days_in_month(cy, cm);
    let today = unsafe { (TODAY_Y, TODAY_M, TODAY_D) };
    for d in 1..=dim {
        let idx = (first_dow as i32) + d as i32 - 1;
        let r = idx / 7;
        let c = idx % 7;
        let x = GRID_X + c * (CELL_W + CELL_GAP);
        let y = GRID_Y + r * (CELL_H + CELL_GAP);
        if y + CELL_H > DISPLAY_H as i32 {
            break;
        }
        let is_today = cy == today.0 && cm == today.1 && d == today.2 as u32;
        let cell_r = (CELL_H * 25 / 100).min(18);
        if is_today {
            draw_glass_style(x, y, CELL_W, CELL_H, cell_r, &STYLE_ACCENT);
        } else {
            draw_glass(x, y, CELL_W, CELL_H, cell_r);
        }
        let mut dbuf = [0u8; 4];
        let dn = push_num(&mut dbuf, 0, d);
        let tx = x + 8;
        let ty = y + 10;
        draw_text(tx, ty, &dbuf[..dn], if is_today { C_TODAY_TEXT } else { C_TEXT });
        if dots[d as usize] {
            draw_disc(x + CELL_W - 14, y + CELL_H - 14, 5, C_DOT);
        }
    }

    // 同步状态提示
    unsafe {
        if SYNC_MSG_LEN > 0 {
            draw_text(16, DISPLAY_H as i32 - 100, &SYNC_MSG[..SYNC_MSG_LEN], C_DIM);
        }
        if SYNC_BUSY {
            draw_text(16, DISPLAY_H as i32 - 76, "同步中...".as_bytes(), C_WARN);
        } else {
            draw_text(16, DISPLAY_H as i32 - 76, "点日期看详情 | 新建 | 同步".as_bytes(), C_DIM);
        }
    }
    draw_sys_nav();
}

fn render_day() {
    draw_gradient_bg(C_BG, glass_dark());
    let (yy, mm, dd) = unsafe { (CUR_Y, CUR_M, SEL_D) };
    let mut title = [0u8; 24];
    let mut n = 0;
    n = push_num(&mut title, n, yy as u32);
    title[n] = b'-';
    n += 1;
    n = push_num(&mut title, n, mm as u32);
    title[n] = b'-';
    n += 1;
    n = push_num(&mut title, n, dd as u32);
    title[n] = b' ';
    n += 1;
    let wd = WEEKDAY_NAMES[weekday_from_days(days_from_civil(yy, mm, dd)) as usize];
    title[n..n + wd.len()].copy_from_slice(wd);
    n += wd.len();
    draw_title_bar(&title[..n]);

    let mut sums = [[0u8; 160]; 12];
    let mut starts = [0i64; 12];
    let mut ends = [0i64; 12];
    let cnt = day_events(yy, mm, dd, &mut sums, &mut starts, &mut ends);
    if cnt == 0 {
        draw_text(60, 300, "这一天没有事件".as_bytes(), C_DIM);
    }
    for i in 0..cnt.min(12) {
        let y = 90 + i as i32 * 62;
        draw_glass(12, y, DISPLAY_W as i32 - 24, 54, 14);
        draw_text(24, y + 8, &sums[i], C_TEXT);
        let mut tbuf = [0u8; 16];
        let mut tn = 0;
        let (_, _, _, h1, m1, _) = sync::util::civil_from_epoch(starts[i]);
        tn = push_num(&mut tbuf, tn, h1 as u32);
        tbuf[tn] = b':';
        tn += 1;
        tn = push_num(&mut tbuf, tn, m1 as u32);
        let (_, _, _, h2, m2, _) = sync::util::civil_from_epoch(ends[i]);
        if ends[i] > starts[i] {
            tbuf[tn] = b'-';
            tn += 1;
            tn = push_num(&mut tbuf, tn, h2 as u32);
            tbuf[tn] = b':';
            tn += 1;
            tn = push_num(&mut tbuf, tn, m2 as u32);
        }
        draw_text(24, y + 30, &tbuf[..tn], C_DIM);
    }
    draw_sys_nav();
}

/// 输入框。
fn field(x: i32, y: i32, w: i32, label: &[u8], value: &[u8], focus: bool) {
    draw_text(x, y, label, C_DIM);
    draw_glass(x, y + 26, w, 40, 10);
    if focus {
        draw_rect(x - 1, y + 25, w + 2, 42, C_ACCENT);
    }
    draw_text(x + 10, y + 34, value, C_TEXT);
}

fn render_new() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("新建事件".as_bytes());
    unsafe {
        let f = F_FOCUS;
        field(24, 120, 480, "标题".as_bytes(), &F_TITLE[..F_TITLE_LEN], f == 0);
        field(24, 210, 480, "日期 (YYYY-MM-DD)".as_bytes(), &F_DATE[..F_DATE_LEN], f == 1);
        field(24, 300, 480, "时间 (HH:MM, 留空=全天)".as_bytes(), &F_TIME[..F_TIME_LEN], f == 2);
    }
    draw_glass_button(24, 420, 200, 48, "保存".as_bytes(), true);
    draw_glass_button(320, 420, 200, 48, "取消".as_bytes(), false);
    draw_text(24, 500, "Enter 切换输入框；保存后写入本地并待同步".as_bytes(), C_DIM);
    draw_sys_nav();
}

fn render_sync() {
    draw_gradient_bg(C_BG, glass_dark());
    draw_title_bar("同步设置".as_bytes());
    unsafe {
        let f = F_FOCUS;
        field(24, 110, 480, "服务器 (CalDAV 根路径)".as_bytes(), &F_SERVER[..F_SERVER_LEN], f == 0);
        field(24, 200, 480, "用户名".as_bytes(), &F_USER[..F_USER_LEN], f == 1);
        field(24, 290, 480, "密码".as_bytes(), &F_PASS[..F_PASS_LEN], f == 2);
    }
    draw_glass_button(24, 380, 150, 44, "保存账户".as_bytes(), false);
    draw_glass_button(190, 380, 150, 44, "立即同步".as_bytes(), true);
    draw_glass_button(356, 380, 160, 44, "返回".as_bytes(), false);
    if unsafe { SYNC_MSG_LEN > 0 } {
        draw_text(24, 460, unsafe { &SYNC_MSG[..SYNC_MSG_LEN] }, C_WARN);
    }
    draw_text(24, 520, "示例：https://apidata.googleusercontent.com/caldav/v2/".as_bytes(), C_DIM);
    draw_text(24, 548, "或自建 Radicale/Nextcloud 的 DAV 根路径".as_bytes(), C_DIM);
    draw_sys_nav();
}

// ==================== 业务逻辑 ====================

fn btn_hit(x: i32, y: i32, bx: i32, by: i32, bw: i32, bh: i32) -> bool {
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

fn push_num(buf: &mut [u8], mut n: usize, mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if v == 0 {
        tmp[i - 1] = b'0';
        i -= 1;
    } else {
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    while i < tmp.len() {
        buf[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

/// 解析 "YYYY-MM-DD" → (y, m, d)；失败返回 None。
fn parse_date(s: &[u8]) -> Option<(i32, u32, u32)> {
    if s.len() != 10 || s[4] != b'-' || s[7] != b'-' {
        return None;
    }
    let y = sync::util::parse_i64(&s[0..4])? as i32;
    let mo = sync::util::parse_u32(&s[5..7])?;
    let d = sync::util::parse_u32(&s[8..10])?;
    if mo < 1 || mo > 12 || d < 1 || d > sync::util::days_in_month(y, mo) {
        return None;
    }
    Some((y, mo, d))
}

/// 解析 "HH:MM" → (h, m)；失败返回 None。
fn parse_time(s: &[u8]) -> Option<(u32, u32)> {
    if s.len() != 5 || s[2] != b':' {
        return None;
    }
    let h = sync::util::parse_u32(&s[0..2])?;
    let m = sync::util::parse_u32(&s[3..5])?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

fn save_new_event() {
    unsafe {
        // 标题必填；日期缺省为当前选中日
        let title = &F_TITLE[..F_TITLE_LEN];
        let date = if F_DATE_LEN > 0 { &F_DATE[..F_DATE_LEN] } else { b"" };
        let (y, mo, d) = if date.is_empty() {
            (CUR_Y, CUR_M, SEL_D)
        } else {
            match parse_date(date) {
                Some(v) => v,
                None => {
                    SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "日期格式错误".as_bytes());
                    return;
                }
            }
        };
        let (h, mi, all_day) = if F_TIME_LEN > 0 {
            match parse_time(&F_TIME[..F_TIME_LEN]) {
                Some((h, m)) => (h, m, false),
                None => {
                    SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "时间格式错误".as_bytes());
                    return;
                }
            }
        } else {
            (9, 0, true)
        };
        let mut ev = Event::new();
        ev.summary_len = sync::util::copy_into(&mut ev.summary, title);
        ev.dtstart = DateTime::new(y, mo, d, h, mi, 0, 0, !all_day);
        if all_day {
            ev.all_day = true;
            ev.dtend = DateTime::from_epoch(sync::util::epoch_from_civil(y, mo, d, 0, 0, 0) + 86400);
        } else {
            ev.dtend = DateTime::new(y, mo, d, (h + 1) % 24, mi, 0, 0, true);
        }
        // 写入本地存储（含生成 iCalendar Blob，置脏待同步）
        let mut env = SyncEnv {
            fs: &mut SYS_FS,
            net: &mut SYS_NET,
            dev_key: DEV_KEY,
            now_us: now_us(),
            resolve: RESOLVE_MANUAL,
        };
        let mut blob = [0u8; 4096];
        let mut s = store(&mut blob);
        let mut pbuf = [0u8; 192];
        let mut pl = sync::util::copy_into(&mut pbuf, b"/");
        if let Some(c) = meta().collection(1) {
            pl = sync::util::copy_into(&mut pbuf, &c.path[..c.path_len]);
        }
        let ok = sync::sync::create_event_local(&mut env, &mut s, 1, &pbuf[..pl], &mut ev, 1);
        if ok {
            let _ = s.save(&mut SYS_FS);
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "已保存，待同步".as_bytes());
        } else {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "保存失败（存储已满？）".as_bytes());
        }
        F_TITLE_LEN = 0;
        F_DATE_LEN = 0;
        F_TIME_LEN = 0;
        F_FOCUS = 0;
    }
}

fn load_account_into_form() {
    unsafe {
        let m = meta();
        if let Some(a) = m.account(1) {
            F_SERVER_LEN = sync::util::copy_into(&mut F_SERVER, &a.server[..a.server_len]);
            F_USER_LEN = sync::util::copy_into(&mut F_USER, &a.username[..a.username_len]);
        }
        F_PASS_LEN = 0;
    }
}

fn save_account() {
    unsafe {
        let mut m = meta();
        let mut a = Account::new();
        a.id = 1;
        a.name_len = sync::util::copy_into(&mut a.name, b"CalDAV");
        a.acct_type = ACCT_GENERIC;
        a.server_len = sync::util::copy_into(&mut a.server, &F_SERVER[..F_SERVER_LEN]);
        a.username_len = sync::util::copy_into(&mut a.username, &F_USER[..F_USER_LEN]);
        a.auth = AUTH_BASIC;
        a.enabled = true;
        m.upsert_account(a);
        // 密码入 KeyStore（异或混淆，占位级）
        if F_PASS_LEN > 0 {
            let mut sn = [0u8; 16];
            let snl = MetaStore::account_secret_name(1, &mut sn);
            m.secret_set(&sn[..snl], &F_PASS[..F_PASS_LEN], DEV_KEY);
        }
        m.save(&mut SYS_FS);
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "账户已保存".as_bytes());
    }
}

/// 发现并保存第一个日历集合（若尚无）。
fn ensure_collection(dav: &mut sync::dav::DavClient<'_>, m: &mut MetaStore) -> bool {
    if m.collection(1).is_some() {
        return true;
    }
    unsafe {
        let mut colls = [Collection::new(); 8];
        let n = match sync::caldav::discover_calendars(dav, b"/", &mut colls, &mut RESP_BUF, &mut REQ_BUF, &mut XML_BUF) {
            Ok(n) => n,
            Err(_) => 0,
        };
        if n == 0 {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "未发现日历集合".as_bytes());
            return false;
        }
        let mut c = colls[0];
        c.id = 1;
        c.acct = 1;
        c.kind = COLL_CALENDAR;
        m.upsert_collection(c);
        m.save(&mut SYS_FS);
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "已发现日历集合".as_bytes());
        true
    }
}

fn do_sync() {
    unsafe {
        SYNC_BUSY = true;
        let mut m = meta();
        let acct_ok = m.account(1).map(|a| a.enabled && a.server_len > 0).unwrap_or(false);
        if !acct_ok {
            SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "请先配置并保存账户".as_bytes());
            SYNC_BUSY = false;
            return;
        }
        let mut env = SyncEnv {
            fs: &mut SYS_FS,
            net: &mut SYS_NET,
            dev_key: DEV_KEY,
            now_us: now_us(),
            resolve: RESOLVE_MANUAL,
        };
        // 认证头
        let mut auth = [0u8; 260];
        let auth_len = {
            let a = m.account(1).unwrap();
            let mut pw = [0u8; SECRET_LEN];
            let mut sn = [0u8; 16];
            let snl = MetaStore::account_secret_name(1, &mut sn);
            let pl = m.secret_get(&sn[..snl], DEV_KEY, &mut pw);
            sync::http::basic_auth(&a.username[..a.username_len], &pw[..pl], &mut auth)
        };
        let mut srv = [0u8; SERVER_LEN];
        {
            let a = m.account(1).unwrap();
            let sl = sync::util::copy_into(&mut srv, &a.server[..a.server_len]);
            let _ = sl;
        }
        let sl = {
            let a = m.account(1).unwrap();
            a.server_len
        };
        let mut dav = sync::dav::DavClient::new(&mut SYS_NET, &srv[..sl], &auth[..auth_len]);
        if !ensure_collection(&mut dav, &mut m) {
            SYNC_BUSY = false;
            return;
        }
        // blob 复用静态 EV_BLOB：避免 256KB 栈帧（用户栈 1MB，do_sync 深调用链会溢出）
        let mut s = store(&mut *core::ptr::addr_of_mut!(EV_BLOB));
        let mut stats = SyncStats::new();
        let r = sync::sync::sync_collection(&mut env, &mut m, 1, &mut s, COLL_CALENDAR, &mut RESP_BUF, &mut REQ_BUF, &mut XML_BUF, &mut stats);
        match r {
            Ok(()) => {
                let mut msg = [0u8; 96];
                let mut n = sync::util::copy_into(&mut msg, "同步完成 推".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.pushed as u64);
                n = sync::util::push_str(&mut msg, n, " 拉".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.pulled as u64);
                n = sync::util::push_str(&mut msg, n, " 冲突".as_bytes());
                n = sync::util::push_dec(&mut msg, n, stats.conflicts as u64);
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, &msg[..n]);
            }
            Err(e) => {
                let mut msg = [0u8; 96];
                let mut n = sync::util::copy_into(&mut msg, "同步失败: ".as_bytes());
                n = sync::util::push_str(&mut msg, n, e.label());
                if stats.msg_len > 0 {
                    n = sync::util::push_str(&mut msg, n, b" ");
                    n = sync::util::push_str(&mut msg, n, &stats.msg[..stats.msg_len]);
                }
                SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, &msg[..n]);
            }
        }
        SYNC_BUSY = false;
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[calendar] 启动中\n");
    if !fb_init() {
        print("[calendar] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    // 初始化今天
    let (y, m, d) = now_date();
    if now_us() != 0 {
        unsafe {
            CUR_Y = y;
            CUR_M = m;
            TODAY_Y = y;
            TODAY_M = m;
            TODAY_D = d;
        }
    }
    // 首次运行播种示例事件（本地存储为空时）
    unsafe {
        // blob 复用静态 EV_BLOB：避免 256KB 栈帧（用户栈 1MB，do_sync 深调用链会溢出）
        let mut s = store(&mut *core::ptr::addr_of_mut!(EV_BLOB));
        if s.n == 0 {
            let mut env = SyncEnv {
                fs: &mut SYS_FS,
                net: &mut SYS_NET,
                dev_key: DEV_KEY,
                now_us: now_us(),
                resolve: RESOLVE_MANUAL,
            };
            let mut ev = Event::new();
            ev.summary_len = sync::util::copy_into(&mut ev.summary, "项目评审".as_bytes());
            ev.dtstart = DateTime::new(CUR_Y, CUR_M, TODAY_D.min(28), 10, 0, 0, 0, true);
            ev.dtend = DateTime::new(CUR_Y, CUR_M, TODAY_D.min(28), 11, 0, 0, 0, true);
            sync::sync::create_event_local(&mut env, &mut s, 1, b"/", &mut ev, 1);
            let mut ev2 = Event::new();
            ev2.summary_len = sync::util::copy_into(&mut ev2.summary, "团队例会".as_bytes());
            ev2.dtstart = DateTime::new(CUR_Y, CUR_M, TODAY_D.min(28), 15, 0, 0, 0, true);
            ev2.dtend = DateTime::new(CUR_Y, CUR_M, TODAY_D.min(28), 16, 0, 0, 0, true);
            ev2.rrule = Rrule { freq: FREQ_WEEKLY, interval: 1, count: 0, until: 0, has_count: false, has_until: false, byday: 1 << 1 };
            sync::sync::create_event_local(&mut env, &mut s, 1, b"/", &mut ev2, 2);
            let _ = s.save(&mut SYS_FS);
            print("[calendar] 已写入示例事件\n");
        }
        SYNC_MSG_LEN = sync::util::copy_into(&mut SYNC_MSG, "本地存储就绪".as_bytes());
    }
    print("[calendar] 就绪\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            let page = unsafe { PAGE };
            match page {
                PAGE_MONTH => {
                    if back_hit(x, y) {
                        break;
                    }
                    if btn_hit(x, y, 16, 110, 84, 36) {
                        unsafe {
                            PAGE = PAGE_NEW;
                            F_FOCUS = 0;
                            F_TITLE_LEN = 0;
                            F_DATE_LEN = 0;
                            F_TIME_LEN = 0;
                        }
                    } else if btn_hit(x, y, 104, 110, 84, 36) {
                        unsafe {
                            load_account_into_form();
                            PAGE = PAGE_SYNC;
                            F_FOCUS = 0;
                        }
                    } else {
                        // 点击日期格子
                        let first_dow = weekday_from_days(days_from_civil(unsafe { CUR_Y }, unsafe { CUR_M }, 1));
                        let cx = x - GRID_X;
                        let cy = y - GRID_Y;
                        if cx >= 0 && cy >= 0 {
                            let c = cx / (CELL_W + CELL_GAP);
                            let r = cy / (CELL_H + CELL_GAP);
                            if c < 7 && r < 6 {
                                let day = (r * 7 + c) as i32 - first_dow as i32 + 1;
                                if day >= 1 && day <= sync::util::days_in_month(unsafe { CUR_Y }, unsafe { CUR_M }) as i32 {
                                    unsafe {
                                        SEL_D = day as u32;
                                        PAGE = PAGE_DAY;
                                    }
                                }
                            }
                        }
                    }
                }
                PAGE_DAY => {
                    if back_hit(x, y) {
                        unsafe { PAGE = PAGE_MONTH; }
                    }
                }
                PAGE_NEW => {
                    if btn_hit(x, y, 24, 420, 200, 48) {
                        save_new_event();
                        unsafe { PAGE = PAGE_MONTH; }
                    } else if btn_hit(x, y, 320, 420, 200, 48) || back_hit(x, y) {
                        unsafe { PAGE = PAGE_MONTH; }
                    } else if y >= 120 && y < 260 {
                        unsafe { F_FOCUS = ((y - 120) / 90) as u8; }
                    }
                }
                PAGE_SYNC => {
                    if btn_hit(x, y, 24, 380, 150, 44) {
                        save_account();
                    } else if btn_hit(x, y, 190, 380, 150, 44) {
                        save_account();
                        do_sync();
                    } else if btn_hit(x, y, 356, 380, 160, 44) || back_hit(x, y) {
                        unsafe { PAGE = PAGE_MONTH; }
                    } else if y >= 110 && y < 260 {
                        unsafe { F_FOCUS = ((y - 110) / 90) as u8; }
                    }
                }
                _ => {}
            }
            dirty = true;
        }
        // 键盘输入
        match ev.key {
            KeyAction::Char(c) => {
                let page = unsafe { PAGE };
                if page == PAGE_NEW || page == PAGE_SYNC {
                    let focus = unsafe { F_FOCUS };
                    unsafe {
                        let (buf, len): (&mut [u8], &mut usize) = match (page, focus) {
                            (PAGE_NEW, 0) => (&mut F_TITLE, &mut F_TITLE_LEN),
                            (PAGE_NEW, 1) => (&mut F_DATE, &mut F_DATE_LEN),
                            (PAGE_NEW, 2) => (&mut F_TIME, &mut F_TIME_LEN),
                            (PAGE_SYNC, 0) => (&mut F_SERVER, &mut F_SERVER_LEN),
                            (PAGE_SYNC, 1) => (&mut F_USER, &mut F_USER_LEN),
                            (PAGE_SYNC, 2) => (&mut F_PASS, &mut F_PASS_LEN),
                            _ => continue,
                        };
                        if *len < buf.len() {
                            buf[*len] = c;
                            *len += 1;
                        }
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                let page = unsafe { PAGE };
                if page == PAGE_NEW || page == PAGE_SYNC {
                    let focus = unsafe { F_FOCUS };
                    unsafe {
                        let len: &mut usize = match (page, focus) {
                            (PAGE_NEW, 0) => &mut F_TITLE_LEN,
                            (PAGE_NEW, 1) => &mut F_DATE_LEN,
                            (PAGE_NEW, 2) => &mut F_TIME_LEN,
                            (PAGE_SYNC, 0) => &mut F_SERVER_LEN,
                            (PAGE_SYNC, 1) => &mut F_USER_LEN,
                            (PAGE_SYNC, 2) => &mut F_PASS_LEN,
                            _ => continue,
                        };
                        if *len > 0 {
                            *len -= 1;
                        }
                    }
                    dirty = true;
                }
            }
            KeyAction::Enter => {
                let page = unsafe { PAGE };
                if page == PAGE_NEW {
                    unsafe {
                        if F_FOCUS < 2 {
                            F_FOCUS += 1;
                        } else {
                            save_new_event();
                            PAGE = PAGE_MONTH;
                        }
                    }
                    dirty = true;
                } else if page == PAGE_SYNC {
                    unsafe {
                        if F_FOCUS < 2 {
                            F_FOCUS += 1;
                        } else {
                            save_account();
                            do_sync();
                        }
                    }
                    dirty = true;
                }
            }
            KeyAction::Esc => {
                if unsafe { PAGE != PAGE_MONTH } {
                    unsafe { PAGE = PAGE_MONTH; }
                    dirty = true;
                } else {
                    print("[calendar] ESC -> 退出\n");
                    break;
                }
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[calendar] ESC -> 退出\n");
            break;
        }
        if dirty {
            match unsafe { PAGE } {
                PAGE_DAY => render_day(),
                PAGE_NEW => render_new(),
                PAGE_SYNC => render_sync(),
                _ => render_month(),
            }
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[calendar] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",
    "    mov x1, x2",
    "    bl k_main",
    "    mov x8, #93",
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: calendar\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
