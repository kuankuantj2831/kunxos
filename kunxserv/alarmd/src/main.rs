// KunxOS alarmd：闹钟守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 kunxinit 拉起（pushd 已不再自动启动，见 kunxinit 头注释）。职责：
//   1. 监控 /data/alarms.txt（闹钟列表：一次性/每周重复、开关、标签），
//      内容校验和变化时自动重载。
//   2. 以墙钟（内核 RTC/PL031 UTC 纪元秒 + 本地时区 /data/oobe/timezone）
//      计算每个闹钟的下一次触发时刻；到点进入响铃状态。
//   3. 响铃：经 audiod（shared/audio.rs）播放双音蜂鸣铃声；同时把状态写入
//      /var/run/alarmd.state（RINGING/SNOOZED/IDLE），供时钟应用叠加响铃
//      界面（贪睡 / 停止）。
//   4. 命令信箱 /var/run/alarmd.ctl：
//        RELOAD            强制重载列表
//        SNOOZE <id>       贪睡 5 分钟
//        DISMISS <id>      停止并停用本次
//        STOPRING          立即停止响铃
//        TESTBEEP          测试铃声（短促双音）
//   5. 一次性闹钟响铃结束（停止/超时，且未贪睡）后自动置 on=0 并回写列表。
//
// 关键问题应答：
//   - 内核提供 PL031 RTC（QEMU virt），gettimeofday 返回 UTC 纪元时间；
//     Pi4 等无 RTC 平台退化为单调时钟。QEMU 掉电仅发生在进程退出时，
//     重启后 PL031 由宿主机时钟初始化，时间连续。
//   - QEMU 无系统休眠：CPU idle 为 wfi，CNTP/调度定时器周期性唤醒进程；
//     本守护以 10ms 轮询，闹钟到点可靠触发，无需唤醒机制。真机深度挂起
//     属 v2（见 docs/power_management.md §4）。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/audio.rs"]
mod audio;

#[path = "../../../shared/tz.rs"]
mod tz;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_MKDIRAT: usize = 34;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_NANOSLEEP: usize = 101;
const SYS_EXIT: usize = 93;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_APPEND: usize = 0x400;

// ==================== 路径与协议常量 ====================

const ALARM_LIST: &str = "/data/alarms.txt";
const STATE_PATH: &str = "/var/run/alarmd.state";
const CTL_PATH: &str = "/var/run/alarmd.ctl";

/// 最大闹钟数。
const MAX_ALARMS: usize = 16;
/// 主循环轮询间隔（ms，与 10ms 音频块对齐）。
const LOOP_MS: usize = 10;
/// 响铃最长时长（秒，超时自动静音；一次性闹钟随即停用）。
const RING_MAX_SECS: u64 = 60;
/// 贪睡时长（秒）。
const SNOOZE_SECS: u64 = 300;
/// 触发窗口（秒）：到点后 60s 内视为触发时段（防重复/丢失）。
const FIRE_WINDOW_SECS: u64 = 60;

// ==================== 数据结构 ====================

#[derive(Clone, Copy)]
struct Alarm {
    id: u32,
    on: bool,
    hh: u32,
    mm: u32,
    /// 周重复位掩码：bit0=周一 … bit6=周日；0 = 一次性。
    repeat: u8,
    label: [u8; 32],
    label_len: usize,
    /// 贪睡目标（epoch 秒；0 = 无）。
    snooze_until: u64,
}

impl Alarm {
    const fn empty() -> Alarm {
        Alarm {
            id: 0,
            on: false,
            hh: 8,
            mm: 0,
            repeat: 0,
            label: [0; 32],
            label_len: 0,
            snooze_until: 0,
        }
    }
}

static mut ALARMS: [Alarm; MAX_ALARMS] = [Alarm::empty(); MAX_ALARMS];
static mut ALARM_COUNT: usize = 0;
static mut LIST_HASH: u32 = 0;

/// 本地时区索引（TZDB，读 /data/oobe/timezone）。
static mut ZONE: usize = 0;

