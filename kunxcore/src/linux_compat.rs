//! Linux 系统调用翻译层（Phase 9）：拦截 EL0 `svc #0` 并映射到内核服务。
//!
//! 这是运行 Linux 应用（busybox、SDL2、bionic/glibc 链接的 ARM64 程序）的前端：
//! EL0 执行 `svc #0` 时由 `main.rs` 的 `lower_sync` 异常入口把完整寄存器现场保存
//! 到 [`TrapFrame`]，随后调用 `syscall_dispatch`（见 `syscall_table.rs`），最终转入
//! 本模块的 [`dispatch`]。
//!
//! ## 语义差异处理（Linux ↔ 内核）
//!
//! 内核的 VFS/IPC/进程模型与 Linux ABI 存在若干差异，本层负责翻译：
//!
//! | Linux 语义 | 内核模型 | 翻译手段 |
//! |---|---|---|
//! | `openat(dirfd, ...)` 相对目录 | 无 per-fd cwd | 忽略 dirfd（单根文件系统），仅翻译 flags |
//! | `O_APPEND`/`O_DIRECTORY`/`O_CLOEXEC` | VFS 仅 O_CREAT/O_TRUNC | open 后 seek 到尾部 / 校验目录 |
//! | `getdents64(fd)` 按 fd 枚举目录 | VFS 按「路径」枚举 | `vfs::readdir_fd_entry` 做 fd→(dev,ino) 映射 |
//! | `struct stat`（asm-generic 128B） | 内核仅存类型码+大小 | 按 AArch64 布局合成 st_mode/st_size/... |
//! | `struct timespec`/`timeval` | 内核微秒计数器 | 拆分秒/纳秒/微秒 |
//! | `brk` 程序断点 | 无用户堆 | `bionic_shim::brk_get/set` 管理页映射 |
//! | `mmap(MAP_ANONYMOUS)` | 物理页分配 | `bionic_shim::mmap_anon` |
//! | `clone(CLONE_THREAD)`/`futex` | 无线程/无 futex | `bionic_shim::clone_thread/futex_*` |
//! | `execve` | 单地址空间 trampoline 模型 | 暂返回 `ENOSYS`（busybox 按 applet 直调无需 exec） |
//! | 套接字 `sockaddr_in` | 内核 (port, ip) 元组 | 解析 16 字节 `sockaddr_in` |
//!
//! 用户地址空间在陷入 EL1 时仍随 `TTBR0_EL1`（用户页表）生效，因此本层可经
//! 用户指针直接读写用户内存（`copy_from_user`/`copy_to_user` 做基本边界检查）。

use crate::bionic_shim;
use crate::devfs;
use crate::elf;
use crate::fs_simple;
use crate::ipc;
use crate::process::{self, FdKind};
use crate::sched_cfs;
use crate::socket;
use crate::syscall_table::TrapFrame;
use crate::vfs;
use crate::vm;

// ==================== errno ====================

const EPERM: i64 = -1;
const ENOENT: i64 = -2;
const ESRCH: i64 = -3;
const EINTR: i64 = -4;
const EIO: i64 = -5;
const ENOEXEC: i64 = -8;
const EBADF: i64 = -9;
const ECHILD: i64 = -10;
const EAGAIN: i64 = -11;
const ENOMEM: i64 = -12;
const EACCES: i64 = -13;
const EFAULT: i64 = -14;
const EBUSY: i64 = -16;
const EEXIST: i64 = -17;
const ENODEV: i64 = -19;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const EINVAL: i64 = -22;
const ENFILE: i64 = -23;
const EMFILE: i64 = -24;
const ENOTTY: i64 = -25;
const EFBIG: i64 = -27;
const ENOSPC: i64 = -28;
const ESPIPE: i64 = -29;
const EROFS: i64 = -30;
const EPIPE: i64 = -32;
const ERANGE: i64 = -34;
const ENOSYS: i64 = -38;
const ENOTEMPTY: i64 = -39;
const EAFNOSUPPORT: i64 = -97;
const EPROTONOSUPPORT: i64 = -93;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_IO_SETUP: usize = 0;
const SYS_IO_DESTROY: usize = 1;
const SYS_IO_SUBMIT: usize = 2;
const SYS_IO_CANCEL: usize = 3;
const SYS_IO_GETEVENTS: usize = 4;
const SYS_GETCWD: usize = 17;
const SYS_EVENTFD2: usize = 19;
const SYS_EPOLL_CREATE1: usize = 20;
const SYS_EPOLL_CTL: usize = 21;
const SYS_EPOLL_PWAIT: usize = 22;
const SYS_DUP: usize = 23;
const SYS_DUP3: usize = 24;
const SYS_FCNTL: usize = 25;
const SYS_IOCTL: usize = 29;
const SYS_FLOCK: usize = 32;
const SYS_MKNODAT: usize = 33;
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_SYMLINKAT: usize = 36;
const SYS_LINKAT: usize = 37;
const SYS_RENAMEAT: usize = 38;
const SYS_UMOUNT2: usize = 39;
const SYS_MOUNT: usize = 40;
const SYS_STATFS: usize = 43;
const SYS_FSTATFS: usize = 44;
const SYS_TRUNCATE: usize = 45;
const SYS_FTRUNCATE: usize = 46;
const SYS_FALLOCATE: usize = 47;
const SYS_FACCESSAT: usize = 48;
const SYS_CHDIR: usize = 49;
const SYS_FCHDIR: usize = 50;
const SYS_FCHMOD: usize = 52;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_PIPE2: usize = 59;
const SYS_GETDENTS64: usize = 61;
const SYS_LSEEK: usize = 62;
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_READV: usize = 65;
const SYS_WRITEV: usize = 66;
const SYS_PREAD64: usize = 67;
const SYS_PWRITE64: usize = 68;
const SYS_SENDFILE: usize = 71;
const SYS_PSELECT6: usize = 72;
const SYS_PPOLL: usize = 73;
const SYS_READLINKAT: usize = 78;
const SYS_NEWFSTATAT: usize = 79;
const SYS_FSTAT: usize = 80;
const SYS_SYNC: usize = 81;
const SYS_FSYNC: usize = 82;
const SYS_FDATASYNC: usize = 83;
const SYS_UTIMENSAT: usize = 88;
const SYS_EXIT: usize = 93;
const SYS_EXIT_GROUP: usize = 94;
const SYS_WAITID: usize = 95;
const SYS_SET_TID_ADDRESS: usize = 96;
const SYS_FUTEX: usize = 98;
const SYS_SET_ROBUST_LIST: usize = 99;
const SYS_GET_ROBUST_LIST: usize = 100;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETITIMER: usize = 102;
const SYS_SETITIMER: usize = 103;
const SYS_CLOCK_SETTIME: usize = 112;
const SYS_CLOCK_GETTIME: usize = 113;
const SYS_CLOCK_GETRES: usize = 114;
const SYS_CLOCK_NANOSLEEP: usize = 115;
const SYS_SCHED_YIELD: usize = 124;
const SYS_KILL: usize = 129;
const SYS_TKILL: usize = 130;
const SYS_TGKILL: usize = 131;
const SYS_SIGALTSTACK: usize = 132;
const SYS_RT_SIGSUSPEND: usize = 133;
const SYS_RT_SIGACTION: usize = 134;
const SYS_RT_SIGPROCMASK: usize = 135;
const SYS_RT_SIGPENDING: usize = 136;
const SYS_RT_SIGTIMEDWAIT: usize = 137;
const SYS_RT_SIGQUEUEINFO: usize = 138;
const SYS_RT_SIGRETURN: usize = 139;
const SYS_SETPRIORITY: usize = 140;
const SYS_GETPRIORITY: usize = 141;
const SYS_REBOOT: usize = 142;
const SYS_SETREGID: usize = 143;
const SYS_SETGID: usize = 144;
const SYS_SETREUID: usize = 145;
const SYS_SETUID: usize = 146;
const SYS_SETRESUID: usize = 147;
const SYS_GETRESUID: usize = 148;
const SYS_SETRESGID: usize = 149;
const SYS_GETRESGID: usize = 150;
const SYS_TIMES: usize = 153;
const SYS_SETPGID: usize = 154;
const SYS_GETPGID: usize = 155;
const SYS_GETSID: usize = 156;
const SYS_SETSID: usize = 157;
const SYS_UNAME: usize = 160;
const SYS_SETHOSTNAME: usize = 161;
const SYS_SETDOMAINNAME: usize = 162;
const SYS_GETRLIMIT: usize = 163;
const SYS_SETRLIMIT: usize = 164;
const SYS_GETRUSAGE: usize = 165;
const SYS_UMASK: usize = 166;
const SYS_PRCTL: usize = 167;
const SYS_GETCPU: usize = 168;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_SETTIMEOFDAY: usize = 170;
const SYS_ADJTIMEX: usize = 171;
const SYS_GETPID: usize = 172;
const SYS_GETPPID: usize = 173;
const SYS_GETUID: usize = 174;
const SYS_GETEUID: usize = 175;
const SYS_GETGID: usize = 176;
const SYS_GETEGID: usize = 177;
const SYS_GETTID: usize = 178;
const SYS_SYSINFO: usize = 179;
const SYS_SOCKET: usize = 198;
const SYS_SOCKETPAIR: usize = 199;
const SYS_BIND: usize = 200;
const SYS_LISTEN: usize = 201;
const SYS_ACCEPT: usize = 202;
const SYS_CONNECT: usize = 203;
const SYS_GETSOCKNAME: usize = 204;
const SYS_GETPEERNAME: usize = 205;
const SYS_SENDTO: usize = 206;
const SYS_RECVFROM: usize = 207;
const SYS_SETSOCKOPT: usize = 208;
const SYS_GETSOCKOPT: usize = 209;
const SYS_SHUTDOWN: usize = 210;
const SYS_SENDMSG: usize = 211;
const SYS_RECVMSG: usize = 212;
const SYS_BRK: usize = 214;
const SYS_MUNMAP: usize = 215;
const SYS_MREMAP: usize = 216;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_MMAP: usize = 222;
const SYS_MPROTECT: usize = 226;
const SYS_MSYNC: usize = 227;
const SYS_MLOCK: usize = 228;
const SYS_MUNLOCK: usize = 229;
const SYS_MLOCKALL: usize = 230;
const SYS_MUNLOCKALL: usize = 231;
const SYS_MINCORE: usize = 232;
const SYS_MADVISE: usize = 233;
const SYS_ACCEPT4: usize = 242;
const SYS_WAIT4: usize = 260;
const SYS_PRLIMIT64: usize = 261;
const SYS_CLOCK_ADJTIME: usize = 266;
const SYS_GETRANDOM: usize = 278;
const SYS_MEMFD_CREATE: usize = 279;
const SYS_BPF: usize = 280;
const SYS_EXECVEAT: usize = 281;
const SYS_STATX: usize = 291;
// 自定义（Linux 未占用）：剪贴板读写
const SYS_CLIPBOARD_SET: usize = 401;
const SYS_CLIPBOARD_GET: usize = 402;

