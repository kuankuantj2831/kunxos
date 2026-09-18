// KunxOS pushd：推送守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 职责（设计见 docs/lifecycle.md）：
//   1. 本地通知队列：轮询 /data/push/in/（应用写入 "<title>|<body>" 的
//      请求文件，文件名 "<pid>_<seq>"），分配通知 id 并转发给 lifd
//      （命令信箱 "DELIVER <pid> <id> <payload>"）。
//   2. 投递策略归属 lifd（其持有应用状态注册表）：Running 立即投递事件；
//      Background/Suspended 延迟到恢复时补投；未注册丢弃。
//   3. 远程推送接口预留（APNS/FCM 等价物）：`remote_send` 占位实现
//      （日志 + 持久化），通道层留待后续接入。
//
// 不直接接触应用进程：所有投递经 lifd 转发，保证单一投递路径。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_GETDENTS64: usize = 61;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_MKDIRAT: usize = 34;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETPID: usize = 172;
const SYS_GETTIMEOFDAY: usize = 169;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

const PUSH_IN: &str = "/data/push/in";
const CMD_PREFIX: &str = "/data/life/cmd_";

/// 主循环轮询间隔（ms）。
const POLL_MS: usize = 80;
/// 通知 id 计数器（持久化可后续加入；v1 内存递增）。
static mut NOTIFY_SEQ: u32 = 1;
static mut SELF_PID: u32 = 0;
/// 请求内容缓冲。
static mut BUF: [u8; 200] = [0; 200];

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

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
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
    print("[pushd] ");
    print(s);
    print("\n");
}

fn log2(s: &str, v: u64) {
    print("[pushd] ");
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

fn exit(code: usize) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    loop {}
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
    // 读失败返回 -errno 的 usize 原值：按 isize 判负，否则把陈旧缓冲内容
    // 当新通知转发（第 13 轮 lifd/logd 同族漏网）
    let r = r as isize;
    if r <= 0 {
        0
    } else {
        (r as usize).min(out.len())
    }
}

fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = sys_open(path, O_WRONLY | O_CREAT | O_TRUNC);
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

/// 十进制解析（返回 (值, 消费字节数)）。
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

// ==================== 通知队列 ====================

/// 扫描 /data/push/in：处理每个 "<pid>_<seq>" 请求文件。
/// 内容 "<title>|<body>"；处理后分配 id 并转发 lifd，然后删除请求文件。
fn poll_inbox() {
    let mut raw = [0u8; 4096];
    let fd = sys_open(PUSH_IN, O_RDONLY);
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
            if elen >= 3 {
                handle_request(&name[..elen]);
            }
            off += reclen;
        }
    }
    unsafe { syscall1(SYS_CLOSE, fd as usize) };
}

/// 处理单个通知请求文件。
fn handle_request(fname: &[u8]) {
    // 文件名 "<pid>_<seq>"：取 pid
    let (pid, consumed) = parse_dec(fname);
    if pid == 0 || consumed == 0 || consumed >= fname.len() {
        return;
    }
    // 拼完整路径并读取
    let mut path = [0u8; 96];
    let mut n = 0usize;
    copy_str(&mut path, &mut n, PUSH_IN);
    path[n] = b'/';
    n += 1;
    let k = fname.len().min(60);
    path[n..n + k].copy_from_slice(&fname[..k]);
    n += k;
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let got = read_file(p, unsafe { &mut BUF });
    // 无论内容是否有效都删除请求（避免重复处理）
    unlink(p);
    if got == 0 {
        return;
    }
    // 就绪窗口防护见下方 write 失败路径。
    // 内容 "<title>|<body>"（≤160B）
    let content = unsafe { &BUF[..got.min(BUF.len())] };
    let id = next_id();
    // 转发 lifd：cmd_<pushd_pid>_<seq> = "DELIVER <pid> <id> <title>|<body>"
    let mut cmd = [0u8; 220];
    let mut m = 0usize;
    copy_str(&mut cmd, &mut m, "DELIVER ");
    push_dec_to(&mut cmd, &mut m, pid);
    copy_str(&mut cmd, &mut m, " ");
    push_dec_to(&mut cmd, &mut m, id as u64);
    copy_str(&mut cmd, &mut m, " ");
    let k2 = content.len().min(cmd.len() - m);
    cmd[m..m + k2].copy_from_slice(&content[..k2]);
    m += k2;
    let mut ok = publish_cmd(&cmd[..m]);
    if !ok {
        // lifd 未就绪窗口（/data/life 由 lifd 创建，启动顺序不保证）：补建
        // 目录后重试一次；仍失败则把请求文件原样写回，通知不丢。
        mkdir("/data/life");
        ok = publish_cmd(&cmd[..m]);
        if !ok {
            let mut rp = [0u8; 96];
            let mut rn = 0usize;
            copy_str(&mut rp, &mut rn, PUSH_IN);
            rp[rn] = b'/';
            rn += 1;
            let k3 = fname.len().min(rp.len() - rn);
            rp[rn..rn + k3].copy_from_slice(&fname[..k3]);
            rn += k3;
            write_file(core::str::from_utf8(&rp[..rn]).unwrap_or(""), content);
        }
    }
    log2("通知 id=", id as u64);
    print(" -> pid=");
    print_dec(pid);
    print(if ok { " 已转发\n" } else { " 转发失败\n" });
}