/// 响铃状态。
static mut RINGING: bool = false;
static mut RING_ID: u32 = 0;
static mut RING_FIRE_EPOCH: u64 = 0;
/// 本次响铃开始时刻（epoch 秒，用于超时自停）。
static mut RING_START_EPOCH: u64 = 0;
/// 响铃音频相位（块序号，驱动双音节奏）。
static mut RING_CHUNK: u32 = 0;
static mut BEEP_OPEN: bool = false;

/// 命令/内容缓冲。
static mut BUF: [u8; 512] = [0; 512];

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

fn print(s: &str) {
    unsafe {
        syscall3(SYS_WRITE, 1, s.as_ptr() as usize, s.len());
    }
}

fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        syscall3(SYS_WRITE, 1, buf[i..].as_ptr() as usize, buf.len() - i);
    }
}

fn log(s: &str) {
    print("[alarmd] ");
    print(s);
    print("\n");
}

fn log2(s: &str, v: u64) {
    print("[alarmd] ");
    print(s);
    print_dec(v);
    print("\n");
}

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

/// 当前墙钟（µs，失败 0）。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        let r = syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        if (r as isize) < 0 {
            return 0;
        }
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
}

fn now_epoch() -> u64 {
    now_us() / 1_000_000
}

// ==================== 文件操作 ====================

fn sys_open(path: &str, flags: usize) -> isize {
    let mut p = [0u8; 96];
    let n = path.len().min(95);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    p[n] = 0;
    unsafe { syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, flags) as isize }
}

