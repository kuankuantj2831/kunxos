// KunxOS 短信本地数据库 SmsStore：/data/sms/ 下的会话级状态存储。
//
// 文件布局（手写文本格式，不依赖堆/sqlite）：
//   /data/sms/index.dat         会话索引：一行一会话，按最后消息时间倒序
//   /data/sms/conv<N>.dat       每会话一个消息文件（N = 会话稳定 id）
//   /data/sms/watermark.dat     已导入水位（rild 短信 id 单调递增，据此去重）
//
// 与 rild（提示词 #14 的 RIL 服务）的分工：rild 负责 Modem 收发与 /data/rild/*.dat
// 持久化；本模块把消息按联系人聚合为会话（时间倒序），并维护发送状态
// （草稿/发送中/已发送/失败）与已读标记。首次启动把 rild 的 inbox/outbox 一次性
// 迁入 /data/sms/；之后仅增量导入新收件（水位去重），新发文由本应用直接写入。

use crate::kit;

pub const MAX_MSGS: usize = 32;
pub const MAX_CONVS: usize = 8;
pub const MSG_TEXT_CAP: usize = 120;
pub const NUM_CAP: usize = 20;

pub const DATA_DIR: &[u8] = b"/data/sms";
pub const INDEX_PATH: &[u8] = b"/data/sms/index.dat";
pub const WATERMARK_PATH: &[u8] = b"/data/sms/watermark.dat";
pub const CONV_PREFIX: &[u8] = b"/data/sms/conv";
pub const INBOX_PATH: &[u8] = b"/data/rild/inbox.dat";
pub const OUTBOX_PATH: &[u8] = b"/data/rild/outbox.dat";

const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const SYS_READ: usize = 63;
const SYS_CLOSE: usize = 57;
const SYS_OPENAT: usize = 56;
const SYS_MKDIRAT: usize = 34;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;

// ==================== 消息状态 ====================

#[derive(Clone, Copy, PartialEq)]
pub enum SmsStatus {
    Received = 0,
    Draft = 1,
    Sending = 2,
    Sent = 3,
    Failed = 4,
}

impl SmsStatus {
    pub fn is_out(self) -> bool {
        matches!(
            self,
            SmsStatus::Draft | SmsStatus::Sending | SmsStatus::Sent | SmsStatus::Failed
        )
    }
}

pub fn status_to_code(s: SmsStatus) -> u8 {
    match s {
        SmsStatus::Received => 0,
        SmsStatus::Draft => 1,
        SmsStatus::Sending => 2,
        SmsStatus::Sent => 3,
        SmsStatus::Failed => 4,
    }
}

pub fn status_from_code(c: u8) -> SmsStatus {
    match c {
        1 => SmsStatus::Draft,
        2 => SmsStatus::Sending,
        3 => SmsStatus::Sent,
        4 => SmsStatus::Failed,
        _ => SmsStatus::Received,
    }
}

// ==================== 数据模型 ====================

#[derive(Clone, Copy)]
pub struct Msg {
    pub out: bool,
    pub number: [u8; NUM_CAP],
    pub num_len: usize,
    pub stamp: [u8; 16],
    pub read: bool,
    pub status: SmsStatus,
    pub text: [u8; MSG_TEXT_CAP],
    pub text_len: usize,
}

#[derive(Clone, Copy)]
pub struct Conv {
    pub number: [u8; NUM_CAP],
    pub num_len: usize,
    pub last: usize,
    pub unread: u32,
    pub id: u8,
}

const MSG_INIT: Msg = Msg {
    out: false,
    number: [0; NUM_CAP],
    num_len: 0,
    stamp: [0; 16],
    read: true,
    status: SmsStatus::Received,
    text: [0; MSG_TEXT_CAP],
    text_len: 0,
};
const CONV_INIT: Conv = Conv {
    number: [0; NUM_CAP],
    num_len: 0,
    last: 0,
    unread: 0,
    id: 0,
};

pub static mut MSGS: [Msg; MAX_MSGS] = [MSG_INIT; MAX_MSGS];
pub static mut MSG_N: usize = 0;
pub static mut CONVS: [Conv; MAX_CONVS] = [CONV_INIT; MAX_CONVS];
pub static mut CONV_N: usize = 0;
static mut WATERMARK: u32 = 0;

