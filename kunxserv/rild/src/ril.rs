//! RIL 核心：通话状态机、SIM 状态、短信收发（PDU 编解码 + 持久化）、
//! 紧急呼叫、音频焦点/路由联动，以及 /var/run/rild.state 状态文件输出。
//!
//! 状态机（初始范围：单卡、电路域语音、无 VoLTE）：
//! ```text
//! Idle --DIAL--> Dialing --CLCC(stat=0)--> Active --HANGUP/NO CARRIER--> Idle
//!  ^       +CRING(MT) --> Ringing --ATA--> Active
//!  └------------------NO CARRIER/ATH-------------------------------┘
//! ```

use crate::at::{self, At, Final};
use crate::pdu;
use crate::sys;

// ==================== 常量 ====================

/// 紧急号码集合（3GPP TS 22.101：无 SIM 也可拨出）。
const EMERGENCY: [&[u8]; 6] = [b"112", b"911", b"110", b"119", b"120", b"122"];

const STATE_PATH: &str = "/var/run/rild.state";
const INBOX_PATH: &str = "/data/rild/inbox.dat";
const OUTBOX_PATH: &str = "/data/rild/outbox.dat";
const CONTACTS_PATH: &str = "/data/contacts.dat";

/// 默认短信中心（中国移动，可经 SET_SMSC 修改）。
const DEFAULT_SMSC: &[u8] = b"+8613800100500";

/// 音频 ioctl（与内核 devfs 常量一致）。
const AUDIO_IOCTL_TONE: u32 = 0x4105;
const AUDIO_IOCTL_RING_START: u32 = 0x4106;
const AUDIO_IOCTL_RING_STOP: u32 = 0x4107;
const AUDIO_IOCTL_ACQUIRE: u32 = 0x4108;
const AUDIO_IOCTL_RELEASE: u32 = 0x4109;
const AUDIO_IOCTL_GET_STATE: u32 = 0x410A;
const AUDIO_IOCTL_SET_ROUTE: u32 = 0x410B;
const FOCUS_CALL: u32 = 1;

/// 级联短信重组表（并发会话数）。
const CONCAT_SLOTS: usize = 4;
/// 级联段数上限。
const CONCAT_MAX_PARTS: usize = 8;

// ==================== 状态 ====================

