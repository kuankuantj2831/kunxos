//! KunxOS 音频客户端库（EL0 用户态，裸 syscall、no_std）。
//!
//! 应用以 `#[path = "../../../shared/audio.rs"] mod audio_client;` 引入，
//! 然后使用：
//!   - [`open_stream`]：向 audiod 注册一个流（带用途类型）
//!   - [`write_chunk`]：写入一块 10ms PCM（480 帧 × 16bit 立体声）
//!   - [`request_focus`] / [`release_focus`]：申请 / 释放音频焦点
//!   - [`stream_state`]：读取当前增益与焦点状态（audiod 按优先级策略计算）
//!   - [`close_stream`]：注销流
//!
//! # 与 audiod 的协议（文件 IPC，与 displayd 风格一致）
//!
//! 控制通道 `/var/run/audiod.ctl`（行命令）：
//!   `OPEN <pid> <usage>` / `CLOSE <pid>` / `FOCUS <pid>` / `RELEASE <pid>`
//!
//! 数据通道 `/var/run/audio/<pid>.in`（双槽位环形，槽 = 1928 字节）：
//!   ```
//!   slot = [seq:u32][len:u32][pcm:1920B]      // 共 1928 字节
//!   文件 = slot0 | slot1                        // 共 3856 字节
//!   写入端按 seq 奇偶选择槽位，从槽首覆盖写（无 truncate，无跨槽竞争）
//!   ```
//! audiod 每 5ms 轮询一次，取「序号更新且完整」的槽；序号保证新旧判别与
//! 撕裂写检测。
//!
//! 状态通道 `/var/run/audio/<pid>.st`（audiod 写，应用读）：
//!   `"<gain_Q15> <focus_state> <acked_seq>\n"`
//!
//! # 焦点策略（audiod 执行）
//!
//! 优先级：电话(0) > 导航(1) > 音乐(2) > 游戏(3)。持焦的最高优先级应用
//! 全音量；电话持焦时其余全部静音；更低优先级应用被压低（0.25 增益）。

use core::arch::asm;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_IOCTL: usize = 29;
const SYS_GETPID: usize = 172;
const SYS_NANOSLEEP: usize = 101;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;

const SEEK_SET: usize = 0;

// ==================== 协议常量 ====================

/// 用途类型：电话（最高优先级）。
pub const USAGE_PHONE: u8 = 0;
/// 用途类型：导航。
pub const USAGE_NAV: u8 = 1;
/// 用途类型：音乐。
pub const USAGE_MUSIC: u8 = 2;
/// 用途类型：游戏（最低优先级）。
pub const USAGE_GAME: u8 = 3;

/// 焦点状态：持有焦点（全音量）。
pub const FOCUS_FOCUSED: u32 = 0;
/// 焦点状态：被更高优先级抢占（压低音量）。
pub const FOCUS_DUCKED: u32 = 1;
/// 焦点状态：被电话抢占（静音）。
pub const FOCUS_MUTED: u32 = 2;

/// 每块帧数（10ms @ 48kHz）。
pub const CHUNK_FRAMES: usize = 480;
/// 每块字节数（480 帧 × 2 声道 × 2 字节）。
pub const CHUNK_BYTES: usize = 1920;
/// 采样率（Hz）。
pub const SAMPLE_RATE: u32 = 48000;

/// 数据槽字节数（8 字节头 + 1920 字节 PCM）。
const SLOT_BYTES: usize = 8 + CHUNK_BYTES;
/// 数据文件字节数（双槽）。
const FILE_BYTES: usize = 2 * SLOT_BYTES;

/// audiod 控制文件路径。
pub const CTL_PATH: &str = "/var/run/audiod.ctl";
/// 客户端数据目录（含尾斜杠；文件名 = `<pid>.in` / `<pid>.st`）。
pub const DATA_DIR: &str = "/var/run/audio/";

/// 打开流失败：audiod 未运行或注册被拒。
pub const ERR_NO_SERVICE: i32 = -1;
/// 写入失败：块尺寸错误。
pub const ERR_BAD_CHUNK: i32 = -2;

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
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

// ==================== 内部状态 ====================

/// 本进程的流状态。
static mut SEQ: u32 = 0;
static mut OPENED: bool = false;
static mut PID: i32 = -1;
/// 路径缓冲（避免每次构造）。
static mut PATH_BUF: [u8; 64] = [0; 64];

/// 系统调用写。
unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

