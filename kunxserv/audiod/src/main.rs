// KunxOS audiod：音频守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 kunxinit 在 networkd 之后拉起（/bin/audiod），常驻运行。职责：
//   1. 打开 /dev/audio（内核低延迟播放设备，virtio-sound 后端），
//      经 AUDIO_IOCTL_RT 提升到 Realtime 调度类（SCHED_FIFO 等价物），
//      保证 10ms 混音周期的调度抖动最小化
//   2. 焦点管理：电话(0) > 导航(1) > 音乐(2) > 游戏(3) 优先级策略
//   3. 软件混音：把多个应用的 PCM 块按增益（Q15 定点）混合为一路输出
//   4. 客户端协议：/var/run/audiod.ctl（控制）+ /var/run/audio/<pid>.in
//      （数据，双槽位环形）+ /var/run/audio/<pid>.st（状态）
//   5. 主循环：5ms 轮询控制/数据文件，10ms 混音并写入 /dev/audio
//
// 混音数学（定点）：
//   out[i] = clamp( Σ (sample_k[i] * gain_k) >> 15, -32768..32767 )
//   增益 Q15：32768 = 1.0，8192 = 0.25（被压低），0 = 静音。
//
// 焦点策略（与 shared/audio.rs 文档一致）：
//   - 无应用持焦：全部满音量
//   - 电话持焦：电话满音量，其余全部静音（通话独占）
//   - 导航持焦：导航满音量，音乐/游戏压低（0.25）
//   - 音乐持焦：音乐满音量，游戏压低
//   - 高优先级应用即使未持焦也可听见（如导航播报时电话提示音仍响）

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
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_IOCTL: usize = 29;
const SYS_NANOSLEEP: usize = 101;
const SYS_MKDIRAT: usize = 34;
const SYS_EXIT: usize = 93;
const SYS_KILL: usize = 129;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;

const SEEK_SET: usize = 0;

// ==================== 音频常量（与内核 audio.rs / shared/audio.rs 一致） ====================

/// /dev/audio 设备路径。
const AUDIO_PATH: &str = "/dev/audio";
/// 采样率（Hz）。
const SAMPLE_RATE: u32 = 48000;
/// 每块帧数（10ms）。
const CHUNK_FRAMES: usize = 480;
/// 每块字节数。
const CHUNK_BYTES: usize = 1920;

/// 内核 ioctl：GETPARAMS / START / STOP / STATS / RT。
const AUDIO_IOCTL_GETPARAMS: u32 = 0x4100;
const AUDIO_IOCTL_START: u32 = 0x4101;
const AUDIO_IOCTL_STOP: u32 = 0x4102;
const AUDIO_IOCTL_STATS: u32 = 0x4103;
const AUDIO_IOCTL_RT: u32 = 0x4104;

/// 内核统计结构（128 字节，与 audio.rs AudioStats 布局一致）。
#[repr(C)]
struct AudioStats {
    running: u32,
    rate: u32,
    channels: u32,
    bits: u32,
    sw_frames: u64,
    hw_frames: u64,
    underruns: u64,
    late_writes: u64,
    irq_count: u64,
    period_min_us: u64,
    period_avg_us: u64,
    period_max_us: u64,
    refill_min_us: u64,
    refill_avg_us: u64,
    refill_max_us: u64,
    dma_min_us: u64,
    dma_avg_us: u64,
    dma_max_us: u64,
}

// ==================== 协议常量（与 shared/audio.rs 一致） ====================

const CTL_PATH: &str = "/var/run/audiod.ctl";
const DATA_DIR: &str = "/var/run/audio/";

/// 用途类型（越小优先级越高）。
const USAGE_PHONE: u8 = 0;
const USAGE_NAV: u8 = 1;
const USAGE_MUSIC: u8 = 2;
const USAGE_GAME: u8 = 3;

/// 焦点状态（写往 .st 文件）。
const FOCUS_FOCUSED: u32 = 0;
const FOCUS_DUCKED: u32 = 1;
const FOCUS_MUTED: u32 = 2;

/// 增益（Q15 定点）。
const G_FULL: i32 = 32768;
const G_DUCK: i32 = 8192; // 0.25
const G_MUTE: i32 = 0;