// ==================== Linux 常量 ====================

/// `open` 标志位。
const O_CREAT: u32 = 0x40;
const O_TRUNC: u32 = 0x200;
const O_APPEND: u32 = 0x400;
const O_DIRECTORY: u32 = 0x10000;

/// `mmap` 标志位。
const MAP_FIXED: u32 = 0x10;
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_FIXED_NOREPLACE: u32 = 0x100000;

/// `clone` 标志位。
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_VM: u64 = 0x0000_0100;

/// `futex` 操作码。
const FUTEX_WAIT: u32 = 0;
const FUTEX_WAKE: u32 = 1;
const FUTEX_FD: u32 = 2;
const FUTEX_REQUEUE: u32 = 3;
const FUTEX_CMP_REQUEUE: u32 = 4;
const FUTEX_WAKE_OP: u32 = 5;
const FUTEX_WAIT_BITSET: u32 = 9;
const FUTEX_WAKE_BITSET: u32 = 10;
const FUTEX_PRIVATE_FLAG: u32 = 128;

/// 终端 ioctl 请求码。
const TCGETS: u32 = 0x5401;
const TCSETS: u32 = 0x5402;
const TIOCGWINSZ: u32 = 0x5413;

/// 用户指针上界（低于内核恒等映射起始 0x4000_0000）。
/// 用户地址空间整体位于 0x0000_0000-0x3FFF_FFFF（L1[0]），内核 RAM 恒等映射
/// 从 0x4000_0000 开始，因此用户指针必须严格低于该值。
const USER_TOP: u64 = 0x4000_0000;

/// execve 的 argv/envp 元素上限与单串长度上限。
const MAX_EXEC_ARGS: usize = 16;
const EXEC_STR_LEN: usize = 64;

// ==================== 用户内存访问 ====================

/// 校验用户指针 `[addr, addr+len)`：范围合法**且**每一页在当前进程页表中
/// 都已映射。仅范围合法是不够的——用户地址空间稀疏，未映射地址在 EL1 解引用
/// 会触发 data abort 挂死内核（任何用户进程都可借此 DoS）。
///
/// 按需分页（demand paging）：mmap 返回的匿名缓冲区初始只是"预留"，尚未分配
/// 物理页。内核态访问前先经 `ensure_user_range_mapped` 把范围内的预留页全部
/// 落地，再校验映射，避免 EL1 解引用未映射用户页触发 data abort。
fn user_ok(addr: u64, len: usize) -> bool {
    if addr == 0 || addr >= USER_TOP {
        return false;
    }
    if !(addr.checked_add(len as u64).map_or(false, |e| e <= USER_TOP)) {
        return false;
    }
    if len == 0 {
        return true;
    }
    if !process::ensure_user_range_mapped(addr as usize, len) {
        return false;
    }
    vm::user_range_mapped(process::current_root(), addr as usize, len)
}

/// 校验用户指针 `[addr, addr+len)` 可写：范围合法、每页已映射，且 COW 共享
/// 页已 break 出本进程私有可写副本。
///
/// 用于内核态**写入**用户内存前（`copy_to_user`）：COW 页页表映射为只读只
/// 约束 EL0，EL1 内核直接写共享页会污染其它进程数据，必须先 break。
fn user_ok_writable(addr: u64, len: usize) -> bool {
    if addr == 0 || addr >= USER_TOP {
        return false;
    }
    if !(addr.checked_add(len as u64).map_or(false, |e| e <= USER_TOP)) {
        return false;
    }
    if len == 0 {
        return true;
    }
    if !process::ensure_user_range_writable(addr as usize, len) {
        return false;
    }
    vm::user_range_mapped(process::current_root(), addr as usize, len)
}

unsafe fn copy_from_user(src: u64, dst: &mut [u8]) -> usize {
    if !user_ok(src, dst.len()) {
        return 0;
    }
    core::ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), dst.len());
    dst.len()
}

unsafe fn copy_to_user(dst: u64, src: &[u8]) -> usize {
    if !user_ok_writable(dst, src.len()) {
        return 0;
    }
    core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len());
    src.len()
}

/// 从用户空间读取以 NUL 结尾的字符串，返回其长度（不含 NUL）。
unsafe fn read_cstr(addr: u64, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let mut i = 0usize;
    while i < buf.len() - 1 {
        // 逐字节校验边界，避免越过用户区读入内核恒等映射内存
        if !user_ok(addr + i as u64, 1) {
            break;
        }
        let c = *(addr as *const u8).add(i);
        if c == 0 {
            break;
        }
        buf[i] = c;
        i += 1;
    }
    buf[i] = 0;
    i
}

unsafe fn write_cstr(dst: u64, s: &str) -> usize {
    copy_to_user(dst, s.as_bytes())
}

/// 把用户传入的路径解析为绝对路径（基于当前进程 cwd），返回 `&'static str`。
///
/// 相对路径按 `cwd + "/" + 路径` 拼接；路径中的 `.`/`..` 分量由 VFS 层
/// （目录项查找）自然处理。结果写入静态缓冲（单线程系统调用上下文，安全）。
fn resolve_user_path(p: &str) -> &'static str {
    static mut PATH_BUF: [u8; 128] = [0; 128];
    unsafe {
        let buf = &mut PATH_BUF;
        buf.fill(0);
        if p.starts_with('/') {
            let n = p.len().min(buf.len() - 1);
            buf[..n].copy_from_slice(&p.as_bytes()[..n]);
            return core::str::from_utf8_unchecked(core::slice::from_raw_parts(buf.as_ptr(), n));
        }
        let mut cwd = [0u8; 128];
        let cl = process::current_cwd(&mut cwd);
        let mut off = 0usize;
        if cl > 0 && cl < buf.len() {
            buf[..cl].copy_from_slice(&cwd[..cl]);
            off = cl;
        }
        if off < buf.len() - 1 && (off == 0 || buf[off - 1] != b'/') {
            buf[off] = b'/';
            off += 1;
        }
        let space = buf.len() - off - 1;
        let n = p.len().min(space);
        buf[off..off + n].copy_from_slice(&p.as_bytes()[..n]);
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(buf.as_ptr(), off + n))
    }
}

// ==================== 系统调用分发 ====================

/// 最近一次系统调用的诊断信息（供 EL0 fault 定位：fault 是否发生在
/// syscall 返回路径，以及是哪个系统调用）。
static mut LAST_SYSCALL: (usize, u64, u64) = (0, 0, 0); // (nr, 陷阱帧 elr, sp_el0)
/// 最近 24 次系统调用环形记录（fault 诊断：定位「sp 被某次 syscall 返回错位」）。
const SYS_TRACE_N: usize = 24;
static mut SYS_TRACE: [(usize, u64, u64); SYS_TRACE_N] = [(0, 0, 0); SYS_TRACE_N];
static mut SYS_TRACE_POS: usize = 0;

/// 记录最近一次系统调用（fault 诊断用；sp_el0 用于核对「sp 被 syscall 返回错位」）。
fn trace_syscall(nr: usize, f: &TrapFrame) {
    unsafe {
        LAST_SYSCALL = (nr, f.elr, f.sp_el0);
        SYS_TRACE[SYS_TRACE_POS] = (nr, f.elr, f.sp_el0);
        SYS_TRACE_POS = (SYS_TRACE_POS + 1) % SYS_TRACE_N;
    }
}

/// 读取最近一次系统调用的诊断信息。
pub fn last_syscall() -> (usize, u64, u64) {
    unsafe { LAST_SYSCALL }
}

/// 转储最近 N 次系统调用（nr, elr, sp），用于核对 sp 错位的发生点。
pub fn dump_sys_trace() {
    unsafe {
        for i in 0..SYS_TRACE_N {
            let idx = (SYS_TRACE_POS + i) % SYS_TRACE_N;
            let (nr, elr, sp) = SYS_TRACE[idx];
            if nr == 0 && elr == 0 && sp == 0 {
                continue;
            }
            crate::uart_send_str("[systrace] nr=");
            crate::print_dec(nr as u64);
            crate::uart_send_str(" elr=");
            crate::print_hex(elr);
            crate::uart_send_str(" sp=");
            crate::print_hex(sp);
            crate::uart_send_str("\n");
        }
    }
}

