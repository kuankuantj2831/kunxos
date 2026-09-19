//! 文件系统 I/O（经 Linux 兼容层系统调用）：openat/read/write/close/
//! mkdirat/unlinkat/getdents64 + 路径工具。no_std、静态缓冲。

use core::arch::asm;

// ==================== 系统调用号（AArch64 asm-generic） ====================

pub const SYS_MKDIRAT: usize = 34;
pub const SYS_UNLINKAT: usize = 35;
pub const SYS_RENAMEAT: usize = 38;
pub const SYS_OPENAT: usize = 56;
pub const SYS_CLOSE: usize = 57;
pub const SYS_GETDENTS64: usize = 61;
pub const SYS_READ: usize = 63;
pub const SYS_WRITE: usize = 64;
pub const SYS_FSYNC: usize = 82;
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
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        in("a3") a4,
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

/// renameat(AT_FDCWD, old, AT_FDCWD, new)。内核按绝对路径解析、忽略 dirfd；
/// 目标已存在或跨文件系统时失败（vfs::rename 语义）。
pub fn rename_path(old: &[u8], new: &[u8]) -> isize {
    unsafe {
        let mut ob = [0u8; 256];
        let mut nb = [0u8; 256];
        let on = old.len().min(ob.len() - 1);
        ob[..on].copy_from_slice(&old[..on]);
        ob[on] = 0;
        let nn = new.len().min(nb.len() - 1);
        nb[..nn].copy_from_slice(&new[..nn]);
        nb[nn] = 0;
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            ob.as_ptr() as usize,
            AT_FDCWD,
            nb.as_ptr() as usize,
        ) as isize
    }
}

/// fsync(路径)：打开后按 fd 落盘（内核按设备级检查点冲刷元数据）。
pub fn fsync_path(path: &[u8]) -> isize {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return -1;
    }
    let r = unsafe { syscall1(SYS_FSYNC, fd as usize) as isize };
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    r
}

/// 原子替换写（索引 / 安装记录等元数据专用）：
/// 写 `<path>.tmp` → fsync → rename（POSIX 覆盖语义）→ fsync。
/// 内核 rename 对未打开的目标是原子替换（见 fs_simple::fs_rename），无需
/// 先 unlink——unlink 再 rename 反而引入「目标已删、新名未落」的丢档窗口
/// 且 rename 中途失败时旧记录已不可恢复。目标恰被读方打开（EBUSY）或内核
/// 事务泄漏时，由 `recover`（文件系统检查点）触发一次重试。任何时刻读者
/// 只见旧全量或新全量，不会出现截断/交错。两次 fsync 分别保证数据与换名
/// 元数据落盘。
pub fn write_path_atomic_with(
    path: &[u8],
    data: &[u8],
    recover: Option<fn()>,
) -> isize {
    let mut tmp = [0u8; 288];
    let tn = path_join(&mut tmp, path, b".tmp");
    if write_path(&tmp[..tn], data) != 0 {
        if let Some(r) = recover {
            r();
            if write_path(&tmp[..tn], data) == 0 {
                return finish_with_retry(&tmp[..tn], path, recover);
            }
        }
        unlink_path(&tmp[..tn]);
        return -1;
    }
    finish_with_retry(&tmp[..tn], path, recover)
}

/// rename 提交（含一次 recover 重试）；失败时清理临时名并报错。
fn finish_with_retry(tmp: &[u8], path: &[u8], recover: Option<fn()>) -> isize {
    if finish_atomic(tmp, path) == 0 {
        return 0;
    }
    if let Some(r) = recover {
        r();
        if finish_atomic(tmp, path) == 0 {
            return 0;
        }
    }
    unlink_path(tmp);
    -1
}

fn finish_atomic(tmp: &[u8], path: &[u8]) -> isize {
    fsync_path(tmp);
    if rename_path(tmp, path) != 0 {
        return -1; // 临时名保留，供上层重试
    }
    fsync_path(path);
    0
}

pub fn write_path_atomic(path: &[u8], data: &[u8]) -> isize {
    write_path_atomic_with(path, data, None)
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