// ==================== 文件工具 ====================

static mut PATH_BUF: [u8; 48] = [0; 48];

fn open_bytes(path: &[u8], flags: usize) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(path);
        PATH_BUF[n] = 0;
        kit::syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
    }
}

fn read_file(path: &[u8], buf: &mut [u8]) -> usize {
    unsafe {
        let fd = open_bytes(path, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let n = kit::syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        kit::syscall1(SYS_CLOSE, fd as usize);
        if n > 0 {
            (n as usize).min(buf.len())
        } else {
            0
        }
    }
}

fn write_file(path: &[u8], data: &[u8]) {
    unsafe {
        let fd = open_bytes(path, O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            return;
        }
        kit::write_fd(fd as usize, data);
        kit::syscall1(SYS_CLOSE, fd as usize);
    }
}

fn mkdir_dir() {
    unsafe {
        let n = DATA_DIR.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(DATA_DIR);
        PATH_BUF[n] = 0;
        kit::syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0o755);
    }
}

// ==================== 数值与文本工具 ====================

fn write_dec(buf: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    if v == 0 {
        // 修复：v==0 也要产出 "0"（原实现缺 i-=1，写出空串 —— 会话 id=0
        // 的路径缺号、时间戳变长，所有 id=0 会话共用一个文件互相污染）。
        i -= 1;
        tmp[i] = b'0';
    } else {
        while v > 0 && i > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    let n = tmp.len() - i;
    let c = n.min(buf.len());
    buf[..c].copy_from_slice(&tmp[i..i + c]);
    c
}

/// 两位补零写出（时间戳定长字段用：MM/DD/HH/MM/SS）。
fn write_dec2(buf: &mut [u8], v: u32) -> usize {
    let mut tmp = [0u8; 2];
    tmp[0] = b'0' + ((v / 10) % 10) as u8;
    tmp[1] = b'0' + (v % 10) as u8;
    let c = 2usize.min(buf.len());
    buf[..c].copy_from_slice(&tmp[..c]);
    c
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0u32;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u32);
    }
    Some(v)
}

/// 按 0x1F 切分一行，字段写入 `out`，返回字段数。
fn split_fields<'a>(line: &'a [u8], out: &mut [&'a [u8]; 6]) -> usize {
    let mut f = 0;
    let mut s = 0;
    for i in 0..=line.len() {
        if i == line.len() || line[i] == 0x1F {
            if f < out.len() {
                out[f] = &line[s..i];
                f += 1;
            }
            s = i + 1;
        }
    }
    f
}

fn stamp_bytes(s: &[u8; 16]) -> usize {
    let mut n = 0;
    while n < 16 && s[n] != 0 {
        n += 1;
    }
    n
}

fn clean_text(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] = if src[i] == b'\n' || src[i] == b'\r' { b' ' } else { src[i] };
    }
    n
}

pub fn now_stamp(out: &mut [u8]) -> usize {
    let us = kit::now_us();
    let secs = (us / 1_000_000) as u64;
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = kit::civil_from_days(days);
    let hh = rem / 3600;
    let mi = (rem % 3600) / 60;
    let ss = rem % 60;
    // 定长 "YYYYMMDDHHMMSS"（14B）：显示层按固定偏移取 HHMMSS，
    // 月份等个位数不补零会把后面字段全部错位。
    let mut p = 0;
    p += write_dec(&mut out[p..], y as u32);
    p += write_dec2(&mut out[p..], m);
    p += write_dec2(&mut out[p..], d);
    p += write_dec2(&mut out[p..], hh as u32);
    p += write_dec2(&mut out[p..], mi as u32);
    p += write_dec2(&mut out[p..], ss as u32);
    p
}

// ==================== 内存 CRUD ====================

/// 追加一条消息并重建会话分组（时间倒序）。
pub fn push(out: bool, number: &[u8], stamp: &[u8], read: bool, status: SmsStatus, text: &[u8]) -> bool {
    unsafe {
        // 空号码会让 conv_title 返回空串 → 列表绘制 title[0] panic，
        // 且空号码会话无法按号码匹配。拒绝入库。
        if MSG_N >= MAX_MSGS || number.is_empty() {
            return false;
        }
        let m = &mut MSGS[MSG_N];
        m.out = out;
        m.num_len = number.len().min(NUM_CAP);
        m.number[..m.num_len].copy_from_slice(&number[..m.num_len]);
        let sl = stamp.len().min(16);
        m.stamp[..sl].copy_from_slice(&stamp[..sl]);
        m.read = read;
        m.status = status;
        m.text_len = text.len().min(MSG_TEXT_CAP);
        m.text[..m.text_len].copy_from_slice(&text[..m.text_len]);
        MSG_N += 1;
        rebuild_convs();
    }
    true
}

