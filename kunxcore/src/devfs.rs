//! 设备文件：字符设备驱动注册表，以及 /dev/null、/dev/zero、/dev/tty。
//!
//! 字符设备以 (major, ops) 标识，`minor` 用于区分同一主设备号下的不同设备。
//! 设备文件本身由 `vfs::mknod` 在文件系统中创建为 CHR 类型 inode，并携带
//! 编码后的 (major, minor)；读写时由 VFS 按设备号分发到本模块。

use crate::uart_send_char;

/// 内存设备主设备号（null/zero）。
pub const MEM_MAJOR: u32 = 1;
/// 终端设备主设备号（tty）。
pub const TTY_MAJOR: u32 = 2;
/// Modem 设备主设备号（/dev/modem 与 /dev/modemctl）。
pub const MODEM_MAJOR: u32 = 8;
/// 伪终端分配器主设备号（ptmx）。
pub const PTY_MAJOR: u32 = 5;
/// /dev/ptmx 的次设备号。
pub const PTMX_MINOR: u32 = 0;
/// 输入事件设备主设备号（evdev）。
pub const INPUT_MAJOR: u32 = 13;
/// 帧缓冲设备主设备号（Linux fb major = 29）。
pub const FB_MAJOR: u32 = 29;
/// 音频设备主设备号（Linux 传统 sound major；audio.rs 亦引用）。
pub const AUDIO_MAJOR: u32 = 14;

/// /dev/null 的次设备号。
pub const NULL_MINOR: u32 = 0;
/// /dev/zero 的次设备号。
pub const ZERO_MINOR: u32 = 1;
/// /dev/tty（UART 直通）的次设备号。
pub const TTY_MINOR: u32 = 0;
/// 终端会话起始次设备号：/dev/tty1.. 对应会话 1..MAX_TTY（会话 0 绑定内核 Shell）。
pub const TTY_SESSION_BASE: u32 = 1;
/// 会话终端数量（/dev/tty1..ttyN），与 `crate::tty::MAX_TTY` 保持一致。
pub const TTY_SESSION_MAX: u32 = 4;

/// TIOCGWINSZ：查询窗口尺寸（winsize {ws_row, ws_col, ws_xpixel, ws_ypixel}）。
pub const TIOCGWINSZ: u32 = 0x5413;
/// TIOCSWINSZ：设置窗口尺寸。
pub const TIOCSWINSZ: u32 = 0x5414;
/// TIOCGPTN：返回 ptmx 分配的从侧设备号。
pub const TIOCGPTN: u32 = 0x5430;
/// /dev/input/event0 的次设备号。
pub const INPUT_MINOR: u32 = 0;
/// /dev/fb0 的次设备号。
pub const FB_MINOR: u32 = 0;
/// /dev/modem（AT 字节流）的次设备号。
pub const MODEM_MINOR: u32 = 0;
/// /dev/modemctl（虚拟调制解调器事件注入）的次设备号。
pub const MODEMCTL_MINOR: u32 = 1;
/// /dev/audio 的次设备号。
pub const AUDIO_MINOR: u32 = 0;

/// FBIOGET_VSCREENINFO 请求码（ioctl('F', 0)）。
pub const FBIOGET_VSCREENINFO: u32 = 0x4600;
/// FBIO_FADE_SPLASH 请求码：触发启动画面线性淡出（用户态首帧渲染完成后调用）。
pub const FBIO_FADE_SPLASH: u32 = 0x4601;
/// FBIO_PUT_DAMAGE 请求码：应用直绘脏区域提交（读用户缓冲区，见 linux_compat）。
pub const FBIO_PUT_DAMAGE: u32 = 0x4602;
/// FBIO_PRESENT 请求码：应用帧呈现（合成器提交 + 翻页）。
pub const FBIO_PRESENT: u32 = 0x4603;

/// Modem 控制请求码（ioctl）：查询调制解调器是否存在（1=存在）。
pub const MODEM_IOCTL_PRESENT: u32 = 0x4800;
/// Modem 控制请求码（ioctl）：查询虚拟调制解调器当前注入状态（调试用）。
pub const MODEM_IOCTL_STATUS: u32 = 0x4801;

