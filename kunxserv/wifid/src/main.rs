// KunxOS wifid：Wi-Fi 守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 职责：
//   1. 经 /dev/wifi（内核 kunxcore::wifi）下发命令并读取事件：
//      SCAN / CONNECT / DISCONNECT / STATUS；
//      事件：SCAN_RESULT / SCAN_DONE / CONNECTING / CONNECTED /
//            DISCONNECTED / HANDSHAKE_MSG / ERROR。
//   2. 状态机（任务要求的用户侧镜像）：Idle → Scanning → Associating →
//      Authenticating → Connected → Disconnected；ERROR 时按
//      CONNECT_RETRY_MAX 自动重连，耗尽后回到 Disconnected。
//   3. 配置存储：/data/wifi/networks/<ssid>.conf（ssid / psk / auth /
//      priority），口令明文存储（v1：任务允许明文或简单加密）。
//   4. 命令入口：/var/run/wifid.pipe（SCAN / CONNECT / SAVE / FORGET /
//      LIST / AUTO / STATUS / DISCONNECT），日志写入 /var/log/wifid.log。
//
// 说明：与 networkd 一致，本演示版本运行有限轮后退出（内核随后回退到
// 内置 shell；wifid 的业务逻辑 / 状态机 / 存储均在轮询循环中完成）。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]


use kunxwireless::devproto::{self as dp};

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_FSYNC: usize = 82;
const SYS_GETDENTS64: usize = 61;
const SYS_IOCTL: usize = 29;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

const SEEK_SET: usize = 0;

// ==================== 常量 ====================

/// 命令管道与日志路径。
const PIPE_PATH: &str = "/var/run/wifid.pipe";
const LOG_PATH: &str = "/var/log/wifid.log";
/// 配置存储目录（/data 由内核挂载在持久化 SFS 分区）。
const NET_DIR: &str = "/data/wifi/networks";
const NET_DIR_PARENT: &str = "/data/wifi";

/// 已保存网络数上限。
const NET_MAX: usize = 8;
/// 扫描结果表容量（内核缓存 16）。
const SCAN_MAX: usize = 16;
/// 握手错误自动重连次数上限。
const CONNECT_RETRY_MAX: u8 = 3;

/// 演示主循环轮数（每轮 250ms）。

// ==================== 基础 I/O ====================

use kunxrt::syscall::syscall1;

use kunxrt::syscall::syscall2;

use kunxrt::syscall::syscall3;

use kunxrt::syscall::syscall4;

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

/// 把字符串写入 fd 1（stdout）。
fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_bytes(b: &[u8]) {
    unsafe {
        write_fd(1, b);
    }
}

fn print_dec(mut v: usize) {
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
        write_fd(1, &buf[i..]);
    }
}

/// 打印有符号数值（rssi 等）。
fn print_signed(v: i64) {
    if v < 0 {
        print("-");
        let a = (-(v as i128)) as u128;
        print_uint(a);
    } else {
        print_uint(v as u128);
    }
}

fn print_uint(mut x: u128) {
    if x == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 40];
    let mut i = buf.len();
    while x > 0 {
        i -= 1;
        buf[i] = b'0' + (x % 10) as u8;
        x /= 10;
    }
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

/// 打印 IPv4（点分十进制）。
fn print_ip(ip: &[u8; 4]) {
    for (i, b) in ip.iter().enumerate() {
        if i > 0 {
            print(".");
        }
        print_dec(*b as usize);
    }
}

/// 打印 MAC。
fn print_mac(mac: &[u8; 6]) {
    let hex = b"0123456789abcdef";
    for (i, b) in mac.iter().enumerate() {
        if i > 0 {
            print(":");
        }
        print_bytes(&[hex[(b >> 4) as usize], hex[(b & 0xF) as usize]]);
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

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn sys_open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// mkdirat（路径不存在时静默成功——目录已存在是幂等安全场景）。
unsafe fn sys_mkdir(path: &str) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0o755) as isize
}

/// unlinkat（FORGET 用）。
unsafe fn sys_unlink(path: &str) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
}

