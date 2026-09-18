// rild 系统调用封装（EL0 用户态，裸 syscall、no_std、无堆）。
// AArch64 asm-generic 系统调用编号，与其它 KunxOS 服务一致。

use core::arch::asm;

pub const SYS_READ: usize = 63;
pub const SYS_WRITE: usize = 64;
pub const SYS_OPENAT: usize = 56;
pub const SYS_CLOSE: usize = 57;
pub const SYS_LSEEK: usize = 62;
pub const SYS_FTRUNCATE: usize = 46;
pub const SYS_EXIT: usize = 93;
pub const SYS_NANOSLEEP: usize = 101;
pub const SYS_GETTIMEOFDAY: usize = 169;
pub const SYS_IOCTL: usize = 29;
pub const SYS_MKDIRAT: usize = 34;
pub const SYS_UNLINKAT: usize = 35;
pub const SYS_RENAMEAT: usize = 38;
pub const SYS_FSYNC: usize = 82;
pub const SYS_REBOOT: usize = 142;

pub const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
pub const O_RDONLY: usize = 0;
pub const O_WRONLY: usize = 1;
pub const O_RDWR: usize = 2;
pub const O_CREAT: usize = 0x40;
pub const O_TRUNC: usize = 0x200;
pub const O_APPEND: usize = 0x400;

pub const SEEK_SET: usize = 0;
pub const SEEK_END: usize = 2;

#[inline(always)]
pub unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
pub unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
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
pub unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
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
pub unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
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

#[inline(always)]
pub unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        in("x3") a4,
        in("x4") a5,
        options(nostack)
    );
    ret
}

pub unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

pub fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

pub fn print_bytes(b: &[u8]) {
    unsafe {
        write_fd(1, b);
    }
}

pub fn print_dec(mut v: u64) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 24];
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

/// 路径缓冲（所有 open 路径 NUL 结尾）。
pub static mut PATH_BUF: [u8; 128] = [0u8; 128];

pub unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

pub unsafe fn mkdir(path: &str) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0o755) as isize
}

/// rename 的两个路径缓冲（PATH_BUF 只有一份，两个路径不能共用）。
static mut RENAME_SRC: [u8; 96] = [0; 96];
static mut RENAME_DST: [u8; 96] = [0; 96];

/// 同目录改名（jlog 事务内提交，掉电原子）。目标已存在时按 POSIX 语义
/// 原子替换（普通文件/符号链接；目标仍被打开时返回 -16/EBUSY）。
pub unsafe fn rename(old: &str, new: &str) -> isize {
    let n = old.len().min(RENAME_SRC.len() - 1);
    RENAME_SRC[..n].copy_from_slice(&old.as_bytes()[..n]);
    RENAME_SRC[n] = 0;
    let m = new.len().min(RENAME_DST.len() - 1);
    RENAME_DST[..m].copy_from_slice(&new.as_bytes()[..m]);
    RENAME_DST[m] = 0;
    syscall4(
        SYS_RENAMEAT,
        AT_FDCWD,
        RENAME_SRC.as_ptr() as usize,
        AT_FDCWD,
        RENAME_DST.as_ptr() as usize,
    ) as isize
}

/// fsync(fd)：SFS 检查点（imap/位图/超级块 + 设备冲刷），保证已写数据与
/// 元数据落盘。持久存储（短信箱）提交路径必调；信箱/状态文件可不调。
pub unsafe fn fsync(fd: usize) -> isize {
    syscall1(SYS_FSYNC, fd) as isize
}

/// 删除文件（信箱消息取走 / 原子重写的旧备份清理）。
pub unsafe fn unlink(path: &str) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
}

pub unsafe fn read_fd(fd: usize, buf: &mut [u8]) -> isize {
    syscall3(SYS_READ, fd, buf.as_mut_ptr() as usize, buf.len()) as isize
}

pub fn sleep_ms(ms: u64) {
    let mut req = [0u8; 16];
    let sec = ms / 1000;
    let nsec = (ms % 1000) * 1_000_000;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

/// 单调微秒（内核 CLOCK_MONOTONIC 语义：自启动计时）。
pub fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
    }
    let sec = u64::from_le_bytes(tv[..8].try_into().unwrap());
    let usec = u64::from_le_bytes(tv[8..].try_into().unwrap());
    sec * 1_000_000 + usec
}

/// 墙钟时间（epoch 秒 + 微秒），用于短信时间戳。
pub fn wall_us() -> u64 {
    now_us()
}

pub unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

pub unsafe fn ioctl(fd: usize, request: u32, arg: *mut u8) -> isize {
    syscall3(SYS_IOCTL, fd, request as usize, arg as usize) as isize
}

/// 关机（PSCI SYSTEM_OFF，QEMU -no-reboot 下退出模拟器）。
pub unsafe fn poweroff() -> ! {
    // reboot(RB_POWER_OFF = 0x4321FEDC)
    let _ = syscall3(SYS_REBOOT, 0xFEE1_DEAD, 0x2812_1969, 0x4321_FEDC);
    loop {}
}