/// 打开文件（路径 NUL 结尾于 `PATH_BUF`）。
unsafe fn open_at(flags: usize) -> i32 {
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as i32
}

fn build_path(tag: &str, pid: i32) -> usize {
    unsafe {
        let p = PATH_BUF.as_mut_ptr();
        let d = DATA_DIR.as_bytes();
        let t = tag.as_bytes();
        let mut n = 0usize;
        for &c in d.iter().take(48) {
            *p.add(n) = c;
            n += 1;
        }
        // pid 十进制
        let mut tmp = [0u8; 12];
        let mut i = tmp.len();
        let mut v = pid as u32;
        if v == 0 {
            tmp[tmp.len() - 1] = b'0';
            i = tmp.len() - 1;
        } else {
            while v > 0 {
                i -= 1;
                tmp[i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
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
        n
    }
}

// ==================== 客户端 API ====================

/// 向 audiod 注册一个流（用途类型见 [`USAGE_PHONE`] 等）。
///
/// 成功返回 0；audiod 未运行 / 注册失败返回负值。注册后 audiod 会创建
/// `/var/run/audio/<pid>.in` 数据文件。
pub fn open_stream(usage: u8) -> i32 {
    unsafe {
        if OPENED {
            return 0;
        }
        PID = syscall1(SYS_GETPID, 0) as i32;
        // 写入控制命令 "OPEN <pid> <usage>\n"
        let mut line = [0u8; 32];
        let mut n = 0usize;
        for &c in b"OPEN ".iter() {
            line[n] = c;
            n += 1;
        }
        let mut tmp = [0u8; 12];
        let mut i = tmp.len();
        let mut v = PID as u32;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in tmp.iter().skip(i) {
            line[n] = c;
            n += 1;
        }
        line[n] = b' ';
        n += 1;
        line[n] = b'0' + (usage % 10);
        n += 1;
        line[n] = b'\n';
        n += 1;

        // 打开控制文件并写入 "OPEN <pid> <usage>\\n"；audiod 可能尚未就绪
        // （守护进程 execve 比应用慢），重试最多 3 秒（50ms 间隔）。
        let ctl_path = b"/var/run/audiod.ctl\0";
        let mut attempts = 0usize;
        let mut ok = false;
        while attempts < 60 {
            let fd = syscall3(
                SYS_OPENAT,
                AT_FDCWD,
                ctl_path.as_ptr() as usize,
                O_RDWR | O_CREAT,
            ) as i32;
            if fd >= 0 {
                let r = syscall3(SYS_WRITE, fd as usize, line.as_ptr() as usize, n);
                syscall1(SYS_CLOSE, fd as usize);
                // write 失败返回 -errno 的 usize 原值（巨大正数），必须按
                // isize 比较且要求全等——否则失败被误判"注册成功"
                if r as isize == n as isize {
                    ok = true;
                    break;
                }
            }
            attempts += 1;
            // 等待 audiod 创建控制文件（轻量短睡）
            let mut req = [0u8; 16];
            let nsec = 50_000_000u64;
            req[8..].copy_from_slice(&nsec.to_le_bytes());
            syscall3(SYS_NANOSLEEP, req.as_ptr() as usize, 0, 0);
        }
        if !ok {
            return ERR_NO_SERVICE;
        }
        OPENED = true;
        SEQ = 0;
        0
    }
}

/// 写入一块 10ms PCM（`CHUNK_BYTES` 字节，16bit 立体声小端）。
///
/// 阻塞式（内部按需短睡）：调用频率应约为 100Hz（每 10ms 一块）。
pub fn write_chunk(pcm: &[u8]) -> i32 {
    unsafe {
        if !OPENED || pcm.len() != CHUNK_BYTES {
            return ERR_BAD_CHUNK;
        }
        let seq = SEQ.wrapping_add(1);
        let slot = ((seq & 1) as usize) * SLOT_BYTES;
        let _ = build_path(".in", PID);
        let fd = open_at(O_RDWR | O_CREAT);
        if fd < 0 {
            return ERR_NO_SERVICE;
        }
        // 定位到槽首（lseek 绝对位置）
        syscall3(SYS_LSEEK, fd as usize, slot, SEEK_SET);
        // 写 [seq][len][pcm]
        let mut hdr = [0u8; 8];
        hdr[0..4].copy_from_slice(&seq.to_le_bytes());
        hdr[4..8].copy_from_slice(&(CHUNK_BYTES as u32).to_le_bytes());
        let mut hw = syscall3(SYS_WRITE, fd as usize, hdr.as_ptr() as usize, 8);
        if hw == 8 {
            hw += syscall3(SYS_WRITE, fd as usize, pcm.as_ptr() as usize, pcm.len());
        }
        syscall1(SYS_CLOSE, fd as usize);
        if hw == 8 + CHUNK_BYTES {
            SEQ = seq;
            0
        } else {
            ERR_NO_SERVICE
        }
    }
}

/// 申请音频焦点（按用途优先级参与仲裁；电话会抢占其它应用）。
pub fn request_focus() -> i32 {
    ctl_cmd(b"FOCUS")
}

/// 释放音频焦点（让出给其它应用）。
pub fn release_focus() -> i32 {
    ctl_cmd(b"RELEASE")
}

/// 注销流（audiod 清理数据文件与混音槽位）。
pub fn close_stream() -> i32 {
    let r = ctl_cmd(b"CLOSE");
    unsafe { OPENED = false };
    r
}

/// 写一条控制命令（FOCUS/RELEASE/CLOSE）。
fn ctl_cmd(cmd: &[u8]) -> i32 {
    unsafe {
        if !OPENED {
            return ERR_NO_SERVICE;
        }
        let mut line = [0u8; 32];
        let mut n = 0usize;
        for &c in cmd.iter().take(8) {
            line[n] = c;
            n += 1;
        }
        line[n] = b' ';
        n += 1;
        let mut tmp = [0u8; 12];
        let mut i = tmp.len();
        let mut v = PID as u32;
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        for &c in tmp.iter().skip(i) {
            line[n] = c;
            n += 1;
        }
        line[n] = b'\n';
        n += 1;
        let ctl_path = b"/var/run/audiod.ctl\0";
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, ctl_path.as_ptr() as usize, O_RDWR) as i32;
        if fd < 0 {
            return ERR_NO_SERVICE;
        }
        let r = syscall3(SYS_WRITE, fd as usize, line.as_ptr() as usize, n);
        syscall1(SYS_CLOSE, fd as usize);
        // write 失败 -errno 的 usize 原值按 isize 比较（否则 FOCUS/RELEASE/
        // CLOSE 失败返回 0 假成功）
        if r as isize != n as isize {
            ERR_NO_SERVICE
        } else {
            0
        }
    }
}

/// 读取当前流状态：返回 `(gain_Q15, focus_state)`；audiod 未就绪返回
/// `(0, FOCUS_MUTED)` 并置 `ok = false`。
///
/// gain_Q15：32768 = 满音量，8192 = 0.25（被压低），0 = 静音。
pub fn stream_state() -> (i32, u32, bool) {
    unsafe {
        if !OPENED {
            return (0, FOCUS_MUTED, false);
        }
        let _ = build_path(".st", PID);
        let fd = open_at(O_RDONLY);
        if fd < 0 {
            return (0, FOCUS_MUTED, false);
        }
        let mut buf = [0u8; 32];
        // ★ 按 isize 判负：read 失败 -errno 的 usize 原值是 usize::MAX，
        // `n < 4` 拦不住 → 把陈旧缓冲当有效状态（wifid 同族 bug）。
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        if n < 4 {
            return (0, FOCUS_MUTED, false);
        }
        let n = n as usize;
        // 解析 "<gain> <state>"
        let mut gain: i32 = 0;
        let mut i = 0usize;
        let mut neg = false;
        if buf[0] == b'-' {
            neg = true;
            i = 1;
        }
        while i < n as usize && buf[i].is_ascii_digit() {
            gain = gain * 10 + (buf[i] - b'0') as i32;
            i += 1;
        }
        if neg {
            gain = -gain;
        }
        while i < n as usize && buf[i] == b' ' {
            i += 1;
        }
        let mut state: u32 = 0;
        while i < n as usize && buf[i].is_ascii_digit() {
            state = state * 10 + (buf[i] - b'0') as u32;
            i += 1;
        }
        (gain, state, true)
    }
}

/// 短睡（毫秒）。
pub fn sleep_ms(ms: usize) {
    unsafe {
        let mut req = [0u8; 16];
        let sec = (ms / 1000) as u64;
        let nsec = ((ms % 1000) * 1_000_000) as u64;
        req[..8].copy_from_slice(&sec.to_le_bytes());
        req[8..].copy_from_slice(&nsec.to_le_bytes());
        syscall3(SYS_NANOSLEEP, req.as_ptr() as usize, 0, 0);
    }
}