fn log_line(s: &str) {
    unsafe {
        let fd = sys_open(LOG_PATH, O_CREAT | O_WRONLY | O_TRUNC);
        if fd >= 0 {
            write_fd(fd as usize, s.as_bytes());
            write_fd(fd as usize, b"\n");
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

/// 读整个文件到 `buf`，返回字节数（-1 出错）。
fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = sys_open(path, O_RDONLY);
        if fd < 0 {
            return -1;
        }
        let mut total = 0isize;
        loop {
            let n = syscall3(
                SYS_READ,
                fd as usize,
                buf.as_mut_ptr() as usize + total as usize,
                buf.len() - total as usize,
            );
            if n <= 0 {
                break;
            }
            total += n as isize;
            if total as usize >= buf.len() {
                break;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
        total
    }
}

/// 同目录改名：目标已存在时内核按 POSIX 语义原子替换。成功 0。
fn rename_path(old: &str, new: &str) -> isize {
    let mut o = [0u8; 128];
    let mut q = [0u8; 128];
    let on = old.len().min(127);
    o[..on].copy_from_slice(&old.as_bytes()[..on]);
    let nn = new.len().min(127);
    q[..nn].copy_from_slice(&new.as_bytes()[..nn]);
    unsafe {
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            o.as_ptr() as usize,
            AT_FDCWD,
            q.as_ptr() as usize,
        ) as isize
    }
}

/// 写整个文件（临时文件 + fsync + rename 原子提交）。
///
/// 就地 O_TRUNC 重写已保存网络的配置文件，掉电会留下半份 psk 丢失的
/// 配置；rename 覆盖后任一时刻磁盘上都是完整旧文件或完整新文件。
fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let mut tmp = [0u8; 128];
        let n = path.len().min(122);
        tmp[..n].copy_from_slice(&path.as_bytes()[..n]);
        tmp[n..n + 4].copy_from_slice(b".tmp");
        let tpath = core::str::from_utf8(&tmp[..n + 4]).unwrap_or("");
        let fd = sys_open(tpath, O_CREAT | O_WRONLY | O_TRUNC);
        if fd < 0 {
            return false;
        }
        let w = write_fd(fd as usize, data);
        syscall1(SYS_FSYNC, fd as usize);
        syscall1(SYS_CLOSE, fd as usize);
        if w != data.len() {
            return false;
        }
        if rename_path(tpath, path) == 0 {
            return true;
        }
        // 极小概率 EBUSY（读方恰好持有目标打开）：退回就地写
        let fd2 = sys_open(path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd2 < 0 {
            return false;
        }
        let w2 = write_fd(fd2 as usize, data);
        syscall1(SYS_CLOSE, fd2 as usize);
        w2 == data.len()
    }
}

// ==================== /dev/wifi 接入 ====================

static mut WIFI_FD: isize = -1;

/// 打开 /dev/wifi 字符设备。
fn wifi_open() -> bool {
    unsafe {
        WIFI_FD = sys_open("/dev/wifi", O_RDWR);
        WIFI_FD >= 0
    }
}

/// 写一条命令（首字节为 devproto 命令码）。
fn wifi_command(cmd: &[u8]) -> bool {
    unsafe {
        if WIFI_FD < 0 {
            return false;
        }
        write_fd(WIFI_FD as usize, cmd) == cmd.len()
    }
}

/// ioctl GET_STATE：读取 41 字节状态记录。
fn wifi_status(rec: &mut dp::WifiStateRecord) -> bool {
    unsafe {
        if WIFI_FD < 0 {
            return false;
        }
        let mut buf = [0u8; 64];
        let r = syscall4(
            SYS_IOCTL,
            WIFI_FD as usize,
            dp::WIFI_IOCTL_GET_STATE as usize,
            buf.as_mut_ptr() as usize,
            buf.len(),
        );
        // ioctl 失败返回 -1（usize 下为天文数字），必须按 isize 判负，
        // 否则错误状态被当成功、读到未初始化缓冲当真实状态
        if (r as isize) < 41 {
            return false;
        }
        *rec = unsafe { core::ptr::read(buf.as_ptr() as *const dp::WifiStateRecord) };
        true
    }
}

// ==================== 状态机（wifid 侧镜像） ====================

/// wifid 侧连接状态（任务要求：Idle→Scanning→Associating→Authenticating→
/// Connected→Disconnected）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WifiState {
    Idle,
    Scanning,
    Associating,
    Authenticating,
    Connected,
    Disconnected,
}

static mut STA_STATE: WifiState = WifiState::Idle;
/// 当前/最近关联的 SSID。
static mut CUR_SSID: [u8; 32] = [0; 32];
static mut CUR_SSID_LEN: u8 = 0;
/// 待连接目标（CONNECT 下发后保留，用于自动重连）。
static mut PEND_SSID: [u8; 32] = [0; 32];
static mut PEND_SSID_LEN: u8 = 0;
static mut PEND_PSK: [u8; 64] = [0; 64];
static mut PEND_PSK_LEN: u8 = 0;
static mut CONNECT_RETRY: u8 = 0;