/// 音频控制请求码（ioctl，见 audio.rs）。TONE 与 RING_START 的入参均为
/// `[freq:u32, dur_ms:u32]`（8 字节，内核从用户缓冲读取）；ACQUIRE/RELEASE
/// 入参为 `[owner:u32]`；SET_ROUTE 入参为 `[route:u32]`；GET_STATE 出参为
/// `[focus:u32, route:u32]`。
pub const AUDIO_IOCTL_TONE: u32 = 0x4105;
pub const AUDIO_IOCTL_RING_START: u32 = 0x4106;
pub const AUDIO_IOCTL_RING_STOP: u32 = 0x4107;
pub const AUDIO_IOCTL_ACQUIRE: u32 = 0x4108;
pub const AUDIO_IOCTL_RELEASE: u32 = 0x4109;
pub const AUDIO_IOCTL_GET_STATE: u32 = 0x410A;
pub const AUDIO_IOCTL_SET_ROUTE: u32 = 0x410B;

const MAX_CHRDEV: usize = 12;

pub type ChrReadFn = fn(minor: u32, flags: u32, offset: usize, buf: &mut [u8]) -> i32;
pub type ChrWriteFn = fn(minor: u32, buf: &[u8]) -> i32;
pub type ChrIoctlFn = fn(minor: u32, request: u32, out: &mut [u8]) -> i32;

/// 字符设备操作集。
#[derive(Clone, Copy)]
pub struct ChrOps {
    pub read: ChrReadFn,
    pub write: ChrWriteFn,
    pub ioctl: ChrIoctlFn,
}

/// 字符设备描述。
#[derive(Clone, Copy)]
pub struct ChrDev {
    pub major: u32,
    pub name: &'static str,
    pub ops: ChrOps,
}

fn nop_read(_minor: u32, _flags: u32, _offset: usize, _buf: &mut [u8]) -> i32 {
    -1
}
fn nop_write(_minor: u32, _buf: &[u8]) -> i32 {
    -1
}
fn nop_ioctl(_minor: u32, _request: u32, _out: &mut [u8]) -> i32 {
    -25 // ENOTTY
}

const EMPTY_CHRDEV: ChrDev = ChrDev {
    major: 0,
    name: "",
    ops: ChrOps { read: nop_read, write: nop_write, ioctl: nop_ioctl },
};

/// 字符设备表（位于 .bss，name 为空表示槽位空闲）。
static mut CHRDEVS: [ChrDev; MAX_CHRDEV] = [EMPTY_CHRDEV; MAX_CHRDEV];

/// 注册字符设备 `major`，返回 0 成功、-1 表满。
pub fn register_chrdev(major: u32, name: &'static str, ops: ChrOps) -> i32 {
    unsafe {
        for i in 0..MAX_CHRDEV {
            if CHRDEVS[i].name.is_empty() {
                CHRDEVS[i] = ChrDev { major, name, ops };
                return 0;
            }
        }
    }
    -1
}

/// 从字符设备 (major, minor) 读取，返回字节数；未注册返回 -1。
pub fn chr_read(major: u32, minor: u32, flags: u32, offset: usize, buf: &mut [u8]) -> i32 {
    unsafe {
        for i in 0..MAX_CHRDEV {
            let d = CHRDEVS[i];
            if !d.name.is_empty() && d.major == major {
                return (d.ops.read)(minor, flags, offset, buf);
            }
        }
    }
    -1
}

/// 向字符设备 (major, minor) 写入，返回字节数；未注册返回 -1。
pub fn chr_write(major: u32, minor: u32, buf: &[u8]) -> i32 {
    unsafe {
        for i in 0..MAX_CHRDEV {
            let d = CHRDEVS[i];
            if !d.name.is_empty() && d.major == major {
                return (d.ops.write)(minor, buf);
            }
        }
    }
    -1
}

/// 向字符设备 (major, minor) 下发 ioctl，返回输出字节数或负 errno。
pub fn chr_ioctl(major: u32, minor: u32, request: u32, out: &mut [u8]) -> i32 {
    unsafe {
        for i in 0..MAX_CHRDEV {
            let d = CHRDEVS[i];
            if !d.name.is_empty() && d.major == major {
                return (d.ops.ioctl)(minor, request, out);
            }
        }
    }
    -25 // ENOTTY
}

// ==================== 设备实现 ====================

/// /dev/null（读返回 EOF）、/dev/zero（读返回零）。
fn mem_read(minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    match minor {
        NULL_MINOR => 0,
        ZERO_MINOR => {
            buf.fill(0);
            buf.len() as i32
        }
        _ => -1,
    }
}