/// 数据槽字节数（8 字节头 + 1920 字节 PCM）。
const SLOT_BYTES: usize = 8 + CHUNK_BYTES;
/// 数据文件字节数（双槽）。
const FILE_BYTES: usize = 2 * SLOT_BYTES;

/// 最大并发客户端。
const MAX_CLIENTS: usize = 8;

/// 混音周期（微秒）；必须与内核 CHUNK_FRAMES 一致（10ms @ 48kHz）。
const MIX_PERIOD_US: u64 = 10_000;
/// 轮询间隔（微秒）。低延迟配置：2ms（原 5ms），配合 RT 提升后使
/// 「应用写入 → audiod 拾取」的相位误差降到 ~2ms（见 docs/audio/architecture.md）。
const POLL_INTERVAL_US: u64 = 2_000;

// ==================== 客户端表 ====================

#[derive(Clone, Copy)]
struct Client {
    pid: i32,
    usage: u8,
    focused: bool,
    active: bool,
    gain: i32,
    state: u32,
    last_seq: u32,
    has_chunk: bool,
    chunk: [u8; CHUNK_BYTES],
}

const EMPTY_CLIENT: Client = Client {
    pid: 0,
    usage: 0,
    focused: false,
    active: false,
    gain: G_FULL,
    state: FOCUS_FOCUSED,
    last_seq: 0,
    has_chunk: false,
    chunk: [0; CHUNK_BYTES],
};

static mut CLIENTS: [Client; MAX_CLIENTS] = [EMPTY_CLIENT; MAX_CLIENTS];
/// 客户端数量（诊断）。
static mut CLIENT_COUNT: usize = 0;

// ==================== 静态缓冲 ====================

/// 控制文件内容缓冲。
static mut CTL_BUF: [u8; 1024] = [0; 1024];
/// 客户端数据拉取缓冲（3856 字节）。
static mut PULL_BUF: [u8; FILE_BYTES] = [0; FILE_BYTES];
/// 混音累加器（960 = 480 帧 × 2 声道）。
static mut ACC: [i32; CHUNK_FRAMES * 2] = [0; CHUNK_FRAMES * 2];
/// 输出块。
static mut OUT: [u8; CHUNK_BYTES] = [0; CHUNK_BYTES];
/// 路径缓冲。
static mut PATH_BUF: [u8; 64] = [0; 64];
/// 状态行缓冲。
static mut STATUS_BUF: [u8; 32] = [0; 32];

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

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
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
        write_fd(1, &buf[i..]);
    }
}

fn sleep_us(us: usize) {
    unsafe {
        let mut req = [0u8; 16];
        let sec = (us / 1_000_000) as u64;
        let nsec = ((us % 1_000_000) * 1000) as u64;
        req[..8].copy_from_slice(&sec.to_le_bytes());
        req[8..].copy_from_slice(&nsec.to_le_bytes());
        syscall3(SYS_NANOSLEEP, req.as_ptr() as usize, 0, 0);
    }
}

/// 打开文件（路径 NUL 结尾于 `PATH_BUF`）。
unsafe fn open_at(flags: usize) -> i32 {
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as i32
}

/// 构建 `/var/run/audio/<pid><tag>` 路径（NUL 结尾）。
fn build_audio_path(tag: &str, pid: i32) {
    unsafe {
        let p = PATH_BUF.as_mut_ptr();
        let d = DATA_DIR.as_bytes();
        let t = tag.as_bytes();
        let mut n = 0usize;
        for &c in d.iter().take(48) {
            *p.add(n) = c;
            n += 1;
        }
        let mut tmp = [0u8; 12];
        let mut i = tmp.len();
        let mut v = pid as u32;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in tmp.iter().skip(i) {
            *p.add(n) = c;
            n += 1;
        }
        for &c in t.iter().take(8) {
            *p.add(n) = c;
            n += 1;
        }
        *p.add(n) = 0;
    }
}

// ==================== 焦点策略 ====================