fn state_name(s: WifiState) -> &'static str {
    match s {
        WifiState::Idle => "Idle",
        WifiState::Scanning => "Scanning",
        WifiState::Associating => "Associating",
        WifiState::Authenticating => "Authenticating",
        WifiState::Connected => "Connected",
        WifiState::Disconnected => "Disconnected",
    }
}

/// devproto 状态记录字节 → 可读名（0-idle 1-scanning 2-connecting 3-handshake 4-connected）。
fn record_state_name(s: u8) -> &'static str {
    match s {
        dp::WIFI_STATE_SCANNING => "Scanning",
        dp::WIFI_STATE_CONNECTING => "Associating",
        dp::WIFI_STATE_HANDSHAKE => "Authenticating",
        dp::WIFI_STATE_CONNECTED => "Connected",
        _ => "Idle/Disconnected",
    }
}

fn set_state(s: WifiState) {
    unsafe {
        if STA_STATE != s {
            print("[wifid] 状态: ");
            print(state_name(s));
            if s == WifiState::Connected {
                print(" ssid=");
                print_bytes(&CUR_SSID[..CUR_SSID_LEN as usize]);
            }
            print("\n");
            STA_STATE = s;
        }
    }
}

/// 下发 CONNECT（附带保存的目标，供出错自动重连）。
fn issue_connect(ssid: &[u8], psk: &[u8], auth: u8, auto_retry: bool) {
    if ssid.len() > 32 || psk.len() > 64 {
        print("[wifid] CONNECT 被拒绝: ssid<=32 psk<=64\n");
        return;
    }
    unsafe {
        PEND_SSID = [0; 32];
        PEND_PSK = [0; 64];
        PEND_SSID[..ssid.len()].copy_from_slice(ssid);
        PEND_SSID_LEN = ssid.len() as u8;
        PEND_PSK[..psk.len()].copy_from_slice(psk);
        PEND_PSK_LEN = psk.len() as u8;
        CONNECT_RETRY = 0;
    }
    let _ = auto_retry;
    // CONNECT 命令：ssid\0 psk\0 auth
    let mut cmd = [0u8; 1 + 32 + 1 + 64 + 1 + 1];
    cmd[0] = dp::WIFI_CMD_CONNECT;
    let mut pos = 1;
    cmd[pos..pos + ssid.len()].copy_from_slice(ssid);
    pos += ssid.len();
    pos += 1; // \0
    cmd[pos..pos + psk.len()].copy_from_slice(psk);
    pos += psk.len();
    pos += 1; // \0
    cmd[pos] = auth;
    wifi_command(&cmd[..pos + 1]);
    set_state(WifiState::Associating);
}

/// 重扫（SCAN 命令）。
fn issue_scan() {
    wifi_command(&[dp::WIFI_CMD_SCAN]);
    set_state(WifiState::Scanning);
}

// ==================== 配置存储（/data/wifi/networks） ====================

/// 一条已保存网络。
#[derive(Clone, Copy)]
struct NetEntry {
    active: bool,
    ssid: [u8; 32],
    ssid_len: u8,
    psk: [u8; 64],
    psk_len: u8,
    auth: u8,
    priority: u8,
}

static mut NETS: [NetEntry; NET_MAX] = [NetEntry {
    active: false,
    ssid: [0; 32],
    ssid_len: 0,
    psk: [0; 64],
    psk_len: 0,
    auth: 1,
    priority: 0,
}; NET_MAX];

fn net_conf_path<'a>(ssid: &[u8], buf: &'a mut [u8; 128]) -> &'a str {
    let mut out = [0u8; 128];
    let prefix = b"/data/wifi/networks/";
    // ssid 字节来自管道命令/扫描结果，可为任意非 UTF-8 序列：from_utf8_unchecked
    // 构造 &str 属于 UB。逐字节白名单净化（非字母数字/_- 替换为 '_'），
    // 结果必然是合法 UTF-8 且不含路径分隔符（顺带堵死 ../ 穿越）。
    let mut o = prefix.len();
    out[..o].copy_from_slice(prefix);
    let cap = (out.len() - 5).min(128);
    for &b in ssid.iter().take(80) {
        if o >= cap {
            break;
        }
        let ok = b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        out[o] = if ok { b } else { b'_' };
        o += 1;
    }
    let total = o + 5; // ".conf"
    out[o..total].copy_from_slice(b".conf");
    buf[..total].copy_from_slice(&out[..total]);
    // 净化后必为合法 UTF-8；仍走安全转换兜底
    core::str::from_utf8(&buf[..total]).unwrap_or("/data/wifi/networks/x.conf")
}