/// /dev/null、/dev/zero：写即丢弃。
fn mem_write(minor: u32, buf: &[u8]) -> i32 {
    match minor {
        NULL_MINOR | ZERO_MINOR => buf.len() as i32,
        _ => -1,
    }
}

/// /dev/tty（minor 0）：非阻塞读取 UART 接收 FIFO 中已有字节。
/// /dev/ttyN（minor ≥ 1 的会话终端，图形终端主侧）：读取从侧（Shell）
/// 镜像到 out_q 的屏幕输出字节。
fn tty_read(minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    if minor == TTY_MINOR {
        let mut n = 0usize;
        while n < buf.len() {
            match crate::uart_try_read_char() {
                Some(c) => {
                    buf[n] = c;
                    n += 1;
                }
                None => break,
            }
        }
        return n as i32;
    }
    crate::tty::screen_get(minor, buf) as i32
}

/// /dev/tty（minor 0）：把字节写入串口（\n 转换为 \r\n，保证终端换行正确）。
/// /dev/ttyN（会话终端）：主侧写入键码/按键序列，进入 in_q 供 Shell 读取。
fn tty_write(minor: u32, buf: &[u8]) -> i32 {
    if minor == TTY_MINOR {
        for &c in buf {
            if c == b'\n' {
                uart_send_char(b'\r');
            }
            uart_send_char(c);
        }
        return buf.len() as i32;
    }
    crate::tty::screen_put(minor, buf) as i32
}

// ==================== 帧缓冲设备 ====================

/// 帧缓冲物理基址（0 表示未初始化；合成器就绪后由 `compositor::fb_target` 取代）。
static mut FB_BASE: usize = 0;
/// 帧缓冲字节长度。
static mut FB_LEN: usize = 0;
/// 帧缓冲几何（由 gpu_simple 初始化）。
static mut FB_W: u32 = 0;
static mut FB_H: u32 = 0;
static mut FB_STRIDE: u32 = 0;

/// 初始化帧缓冲设备：记录由 `gpu_simple` 分配的显存与几何。
pub fn fb_init(base: usize, len: usize, w: u32, h: u32, stride: u32) {
    unsafe {
        FB_BASE = base;
        FB_LEN = len;
        FB_W = w;
        FB_H = h;
        FB_STRIDE = stride;
    }
}

/// 合成器登记其持久画布为 `/dev/fb0` 的目标（应用直绘接入后）。
pub fn fb_set_target(canvas: usize) {
    unsafe { FB_BASE = canvas; }
}

/// 返回帧缓冲映射信息 `(物理基址, 字节长度)`，供 `mmap` 映射到用户空间。
///
/// 合成器就绪后返回**持久应用画布**（固定物理页，不随页翻转旋转）；
/// 合成器未就绪时返回启动阶段的显存基址。
pub fn fb_map_info() -> (usize, usize) {
    unsafe {
        if crate::compositor::ready() {
            (crate::compositor::fb_target(), FB_LEN)
        } else {
            (FB_BASE, FB_LEN)
        }
    }
}

/// /dev/fb0：从显存拷贝读出（无流偏移，始终从基址开始）。
fn fb_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    unsafe {
        let (base, len) = fb_map_info();
        if base == 0 || len == 0 {
            return -1;
        }
        let n = buf.len().min(len);
        core::ptr::copy_nonoverlapping(base as *const u8, buf.as_mut_ptr(), n);
        n as i32
    }
}

/// /dev/fb0：把像素数据写入显存（写入即视为整屏变更，登记整屏脏区）。
fn fb_write(_minor: u32, buf: &[u8]) -> i32 {
    unsafe {
        let (base, len) = fb_map_info();
        if base == 0 || len == 0 {
            return -1;
        }
        let n = buf.len().min(len);
        core::ptr::copy_nonoverlapping(buf.as_ptr(), base as *mut u8, n);
        n as i32
    }
}