#[derive(Clone, Copy, PartialEq)]
pub enum CallState {
    Idle,
    Dialing,
    Ringing,
    Active,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Dir {
    None,
    Out,
    In,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SimState {
    Unknown,
    Ready,
    Absent,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SmsState {
    Idle,
    Sending,
    Sent,
    Failed,
}

pub struct Ril {
    pub call: CallState,
    pub dir: Dir,
    pub num: [u8; 24],
    pub num_len: usize,
    pub call_start: u64,
    pub missed: bool,
    pub miss_num: [u8; 24],
    pub miss_num_len: usize,
    pub sim: SimState,
    pub signal: u8,
    pub op: [u8; 24],
    pub op_len: usize,
    pub reg: u8,
    pub smsc: [u8; 24],
    pub smsc_len: usize,
    pub sms_state: SmsState,
    pub sms_info: [u8; 48],
    pub sms_info_len: usize,
    pub inbox: u32,
    pub outbox: u32,
    /// 状态修订号（每次变化 +1，应用据此重读）。
    pub seq: u32,
    /// 短信记录单调 id。
    pub sms_id: u32,
    /// 是否已初始化（modem 探测完成）。
    pub modem_up: bool,
}

impl Ril {
    const fn new() -> Ril {
        Ril {
            call: CallState::Idle,
            dir: Dir::None,
            num: [0; 24],
            num_len: 0,
            call_start: 0,
            missed: false,
            miss_num: [0; 24],
            miss_num_len: 0,
            sim: SimState::Unknown,
            signal: 0,
            op: [0; 24],
            op_len: 0,
            reg: 0,
            smsc: [0; 24],
            smsc_len: 0,
            sms_state: SmsState::Idle,
            sms_info: [0; 48],
            sms_info_len: 0,
            inbox: 0,
            outbox: 0,
            seq: 1,
            sms_id: 1,
            modem_up: false,
        }
    }
}

pub static mut RIL: Ril = Ril::new();
pub static mut AT: At = At::new();
pub static mut AUDIO_FD: isize = -1;
static mut DIRTY: bool = true;
static mut LAST_CLCC: u64 = 0;
static mut LAST_STATE_WRITE: u64 = 0;
static mut LAST_CONTACTS_LOAD: u64 = 0;
static mut LAST_SIM_POLL: u64 = 0;
static mut MR: u8 = 1;
/// 上次拨号 / 来电号码（供 contacts 查询缓存）。
static mut LAST_NUM: [u8; 24] = [0; 24];
static mut LAST_NUM_LEN: usize = 0;

// ---- 联系人缓存（/data/contacts.dat） ----
static mut CONTACTS: [[u8; 64]; 16] = [[0; 64]; 16]; // "name\x1Fnum" 行
static mut CONTACTS_N: usize = 0;

// ---- 级联短信重组 ----
#[derive(Clone, Copy)]
struct ConcatSlot {
    active: bool,
    ref_id: u8,
    total: u8,
    started_us: u64, // 首段到达时刻（concat_gc 超时清理用）
    parts: [u8; CONCAT_MAX_PARTS], // 已收段位图
    oa: [u8; 24],
    oa_len: usize,
    scts: [u8; 24],
    buf: [u8; CONCAT_MAX_PARTS * 200],
    lens: [u16; CONCAT_MAX_PARTS],
}
static mut CONCAT: [ConcatSlot; CONCAT_SLOTS] = [ConcatSlot {
    active: false,
    ref_id: 0,
    started_us: 0,
    total: 0,
    parts: [0; CONCAT_MAX_PARTS],
    oa: [0; 24],
    oa_len: 0,
    scts: [0; 24],
    buf: [0; CONCAT_MAX_PARTS * 200],
    lens: [0; CONCAT_MAX_PARTS],
}; CONCAT_SLOTS];

// ==================== 工具 ====================

pub fn log(s: &str) {
    sys::print(s);
}

fn set_info(s: &[u8]) {
    unsafe {
        RIL.sms_info_len = s.len().min(RIL.sms_info.len());
        RIL.sms_info[..RIL.sms_info_len].copy_from_slice(&s[..RIL.sms_info_len]);
        RIL.sms_state = SmsState::Idle;
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
}

pub fn mark_dirty() {
    unsafe { DIRTY = true };
}

fn is_emergency(num: &[u8]) -> bool {
    EMERGENCY.iter().any(|e| *e == num)
}

/// 规范化号码（去空格 / 破折号）。
fn norm_num(s: &[u8]) -> &[u8] {
    // 返回去除空白后的切片（仅去除首尾）
    s.trim_ascii()
}

fn hex_digit(v: u8) -> u8 {
    if v < 10 {
        b'0' + v
    } else {
        b'A' + v - 10
    }
}

fn to_hex(src: &[u8], dst: &mut [u8]) -> usize {
    let n = src.len().min(dst.len() / 2);
    for i in 0..n {
        dst[i * 2] = hex_digit(src[i] >> 4);
        dst[i * 2 + 1] = hex_digit(src[i] & 0x0F);
    }
    n * 2
}

fn from_hex(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + 1 < src.len() && n < dst.len() {
        let hi = (src[i] as char).to_digit(16);
        let lo = (src[i + 1] as char).to_digit(16);
        if let (Some(h), Some(l)) = (hi, lo) {
            dst[n] = ((h << 4) | l) as u8;
            n += 1;
            i += 2;
        } else {
            break;
        }
    }
    n
}

trait TrimAsciiExt {
    fn trim_ascii(&self) -> &[u8];
}
impl TrimAsciiExt for [u8] {
    fn trim_ascii(&self) -> &[u8] {
        let mut s = 0;
        let mut e = self.len();
        while s < e && (self[s] == b' ' || self[s] == b'\t' || self[s] == b'\r' || self[s] == b'\n') {
            s += 1;
        }
        while e > s && (self[e - 1] == b' ' || self[e - 1] == b'\t' || self[e - 1] == b'\r' || self[e - 1] == b'\n') {
            e -= 1;
        }
        &self[s..e]
    }
}

/// 墙钟时间 → "YYYYMMDDHHMMSS"（14 字节）。
fn now_stamp(buf: &mut [u8]) -> usize {
    let us = sys::wall_us();
    let secs = (us / 1_000_000) as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let mut p = 0;
    p += put_dec(buf, p, y as u32, 4);
    p += put_dec(buf, p, m, 2);
    p += put_dec(buf, p, d, 2);
    p += put_dec(buf, p, hh as u32, 2);
    p += put_dec(buf, p, mm as u32, 2);
    p += put_dec(buf, p, ss as u32, 2);
    p
}

fn put_dec(buf: &mut [u8], pos: usize, mut v: u32, width: usize) -> usize {
    let mut i = pos + width;
    while i > pos {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    width
}

/// Howard Hinnant 的 civil_from_days（与 app_kit 一致）。
fn civil_from_days(z: i64) -> (u32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as u32;
    (y, m, d)
}

// ==================== 音频 ====================

fn audio_ioctl(request: u32, arg: &mut [u8]) {
    if unsafe { AUDIO_FD } < 0 {
        return;
    }
    unsafe {
        sys::ioctl(AUDIO_FD as usize, request, arg.as_mut_ptr());
    }
}

fn audio_tone(freq: u32, dur_ms: u32) {
    let mut arg = [0u8; 8];
    arg[0..4].copy_from_slice(&freq.to_le_bytes());
    arg[4..8].copy_from_slice(&dur_ms.to_le_bytes());
    audio_ioctl(AUDIO_IOCTL_TONE, &mut arg);
}

fn audio_ring_start() {
    let mut arg = [0u8; 8];
    arg[0..4].copy_from_slice(&880u32.to_le_bytes());
    audio_ioctl(AUDIO_IOCTL_RING_START, &mut arg);
}

fn audio_ring_stop() {
    let mut arg = [0u8; 8];
    audio_ioctl(AUDIO_IOCTL_RING_STOP, &mut arg);
}

fn audio_acquire_call() {
    let mut arg = [0u8; 8];
    arg[0..4].copy_from_slice(&FOCUS_CALL.to_le_bytes());
    audio_ioctl(AUDIO_IOCTL_ACQUIRE, &mut arg);
}

fn audio_release_call() {
    let mut arg = [0u8; 8];
    arg[0..4].copy_from_slice(&FOCUS_CALL.to_le_bytes());
    audio_ioctl(AUDIO_IOCTL_RELEASE, &mut arg);
}

fn audio_route(route: u32) {
    let mut arg = [0u8; 8];
    arg[0..4].copy_from_slice(&route.to_le_bytes());
    audio_ioctl(AUDIO_IOCTL_SET_ROUTE, &mut arg);
}

/// 当前音频路由（查询 /dev/audio）。
fn audio_route_now() -> u32 {
    let mut arg = [0u8; 8];
    audio_ioctl(AUDIO_IOCTL_GET_STATE, &mut arg);
    u32::from_le_bytes([arg[4], arg[5], arg[6], arg[7]])
}

// ==================== Modem 初始化 ====================

/// 启动握手：查询 SIM / 信号 / 运营商 / 注册 / 短信中心。
pub fn init_modem() {
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };

    // 基础配置
    at.command(b"AT", &mut resp);
    at.command(b"ATE0", &mut resp);
    at.command(b"AT+CMGF=0", &mut resp);
    at.command(b"AT+CMEE=1", &mut resp);
    at.command(b"AT+CNMI=2,1,0,0,0", &mut resp);
    at.command(b"AT+CLIP=1", &mut resp);

    // SIM 状态
    match at.command(b"AT+CPIN?", &mut resp) {
        Final::Ok => {
            unsafe { RIL.sim = SimState::Ready };
        }
        Final::Cme(10) => {
            unsafe { RIL.sim = SimState::Absent };
        }
        _ => {
            unsafe { RIL.sim = SimState::Unknown };
        }
    }

    // 信号
    at.command(b"AT+CSQ", &mut resp);
    if let Some(line) = first_line(&resp, b"+CSQ") {
        if let Some(first) = at::after_colon(line).split_commas().next() {
            if let Some(n) = at::parse_num(first) {
                unsafe { RIL.signal = n.min(31) as u8 };
            }
        }
    }

    // 运营商
    at.command(b"AT+COPS?", &mut resp);
    if let Some(line) = first_line(&resp, b"+COPS") {
        if let Some(op) = quoted_field(line) {
            unsafe {
                RIL.op_len = op.len().min(RIL.op.len());
                RIL.op[..RIL.op_len].copy_from_slice(&op[..RIL.op_len]);
            }
        }
    }

    // 注册
    at.command(b"AT+CREG?", &mut resp);
    if let Some(line) = first_line(&resp, b"+CREG") {
        let after = at::after_colon(line);
        if let Some((_, b)) = split_comma(after) {
            if let Some(n) = at::parse_num(b) {
                unsafe { RIL.reg = n.min(5) as u8 };
            }
        }
    }

    // 短信中心
    if at.command(b"AT+CSCA?", &mut resp) == Final::Ok {
        if let Some(line) = first_line(&resp, b"+CSCA") {
            if let Some(smsc) = quoted_field(line) {
                unsafe {
                    RIL.smsc_len = smsc.len().min(RIL.smsc.len());
                    RIL.smsc[..RIL.smsc_len].copy_from_slice(&smsc[..RIL.smsc_len]);
                }
            }
        }
    }
    if unsafe { RIL.smsc_len } == 0 {
        unsafe {
            RIL.smsc_len = DEFAULT_SMSC.len().min(RIL.smsc.len());
            RIL.smsc[..RIL.smsc_len].copy_from_slice(&DEFAULT_SMSC[..RIL.smsc_len]);
        }
    }

    // 收件箱 / 发件箱计数
    unsafe {
        RIL.inbox = count_records(INBOX_PATH);
        RIL.outbox = count_records(OUTBOX_PATH);
        RIL.modem_up = true;
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
    crate::log("[rild] 调制解调器初始化完成\n");
}

trait SplitCommas {
    fn split_commas(&self) -> SplitIter<'_>;
}
impl SplitCommas for [u8] {
    fn split_commas(&self) -> SplitIter<'_> {
        SplitIter { s: self, pos: 0 }
    }
}
struct SplitIter<'a> {
    s: &'a [u8],
    pos: usize,
}
impl<'a> Iterator for SplitIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos > self.s.len() {
            return None;
        }
        let start = self.pos;
        let end = self.s[start..].iter().position(|&b| b == b',').map(|p| start + p).unwrap_or(self.s.len());
        self.pos = end + 1;
        Some(&self.s[start..end])
    }
}