pub fn has_sending() -> bool {
    unsafe {
        for i in 0..MSG_N {
            if MSGS[i].status == SmsStatus::Sending {
                return true;
            }
        }
    }
    false
}

pub fn find_conv(number: &[u8]) -> Option<usize> {
    unsafe {
        for c in 0..CONV_N {
            if CONVS[c].num_len == number.len() && CONVS[c].number[..number.len()] == number[..] {
                return Some(c);
            }
        }
        None
    }
}

pub fn unread_total() -> u32 {
    unsafe {
        let mut n = 0;
        for c in 0..CONV_N {
            n += CONVS[c].unread;
        }
        n
    }
}

const CONV_MAX: usize = MAX_CONVS;

/// 按号码分组重建会话列表：时间倒序（最后一条消息最近者在前）。
fn rebuild_convs() {
    unsafe {
        let old = CONVS;
        CONV_N = 0;
        for i in (0..MSG_N).rev() {
            let m = MSGS[i];
            let mut found = false;
            for c in 0..CONV_N {
                if CONVS[c].num_len == m.num_len && CONVS[c].number[..m.num_len] == m.number[..m.num_len] {
                    found = true;
                    break;
                }
            }
            if !found && CONV_N < CONV_MAX {
                // 会话稳定 id：沿用旧映射；新会话分配最小未用 id。
                let mut id = 0xFFu8;
                for o in 0..CONV_MAX {
                    if old[o].num_len == m.num_len && old[o].number[..m.num_len] == m.number[..m.num_len] {
                        id = old[o].id;
                        break;
                    }
                }
                if id == 0xFF {
                    id = 0;
                    loop {
                        let mut used = false;
                        for c in 0..CONV_N {
                            if CONVS[c].id == id {
                                used = true;
                                break;
                            }
                        }
                        if !used {
                            break;
                        }
                        id = id.wrapping_add(1);
                    }
                }
                CONVS[CONV_N].number = m.number;
                CONVS[CONV_N].num_len = m.num_len;
                CONVS[CONV_N].id = id;
                CONVS[CONV_N].last = i;
                CONVS[CONV_N].unread = 0;
                CONV_N += 1;
            }
        }
        // 未读数
        for c in 0..CONV_N {
            CONVS[c].unread = 0;
            for i in 0..MSG_N {
                let m = MSGS[i];
                if !m.out && !m.read && m.num_len == CONVS[c].num_len
                    && m.number[..m.num_len] == CONVS[c].number[..CONVS[c].num_len]
                {
                    CONVS[c].unread += 1;
                }
            }
        }
    }
}

/// 将会话全部标为已读并持久化。
pub fn mark_conv_read(idx: usize) {
    unsafe {
        if idx >= CONV_N {
            return;
        }
        let num = CONVS[idx].number;
        let num_len = CONVS[idx].num_len;
        for i in 0..MSG_N {
            let m = &mut MSGS[i];
            if !m.out && !m.read && m.num_len == num_len && m.number[..num_len] == num[..num_len] {
                m.read = true;
            }
        }
        rebuild_convs();
        save_all();
    }
}

/// 根据 rild 的 SMS 状态推进「发送中」消息。
pub fn apply_send_result(ok: bool) {
    unsafe {
        let mut changed = false;
        for i in 0..MSG_N {
            let m = &mut MSGS[i];
            if m.status == SmsStatus::Sending {
                m.status = if ok { SmsStatus::Sent } else { SmsStatus::Failed };
                changed = true;
            }
        }
        if changed {
            save_all();
        }
    }
}

// ==================== 持久化 ====================