/// Linux 系统调用分发：`nr` 为 x8 系统调用号，`f` 为陷阱帧（x0..x5 为参数）。
pub fn dispatch(nr: usize, f: &mut TrapFrame) -> i64 {
    trace_syscall(nr, f);
    // 信号投递：待处理信号在 syscall 入口检查；已注册 handler 的信号改写
    // 陷阱帧进入 handler（跳过本次 syscall），未注册的按默认动作（终止/忽略）。
    if crate::signal::handle_pending(f) {
        return 0; // 信号已投递，本 syscall 被信号打断
    }
    match nr {
        SYS_OPENAT => sys_openat(f),
        SYS_CLOSE => sys_close(f),
        SYS_READ => sys_read(f),
        SYS_WRITE => sys_write(f),
        SYS_READV => sys_readv(f),
        SYS_WRITEV => sys_writev(f),
        SYS_PREAD64 => sys_pread64(f),
        SYS_PWRITE64 => sys_pwrite64(f),
        SYS_LSEEK => sys_lseek(f),
        SYS_PIPE2 => sys_pipe2(f),
        SYS_DUP => sys_dup(f),
        SYS_DUP3 => sys_dup3(f),
        SYS_FCNTL => sys_fcntl(f),
        SYS_IOCTL => sys_ioctl(f),
        SYS_GETDENTS64 => sys_getdents64(f),
        SYS_GETCWD => sys_getcwd(f),
        SYS_CHDIR => sys_chdir(f),
        SYS_MKDIRAT => sys_mkdirat(f),
        SYS_UNLINKAT => sys_unlinkat(f),
        SYS_MKNODAT => sys_mknodat(f),
        SYS_RENAMEAT => sys_renameat(f),
        SYS_FACCESSAT => sys_faccessat(f),
        SYS_READLINKAT => sys_readlinkat(f),
        SYS_SYMLINKAT => sys_symlinkat(f),
        SYS_NEWFSTATAT => sys_newfstatat(f),
        SYS_FSTAT => sys_fstat(f),
        SYS_FTRUNCATE => sys_ftruncate(f),
        SYS_TRUNCATE => sys_truncate(f),
        SYS_SYNC | SYS_FSYNC | SYS_FDATASYNC => 0,
        SYS_FALLOCATE => 0,
        SYS_EXIT | SYS_EXIT_GROUP => sys_exit(f),
        SYS_GETPID => process::current_pid() as i64,
        SYS_GETPPID => process::current_parent() as i64,
        SYS_GETUID | SYS_GETEUID => 0,
        SYS_GETGID | SYS_GETEGID => 0,
        SYS_GETTID => sched_cfs::current_tid() as i64,
        SYS_SET_TID_ADDRESS => bionic_shim::set_tid_address(f.x[0] as usize) as i64,
        SYS_FUTEX => sys_futex(f),
        SYS_SET_ROBUST_LIST | SYS_GET_ROBUST_LIST => 0,
        SYS_UNAME => sys_uname(f),
        SYS_SETHOSTNAME => 0,
        SYS_GETTIMEOFDAY => sys_gettimeofday(f),
        SYS_CLOCK_GETTIME => sys_clock_gettime(f),
        SYS_CLOCK_GETRES => sys_clock_getres(f),
        SYS_NANOSLEEP => sys_nanosleep(f),
        SYS_CLOCK_NANOSLEEP => sys_nanosleep(f),
        SYS_SCHED_YIELD => {
            sched_cfs::yield_now();
            0
        }
        SYS_KILL | SYS_TKILL | SYS_TGKILL => sys_kill(f),
        SYS_RT_SIGACTION => crate::signal::rt_sigaction_syscall(f),
        SYS_RT_SIGRETURN => crate::signal::rt_sigreturn_syscall(f),
        SYS_RT_SIGPROCMASK | SYS_SIGALTSTACK | SYS_RT_SIGPENDING
        | SYS_RT_SIGSUSPEND => 0,
        SYS_WAIT4 => sys_wait4(f),
        SYS_BRK => sys_brk(f),
        SYS_MMAP => sys_mmap(f),
        SYS_MUNMAP => sys_munmap(f),
        SYS_MPROTECT => 0,
        SYS_MSYNC | SYS_MLOCK | SYS_MUNLOCK | SYS_MLOCKALL | SYS_MUNLOCKALL
        | SYS_MINCORE | SYS_MADVISE => 0,
        SYS_CLONE => sys_clone(f),
        SYS_EXECVE | SYS_EXECVEAT => sys_execve(f),
        SYS_PRCTL => 0,
        SYS_UMASK => 0o022,
        SYS_GETRLIMIT | SYS_SETRLIMIT | SYS_PRLIMIT64 => 0,
        SYS_GETRANDOM => sys_getrandom(f),
        SYS_SYSINFO => sys_sysinfo(f),
        SYS_TIMES => sys_times(f),
        SYS_GETPRIORITY => 0,
        SYS_SETPRIORITY => 0,
        SYS_SOCKET => sys_socket(f),
        SYS_CONNECT => sys_connect(f),
        SYS_BIND => sys_bind(f),
        SYS_LISTEN => socket::listen(f.x[0] as i32, f.x[1] as usize) as i64,
        SYS_ACCEPT | SYS_ACCEPT4 => socket::accept(f.x[0] as i32) as i64,
        SYS_SENDTO => sys_sendto(f),
        SYS_RECVFROM => sys_recvfrom(f),
        SYS_SETSOCKOPT | SYS_GETSOCKOPT => 0,
        SYS_SHUTDOWN => sys_shutdown(f),
        SYS_SENDMSG => sys_sendmsg(f),
        SYS_RECVMSG => sys_recvmsg(f),
        SYS_SOCKETPAIR => sys_socketpair(f),
        SYS_CLIPBOARD_SET => sys_clipboard_set(f),
        SYS_CLIPBOARD_GET => sys_clipboard_get(f),
        SYS_IO_SETUP | SYS_IO_DESTROY | SYS_IO_SUBMIT | SYS_IO_CANCEL | SYS_IO_GETEVENTS
        | SYS_EVENTFD2 | SYS_EPOLL_CREATE1 | SYS_EPOLL_CTL | SYS_EPOLL_PWAIT
        | SYS_FLOCK | SYS_LINKAT | SYS_UMOUNT2 | SYS_MOUNT
        | SYS_STATFS | SYS_FSTATFS | SYS_FCHDIR | SYS_FCHMOD | SYS_SENDFILE
        | SYS_PSELECT6 | SYS_PPOLL | SYS_UTIMENSAT | SYS_WAITID
        | SYS_GETITIMER | SYS_SETITIMER | SYS_CLOCK_SETTIME | SYS_RT_SIGTIMEDWAIT
        | SYS_RT_SIGQUEUEINFO | SYS_REBOOT | SYS_SETREGID | SYS_SETGID | SYS_SETREUID
        | SYS_SETUID | SYS_SETRESUID | SYS_GETRESUID | SYS_SETRESGID | SYS_GETRESGID
        | SYS_SETPGID | SYS_GETPGID | SYS_GETSID | SYS_SETSID | SYS_SETDOMAINNAME
        | SYS_GETRUSAGE | SYS_GETCPU | SYS_SETTIMEOFDAY | SYS_ADJTIMEX | SYS_GETSOCKNAME
        | SYS_GETPEERNAME | SYS_MREMAP | SYS_CLOCK_ADJTIME | SYS_MEMFD_CREATE | SYS_BPF
        | SYS_STATX => sys_statx(f),
        _ => ENOSYS,
    }
}

// ==================== 文件 I/O ====================

/// 把 Linux `struct stat` 的 S_IF* 类型位映射到内核 fs 类型码，反之亦然。
fn s_ifmt(t: u16) -> u32 {
    match t {
        fs_simple::T_DIR => 0o040000,
        fs_simple::T_CHR => 0o020000,
        fs_simple::T_BLK => 0o060000,
        fs_simple::T_FIFO => 0o010000,
        fs_simple::T_SYM => 0o120000,
        _ => 0o100000, // T_REG 及默认
    }
}

/// `getdents64` 的 d_type 值（DT_*）。
fn dt_of(t: u16) -> u8 {
    match t {
        fs_simple::T_REG => 8,
        fs_simple::T_DIR => 4,
        fs_simple::T_CHR => 2,
        fs_simple::T_BLK => 6,
        fs_simple::T_FIFO => 1,
        fs_simple::T_SYM => 10,
        _ => 0,
    }
}

/// 合成一个最小但结构正确的 AArch64 `struct stat`（128 字节）。
///
/// 关键字段：st_dev@0, st_ino@8, st_mode@16, st_nlink@20, st_size@48, st_blksize@56。
/// 目标指针非法（越界/未映射）时返回 `false`，调用方应返回 `EFAULT`。
unsafe fn write_stat(stat_ptr: u64, t: u16, size: u32) -> bool {
    if !user_ok_writable(stat_ptr, 128) {
        return false;
    }
    let p = stat_ptr as *mut u8;
    core::ptr::write_bytes(p, 0, 128);
    *(p as *mut u64) = 0; // st_dev
    *((p as *mut u64).add(1)) = 1; // st_ino
    *((p as *mut u32).add(4)) = s_ifmt(t) | if t == fs_simple::T_DIR { 0o755 } else { 0o644 }; // st_mode
    *((p as *mut u32).add(5)) = if t == fs_simple::T_DIR { 2 } else { 1 }; // st_nlink
    *((p as *mut i64).add(6)) = size as i64; // st_size @48
    *((p as *mut i32).add(14)) = 4096; // st_blksize @56
    *((p as *mut i64).add(8)) = (size as i64 + 511) / 512; // st_blocks @64
    true
}