/// 解析一行 `key=value`，把 value 拷入 `dst`，返回长度。
fn parse_kv(line: &[u8], key: &[u8], dst: &mut [u8]) -> Option<usize> {
    let eq = line.iter().position(|&b| b == b'=')?;
    if &line[..eq] != key {
        return None;
    }
    let v = &line[eq + 1..];
    let n = v.len().min(dst.len());
    dst[..n].copy_from_slice(&v[..n]);
    Some(n)
}

fn parse_priority(line: &[u8], key: &[u8]) -> Option<u8> {
    let eq = line.iter().position(|&b| b == b'=')?;
    if &line[..eq] != key {
        return None;
    }
    let mut v = 0u32;
    for &b in &line[eq + 1..] {
        if b < b'0' || b > b'9' {
            break;
        }
        v = v * 10 + (b - b'0') as u32;
    }
    Some(v.min(255) as u8)
}

/// 从配置段加载一条网络。
fn load_network(ssid: &[u8]) -> bool {
    let mut path = [0u8; 128];
    let p = net_conf_path(ssid, &mut path);
    let mut fbuf = [0u8; 384];
    let n = read_file(p, &mut fbuf);
    if n < 0 {
        return false;
    }
    let data = &fbuf[..n as usize];
    let mut e = NetEntry {
        active: true,
        ssid: [0; 32],
        ssid_len: ssid.len().min(32) as u8,
        psk: [0; 64],
        psk_len: 0,
        auth: 1,
        priority: 0,
    };
    e.ssid[..e.ssid_len as usize].copy_from_slice(&ssid[..e.ssid_len as usize]);
    let mut ok = false;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(plen) = parse_kv(line, b"psk", &mut e.psk) {
            e.psk_len = plen.min(64) as u8;
            ok = true;
        }
        if let Some(a) = parse_priority(line, b"priority") {
            e.priority = a;
        }
        if line.starts_with(b"auth=") {
            let v = &line[5..];
            e.auth = if v.first() == Some(&b'0') { 0 } else { 1 };
        }
    }
    if !ok {
        return false;
    }
    unsafe {
        for s in NETS.iter_mut() {
            if !s.active {
                *s = e;
                return true;
            }
        }
    }
    false
}

/// 扫描配置目录（getdents64）并加载全部网络中已保存的条目。
fn load_all_networks() {
    unsafe {
        let dir = sys_open(NET_DIR, O_RDONLY);
        if dir < 0 {
            print("[wifid] 无网络配置目录，正在创建 ");
            print(NET_DIR);
            print("...\n");
            sys_mkdir(NET_DIR_PARENT);
            sys_mkdir(NET_DIR);
            return;
        }
        let mut buf = [0u8; 512];
        let mut count = 0usize;
        loop {
            let n = syscall3(
                SYS_GETDENTS64,
                dir as usize,
                buf.as_mut_ptr() as usize,
                buf.len(),
            );
            if (n as isize) <= 0 {
                break;
            }
            let n = n as usize;
            let mut off = 0usize;
            while off + 18 < n as usize {
                let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
                // reclen<19 或末记录越界：目录流异常，防御性 break（对齐
                // lifd/pushd 的解析守卫 —— 否则下方切片起点越过终点或越出
                // buf 边界直接 panic）。
                if reclen < 19 || off + reclen > n as usize {
                    break;
                }
                let name = &buf[off + 19..(off + reclen).min(off + 19 + 64)];
                // 跳过 . 与 ..（d_type=DT_DIR=4；DT_REG=8）
                let d_type = buf[off + 18];
                if d_type == 4 && name.len() <= 2 {
                    off += reclen;
                    continue;
                }
                let mut ssid = [0u8; 32];
                let mut slen = 0;
                for &b in name {
                    if b == b'.' {
                        break; // .conf 截断
                    }
                    if slen >= 32 {
                        break;
                    }
                    ssid[slen] = b;
                    slen += 1;
                }
                if slen > 0 && count < NET_MAX {
                    if load_network(&ssid[..slen]) {
                        count += 1;
                    }
                }
                off += reclen;
            }
        }
        syscall1(SYS_CLOSE, dir as usize);
        print("[wifid] 已加载 ");
        print_dec(count);
        print(" 个已保存网络\n");
    }
}