fn split_comma(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let idx = s.iter().position(|&b| b == b',')?;
    Some((&s[..idx], &s[idx + 1..]))
}

/// 在响应缓冲（以 \n 分隔的行）中找以 `prefix` 开头的首行。
fn first_line<'a>(resp: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    for line in resp.split(|&b| b == b'\n') {
        let t = line.trim_ascii();
        if t.starts_with(prefix) {
            return Some(t);
        }
    }
    None
}

/// 提取带引号字段 "+COPS: 0,0,\"CMCC\"" → "CMCC"。
fn quoted_field(line: &[u8]) -> Option<&[u8]> {
    let start = line.iter().position(|&b| b == b'"')?;
    let rest = &line[start + 1..];
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..end])
}

// ==================== URC 处理 ====================

/// AT 层派发 URC 行（无挂起命令时）。
pub fn on_urc(line: &[u8]) {
    let t = line.trim_ascii();
    if t.starts_with(b"+CRING") || t == b"RING" {
        // 来电
        let num = at::after_colon(t);
        let num = num.trim_ascii();
        unsafe {
            if RIL.call == CallState::Idle {
                RIL.call = CallState::Ringing;
                RIL.dir = Dir::In;
                RIL.num_len = num.len().min(RIL.num.len());
                RIL.num[..RIL.num_len].copy_from_slice(&num[..RIL.num_len]);
                RIL.missed = false;
                RIL.seq = RIL.seq.wrapping_add(1);
                DIRTY = true;
                audio_ring_start();
                crate::log("[rild] 来电：");
                sys::print_bytes(num);
                crate::log("\n");
            }
        }
    } else if t.starts_with(b"+CMTI") {
        // 新短信："+CMTI: \"SM\",<idx>"
        let after = at::after_colon(t);
        if let Some((_, idx)) = split_comma(after) {
            if let Some(n) = at::parse_num(idx.trim_ascii()) {
                crate::log("[rild] 新短信：SM 索引 ");
                sys::print_dec(n as u64);
                crate::log("\n");
                receive_sms(n);
            }
        }
    } else if t.starts_with(b"NO CARRIER") {
        crate::log("[rild] NO CARRIER\n");
        end_call();
    } else if t.starts_with(b"+CREG") {
        let after = at::after_colon(t);
        if let Some((_, b)) = split_comma(after) {
            if let Some(n) = at::parse_num(b.trim_ascii()) {
                unsafe {
                    RIL.reg = n.min(5) as u8;
                    RIL.seq = RIL.seq.wrapping_add(1);
                    DIRTY = true;
                }
            }
        }
    }
}

fn end_call() {
    unsafe {
        let was_ringing_in = RIL.call == CallState::Ringing && RIL.dir == Dir::In;
        if was_ringing_in {
            RIL.missed = true;
            RIL.miss_num_len = RIL.num_len;
            RIL.miss_num[..RIL.miss_num_len].copy_from_slice(&RIL.num[..RIL.num_len]);
        }
        let was_in_call = RIL.call != CallState::Idle;
        RIL.call = CallState::Idle;
        RIL.dir = Dir::None;
        RIL.num_len = 0;
        RIL.call_start = 0;
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
        audio_ring_stop();
        if was_in_call {
            audio_release_call();
            audio_route(0);
        }
    }
}

// ==================== 通话命令 ====================