/// /dev/fb0：返回可变屏幕信息（24 字节，与 launcher 约定布局一致）。
fn fb_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    if request == FBIO_FADE_SPLASH {
        crate::splash_fade();
        return 0;
    }
    if request != FBIOGET_VSCREENINFO {
        return -25; // ENOTTY
    }
    if out.len() < 24 {
        return -1;
    }
    unsafe {
        let w = FB_W;
        let h = FB_H;
        let stride = FB_STRIDE;
        // 布局：xres, yres, xres_virtual, yres_virtual, bits_per_pixel, stride
        out[0..4].copy_from_slice(&w.to_le_bytes());
        out[4..8].copy_from_slice(&h.to_le_bytes());
        out[8..12].copy_from_slice(&w.to_le_bytes());
        out[12..16].copy_from_slice(&h.to_le_bytes());
        out[16..20].copy_from_slice(&32u32.to_le_bytes());
        out[20..24].copy_from_slice(&stride.to_le_bytes());
    }
    24
}

/// /dev/ttyN 会话终端 ioctl：窗口尺寸查询/设置。
fn tty_ioctl(minor: u32, request: u32, out: &mut [u8]) -> i32 {
    match request {
        TIOCGWINSZ => {
            if out.len() < 8 {
                return -25;
            }
            let (cols, rows) = crate::tty::win_size(minor);
            // winsize 布局：ws_row, ws_col, ws_xpixel, ws_ypixel（全 u16）。
            out[0..2].copy_from_slice(&rows.to_le_bytes());
            out[2..4].copy_from_slice(&cols.to_le_bytes());
            out[4..8].fill(0);
            8
        }
        TIOCSWINSZ => {
            if out.len() < 8 {
                return -25;
            }
            let rows = u16::from_le_bytes([out[0], out[1]]);
            let cols = u16::from_le_bytes([out[2], out[3]]);
            crate::tty::set_win_size(minor, cols, rows);
            0
        }
        _ => -25, // ENOTTY
    }
}

/// /dev/ptmx：主侧写入键码 → 路由到 ptmx 预留会话的 in_q。
fn ptmx_write(_minor: u32, buf: &[u8]) -> i32 {
    crate::tty::screen_put(crate::tty::ptmx_minor(), buf) as i32
}

/// /dev/ptmx：主侧读 → 取预留会话 out_q 的屏幕字节。
fn ptmx_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    crate::tty::screen_get(crate::tty::ptmx_minor(), buf) as i32
}

/// /dev/ptmx ioctl：TIOCGPTN 返回预留会话的从侧次设备号（供打开 /dev/ttyN）。
fn ptmx_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
    if request != TIOCGPTN {
        return -25; // ENOTTY
    }
    if out.len() < 4 {
        return -1;
    }
    let minor = crate::tty::ptmx_alloc();
    out[0..4].copy_from_slice(&minor.to_le_bytes());
    4
}

// ==================== 初始化 ====================

/// 初始化字符设备：注册 mem（null/zero）、tty（含会话终端）、ptmx 与帧缓冲 fb。
pub fn init() {
    register_chrdev(
        MEM_MAJOR,
        "mem",
        ChrOps { read: mem_read, write: mem_write, ioctl: nop_ioctl },
    );
    register_chrdev(
        TTY_MAJOR,
        "tty",
        ChrOps { read: tty_read, write: tty_write, ioctl: tty_ioctl },
    );
    register_chrdev(
        PTY_MAJOR,
        "ptmx",
        ChrOps { read: ptmx_read, write: ptmx_write, ioctl: ptmx_ioctl },
    );
    register_chrdev(
        FB_MAJOR,
        "fb",
        ChrOps { read: fb_read, write: fb_write, ioctl: fb_ioctl },
    );
    register_chrdev(
        KMSG_MAJOR,
        "kmsg",
        ChrOps { read: kmsg_read, write: kmsg_write, ioctl: nop_ioctl },
    );
}

// ==================== /dev/kmsg（结构化内核日志） ====================

/// /dev/kmsg 主设备号（与 Linux 一致：10/1 之外的自定义值，此处用 11）。
pub const KMSG_MAJOR: u32 = 11;
/// /dev/kmsg 次设备号。
pub const KMSG_MINOR: u32 = 1;

/// /dev/kmsg：增量读取下一条结构化日志记录（全局游标，供 logd tail）。
/// 无新记录返回 0（EOF）。
fn kmsg_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    crate::log::log_read_next(buf) as i32
}

/// /dev/kmsg：注入用户态日志（`<level>[tag] msg` 或 `<level> tag: msg`），
/// 镜像到控制台（logcat 等价物的内核侧通道）。
fn kmsg_write(_minor: u32, buf: &[u8]) -> i32 {
    crate::log::log_inject(buf)
}