/// 写回索引与会话文件（全部，数据量小）。
pub fn save_all() {
    unsafe {
        // ---- 索引 ----//
        let mut idx = [0u8; 1024];
        idx[..9].copy_from_slice(b"KXSMSIDX1");
        idx[9] = b'\n';
        let mut p = 10;
        for c in 0..CONV_N {
            let conv = CONVS[c];
            let m = MSGS[conv.last];
            p += write_dec(&mut idx[p..], conv.id as u32);
            idx[p] = 0x1F;
            p += 1;
            let nl = conv.num_len.min(20);
            idx[p..p + nl].copy_from_slice(&conv.number[..nl]);
            p += nl;
            idx[p] = 0x1F;
            p += 1;
            let sl = stamp_bytes(&m.stamp).min(14);
            idx[p..p + sl].copy_from_slice(&m.stamp[..sl]);
            p += sl;
            idx[p] = 0x1F;
            p += 1;
            p += write_dec(&mut idx[p..], conv.unread);
            idx[p] = b'\n';
            p += 1;
        }
        write_file(INDEX_PATH, &idx[..p]);

        // ---- 会话文件 ----//
        for c in 0..CONV_N {
            let conv = CONVS[c];
            let mut fbuf = [0u8; 6144];
            fbuf[..7].copy_from_slice(b"KXSMSG1");
            fbuf[7] = b'\n';
            let mut q = 8;
            for i in 0..MSG_N {
                let m = MSGS[i];
                if m.num_len != conv.num_len || m.number[..m.num_len] != conv.number[..conv.num_len] {
                    continue;
                }
                let sl = stamp_bytes(&m.stamp).min(14);
                let space = fbuf.len().saturating_sub(q + 1);
                let sl2 = sl.min(space);
                fbuf[q..q + sl2].copy_from_slice(&m.stamp[..sl2]);
                q += sl2;
                fbuf[q] = 0x1F;
                q += 1;
                fbuf[q] = b'0' + status_to_code(m.status);
                q += 1;
                fbuf[q] = 0x1F;
                q += 1;
                fbuf[q] = if m.read { b'1' } else { b'0' };
                q += 1;
                fbuf[q] = 0x1F;
                q += 1;
                let nl = m.num_len.min(20);
                fbuf[q..q + nl].copy_from_slice(&m.number[..nl]);
                q += nl;
                fbuf[q] = 0x1F;
                q += 1;
                let tl = m.text_len.min(MSG_TEXT_CAP);
                let mut tb = [0u8; MSG_TEXT_CAP];
                let tn = clean_text(&mut tb, &m.text[..tl]).min(fbuf.len().saturating_sub(q + 1));
                fbuf[q..q + tn].copy_from_slice(&tb[..tn]);
                q += tn;
                fbuf[q] = b'\n';
                q += 1;
            }
            let mut path = [0u8; 40];
            let base = CONV_PREFIX.len();
            path[..base].copy_from_slice(CONV_PREFIX);
            let pn = base + write_dec(&mut path[base..], conv.id as u32);
            path[pn..pn + 4].copy_from_slice(b".dat");
            write_file(&path[..pn + 4], &fbuf[..q]);
        }
    }
}

// ==================== 导入（rild 增量） ====================

fn load_watermark() {
    unsafe {
        let mut buf = [0u8; 16];
        let n = read_file(WATERMARK_PATH, &mut buf);
        WATERMARK = parse_u32(&buf[..n]).unwrap_or(0);
    }
}

fn save_watermark() {
    let mut buf = [0u8; 16];
    let n = write_dec(&mut buf, unsafe { WATERMARK });
    write_file(WATERMARK_PATH, &buf[..n]);
}