fn read_file(path: &str, out: &mut [u8]) -> usize {
    let fd = sys_open(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let r = unsafe { syscall3(SYS_READ, fd as usize, out.as_mut_ptr() as usize, out.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    if (r as isize) <= 0 {
        0
    } else {
        (r as usize).min(out.len())
    }
}

/// 写整个文件（O_CREAT|O_TRUNC）。返回是否成功。
fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = sys_open(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return false;
    }
    let r = unsafe { syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    (r as isize) == data.len() as isize
}

fn mkdir(path: &str) {
    let mut p = [0u8; 96];
    let n = path.len().min(95);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    p[n] = 0;
    unsafe {
        syscall3(SYS_MKDIRAT, AT_FDCWD, p.as_ptr() as usize, 0o755);
    }
}

// ==================== 十进制解析 ====================

fn parse_dec(s: &[u8]) -> (u64, usize) {
    let mut i = 0usize;
    let mut v: u64 = 0;
    let mut any = false;
    while i < s.len() && s[i] >= b'0' && s[i] <= b'9' {
        v = v.wrapping_mul(10).wrapping_add((s[i] - b'0') as u64);
        any = true;
        i += 1;
    }
    if any { (v, i) } else { (0, 0) }
}

// ==================== 闹钟列表加载 ====================

/// 列表内容哈希（长度 + FNV），用于变化检测。
fn hash_content(s: &[u8]) -> u32 {
    let mut h: u32 = 0x811C9DC5;
    for &c in s {
        h ^= c as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h ^ (s.len() as u32).wrapping_mul(0x9E3779B9)
}

/// 解析一行闹钟：`id on hh mm repeat label...`。
fn parse_line(line: &[u8]) -> Option<Alarm> {
    let mut f: [&[u8]; 5] = [&[]; 5];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < line.len() && n < 5 {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
            i += 1;
        }
        f[n] = &line[start..i];
        n += 1;
    }
    if n < 5 {
        return None;
    }
    let (id, _) = parse_dec(f[0]);
    let (on, _) = parse_dec(f[1]);
    let (hh, _) = parse_dec(f[2]);
    let (mm, _) = parse_dec(f[3]);
    let (rep, _) = parse_dec(f[4]);
    if id == 0 || hh > 23 || mm > 59 {
        return None;
    }
    let mut a = Alarm::empty();
    a.id = id as u32;
    a.on = on != 0;
    a.hh = hh as u32;
    a.mm = mm as u32;
    a.repeat = (rep & 0x7F) as u8;
    // 标签 = 第五字段之后剩余（去行首空白与行尾 \r\n）。
    let mut li = i;
    while li < line.len() && (line[li] == b' ' || line[li] == b'\t') {
        li += 1;
    }
    let mut end = line.len();
    while end > li && (line[end - 1] == b'\r' || line[end - 1] == b'\n' || line[end - 1] == b' ') {
        end -= 1;
    }
    let k = (end - li).min(32);
    a.label[..k].copy_from_slice(&line[li..li + k]);
    a.label_len = k;
    Some(a)
}

/// 重载闹钟列表（先校验变化）。
fn reload_alarms() {
    let got = read_file(ALARM_LIST, unsafe { &mut BUF });
    let data = unsafe { &BUF[..got] };
    let h = hash_content(data);
    if h == unsafe { LIST_HASH } {
        return;
    }
    unsafe { LIST_HASH = h };
    let mut count = 0usize;
    let mut i = 0usize;
    while i < data.len() && count < MAX_ALARMS {
        let mut line_end = i;
        while line_end < data.len() && data[line_end] != b'\n' {
            line_end += 1;
        }
        let line = &data[i..line_end];
        if let Some(a) = parse_line(line) {
            unsafe {
                ALARMS[count] = a;
            }
            count += 1;
        }
        i = line_end + 1;
    }
    unsafe { ALARM_COUNT = count };
    log2("已重载闹钟，数量=", count as u64);
}

/// 回写列表（保留 id/on/标签），供一次性闹钟用完停用。
fn persist_alarms() {
    let mut line = [0u8; 96];
    let mut out = [0u8; 2048];
    let mut n = 0usize;
    for i in 0..unsafe { ALARM_COUNT } {
        let a = unsafe { &ALARMS[i] };
        let mut m = 0usize;
        push_dec(&mut line, &mut m, a.id as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, if a.on { 1 } else { 0 });
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.hh as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.mm as u64);
        line[m] = b' ';
        m += 1;
        push_dec(&mut line, &mut m, a.repeat as u64);
        line[m] = b' ';
        m += 1;
        let k = a.label_len.min(32);
        line[m..m + k].copy_from_slice(&a.label[..k]);
        m += k;
        line[m] = b'\n';
        m += 1;
        if n + m > out.len() {
            break;
        }
        out[n..n + m].copy_from_slice(&line[..m]);
        n += m;
    }
    write_file(ALARM_LIST, &out[..n]);
    unsafe { LIST_HASH = hash_content(&out[..n]) };
}

fn push_dec(out: &mut [u8], n: &mut usize, v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut x = v;
    if x == 0 {
        if *n < out.len() {
            out[*n] = b'0';
            *n += 1;
        }
        return;
    }
    while x > 0 {
        i -= 1;
        tmp[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    let d = tmp.len() - i;
    if *n + d <= out.len() {
        out[*n..*n + d].copy_from_slice(&tmp[i..]);
        *n += d;
    }
}

// ==================== 时区 ====================

fn load_zone() -> usize {
    let got = read_file("/data/oobe/timezone", unsafe { &mut BUF });
    let data = unsafe { &BUF[..got] };
    let mut end = 0usize;
    while end < data.len() && data[end] != b'\n' && data[end] != b'\r' && data[end] != b' ' {
        end += 1;
    }
    let name = core::str::from_utf8(&data[..end]).unwrap_or("");
    match tz::zone_by_name(name) {
        Some(z) => z,
        None => {
            log("未找到时区，默认 Asia/Shanghai");
            0
        }
    }
}

// ==================== 触发计算 ====================

/// 闹钟下次触发时刻（epoch 秒）。0 = 永不。
fn next_fire(a: &Alarm, zone: usize, now_ep: u64) -> u64 {
    if !a.on {
        return 0;
    }
    if a.snooze_until != 0 {
        return a.snooze_until;
    }
    let Some(lt) = tz::epoch_to_local(zone, now_ep as i64) else {
        return 0;
    };
    let tod = (lt.h * 3600 + lt.min * 60 + lt.s) as u64;
    let midnight = now_ep.saturating_sub(tod);
    let target = a.hh as u64 * 3600 + a.mm as u64 * 60;
    if a.repeat == 0 {
        if midnight + target > now_ep {
            return midnight + target;
        }
        return midnight + 86400 + target;
    }
    // 每周重复：找 [今天..+7天] 首个启用且未来的日子。
    for d in 0..9u64 {
        let day = midnight + d * 86400;
        let wd = (day / 86400 + 4).rem_euclid(7) as u8; // 0=周日
        let bit = ((wd + 6) % 7) as u8; // bit0=周一
        if (a.repeat >> bit) & 1 == 0 {
            continue;
        }
        let t = day + target;
        if d == 0 {
            if t > now_ep {
                return t;
            }
        } else {
            return t;
        }
    }
    0
}

/// 当前是否在闹钟触发窗口内。
fn is_due(a: &Alarm, zone: usize, now_ep: u64) -> bool {
    if !a.on {
        return false;
    }
    if a.snooze_until != 0 {
        return now_ep >= a.snooze_until;
    }
    let Some(lt) = tz::epoch_to_local(zone, now_ep as i64) else {
        return false;
    };
    let tod = (lt.h * 3600 + lt.min * 60 + lt.s) as u64;
    let midnight = now_ep.saturating_sub(tod);
    let target = midnight + a.hh as u64 * 3600 + a.mm as u64 * 60;
    if a.repeat != 0 {
        let wd = (target / 86400 + 4).rem_euclid(7) as u8;
        let bit = ((wd + 6) % 7) as u8;
        if (a.repeat >> bit) & 1 == 0 {
            return false;
        }
    }
    now_ep >= target && now_ep < target + FIRE_WINDOW_SECS
}

/// 全列表最近触发时刻（0 = 无）。
fn next_due(now_ep: u64) -> u64 {
    let mut best = 0u64;
    for i in 0..unsafe { ALARM_COUNT } {
        let a = unsafe { &ALARMS[i] };
        let t = next_fire(a, unsafe { ZONE }, now_ep);
        if t != 0 && (best == 0 || t < best) {
            best = t;
        }
    }
    best
}

/// 当前应响铃的闹钟索引（None = 无；多闹钟同时到点取最早目标）。
fn due_index(now_ep: u64) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for i in 0..unsafe { ALARM_COUNT } {
        let a = unsafe { &ALARMS[i] };
        if is_due(a, unsafe { ZONE }, now_ep) {
            let t = if a.snooze_until != 0 { a.snooze_until } else { now_ep };
            if best.map(|(_, bt)| t < bt).unwrap_or(true) {
                best = Some((i, t));
            }
        }
    }
    best.map(|(i, _)| i)
}

// ==================== 状态文件 ====================

fn copy_bytes(out: &mut [u8], n: &mut usize, s: &[u8]) {
    let k = s.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&s[..k]);
    *n += k;
}

fn write_state(s: &str) {
    write_file(STATE_PATH, s.as_bytes());
}

/// 更新状态文件：RINGING / SNOOZED / IDLE。
fn update_state(now_ep: u64) {
    unsafe {
        if RINGING {
            let mut s = [0u8; 64];
            let mut n = 0usize;
            copy_bytes(&mut s, &mut n, b"RINGING ");
            push_dec(&mut s, &mut n, RING_ID as u64);
            s[n] = b' ';
            n += 1;
            push_dec(&mut s, &mut n, RING_FIRE_EPOCH);
            write_file(STATE_PATH, &s[..n]);
            return;
        }
        // 贪睡中？
        let mut snoozing = 0u64;
        let mut sid = 0u32;
        for i in 0..ALARM_COUNT {
            let a = &ALARMS[i];
            if a.on && a.snooze_until != 0 {
                snoozing = a.snooze_until;
                sid = a.id;
                break;
            }
        }
        if snoozing != 0 {
            let mut s = [0u8; 64];
            let mut n = 0usize;
            copy_bytes(&mut s, &mut n, b"SNOOZED ");
            push_dec(&mut s, &mut n, sid as u64);
            s[n] = b' ';
            n += 1;
            push_dec(&mut s, &mut n, snoozing);
            write_file(STATE_PATH, &s[..n]);
            return;
        }
        let nd = next_due(now_ep);
        let mut s = [0u8; 64];
        let mut n = 0usize;
        copy_bytes(&mut s, &mut n, b"IDLE ");
        push_dec(&mut s, &mut n, nd);
        write_file(STATE_PATH, &s[..n]);
    }
}

// ==================== 铃声（audiod 双音蜂鸣） ====================

/// 铃声节奏周期：350ms 880Hz + 130ms 静音 + 350ms 1046Hz + 110ms 静音 ≈ 940ms。
const RING_CYCLE_CHUNKS: u32 = 94;
const SEG_A_GAP_END: u32 = 35; // [0,35) 段 A
const SEG_B_GAP_END: u32 = 48; // [35,48) 静音
const SEG_B_ON_END: u32 = 83; // [48,83) 段 B；[83,94) 静音
const FREQ_A: u32 = 880;
const FREQ_B: u32 = 1046;

static mut SINE: [i16; 256] = [0; 256];
static mut SINE_READY: bool = false;

fn build_sine() {
    unsafe {
        if SINE_READY {
            return;
        }
        let pi = core::f32::consts::PI;
        let two_pi = 2.0 * pi;
        for i in 0..256usize {
            let x = two_pi * i as f32 / 256.0;
            // 无 libm：多项式近似正弦（与 shared/audio 测试一致）。
            let mut xx = x % two_pi;
            let neg = xx > pi;
            if neg {
                xx -= pi;
            }
            if xx > pi / 2.0 {
                xx = pi - xx;
            }
            let num = 16.0 * xx * (pi - xx);
            let den = 5.0 * pi * pi - 4.0 * xx * (pi - xx);
            let mut s = if den != 0.0 { num / den } else { 0.0 };
            if neg {
                s = -s;
            }
            SINE[i] = (s * 26000.0) as i16;
        }
        SINE_READY = true;
    }
}

/// 合成一块 10ms 铃声（段首/段末各 5ms 淡变消咔嚓声）。
fn synth_ring_chunk(chunk: u32, out: &mut [u8]) {
    build_sine();
    out.fill(0);
    let off = chunk % RING_CYCLE_CHUNKS;
    let freq = if off < SEG_A_GAP_END {
        FREQ_A
    } else if off >= SEG_B_GAP_END && off < SEG_B_ON_END {
        FREQ_B
    } else {
        0
    };
    if freq == 0 {
        return;
    }
    let step = (freq as u64 * 256 / audio::SAMPLE_RATE as u64) as u32;
    if step == 0 {
        return;
    }
    let base = chunk as u64 * audio::CHUNK_FRAMES as u64 * step as u64;
    // 段内位置（用于淡入淡出）。
    let seg_start = if off < SEG_A_GAP_END {
        0
    } else {
        SEG_B_GAP_END
    };
    let seg_len = if off < SEG_A_GAP_END {
        SEG_A_GAP_END
    } else {
        SEG_B_ON_END - SEG_B_GAP_END
    };
    let fade = 240; // 5ms × 480 帧/s = 240 采样
    for i in 0..audio::CHUNK_FRAMES {
        let phase = (base + i as u64 * step as u64) & 255;
        let mut v = unsafe { SINE[phase as usize] as i32 };
        // 淡入淡出。
        let mut env = 32768;
        let pos = seg_start as i32 * audio::CHUNK_FRAMES as i32 + i as i32;
        if pos < fade {
            env = env * pos / fade;
        } else if pos >= seg_len as i32 * audio::CHUNK_FRAMES as i32 - fade {
            let remain = seg_len as i32 * audio::CHUNK_FRAMES as i32 - pos;
            env = env * remain.max(1) / fade;
        }
        v = v * env / 32768;
        let s = v.clamp(-32767, 32767) as i16;
        let o = i * 4;
        out[o] = (s as u16 & 0xFF) as u8;
        out[o + 1] = ((s as u16 >> 8) & 0xFF) as u8;
        out[o + 2] = (s as u16 & 0xFF) as u8;
        out[o + 3] = ((s as u16 >> 8) & 0xFF) as u8;
    }
}

/// 打开音频流并请求焦点（供铃声响）。
fn beep_open() {
    if unsafe { BEEP_OPEN } {
        return;
    }
    if audio::open_stream(audio::USAGE_MUSIC) == 0 {
        audio::request_focus();
        unsafe { BEEP_OPEN = true };
        log("铃声流已打开");
    } else {
        log("打开铃声流失败（audiod 未就绪）");
    }
}

/// 停止铃声：释放焦点、关流。
fn beep_close() {
    unsafe {
        if !BEEP_OPEN {
            return;
        }
        BEEP_OPEN = false;
    }
    audio::release_focus();
    audio::close_stream();
    log("铃声流已关闭");
}

/// 测试铃声：短促段 A 蜂鸣（300ms）后关闭。
fn beep_test() {
    if audio::open_stream(audio::USAGE_MUSIC) != 0 {
        log("测试蜂鸣失败（audiod 未就绪）");
        return;
    }
    audio::request_focus();
    let mut chunk = [0u8; audio::CHUNK_BYTES];
    for c in 0..30 {
        synth_ring_chunk(c, &mut chunk);
        let r = audio::write_chunk(&chunk);
        if r != 0 {
            break;
        }
        sleep_ms(10);
    }
    audio::release_focus();
    audio::close_stream();
    log("测试蜂鸣完成");
}

// ==================== 命令处理 ====================

fn find_index(id: u32) -> Option<usize> {
    for i in 0..unsafe { ALARM_COUNT } {
        if unsafe { ALARMS[i].id } == id {
            return Some(i);
        }
    }
    None
}

/// 读取并清空命令信箱（读后 ftruncate 清零；信箱文件常驻——启动即创建，
/// 若沿用「读后 unlink」语义，文件消失后主循环每次 open 都失败，串口被
/// `[vfs] open 失败` 刷屏）。逐行执行，与 lifd 信箱语义一致。
fn poll_ctl() {
    let fd = sys_open(CTL_PATH, O_RDWR);
    if fd < 0 {
        return;
    }
    let ctl = unsafe { &mut BUF };
    let got = unsafe { syscall3(SYS_READ, fd as usize, ctl.as_mut_ptr() as usize, 256) };
    unsafe { syscall2(SYS_FTRUNCATE, fd as usize, 0) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    if (got as isize) <= 0 {
        return;
    }
    let n = (got as usize).min(256);
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j < n && ctl[j] != b'\n' {
            j += 1;
        }
        let line = &ctl[i..j];
        if line.len() >= 2 {
            handle_cmd(line);
        }
        i = j + 1;
    }
}

fn handle_cmd(line: &[u8]) {
    let mut f: [&[u8]; 3] = [&[]; 3];
    let mut n = 0usize;
    let mut i = 0usize;
    while i < line.len() && n < 3 {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
            i += 1;
        }
        f[n] = &line[start..i];
        n += 1;
    }
    if n == 0 {
        return;
    }
    match f[0] {
        b"RELOAD" => {
            unsafe { LIST_HASH = 0 };
            reload_alarms();
            update_state(now_epoch());
        }
        b"SNOOZE" => {
            if n >= 2 {
                let (id, _) = parse_dec(f[1]);
                if let Some(idx) = find_index(id as u32) {
                    let now = now_epoch();
                    unsafe {
                        ALARMS[idx].snooze_until = now + SNOOZE_SECS;
                    }
                    log2("贪睡闹钟 id=", id as u64);
                    stop_ring();
                }
            }
        }
        b"DISMISS" => {
            if n >= 2 {
                let (id, _) = parse_dec(f[1]);
                dismiss_ring(id as u32);
            }
        }
        b"STOPRING" => {
            stop_ring();
        }
        b"TESTBEEP" => {
            beep_test();
        }
        _ => {}
    }
}

/// 开始响铃（idx 对应闹钟；多闹钟同时到点取最早目标，中途切换自动处理）。
fn start_ring(idx: usize) {
    unsafe {
        if RINGING {
            // 已在响：若新闹钟目标更早则切换。
            let a = &ALARMS[idx];
            let t = if a.snooze_until != 0 { a.snooze_until } else { now_epoch() };
            if a.id != RING_ID && t < RING_FIRE_EPOCH {
                RING_ID = a.id;
                RING_FIRE_EPOCH = t;
                RING_START_EPOCH = now_epoch();
                RING_CHUNK = 0;
            }
            return;
        }
        let a = &ALARMS[idx];
        RING_ID = a.id;
        RING_FIRE_EPOCH = if a.snooze_until != 0 {
            a.snooze_until
        } else {
            now_epoch()
        };
        RING_START_EPOCH = now_epoch();
        RING_CHUNK = 0;
        RINGING = true;
        // 贪睡目标已消费 → 后续按常规排程。
        ALARMS[idx].snooze_until = 0;
        log2("响铃闹钟 id=", a.id as u64);
        beep_open();
    }
    update_state(now_epoch());
}

/// 停止响铃；一次性闹钟且未在贪睡 → 停用并回写列表。
fn stop_ring() {
    unsafe {
        if !RINGING {
            beep_close();
            update_state(now_epoch());
            return;
        }
        RINGING = false;
        let id = RING_ID;
        RING_ID = 0;
        beep_close();
        if let Some(idx) = find_index(id) {
            let a = &mut ALARMS[idx];
            if a.on && a.repeat == 0 && a.snooze_until == 0 {
                a.on = false;
                log2("一次性闹钟已停用 id=", id as u64);
                persist_alarms();
            }
        }
        update_state(now_epoch());
    }
}

/// 用户明确停用（DISMISS）：清除贪睡并停止本次响铃。
fn dismiss_ring(id: u32) {
    if let Some(idx) = find_index(id) {
        unsafe {
            ALARMS[idx].snooze_until = 0;
        }
    }
    unsafe {
        if RINGING && RING_ID == id {
            stop_ring();
        } else {
            // 不在响铃（贪睡待响/已停）：仅刷新状态。
            update_state(now_epoch());
        }
    }
}

// ==================== 主流程 ====================

fn _unused() {}

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[alarmd] KunxOS 闹钟守护进程启动中\n");
    mkdir("/data");
    unsafe {
        ZONE = load_zone();
        LIST_HASH = 0;
    }
    log2("zone idx=", unsafe { ZONE } as u64);
    reload_alarms();
    // 命令信箱启动即创建（O_TRUNC 顺带清陈旧命令）：poll_ctl 以 O_RDWR
    // 打开，若等客户端首写才存在，主循环每次 open 都失败。
    write_file(CTL_PATH, b"");

    let mut tick = 0u64;
    loop {
        let now = now_epoch();
        poll_ctl();
        if tick % 40 == 0 {
            reload_alarms();
        }
        if unsafe { RINGING } {
            // 写铃声块 + 超时自停。
            if now.saturating_sub(unsafe { RING_START_EPOCH }) >= RING_MAX_SECS {
                log("响铃超时，自动停止");
                stop_ring();
            } else if let Some(idx) = due_index(now) {
                let id = unsafe { ALARMS[idx].id };
                if id != unsafe { RING_ID } {
                    start_ring(idx);
                } else {
                    let mut chunk = [0u8; audio::CHUNK_BYTES];
                    synth_ring_chunk(unsafe { RING_CHUNK }, &mut chunk);
                    let rw = audio::write_chunk(&chunk);
                    if rw != 0 {
                        // audiod 断开：重开。
                        beep_close();
                        beep_open();
                    }
                    unsafe { RING_CHUNK += 1 };
                }
            } else {
                stop_ring();
            }
        } else {
            if let Some(idx) = due_index(now) {
                start_ring(idx);
            }
            if tick % 20 == 0 {
                update_state(now);
            }
        }
        tick += 1;
        sleep_ms(LOOP_MS);
    }
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

// ==================== panic ====================

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：打印 panic 位置（文件:行:列），无堆、固定缓冲。
    struct PanicBuf { buf: [u8; 256], len: usize }
    impl core::fmt::Write for PanicBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = PanicBuf { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        syscall3(SYS_WRITE, 2, b"PANIC: alarmd\n".as_ptr() as usize, 14);
        if w.len > 0 {
            syscall3(SYS_WRITE, 2, w.buf.as_ptr() as usize, w.len);
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}