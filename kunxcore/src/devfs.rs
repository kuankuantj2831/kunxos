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
/// 输入事件设备主设备号（evdev）。
pub const INPUT_MAJOR: u32 = 13;
/// 帧缓冲设备主设备号（Linux fb major = 29）。
pub const FB_MAJOR: u32 = 29;

/// /dev/null 的次设备号。
pub const NULL_MINOR: u32 = 0;
/// /dev/zero 的次设备号。
pub const ZERO_MINOR: u32 = 1;
/// /dev/tty 的次设备号。
pub const TTY_MINOR: u32 = 0;
/// /dev/input/event0 的次设备号。
pub const INPUT_MINOR: u32 = 0;
/// /dev/fb0 的次设备号。
pub const FB_MINOR: u32 = 0;

/// FBIOGET_VSCREENINFO 请求码（ioctl('F', 0)）。
pub const FBIOGET_VSCREENINFO: u32 = 0x4600;

const MAX_CHRDEV: usize = 8;

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

/// /dev/tty：非阻塞读取 UART 接收 FIFO 中已有字节。
fn tty_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
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
    n as i32
}

/// /dev/tty：把字节写入串口（\n 转换为 \r\n，保证终端换行正确）。
fn tty_write(_minor: u32, buf: &[u8]) -> i32 {
    for &c in buf {
        if c == b'\n' {
            uart_send_char(b'\r');
        }
        uart_send_char(c);
    }
    buf.len() as i32
}

// ==================== 帧缓冲设备 ====================

/// 帧缓冲物理基址（0 表示未初始化）。
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

/// 返回帧缓冲映射信息 `(物理基址, 字节长度)`，供 `mmap` 映射到用户空间。
pub fn fb_map_info() -> (usize, usize) {
    unsafe { (FB_BASE, FB_LEN) }
}

/// /dev/fb0：从显存拷贝读出（无流偏移，始终从基址开始）。
fn fb_read(_minor: u32, _flags: u32, _offset: usize, buf: &mut [u8]) -> i32 {
    unsafe {
        if FB_BASE == 0 || FB_LEN == 0 {
            return -1;
        }
        let n = buf.len().min(FB_LEN);
        core::ptr::copy_nonoverlapping(FB_BASE as *const u8, buf.as_mut_ptr(), n);
        n as i32
    }
}

/// /dev/fb0：把像素数据写入显存。
fn fb_write(_minor: u32, buf: &[u8]) -> i32 {
    unsafe {
        if FB_BASE == 0 || FB_LEN == 0 {
            return -1;
        }
        let n = buf.len().min(FB_LEN);
        core::ptr::copy_nonoverlapping(buf.as_ptr(), FB_BASE as *mut u8, n);
        n as i32
    }
}

/// /dev/fb0：返回可变屏幕信息（24 字节，与 launcher 约定布局一致）。
fn fb_ioctl(_minor: u32, request: u32, out: &mut [u8]) -> i32 {
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

// ==================== 初始化 ====================

/// 初始化字符设备：注册 mem（null/zero）、tty 与帧缓冲 fb。
pub fn init() {
    register_chrdev(
        MEM_MAJOR,
        "mem",
        ChrOps { read: mem_read, write: mem_write, ioctl: nop_ioctl },
    );
    register_chrdev(
        TTY_MAJOR,
        "tty",
        ChrOps { read: tty_read, write: tty_write, ioctl: nop_ioctl },
    );
    register_chrdev(
        FB_MAJOR,
        "fb",
        ChrOps { read: fb_read, write: fb_write, ioctl: fb_ioctl },
    );
}