/// 扫描一张 rild 记录文件导入消息；`all=true`（首次迁移）时忽略水位，
/// 否则只导入 id 大于水位的增量。推进水位并返回导入条数。
fn import_records(path: &[u8], out: bool, status: SmsStatus, all: bool) -> u32 {
    unsafe {
        let mut buf = [0u8; 8192];
        let n = read_file(path, &mut buf);
        if n < 8 || !buf[..n].starts_with(b"KXSMS1") {
            return 0;
        }
        let mut added = 0u32;
        let mut max_id = WATERMARK;
        let mut start = 7usize;
        while start < n {
            let end = match buf[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &buf[start..end];
            let mut f: [&[u8]; 6] = [&[]; 6];
            let fc = split_fields(line, &mut f);
            if fc >= 5 {
                if let Some(id) = parse_u32(f[0]) {
                    if id > max_id {
                        max_id = id;
                    }
                    if (all || id > WATERMARK)
                        && push(out, f[1], f[2], f[3].starts_with(b"1"), status, f[4])
                    {
                        added += 1;
                    }
                }
            }
            start = end + 1;
        }
        WATERMARK = max_id;
        added
    }
}

/// 增量导入新收件（监听 rild 推送）。返回导入条数。
pub fn import_inbox() -> u32 {
    let added = import_records(INBOX_PATH, false, SmsStatus::Received, false);
    save_watermark();
    if added > 0 {
        save_all();
    }
    added
}

/// 一次性迁移 rild 历史（发件箱 + 收件箱，首次进入 /data/sms 时调用）。
fn import_migration() -> u32 {
    let n1 = import_records(OUTBOX_PATH, true, SmsStatus::Sent, true);
    let n2 = import_records(INBOX_PATH, false, SmsStatus::Received, true);
    save_watermark();
    if n1 + n2 > 0 {
        save_all();
    }
    n1 + n2
}

// ==================== 初始化 ====================

/// 从磁盘加载：水位 → 索引 → 各会话消息。返回 false 表示首次运行。
fn load_db() -> bool {
    unsafe {
        load_watermark();
        MSG_N = 0;
        CONV_N = 0;
        let mut idx = [0u8; 1024];
        let n = read_file(INDEX_PATH, &mut idx);
        if n < 10 || !idx[..n].starts_with(b"KXSMSIDX1") {
            return false;
        }
        // 索引 → 会话骨架（含稳定 id）
        let mut start = 10usize;
        while start < n {
            let end = match idx[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &idx[start..end];
            let mut f: [&[u8]; 6] = [&[]; 6];
            let fc = split_fields(line, &mut f);
            if fc >= 4 && CONV_N < MAX_CONVS {
                let c = CONV_N;
                CONVS[c].id = parse_u32(f[0]).unwrap_or(0).min((MAX_CONVS - 1) as u32) as u8;
                CONVS[c].num_len = f[1].len().min(NUM_CAP);
                CONVS[c].number[..CONVS[c].num_len].copy_from_slice(&f[1][..CONVS[c].num_len]);
                CONVS[c].last = 0;
                CONVS[c].unread = 0;
                CONV_N += 1;
            }
            start = end + 1;
        }
        // 会话文件 → 消息（追加进全局 MSGS）
        for c in 0..CONV_N {
            let conv = CONVS[c];
            let mut path = [0u8; 40];
            let base = CONV_PREFIX.len();
            path[..base].copy_from_slice(CONV_PREFIX);
            let pn = base + write_dec(&mut path[base..], conv.id as u32);
            path[pn..pn + 4].copy_from_slice(b".dat");
            let mut fbuf = [0u8; 6144];
            let fn_ = read_file(&path[..pn + 4], &mut fbuf);
            if fn_ < 8 || !fbuf[..fn_].starts_with(b"KXSMSG1") {
                continue;
            }
            let mut s = 8usize;
            while s < fn_ {
                let end = match fbuf[s..fn_].iter().position(|&b| b == b'\n') {
                    Some(x) => s + x,
                    None => fn_,
                };
                let line = &fbuf[s..end];
                let mut f: [&[u8]; 6] = [&[]; 6];
                let fc = split_fields(line, &mut f);
                // stamp \x1F status \x1F read \x1F num \x1F text
                if fc >= 5 {
                    let status = status_from_code(f[1].first().map(|b| *b - b'0').unwrap_or(0));
                    let read = f[2].starts_with(b"1");
                    let num = if f[3].is_empty() { &conv.number[..conv.num_len] } else { f[3] };
                    if MSG_N < MAX_MSGS {
                        push(status.is_out(), num, f[0], read, status, f[4]);
                    }
                }
                s = end + 1;
            }
        }
        rebuild_convs();
        true
    }
}

/// 初始化：建目录、加载本地库、首次迁移 rild 历史、增量导入新收件。
pub fn init_db() {
    mkdir_dir();
    unsafe {
        MSG_N = 0;
        CONV_N = 0;
    }
    let first = load_db();
    if first {
        import_migration();
    } else {
        import_inbox();
    }
}