/// DIAL:<num> — 发起呼叫（无 SIM 时仅允许紧急号码）。
pub fn dial(num: &[u8]) {
    let num = norm_num(num);
    if num.is_empty() {
        set_info("号码为空".as_bytes());
        return;
    }
    unsafe {
        if RIL.call != CallState::Idle {
            set_info("通话进行中".as_bytes());
            return;
        }
        if RIL.sim == SimState::Absent && !is_emergency(num) {
            set_info("无 SIM，仅支持紧急呼叫".as_bytes());
            return;
        }
        RIL.num_len = num.len().min(RIL.num.len());
        RIL.num[..RIL.num_len].copy_from_slice(&num[..RIL.num_len]);
        RIL.dir = Dir::Out;
        RIL.call = CallState::Dialing;
        RIL.missed = false;
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
    crate::log("[rild] 拨号 ");
    sys::print_bytes(num);
    crate::log("\n");

    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    let mut cmd = [0u8; 40];
    cmd[0] = b'A';
    cmd[1] = b'T';
    cmd[2] = b'D';
    let n = 3 + num.len().min(30);
    cmd[3..n].copy_from_slice(&num[..n - 3]);
    cmd[n] = b';';
    match at.command(&cmd[..n + 1], &mut resp) {
        Final::Ok => {
            crate::log("[rild] ATD OK\n");
            // 拨号音（异步内核音调任务；QEMU 无音频后端时静默）
        }
        Final::Cme(code) => {
            crate::log("[rild] ATD +CME ERROR: ");
            sys::print_dec(code as u64);
            crate::log("\n");
            end_call();
            if code == 10 {
                set_info("无 SIM 或未插卡".as_bytes());
            } else {
                set_info("拨号失败".as_bytes());
            }
        }
        _ => {
            crate::log("[rild] ATD 失败\n");
            end_call();
            set_info("拨号失败".as_bytes());
        }
    }
}

/// HANGUP — 挂断。
pub fn hangup() {
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    let _ = at.command(b"ATH", &mut resp);
    crate::log("[rild] 挂断\n");
    end_call();
}

/// ANSWER — 接听。
pub fn answer() {
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    match at.command(b"ATA", &mut resp) {
        Final::Ok => {
            crate::log("[rild] ATA OK\n");
            unsafe {
                RIL.call = CallState::Active;
                RIL.call_start = sys::now_us();
                RIL.seq = RIL.seq.wrapping_add(1);
                DIRTY = true;
            }
            audio_ring_stop();
            audio_acquire_call();
            audio_route(0);
        }
        _ => {
            crate::log("[rild] ATA 失败\n");
            set_info("接听失败".as_bytes());
        }
    }
}

// ==================== CLCC 轮询 ====================

/// 通话期间周期轮询 AT+CLCC 同步真实呼叫状态（呼入振铃→接通等）。
pub fn poll_clcc() {
    unsafe {
        if RIL.call == CallState::Idle {
            return;
        }
        let now = sys::now_us();
        if now.saturating_sub(LAST_CLCC) < 500_000 {
            return;
        }
        LAST_CLCC = now;
    }
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    if at.command(b"AT+CLCC", &mut resp) != Final::Ok {
        return;
    }
    // 解析 "+CLCC: <id>,<dir>,<stat>,..."
    let line = match first_line(&resp, b"+CLCC") {
        Some(l) => l,
        None => {
            // 无呼叫行 → 呼叫已结束
            if unsafe { RIL.call != CallState::Idle } {
                crate::log("[rild] CLCC：无呼叫，结束通话\n");
                end_call();
            }
            return;
        }
    };
    let after = at::after_colon(line);
    let mut it = after.split_commas();
    let _id = it.next();
    let dir = it.next().and_then(|s| at::parse_num(s.trim_ascii()));
    let stat = it.next().and_then(|s| at::parse_num(s.trim_ascii()));
    // 跳过 mode / mpty，取号码字段
    let _mode = it.next();
    let _mpty = it.next();
    let num = it.next().map(|s| s.trim_ascii().trim_quotes());
    let stat = stat.unwrap_or(0);
    unsafe {
        match stat {
            0 => {
                // 通话中
                if RIL.call == CallState::Dialing || RIL.call == CallState::Ringing {
                    crate::log("[rild] 通话 → ACTIVE\n");
                    RIL.call = CallState::Active;
                    RIL.call_start = sys::now_us();
                    RIL.seq = RIL.seq.wrapping_add(1);
                    DIRTY = true;
                    audio_ring_stop();
                    audio_acquire_call();
                    audio_route(0);
                }
            }
            2 => {
                RIL.call = CallState::Dialing;
            }
            3 => {
                if RIL.call == CallState::Idle {
                    RIL.call = CallState::Ringing;
                    RIL.dir = Dir::In;
                    audio_ring_start();
                }
            }
            _ => {}
        }
        if let Some(n) = num {
            if !n.is_empty() && RIL.num_len != n.len() {
                RIL.num_len = n.len().min(RIL.num.len());
                RIL.num[..RIL.num_len].copy_from_slice(&n[..RIL.num_len]);
                RIL.seq = RIL.seq.wrapping_add(1);
                DIRTY = true;
            }
        }
        let _ = dir;
    }
}

trait TrimQuotes {
    fn trim_quotes(&self) -> &[u8];
}
impl TrimQuotes for [u8] {
    fn trim_quotes(&self) -> &[u8] {
        let t = self.trim_ascii();
        if t.starts_with(b"\"") && t.ends_with(b"\"") && t.len() >= 2 {
            &t[1..t.len() - 1]
        } else {
            t
        }
    }
}

// ==================== 短信发送 ====================

/// SEND:<num>\x1F<text> — 发送短信（含长短信分段）。
pub fn send_sms(raw: &[u8]) {
    // 拆分 num \x1F text
    let sep = match raw.iter().position(|&b| b == 0x1F) {
        Some(p) => p,
        None => {
            set_info("格式错误：SEND:号码\\x1F内容".as_bytes());
            return;
        }
    };
    let num = norm_num(&raw[..sep]);
    let text = raw[sep + 1..].trim_ascii();
    if num.is_empty() || text.is_empty() {
        set_info("号码或内容为空".as_bytes());
        return;
    }
    unsafe {
        RIL.sms_state = SmsState::Sending;
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
    crate::log("[rild] 发送短信给 ");
    sys::print_bytes(num);
    crate::log(" len=");
    sys::print_dec(text.len() as u64);
    crate::log("\n");

    let dcs = pdu::choose_dcs(text);
    let parts = pdu::concat_parts(text, dcs);
    let mut ok_parts = 0u32;
    for part in 1..=parts {
        let slice = match pdu::concat_slice(text, dcs, part) {
            Some(s) => s,
            None => break,
        };
        let (off, len) = slice;
        let text_part = &text[off..off + len];
        let mr = unsafe { MR };
        let (pdu_bytes, _udl) = match pdu::submit_encode(
            unsafe { &RIL.smsc[..RIL.smsc_len] },
            num,
            text_part,
            dcs,
            mr,
            0x01,
            parts,
            part,
        ) {
            Some(p) => p,
            None => {
                set_info("PDU 编码失败".as_bytes());
                return;
            }
        };
        let mut hex = [0u8; 420];
        let hex_len = to_hex(pdu_bytes, &mut hex);
        let mut cmd = [0u8; 24];
        cmd[..8].copy_from_slice(b"AT+CMGS=");
        let n = 8 + write_dec(&mut cmd[8..], pdu_bytes.len() as u32);
        let mut resp = [0u8; at::RESP_CAP];
        let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
        match at.command_with_data(&cmd[..n], &hex[..hex_len], &mut resp) {
            Final::Ok => {
                ok_parts += 1;
                unsafe { MR = MR.wrapping_add(1) };
                crate::log("[rild] 分段 ");
                sys::print_dec(part as u64);
                crate::log(" 已发送\n");
            }
            Final::Cms(code) => {
                crate::log("[rild] +CMS ERROR: ");
                sys::print_dec(code as u64);
                crate::log("\n");
                break;
            }
            _ => {
                crate::log("[rild] 发送分段失败\n");
                break;
            }
        }
    }

    if ok_parts == parts {
        // 全部成功：写入发件箱
        unsafe {
            RIL.sms_id = RIL.sms_id.wrapping_add(1);
        }
        let id = unsafe { RIL.sms_id };
        let mut stamp = [0u8; 16];
        let stamp_len = now_stamp(&mut stamp);
        append_record(OUTBOX_PATH, id, num, &stamp[..stamp_len], 1, text);
        unsafe {
            RIL.outbox = count_records(OUTBOX_PATH);
            RIL.sms_state = SmsState::Sent;
            RIL.sms_info_len = 0;
            RIL.seq = RIL.seq.wrapping_add(1);
            DIRTY = true;
        }
        crate::log("[rild] 短信已发送\n");
    } else {
        set_info("短信发送失败".as_bytes());
        crate::log("[rild] 短信发送失败\n");
    }
}

fn write_dec(buf: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    if v == 0 {
        // v==0 也要产出 "0"（缺 i-=1 会写出空串，AT+CMGR 缺索引）。
        i -= 1;
        tmp[i] = b'0';
    } else {
        while v > 0 && i > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    // 有界：调用方缓冲尾可能小于完整十进制长度（cmd[8..] 仅 8 字节，
    // idx ≥ 10^8 时 copy_from_slice 长度不匹配 panic）。
    let n = (tmp.len() - i).min(buf.len());
    buf[..n].copy_from_slice(&tmp[i..i + n]);
    n
}

// ==================== 短信接收 ====================

/// +CMTI 触发的接收：AT+CMGR 取 PDU → 解码 → 级联重组 → 落盘。
fn receive_sms(idx: u32) {
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    let mut cmd = [0u8; 16];
    cmd[..8].copy_from_slice(b"AT+CMGR=");
    let n = 8 + write_dec(&mut cmd[8..], idx);
    if at.command(&cmd[..n], &mut resp) != Final::Ok {
        crate::log("[rild] CMGR 失败\n");
        return;
    }
    // 取 PDU hex 行：第二行（+CMGR 头之后）
    let mut lines = resp.split(|&b| b == b'\n');
    let _hdr = lines.next();
    let pdu_hex = lines.next().unwrap_or(&[]);
    let pdu_hex = pdu_hex.trim_ascii();
    if pdu_hex.is_empty() {
        crate::log("[rild] CMGR 空 PDU\n");
        return;
    }
    let mut pdu_buf = [0u8; 200];
    let pdu_len = from_hex(pdu_hex, &mut pdu_buf);
    let d = match pdu::deliver_decode(&pdu_buf[..pdu_len]) {
        Some(d) => d,
        None => {
            crate::log("[rild] PDU 解码失败\n");
            return;
        }
    };
    // 删除调制解调器存储中的该条
    let mut del = [0u8; 16];
    del[..8].copy_from_slice(b"AT+CMGD=");
    let dn = 8 + write_dec(&mut del[8..], idx);
    let mut r2 = [0u8; 64];
    let _ = at.command(&del[..dn], &mut r2);

    if d.concat_total > 1 {
        // 级联重组（d 被移动进重组表，先拷出元数据）
        let num_copy = d.oa;
        let num_len = d.oa_len;
        let scts_copy = d.scts;
        let cref = d.concat_ref;
        let ctotal = d.concat_total;
        let complete = concat_add(d);
        if !complete {
            crate::log("[rild] 拼接分段已缓冲\n");
            return;
        }
        // 完成：取重组文本（total 以槽内为准）
        let text = concat_text(cref as u8, ctotal as u8);
        let _ = (num_copy, num_len, scts_copy, text);
        let text = match text {
            Some(t) => t,
            None => return,
        };
        store_inbox(&num_copy[..num_len], &scts_copy, text);
    } else {
        store_inbox(&d.oa[..d.oa_len], &d.scts, &d.text[..d.text_len]);
    }
}

/// 把一段级联短信存入重组表；全部到齐返回 true。
fn concat_add(d: pdu::Deliver) -> bool {
    unsafe {
        let slot = find_concat_slot(d.concat_ref as u8);
        if slot.is_none() {
            return false;
        }
        let s = slot.unwrap();
        let part = d.concat_part as usize;
        // 槽的 total 来自消息本身（首段到达时设置）
        if s.total == 0 {
            s.total = (d.concat_total as usize).min(CONCAT_MAX_PARTS) as u8;
        }
        if part < 1 || part > CONCAT_MAX_PARTS || part as u8 > s.total {
            return false;
        }
        if s.parts[part - 1] != 0 {
            return false; // 重复段：忽略（不视为完成）
        }
        s.parts[part - 1] = 1;
        s.oa_len = d.oa_len;
        s.oa[..s.oa_len].copy_from_slice(&d.oa[..d.oa_len]);
        s.scts.copy_from_slice(&d.scts);
        s.lens[part - 1] = d.text_len as u16;
        let base = (part - 1) * 200;
        s.buf[base..base + d.text_len].copy_from_slice(&d.text[..d.text_len]);
        // 检查是否全部到齐
        let mut n = 0;
        for i in 0..s.total as usize {
            if s.parts[i] != 0 {
                n += 1;
            }
        }
        n == s.total as usize
    }
}

fn find_concat_slot(ref_id: u8) -> Option<&'static mut ConcatSlot> {
    unsafe {
        // 已存在同 ref 的槽
        for i in 0..CONCAT_SLOTS {
            if CONCAT[i].active && CONCAT[i].ref_id == ref_id {
                return Some(&mut *core::ptr::addr_of_mut!(CONCAT[i]));
            }
        }
        // 新建（超时槽由主循环清理）
        for i in 0..CONCAT_SLOTS {
            if !CONCAT[i].active {
                CONCAT[i].active = true;
                CONCAT[i].ref_id = ref_id;
                CONCAT[i].total = 0;
                CONCAT[i].started_us = sys::now_us();
                CONCAT[i].parts = [0; CONCAT_MAX_PARTS];
                CONCAT[i].oa_len = 0;
                return Some(&mut *core::ptr::addr_of_mut!(CONCAT[i]));
            }
        }
        None
    }
}

/// 取重组完成后的完整文本。按 (ref_id, total) 匹配槽位：只按 total 匹配会
/// 在两个同段数并发级联会话（如两条 3 段短信）间拿错槽位，完成的消息永远
/// 拼不齐且槽位不释放。
fn concat_text(ref_id: u8, total: u8) -> Option<&'static [u8]> {
    unsafe {
        for i in 0..CONCAT_SLOTS {
            if CONCAT[i].active && CONCAT[i].ref_id == ref_id && CONCAT[i].total == total {
                let s = &CONCAT[i];
                let mut full = [0u8; 1600];
                let mut pos = 0;
                for p in 0..total as usize {
                    if s.parts[p] == 0 {
                        return None;
                    }
                    let len = s.lens[p] as usize;
                    let base = p * 200;
                    full[pos..pos + len].copy_from_slice(&s.buf[base..base + len]);
                    pos += len;
                }
                // 拷贝到静态文本缓冲返回
                let out = &mut *core::ptr::addr_of_mut!(CONCAT_TEXT);
                out[..pos].copy_from_slice(&full[..pos]);
                CONCAT[i].active = false;
                return Some(&out[..pos]);
            }
        }
        None
    }
}

static mut CONCAT_TEXT: [u8; 1600] = [0; 1600];

/// 超时清理未完成的级联槽（主循环周期调用）。
///
/// 槽位登记时间戳用单调时钟近似：CONCAT_SLOTS 有限（4），半途而废的级联
/// 会话不清理会永久占用槽位，之后所有多段短信（find_concat_slot 失败）
/// 都被丢弃 —— 多段短信功能整体失效。
pub fn concat_gc() {
    const CONCAT_SLOT_TTL_US: u64 = 6_000_000; // 6 秒内收不齐即放弃
    unsafe {
        let now = sys::now_us();
        for i in 0..CONCAT_SLOTS {
            if CONCAT[i].active {
                let age = now.wrapping_sub(CONCAT[i].started_us);
                if age > CONCAT_SLOT_TTL_US {
                    CONCAT[i].active = false;
                }
            }
        }
    }
}

fn store_inbox(num: &[u8], _scts: &[u8], text: &[u8]) {
    unsafe {
        RIL.sms_id = RIL.sms_id.wrapping_add(1);
    }
    let id = unsafe { RIL.sms_id };
    let mut stamp = [0u8; 16];
    let stamp_len = now_stamp(&mut stamp);
    append_record(INBOX_PATH, id, num, &stamp[..stamp_len], 0, text);
    unsafe {
        RIL.inbox = count_records(INBOX_PATH);
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
    crate::log("[rild] 收到来自 ");
    sys::print_bytes(num);
    crate::log("\n");
}

// ==================== 短信持久化 ====================

/// 记录格式：KXSMS1<id>\x1F<num>\x1F<stamp>\x1F<read>\x1F<text>

/// 存储读入缓冲上限（字节）：约 120 条记录。rewrite/count 分块读全文件，
/// 超出部分不在删除/标记已读时保留（追加路径不受影响）。
const SMS_STORE_CAP: usize = 32768;

fn append_record(path: &str, id: u32, num: &[u8], stamp: &[u8], read: u8, text: &[u8]) {
    unsafe {
        let fd = sys::open(path, sys::O_RDWR | sys::O_CREAT);
        if fd < 0 {
            crate::log("[rild] 无法打开短信存储\n");
            return;
        }
        // ★ 纯追加写：此前「读 ≤8KB + 整文件重写」，文件超过 8KB 后每次
        // 追加都把尾部旧记录截掉（约 30 条后最旧短信静默丢失）。改为校验
        // 头部后 lseek 到文件末尾直接追加，历史记录完整保留。
        sys::syscall3(sys::SYS_LSEEK, fd as usize, 0, sys::SEEK_SET);
        let mut head = [0u8; 8];
        let hn = sys::read_fd(fd as usize, &mut head);
        let hn = if hn > 0 { hn as usize } else { 0 };
        if hn == 0 || !head[..hn.min(6)].starts_with(b"KXSMS1") {
            sys::syscall3(sys::SYS_LSEEK, fd as usize, 0, sys::SEEK_SET);
            sys::write_fd(fd as usize, b"KXSMS1\n");
        }
        // 只拼装本条新记录：<id>\x1F<num>\x1F<stamp>\x1F<read>\x1F<text>
        let mut out = [0u8; 300];
        let mut pos = 0;
        {
            let mut tmp = [0u8; 40];
            let dn = write_dec(&mut tmp, id);
            out[pos..pos + dn].copy_from_slice(&tmp[..dn]);
            pos += dn;
        }
        out[pos] = 0x1F;
        pos += 1;
        let nl = num.len().min(24);
        out[pos..pos + nl].copy_from_slice(&num[..nl]);
        pos += nl;
        out[pos] = 0x1F;
        pos += 1;
        let sl = stamp.len().min(16);
        out[pos..pos + sl].copy_from_slice(&stamp[..sl]);
        pos += sl;
        out[pos] = 0x1F;
        pos += 1;
        out[pos] = b'0' + read;
        pos += 1;
        out[pos] = 0x1F;
        pos += 1;
        let tl = text.len().min(200).min(out.len().saturating_sub(pos + 1));
        // 文本中的换行替换为空格（保持行格式）
        for i in 0..tl {
            out[pos + i] = if text[i] == b'\n' || text[i] == b'\r' { b' ' } else { text[i] };
        }
        pos += tl;
        out[pos] = b'\n';
        pos += 1;
        sys::syscall3(sys::SYS_LSEEK, fd as usize, 0, sys::SEEK_END);
        sys::write_fd(fd as usize, &out[..pos]);
        sys::syscall1(sys::SYS_CLOSE, fd as usize);
    }
}

/// 分块读入整个文件（上限 32KB），返回读入字节数。
///
/// 存储改为追加写后文件会持续增长，单次 read 只拿前 8KB 的旧读法会让
/// 统计/重写静默截掉尾部记录。超过 SMS_STORE_CAP 的部分不读入。
unsafe fn read_whole_file(fd: usize, buf: &mut [u8]) -> usize {
    let mut total = 0usize;
    while total < buf.len() {
        let k = sys::read_fd(fd, &mut buf[total..]);
        let k = k as isize;
        if k <= 0 {
            break;
        }
        total += k as usize;
    }
    total
}

/// 统计文件中的记录数（行数 - 1 头）。
fn count_records(path: &str) -> u32 {
    unsafe {
        let fd = sys::open(path, sys::O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let mut buf = [0u8; SMS_STORE_CAP];
        let n = read_whole_file(fd as usize, &mut buf);
        sys::syscall1(sys::SYS_CLOSE, fd as usize);
        if n == 0 {
            return 0;
        }
        if !buf[..n].starts_with(b"KXSMS1") {
            return 0;
        }
        let mut count = 0u32;
        for &b in &buf[..n] {
            if b == b'\n' {
                count += 1;
            }
        }
        count.saturating_sub(1)
    }
}

/// SMS_DEL:<id> — 从收件箱删除记录。
pub fn delete_sms(id: u32) {
    rewrite_store(INBOX_PATH, id, true);
}

/// SMS_READ:<id> — 标记已读。
pub fn mark_read(id: u32) {
    rewrite_store(INBOX_PATH, id, false);
}

/// 重写收件箱：删除（或标记已读）指定 id 的记录。
fn rewrite_store(path: &str, id: u32, delete: bool) {
    unsafe {
        // 分块读入整个文件（SMS_STORE_CAP 上限）：旧实现单次 read 8KB，
        // 文件超 8KB 后重写即截掉尾部记录（与追加写配套修复）
        let mut buf = [0u8; SMS_STORE_CAP];
        let fd = sys::open(path, sys::O_RDWR | sys::O_CREAT);
        if fd < 0 {
            return;
        }
        sys::syscall3(sys::SYS_LSEEK, fd as usize, 0, sys::SEEK_SET);
        let n = read_whole_file(fd as usize, &mut buf);
        if n == 0 || !buf[..n].starts_with(b"KXSMS1") {
            sys::syscall1(sys::SYS_CLOSE, fd as usize);
            return;
        }
        let mut out = [0u8; SMS_STORE_CAP];
        out[..6].copy_from_slice(b"KXSMS1\n");
        let mut p = 7;
        // 逐行处理
        let mut line_start = 7usize;
        while line_start < n {
            let line_end = match buf[line_start..n].iter().position(|&b| b == b'\n') {
                Some(x) => line_start + x,
                None => n,
            };
            let line = &buf[line_start..line_end];
            // out 容量守卫：输入接近 SMS_STORE_CAP 时保留写出空间
            if p + line.len() + 1 > out.len() {
                break;
            }
            // 解析 id（首字段）
            let id_end = line.iter().position(|&b| b == 0x1F).unwrap_or(line.len());
            let rec_id = at::parse_num(&line[..id_end]).unwrap_or(0);
            let mut keep = true;
            if rec_id == id {
                if delete {
                    keep = false;
                } else {
                    // 标记已读：第 4 字段
                    let mut new_line = [0u8; 300];
                    let mut q = 0;
                    let mut field = 0;
                    let mut copied = 0usize;
                    for &b in line {
                        if b == 0x1F {
                            field += 1;
                            new_line[q] = b;
                            q += 1;
                        } else if field == 3 && b == b'0' {
                            new_line[q] = b'1';
                            q += 1;
                        } else {
                            new_line[q] = b;
                            q += 1;
                        }
                        copied += 1;
                        if copied > 299 {
                            break;
                        }
                    }
                    out[p..p + q].copy_from_slice(&new_line[..q]);
                    p += q;
                    out[p] = b'\n';
                    p += 1;
                    keep = false; // 已写入，跳过原行
                }
            }
            if keep {
                let l = line.len().min(299);
                out[p..p + l].copy_from_slice(&line[..l]);
                p += l;
                out[p] = b'\n';
                p += 1;
            }
            line_start = line_end + 1;
        }
        sys::syscall3(sys::SYS_LSEEK, fd as usize, 0, sys::SEEK_SET);
        sys::write_fd(fd as usize, &out[..p]);
        sys::syscall1(sys::SYS_CLOSE, fd as usize);
        RIL.inbox = count_records(INBOX_PATH);
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
}

// ==================== 通讯录集成 ====================

/// 从 /data/contacts.dat 加载联系人缓存（name\x1Fnum 行）。
pub fn load_contacts() {
    unsafe {
        let fd = sys::open(CONTACTS_PATH, sys::O_RDONLY);
        if fd < 0 {
            CONTACTS_N = 0;
            return;
        }
        let mut buf = [0u8; 2048];
        let n = sys::read_fd(fd as usize, &mut buf);
        sys::syscall1(sys::SYS_CLOSE, fd as usize);
        if n <= 0 {
            CONTACTS_N = 0;
            return;
        }
        let n = n as usize;
        CONTACTS_N = 0;
        let mut start = 0usize;
        while start < n && CONTACTS_N < CONTACTS.len() {
            let end = match buf[start..n].iter().position(|&b| b == b'\n') {
                Some(x) => start + x,
                None => n,
            };
            let line = &buf[start..end];
            let l = line.len().min(63);
            CONTACTS[CONTACTS_N][..l].copy_from_slice(&line[..l]);
            CONTACTS[CONTACTS_N][l] = 0;
            CONTACTS_N += 1;
            start = end + 1;
        }
    }
}

/// 号码 → 联系人姓名（无匹配返回 None）。数字归一化比较（忽略空格/横线）。
pub fn contact_name(num: &[u8]) -> Option<&'static [u8]> {
    unsafe {
        let mut digits_a = [0u8; 24];
        let mut na = 0;
        for &b in num {
            if b.is_ascii_digit() && na < 24 {
                digits_a[na] = b;
                na += 1;
            }
        }
        for i in 0..CONTACTS_N {
            let line = &CONTACTS[i];
            let sep = line.iter().position(|&b| b == 0x1F || b == 0);
            if let Some(sep) = sep {
                if sep > 0 && line[sep] == 0x1F {
                    let name = &line[..sep];
                    let number = &line[sep + 1..];
                    let number_end = number.iter().position(|&b| b == 0).unwrap_or(number.len());
                    let mut digits_b = [0u8; 24];
                    let mut nb = 0;
                    for &b in &number[..number_end] {
                        if b.is_ascii_digit() && nb < 24 {
                            digits_b[nb] = b;
                            nb += 1;
                        }
                    }
                    if na > 0 && na == nb && digits_a[..na] == digits_b[..nb] {
                        return Some(name);
                    }
                }
            }
        }
        None
    }
}

// ==================== 状态文件 ====================

/// 写 /var/run/rild.state（应用轮询读取）。
pub fn write_state() {
    unsafe {
        let mut buf = [0u8; 512];
        let mut p = 0;
        macro_rules! line {
            ($k:expr, $($v:expr),*) => {{
                let k = $k;
                let mut tmp = [0u8; 64];
                let mut q = 0;
                for &b in k {
                    if q < 63 { tmp[q] = b; q += 1; }
                }
                tmp[q] = b':'; q += 1;
                $(for &b in $v { if q < 63 { tmp[q] = b; q += 1; } })*
                tmp[q] = b'\n'; q += 1;
                if p + q <= buf.len() {
                    buf[p..p + q].copy_from_slice(&tmp[..q]);
                    p += q;
                }
            }};
        }
        let call: &[u8] = match RIL.call {
            CallState::Idle => b"IDLE",
            CallState::Dialing => b"DIALING",
            CallState::Ringing => b"RINGING",
            CallState::Active => b"ACTIVE",
        };
        let dir: &[u8] = match RIL.dir {
            Dir::Out => b"OUT",
            Dir::In => b"IN",
            Dir::None => b"-",
        };
        let sim: &[u8] = match RIL.sim {
            SimState::Ready => b"READY",
            SimState::Absent => b"ABSENT",
            SimState::Unknown => b"UNKNOWN",
        };
        let sms: &[u8] = match RIL.sms_state {
            SmsState::Idle => b"IDLE",
            SmsState::Sending => b"SENDING",
            SmsState::Sent => b"SENT",
            SmsState::Failed => b"FAILED",
        };
        // 号码 + 姓名
        let mut numline = [0u8; 72];
        let mut nq = 0;
        for &b in &RIL.num[..RIL.num_len] {
            if nq < 70 { numline[nq] = b; nq += 1; }
        }
        if nq < 70 {
            numline[nq] = 0x1F;
            nq += 1;
        }
        if let Some(name) = contact_name(&RIL.num[..RIL.num_len]) {
            for &b in name {
                if nq < 70 { numline[nq] = b; nq += 1; }
            }
        }
        let _ = numline;
        // 直接用两行输出 NUM 与 NAME
        line!(b"V", b"1");
        line!(b"SIM", sim);
        line!(b"SIG", &fmt_dec(RIL.signal as u32));
        line!(b"OP", &RIL.op[..RIL.op_len]);
        line!(b"REG", &fmt_dec(RIL.reg as u32));
        line!(b"CALL", call);
        line!(b"DIR", dir);
        line!(b"NUM", &RIL.num[..RIL.num_len]);
        line!(b"NAME", contact_name(&RIL.num[..RIL.num_len]).unwrap_or(b""));
        line!(b"START", &fmt_dec((RIL.call_start / 1_000_000) as u32));
        line!(b"MISSED", if RIL.missed { b"1" } else { b"0" });
        line!(b"MISSNUM", &RIL.miss_num[..RIL.miss_num_len]);
        line!(b"SMS", sms);
        line!(b"SMSINFO", &RIL.sms_info[..RIL.sms_info_len]);
        line!(b"INBOX", &fmt_dec(RIL.inbox));
        line!(b"OUTBOX", &fmt_dec(RIL.outbox));
        line!(b"SMSC", &RIL.smsc[..RIL.smsc_len]);
        line!(b"ROUTE", &fmt_dec(audio_route_now()));
        line!(b"SEQ", &fmt_dec(RIL.seq));

        let fd = sys::open(STATE_PATH, sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC);
        if fd >= 0 {
            sys::write_fd(fd as usize, &buf[..p]);
            sys::syscall1(sys::SYS_CLOSE, fd as usize);
        }
        DIRTY = false;
        LAST_STATE_WRITE = sys::now_us();
    }
}

/// 十进制格式化到静态缓冲。
fn fmt_dec(v: u32) -> [u8; 16] {
    let mut tmp = [0u8; 16];
    let mut i = 16;
    if v == 0 {
        tmp[15] = b'0';
    } else {
        let mut x = v;
        while x > 0 && i > 0 {
            i -= 1;
            tmp[i] = b'0' + (x % 10) as u8;
            x /= 10;
        }
    }
    let n = 16 - i;
    let mut out = [0u8; 16];
    out[..n].copy_from_slice(&tmp[i..]);
    out
}

// ==================== 主循环辅助 ====================

/// 周期刷新 SIM 状态（支持热插拔与测试注入的 SIM:READY/ABSENT）。
pub fn refresh_sim() {
    let mut resp = [0u8; at::RESP_CAP];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    let new_sim = match at.command(b"AT+CPIN?", &mut resp) {
        Final::Ok => SimState::Ready,
        Final::Cme(10) => SimState::Absent,
        _ => SimState::Unknown,
    };
    unsafe {
        if RIL.sim != new_sim {
            RIL.sim = new_sim;
            RIL.seq = RIL.seq.wrapping_add(1);
            DIRTY = true;
            crate::log("[rild] SIM 状态 → ");
            crate::log(match new_sim {
                SimState::Ready => "READY\n",
                SimState::Absent => "ABSENT\n",
                _ => "UNKNOWN\n",
            });
        }
    }
}

/// 主循环周期任务：状态文件刷新、SIM 轮询与联系人缓存刷新。
pub fn periodic() {
    unsafe {
        let now = sys::now_us();
        // 联系人缓存：每 10 秒刷新（contacts 应用可能已写入新数据）
        if now.saturating_sub(LAST_CONTACTS_LOAD) > 10_000_000 {
            LAST_CONTACTS_LOAD = now;
            load_contacts();
        }
        // SIM 状态：每 3 秒刷新（虚拟调制解调器注入 / 真机热插拔）
        if now.saturating_sub(LAST_SIM_POLL) > 3_000_000 {
            LAST_SIM_POLL = now;
            refresh_sim();
        }
        // 状态文件：dirty 或每 2 秒强制刷新
        if DIRTY || now.saturating_sub(LAST_STATE_WRITE) > 2_000_000 {
            write_state();
        }
        concat_gc();
    }
}

/// 命令：SET_SMSC:<num>
pub fn set_smsc(num: &[u8]) {
    unsafe {
        RIL.smsc_len = num.len().min(RIL.smsc.len());
        RIL.smsc[..RIL.smsc_len].copy_from_slice(&num[..RIL.smsc_len]);
        RIL.seq = RIL.seq.wrapping_add(1);
        DIRTY = true;
    }
    let mut resp = [0u8; 64];
    let at = unsafe { &mut *core::ptr::addr_of_mut!(AT) };
    let mut cmd = [0u8; 48];
    cmd[..9].copy_from_slice(b"AT+CSCA=\"");
    let n = 9 + unsafe { RIL.smsc_len }.min(30);
    cmd[9..n].copy_from_slice(&num[..n - 9]);
    cmd[n] = b'"';
    let _ = at.command(&cmd[..n + 1], &mut resp);
    set_info("短信中心已设置".as_bytes());
}