/// 重新计算所有客户端增益（电话 > 导航 > 音乐 > 游戏）。
///
/// 规则：
///   - 无应用持焦：全部满音量
///   - 电话持焦：电话满音量，其余全部静音（通话独占）
///   - 否则：持焦的最高优先级应用满音量；比它优先级高但未持焦的仍满音量
///     （高优先级提示音不被打断）；比它优先级低的压低到 0.25
fn recompute_gains() {
    unsafe {
        // 找到持焦的最高优先级（usage 最小）
        let mut highest: u8 = 255;
        for i in 0..MAX_CLIENTS {
            if CLIENTS[i].active && CLIENTS[i].focused && CLIENTS[i].usage < highest {
                highest = CLIENTS[i].usage;
            }
        }
        for i in 0..MAX_CLIENTS {
            let c = &mut CLIENTS[i];
            if !c.active {
                continue;
            }
            let g = if highest == 255 {
                G_FULL
            } else if c.usage == highest {
                G_FULL
            } else if highest == USAGE_PHONE {
                G_MUTE
            } else if c.usage < highest {
                G_FULL
            } else {
                G_DUCK
            };
            c.gain = g;
            c.state = if g == G_FULL {
                FOCUS_FOCUSED
            } else if g == G_DUCK {
                FOCUS_DUCKED
            } else {
                FOCUS_MUTED
            };
        }
    }
}

// ==================== 控制命令处理 ====================

fn find_client(pid: i32) -> Option<usize> {
    unsafe {
        for i in 0..MAX_CLIENTS {
            if CLIENTS[i].active && CLIENTS[i].pid == pid {
                return Some(i);
            }
        }
    }
    None
}

fn cmd_open(pid: i32, usage: u8) {
    unsafe {
        if pid <= 0 {
            return;
        }
        // 已存在：更新用途
        if let Some(i) = find_client(pid) {
            CLIENTS[i].usage = usage.min(3);
            return;
        }
        for i in 0..MAX_CLIENTS {
            if !CLIENTS[i].active {
                CLIENTS[i] = EMPTY_CLIENT;
                CLIENTS[i].pid = pid;
                CLIENTS[i].usage = usage.min(3);
                CLIENTS[i].active = true;
                CLIENTS[i].gain = G_FULL;
                CLIENT_COUNT += 1;
                print("[audiod] OPEN pid=");
                print_dec(pid as u64);
                print(" usage=");
                print_dec(usage.min(3) as u64);
                print("\n");
                recompute_gains();
                return;
            }
        }
        print("[audiod] OPEN 被拒绝：客户端表已满\n");
    }
}

fn cmd_close(pid: i32) {
    unsafe {
        if let Some(i) = find_client(pid) {
            CLIENTS[i] = EMPTY_CLIENT;
            if CLIENT_COUNT > 0 {
                CLIENT_COUNT -= 1;
            }
            print("[audiod] CLOSE pid=");
            print_dec(pid as u64);
            print("\n");
            recompute_gains();
        }
    }
}

/// 客户端存活探测（kill(pid,0)）：被 SIGKILL/崩溃杀掉的应用不会执行
/// CLOSE（no_std 进程被杀无清理路径，内核只关它的 fd）。不回收则 8 个
/// 槽位被死客户端逐个占满后所有应用开流永久失败（音频整体失效）。
fn poll_deaths() {
    unsafe {
        let mut changed = false;
        for i in 0..MAX_CLIENTS {
            if !CLIENTS[i].active {
                continue;
            }
            let pid = CLIENTS[i].pid;
            // kill(pid, 0)：仅探测存在性；errno ESRCH = 已死
            let r = syscall2(SYS_KILL, pid as usize, 0) as isize;
            if r < 0 {
                print("[audiod] 客户端退出 pid=");
                print_dec(pid as u64);
                print("\n");
                CLIENTS[i] = EMPTY_CLIENT;
                if CLIENT_COUNT > 0 {
                    CLIENT_COUNT -= 1;
                }
                changed = true;
            }
        }
        if changed {
            recompute_gains();
        }
    }
}

fn cmd_focus(pid: i32, want: bool) {
    unsafe {
        if let Some(i) = find_client(pid) {
            CLIENTS[i].focused = want;
            print("[audiod] ");
            print(if want { "FOCUS" } else { "RELEASE" });
            print(" pid=");
            print_dec(pid as u64);
            print(" usage=");
            print_dec(CLIENTS[i].usage as u64);
            print("\n");
            recompute_gains();
        }
    }
}