fn sys_openat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let flags = f.x[2] as u32;
    let _dirfd = f.x[0] as i64; // 单根文件系统：忽略 dirfd（含 AT_FDCWD）
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    // 翻译访问模式与创建/截断标志；O_NONBLOCK 等其余标志透传给 VFS
    let acc = flags & 0x3;
    let creat = flags & O_CREAT;
    let trunc = flags & O_TRUNC;
    let fd = vfs::open(s, acc | creat | trunc | (flags & vfs::O_NONBLOCK));
    if fd < 0 {
        return fd as i64;
    }
    // O_DIRECTORY：目标必须是目录
    if flags & O_DIRECTORY != 0 {
        if !matches!(vfs::fstat(fd), Some((t, _)) if t == fs_simple::T_DIR) {
            vfs::close(fd);
            return ENOTDIR;
        }
    }
    // O_APPEND：打开后把文件位置移到末尾
    if flags & O_APPEND != 0 {
        vfs::seek(fd, 0, vfs::SEEK_END);
    }
    // O_CLOEXEC：本内核无 exec，忽略
    fd as i64
}

fn sys_close(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    match process::get_fd(fd) {
        Some(e) => match e.kind {
            FdKind::File => vfs::close(fd) as i64,
            FdKind::Pipe => {
                process::close_fd(fd);
                0
            }
            FdKind::MsgQueue => {
                ipc::msgq_drop_ref(e.index);
                process::close_fd(fd);
                0
            }
            FdKind::Shm => {
                ipc::shm_close(fd);
                process::close_fd(fd);
                0
            }
            FdKind::Pair => {
                ipc::pair_close(fd);
                process::close_fd(fd);
                0
            }
            _ => EBADF,
        },
        None => {
            // 未登记到进程 fd 表的句柄：仅当确为 socket 句柄时关闭，
            // 否则返回 EBADF（此前无条件返回 0，掩盖「关闭未打开 fd」的错误）。
            if socket::is_socket(fd) {
                socket::close(fd);
                0
            } else {
                EBADF
            }
        }
    }
}

fn sys_read(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let count = f.x[2] as usize;
    if count == 0 {
        return 0;
    }
    let n = count.min(4096);
    let mut tmp = [0u8; 4096];
    let entry = match process::get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    let r = match entry.kind {
        FdKind::File => vfs::read(fd, &mut tmp[..n]),
        FdKind::Pipe => ipc::pipe_read(fd, &mut tmp[..n]) as i32,
        FdKind::Pair => ipc::pair_read(fd, &mut tmp[..n]) as i32,
        _ => return EBADF,
    };
    if r < 0 {
        return r as i64;
    }
    let copied = unsafe { copy_to_user(buf_ptr, &tmp[..r as usize]) };
    if copied != r as usize {
        return EFAULT;
    }
    copied as i64
}

fn sys_write(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let count = f.x[2] as usize;
    if count == 0 {
        return 0;
    }
    let n = count.min(4096);
    let mut tmp = [0u8; 4096];
    let copied = unsafe { copy_from_user(buf_ptr, &mut tmp[..n]) };
    if copied == 0 {
        return EFAULT;
    }
    let entry = match process::get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    match entry.kind {
        FdKind::File => vfs::write(fd, &tmp[..copied]) as i64,
        FdKind::Pipe => ipc::pipe_write(fd, &tmp[..copied]) as i64,
        FdKind::Pair => ipc::pair_write(fd, &tmp[..copied]) as i64,
        _ => EBADF,
    }
}

/// iovec：`{ base: u64, len: u64 }`。
fn sys_writev(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let iov = f.x[1];
    let iovcnt = f.x[2] as usize;
    if iovcnt == 0 || iovcnt > 64 {
        return EINVAL;
    }
    if !user_ok(iov, iovcnt * 16) {
        return EFAULT;
    }
    let mut total = 0i64;
    for i in 0..iovcnt {
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        let n = len.min(4096);
        let mut tmp = [0u8; 4096];
        let c = unsafe { copy_from_user(base, &mut tmp[..n]) };
        if c == 0 {
            break;
        }
        let entry = match process::get_fd(fd) {
            Some(e) => e,
            None => return EBADF,
        };
        let w = match entry.kind {
            FdKind::File => vfs::write(fd, &tmp[..c]),
            FdKind::Pipe => ipc::pipe_write(fd, &tmp[..c]) as i32,
            _ => return EBADF,
        };
        if w < 0 {
            return w as i64;
        }
        total += w as i64;
        if (w as usize) < c {
            break;
        }
    }
    total
}

fn sys_readv(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let iov = f.x[1];
    let iovcnt = f.x[2] as usize;
    if iovcnt == 0 || iovcnt > 64 {
        return EINVAL;
    }
    if !user_ok(iov, iovcnt * 16) {
        return EFAULT;
    }
    let mut total = 0i64;
    for i in 0..iovcnt {
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        let n = len.min(4096);
        let mut tmp = [0u8; 4096];
        let entry = match process::get_fd(fd) {
            Some(e) => e,
            None => return EBADF,
        };
        let r = match entry.kind {
            FdKind::File => vfs::read(fd, &mut tmp[..n]),
            FdKind::Pipe => ipc::pipe_read(fd, &mut tmp[..n]) as i32,
            _ => return EBADF,
        };
        if r <= 0 {
            break;
        }
        let copied = unsafe { copy_to_user(base, &tmp[..r as usize]) };
        if copied != r as usize {
            return EFAULT;
        }
        total += r as i64;
        if (r as usize) < n {
            break;
        }
    }
    total
}

/// `pread64`：在指定偏移处读取（不改变文件位置）。
fn sys_pread64(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let count = f.x[2] as usize;
    let offset = f.x[3] as i64; // 64 位偏移，不再截断为 i32
    let cur = vfs::seek(fd, 0, vfs::SEEK_CUR);
    vfs::seek(fd, offset, vfs::SEEK_SET);
    let r = sys_read_at(fd, buf_ptr, count);
    if cur >= 0 {
        vfs::seek(fd, cur, vfs::SEEK_SET);
    }
    r
}

fn sys_pwrite64(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let count = f.x[2] as usize;
    let offset = f.x[3] as i64; // 64 位偏移，不再截断为 i32
    let cur = vfs::seek(fd, 0, vfs::SEEK_CUR);
    vfs::seek(fd, offset, vfs::SEEK_SET);
    let r = sys_write_at(fd, buf_ptr, count);
    if cur >= 0 {
        vfs::seek(fd, cur, vfs::SEEK_SET);
    }
    r
}

fn sys_read_at(fd: i32, buf_ptr: u64, count: usize) -> i64 {
    if count == 0 {
        return 0;
    }
    let n = count.min(4096);
    let mut tmp = [0u8; 4096];
    let entry = match process::get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    let r = match entry.kind {
        FdKind::File => vfs::read(fd, &mut tmp[..n]),
        _ => return EBADF,
    };
    if r < 0 {
        return r as i64;
    }
    unsafe { copy_to_user(buf_ptr, &tmp[..r as usize]) as i64 }
}

fn sys_write_at(fd: i32, buf_ptr: u64, count: usize) -> i64 {
    if count == 0 {
        return 0;
    }
    let n = count.min(4096);
    let mut tmp = [0u8; 4096];
    let copied = unsafe { copy_from_user(buf_ptr, &mut tmp[..n]) };
    if copied == 0 {
        return EFAULT;
    }
    let entry = match process::get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    match entry.kind {
        FdKind::File => vfs::write(fd, &tmp[..copied]) as i64,
        _ => EBADF,
    }
}

fn sys_lseek(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let offset = f.x[1] as i64; // 64 位偏移，不再截断为 i32
    let whence = f.x[2] as u32;
    vfs::seek(fd, offset, whence)
}

fn sys_pipe2(f: &mut TrapFrame) -> i64 {
    let fds_ptr = f.x[0];
    if !user_ok_writable(fds_ptr, 8) {
        return EFAULT;
    }
    let (r, w) = ipc::pipe_create();
    unsafe {
        *(fds_ptr as *mut i32) = r;
        *(fds_ptr as *mut i32).add(1) = w;
    }
    0
}

/// `dup`/`dup3` 的公共实现：复制 fd 表项（共享同一底层对象，文件共享偏移）。
///
/// `newfd < 0`（dup）：分配最小可用描述符；`newfd >= 0`（dup3）：写入指定槽位
/// （若已被占用先关闭，引用计数相应 -1）。对象引用计数对 File/Shm +1，避免
/// 复制的描述符关闭后提前释放对象。
fn dup_fd_common(oldfd: i32, newfd: i32) -> i64 {
    let e = match process::get_fd(oldfd) {
        Some(e) => e,
        None => return EBADF,
    };
    // 目标槽位已被占用：先按 close 语义释放（引用计数 -1）
    if newfd >= 0 {
        if process::get_fd(newfd).is_some() {
            sys_close_raw(newfd);
        }
    }
    let nfd = match e.kind {
        FdKind::File => {
            vfs::open_ref(e.index);
            let f = process::alloc_fd(FdKind::File, e.index, e.flags);
            if f < 0 {
                vfs::close_ref(e.index);
                return EMFILE;
            }
            f
        }
        FdKind::Shm => {
            ipc::shm_add_ref(e.index);
            let f = process::alloc_fd(FdKind::Shm, e.index, e.flags);
            if f < 0 {
                // 撤销刚加的引用（当前进程未映射，仅减 refs）
                ipc::shm_close_index(process::current_pid(), e.index);
                return EMFILE;
            }
            f
        }
        FdKind::Pipe | FdKind::MsgQueue | FdKind::Pair => {
            // 管道/队列/套接字对对象按进程持有（退出时统一释放），复制槽位无需计数
            let f = process::alloc_fd(e.kind, e.index, e.flags);
            if f < 0 {
                return EMFILE;
            }
            f
        }
        FdKind::None => return EBADF,
    };
    if newfd >= 0 && nfd != newfd {
        // dup3 固定目标：把表项写入 newfd，并清空临时分配到的 nfd 槽位
        // （对象引用计数不受影响，因为表项只是“搬家”）。
        process::set_fd(newfd as usize, process::FdEntry { kind: e.kind, index: e.index, flags: e.flags });
        process::close_fd(nfd);
        newfd as i64
    } else {
        nfd as i64
    }
}