fn save_network(ssid: &[u8], psk: &[u8], auth: u8, priority: u8) -> bool {
    let mut path = [0u8; 128];
    let p = net_conf_path(ssid, &mut path);
    let mut text = [0u8; 32 + 1 + 64 + 1 + 64 + 1 + 64];
    let mut n = 0usize;
    n += write_kv(&mut text[n..], b"ssid", ssid);
    n += write_kv(&mut text[n..], b"psk", psk);
    n += write_dec_kv(&mut text[n..], b"auth", auth as usize);
    n += write_dec_kv(&mut text[n..], b"priority", priority as usize);
    text[n] = b'\n';
    n += 1;
    let ok = write_file(p, &text[..n]);
    if ok {
        // 内存表同步：wifid 常驻后 NETS 不会从磁盘重载 —— 同 ssid 必须就地
        // 更新（重新 SAVE 换 psk 才能生效），否则只有空闲槽能吸收新条目。
        let sl = ssid.len().min(32);
        unsafe {
            let mut e = NetEntry {
                active: true,
                ssid: [0; 32],
                ssid_len: sl as u8,
                psk: [0; 64],
                psk_len: psk.len().min(64) as u8,
                auth: if auth == 0 { 0 } else { 1 },
                priority,
            };
            e.ssid[..sl].copy_from_slice(&ssid[..sl]);
            e.psk[..e.psk_len as usize].copy_from_slice(&psk[..e.psk_len as usize]);
            let mut done = false;
            for s in NETS.iter_mut() {
                if s.active && s.ssid_len as usize == sl && s.ssid[..sl] == e.ssid[..sl] {
                    *s = e;
                    done = true;
                    break;
                }
            }
            if !done {
                for s in NETS.iter_mut() {
                    if !s.active {
                        *s = e;
                        break;
                    }
                }
            }
        }
    }
    ok
}

fn write_kv(dst: &mut [u8], key: &[u8], val: &[u8]) -> usize {
    // 有界写入：val 可含管道命令注入的任意长度（如 CONNECT <ssid> <psk>），
    // 超出剩余空间必须截断而非 copy_from_slice panic（守护进程被一个命令杀死）
    let need = key.len() + 1 + val.len() + 1;
    if need > dst.len() {
        // 至少保证 key= 与换行完整，val 按剩余空间截断
        let mut n = 0;
        if key.len() + 1 >= dst.len() {
            return 0;
        }
        dst[..key.len()].copy_from_slice(key);
        n += key.len();
        dst[n] = b'=';
        n += 1;
        let room = dst.len() - n - 1;
        dst[n..n + room].copy_from_slice(&val[..room]);
        dst[dst.len() - 1] = b'\n';
        return dst.len();
    }
    let mut n = 0;
    n += key.len();
    dst[..n].copy_from_slice(key);
    dst[n] = b'=';
    n += 1;
    dst[n..n + val.len()].copy_from_slice(val);
    n += val.len();
    dst[n] = b'\n';
    n += 1;
    n
}

fn write_dec_kv(dst: &mut [u8], key: &[u8], val: usize) -> usize {
    let mut d = [0u8; 20];
    let mut dn = 0;
    let mut v = val;
    if v == 0 {
        d[0] = b'0';
        dn = 1;
    } else {
        while v > 0 && dn < d.len() {
            d[dn] = b'0' + (v % 10) as u8;
            dn += 1;
            v /= 10;
        }
        d[..dn].reverse();
    }
    write_kv(dst, key, &d[..dn])
}

fn forget_network(ssid: &[u8]) -> bool {
    let mut path = [0u8; 128];
    let p = net_conf_path(ssid, &mut path);
    let ok = unsafe { sys_unlink(p) == 0 };
    // 内存表同步失活：此前只删文件 —— NETS 条目一旦加载永不变 inactive，
    // best_saved() 仍会选中「已忘记」的网络被 AUTO 开机自连；同 ssid 重新
    // SAVE 时 load_network 也只在空闲槽吸收新条目，旧 psk 一直留在内存。
    if ok {
        let sl = ssid.len().min(32);
        unsafe {
            for s in NETS.iter_mut() {
                if s.active && s.ssid_len as usize == sl && s.ssid[..sl] == ssid[..sl] {
                    s.active = false;
                }
            }
        }
    }
    ok
}

fn list_networks() {
    unsafe {
        let mut any = false;
        for e in NETS.iter() {
            if e.active {
                any = true;
                print("[wifid] 已保存: ");
                print_bytes(&e.ssid[..e.ssid_len as usize]);
                print(" auth=");
                print_dec(e.auth as usize);
                print(" prio=");
                print_dec(e.priority as usize);
                print("\n");
            }
        }
        if !any {
            print("[wifid] 没有已保存的网络\n");
        }
    }
}