/// 解析十进制数（`s` 到 `end`）。
fn parse_u32(s: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    let mut any = false;
    for &c in s {
        if (b'0'..=b'9').contains(&c) {
            v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
            any = true;
        } else {
            break;
        }
    }
    if any {
        Some(v)
    } else {
        None
    }
}

/// 轮询控制文件并逐行分发命令。
fn poll_ctl() {
    unsafe {
        let mut p = [0u8; 64];
        for (i, c) in CTL_PATH.as_bytes().iter().enumerate() {
            p[i] = *c;
        }
        p[CTL_PATH.len()] = 0;
        let fd = syscall3(
            SYS_OPENAT,
            AT_FDCWD,
            p.as_ptr() as usize,
            O_RDWR | O_CREAT,
        ) as i32;
        if fd < 0 {
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        let n = syscall3(SYS_READ, fd as usize, CTL_BUF.as_mut_ptr() as usize, CTL_BUF.len());
        // 清空控制文件
        syscall2_ftruncate(fd);
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        syscall1(SYS_CLOSE, fd as usize);
        if n == 0 || n > CTL_BUF.len() {
            return;
        }
        // 按行分发
        let data = &CTL_BUF[..n];
        let mut start = 0usize;
        for i in 0..=n {
            if i == n || data[i] == b'\n' {
                if i > start {
                    dispatch_line(&data[start..i]);
                }
                start = i + 1;
            }
        }
    }
}

unsafe fn syscall2_ftruncate(fd: i32) {
    syscall3(SYS_FTRUNCATE, fd as usize, 0, 0);
}

/// 分发行命令：`CMD <pid> [usage]`。
fn dispatch_line(line: &[u8]) {
    // 提取第一个词（命令名）
    let mut i = 0usize;
    while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
        i += 1;
    }
    let wstart = i;
    while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
        i += 1;
    }
    let cmd = &line[wstart..i];
    // 数字参数
    let mut args = [0u32; 4];
    let mut argc = 0usize;
    while i < line.len() && argc < 4 {
        while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let astart = i;
        while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
            i += 1;
        }
        if let Some(v) = parse_u32(&line[astart..i]) {
            args[argc] = v;
            argc += 1;
        }
    }
    if cmd.len() == 4 && &cmd[..4] == b"OPEN" {
        if argc >= 2 {
            cmd_open(args[0] as i32, args[1] as u8);
        }
    } else if cmd.len() == 5 && &cmd[..5] == b"CLOSE" {
        if argc >= 1 {
            cmd_close(args[0] as i32);
        }
    } else if cmd.len() == 5 && &cmd[..5] == b"FOCUS" {
        if argc >= 1 {
            cmd_focus(args[0] as i32, true);
        }
    } else if cmd.len() == 7 && &cmd[..7] == b"RELEASE" {
        if argc >= 1 {
            cmd_focus(args[0] as i32, false);
        }
    }
}

// ==================== 数据拉取 ====================

/// 拉取客户端最新完整数据块（双槽位，最新序号优先）。
fn pull_client(i: usize) {
    unsafe {
        let pid = CLIENTS[i].pid;
        build_audio_path(".in", pid);
        let fd = open_at(O_RDONLY);
        if fd < 0 {
            return;
        }
        let n = syscall3(SYS_READ, fd as usize, PULL_BUF.as_mut_ptr() as usize, FILE_BYTES);
        syscall1(SYS_CLOSE, fd as usize);
        // 读失败 -1（usize 天文数字）按 isize 判负：否则把读错误当成功长度，
        // 反复重吃上一轮拉取的陈旧音频数据（PULL_BUF 是静态缓冲）
        if (n as isize) < 8 {
            return;
        }
        let last_seq = CLIENTS[i].last_seq;
        let mut best_seq = 0u32;
        let mut best_slot = usize::MAX;
        for slot in 0..2 {
            let off = slot * SLOT_BYTES;
            if (n as usize) < off + 8 {
                continue;
            }
            let seq = u32::from_le_bytes([
                PULL_BUF[off],
                PULL_BUF[off + 1],
                PULL_BUF[off + 2],
                PULL_BUF[off + 3],
            ]);
            let len = u32::from_le_bytes([
                PULL_BUF[off + 4],
                PULL_BUF[off + 5],
                PULL_BUF[off + 6],
                PULL_BUF[off + 7],
            ]);
            // 完整 + 更新（seq 环绕判定：差值小于 2^31 视为更新）
            if len == CHUNK_BYTES as u32
                && (n as usize) >= off + 8 + len as usize
                && seq != 0
                && seq.wrapping_sub(last_seq) < 0x8000_0000
                && seq.wrapping_sub(best_seq) < 0x8000_0000
                && seq != best_seq
            {
                // 新槽位必须比当前 best 更新才替换；两者取最新
                if best_slot == usize::MAX || seq.wrapping_sub(best_seq) < 0x8000_0000 {
                    best_seq = seq;
                    best_slot = slot;
                }
            }
        }
        if best_slot != usize::MAX {
            let off = best_slot * SLOT_BYTES;
            CLIENTS[i].chunk.copy_from_slice(&PULL_BUF[off + 8..off + 8 + CHUNK_BYTES]);
            CLIENTS[i].last_seq = best_seq;
            CLIENTS[i].has_chunk = true;
        }
    }
}