/// 无陷阱帧版的 close（供 dup3 关闭目标槽位复用）。
fn sys_close_raw(fd: i32) -> i64 {
    match process::get_fd(fd) {
        Some(e) => match e.kind {
            FdKind::File => vfs::close(fd) as i64,
            FdKind::Pipe => {
                process::close_fd(fd);
                0
            }
            FdKind::MsgQueue => {
                ipc::msgq_drop_ref(e.index);
                process::close_fd(fd);
                0
            }
            FdKind::Shm => {
                ipc::shm_close(fd);
                process::close_fd(fd);
                0
            }
            FdKind::Pair => {
                ipc::pair_close(fd);
                process::close_fd(fd);
                0
            }
            _ => EBADF,
        },
        None => {
            if socket::is_socket(fd) {
                socket::close(fd);
                0
            } else {
                EBADF
            }
        }
    }
}

fn sys_dup(f: &mut TrapFrame) -> i64 {
    let oldfd = f.x[0] as i32;
    dup_fd_common(oldfd, -1)
}

/// `dup3(oldfd, newfd, flags)`：复制到指定描述符（flags 本内核恒等，忽略）。
fn sys_dup3(f: &mut TrapFrame) -> i64 {
    let oldfd = f.x[0] as i32;
    let newfd = f.x[1] as i32;
    if newfd < 0 {
        return EINVAL;
    }
    if oldfd == newfd {
        // Linux dup3：oldfd == newfd 且未置 O_CLOEXEC 时成功（本内核无 CLOEXEC 语义）
        return newfd as i64;
    }
    dup_fd_common(oldfd, newfd)
}

/// `fcntl`：仅支持 F_DUPFD / F_GETFD / F_SETFD / F_GETFL / F_SETFL（其余忽略）。
fn sys_fcntl(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let cmd = f.x[1] as i32;
    let _arg = f.x[2];
    match cmd {
        // F_DUPFD = 0：复制 fd（分配最小可用描述符），语义与 dup 一致
        0 => dup_fd_common(fd, -1),
        // F_GETFD/F_GETFL/F_SETFD/F_SETFL 等：无状态，返回 0
        _ => 0,
    }
}

/// `ioctl`：终端相关请求，以及字符设备（如 evdev）的 ioctl。
fn sys_ioctl(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let request = f.x[1] as u32;
    let arg = f.x[2];

    let is_tty = bionic_shim::isatty(fd);
    match request {
        // TIOCGWINSZ：写入 winsize {ws_row, ws_col, ws_xpixel, ws_ypixel}
        TIOCGWINSZ => {
            if !is_tty {
                return ENOTTY;
            }
            if arg != 0 {
                if !user_ok_writable(arg, 8) {
                    return EFAULT;
                }
                let ws = bionic_shim::tty_winsize();
                unsafe {
                    *((arg as *mut u16)) = ws[0];
                    *((arg as *mut u16).add(1)) = ws[1];
                    *((arg as *mut u16).add(2)) = ws[2];
                    *((arg as *mut u16).add(3)) = ws[3];
                }
            }
            0
        }
        // TCGETS / TCSETS：终端参数，本内核固定终端，直接成功
        TCGETS | TCSETS => {
            if is_tty {
                0
            } else {
                ENOTTY
            }
        }
        // 其余请求（如 evdev 的 EVIOCGNAME / EVIOCGBIT）分发给字符设备驱动
        _ => {
            let mut out = [0u8; 256];
            let r = vfs::ioctl(fd, request, &mut out);
            if r >= 0 {
                if r > 0 {
                    let copied = unsafe { copy_to_user(arg, &out[..r as usize]) };
                    if copied == 0 {
                        return EFAULT;
                    }
                }
                return 0;
            }
            r as i64
        }
    }
}

/// `getdents64`：把目录 fd 枚举为 `linux_dirent64` 序列（目录流位置由 fd 内部维护）。
fn sys_getdents64(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let dirp = f.x[1];
    let count = f.x[2] as usize;
    if count == 0 {
        return EINVAL;
    }
    if !user_ok_writable(dirp, count) {
        return EFAULT;
    }
    let mut name = [0u8; 32];
    let mut written = 0usize;
    loop {
        let (ino, typ) = vfs::readdir_next(fd, &mut name);
        if ino < 0 {
            break;
        }
        let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        // d_reclen = align(19 + namelen + 1, 8)
        let reclen = (19 + elen + 1 + 7) & !7usize;
        if written + reclen > count {
            break;
        }
        unsafe {
            let p = (dirp + written as u64) as *mut u8;
            *(p as *mut u64) = ino as u64; // d_ino
            *((p as *mut u64).add(1)) = 0; // d_off
            *((p as *mut u16).add(8)) = reclen as u16; // d_reclen
            *((p as *mut u8).add(18)) = dt_of(typ); // d_type
            core::ptr::copy_nonoverlapping(name.as_ptr(), (p as *mut u8).add(19), elen);
            *((p as *mut u8).add(19 + elen)) = 0; // d_name 结尾 NUL
        }
        written += reclen;
    }
    written as i64
}

fn sys_getcwd(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    let size = f.x[1] as usize;
    if size < 2 {
        return ERANGE;
    }
    if !user_ok_writable(buf, size.min(128)) {
        return EFAULT;
    }
    let mut cwd = [0u8; 128];
    let len = process::current_cwd(&mut cwd);
    // 返回的 cwd 必须以 NUL 结尾；缓冲不足返回 ERANGE（Linux 语义）
    if len + 1 > size {
        return ERANGE;
    }
    unsafe {
        copy_to_user(buf, &cwd[..len]);
        *((buf + len as u64) as *mut u8) = 0;
    }
    buf as i64
}

fn sys_chdir(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[0];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    // 目标必须是目录
    match vfs::stat_path(s) {
        Some((t, _)) if t == fs_simple::T_DIR => {
            process::set_current_cwd(s);
            0
        }
        _ => ENOTDIR,
    }
}

fn sys_mkdirat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    vfs::mkdir(s) as i64
}

fn sys_unlinkat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    vfs::unlink(s) as i64
}

fn sys_mknodat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let mode = f.x[2] as u32;
    let dev = f.x[3] as u32;
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    let typ = match mode & 0o170000 {
        0o020000 => vfs::FileType::Chr,
        0o040000 => return vfs::mkdir(s) as i64,
        _ => vfs::FileType::Reg,
    };
    let major = (dev >> 8) & 0xFFF;
    let minor = dev & 0xFF;
    vfs::mknod(s, typ, major, minor) as i64
}

/// `renameat(dirfd, oldpath, newdirfd, newpath)`：单根文件系统，忽略 dirfd（含 AT_FDCWD）。
fn sys_renameat(f: &mut TrapFrame) -> i64 {
    let mut old = [0u8; 128];
    let mut new = [0u8; 128];
    let no = unsafe { read_cstr(f.x[1], &mut old) };
    let nn = unsafe { read_cstr(f.x[3], &mut new) };
    if no == 0 || nn == 0 {
        return EFAULT;
    }
    let o = resolve_user_path(core::str::from_utf8(&old[..no]).unwrap_or(""));
    let n = resolve_user_path(core::str::from_utf8(&new[..nn]).unwrap_or(""));
    // 源必须存在：resolve_parent 阶段不报错，此处区分 ENOENT 与 EEXIST
    let mut dev = 0u32;
    let mut ino = 0u32;
    if vfs::resolve(o, &mut dev, &mut ino) != 0 {
        return ENOENT;
    }
    if vfs::rename(o, n) != 0 {
        EEXIST
    } else {
        0
    }
}

fn sys_faccessat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    let fd = vfs::open(s, vfs::O_RDONLY);
    if fd < 0 {
        ENOENT
    } else {
        vfs::close(fd);
        0
    }
}

/// `readlinkat(dirfd, pathname, buf, bufsiz)`：读取符号链接目标（不跟随）。
///
/// 目标非符号链接返回 `EINVAL`；`bufsiz` 不足时截断（Linux 语义，不补 \0）。
fn sys_readlinkat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let buf_ptr = f.x[2];
    let bufsiz = f.x[3] as usize;
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    let cap = bufsiz.min(256);
    let mut tmp = [0u8; 256];
    let r = vfs::readlink(s, &mut tmp[..cap]);
    if r < 0 {
        return r as i64;
    }
    if !user_ok_writable(buf_ptr, r as usize) {
        return EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf_ptr as *mut u8, r as usize);
    }
    r as i64
}