/// 选优先级最高的已保存网络（并返回其 ssid/psk）。
fn best_saved() -> Option<usize> {
    unsafe {
        let mut best = NET_MAX;
        let mut bp = 0u8;
        for (i, e) in NETS.iter().enumerate() {
            if e.active && (best == NET_MAX || e.priority > bp) {
                best = i;
                bp = e.priority;
            }
        }
        if best == NET_MAX {
            None
        } else {
            Some(best)
        }
    }
}

// ==================== 命令处理（/var/run/wifid.pipe） ====================

/// 把一行拆成词元（空格分隔，存起始下标），返回词数。
fn tokenize(line: &[u8], starts: &mut [usize; 8], lens: &mut [usize; 8]) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;
    while i < line.len() && count < starts.len() {
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        starts[count] = i;
        let mut j = i;
        while j < line.len() && line[j] != b' ' {
            j += 1;
        }
        lens[count] = j - i;
        count += 1;
        i = j;
    }
    count
}

fn tokens_eq(line: &[u8], starts: &[usize; 8], lens: &[usize; 8], idx: usize, want: &[u8]) -> bool {
    if idx >= starts.len() {
        return false;
    }
    lens[idx] == want.len() && &line[starts[idx]..starts[idx] + lens[idx]] == want
}

fn handle_command(cmd: &[u8]) {
    let mut starts = [0usize; 8];
    let mut lens = [0usize; 8];
    let n = tokenize(cmd, &mut starts, &mut lens);
    if n == 0 {
        return;
    }
    if tokens_eq(cmd, &starts, &lens, 0, b"SCAN") {
        print("[wifid] 已请求扫描\n");
        issue_scan();
    } else if tokens_eq(cmd, &starts, &lens, 0, b"CONNECT") && n >= 3 {
        // CONNECT <ssid> <psk> [priority]
        let ssid = &cmd[starts[1]..starts[1] + lens[1]];
        let psk = &cmd[starts[2]..starts[2] + lens[2]];
        // 长度守卫必须在 save_network 之前：此前校验只存在于 issue_connect，
        // 超长 ssid/psk 先进保存路径（缓冲溢出类 panic）；管道命令可被任意
        // 进程写入。
        if ssid.len() > 32 || psk.len() > 64 {
            print("[wifid] CONNECT 被拒绝: ssid<=32 psk<=64\n");
            return;
        }
        let prio = if n >= 4 {
            let mut v = 0u8;
            let p = &cmd[starts[3]..starts[3] + lens[3]];
            for &b in p {
                if b < b'0' || b > b'9' {
                    break;
                }
                v = v.saturating_mul(10).saturating_add(b - b'0');
            }
            v
        } else {
            0
        };
        if save_network(ssid, psk, 1, prio) {
            print("[wifid] 已保存网络 ");
            print_bytes(ssid);
            print("\n");
            load_all_networks(); // 重载以包含新条目
        }
        print("[wifid] 正在连接 ");
        print_bytes(ssid);
        print("...\n");
        issue_connect(ssid, psk, 1, true);
    } else if tokens_eq(cmd, &starts, &lens, 0, b"SAVE") && n >= 3 {
        let ssid = &cmd[starts[1]..starts[1] + lens[1]];
        let psk = &cmd[starts[2]..starts[2] + lens[2]];
        if ssid.len() > 32 || psk.len() > 64 {
            print("[wifid] SAVE 被拒绝: ssid<=32 psk<=64\n");
            return;
        }
        let prio = if n >= 4 {
            let mut v = 0u8;
            let p = &cmd[starts[3]..starts[3] + lens[3]];
            for &b in p {
                if b < b'0' || b > b'9' {
                    break;
                }
                v = v.saturating_mul(10).saturating_add(b - b'0');
            }
            v
        } else {
            0
        };
        if save_network(ssid, psk, 1, prio) {
            print("[wifid] 已保存 ");
            print_bytes(ssid);
            print(" (psk 明文存储于 ");
            print(NET_DIR);
            print(")\n");
            load_all_networks();
        } else {
            print("[wifid] 保存失败\n");
        }
    } else if tokens_eq(cmd, &starts, &lens, 0, b"FORGET") && n >= 2 {
        let ssid = &cmd[starts[1]..starts[1] + lens[1]];
        if forget_network(ssid) {
            print("[wifid] 已删除 ");
            print_bytes(ssid);
            print("\n");
            load_all_networks();
        } else {
            print("[wifid] 删除失败 (未保存?)\n");
        }
    } else if tokens_eq(cmd, &starts, &lens, 0, b"LIST") {
        list_networks();
    } else if tokens_eq(cmd, &starts, &lens, 0, b"AUTO") {
        unsafe {
            if let Some(i) = best_saved() {
                let ssid = &NETS[i].ssid[..NETS[i].ssid_len as usize];
                let psk = &NETS[i].psk[..NETS[i].psk_len as usize];
                print("[wifid] 自动连接 ");
                print_bytes(ssid);
                print("\n");
                issue_connect(ssid, psk, NETS[i].auth, true);
            } else {
                print("[wifid] 无已保存网络；请先执行 SAVE <ssid> <psk>\n");
            }
        }
    } else if tokens_eq(cmd, &starts, &lens, 0, b"STATUS") {
        let mut rec = unsafe { core::mem::zeroed() };
        if wifi_status(&mut rec) {
            print("[wifid] 状态: ");
            print_dec(rec.state as usize);
            print(" (");
            print(record_state_name(rec.state));
            print(") ssid=");
            print_bytes(&rec.ssid[..rec.ssid_len as usize]);
            print(" bssid=");
            print_mac(&rec.bssid);
            print(" rssi=");
            print_signed(rec.rssi as i64);
            print("\n");
        } else {
            print("[wifid] 状态不可用\n");
        }
    } else if tokens_eq(cmd, &starts, &lens, 0, b"DISCONNECT") {
        wifi_command(&[dp::WIFI_CMD_DISCONNECT]);
        set_state(WifiState::Disconnected);
        print("[wifid] 已请求断开\n");
    } else {
        print("[wifid] 未知命令\n");
    }
}

