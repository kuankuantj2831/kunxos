//! procfs：只读虚拟文件系统（挂载于 /proc）。
//!
//! 复用 devfs 的字符设备机制：/proc 下的每个文件是一个 CHR inode，
//! 主设备号 `PROC_MAJOR`、次设备号编码文件 id。读取时按次设备号
//! 动态生成内容（版本 / 内存 / 运行时长 / CPU / 进程表），支持 offset
//! 分次读取（cat 循环读直到 0）。

use crate::devfs::{self, ChrOps};

/// procfs 主设备号（复用 Linux /proc 无主设备号约定，此处自定义 9）。
pub const PROC_MAJOR: u32 = 9;

/// 次设备号：各虚拟文件。
pub const VERSION_MINOR: u32 = 0;
pub const MEMINFO_MINOR: u32 = 1;
pub const UPTIME_MINOR: u32 = 2;
pub const CPUINFO_MINOR: u32 = 3;
pub const PROCESSES_MINOR: u32 = 4;

/// 内容缓冲（任何单个文件内容都远小于此）。
const BUF_CAP: usize = 1024;
static mut BUF: [u8; BUF_CAP] = [0u8; BUF_CAP];

/// 把十进制数写入 buf[pos..]，返回新位置。
fn push_dec(buf: &mut [u8], mut pos: usize, mut v: u64) -> usize {
    if v == 0 {
        if pos < buf.len() {
            buf[pos] = b'0';
            return pos + 1;
        }
        return pos;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    if pos + n <= buf.len() {
        buf[pos..pos + n].copy_from_slice(&tmp[i..]);
    }
    pos + n
}

fn push_str(buf: &mut [u8], mut pos: usize, s: &str) -> usize {
    let b = s.as_bytes();
    let n = b.len().min(buf.len().saturating_sub(pos));
    if n > 0 {
        buf[pos..pos + n].copy_from_slice(&b[..n]);
    }
    pos + n
}

/// 生成指定文件的内容，返回长度。
fn render(minor: u32) -> usize {
    unsafe {
        let buf = &mut BUF;
        buf.fill(0);
        match minor {
            VERSION_MINOR => push_str(buf, 0, "KunxOS 0.1.0 (aarch64) rust-kernel\n"),
            UPTIME_MINOR => {
                let mut pos = push_dec(buf, 0, crate::timer::now_us() / 1_000_000);
                push_str(buf, pos, " seconds since boot\n")
            }
            MEMINFO_MINOR => {
                let total = (crate::allocator::total_pages() as u64) * 4096 / 1024;
                let used = (crate::allocator::used_pages() as u64) * 4096 / 1024;
                let mut pos = push_str(buf, 0, "MemTotal:       ");
                pos = push_dec(buf, pos, total);
                pos = push_str(buf, pos, " kB\nMemUsed:        ");
                pos = push_dec(buf, pos, used);
                push_str(buf, pos, " kB\n")
            }
            CPUINFO_MINOR => {
                let freq = kunxhal::current_platform().timer_freq();
                let mut pos = push_str(buf, 0, "processor\t: 0\nBogoMIPS\t: 100.00\n");
                pos = push_str(buf, pos, "Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32\n");
                pos = push_str(buf, pos, "CPU implementer\t: 0x41\nCPU part\t: 0xd08\n");
                push_str(buf, pos, "CPU frequency\t: 0 MHz\n")
            }
            PROCESSES_MINOR => {
                let mut pos = push_str(buf, 0, "PID  STATE  NAME\n");
                crate::sched_cfs::for_each_task(|pid, state, name| {
                    pos = push_dec(buf, pos, pid as u64);
                    pos = push_str(buf, pos, "     ");
                    pos = push_str(buf, pos, state);
                    pos = push_str(buf, pos, "  ");
                    pos = push_str(buf, pos, name);
                    pos = push_str(buf, pos, "\n");
                });
                pos
            }
            _ => 0,
        }
    }
}

/// procfs 读：按 offset 返回内容片段，读完返回 0（EOF）。
fn proc_read(minor: u32, _flags: u32, offset: usize, buf: &mut [u8]) -> i32 {
    let len = render(minor);
    if offset >= len {
        return 0;
    }
    let n = (len - offset).min(buf.len());
    unsafe {
        buf[..n].copy_from_slice(&BUF[offset..offset + n]);
    }
    n as i32
}

fn nop_write(_minor: u32, _buf: &[u8]) -> i32 {
    -1
}
fn nop_ioctl(_minor: u32, _request: u32, _out: &mut [u8]) -> i32 {
    -1
}

/// 初始化：注册 proc 字符设备，并在 /proc 下创建虚拟文件。
pub fn init() {
    devfs::register_chrdev(
        PROC_MAJOR,
        "proc",
        ChrOps { read: proc_read, write: nop_write, ioctl: nop_ioctl },
    );
    crate::vfs::mkdir("/proc");
    let _ = crate::vfs::mknod("/proc/version", crate::vfs::FileType::Chr, PROC_MAJOR, VERSION_MINOR);
    let _ = crate::vfs::mknod("/proc/meminfo", crate::vfs::FileType::Chr, PROC_MAJOR, MEMINFO_MINOR);
    let _ = crate::vfs::mknod("/proc/uptime", crate::vfs::FileType::Chr, PROC_MAJOR, UPTIME_MINOR);
    let _ = crate::vfs::mknod("/proc/cpuinfo", crate::vfs::FileType::Chr, PROC_MAJOR, CPUINFO_MINOR);
    let _ = crate::vfs::mknod("/proc/processes", crate::vfs::FileType::Chr, PROC_MAJOR, PROCESSES_MINOR);
}