/// `symlinkat(target, newdirfd, linkpath)`：创建符号链接（单根文件系统忽略 dirfd）。
fn sys_symlinkat(f: &mut TrapFrame) -> i64 {
    let target_ptr = f.x[0];
    let link_ptr = f.x[2];
    let mut target = [0u8; 256];
    let mut link = [0u8; 128];
    let tn = unsafe { read_cstr(target_ptr, &mut target) };
    let ln = unsafe { read_cstr(link_ptr, &mut link) };
    if tn == 0 || ln == 0 {
        return EFAULT;
    }
    let t = core::str::from_utf8(&target[..tn]).unwrap_or("");
    let l = resolve_user_path(core::str::from_utf8(&link[..ln]).unwrap_or(""));
    vfs::symlink(t, l) as i64
}

fn sys_newfstatat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let stat_ptr = f.x[2];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    match vfs::stat_path(s) {
        Some((t, size)) => {
            if !unsafe { write_stat(stat_ptr, t, size) } {
                return EFAULT;
            }
            0
        }
        None => ENOENT,
    }
}

fn sys_fstat(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let stat_ptr = f.x[1];
    match vfs::fstat(fd) {
        Some((t, size)) => {
            if !unsafe { write_stat(stat_ptr, t, size) } {
                return EFAULT;
            }
            0
        }
        None => EBADF,
    }
}

/// `ftruncate`：把 fd 对应文件截断为 0（本内核仅支持截断到 0）。
fn sys_ftruncate(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let length = f.x[1] as i64;
    if length != 0 {
        return EINVAL;
    }
    vfs::truncate(fd) as i64
}

/// `truncate(path, length)`：按路径截断；本内核仅支持截断到 0。
fn sys_truncate(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[0];
    let length = f.x[1] as i64;
    if length != 0 {
        return EINVAL;
    }
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    // 打开（O_WRONLY|O_CREAT|O_TRUNC：truncate 语义会创建不存在的文件）
    let fd = vfs::open(s, 1 | O_CREAT | O_TRUNC);
    if fd < 0 {
        return ENOENT;
    }
    let r = vfs::truncate(fd);
    vfs::close(fd);
    r as i64
}

/// `socketpair(domain, type, protocol, sv)`：创建双向字节流套接字对。
/// 仅支持 `AF_UNIX, SOCK_STREAM`；返回两个 fd 写入 `sv[2]`。
fn sys_socketpair(f: &mut TrapFrame) -> i64 {
    let domain = f.x[0] as i32;
    let ty = f.x[1] as i32;
    let sv = f.x[3];
    if domain != AF_UNIX {
        return EAFNOSUPPORT;
    }
    if ty != SOCK_STREAM {
        return EPROTONOSUPPORT;
    }
    if !user_ok(sv, 8) {
        return EFAULT;
    }
    let (fd0, fd1) = ipc::socketpair_create();
    if fd0 < 0 || fd1 < 0 {
        return EMFILE;
    }
    unsafe {
        *(sv as *mut i32) = fd0;
        *(sv as *mut i32).add(1) = fd1;
    }
    0
}

/// 内核全局剪贴板（256 字节）。`sys_clipboard_set(buf, len)`：写入；
/// `sys_clipboard_get(buf, cap)`：读出，返回长度（-1 空）。
const CLIPBOARD_CAP: usize = 256;
static mut CLIPBOARD: [u8; CLIPBOARD_CAP] = [0; CLIPBOARD_CAP];
static mut CLIPBOARD_LEN: usize = 0;

fn sys_clipboard_set(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    let len = f.x[1] as usize;
    if len == 0 || len > CLIPBOARD_CAP {
        return EINVAL;
    }
    if !user_ok(buf, len) {
        return EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(buf as *const u8, CLIPBOARD.as_mut_ptr(), len);
        CLIPBOARD_LEN = len;
    }
    len as i64
}

fn sys_clipboard_get(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    let cap = f.x[1] as usize;
    let len = unsafe { CLIPBOARD_LEN };
    if len == 0 {
        return 0; // 空剪贴板
    }
    let n = len.min(cap);
    if !user_ok(buf, n) {
        return EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(CLIPBOARD.as_ptr(), buf as *mut u8, n);
    }
    n as i64
}

/// `statx(dirfd, path, flags, mask, buf)`：以 statx 布局返回文件信息。
/// 内核无完整 statx；填充基础字段（类型/大小），其余置 0。
fn sys_statx(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[1];
    let buf = f.x[4];
    if !user_ok(buf, 256) {
        return EFAULT;
    }
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let s = resolve_user_path(core::str::from_utf8(&path[..n]).unwrap_or(""));
    let info = match vfs::stat_path(s) {
        Some(x) => x,
        None => return ENOENT,
    };
    unsafe {
        // struct statx 布局（x86_64/aarch64 均为 256 字节）：
        // mask u32@0, blksize u32@4, attributes u64@8, nlink u32@16,
        // uid u32@20, gid u32@24, mode u16@28, spare u16@30,
        // ino u64@32, size u64@40, blocks u64@48, ...
        let b = buf as *mut u8;
        // mask：STX_TYPE|STX_MODE|STX_NLINK|STX_SIZE = 0x1|0x2|0x10|0x200
        *(b as *mut u32) = 0x213;
        // mode：类型位 + 0644 权限
        let (t, size) = info;
        let typ = if t == crate::fs_simple::T_DIR {
            0o040000
        } else if t == crate::fs_simple::T_CHR {
            0o020000
        } else if t == crate::fs_simple::T_SYM {
            0o120000
        } else {
            0o100000
        };
        *((b.add(28)) as *mut u16) = (typ | 0o644) as u16;
        *((b.add(32)) as *mut u64) = 1; // ino
        *((b.add(40)) as *mut u64) = size as u64; // size
        *((b.add(48)) as *mut u64) = (size as u64 + 511) / 512; // blocks
    }
    0
}

// ==================== 进程 / 线程 ====================

fn sys_exit(f: &mut TrapFrame) -> i64 {
    process::exit(f.x[0] as i32)
}

fn sys_wait4(f: &mut TrapFrame) -> i64 {
    let pid = f.x[0] as i32;
    let status_ptr = f.x[1];
    let (rpid, code) = process::wait(pid);
    if rpid < 0 {
        return ECHILD;
    }
    if status_ptr != 0 {
        if !user_ok_writable(status_ptr, 4) {
            return EFAULT;
        }
        unsafe {
            *(status_ptr as *mut i32) = (code & 0xFF) << 8;
        }
    }
    rpid as i64
}

fn sys_kill(f: &mut TrapFrame) -> i64 {
    let pid = f.x[0] as i32;
    let sig = f.x[1] as u32;
    // 信号号校验：内核仅使用 0..64 位图
    if sig >= 64 {
        return EINVAL;
    }
    // pid <= 0（组/广播信号）：本内核无进程组，静默成功
    if pid <= 0 {
        return 0;
    }
    // 目标进程必须存在，否则 ESRCH
    if !process::process_exists(pid as u32) {
        return ESRCH;
    }
    process::signal_send(pid as u32, sig);
    // 唤醒阻塞中的目标进程：其睡眠/等待系统调用返回后，下一次进入内核时
    // dispatch 检查信号位图并终止（SIGKILL）或继续（其它信号）。
    crate::sched_cfs::wake_pid(pid as u32);
    0
}

/// `clone`：不带 `CLONE_THREAD` 时按 `fork` 处理（完整复制进程）；
/// 带 `CLONE_THREAD` 时创建共享地址空间的线程。
///
/// AArch64 参数：x0=flags, x1=child_stack, x2=parent_tidptr, x3=tls, x4=child_tidptr。
fn sys_clone(f: &mut TrapFrame) -> i64 {
    let flags = f.x[0];
    let child_stack = f.x[1] as usize;
    let parent_tidptr = f.x[2] as usize;
    let tls = f.x[3] as usize;
    let child_tidptr = f.x[4] as usize;

    if flags & CLONE_THREAD == 0 {
        // 标准 fork 语义（如 clone(SIGCHLD)）：父返回子 pid，子返回 0。
        return process::fork(f as *mut TrapFrame as usize) as i64;
    }
    if flags & CLONE_VM == 0 {
        // CLONE_THREAD 必须配合 CLONE_VM（共享地址空间）
        return EINVAL;
    }
    // tls 始终写入 TPIDR_EL0（CLONE_SETTLS 语义）
    bionic_shim::clone_thread(f as *mut TrapFrame as usize, child_stack, tls, parent_tidptr, child_tidptr) as i64
}

/// `execve`：读取 path/argv/envp（先拷贝到内核缓冲，旧地址空间随后被释放），
/// 交给 ELF 加载器替换当前进程映像。
///
/// 路径/参数缓冲放在**本系统调用栈帧**而非 static 上：syscall 期间可被定时器
/// 抢占，若另一任务并发 execve 会覆写共享的 static 缓冲，导致本任务读到
/// 被污染的参数/路径（此前 EXEC_PATH/ARGV/ENVP 为 static mut，存在此竞态）。
fn sys_execve(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[0];
    let argv_ptr = f.x[1];
    let envp_ptr = f.x[2];

    let mut exec_path = [0u8; 128];
    let mut exec_argv: [[u8; EXEC_STR_LEN]; MAX_EXEC_ARGS] =
        [[0; EXEC_STR_LEN]; MAX_EXEC_ARGS];
    let mut exec_envp: [[u8; EXEC_STR_LEN]; MAX_EXEC_ARGS] =
        [[0; EXEC_STR_LEN]; MAX_EXEC_ARGS];

    unsafe {
        let path_len = read_cstr(path_ptr, &mut exec_path);
        if path_len == 0 {
            return EFAULT;
        }
        let path = core::str::from_utf8(&exec_path[..path_len]).unwrap_or("");

        let argc = read_str_array(argv_ptr, &mut exec_argv);
        let envc = read_str_array(envp_ptr, &mut exec_envp);

        let mut argv: [&str; MAX_EXEC_ARGS] = [""; MAX_EXEC_ARGS];
        for i in 0..argc {
            let l = exec_argv[i].iter().position(|&c| c == 0).unwrap_or(exec_argv[i].len());
            argv[i] = core::str::from_utf8(&exec_argv[i][..l]).unwrap_or("");
        }
        let mut envp: [&str; MAX_EXEC_ARGS] = [""; MAX_EXEC_ARGS];
        for i in 0..envc {
            let l = exec_envp[i].iter().position(|&c| c == 0).unwrap_or(exec_envp[i].len());
            envp[i] = core::str::from_utf8(&exec_envp[i][..l]).unwrap_or("");
        }

        elf::execve(path, &argv[..argc], &envp[..envc], f) as i64
    }
}