/// 读取管道文件内容并逐行处理（rename 认领取走，同 networkd）。
///
/// 旧的「read → ftruncate 清空」两步之间写入方追加的命令会被 ftruncate
/// 一并抹掉；认领（pipe → pipe.r 原子改名）后新命令落在重生的管道文件
/// 上，留待下一轮。管道文件常驻（读后立即重建）。
fn poll_pipe() {
    static mut BUF: [u8; 1024] = [0u8; 1024];
    // 认领名 "/var/run/wifid.pipe.r"
    let mut taken = [0u8; 32];
    let cb = PIPE_PATH.as_bytes();
    taken[..cb.len()].copy_from_slice(cb);
    taken[cb.len()] = b'.';
    taken[cb.len() + 1] = b'r';
    let tpath = core::str::from_utf8(&taken[..cb.len() + 2]).unwrap_or("");
    // 清掉上次崩溃残留的认领文件（存在则 rename 失败）
    unsafe { syscall3(SYS_UNLINKAT, AT_FDCWD, taken.as_ptr() as usize, 0) };
    if rename_path(PIPE_PATH, tpath) != 0 {
        return; // 无命令文件
    }
    unsafe {
        let fd = sys_open(tpath, O_RDWR);
        let mut n: isize = 0;
        if fd >= 0 {
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
            n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len()) as isize;
            syscall1(SYS_CLOSE, fd as usize);
        }
        syscall3(SYS_UNLINKAT, AT_FDCWD, taken.as_ptr() as usize, 0);
        // 重建空管道文件（无 O_TRUNC：认领后写入方的新命令不截断）
        let fd2 = sys_open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd2 >= 0 {
            syscall1(SYS_CLOSE, fd2 as usize);
        }
        if n > 0 {
            // 防御：syscall 返回原始寄存器值，read 失败（-1）会被误读为
            // 天文数字，直接循环会越界 panic（实测发生：index 1024/1024）。
            // 超长返回值按缓冲上限截断，只处理已读入的部分。
            let m = if n <= BUF.len() as isize { n as usize } else { BUF.len() };
            let mut start = 0usize;
            for i in 0..m {
                if BUF[i] == b'\n' {
                    if i > start {
                        handle_command(&BUF[start..i]);
                    }
                    start = i + 1;
                }
            }
        }
    }
}

// ==================== 事件处理（/dev/wifi） ====================

