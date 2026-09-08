//! 文件系统 I/O（经 Linux 兼容层系统调用）：openat/read/write/close/
//! mkdirat/unlinkat/getdents64 + 路径工具。no_std、静态缓冲。

use core::arch::asm;

// ==================== 系统调用号（AArch64 asm-generic） ====================

pub const SYS_MKDIRAT: usize = 34;
pub const SYS_UNLINKAT: usize = 35;
pub const SYS_OPENAT: usize = 56;
pub const SYS_CLOSE: usize = 57;
pub const SYS_GETDENTS64: usize = 61;
pub const SYS_READ: usize = 63;
pub const SYS_WRITE: usize = 64;
pub const SYS_LSEEK: usize = 62;

// ==================== Linux 常量 ====================

pub const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C; // -100
pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_RDWR: usize = 2;
pub const O_CREAT: usize = 0x40;
pub const O_TRUNC: usize = 0x200;

// ==================== 静态缓冲 ====================

/// 路径临时缓冲。
static mut PATH_BUF: [u8; 256] = [0; 256];
/// getdents64 缓冲。
static mut DIR_BUF: [u8; 4096] = [0; 4096];

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

// ==================== 路径工具 ====================

/// 把 `a` 与 `b` 拼接进 `out` 并以 NUL 结尾，返回长度（不含 NUL）。
pub fn path_join(out: &mut [u8], a: &[u8], b: &[u8]) -> usize {
    let mut n = 0usize;
    for &c in a {
        if n < out.len() - 1 {
            out[n] = c;
            n += 1;
        }
    }
    for &c in b {
        if n < out.len() - 1 {
            out[n] = c;
            n += 1;
        }
    }
    if !out.is_empty() {
        out[n] = 0;
    }
    n
}

/// 三片段拼接。
pub fn path_join3(out: &mut [u8], a: &[u8], b: &[u8], c: &[u8]) -> usize {
    let n = path_join(out, a, b);
    let mut m = n;
    for &ch in c {
        if m < out.len() - 1 {
            out[m] = ch;
            m += 1;
        }
    }
    if !out.is_empty() {
        out[m] = 0;
    }
    m
}

// ==================== 文件操作 ====================

pub fn open_path(path: &[u8], flags: usize) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
    }
}

/// 读取整个文件；返回字节数（失败 0）。
pub fn read_path(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0usize;
    loop {
        let space = buf.len() - total;
        if space == 0 {
            break;
        }
        let chunk = space.min(4096);
        let n = unsafe {
            syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize + total, chunk) as isize
        };
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    total
}

/// 覆盖写文件；成功 0。
pub fn write_path(path: &[u8], data: &[u8]) -> isize {
    let fd = open_path(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return -1;
    }
    let mut off = 0usize;
    let mut ok = true;
    while off < data.len() {
        let chunk = (data.len() - off).min(4096);
        let n = unsafe {
            syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize + off, chunk) as isize
        };
        if n <= 0 {
            ok = false;
            break;
        }
        off += n as usize;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    if ok {
        0
    } else {
        -1
    }
}

pub fn mkdir_path(path: &[u8]) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
    }
}

pub fn unlink_path(path: &[u8]) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
    }
}

pub fn exists_path(path: &[u8]) -> bool {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return false;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    true
}

/// 列出目录条目：名字 NUL 分隔写入 `out`，返回字节数；失败 0。
pub fn list_dir(dir: &[u8], out: &mut [u8]) -> usize {
    let fd = open_path(dir, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0usize;
    unsafe {
        // 循环读至 EOF：单次 getdents64 只返回缓冲能装下的条目（约 170 项），
        // 超过即静默漏项（pkg list / alloc_uid / installed_provides）。
        // getdents64 自带目录流偏移，批次间直接续读（不可 lseek 回 0，
        // 否则从头重复列出）。
        loop {
        let n = syscall3(SYS_GETDENTS64, fd as usize, DIR_BUF.as_ptr() as usize, DIR_BUF.len())
            as isize;
        if n <= 0 {
            break;
        }
        {
            let n = n as usize;
            let mut off = 0usize;
            while off + 19 <= n as usize {
                let reclen = u16::from_le_bytes([DIR_BUF[off + 16], DIR_BUF[off + 17]]) as usize;
                if reclen == 0 || off + reclen > n as usize {
                    break;
                }
                let name_start = off + 19;
                let name_end = (off + reclen).min(n as usize);
                let mut len = 0usize;
                while len < 255 && name_start + len < name_end && DIR_BUF[name_start + len] != 0 {
                    len += 1;
                }
                let dot = len == 1 && DIR_BUF[name_start] == b'.';
                let dotdot = len == 2
                    && DIR_BUF[name_start] == b'.'
                    && DIR_BUF[name_start + 1] == b'.';
                if !dot && !dotdot {
                    let mut i = 0usize;
                    while i < len && total < out.len() {
                        out[total] = DIR_BUF[name_start + i];
                        total += 1;
                        i += 1;
                    }
                    if total < out.len() {
                        out[total] = 0;
                        total += 1;
                    }
                }
                off += reclen;
            }
        }
        }
        syscall1(SYS_CLOSE, fd as usize);
    }
    total
}