/// 从用户空间读取一个指针数组（每项指向 NUL 结尾字符串），逐项拷贝到 `bufs`。
///
/// 返回读到的字符串个数（0 表示空或非法）。
unsafe fn read_str_array(arr_ptr: u64, bufs: &mut [[u8; EXEC_STR_LEN]; MAX_EXEC_ARGS]) -> usize {
    if arr_ptr == 0 || !user_ok(arr_ptr, 8) {
        return 0;
    }
    let mut n = 0usize;
    while n < MAX_EXEC_ARGS {
        // 逐指针校验，避免越过用户区读取指针数组
        if !user_ok(arr_ptr + (n * 8) as u64, 8) {
            break;
        }
        let p = *(arr_ptr as *const u64).add(n);
        if p == 0 {
            break;
        }
        let mut i = 0usize;
        while i < EXEC_STR_LEN - 1 {
            if !user_ok(p + i as u64, 1) {
                break;
            }
            let c = *((p + i as u64) as *const u8);
            if c == 0 {
                break;
            }
            bufs[n][i] = c;
            i += 1;
        }
        bufs[n][i] = 0;
        n += 1;
    }
    n
}

/// `futex`：pthread 互斥量/条件变量的等待与唤醒。
fn sys_futex(f: &mut TrapFrame) -> i64 {
    let addr = f.x[0] as usize;
    let op = f.x[1] as u32;
    let val = f.x[2] as u32;
    let _timeout = f.x[3];
    let addr2 = f.x[4] as usize;
    let val3 = f.x[5] as u32;

    let (base_op, private) = (op & 0x7F, op & FUTEX_PRIVATE_FLAG != 0);
    let _ = private;
    match base_op {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => bionic_shim::futex_wait(addr, val) as i64,
        FUTEX_WAKE | FUTEX_WAKE_BITSET => bionic_shim::futex_wake(addr, val) as i64,
        FUTEX_REQUEUE => bionic_shim::futex_requeue(addr, val, addr2) as i64,
        FUTEX_CMP_REQUEUE => {
            // 简化：先唤醒，再把其余等待者迁移
            let _ = val3;
            bionic_shim::futex_requeue(addr, val, addr2) as i64
        }
        FUTEX_WAKE_OP => 0,
        FUTEX_FD => EINVAL,
        _ => ENOSYS,
    }
}

// ==================== 内存 ====================

fn sys_brk(f: &mut TrapFrame) -> i64 {
    let addr = f.x[0] as usize;
    if addr == 0 {
        return bionic_shim::brk_get() as i64;
    }
    bionic_shim::brk_set(addr) as i64
}

/// `mmap`：匿名映射（MAP_ANONYMOUS）分配新内存；`/dev/fb0` 映射到显存。
fn sys_mmap(f: &mut TrapFrame) -> i64 {
    let addr = f.x[0] as usize;
    let len = f.x[1] as usize;
    let _prot = f.x[2] as u32;
    let flags = f.x[3] as u32;
    let fd = f.x[4] as i32;
    let _offset = f.x[5];

    if len == 0 {
        return EINVAL;
    }
    if flags & MAP_ANONYMOUS == 0 {
        // 文件映射：帧缓冲设备 /dev/fb0 走显存直映；普通文件走私有页拷贝
        if let Some(rdev) = vfs::open_rdev(fd) {
            if rdev >> 16 == devfs::FB_MAJOR {
                return map_framebuffer(addr, len);
            }
        }
        return map_file(f, len, fd, _offset as usize);
    }
    // 匿名映射：优先遵守 MAP_FIXED 的地址提示
    if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 && addr != 0 && addr % vm::PAGE_SIZE == 0 {
        if !vm::user_va_range_ok(addr, len) {
            return EINVAL;
        }
        let pages = len.div_ceil(vm::PAGE_SIZE);
        // MAP_FIXED 语义：先释放目标范围内已有的旧映射（防页泄漏与双重映射）
        process::unmap_user_range(addr, pages * vm::PAGE_SIZE);
        // 按需分页：仅预留地址空间，物理页延后到首次访问（page fault）时分配
        let mut reserved = 0usize;
        for i in 0..pages {
            if !process::reserve_user_page(addr + i * vm::PAGE_SIZE) {
                // 记录表满：回滚已预留页，保持地址空间一致（不泄漏）
                process::rollback_user_pages(addr, reserved);
                return ENOMEM;
            }
            reserved += 1;
        }
        return addr as i64;
    }
    bionic_shim::mmap_anon(len)
}

/// `munmap(addr, len)`：解除 `[addr, addr+len)` 的进程私有映射并归还物理页。
///
/// 只处理记录表中的私有映射（匿名 mmap/brk 页）；共享内存、帧缓冲等
/// 非私有映射不在此列，不会被误释放。地址/长度按页向下/向上取整。
fn sys_munmap(f: &mut TrapFrame) -> i64 {
    let addr = f.x[0] as usize;
    let len = f.x[1] as usize;
    if len == 0 {
        return EINVAL;
    }
    let start = addr & !(vm::PAGE_SIZE - 1);
    // checked 加法防回绕；上界按用户地址空间约束
    let Some(end) = addr.checked_add(len) else { return EINVAL };
    if end <= start {
        return 0;
    }
    if !vm::user_va_range_ok(start, end - start) {
        return EINVAL;
    }
    process::unmap_user_range(start, end - start);
    0
}

/// 帧缓冲映射的固定用户虚拟地址（低于匿名 mmap 游标 0x3000_0000，无冲突）。
const FB_MMAP_ADDR: usize = 0x2000_0000;

/// 普通文件 mmap 游标（每进程，从 0x2E000000 向上，低于匿名游标 0x30000000）。
static mut FILE_MMAP_NEXT: [usize; process::MAX_PROC] = [0x2E00_0000; process::MAX_PROC];

/// 文件映射（MAP_PRIVATE 语义）：读取文件内容到私有物理页并映射进用户空间。
/// 支持任意 offset/len；越界部分截断到文件末尾；文件为空或 offset 越界返回错误。
fn map_file(f: &mut TrapFrame, len: usize, fd: i32, offset: usize) -> i64 {
    if len == 0 {
        return EINVAL;
    }
    let Some((typ, fsize)) = vfs::fstat(fd) else { return EBADF };
    if typ != crate::fs_simple::T_REG || offset >= fsize as usize {
        return EINVAL;
    }
    let map_len = len.min(fsize as usize - offset);
    let pages = map_len.div_ceil(vm::PAGE_SIZE);
    if pages == 0 {
        return EINVAL;
    }
    let pid = process::current_pid();
    let va = unsafe { FILE_MMAP_NEXT[pid as usize] };
    if !vm::user_va_range_ok(va, pages * vm::PAGE_SIZE) {
        return ENOMEM;
    }
    unsafe { FILE_MMAP_NEXT[pid as usize] = va + pages * vm::PAGE_SIZE; }

    let mut off = offset;
    let mut mapped = 0usize;
    for i in 0..pages {
        let pa = match crate::allocator::alloc_page() {
            Some(p) => p,
            None => {
                process::rollback_user_pages(va, mapped);
                return ENOMEM;
            }
        };
        unsafe { core::ptr::write_bytes(pa as *mut u8, 0, vm::PAGE_SIZE) };
        // 逐页读取文件内容（从当前 offset 起，最多一页）
        vfs::seek(fd, off as i64, vfs::SEEK_SET);
        let mut filled = 0usize;
        while filled < vm::PAGE_SIZE && off + filled < fsize as usize {
            let want = (vm::PAGE_SIZE - filled).min(fsize as usize - off - filled);
            let mut tmp = [0u8; 4096];
            let n = vfs::read(fd, &mut tmp[..want]);
            if n <= 0 {
                break;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(tmp.as_ptr(), (pa + filled) as *mut u8, n as usize);
            }
            filled += n as usize;
        }
        off += filled;
        if !process::map_user_page_owned(va + i * vm::PAGE_SIZE, pa) {
            crate::allocator::free_page(pa);
            process::rollback_user_pages(va, mapped);
            return ENOMEM;
        }
        mapped += 1;
    }
    va as i64
}

/// 把 `/dev/fb0` 的显存映射到当前进程用户地址空间，返回起始虚拟地址。
fn map_framebuffer(addr: usize, len: usize) -> i64 {
    let (base, fb_len) = devfs::fb_map_info();
    if base == 0 || fb_len == 0 {
        return ENODEV;
    }
    let map_len = len.min(fb_len);
    if map_len == 0 {
        return EINVAL;
    }
    let va = if addr == 0 { FB_MMAP_ADDR } else { addr };
    if va % vm::PAGE_SIZE != 0 {
        return EINVAL;
    }
    let pages = map_len.div_ceil(vm::PAGE_SIZE);
    if !vm::user_va_range_ok(va, pages * vm::PAGE_SIZE) {
        return EINVAL;
    }
    let root = process::current_root();
    for i in 0..pages {
        vm::map_page_in(root, va + i * vm::PAGE_SIZE, base + i * vm::PAGE_SIZE, vm::FLAG_USER_DATA);
    }
    va as i64
}

