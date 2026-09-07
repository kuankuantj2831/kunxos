// KunxOS lifd：应用生命周期守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 kunxinit 在 launcher 之前拉起。职责（设计见 docs/lifecycle.md）：
//   1. 应用状态机：Running（前台）→ Background（后台，SIGSTOP）→
//      Suspended（挂起，超时/内存压力）→ Terminated（LMK/退出）
//   2. 控制面：/data/life 文件信箱（reg_* 登记 / cmd_<pid> 命令 /
//      evt_<pid> 应用事件 / launcher_evt 启动器事件）
//   3. 后台任务白名单审核（Audio/Location/Voip/Download + 时长上限）
//   4. 内存压力（SYS_LC_MEMPRESSURE 轮询）→ 挂起/LMK（LRU+优先级击杀）
//   5. 与 pushd 协作投递本地通知（DELIVER 命令；挂起/终止应用延迟投递）
//   6. 与 CFS 集成：前台 nice -3 / 后台 nice +2（setpriority）
//
// 信号用法：SIGSTOP(19) 停止应用（内核置 Stopped，直到 SIGCONT）；
// SIGCONT(18) 恢复；SIGKILL(9) 强制终止（内核 process::terminate 对
// 停止态进程同样生效）。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_GETDENTS64: usize = 61;
const SYS_LSEEK: usize = 62;
const SYS_UNLINKAT: usize = 35;
const SYS_MKDIRAT: usize = 34;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETPID: usize = 172;
const SYS_KILL: usize = 129;
const SYS_SETPRIORITY: usize = 140;
const SYS_GETTIMEOFDAY: usize = 169;
/// 自定义：内核内存压力等级（0/1/2）。
const SYS_LC_MEMPRESSURE: usize = 404;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

// 信号号。
const SIGSTOP: usize = 19;
const SIGCONT: usize = 18;
const SIGKILL: usize = 9;
const SIGTERM: usize = 15;

// ==================== 路径与协议常量 ====================

const LC_DIR: &str = "/data/life";
const EVT_PREFIX: &str = "/data/life/evt_";
const CMD_PREFIX: &str = "/data/life/cmd_";
const REG_PREFIX: &str = "/data/life/reg_";
const LAUNCHER_EVT: &str = "/data/life/launcher_evt";
const RECENTS_FILE: &str = "/data/life/recents";
const STATE_DIR: &str = "/data/state";
const PUSH_DIR: &str = "/data/push";
const PUSH_PENDING: &str = "/data/push/pending";

/// 注册表容量（MAX_PROC=16：init/networkd/lifd/pushd/launcher + 应用）。
const MAX_APPS: usize = 8;
/// 主循环轮询间隔（ms）。
const POLL_MS: usize = 50;
/// 后台化宽限窗：PAUSE/SUSPEND 回调执行 + 状态保存的时间（µs）。
const BG_GRACE_US: u64 = 150_000;
/// 后台 → 挂起超时（无后台任务时）。默认 60s；测试/运维可经
/// 命令信箱 `SET_TIMEOUT <ms>` 覆盖（见 `handle_cmd`）。
static mut BG_TIMEOUT_US: u64 = 60_000_000;
/// LMK 终止宽限（µs，当前实现直接 SIGKILL，保留常量供未来 TERMINATE 事件）。
const LMK_GRACE_US: u64 = 80_000;

/// 后台任务类型（白名单）。
const BT_AUDIO: u8 = 0;
const BT_LOCATION: u8 = 1;
const BT_VOIP: u8 = 2;
const BT_DOWNLOAD: u8 = 3;
/// 各类型时长上限（ms）。
const BT_CAP_MS: [u32; 4] = [3_600_000, 1_800_000, 7_200_000, 1_800_000];
/// 每应用最大后台任务数。
const MAX_TASKS_PER_APP: usize = 2;
/// 全系统可同时持有后台任务的应用数上限。
const MAX_TASK_APPS: usize = 3;

/// 应用状态。
const ST_RUNNING: u8 = 0;
const ST_BACKGROUND: u8 = 1;
const ST_SUSPENDED: u8 = 2;

/// 应用注册表项。
struct App {
    pid: u32,
    name: [u8; 24],
    name_len: usize,
    state: u8,
    last_used_us: u64,
    /// 活动后台任务数。
    tasks: usize,
    /// 各任务类型。
    task_type: [u8; MAX_TASKS_PER_APP],
    /// 各任务到期时刻（µs）。
    task_expire: [u64; MAX_TASKS_PER_APP],
}

const EMPTY_APP: App = App {
    pid: 0,
    name: [0; 24],
    name_len: 0,
    state: ST_RUNNING,
    last_used_us: 0,
    tasks: 0,
    task_type: [0; MAX_TASKS_PER_APP],
    task_expire: [0; MAX_TASKS_PER_APP],
};