fn next_id() -> u32 {
    unsafe {
        NOTIFY_SEQ = NOTIFY_SEQ.wrapping_add(1);
        if NOTIFY_SEQ == 0 {
            NOTIFY_SEQ = 1;
        }
        NOTIFY_SEQ
    }
}

/// 命令序号（每消息一文件的唯一名）。
static mut CMD_SEQ: u64 = 0;

/// 发布一条 lifd 命令（cmd_<pid>_<seq>，写 .tmp 后 rename）。
/// 每消息一文件消除「O_TRUNC 覆盖 × lifd 读后删除」的丢命令竞态；
/// 目标名含序号永不存在，rename 恒成功。
fn publish_cmd(cmd: &[u8]) -> bool {
    unsafe {
        CMD_SEQ = CMD_SEQ.wrapping_add(1);
        let mut base = [0u8; 96];
        let mut n = 0usize;
        copy_str(&mut base, &mut n, CMD_PREFIX);
        push_dec_to(&mut base, &mut n, SELF_PID as u64);
        if n + 1 < base.len() {
            base[n] = b'_';
            n += 1;
        }
        push_dec_to(&mut base, &mut n, CMD_SEQ);
        let mut tmp = [0u8; 104];
        let tn = n.min(96);
        tmp[..tn].copy_from_slice(&base[..tn]);
        tmp[tn..tn + 4].copy_from_slice(b".tmp");
        if !write_file(core::str::from_utf8(&tmp[..tn + 4]).unwrap_or(""), cmd) {
            return false;
        }
        let mut newp = [0u8; 104];
        newp[..tn].copy_from_slice(&base[..tn]);
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            tmp.as_ptr() as usize,
            AT_FDCWD,
            newp.as_ptr() as usize,
        ) as isize
            == 0
    }
}

fn copy_str(out: &mut [u8], n: &mut usize, s: &str) {
    let b = s.as_bytes();
    let k = b.len().min(out.len().saturating_sub(*n));
    out[*n..*n + k].copy_from_slice(&b[..k]);
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

// ==================== 远程推送接口（预留） ====================

/// 远程推送通道（APNS/FCM 等价物的占位接口）。
///
/// 真实实现需要：设备 token 注册、与远程服务的加密长连接、消息确认回执。
/// 本期仅记录调用（日志 + 落盘），为后续接入保留调用点。
pub fn remote_send(device_token: &[u8], payload: &[u8]) {
    log("远程推送 (预留接口) token_len=");
    print_dec(device_token.len() as u64);
    print(" payload_len=");
    print_dec(payload.len() as u64);
    print("\n");
    // 占位持久化：/data/push/remote.log（可观测预留接口被调用）
    let mut rec = [0u8; 128];
    let mut n = 0usize;
    copy_str(&mut rec, &mut n, "remote ");
    let k = payload.len().min(rec.len() - n);
    rec[n..n + k].copy_from_slice(&payload[..k]);
    n += k;
    let _ = write_file("/data/push/remote.log", &rec[..n]);
}

/// 设备 token 注册（占位：本地随机 + 持久化）。
pub fn register_device() -> [u8; 16] {
    let mut tok = [0u8; 16];
    // 占位：基于时间戳的伪 token（真实实现应使用硬件随机 + 服务端绑定）
    let t = now_us();
    for i in 0..8 {
        tok[i] = ((t >> (i * 8)) & 0xFF) as u8;
    }
    let _ = write_file("/data/push/device_token", &tok);
    log("设备 token 已登记 (占位)");
    tok
}

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

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[pushd] KunxOS 推送守护进程启动中\n");

    mkdir("/data");
    mkdir("/data/push");
    mkdir("/data/push/in");
    unsafe {
        SELF_PID = syscall1(SYS_GETPID, 0) as u32;
    }
    log2("pid=", unsafe { SELF_PID } as u64);
    // 预留接口自检（启动日志可观测）
    let _tok = register_device();

    loop {
        poll_inbox();
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
        syscall3(SYS_WRITE, 2, b"PANIC: pushd\n".as_ptr() as usize, 13);
        if w.len > 0 {
            syscall3(SYS_WRITE, 2, w.buf.as_ptr() as usize, w.len);
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