// ==================== 时间 ====================

fn now_usec() -> u64 {
    // gettimeofday/clock_gettime 返回墙钟时间（有 RTC 时 = 纪元秒 + 单调时钟）
    crate::rtc::wall_time_us()
}

fn sys_gettimeofday(f: &mut TrapFrame) -> i64 {
    let tv = f.x[0];
    if tv != 0 {
        if !user_ok_writable(tv, 16) {
            return EFAULT;
        }
        let us = now_usec();
        unsafe {
            *(tv as *mut i64) = (us / 1_000_000) as i64;
            *((tv + 8) as *mut i64) = (us % 1_000_000) as i64;
        }
    }
    0
}

fn sys_clock_gettime(f: &mut TrapFrame) -> i64 {
    let _clk = f.x[0] as i32;
    let tp = f.x[1];
    if tp != 0 {
        if !user_ok_writable(tp, 16) {
            return EFAULT;
        }
        let us = now_usec();
        unsafe {
            *(tp as *mut i64) = (us / 1_000_000) as i64;
            *((tp + 8) as *mut i64) = ((us % 1_000_000) * 1000) as i64;
        }
    }
    0
}

fn sys_clock_getres(f: &mut TrapFrame) -> i64 {
    let tp = f.x[1];
    if tp != 0 {
        if !user_ok_writable(tp, 16) {
            return EFAULT;
        }
        unsafe {
            *(tp as *mut i64) = 0;
            *((tp + 8) as *mut i64) = 1_000_000;
        }
    }
    0
}

fn sys_nanosleep(f: &mut TrapFrame) -> i64 {
    let req = f.x[0];
    if !user_ok(req, 16) {
        return EFAULT;
    }
    let (sec, nsec) = unsafe {
        let s = *(req as *const i64);
        let n = *((req + 8) as *const i64);
        (s, n)
    };
    // 饱和运算防止 sec*1_000_000 u64 溢出（长睡眠被卷成短睡眠）
    let us = (sec.max(0) as u64)
        .saturating_mul(1_000_000)
        .saturating_add((nsec.max(0) as u64) / 1000);
    if us != 0 {
        // ★ 必须用单调时钟：sleep_until 的 deadline 与调度器 tick 的
        // `timer::now_us()` 比较。此前误用墙钟（RTC 纪元 µs，~1.78e15），
        // 单调时钟永远到不了该值 → sleeper 永不唤醒（进程永久睡眠）。
        sched_cfs::sleep_until(crate::timer::now_us().saturating_add(us));
    }
    0
}

// ==================== 系统信息 ====================

fn sys_uname(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    unsafe {
        if !user_ok_writable(buf, 6 * 65) {
            return EFAULT;
        }
        let p = buf as *mut u8;
        core::ptr::write_bytes(p, 0, 6 * 65);
        write_cstr(buf, "KunxOS");
        write_cstr(buf + 65, "kunx");
        write_cstr(buf + 130, "1.0.0");
        write_cstr(buf + 195, "KunxOS aarch64");
        write_cstr(buf + 260, "aarch64");
        write_cstr(buf + 325, "(none)");
    }
    0
}

fn sys_sysinfo(f: &mut TrapFrame) -> i64 {
    let info = f.x[0];
    if info != 0 {
        unsafe {
            if !user_ok_writable(info, 112) {
                return EFAULT;
            }
            let p = info as *mut u8;
            core::ptr::write_bytes(p, 0, 112);
            // uptime（秒）@0
            *(p as *mut i64) = (now_usec() / 1_000_000) as i64;
            // totalram @32、freeram @40：按物理页分配器真实统计（此前硬编码 128MB，
            // 与实际 512MB RAM 不符）
            let total_pages = crate::allocator::total_pages();
            let used = crate::allocator::used_pages();
            *((p as *mut u64).add(4)) = (total_pages * crate::allocator::PAGE_SIZE) as u64;
            *((p as *mut u64).add(5)) =
                ((total_pages.saturating_sub(used)) * crate::allocator::PAGE_SIZE) as u64;
        }
    }
    0
}

fn sys_times(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    if buf != 0 {
        unsafe {
            if user_ok_writable(buf, 4 * 16) {
                core::ptr::write_bytes(buf as *mut u8, 0, 4 * 16);
            }
        }
    }
    (now_usec() / 10_000) as i64
}

fn sys_getrandom(f: &mut TrapFrame) -> i64 {
    let buf = f.x[0];
    let len = f.x[1] as usize;
    if len == 0 {
        return 0;
    }
    let n = len.min(256);
    let mut tmp = [0u8; 256];
    // 优先使用 virtio-rng 硬件熵源（/dev/urandom 同源）；无 RNG 时回退
    // 到计时器 + tid 混合的伪随机（演示系统非密码学安全）。
    let r = crate::virtio_rng::get_random(&mut tmp[..n]);
    let filled = if r > 0 {
        r as usize
    } else {
        let mut seed = now_usec()
            .wrapping_mul(0x9E3779B97F4A7C15)
            .rotate_left(17)
            ^ ((sched_cfs::current_tid() as u64) << 32)
            ^ 0xDEAD_BEEF;
        for i in 0..n {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            tmp[i] = (seed >> 33) as u8;
        }
        n
    };
    if !user_ok_writable(buf, filled) {
        return EFAULT;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, filled);
    }
    filled as i64
}

// ==================== 套接字（映射到 Phase 7 socket 层） ====================

const AF_UNIX: i32 = 1;
const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;

/// 读取 `sockaddr_in`：返回 (port, ip)。
unsafe fn read_sockaddr(addr: u64) -> (u16, u32) {
    if !user_ok(addr, 8) {
        return (0, 0);
    }
    let port = u16::from_be_bytes([*(addr as *const u8).add(2), *(addr as *const u8).add(3)]);
    let ip = u32::from_be_bytes([
        *(addr as *const u8).add(4),
        *(addr as *const u8).add(5),
        *(addr as *const u8).add(6),
        *(addr as *const u8).add(7),
    ]);
    (port, ip)
}

fn sys_socket(f: &mut TrapFrame) -> i64 {
    let domain = f.x[0] as i32;
    let ty = f.x[1] as i32;
    if domain != AF_INET {
        return EAFNOSUPPORT;
    }
    match ty {
        SOCK_STREAM => socket::socket_tcp() as i64,
        SOCK_DGRAM => socket::socket_udp() as i64,
        _ => EPROTONOSUPPORT,
    }
}

fn sys_connect(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let (port, ip) = unsafe { read_sockaddr(f.x[1]) };
    socket::connect(fd, ip, port) as i64
}

fn sys_bind(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let (port, _ip) = unsafe { read_sockaddr(f.x[1]) };
    socket::bind(fd, port) as i64
}

fn sys_sendto(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let len = f.x[2] as usize;
    let (port, ip) = unsafe { read_sockaddr(f.x[4]) };
    let n = len.min(4096);
    let mut tmp = [0u8; 4096];
    let c = unsafe { copy_from_user(buf_ptr, &mut tmp[..n]) };
    if c == 0 {
        return EFAULT;
    }
    if ip != 0 {
        socket::send_to(fd, &tmp[..c], ip, port) as i64
    } else {
        socket::send(fd, &tmp[..c]) as i64
    }
}

fn sys_recvfrom(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let buf_ptr = f.x[1];
    let len = f.x[2] as usize;
    let n = len.min(4096);
    let mut tmp = [0u8; 4096];
    let r = socket::recv(fd, &mut tmp[..n]);
    if r <= 0 {
        return r as i64;
    }
    let copied = unsafe { copy_to_user(buf_ptr, &tmp[..r as usize]) };
    if copied != r as usize {
        return EFAULT;
    }
    r as i64
}

fn sys_shutdown(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    socket::close(fd);
    0
}

/// msghdr：`{ name: u64, namelen: u32, iov: u64, iovlen: u64, ... }`。
fn sys_sendmsg(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let msg = f.x[1];
    if !user_ok(msg, 24) {
        return EFAULT;
    }
    unsafe {
        let iov = *(msg as *const u64).add(2);
        if !user_ok(iov, 16) {
            return EFAULT;
        }
        let base = *(iov as *const u64);
        let len = *(iov as *const u64).add(1) as usize;
        let n = len.min(4096);
        let mut tmp = [0u8; 4096];
        let c = copy_from_user(base, &mut tmp[..n]);
        if c == 0 {
            return EFAULT;
        }
        socket::send(fd, &tmp[..c]) as i64
    }
}

fn sys_recvmsg(f: &mut TrapFrame) -> i64 {
    let fd = f.x[0] as i32;
    let msg = f.x[1];
    if !user_ok(msg, 24) {
        return EFAULT;
    }
    unsafe {
        let iov = *(msg as *const u64).add(2);
        if !user_ok(iov, 16) {
            return EFAULT;
        }
        let base = *(iov as *const u64);
        let len = *(iov as *const u64).add(1) as usize;
        let n = len.min(4096);
        let mut tmp = [0u8; 4096];
        let r = socket::recv(fd, &mut tmp[..n]);
        if r <= 0 {
            return r as i64;
        }
        copy_to_user(base, &tmp[..r as usize]);
        r as i64
    }
}