static mut APPS: [App; MAX_APPS] = [EMPTY_APP; MAX_APPS];
/// 最近一次内存压力等级（0/1/2）。
static mut PRESSURE: u8 = 0;
/// 自进程 pid（写 cmd 信箱用；lifd 自身不投递命令，保留）。
static mut SELF_PID: u32 = 0;
/// 事件/文件内容缓冲（单线程主循环，无并发）。
static mut BUF: [u8; 256] = [0; 256];

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
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, in("x1") a2, options(nostack));
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

// ==================== 基础 I/O ====================

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
    print("[lifd] ");
    print(s);
    print("\n");
}

/// 无换行日志前缀：多段消息（前缀 + 名字/pid + 结尾）必须整行输出 ——
/// log() 自带的换行会把「已登记 <名> pid=<n> FG」劈成两行（第二行丢失
/// [lifd] 前缀，且自动化测试的连续子串匹配失败）。
fn log_raw(s: &str) {
    print("[lifd] ");
    print(s);
}

fn log2(s: &str, v: u64) {
    print("[lifd] ");
    print(s);
    print_dec(v);
    print("\n");
}

/// 墙钟时间（µs）。失败返回 0。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        let r = syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        if (r as isize) < 0 {
            return 0;
        }
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec =
            u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
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

fn exit(code: usize) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    loop {}
}

// ==================== 文件操作 ====================

/// openat 封装（路径拷入静态缓冲，NUL 结尾）。
fn sys_open(path: &str, flags: usize) -> isize {
    let mut p = [0u8; 96];
    let n = path.len().min(95);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    p[n] = 0;
    unsafe { syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, flags) as isize }
}