// ==================== 混音 ====================

/// 混合所有客户端块到输出块（Q15 定点累加 + 饱和）。
fn mix() {
    unsafe {
        for a in ACC.iter_mut() {
            *a = 0;
        }
        for i in 0..MAX_CLIENTS {
            let c = &CLIENTS[i];
            if !c.active || !c.has_chunk || c.gain == 0 {
                continue;
            }
            let gain = c.gain;
            let src = &c.chunk;
            for s in 0..CHUNK_FRAMES * 2 {
                let v = i16::from_le_bytes([src[s * 2], src[s * 2 + 1]]) as i32;
                ACC[s] += (v * gain) >> 15;
            }
        }
        for s in 0..CHUNK_FRAMES * 2 {
            let v = ACC[s].clamp(-32768, 32767) as i16;
            OUT[s * 2] = (v as u16 & 0xFF) as u8;
            OUT[s * 2 + 1] = ((v as u16 >> 8) & 0xFF) as u8;
        }
    }
}

// ==================== 状态回写 ====================

/// 把每个客户端的状态写入 `/var/run/audio/<pid>.st`（`<gain> <state> <seq>`）。
fn write_status() {
    unsafe {
        for i in 0..MAX_CLIENTS {
            let c = &CLIENTS[i];
            if !c.active {
                continue;
            }
            // 渲染 "<gain> <state> <seq>"
            let mut n = 0usize;
            let mut v = c.gain;
            if v < 0 {
                STATUS_BUF[n] = b'-';
                n += 1;
                v = -v;
            }
            let mut tmp = [0u8; 12];
            let mut k = tmp.len();
            while v > 0 {
                k -= 1;
                tmp[k] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            for &b in tmp.iter().skip(k) {
                STATUS_BUF[n] = b;
                n += 1;
            }
            STATUS_BUF[n] = b' ';
            n += 1;
            STATUS_BUF[n] = b'0' + c.state as u8;
            n += 1;
            STATUS_BUF[n] = b' ';
            n += 1;
            let mut s = c.last_seq;
            let mut k2 = tmp.len();
            while s > 0 {
                k2 -= 1;
                tmp[k2] = b'0' + (s % 10) as u8;
                s /= 10;
            }
            for &b in tmp.iter().skip(k2) {
                STATUS_BUF[n] = b;
                n += 1;
            }
            STATUS_BUF[n] = b'\n';
            n += 1;

            build_audio_path(".st", c.pid);
            let fd = open_at(O_RDWR | O_CREAT);
            if fd < 0 {
                continue;
            }
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
            syscall3(SYS_WRITE, fd as usize, STATUS_BUF.as_ptr() as usize, n);
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

// ==================== 音频设备 ====================

/// 打开 /dev/audio 并初始化：GETPARAMS 校验 + RT 提升 + START。
fn audio_init() -> i32 {
    unsafe {
        let mut p = [0u8; 32];
        for (i, c) in AUDIO_PATH.as_bytes().iter().enumerate() {
            p[i] = *c;
        }
        p[AUDIO_PATH.len()] = 0;
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDWR) as i32;
        if fd < 0 {
            print("[audiod] 打开 /dev/audio 失败（无音频后端？）\n");
            return fd;
        }
        // GETPARAMS（12 字节输出）
        let mut params = [0u8; 12];
        let r = syscall3(SYS_IOCTL, fd as usize, AUDIO_IOCTL_GETPARAMS as usize, params.as_mut_ptr() as usize);
        // ioctl 失败 -errno 的 usize 原值按 isize 判负（此前 `r < 0` 恒假，
        // 失败被吞、打印未填充的垃圾参数）
        if (r as isize) < 0 {
            print("[audiod] GETPARAMS 失败\n");
        } else {
            let rate = u32::from_le_bytes([params[0], params[1], params[2], params[3]]);
            let ch = u32::from_le_bytes([params[4], params[5], params[6], params[7]]);
            let bits = u32::from_le_bytes([params[8], params[9], params[10], params[11]]);
            print("[audiod] /dev/audio ");
            print_dec(rate as u64);
            print("Hz ");
            print_dec(ch as u64);
            print("ch ");
            print_dec(bits as u64);
            print("bit\n");
        }
        // 实时提升：本进程进入 Realtime 调度类
        syscall3(SYS_IOCTL, fd as usize, AUDIO_IOCTL_RT as usize, 0);
        // 启动播放（首写也会自动启动，这里显式预热）
        syscall3(SYS_IOCTL, fd as usize, AUDIO_IOCTL_START as usize, 0);
        fd
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[audiod] 正在启动\n");

    // 数据目录（不存在则创建）
    unsafe {
        let mut p = [0u8; 64];
        for (i, c) in DATA_DIR.as_bytes().iter().enumerate() {
            p[i] = *c;
        }
        p[DATA_DIR.len()] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, p.as_ptr() as usize, 0o755);
    }

    let afd = audio_init();
    if afd < 0 {
        print("[audiod] 无音频设备，正在退出\n");
        unsafe { syscall1(SYS_EXIT, 1) };
        return 1;
    }

    // 控制文件
    unsafe {
        let mut p = [0u8; 64];
        for (i, c) in CTL_PATH.as_bytes().iter().enumerate() {
            p[i] = *c;
        }
        p[CTL_PATH.len()] = 0;
        // 接住 openat 返回值：这里只需创建控制文件（轮询由 poll_ctl 每次
        // 自行 open/close）。丢弃返回值会泄漏一个内核全局打开文件表槽位；
        // 32 槽时代的教训（对照 rild 创建命令管道的写法）：fd>=0 用完即关，
        // 失败不 close。
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDWR | O_CREAT) as i32;
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
        } else {
            print("[audiod] 无法创建 ");
            print(CTL_PATH);
            print("\n");
        }
    }

    print("[audiod] 焦点策略：phone > nav > music > game\n");
    print("[audiod] 正在监听 /var/run/audiod.ctl\n");

    // 主循环：2ms 轮询控制/数据，每 5 周期（10ms）混音写入
    let mut cycle: u64 = 0;
    loop {
        poll_ctl();
        for i in 0..MAX_CLIENTS {
            if unsafe { CLIENTS[i].active } {
                pull_client(i);
            }
        }
        // 每 250 周期（500ms）探测死亡客户端，回收槽位
        if cycle % 250 == 124 {
            poll_deaths();
        }

        // 混音周期：cycle 每 2ms 递增，每 5 周期（100Hz/10ms）混音一次。
        // 空闲（没有任何客户端产出块）时跳过混音与写入：内核会在补充路径
        // 用静音块兜底，audiod 不为「无人播放」空转文件 I/O。
        cycle += 1;
        if cycle % 5 == 0 {
            let mut any = false;
            for i in 0..MAX_CLIENTS {
                if unsafe { CLIENTS[i].active && CLIENTS[i].has_chunk } {
                    any = true;
                    break;
                }
            }
            if any {
                mix();
                let n = unsafe { syscall3(SYS_WRITE, afd as usize, OUT.as_ptr() as usize, CHUNK_BYTES) };
                if (n as usize) < CHUNK_BYTES {
                    // 内核环形缓冲满（audiod 快于硬件消费）：整块跳过，下周期再写。
                    // 丢弃计数由内核 STATS 的 late_writes 提供，这里不重复计数。
                }
            }
            write_status();
        }

        sleep_us(POLL_INTERVAL_US as usize);
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
        write_fd(2, b"PANIC: audiod\n");
        if w.len > 0 {
            write_fd(2, &w.buf[..w.len]);;
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