/// 从 /dev/wifi 读取事件并按类型更新状态机 / 打印。
fn poll_wifi() {
    unsafe {
        if WIFI_FD < 0 {
            return;
        }
        let mut buf = [0u8; 512];
        loop {
            let n = syscall3(
                SYS_READ,
                WIFI_FD as usize,
                buf.as_mut_ptr() as usize,
                buf.len(),
            );
            // syscall 返回原始寄存器值：-1 在 usize 下是天文数字，必须先按
            // isize 判负（否则 &buf[1..n] 越界 panic 杀死守护进程）
            let n = n as isize;
            if n <= 0 {
                break;
            }
            let n = n as usize;
            let ty = buf[0];
            let body = &buf[1..n.min(buf.len())];
            match ty {
                dp::WIFI_EVT_SCAN_RESULT => {
                    // 43 字节 WifiScanRecord
                    if body.len() >= 43 {
                        let rec = core::ptr::read(body.as_ptr() as *const dp::WifiScanRecord);
                        print("[wifid] AP ");
                        if rec.ssid_len == 0 {
                            print("<隐藏>");
                        } else {
                            print_bytes(&rec.ssid[..rec.ssid_len as usize]);
                        }
                        print(" bssid=");
                        print_mac(&rec.bssid);
                        print(" ch=");
                        print_dec(rec.channel as usize);
                        print(" rssi=");
                        print_signed(rec.rssi as i64);
                        print(" auth=");
                        print_dec(rec.auth as usize);
                        print("\n");
                    }
                }
                dp::WIFI_EVT_SCAN_DONE => {
                    set_state(WifiState::Idle); // 扫描结束
                    print("[wifid] 扫描完成\n");
                }
                dp::WIFI_EVT_CONNECTING => {
                    if body.len() >= 41 {
                        let rec = core::ptr::read(body.as_ptr() as *const dp::WifiStateRecord);
                        if rec.state == dp::WIFI_STATE_HANDSHAKE {
                            set_state(WifiState::Authenticating);
                        } else {
                            set_state(WifiState::Associating);
                        }
                    }
                }
                dp::WIFI_EVT_HANDSHAKE_MSG => {
                    set_state(WifiState::Authenticating);
                }
                dp::WIFI_EVT_CONNECTED => {
                    if body.len() >= 41 {
                        let rec = core::ptr::read(body.as_ptr() as *const dp::WifiStateRecord);
                        CUR_SSID = rec.ssid;
                        CUR_SSID_LEN = rec.ssid_len;
                    }
                    set_state(WifiState::Connected);
                    print("[wifid] 已连接 (DHCP/IP 已由内核应用)\n");
                }
                dp::WIFI_EVT_DISCONNECTED => {
                    set_state(WifiState::Disconnected);
                    print("[wifid] 已断开\n");
                }
                dp::WIFI_EVT_ERROR => {
                    print("[wifid] ERROR: ");
                    print_bytes(body);
                    print("\n");
                    // 自动重连：有目标且未耗尽重试次数
                    if PEND_SSID_LEN > 0 && CONNECT_RETRY < CONNECT_RETRY_MAX {
                        CONNECT_RETRY += 1;
                        print("[wifid] 重试 ");
                        print_dec(CONNECT_RETRY as usize);
                        print("/");
                        print_dec(CONNECT_RETRY_MAX as usize);
                        print("\n");
                        let ssid = &PEND_SSID[..PEND_SSID_LEN as usize];
                        let psk = &PEND_PSK[..PEND_PSK_LEN as usize];
                        issue_connect(ssid, psk, 1, false);
                    } else {
                        set_state(WifiState::Disconnected);
                    }
                }
                _ => {}
            }
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[wifid] 已启动\n");
    log_line("[wifid] 已启动");

    if !wifi_open() {
        print("[wifid] FATAL: 无法打开 /dev/wifi (内核 wifi 未激活)\n");
    } else {
        print("[wifid] 已打开 /dev/wifi\n");
    }

    load_all_networks();
    if let Some(i) = unsafe { best_saved() } {
        let ssid = unsafe { &NETS[i].ssid[..NETS[i].ssid_len as usize] };
        let psk = unsafe { &NETS[i].psk[..NETS[i].psk_len as usize] };
        print("[wifid] 自动连接最佳已保存网络 ");
        print_bytes(ssid);
        print("\n");
        issue_connect(ssid, psk, 1, true);
    } else {
        print("[wifid] 无已保存网络；自动扫描以预热缓存\n");
        issue_scan();
    }

    // 创建命名管道（普通文件模拟）
    unsafe {
        let fd = sys_open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
            print("[wifid] 正在监听 ");
            print(PIPE_PATH);
            print("\n");
        } else {
            print("[wifid] FATAL: 无法创建管道 ");
            print(PIPE_PATH);
            print("\n");
        }
    }

    // 主循环：轮询管道命令 + /dev/wifi 事件，驱动状态机。
    // 常驻服务：init 把 wifid 当作守护进程拉起，OOBE/设置会随时经管道发
    // SCAN/CONNECT/STATUS —— 限时轮次退出后这些命令静默丢失（写入者拿到
    // 的仍是成功回执），表现为「Wi-Fi 永远连不上」。
    loop {
        poll_pipe();
        poll_wifi();
        sleep_ms(250);
    }
}

// ==================== 入口 ====================

kunxrt::entry!();

kunxrt::panic_handler!("wifid");