/// 读整个文件到 `out`，返回字节数（≤ out.len()）；失败/空返回 0。
fn read_file(path: &str, out: &mut [u8]) -> usize {
    let fd = sys_open(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let r = unsafe { syscall3(SYS_READ, fd as usize, out.as_mut_ptr() as usize, out.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    // 读失败 -1（usize 天文数字）按 isize 判负：否则把错误当成功长度，
    // 用上一条命令的陈旧 BUF 内容继续执行
    let r = r as isize;
    if r <= 0 {
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

/// 追加写（O_CREAT|O_APPEND 语义由内核 seek 到尾部实现）。
fn append_file(path: &str, data: &[u8]) -> bool {
    let fd = sys_open(path, O_WRONLY | O_CREAT | 0x400); // O_APPEND
    if fd < 0 {
        return false;
    }
    let r = unsafe { syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len()) };
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
    (r as isize) == data.len() as isize
}

fn unlink(path: &str) {
    let mut p = [0u8; 96];
    let n = path.len().min(95);
    p[..n].copy_from_slice(&path.as_bytes()[..n]);
    p[n] = 0;
    unsafe {
        syscall3(SYS_UNLINKAT, AT_FDCWD, p.as_ptr() as usize, 0);
    }
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

/// 十进制数解析（前导空白忽略）。返回 (值, 消费字节数)。
fn parse_dec(s: &[u8]) -> (u64, usize) {
    let mut i = 0usize;
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
        i += 1;
    }
    let mut v: u64 = 0;
    let mut any = false;
    while i < s.len() && s[i] >= b'0' && s[i] <= b'9' {
        v = v.wrapping_mul(10).wrapping_add((s[i] - b'0') as u64);
        any = true;
        i += 1;
    }
    if any { (v, i) } else { (0, 0) }
}

/// 按空白切分字段（最多 4 个），返回字段数。
/// 去掉前端空白（DELIVER 载荷中 pid 数字后的空格）。
fn trim_spaces(s: &[u8]) -> &[u8] {
    let mut i = 0usize;
    while i < s.len() && s[i] == b' ' {
        i += 1;
    }
    &s[i..]
}

fn split_fields<'a>(s: &'a [u8], out: &mut [&'a [u8]; 4]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < s.len() && n < out.len() {
        while i < s.len() && (s[i] == b' ' || s[i] == b'\n' || s[i] == b'\r' || s[i] == b'\t') {
            i += 1;
        }
        if i >= s.len() {
            break;
        }
        let start = i;
        while i < s.len() && s[i] != b' ' && s[i] != b'\n' && s[i] != b'\r' && s[i] != b'\t' {
            i += 1;
        }
        out[n] = &s[start..i];
        n += 1;
    }
    n
}

// ==================== 注册表 ====================

fn find_by_pid(pid: u32) -> Option<usize> {
    unsafe {
        for i in 0..MAX_APPS {
            if APPS[i].pid == pid {
                return Some(i);
            }
        }
    }
    None
}

fn find_free() -> Option<usize> {
    unsafe {
        for i in 0..MAX_APPS {
            if APPS[i].pid == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn entry_name(e: &App) -> &[u8] {
    &e.name[..e.name_len]
}

/// 删除注册表项并清理其事件文件。
fn remove_entry(idx: usize) {
    unsafe {
        let pid = APPS[idx].pid;
        if pid != 0 {
            let mut p = [0u8; 96];
            let n = build_evt_path(pid, &mut p);
            unlink(core::str::from_utf8(&p[..n]).unwrap_or(""));
            // 清理延迟推送
            let mut pp = [0u8; 96];
            let m = build_pending_path(pid, &mut pp);
            unlink(core::str::from_utf8(&pp[..m]).unwrap_or(""));
        }
        APPS[idx] = EMPTY_APP;
    }
}

/// 进程是否存活（kill(pid,0)）。
fn alive(pid: u32) -> bool {
    let r = unsafe { syscall2(SYS_KILL, pid as usize, 0) };
    (r as isize) >= 0
}

// ==================== 路径构建 ====================

fn build_evt_path(pid: u32, out: &mut [u8; 96]) -> usize {
    let mut n = 0usize;
    copy_str(out, &mut n, EVT_PREFIX);
    push_dec_to(out, &mut n, pid as u64);
    n
}

fn build_cmd_path(pid: u32, out: &mut [u8; 96]) -> usize {
    let mut n = 0usize;
    copy_str(out, &mut n, CMD_PREFIX);
    push_dec_to(out, &mut n, pid as u64);
    n
}

fn build_reg_path(name: &[u8], out: &mut [u8; 96]) -> usize {
    let mut n = 0usize;
    copy_str(out, &mut n, REG_PREFIX);
    let k = name.len().min(48);
    out[n..n + k].copy_from_slice(&name[..k]);
    n += k;
    n
}

fn build_pending_path(pid: u32, out: &mut [u8; 96]) -> usize {
    let mut n = 0usize;
    copy_str(out, &mut n, PUSH_PENDING);
    out[n] = b'/';
    n += 1;
    push_dec_to(out, &mut n, pid as u64);
    n
}

fn copy_str(out: &mut [u8], n: &mut usize, s: &str) {
    let b = s.as_bytes();
    let k = b.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&b[..k]);
    *n += k;
}

fn copy_bytes(out: &mut [u8], n: &mut usize, s: &[u8]) {
    let k = s.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&s[..k]);
    *n += k;
}

fn push_dec_to(out: &mut [u8], n: &mut usize, mut v: u64) {
    if v == 0 {
        if *n < out.len() {
            out[*n] = b'0';
            *n += 1;
        }
        return;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let d = tmp.len() - i;
    if *n + d <= out.len() {
        out[*n..*n + d].copy_from_slice(&tmp[i..]);
        *n += d;
    }
}

// ==================== 事件投递 ====================

/// 向应用写事件（覆盖式：最新事件优先，生命周期事件幂等）。
fn send_evt(pid: u32, evt: &[u8]) {
    let mut p = [0u8; 96];
    let n = build_evt_path(pid, &mut p);
    write_file(core::str::from_utf8(&p[..n]).unwrap_or(""), evt);
}

/// 向 launcher 写事件。
fn send_launcher(evt: &[u8]) {
    write_file(LAUNCHER_EVT, evt);
}

/// 更新最近任务文件（launcher 读取）。
fn update_recents() {
    let mut content = [0u8; 512];
    let mut n = 0usize;
    unsafe {
        for i in 0..MAX_APPS {
            let e = &APPS[i];
            if e.pid == 0 {
                continue;
            }
            let name = entry_name(e);
            let k = name.len().min(16);
            content[n..n + k].copy_from_slice(&name[..k]);
            n += k;
            if n < content.len() {
                content[n] = b' ';
                n += 1;
            }
            push_dec_to(&mut content, &mut n, e.pid as u64);
            if n < content.len() {
                content[n] = b' ';
                n += 1;
            }
            let st = match e.state {
                ST_RUNNING => b"R",
                ST_BACKGROUND => b"B",
                _ => b"S",
            };
            content[n] = st[0];
            n += 1;
            if n < content.len() {
                content[n] = b'\n';
                n += 1;
            }
        }
    }
    write_file(RECENTS_FILE, &content[..n]);
}

// ==================== 信号与调度 ====================

fn sig(pid: u32, s: usize) -> i64 {
    unsafe { syscall2(SYS_KILL, pid as usize, s) as i64 }
}

fn set_nice(pid: u32, prio: i32) {
    // setpriority(PRIO_PROCESS=0, pid, prio)：prio/4 → nice
    unsafe {
        syscall3(SYS_SETPRIORITY, 0, pid as usize, prio as usize);
    }
}

// ==================== 状态转换 ====================

/// 应用进入后台（Home / Recents / 系统请求）。
/// 流程：PAUSE/SUSPEND 事件 → 宽限窗（应用保存状态）→ SIGSTOP → 通知 launcher。
fn go_background(idx: usize, reason: &[u8]) {
    unsafe {
        let e = &mut APPS[idx];
        e.state = ST_BACKGROUND;
        e.last_used_us = now_us();
        let pid = e.pid;
        let name: [u8; 24] = e.name;
        let name_len = e.name_len;
        // 后台化：降低 CFS 份额（进程将被 SIGSTOP，权重影响恢复后瞬时）
        set_nice(pid, 8);
        // 1) 事件（应用在宽限窗内执行 on_pause/on_suspend + 状态保存）
        send_evt(pid, b"PAUSE SUSPEND");
        // 2) 宽限窗
        sleep_us(BG_GRACE_US);
        // 3) SIGSTOP（内核停止进程；若应用已退出则忽略）
        sig(pid, SIGSTOP);
        // 4) 通知 launcher 绘制桌面
        let mut evt = [0u8; 96];
        let mut n = 0usize;
        copy_str(&mut evt, &mut n, "BG ");
        let k = name_len.min(16);
        evt[n..n + k].copy_from_slice(&name[..k]);
        n += k;
        copy_str(&mut evt, &mut n, " ");
        push_dec_to(&mut evt, &mut n, pid as u64);
        copy_str(&mut evt, &mut n, " ");
        let k2 = reason.len().min(8);
        evt[n..n + k2].copy_from_slice(&reason[..k2]);
        n += k2;
        send_launcher(&evt[..n]);
        update_recents();
        log_raw("已转入后台 ");
        print(name_as_str(&name, name_len));
        print("\n");
    }
}

/// 应用回到前台（用户点击图标/最近任务卡片）。
/// 流程（launcher 先恢复帧快照）→ lifd：RESUME 事件（含待投递通知）→ SIGCONT。
fn go_foreground(idx: usize) {
    unsafe {
        let e = &mut APPS[idx];
        e.state = ST_RUNNING;
        e.last_used_us = now_us();
        let pid = e.pid;
        let name: [u8; 24] = e.name;
        let name_len = e.name_len;
        set_nice(pid, -12);
        // RESUME 事件 + 延迟推送
        let mut evt = [0u8; 256];
        let mut n = 0usize;
        copy_str(&mut evt, &mut n, "RESUME");
        // 补投挂起期间的通知（/data/push/pending/<pid>）
        let mut pp = [0u8; 96];
        let m = build_pending_path(pid, &mut pp);
        let got = read_file(core::str::from_utf8(&pp[..m]).unwrap_or(""), &mut BUF);
        if got > 0 && n + got + 1 <= evt.len() {
            evt[n] = b' ';
            n += 1;
            let k = got.min(evt.len() - n);
            evt[n..n + k].copy_from_slice(&BUF[..k]);
            n += k;
            unlink(core::str::from_utf8(&pp[..m]).unwrap_or(""));
        }
        send_evt(pid, &evt[..n]);
        // SIGCONT 恢复（内核 Stopped → Ready）
        sig(pid, SIGCONT);
        // 通知 launcher（其恢复动画/状态机）
        let mut levt = [0u8; 96];
        let mut l = 0usize;
        copy_str(&mut levt, &mut l, "FG ");
        let k = name_len.min(16);
        levt[l..l + k].copy_from_slice(&name[..k]);
        l += k;
        copy_str(&mut levt, &mut l, " ");
        push_dec_to(&mut levt, &mut l, pid as u64);
        send_launcher(&levt[..l]);
        update_recents();
        log_raw("已回前台 ");
        print(name_as_str(&name, name_len));
        print("\n");
    }
}

/// LMK / 系统终止：SIGKILL（内核 process::terminate 对停止态同样生效）。
fn terminate_app(idx: usize, reason: &[u8]) {
    unsafe {
        let e = &APPS[idx];
        let pid = e.pid;
        let name: [u8; 24] = e.name;
        let name_len = e.name_len;
        sig(pid, SIGKILL);
        // 通知 launcher 清理（若该应用是 launcher 的子进程，由 launcher 回收僵尸）
        let mut evt = [0u8; 96];
        let mut n = 0usize;
        copy_bytes(&mut evt, &mut n, reason);
        copy_str(&mut evt, &mut n, " ");
        let k = name_len.min(16);
        evt[n..n + k].copy_from_slice(&name[..k]);
        n += k;
        copy_str(&mut evt, &mut n, " ");
        push_dec_to(&mut evt, &mut n, pid as u64);
        send_launcher(&evt[..n]);
        remove_entry(idx);
        update_recents();
        log_raw("已终止 ");
        print(name_as_str(&name, name_len));
        print("\n");
    }
}

fn sleep_us(us: u64) {
    let ms = ((us + 999) / 1000) as usize;
    if ms > 0 {
        sleep_ms(ms);
    }
}

// ==================== 命令处理 ====================

/// 处理一条命令（来自应用/launcher/pushd 的 cmd_<pid> 文件内容）。
fn handle_cmd(pid: u32, s: &[u8]) {
    let mut f: [&[u8]; 4] = [&[]; 4];
    let n = split_fields(s, &mut f);
    if n == 0 {
        return;
    }
    match f[0] {
        b"BG" => {
            if let Some(i) = find_by_pid(pid) {
                go_background(i, b"HOME");
            }
        }
        b"RECENTS" => {
            if let Some(i) = find_by_pid(pid) {
                go_background(i, b"RECENTS");
            }
        }
        b"FG" => {
            // launcher 请求前台：FG <pid>
            if n >= 2 {
                let (tpid, _) = parse_dec(f[1]);
                if let Some(i) = find_by_pid(tpid as u32) {
                    go_foreground(i);
                }
            }
        }
        b"EXITED" => {
            // launcher 报告应用退出：EXITED <name> <pid>
            if n >= 3 {
                let (tpid, _) = parse_dec(f[2]);
                if let Some(i) = find_by_pid(tpid as u32) {
                    let e = unsafe { &APPS[i] };
                    log("应用已退出 ");
                    print(name_as_str(&e.name, e.name_len));
                    print("\n");
                    remove_entry(i);
                    update_recents();
                }
            }
        }
        b"BTASK" => {
            // 申请后台任务：BTASK <type> <duration_ms>
            if n >= 3 {
                let (ty, _) = parse_dec(f[1]);
                let (dur, _) = parse_dec(f[2]);
                grant_bg_task(pid, ty as u8, dur as u32);
            }
        }
        b"BTASK_END" => {
            if let Some(i) = find_by_pid(pid) {
                unsafe {
                    APPS[i].tasks = 0;
                    APPS[i].task_expire = [0; MAX_TASKS_PER_APP];
                }
                log("后台任务已结束");
            }
        }
        b"DELIVER" => {
            // pushd → 投递通知：DELIVER <target_pid> <id> <title>|<body>
            // 直接从原始串解析（split_fields 的 4 字段上限会截断含空格的
            // 标题/正文）：跳过 "DELIVER " 后取目标 pid，其余为
            // "<id> <title>|<body>"（deliver_notify 会补 "NOTIFY " 前缀）。
            let mut si = 0usize;
            while si < s.len() && s[si] != b' ' {
                si += 1;
            }
            if si < s.len() {
                let rest = &s[si + 1..];
                let (tpid, consumed) = parse_dec(rest);
                if tpid != 0 {
                    let payload = trim_spaces(&rest[consumed..]);
                    deliver_notify(tpid as u32, payload);
                }
            }
        }
        b"SET_TIMEOUT" => {
            // 测试/运维：SET_TIMEOUT <ms> 覆盖「后台 → 挂起」超时（供自动化
            // 测试快速验证超时挂起路径，不必等待 60s 默认值）。
            if n >= 2 {
                let (ms, _) = parse_dec(f[1]);
                unsafe {
                    BG_TIMEOUT_US = ms.saturating_mul(1000).max(1);
                }
                log2("bg->suspend 超时已设为 ", ms);
            }
        }
        _ => {}
    }
}

/// 后台任务审核与授予。
fn grant_bg_task(pid: u32, ty: u8, dur_ms: u32) {
    // 1) 类型白名单
    if ty > BT_DOWNLOAD {
        log("后台任务被拒: 未知类型");
        return;
    }
    let Some(i) = find_by_pid(pid) else {
        log("后台任务被拒: 未知应用");
        return;
    };
    unsafe {
        let e = &mut APPS[i];
        // 2) 每应用上限
        if e.tasks >= MAX_TASKS_PER_APP {
            log("后台任务被拒: 单应用数量已达上限");
            return;
        }
        // 3) 全系统上限（持有任务的应用数）
        let mut task_apps = 0usize;
        for j in 0..MAX_APPS {
            if APPS[j].pid != 0 && APPS[j].tasks > 0 {
                task_apps += 1;
            }
        }
        if task_apps >= MAX_TASK_APPS {
            log("后台任务被拒: 全系统数量已达上限");
            return;
        }
        // 4) 时长上限
        let cap = BT_CAP_MS[ty as usize];
        let dur = dur_ms.min(cap);
        let slot = e.tasks;
        e.task_type[slot] = ty;
        e.task_expire[slot] = now_us().saturating_add(dur as u64 * 1000);
        e.tasks += 1;
        // 持有任务的应用不被挂起超时转入 Suspended
        log_raw("后台任务已授予 type=");
        print_dec(ty as u64);
        print(" pid=");
        print_dec(pid as u64);
        print("\n");
    }
}

/// 通知投递：目标 Running → 立即写事件；否则延迟到恢复时补投。
///
/// payload 形如 "<id> <title>|<body>"（pushd 已拼好 "NOTIFY " 前缀之外部分）。
/// 延迟补投采用**最新覆盖**语义（与事件信箱一致）：多次通知只保留最新一条，
/// 恢复时随 RESUME 事件补投，避免多行累积导致应用侧单事件解析丢失。
fn deliver_notify(target_pid: u32, payload: &[u8]) {
    let mut evt = [0u8; 256];
    let mut n = 0usize;
    copy_str(&mut evt, &mut n, "NOTIFY ");
    let k = payload.len().min(evt.len() - n);
    evt[n..n + k].copy_from_slice(&payload[..k]);
    n += k;
    match find_by_pid(target_pid) {
        Some(i) => {
            let st = unsafe { APPS[i].state };
            if st == ST_RUNNING {
                send_evt(target_pid, &evt[..n]);
                log("通知已投递 pid=");
                print_dec(target_pid as u64);
                print("\n");
            } else {
                // 延迟补投（覆盖式：只保留最新一条）
                let mut pp = [0u8; 96];
                let m = build_pending_path(target_pid, &mut pp);
                write_file(core::str::from_utf8(&pp[..m]).unwrap_or(""), &evt[..n]);
                log_raw("通知已延迟 pid=");
                print_dec(target_pid as u64);
                print("\n");
            }
        }
        None => {
            // 应用不在注册表（未启动/已终止）：丢弃并记录（v1 简化）
            log_raw("通知已丢弃: 应用未登记 pid=");
            print_dec(target_pid as u64);
            print("\n");
        }
    }
}

// ==================== 内存压力 / LMK ====================

/// LMK 评分：越小越先杀。
/// score = 状态基准 + LRU 老化（秒）+ 后台任务罚分。
fn lmk_score(e: &App, now: u64) -> u64 {
    let base: u64 = match e.state {
        ST_SUSPENDED => 0,
        ST_BACKGROUND => 10,
        _ => 1000, // Running：不杀
    };
    let age = (now.saturating_sub(e.last_used_us) / 1_000_000).min(600) as u64; // 秒，封顶 10min
    let mut task_penalty = 0u64;
    for t in 0..e.tasks.min(MAX_TASKS_PER_APP) {
        task_penalty += 5 + e.task_type[t] as u64; // download(3)=8 … audio(0)=5
    }
    base + age + task_penalty
}

/// 内存压力响应（在 poll_pressure 中调用）。
fn on_pressure(level: u8) {
    unsafe {
        if level == PRESSURE {
            return;
        }
        PRESSURE = level;
    }
    match level {
        1 => {
            log("内存压力中等: 挂起空闲后台应用");
            // 无后台任务的 Background → Suspended（应用已停止，纯记账）
            let now = now_us();
            unsafe {
                for i in 0..MAX_APPS {
                    let e = &mut APPS[i];
                    if e.pid != 0 && e.state == ST_BACKGROUND && e.tasks == 0 {
                        e.state = ST_SUSPENDED;
                        log("  已挂起 ");
                        print(name_as_str(&e.name, e.name_len));
                        print("\n");
                    }
                }
            }
            let _ = now;
            update_recents();
        }
        2 => {
            log("内存压力严重: 正在执行 LMK");
            run_lmk();
        }
        _ => {
            log("内存压力正常");
        }
    }
}

/// LMK：按评分升序击杀，直到压力低于 Critical 或无候选。
fn run_lmk() {
    loop {
        let level = unsafe { syscall1(SYS_LC_MEMPRESSURE, 0) } as u8;
        if level < 2 {
            log("LMK: 压力已缓解");
            return;
        }
        // 选择最低评分候选
        let now = now_us();
        let mut best: Option<(usize, u64)> = None;
        unsafe {
            for i in 0..MAX_APPS {
                let e = &APPS[i];
                if e.pid == 0 || e.state == ST_RUNNING {
                    continue;
                }
                let s = lmk_score(e, now);
                if best.map(|(_, bs)| s < bs).unwrap_or(true) {
                    best = Some((i, s));
                }
            }
        }
        match best {
            Some((i, s)) => {
                log("LMK: 正在击杀 score=");
                print_dec(s);
                print("\n");
                terminate_app(i, b"KILLED");
                // 击杀后给 LMK 一点时间释放内存（页面释放由 launcher reap 时完成）
                sleep_ms(50);
            }
            None => {
                log("LMK: 无候选 (仅前台/系统)");
                return;
            }
        }
    }
}

// ==================== 轮询 ====================

/// 扫描 /data/life 目录处理 reg_*/cmd_* 文件。
fn poll_mailbox() {
    let mut raw = [0u8; 4096];
    let fd = sys_open(LC_DIR, O_RDONLY);
    if fd < 0 {
        return;
    }
    loop {
        let n = unsafe {
            syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len())
        };
        // 负 errno 按 isize 判负（usize::MAX 会进入垃圾目录项解析越界 panic）
        if (n as isize) <= 0 {
            break;
        }
        let mut off = 0usize;
        while off + 19 <= n as usize {
            let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
            if reclen < 19 || off + reclen > n as usize {
                break;
            }
            let name = &raw[off + 19..off + reclen];
            let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if elen >= 4 && name.starts_with(b"cmd_") {
                handle_cmd_file(&name[..elen]);
            } else if elen >= 4 && name.starts_with(b"reg_") {
                handle_reg_file(&name[..elen]);
            }
            off += reclen;
        }
    }
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
}

/// 处理单个 cmd_<pid> 文件：读取 → 执行 → 删除。
fn handle_cmd_file(fname: &[u8]) {
    // fname = "cmd_<pid>"
    let (pid, consumed) = parse_dec(&fname[4..]);
    if consumed == 0 {
        return;
    }
    let mut path = [0u8; 96];
    let mut n = 0usize;
    copy_str(&mut path, &mut n, CMD_PREFIX);
    // 去掉文件名自身的 "cmd_" 前缀（与 handle_reg_file 一致）：
    // CMD_PREFIX 已含 "/data/life/cmd_"，直接拼 fname 会变成
    // "/data/life/cmd_cmd_<pid>"（不存在）导致命令永远读不到。
    let name = &fname[4..];
    let k = name.len().min(60);
    path[n..n + k].copy_from_slice(&name[..k]);
    n += k;
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let got = read_file(p, unsafe { &mut BUF });
    // 先删除再处理：避免处理期间重复读到同一命令
    unlink(p);
    if got > 0 {
        handle_cmd(pid as u32, unsafe { &BUF[..got.min(BUF.len())] });
    }
}

/// 处理单个 reg_<name> 文件：读取 "pid FG|BG" → 登记/更新 → 删除。
fn handle_reg_file(fname: &[u8]) {
    // fname = "reg_<name>"
    let name = &fname[4..];
    if name.is_empty() {
        return;
    }
    let mut path = [0u8; 96];
    let mut n = 0usize;
    copy_str(&mut path, &mut n, REG_PREFIX);
    let k = name.len().min(60);
    path[n..n + k].copy_from_slice(&name[..k]);
    n += k;
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let got = read_file(p, unsafe { &mut BUF });
    unlink(p);
    if got == 0 {
        return;
    }
    let (pid, consumed) = parse_dec(unsafe { &BUF[..got] });
    if pid == 0 || consumed == 0 {
        return;
    }
    // 第二字段：FG（前台启动）或 BG
    let rest = unsafe { &BUF[consumed..got] };
    let is_fg = rest.iter().any(|&c| c == b'F');
    register_app(pid as u32, name, is_fg);
}

/// 登记新启动的应用（launcher 已 fork+exec）。
fn register_app(pid: u32, name: &[u8], fg: bool) {
    // 若 pid 已登记（重启同一应用？不，pid 唯一）——直接更新
    if let Some(i) = find_by_pid(pid) {
        unsafe {
            APPS[i].name = [0; 24];
            let k = name.len().min(24);
            APPS[i].name[..k].copy_from_slice(&name[..k]);
            APPS[i].name_len = k;
        }
    } else if let Some(i) = find_free() {
        unsafe {
            APPS[i] = EMPTY_APP;
            APPS[i].pid = pid;
            let k = name.len().min(24);
            APPS[i].name[..k].copy_from_slice(&name[..k]);
            APPS[i].name_len = k;
        }
    } else {
        log("登记失败: 注册表已满 (MAX_APPS)");
        return;
    }
    unsafe {
        let i = find_by_pid(pid).unwrap();
        let e = &mut APPS[i];
        e.last_used_us = now_us();
        e.state = if fg { ST_RUNNING } else { ST_BACKGROUND };
    }
    let i = find_by_pid(pid).unwrap();
    let e = unsafe { &APPS[i] };
    let nm = entry_name(e);
    log_raw("已登记 ");
    print(name_as_str(nm, nm.len()));
    print(" pid=");
    print_dec(pid as u64);
    print(if fg { " FG\n" } else { " BG\n" });
    if fg {
        set_nice(pid, -12);
        // 补投此前挂起的通知
        let mut pp = [0u8; 96];
        let m = build_pending_path(pid, &mut pp);
        let got = read_file(core::str::from_utf8(&pp[..m]).unwrap_or(""), unsafe { &mut BUF });
        if got > 0 {
            let mut evt = [0u8; 256];
            let mut n2 = 0usize;
            copy_str(&mut evt, &mut n2, "RESUME ");
            let k = got.min(evt.len() - n2);
            evt[n2..n2 + k].copy_from_slice(&unsafe { &BUF[..got] }[..k]);
            n2 += k;
            send_evt(pid, &evt[..n2]);
            unlink(core::str::from_utf8(&pp[..m]).unwrap_or(""));
        } else {
            // 新启动：首帧前无需 RESUME（应用自己绘制）；但为统一语义仍发
            send_evt(pid, b"RESUME");
        }
    }
    update_recents();
}

/// 检查超时：后台 → 挂起；后台任务到期。
fn poll_timeouts() {
    let now = now_us();
    unsafe {
        for i in 0..MAX_APPS {
            let e = &mut APPS[i];
            if e.pid == 0 {
                continue;
            }
            // 1) 后台任务到期
            for t in 0..e.tasks.min(MAX_TASKS_PER_APP) {
                if e.task_expire[t] != 0 && now >= e.task_expire[t] {
                    log_raw("后台任务已到期 pid=");
                    print_dec(e.pid as u64);
                    print("\n");
                    send_evt(e.pid, b"BG_TASK_EXPIRED");
                    // 移除到期任务
                    for u in t..e.tasks.saturating_sub(1) {
                        e.task_type[u] = e.task_type[u + 1];
                        e.task_expire[u] = e.task_expire[u + 1];
                    }
                    e.tasks = e.tasks.saturating_sub(1);
                    break;
                }
            }
            // 2) 后台 → 挂起（无任务且超时）
            if e.state == ST_BACKGROUND
                && e.tasks == 0
                && now.saturating_sub(e.last_used_us) >= BG_TIMEOUT_US
            {
                e.state = ST_SUSPENDED;
                log_raw("已挂起 ");
                print(name_as_str(&e.name, e.name_len));
                print("\n");
                update_recents();
            }
        }
    }
}

/// 检测已退出应用（kill(pid,0) 失效）并清理注册表。
fn poll_deaths() {
    unsafe {
        for i in 0..MAX_APPS {
            let e = &APPS[i];
            if e.pid == 0 || e.state == ST_RUNNING {
                continue; // Running 应用退出由 launcher wait4 回收 + EXITED 命令
            }
            if !alive(e.pid) {
                log_raw("应用已死亡 (后台) ");
                print(name_as_str(&e.name, e.name_len));
                print("\n");
                let mut evt = [0u8; 96];
                let mut n = 0usize;
                copy_str(&mut evt, &mut n, "EXITED ");
                let k = e.name_len.min(16);
                evt[n..n + k].copy_from_slice(&e.name[..k]);
                n += k;
                copy_str(&mut evt, &mut n, " ");
                push_dec_to(&mut evt, &mut n, e.pid as u64);
                send_launcher(&evt[..n]);
                remove_entry(i);
                update_recents();
            }
        }
    }
}

fn name_as_str(name: &[u8], len: usize) -> &str {
    core::str::from_utf8(&name[..len]).unwrap_or("?")
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[lifd] KunxOS 生命周期守护进程启动中\n");

    // 目录初始化
    mkdir("/data");
    mkdir("/data/life");
    mkdir(STATE_DIR);
    mkdir(PUSH_DIR);
    mkdir(PUSH_PENDING);
    // 清空残留事件/命令（上次异常退出的遗留）
    // （逐文件清理在 poll_mailbox 处理 reg_/cmd_ 时自动删除；evt_ 由注册清理）
    unsafe {
        SELF_PID = syscall1(SYS_GETPID, 0) as u32;
        PRESSURE = syscall1(SYS_LC_MEMPRESSURE, 0) as u8;
    }
    log2("pid=", unsafe { SELF_PID } as u64);
    log2("初始内存压力=", unsafe { PRESSURE } as u64);

    loop {
        poll_mailbox();
        poll_timeouts();
        poll_deaths();
        // 内存压力轮询（内核 tick 计算，syscall 读取）
        let level = unsafe { syscall1(SYS_LC_MEMPRESSURE, 0) } as u8;
        on_pressure(level);
        sleep_ms(POLL_MS);
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
        syscall3(SYS_WRITE, 2, b"PANIC: lifd\n".as_ptr() as usize, 12);
        if w.len > 0 {
            syscall3(SYS_WRITE, 2, w.buf.as_ptr() as usize, w.len);
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
