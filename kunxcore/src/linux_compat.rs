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
use crate::compositor;
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

use core::sync::atomic::{AtomicBool, Ordering};

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
const ENAMETOOLONG: i64 = -36;
const ENOSYS: i64 = -38;
const ENOTEMPTY: i64 = -39;
const EAFNOSUPPORT: i64 = -97;
const EPROTONOSUPPORT: i64 = -93;

// ==================== mmap/mprotect 保护标志（Linux PROT_*） ====================

/// PROT_READ：页面可读。
const PROT_READ: u32 = 0x1;
/// PROT_WRITE：页面可写。
const PROT_WRITE: u32 = 0x2;
/// PROT_EXEC：页面可执行。
const PROT_EXEC: u32 = 0x4;

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
/// 自定义：DNS 域名解析（返回主机字节序 IPv4，0 = 失败）。
const SYS_DNS_RESOLVE: usize = 403;
/// 自定义：内核内存压力等级（0 = Normal / 1 = Moderate / 2 = Critical）。
/// 供 lifd 轮询内存压力（内核定时器 tick 计算，见 `mem_pressure` 模块）。
const SYS_LC_MEMPRESSURE: usize = 404;
/// 自定义：KunxOS OTA 系统调用（430，避开已用 401-404 / 410-413）。
const SYS_KUNX_OTA: usize = 430;
const OTA_CMD_INSTALL: u64 = 1;
const OTA_CMD_CHECK: u64 = 2;
const OTA_CMD_STATUS: u64 = 3;
const OTA_CMD_SET_RECOVERY: u64 = 5;
const OTA_CMD_FORCE_SLOT: u64 = 6;
const OTA_CMD_WIPE_DATA: u64 = 7;
// 自定义：电源管理（410-413，避开已用 401-404）。
/// 查询电源状态打包值（见 `power::sys_pm_get` 的位布局），供应用低成本轮询。
const SYS_KUNX_PM_GET: usize = 410;
/// 切换 DVFS 调速器（x0 = governor id 0..3）。
const SYS_KUNX_PM_SET_GOVERNOR: usize = 411;
/// 设置进程 CPU 上限（Doze）：x0 = pid，x1 = cap 0..100。
const SYS_KUNX_PM_SET_CPU_CAP: usize = 412;
/// 冻结 / 解冻进程（Doze）：x0 = pid，x1 = 0/1。
const SYS_KUNX_PM_FREEZE: usize = 413;
/// 自定义：当前任务设置一次性调度 deadline（x0 = 相对期限 µs，0 = 清除）。
/// 统一响应 deadline 体系的用户态入口（渲染线程把帧预算显式告知调度器）。
const SYS_KUNX_SCHED_SET_DEADLINE: usize = 414;
/// 故障注入（开发/CI 验证遏制路径，root 专用）：x0=1 在 S 态解引用坏地址
/// x1（触发 kernel_fault_contain），x0=2 内核切片越界（触发 panic 遏制）。
/// 真正注入时进程被 SIGSEGV 语义终止（不返回）；crashme kfault/kpanic 用。
const SYS_KUNX_FAULT_INJECT: usize = 415;

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
pub const USER_TOP: u64 = 0x4000_0000;

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
    // PAN 下 EL1 直读用户内存需临时放行（无 PAN 时为空操作）
    crate::mmu::uaccess_begin();
    core::ptr::copy_nonoverlapping(src as *const u8, dst.as_mut_ptr(), dst.len());
    crate::mmu::uaccess_end();
    dst.len()
}

unsafe fn copy_to_user(dst: u64, src: &[u8]) -> usize {
    if !user_ok_writable(dst, src.len()) {
        return 0;
    }
    crate::mmu::uaccess_begin();
    core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len());
    crate::mmu::uaccess_end();
    src.len()
}

/// PAN 下写入用户 `u64`（安全放行封装）。调用方须已用 `user_ok_writable`
/// 校验目标范围合法且已映射（COW 页须已 break），与 `copy_to_user` 同规约。
unsafe fn write_u64_to_user(dst: u64, v: u64) {
    crate::mmu::uaccess_begin();
    *(dst as *mut u64) = v;
    crate::mmu::uaccess_end();
}

/// PAN 下写入用户 `u32`（安全放行封装），同 [`write_u64_to_user`] 规约。
unsafe fn write_u32_to_user(dst: u64, v: u32) {
    crate::mmu::uaccess_begin();
    *(dst as *mut u32) = v;
    crate::mmu::uaccess_end();
}

/// 从用户空间读取以 NUL 结尾的字符串，返回其长度（不含 NUL）。
unsafe fn read_cstr(addr: u64, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let mut i = 0usize;
    // 整个读取过程处于 uaccess 区（PAN 放行）；user_ok 逐字节校验边界，
    // 避免越过用户区读入内核恒等映射内存
    crate::mmu::uaccess_begin();
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
    crate::mmu::uaccess_end();
    buf[i] = 0;
    i
}

unsafe fn write_cstr(dst: u64, s: &str) -> usize {
    copy_to_user(dst, s.as_bytes())
}

/// 把用户传入的路径解析为绝对路径（基于当前进程 cwd），写入调用方提供的
/// `buf` 并返回其切片。
///
/// 相对路径按 `cwd + "/" + 路径` 拼接；路径中的 `.`/`..` 分量由 VFS 层
/// （目录项查找）自然处理。结果写入**调用方栈上缓冲**而非共享静态缓冲：
/// syscall 体可被定时器抢占，若使用 static 缓冲，并发任务的路径解析会覆写
/// 本任务已解析但尚未使用的路径（跨进程路径污染）。
/// 返回 `None` 表示路径超长（调用方返回 ENAMETOOLONG）——此前对超长路径
/// 按字节静默截断，截断点既可能落在多字节字符中间（`from_utf8_unchecked`
/// 构造非法 &str，UB），也可能恰好截出另一个**真实存在**的路径前缀（
/// open 打开错误的文件）。显式报错是 POSIX（ENAMETOOLONG）语义。
fn resolve_user_path<'a>(p: &str, buf: &'a mut [u8; 128]) -> Option<&'a str> {
    buf.fill(0);
    if p.starts_with('/') {
        if p.len() > buf.len() - 1 {
            return None;
        }
        let n = p.len();
        buf[..n].copy_from_slice(&p.as_bytes()[..n]);
        // 输入 &str 已是合法 UTF-8，字节级完整拷贝后仍合法
        return Some(unsafe {
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(buf.as_ptr(), n))
        });
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
    if p.len() > space {
        return None;
    }
    let n = p.len();
    buf[off..off + n].copy_from_slice(&p.as_bytes()[..n]);
    Some(unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(buf.as_ptr(), off + n))
    })
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
    // 临时诊断：白名单外的 syscall 打印（定位 musl 程序启动卡点）
    const COMMON: [usize; 22] = [
        63, 64, 66, 56, 57, 214, 222, 215, 101, 169, 98, 278, 29, 62, 80, 61, 172, 96, 93, 94,
        78, 113,
    ];
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
///
/// 入口/出口维护 syscall 上下文标记（`sched_cfs::syscall_enter/leave`）：
/// 其间发生的内核 panic / S 态异常可被故障遏制归咎于当前用户进程并只
/// 终止它，整机不停机。
pub fn dispatch(nr: usize, f: &mut TrapFrame) -> i64 {
    sched_cfs::syscall_enter();
    let ret = dispatch_inner(nr, f);
    sched_cfs::syscall_leave();
    ret
}

fn dispatch_inner(nr: usize, f: &mut TrapFrame) -> i64 {
    trace_syscall(nr, f);
    // 信号投递：待处理信号在 syscall 入口检查；已注册 handler 的信号改写
    // 陷阱帧进入 handler（跳过本次 syscall），未注册的按默认动作（终止/忽略）。
    if crate::signal::handle_pending(f) {
        // 信号已投递，本 syscall 被信号打断：返回 -EINTR（此前返回 0，
        // 调用方会把被打断的 read 等误认为 EOF/成功）。
        return EINTR;
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
        SYS_STATFS => sys_statfs(f),
        SYS_FACCESSAT => sys_faccessat(f),
        SYS_READLINKAT => sys_readlinkat(f),
        SYS_SYMLINKAT => sys_symlinkat(f),
        SYS_NEWFSTATAT => sys_newfstatat(f),
        SYS_FSTAT => sys_fstat(f),
        SYS_FTRUNCATE => sys_ftruncate(f),
        SYS_TRUNCATE => sys_truncate(f),
        SYS_SYNC => vfs::sync_all() as i64,
        SYS_FSYNC | SYS_FDATASYNC => vfs::fsync(f.x[10] as i32) as i64,
        SYS_FALLOCATE => ENOSYS, // 不支持文件预分配：明确报错而非假装成功
        // Linux ABI：93=exit 仅终止当前线程（最后一个线程退出才终结进程），
        // 94=exit_group 终止整个线程组。此前混同 —— 任何工作线程收尾都会
        // kill_pid 整个进程，pthread_join 永远等不到。
        SYS_EXIT => sys_exit_thread(f),
        SYS_EXIT_GROUP => sys_exit(f),
        SYS_GETPID => process::current_pid() as i64,
        SYS_GETPPID => process::current_parent() as i64,
        SYS_GETUID | SYS_GETEUID => process::current_uid() as i64,
        SYS_GETGID | SYS_GETEGID => process::current_gid() as i64,
        SYS_SETUID => sys_setuid(f),
        SYS_SETGID => sys_setgid(f),
        SYS_GETTID => sched_cfs::current_tid() as i64,
        SYS_SET_TID_ADDRESS => bionic_shim::set_tid_address(f.x[10] as usize) as i64,
        SYS_FUTEX => sys_futex(f),
        SYS_SET_ROBUST_LIST | SYS_GET_ROBUST_LIST => 0,
        SYS_UNAME => sys_uname(f),
        SYS_SETHOSTNAME => ENOSYS, // 不支持修改主机名（uname 只读）
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
        // 无掩码/等待机制：明确报 ENOSYS，避免调用方误以为已生效。
        // musl 启动路径 __block_all_sigs 会调用（忽略返回值），成功桩即可
        SYS_RT_SIGPROCMASK => 0,
        // pselect6/ppoll：monkey 等 poll()/select() 事件循环依赖（musl 的
        // poll() 走 ppoll）。pselect6 按 fd_set 语义（x0=nfds、x1=readfds）；
        // ppoll 是 pollfd 数组 ABI（x0=数组指针、x1=条数），各自独立实现。
        SYS_PSELECT6 => sys_poll_family(f, 4),
        SYS_PPOLL => sys_ppoll(f),
        SYS_RT_SIGPENDING | SYS_RT_SIGSUSPEND => ENOSYS,
        SYS_SIGALTSTACK => 0, // 信号处理不依赖 altstack，接受调用
        SYS_WAIT4 => sys_wait4(f),
        SYS_BRK => sys_brk(f),
        SYS_MMAP => sys_mmap(f),
        SYS_MUNMAP => sys_munmap(f),
        SYS_MPROTECT => sys_mprotect(f),
        // 映射为写穿且无换页：msync/mlock/madvise 等咨询性调用无需动作。
        SYS_MSYNC | SYS_MLOCK | SYS_MUNLOCK | SYS_MLOCKALL | SYS_MUNLOCKALL
        | SYS_MINCORE | SYS_MADVISE => 0,
        SYS_CLONE => sys_clone(f),
        SYS_EXECVE | SYS_EXECVEAT => sys_execve(f),
        SYS_PRCTL => 0, // PR_SET_NAME 等良性调用，无对应机制时接受
        // umask：返回旧值并更新（此前恒返回 0o022、忽略参数，语义错误）。
        SYS_UMASK => sys_umask(f.x[10] as u32),
        // getrlimit 必须把默认值写入用户缓冲区（否则调用方读到垃圾）。
        SYS_GETRLIMIT => sys_getrlimit(f),
        SYS_PRLIMIT64 => sys_prlimit64(f),
        SYS_SETRLIMIT => 0, // 接受设置（不强制限额）
        SYS_GETRANDOM => sys_getrandom(f),
        SYS_SYSINFO => sys_sysinfo(f),
        SYS_TIMES => sys_times(f),
        SYS_GETPRIORITY => 0, // 默认优先级 0（getpriority 语义上合法）
        SYS_SETPRIORITY => sys_setpriority(f),
        SYS_SOCKET => sys_socket(f),
        SYS_CONNECT => sys_connect(f),
        SYS_BIND => sys_bind(f),
        SYS_LISTEN => socket::listen(f.x[10] as i32, f.x[11] as usize) as i64,
        SYS_ACCEPT | SYS_ACCEPT4 => socket::accept(f.x[10] as i32) as i64,
        SYS_SENDTO => sys_sendto(f),
        SYS_RECVFROM => sys_recvfrom(f),
        SYS_SETSOCKOPT | SYS_GETSOCKOPT => ENOSYS, // 未实现：明确报错而非假装成功
        SYS_SHUTDOWN => sys_shutdown(f),
        SYS_SENDMSG => sys_sendmsg(f),
        SYS_RECVMSG => sys_recvmsg(f),
        SYS_SOCKETPAIR => sys_socketpair(f),
        SYS_CLIPBOARD_SET => sys_clipboard_set(f),
        SYS_CLIPBOARD_GET => sys_clipboard_get(f),
        SYS_KUNX_OTA => sys_kunx_ota(f),
        SYS_DNS_RESOLVE => sys_dns_resolve(f),
        SYS_REBOOT => sys_reboot(f),
        SYS_LC_MEMPRESSURE => crate::mem_pressure::level() as i64,
        SYS_STATX => sys_statx(f),
        // 电源管理（自研 syscall：查询 / 调速器 / Doze）
        SYS_KUNX_PM_GET => crate::power::sys_pm_get() as i64,
        SYS_KUNX_PM_SET_GOVERNOR => {
            match crate::power::Governor::from_id(f.x[10] as u32) {
                Some(g) => {
                    crate::power::set_governor(g);
                    0
                }
                None => EINVAL,
            }
        }
        SYS_KUNX_PM_SET_CPU_CAP => {
            let pid = f.x[10] as u32;
            let cap = f.x[11] as u16;
            if cap > 100 {
                EINVAL
            } else if !pm_target_allowed(pid) {
                // 冻结/限速能把任意进程调度权重打到 0（cpu_cap==0 的任务
                // pick_next 直接跳过 = 软冻结），权限必须与 sys_kill 对齐。
                EPERM
            } else {
                crate::sched_cfs::set_task_cpu_cap(pid, cap) as i64
            }
        }
        SYS_KUNX_PM_FREEZE => {
            let pid = f.x[10] as u32;
            if !pm_target_allowed(pid) {
                EPERM
            } else {
                let freeze = f.x[11] != 0;
                crate::sched_cfs::set_task_cpu_cap(pid, if freeze { 0 } else { 100 }) as i64
            }
        }
        SYS_KUNX_SCHED_SET_DEADLINE => crate::sched_cfs::set_current_deadline(f.x[10] as u64),
        SYS_KUNX_FAULT_INJECT => sys_kunx_fault_inject(f),
        // 其余未实现系统调用：统一返回 ENOSYS（此前误路由到 sys_statx，
        // 会向调用方提供的地址写入垃圾并返回误导性 errno）。
        // 临时诊断：打印未实现调用号（帮助移植 musl 程序定位缺口）
        _ => {
            crate::uart_send_str("[syscall-unimpl] nr=");
            crate::print_dec(nr as u64);
            crate::uart_send_str("\n");
            ENOSYS
        }
    }
}

/// 故障注入（SYS_KUNX_FAULT_INJECT，root 专用，开发/CI 用）：
/// - x0=1：S 态解引用 x1 地址（未映射 → S 态缺页 → `kernel_fault_contain`
///   遏制路径：tombstone + 终止本进程，系统继续运行）；
/// - x0=2：内核切片越界（panic → panic 处理器遏制路径）；
/// - x0=3：下一次定时器中断在 handler 上下文解引用坏地址（IRQ 遏制：
///   帧展开恢复被打断的上下文，系统继续运行）。
/// 注入生效时 x0=1/2 进程以 SIGSEGV 语义被终止不返回；x0=3 返回 0，
/// 故障在下个 tick（10ms 内）发生。非 root 或未知模式返回错误码。
/// crashme kfault / kpanic / kirq 经此验证「报错不死机」。
fn sys_kunx_fault_inject(f: &mut TrapFrame) -> i64 {
    if process::current_uid() != 0 {
        return EPERM;
    }
    match f.x[10] {
        1 => {
            let bad = if f.x[11] != 0 { f.x[11] as usize } else { 0x1 };
            // read_volatile 防优化；S 态无 SUM 放行，用户地址/未映射地址
            // 直接触发缺页异常，由 trap_dispatch → kernel_fault_contain 遏制
            let v = unsafe { core::ptr::read_volatile(bad as *const u64) };
            v as i64 // 地址意外可映射时才会到达（注入未生效）
        }
        2 => {
            let probe = [0u8; 4];
            // 索引经 volatile 读获得：躲开 unconditional_panic 的编译期
            // 判定（panic 本身就是注入目标，运行时触发走遏制路径）
            let idx = unsafe { core::ptr::read_volatile(&0x1000usize) };
            probe[idx] as i64 // 越界 → panic → panic 处理器遏制
        }
        3 => {
            crate::timer::INJECT_IRQ_FAULT.store(true, core::sync::atomic::Ordering::Relaxed);
            0
        }
        // 主核楔死注入（crashme kwedge）：本核关中断 + wfi 永眠——存活核
        // 的互监看门狗应在 5 秒内 SBI 冷复位（QEMU -no-reboot 下模拟器
        // 退出）。端到端验证「崩了 → 自动重启 → （下次启动脏标志告知）」。
        4 => {
            crate::health::report_irq(
                true,
                crate::health::SEV_INFO,
                "watchdog",
                "故障注入：本核心即将楔死（验证看门狗）",
                "预期 5 秒内系统自动重启",
            );
            unsafe {
                core::arch::asm!("csrci sstatus, 2", options(nostack));
            }
            loop {
                unsafe { core::arch::asm!("wfi", options(nostack)) };
            }
        }
        _ => EINVAL,
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
unsafe fn write_stat(stat_ptr: u64, t: u16, size: u64) -> bool {
    if !user_ok_writable(stat_ptr, 128) {
        return false;
    }
    crate::mmu::uaccess_begin();
    let p = stat_ptr as *mut u8;
    core::ptr::write_bytes(p, 0, 128);
    *(p as *mut u64) = 0; // st_dev
    *((p as *mut u64).add(1)) = 1; // st_ino
    *((p as *mut u32).add(4)) = s_ifmt(t) | if t == fs_simple::T_DIR { 0o755 } else { 0o644 }; // st_mode
    *((p as *mut u32).add(5)) = if t == fs_simple::T_DIR { 2 } else { 1 }; // st_nlink
    *((p as *mut i64).add(6)) = size as i64; // st_size @48
    *((p as *mut i32).add(14)) = 4096; // st_blksize @56
    *((p as *mut i64).add(8)) = (size as i64 + 511) / 512; // st_blocks @64
    crate::mmu::uaccess_end();
    true
}

fn sys_openat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let flags = f.x[12] as u32;
    let _dirfd = f.x[10] as i64; // 单根文件系统：忽略 dirfd（含 AT_FDCWD）
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    // 翻译访问模式与创建/截断标志；O_NONBLOCK 等其余标志透传给 VFS
    let acc = flags & 0x3;
    let creat = flags & O_CREAT;
    let trunc = flags & O_TRUNC;
    // O_APPEND 必须透传给 VFS（vfs::write 依赖 OpenFile.flags 判定追加，
    // 多写者信箱各自「open 时 seek 尾」会取到同一尾偏移互相覆盖）。
    // O_APPEND 必须透传给 VFS（vfs::write 依赖 OpenFile.flags 判定追加，
    // 多写者信箱各自「open 时 seek 尾」会取到同一尾偏移互相覆盖）。
    let fd = vfs::open(
        s,
        acc | creat | trunc | (flags & vfs::O_NONBLOCK) | (flags & vfs::O_APPEND),
    );
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
    let fd = f.x[10] as i32;
    match process::get_fd(fd) {
        Some(e) => match e.kind {
            FdKind::File => vfs::close(fd) as i64,
            FdKind::Pipe => {
                // 管道引用计数 -1（与 MsgQueue/Pair 一致）：此前只清 fd 表不清
                // 引用，close 读端/写端后 refs 不减 → 管道槽位永不归零释放。
                // is_write 由 fd 标志推导：EOF/EPIPE 语义按「对端 fd 全关」触发。
                let is_write = e.flags & crate::ipc::FD_WRITE != 0;
                ipc::pipe_drop_ref(e.index, is_write);
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
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let count = f.x[12] as usize;
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
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let count = f.x[12] as usize;
    if count == 0 {
        return 0;
    }
    let entry = match process::get_fd(fd) {
        Some(e) => e,
        None => return EBADF,
    };
    match entry.kind {
        FdKind::File => {
            // ★ 按块循环写（与 sys_read_at 对称）：此前 `count.min(4096)` 把
            // 大于 4KB 的写入静默截断为短写（朴素应用不重试即丢数据）。
            // 中途错误返回已完成部分（Linux 语义）。
            let mut done: i64 = 0;
            let mut off = 0usize;
            while off < count {
                let n = (count - off).min(4096);
                let mut tmp = [0u8; 4096];
                let copied = unsafe { copy_from_user(buf_ptr + off as u64, &mut tmp[..n]) };
                if copied == 0 {
                    return if done > 0 { done } else { EFAULT };
                }
                let w = vfs::write(fd, &tmp[..copied]);
                if w < 0 {
                    return if done > 0 { done } else { w as i64 };
                }
                done += w as i64;
                off += w as usize;
                if (w as usize) < copied {
                    break; // 底层短写且无错误：到此为止
                }
            }
            done
        }
        FdKind::Pipe | FdKind::Pair => {
            // 管道保持单趟 ≤4KB：管道写循环在读者不消费时会无限阻塞，
            // 且 POSIX 允许管道短写。
            let n = count.min(4096);
            let mut tmp = [0u8; 4096];
            let copied = unsafe { copy_from_user(buf_ptr, &mut tmp[..n]) };
            if copied == 0 {
                return EFAULT;
            }
            if entry.kind == FdKind::Pipe {
                ipc::pipe_write(fd, &tmp[..copied]) as i64
            } else {
                ipc::pair_write(fd, &tmp[..copied]) as i64
            }
        }
        _ => EBADF,
    }
}

/// iovec：`{ base: u64, len: u64 }`。
fn sys_writev(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let iov = f.x[11];
    let iovcnt = f.x[12] as usize;
    if iovcnt == 0 || iovcnt > 64 {
        return EINVAL;
    }
    if !user_ok(iov, iovcnt * 16) {
        return EFAULT;
    }
    // 第一遍：预校验全部 iovec 的 base/len 均合法（用户空间、已映射）。
    // 全程 uaccess 放行读 iovec 数组（此前逐项边写边查，读到第 2 个非法
    // iovec 时第 1 个已写入文件——局部写入；Linux 语义应整体返回 EFAULT）。
    unsafe {
        crate::mmu::uaccess_begin();
    }
    let mut all_ok = true;
    for i in 0..iovcnt {
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        // 预校验范围钳到实际拷贝量（下方拷贝每 iovec 只用 len.min(4096)）：
        // 对用户给的完整 len 逐页 ensure 会强制落地/清零至多 ~256MB 预留页
        // ——单条 syscall 打穿物理内存 + 数十亿次映射记录扫描。
        if len == 0 {
            continue; // 零长度 iov 项合法（base 可为 NULL）：按 POSIX 忽略
        }
        if !user_ok(base, len.min(4096)) {
            all_ok = false;
            break;
        }
    }
    unsafe {
        crate::mmu::uaccess_end();
    }
    if !all_ok {
        return EFAULT;
    }
    let mut total = 0i64;
    for i in 0..iovcnt {
        // iovec 数组在用户内存：PAN 下需 uaccess 放行
        unsafe { crate::mmu::uaccess_begin() };
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        unsafe { crate::mmu::uaccess_end() };
        let n = len.min(4096);
        if n == 0 {
            continue; // 零长度 iov 项：跳过而非 break（否则 musl 无缓冲流死循环）
        }
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
    let fd = f.x[10] as i32;
    let iov = f.x[11];
    let iovcnt = f.x[12] as usize;
    if iovcnt == 0 || iovcnt > 64 {
        return EINVAL;
    }
    if !user_ok(iov, iovcnt * 16) {
        return EFAULT;
    }
    // 第一遍：预校验全部 iovec 的 base/len **可写**（用户空间、已映射、COW
    // 已 break）。此前边读边拷，读到第 2 个非法 iovec 时第 1 个已写脏用户缓冲
    // ——部分完成反而掩盖错误；Linux 语义应整体返回 EFAULT。
    unsafe {
        crate::mmu::uaccess_begin();
    }
    let mut all_ok = true;
    for i in 0..iovcnt {
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        // 预校验范围钳到实际拷贝量（同 writev：每 iovec 只用 len.min(4096)），
        // 防止单条 syscall 对完整 len 逐页分配/清零预留页打穿物理内存。
        if !user_ok_writable(base, len.min(4096)) {
            all_ok = false;
            break;
        }
    }
    unsafe {
        crate::mmu::uaccess_end();
    }
    if !all_ok {
        return EFAULT;
    }
    let mut total = 0i64;
    for i in 0..iovcnt {
        // iovec 数组在用户内存：PAN 下需 uaccess 放行
        unsafe { crate::mmu::uaccess_begin() };
        let base = unsafe { *(iov as *const u64).add(i * 2) };
        let len = unsafe { *(iov as *const u64).add(i * 2 + 1) } as usize;
        unsafe { crate::mmu::uaccess_end() };
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
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let count = f.x[12] as usize;
    let offset = f.x[13] as i64;
    // ★ 负偏移防护：此前负 offset 直接当 SEEK_SET 绝对位置使用（语义错乱），
    // 且 seek 失败返回值被吞掉——非法偏移仍在错误位置读写。现拒绝负偏移并对
    // seek 失败上抛（不移动文件位置）。
    if offset < 0 {
        return EINVAL;
    }
    let cur = vfs::seek(fd, 0, vfs::SEEK_CUR);
    let s = vfs::seek(fd, offset, vfs::SEEK_SET);
    if s < 0 {
        return s;
    }
    let r = sys_read_at(fd, buf_ptr, count);
    if cur >= 0 {
        vfs::seek(fd, cur, vfs::SEEK_SET);
    }
    r
}

fn sys_pwrite64(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let count = f.x[12] as usize;
    let offset = f.x[13] as i64;
    // ★ 负偏移防护（与 pread64 同源修复）：此前负 offset 被直接当 SEEK_SET 绝对
    // 位置使用、seek 失败返回值被吞——非法偏移下仍在错位读写。
    if offset < 0 {
        return EINVAL;
    }
    let cur = vfs::seek(fd, 0, vfs::SEEK_CUR);
    let s = vfs::seek(fd, offset, vfs::SEEK_SET);
    if s < 0 {
        return s;
    }
    let r = sys_write_at(fd, buf_ptr, count);
    if cur >= 0 {
        let _ = vfs::seek(fd, cur, vfs::SEEK_SET);
    }
    r
}

/// ★ 按块循环完成读：此前 `count.min(4096)` 把大于 4KB 的读取静默截断（写 100KB
/// 文件只写了 ~4KB 却返回成功——朴素应用丢失数据）。改为每次最多 4KB、循环直到
/// 读完/EOF，累计返回实际字节数；中途错误返回已完成部分（Linux 语义）。
///
fn sys_read_at(fd: i32, buf_ptr: u64, count: usize) -> i64 {
    if count == 0 {
        return 0;
    }
    let mut done: i64 = 0;
    let mut off = 0usize;
    while off < count {
        let n = (count - off).min(4096);
        let mut tmp = [0u8; 4096];
        let entry = match process::get_fd(fd) {
            Some(e) => e,
            None => return if done > 0 { done } else { EBADF },
        };
        let r = match entry.kind {
            FdKind::File => vfs::read(fd, &mut tmp[..n]),
            _ => return if done > 0 { done } else { EBADF },
        };
        if r < 0 {
            return if done > 0 { done } else { r as i64 };
        }
        if r == 0 {
            break; // 读至文件末尾（EOF）
        }
        let copied = unsafe { copy_to_user(buf_ptr + off as u64, &tmp[..r as usize]) };
        if copied != r as usize {
            return if done > 0 { done } else { EFAULT };
        }
        done += r as i64;
        off += r as usize;
    }
    done
}

fn sys_write_at(fd: i32, buf_ptr: u64, count: usize) -> i64 {
    if count == 0 {
        return 0;
    }
    // ★ 按块循环写（与 sys_read_at 对称）：此前大于 4KB 的 pwrite 静默截断。
    let mut done: i64 = 0;
    let mut off = 0usize;
    while off < count {
        let n = (count - off).min(4096);
        let mut tmp = [0u8; 4096];
        let copied = unsafe { copy_from_user(buf_ptr + off as u64, &mut tmp[..n]) };
        if copied == 0 {
            return if done > 0 { done } else { EFAULT };
        }
        let entry = match process::get_fd(fd) {
            Some(e) => e,
            None => return if done > 0 { done } else { EBADF },
        };
        let w = match entry.kind {
            FdKind::File => vfs::write(fd, &tmp[..copied]),
            _ => return if done > 0 { done } else { EBADF },
        };
        if w < 0 {
            return if done > 0 { done } else { w as i64 };
        }
        done += w as i64;
        off += w as usize;
        if (w as usize) < copied {
            break;
        }
    }
    done
}

fn sys_lseek(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let offset = f.x[11] as i64; // 64 位偏移，不再截断为 i32
    let whence = f.x[12] as u32;
    vfs::seek(fd, offset, whence)
}

fn sys_pipe2(f: &mut TrapFrame) -> i64 {
    let fds_ptr = f.x[10];
    if !user_ok_writable(fds_ptr, 8) {
        return EFAULT;
    }
    let (r, w) = ipc::pipe_create();
    if r < 0 || w < 0 {
        // 池满/fd 表满：此前不检查、把两个 -1 写给用户还返回成功，应用拿
        // fd=-1 后续 read/close 全乱。与 sys_socketpair 同款处理。
        return EMFILE;
    }
    // PAN 放行：写入用户 fds[2]（此前只校验后裸写，PAN 开启时挂死内核）
    unsafe {
        crate::mmu::uaccess_begin();
        *(fds_ptr as *mut i32) = r;
        *(fds_ptr as *mut i32).add(1) = w;
        crate::mmu::uaccess_end();
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
                // 撤销刚加的一次 fd 引用（绝不能走 shm_close_index——它会清光
                // 本进程的映射记录但不 unmap，活着回滚 = 记录没了 PTE 还在）。
                ipc::shm_drop_one_ref(e.index);
                return EMFILE;
            }
            f
        }
        FdKind::Pipe | FdKind::MsgQueue | FdKind::Pair => {
            // ★ 管道/队列/套接字对按进程持有引用计数：复制（dup/fcntl F_DUPFD）
            // 时引用 +1（此前只复制槽位不计数——关闭任一副本后 refs 提前归零，
            // 另一副本成了悬空引用，槽位被整槽清空）。alloc_fd 失败时回滚 +1。
            match e.kind {
                FdKind::Pipe => ipc::pipe_add_ref(e.index, e.flags & ipc::FD_WRITE != 0),
                FdKind::MsgQueue => ipc::msgq_add_ref(e.index),
                FdKind::Pair => ipc::pair_add_ref(e.index, e.flags as u32),
                _ => {}
            }
            let f = process::alloc_fd(e.kind, e.index, e.flags);
            if f < 0 {
                match e.kind {
                    FdKind::Pipe => ipc::pipe_drop_ref(e.index, e.flags & ipc::FD_WRITE != 0),
                    FdKind::MsgQueue => ipc::msgq_drop_ref(e.index),
                    FdKind::Pair => ipc::pair_drop_ref(e.index, e.flags as u32),
                    _ => {}
                }
                return EMFILE;
            }
            f
        }
        FdKind::None => return EBADF,
    };
    if newfd >= 0 && nfd != newfd {
        // dup3 固定目标：把表项写入 newfd，并清空临时分配到的 nfd 槽位
        // （对象引用计数不受影响，因为表项只是“搬家”）。
        if process::set_fd(newfd as usize, process::FdEntry { kind: e.kind, index: e.index, flags: e.flags }) < 0 {
            // 目标不可写（超界）：回滚临时槽位与刚 +1 的对象引用
            process::close_fd(nfd);
            match e.kind {
                FdKind::File => vfs::close_ref(e.index),
                FdKind::Shm => ipc::shm_drop_one_ref(e.index),
                FdKind::Pipe => ipc::pipe_drop_ref(e.index, e.flags & ipc::FD_WRITE != 0),
                FdKind::MsgQueue => ipc::msgq_drop_ref(e.index),
                FdKind::Pair => ipc::pair_drop_ref(e.index, e.flags as u32),
                FdKind::None => {}
            }
            return EBADF;
        }
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
                // 与 sys_close 一致：管道引用计数 -1（此前漏减，槽位永不释放）
                ipc::pipe_drop_ref(e.index, e.flags & crate::ipc::FD_WRITE != 0);
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
    let oldfd = f.x[10] as i32;
    dup_fd_common(oldfd, -1)
}

/// `dup3(oldfd, newfd, flags)`：复制到指定描述符（flags 本内核恒等，忽略）。
fn sys_dup3(f: &mut TrapFrame) -> i64 {
    let oldfd = f.x[10] as i32;
    let newfd = f.x[11] as i32;
    if newfd < 0 {
        return EINVAL;
    }
    // 目标超出 fd 表上界：必须提前拒绝，否则下方 set_fd 失败被忽略后，
    // 已 +1 的对象引用永久泄漏（每次调用漏一个槽位引用直至池耗尽）。
    if newfd as usize >= process::FD_MAX {
        return EBADF;
    }
    if oldfd == newfd {
        // Linux dup3：oldfd == newfd 且未置 O_CLOEXEC 时成功（本内核无
        // CLOEXEC 语义）—— 前提是 oldfd 有效，无效描述符返回 EBADF。
        if process::get_fd(oldfd).is_none() {
            return EBADF;
        }
        return newfd as i64;
    }
    dup_fd_common(oldfd, newfd)
}

/// `fcntl`：仅支持 F_DUPFD / F_GETFD / F_SETFD / F_GETFL / F_SETFL（其余忽略）。
fn sys_fcntl(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let cmd = f.x[11] as i32;
    let _arg = f.x[12];
    match cmd {
        // F_DUPFD = 0：复制 fd（分配最小可用描述符），语义与 dup 一致
        0 => dup_fd_common(fd, -1),
        // F_GETFD/F_GETFL/F_SETFD/F_SETFL 等：无状态，返回 0
        _ => 0,
    }
}

/// `ioctl`：终端相关请求，以及字符设备（如 evdev）的 ioctl。
fn sys_ioctl(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let request = f.x[11] as u32;
    let arg = f.x[12];

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
                    crate::mmu::uaccess_begin();
                    *((arg as *mut u16)) = ws[0];
                    *((arg as *mut u16).add(1)) = ws[1];
                    *((arg as *mut u16).add(2)) = ws[2];
                    *((arg as *mut u16).add(3)) = ws[3];
                    crate::mmu::uaccess_end();
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
        // FBIO_PUT_DAMAGE / FBIO_PRESENT：应用直绘脏区域提交（读用户缓冲区）。
        // 缓冲区布局：[count:u32][rect × count]，rect = {x:i32, y:i32, w:u32, h:u32}。
        devfs::FBIO_PUT_DAMAGE | devfs::FBIO_PRESENT => {
            if !matches!(vfs::open_rdev(fd), Some(r) if (r >> 16) == devfs::FB_MAJOR) {
                return ENOTTY;
            }
            let mut hdr = [0u8; 4];
            if unsafe { copy_from_user(arg, &mut hdr) } == 0 {
                return EFAULT;
            }
            let count = u32::from_le_bytes(hdr) as usize;
            if count == 0 {
                // count == 0：只触发呈现/登记，无矩形
                if request == devfs::FBIO_PRESENT {
                    compositor::fb_present(&[]);
                }
                return 0;
            }
            if count > compositor::MAX_DIRTY {
                return EINVAL;
            }
            let mut buf = [0u8; compositor::MAX_DIRTY * 16];
            let bytes = count * 16;
            if unsafe { copy_from_user(arg + 4, &mut buf[..bytes]) } == 0 {
                return EFAULT;
            }
            let mut rects = [compositor::Rect::empty(); compositor::MAX_DIRTY];
            for i in 0..count {
                let b = &buf[i * 16..i * 16 + 16];
                rects[i] = compositor::Rect::new(
                    i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
                    u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
                    u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
                );
            }
            if request == devfs::FBIO_PRESENT {
                compositor::fb_present(&rects[..count]);
            } else {
                compositor::fb_damage(&rects[..count]);
            }
            0
        }
        // 音频参数 ioctl：TONE / RING_START / ACQUIRE / RELEASE / SET_ROUTE
        // 的入参由用户缓冲传入（前 8 字节），先拷入再分发给 /dev/audio 驱动。
        devfs::AUDIO_IOCTL_TONE
        | devfs::AUDIO_IOCTL_RING_START
        | devfs::AUDIO_IOCTL_ACQUIRE
        | devfs::AUDIO_IOCTL_RELEASE
        | devfs::AUDIO_IOCTL_SET_ROUTE => {
            let mut out = [0u8; 256];
            if arg != 0 {
                let _ = unsafe { copy_from_user(arg, &mut out[..8]) };
            }
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
/// PM 冻结/限速的目标权限：与 sys_kill 同级 —— 仅 root、目标自身或目标的
/// 父进程。此前无任何检查：uid 10000+ 的任意应用可把 launcher/系统服务
/// 限到 1%（或传 cap=0 软冻结桌面）。
fn pm_target_allowed(pid: u32) -> bool {
    process::current_uid() == 0
        || pid == process::current_pid()
        || process::parent_of(pid) == process::current_pid() as i32
}

fn sys_getdents64(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let dirp = f.x[11];
    let count = f.x[12] as usize;
    if count == 0 {
        return EINVAL;
    }
    // 每条目录项至多 19+32+1 → 对齐后 56 字节，单批 4096 已远超一页目录
    // 的项数。校验与写入都按钳制后的 count 走（同 readv/writev：对用户给的
    // 完整 count 逐页 ensure 会强制落地/清零至多 ~256MB 预留页——单条
    // syscall 打穿物理内存 + 数十亿次映射记录扫描）。调用方按 POSIX 语义
    // 循环调用直到返回 0，单批少返回不影响正确性。
    let count = count.min(4096);
    if !user_ok_writable(dirp, count) {
        return EFAULT;
    }
    let mut name = [0u8; 32];
    let mut written = 0usize;
    let mut overflow = false;
    loop {
        let (ino, typ) = vfs::readdir_next(fd, &mut name);
        if ino < 0 {
            break;
        }
        let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        // d_reclen = align(19 + namelen + 1, 8)
        let reclen = (19 + elen + 1 + 7) & !7usize;
        if written + reclen > count {
            // 放不下：把刚被 readdir_next 消费的条目回退，下次调用重新
            // 返回（此前直接 break —— 缓冲在两条记录中间装满时该条目被
            // 静默跳过，目录枚举从此丢项）。
            vfs::readdir_back(fd);
            overflow = true;
            break;
        }
        unsafe {
            crate::mmu::uaccess_begin();
            let p = (dirp + written as u64) as *mut u8;
            *(p as *mut u64) = ino as u64; // d_ino
            *((p as *mut u64).add(1)) = 0; // d_off
            *((p as *mut u16).add(8)) = reclen as u16; // d_reclen
            *((p as *mut u8).add(18)) = dt_of(typ); // d_type
            core::ptr::copy_nonoverlapping(name.as_ptr(), (p as *mut u8).add(19), elen);
            *((p as *mut u8).add(19 + elen)) = 0; // d_name 结尾 NUL
            crate::mmu::uaccess_end();
        }
        written += reclen;
    }
    if written == 0 && overflow {
        // 缓冲连一条记录都装不下：POSIX 语义返回 EINVAL 而非 0（返回 0
        // 会被调用方当成「目录结束」提前终止枚举）。
        return EINVAL;
    }
    written as i64
}

fn sys_getcwd(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    let size = f.x[11] as usize;
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
        crate::mmu::uaccess_begin();
        *((buf + len as u64) as *mut u8) = 0;
        crate::mmu::uaccess_end();
    }
    buf as i64
}

fn sys_chdir(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[10];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
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
    let path_ptr = f.x[11];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    vfs::mkdir(s) as i64
}

fn sys_unlinkat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    vfs::unlink(s) as i64
}

fn sys_mknodat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let mode = f.x[12] as u32;
    let dev = f.x[13] as u32;
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
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
///
/// POSIX 覆盖语义：目标已存在且为普通文件/符号链接时原子替换（SFS 单日志
/// 事务）。AArch64 asm-generic 无独立 `rename` 号（134 是 rt_sigaction，已被
/// 占用）；`rename` 语义经由本调用（38）与 AT_FDCWD 组合提供。
fn sys_renameat(f: &mut TrapFrame) -> i64 {
    let mut old = [0u8; 128];
    let mut new = [0u8; 128];
    let no = unsafe { read_cstr(f.x[11], &mut old) };
    let nn = unsafe { read_cstr(f.x[13], &mut new) };
    if no == 0 || nn == 0 {
        return EFAULT;
    }
    let mut abspath_o = [0u8; 128];
    let mut abspath_n = [0u8; 128];
    let po = core::str::from_utf8(&old[..no]).unwrap_or("");
    let pn = core::str::from_utf8(&new[..nn]).unwrap_or("");
    let Some(o) = resolve_user_path(po, &mut abspath_o) else { return ENAMETOOLONG; };
    let Some(n) = resolve_user_path(pn, &mut abspath_n) else { return ENAMETOOLONG; };
    // 源必须存在：resolve_parent 阶段不报错，此处区分 ENOENT 与其他错误
    let mut dev = 0u32;
    let mut ino = 0u32;
    if vfs::resolve(o, &mut dev, &mut ino) != 0 {
        return ENOENT;
    }
    match vfs::rename(o, n) {
        0 => 0,
        -18 => -18, // EXDEV：跨文件系统
        -16 => EBUSY, // 目标 inode 仍被打开
        _ => EEXIST,
    }
}

fn sys_faccessat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
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
    let path_ptr = f.x[11];
    let buf_ptr = f.x[12];
    let bufsiz = f.x[13] as usize;
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
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
        crate::mmu::uaccess_begin();
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf_ptr as *mut u8, r as usize);
        crate::mmu::uaccess_end();
    }
    r as i64
}

/// `symlinkat(target, newdirfd, linkpath)`：创建符号链接（单根文件系统忽略 dirfd）。
fn sys_symlinkat(f: &mut TrapFrame) -> i64 {
    let target_ptr = f.x[10];
    let link_ptr = f.x[12];
    let mut target = [0u8; 256];
    let mut link = [0u8; 128];
    let tn = unsafe { read_cstr(target_ptr, &mut target) };
    let ln = unsafe { read_cstr(link_ptr, &mut link) };
    if tn == 0 || ln == 0 {
        return EFAULT;
    }
    let t = core::str::from_utf8(&target[..tn]).unwrap_or("");
    let mut abspath = [0u8; 128];
    let Some(l) = resolve_user_path(core::str::from_utf8(&link[..ln]).unwrap_or(""), &mut abspath) else { return ENAMETOOLONG; };
    vfs::symlink(t, l) as i64
}

fn sys_newfstatat(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let stat_ptr = f.x[12];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
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
    let fd = f.x[10] as i32;
    let stat_ptr = f.x[11];
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
    let fd = f.x[10] as i32;
    let length = f.x[11] as i64;
    if length != 0 {
        return EINVAL;
    }
    vfs::truncate(fd) as i64
}

/// `truncate(path, length)`：按路径截断；本内核仅支持截断到 0。
fn sys_truncate(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[10];
    let length = f.x[11] as i64;
    if length != 0 {
        return EINVAL;
    }
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    // 打开（O_WRONLY|O_CREAT|O_TRUNC：truncate 语义会创建不存在的文件）
    let fd = vfs::open(s, 1 | O_CREAT | O_TRUNC);
    if fd < 0 {
        return ENOENT;
    }
    let r = vfs::truncate(fd);
    vfs::close(fd);
    r as i64
}

/// `statfs(path, buf)`：查询 `path` 所在文件系统的容量信息。
///
/// AArch64 `struct statfs` 为 120 字节：七个 i64 + fsid(i64) + namelen(i64) +
/// frsize(i64) + spare[5](40B)。只填写块尺寸/总数/空闲等有意义的字段，
/// 其余清零。
fn sys_statfs(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[10];
    let buf_ptr = f.x[11];
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    if !user_ok_writable(buf_ptr, 120) {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    let mut dev = 0u32;
    let mut ino = 0u32;
    if vfs::resolve(s, &mut dev, &mut ino) != 0 {
        return ENOENT;
    }
    let bsize = crate::fsops::fs_block_size(dev) as i64;
    let total = crate::fsops::fs_total_blocks(dev) as i64;
    let free = crate::fsops::fs_free_blocks(dev) as i64;

    unsafe {
        crate::mmu::uaccess_begin();
    }
    let p = buf_ptr as *mut u8;
    unsafe {
        core::ptr::write_bytes(p, 0, 120);
        *((p as *mut u64).add(1)) = bsize as u64; // f_bsize @8
        *((p as *mut u64).add(2)) = total as u64; // f_blocks @16
        *((p as *mut u64).add(3)) = free as u64; // f_bfree @24
        *((p as *mut u64).add(4)) = free as u64; // f_bavail @32
        *((p as *mut u64).add(8)) = 255; // f_namelen @64
        *((p as *mut u64).add(9)) = bsize as u64; // f_frsize @72
        crate::mmu::uaccess_end();
    }
    0
}

/// `socketpair(domain, type, protocol, sv)`：创建双向字节流套接字对。
/// 仅支持 `AF_UNIX, SOCK_STREAM`；返回两个 fd 写入 `sv[2]`。
fn sys_socketpair(f: &mut TrapFrame) -> i64 {
    let domain = f.x[10] as i32;
    let ty = f.x[11] as i32;
    let sv = f.x[13];
    if domain != AF_UNIX {
        return EAFNOSUPPORT;
    }
    if ty != SOCK_STREAM {
        return EPROTONOSUPPORT;
    }
    // 写入目标按"可写"校验：user_ok 只保证已映射，RO 页/COW 未断页在
    // EL1 裸写会触发权限故障 panic
    if !user_ok_writable(sv, 8) {
        return EFAULT;
    }
    let (fd0, fd1) = ipc::socketpair_create();
    if fd0 < 0 || fd1 < 0 {
        return EMFILE;
    }
    // PAN 放行：写入用户 sv[2]（此前只校验后裸写，PAN 开启时挂死内核）
    unsafe {
        crate::mmu::uaccess_begin();
        *(sv as *mut i32) = fd0;
        *(sv as *mut i32).add(1) = fd1;
        crate::mmu::uaccess_end();
    }
    0
}

/// `reboot(cmd, ...)`：Linux 语义的电源管理入口。
/// 支持 LINUX_REBOOT_CMD_POWER_OFF（0x4321FEDC）→ PSCI SYSTEM_OFF、
/// LINUX_REBOOT_CMD_RESTART（0x1234567）→ PSCI SYSTEM_RESET；
/// 其它命令返回 EINVAL。QEMU `-no-reboot` 下关机即退出模拟器。
fn sys_reboot(f: &mut TrapFrame) -> i64 {
    let cmd = f.x[12] as u32;
    match cmd {
        0x4321_FEDC => {
            crate::uart_send_str("[kernel] 通过 reboot(2) 关机\n");
            // 正常关机：清脏标志，下次启动不误报「异常关机」
            let _ = vfs::unlink("/data/.bootdirty");
            crate::sbi::shutdown();
        }
        0x1234_567 => {
            crate::uart_send_str("[kernel] 通过 reboot(2) 重启\n");
            let _ = vfs::unlink("/data/.bootdirty");
            crate::sbi::system_reset();
        }
        _ => EINVAL,
    }
}

/// 内核全局剪贴板（256 字节）。`sys_clipboard_set(buf, len)`：写入；
/// `sys_clipboard_get(buf, cap)`：读出，返回长度（-1 空）。
const CLIPBOARD_CAP: usize = 256;
static mut CLIPBOARD: [u8; CLIPBOARD_CAP] = [0; CLIPBOARD_CAP];
static mut CLIPBOARD_LEN: usize = 0;

fn sys_clipboard_set(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    let len = f.x[11] as usize;
    if len == 0 || len > CLIPBOARD_CAP {
        return EINVAL;
    }
    if !user_ok(buf, len) {
        return EFAULT;
    }
    unsafe {
        crate::mmu::uaccess_begin();
        core::ptr::copy_nonoverlapping(buf as *const u8, CLIPBOARD.as_mut_ptr(), len);
        crate::mmu::uaccess_end();
        CLIPBOARD_LEN = len;
    }
    len as i64
}

fn sys_clipboard_get(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    let cap = f.x[11] as usize;
    let len = unsafe { CLIPBOARD_LEN };
    if len == 0 {
        return 0; // 空剪贴板
    }
    let n = len.min(cap);
    if !user_ok_writable(buf, n) {
        return EFAULT;
    }
    unsafe {
        crate::mmu::uaccess_begin();
        core::ptr::copy_nonoverlapping(CLIPBOARD.as_ptr(), buf as *mut u8, n);
        crate::mmu::uaccess_end();
    }
    n as i64
}

/// 从用户空间读取 NUL 结尾字符串（有界，最大 255 字节）。返回静态拷贝。
fn copy_user_string(ptr: usize, max: usize) -> Option<[u8; 256]> {
    if ptr == 0 || max > 255 {
        return None;
    }
    if !user_ok(ptr as u64, max + 1) {
        return None;
    }
    let mut buf = [0u8; 256];
    unsafe {
        crate::mmu::uaccess_begin();
        for i in 0..max {
            let c = core::ptr::read_volatile((ptr + i) as *const u8);
            buf[i] = c;
            if c == 0 {
                crate::mmu::uaccess_end();
                return Some(buf);
            }
        }
        crate::mmu::uaccess_end();
    }
    None // 未遇到 NUL → 拒绝（防超长路径）
}

/// `kunx_ota(cmd, arg1, arg2)`：用户态触发内核 OTA 服务（恢复菜单 / otad）。
///
/// cmd: 1=安装(path) 2=检查(path) 3=状态 5=设置恢复 6=强制切槽(char)
fn sys_kunx_ota(f: &mut TrapFrame) -> i64 {
    // 权限门禁：INSTALL/WIPE/槽位切换会擦写数据或切启动槽，属于特权操作。
    // 此前任何应用都能 wipe /data 或从任意路径刷写 —— 按 setuid/kill 的
    // 同一隔离模型仅允许 root（uid 0）。STATUS/CHECK 只读，保持开放。
    let mutating = matches!(
        f.x[10],
        OTA_CMD_INSTALL | OTA_CMD_SET_RECOVERY | OTA_CMD_FORCE_SLOT | OTA_CMD_WIPE_DATA
    );
    if mutating && process::current_uid() != 0 {
        return EPERM;
    }
    match f.x[10] {
        OTA_CMD_INSTALL | OTA_CMD_CHECK => {
            let path = match copy_user_string(f.x[11] as usize, 255) {
                Some(p) => p,
                None => return EFAULT,
            };
            let end = path.iter().position(|&c| c == 0).unwrap_or(0);
            if end == 0 {
                return EINVAL;
            }
            let s = match core::str::from_utf8(&path[..end]) {
                Ok(s) => s,
                Err(_) => return EINVAL,
            };
            let r = if f.x[10] == OTA_CMD_INSTALL {
                crate::ota::ota_install(s)
            } else {
                crate::ota::ota_check(s)
            };
            r as i64
        }
        OTA_CMD_STATUS => {
            crate::ota::print_status();
            0
        }
        OTA_CMD_SET_RECOVERY => {
            if crate::ota::ota_set_recovery() {
                0
            } else {
                -1
            }
        }
        OTA_CMD_FORCE_SLOT => {
            let c = (f.x[11] & 0xFF) as u8;
            if crate::ota::ota_force_slot(c) {
                0
            } else {
                EINVAL
            }
        }
        OTA_CMD_WIPE_DATA => crate::ota::wipe_data() as i64,
        _ => EINVAL,
    }
}

/// `dns_resolve(name)`：解析域名（NUL 结尾字符串），返回主机字节序 IPv4；
/// 0 表示失败（未配置 DNS / 解析超时）。
fn sys_dns_resolve(f: &mut TrapFrame) -> i64 {
    let ptr = f.x[10];
    if ptr == 0 || ptr >= USER_TOP {
        return 0;
    }
    // 先读前 255 字节找 NUL（域名最长 253 字符，留余量）
    let mut buf = [0u8; 256];
    let mut n = 0usize;
    unsafe { crate::mmu::uaccess_begin() };
    while n < buf.len() {
        if !user_ok(ptr + n as u64, 1) {
            unsafe { crate::mmu::uaccess_end() };
            return 0;
        }
        let b = unsafe { *(ptr as *const u8).add(n) };
        buf[n] = b;
        n += 1;
        if b == 0 {
            break;
        }
    }
    unsafe { crate::mmu::uaccess_end() };
    if n == 0 || buf[0] == 0 {
        return 0;
    }
    let name = core::str::from_utf8(&buf[..n - 1]).unwrap_or("");
    match crate::net_stack::dns_resolve(name) {
        Some(ip) => ip as i64,
        None => 0,
    }
}

/// `statx(dirfd, path, flags, mask, buf)`：以 statx 布局返回文件信息。
/// 内核无完整 statx；填充基础字段（类型/大小），其余置 0。
fn sys_statx(f: &mut TrapFrame) -> i64 {
    let path_ptr = f.x[11];
    let buf = f.x[14];
    if !user_ok_writable(buf, 256) {
        return EFAULT;
    }
    let mut path = [0u8; 128];
    let n = unsafe { read_cstr(path_ptr, &mut path) };
    if n == 0 {
        return EFAULT;
    }
    let mut abspath = [0u8; 128];
    let p = core::str::from_utf8(&path[..n]).unwrap_or("");
    let Some(s) = resolve_user_path(p, &mut abspath) else { return ENAMETOOLONG; };
    let info = match vfs::stat_path(s) {
        Some(x) => x,
        None => return ENOENT,
    };
    unsafe {
        crate::mmu::uaccess_begin();
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
        crate::mmu::uaccess_end();
    }
    0
}

// ==================== 进程 / 线程 ====================

fn sys_exit(f: &mut TrapFrame) -> i64 {
    process::exit(f.x[10] as i32)
}

/// exit(93)：仅退出当前线程（最后一个线程自动升级为整进程退出）。
fn sys_exit_thread(f: &mut TrapFrame) -> i64 {
    process::exit_thread(f.x[10] as i32)
}

fn sys_wait4(f: &mut TrapFrame) -> i64 {
    let pid = f.x[10] as i32;
    let status_ptr = f.x[11];
    // Linux ABI：wait4(pid, status, options, rusage) —— options 在 x2，
    // rusage 指针在 x3。此前误读 x3，把 rusage 指针当 options（真实
    // bionic/glibc 调用 WNOHANG 会退化为阻塞等待）。
    let options = f.x[12] as u32;
    // WNOHANG（1）：无已退出子进程时立即返回 0，不阻塞。
    // 供 launcher 非阻塞轮询应用子进程（应用可能处于 SIGSTOP 后台态，
    // wait 只对 Zombie 返回，停止中的子进程不会被视为"已退出"）。
    const WNOHANG: u32 = 0x1;
    if options & WNOHANG != 0 && !process::has_zombie_child(pid) {
        return 0;
    }
    let (rpid, code) = process::wait(pid);
    if rpid < 0 {
        return ECHILD;
    }
    if status_ptr != 0 {
        if !user_ok_writable(status_ptr, 4) {
            return EFAULT;
        }
        // PAN 放行（此前只校验后裸写）
        // 编码（L-5）：code >= 0 走正常退出（exit code << 8）；code < 0 是
        // 遏制/信号击杀（如 process::exit(-11)），按 Linux WIFSIGNALED 语义
        // 写低 7 位信号号 + bit7（kunxinit 的 sig = status & 0x7F 从此可见，
        // 「被信号终止」日志不再是死代码）。
        let status = if code < 0 {
            (((-code) as u32) & 0x7F) | 0x80
        } else {
            ((code & 0xFF) << 8) as u32
        };
        unsafe {
            write_u32_to_user(status_ptr, status);
        }
    }
    rpid as i64
}

fn sys_kill(f: &mut TrapFrame) -> i64 {
    let pid = f.x[10] as i32;
    let sig = f.x[11] as u32;
    // 信号号校验：内核仅使用 0..64 位图
    if sig >= 64 {
        return EINVAL;
    }
    // POSIX sig-0 探测（检查进程存在性，不投递信号）：位图 bit0 永不会被
    // handle_pending 派发，置位后 signal_pending 恒非零，每个 syscall 入口
    // 都要多扫 63 位 —— 直接按探测语义返回。
    if sig == 0 {
        if pid <= 0 {
            return 0;
        }
        return if process::process_exists(pid as u32) { 0 } else { ESRCH };
    }
    // pid <= 0（组/广播信号）：本内核无进程组，静默成功
    if pid <= 0 {
        return 0;
    }
    // 目标进程必须存在，否则 ESRCH
    if !process::process_exists(pid as u32) {
        return ESRCH;
    }
    // ★ 权限检查（Android 式进程隔离雏形）：仅 root（uid 0）或目标进程的
    // 父进程可以向其发信号。应用进程（launcher setuid 到 10000+）因此不能
    // SIGKILL 掉 launcher/init/系统服务（uid 0），也不能干扰其它应用
    // （非其子进程）——恶意应用无法通过 kill 破坏桌面会话。
    if process::current_uid() != 0 && process::parent_of(pid as u32) != process::current_pid() as i32 {
        return EPERM;
    }
    // SIGCONT(18)：恢复 SIGSTOP 停止的进程（生命周期恢复路径）。恢复动作
    // 在发送方完成（Stopped → Ready）；信号位仍置位，进程恢复后经
    // handle_pending 看到 SIGCONT（默认动作=继续，无副作用）。
    if sig == 18 {
        crate::sched_cfs::cont_pid(pid as u32);
    }
    // SIGKILL(9)：强制终止。停止态（SIGSTOP 后台/挂起）进程不会再次进入
    // 内核处理待处理信号，必须在此直接终止（process::terminate 负责
    // kill_pid + 标记僵尸 + 唤醒父进程/等待者）。
    if sig == 9 {
        process::terminate(pid as u32);
        return 0;
    }
    process::signal_send(pid as u32, sig);
    // 唤醒阻塞中的目标进程：其睡眠/等待系统调用返回后，下一次进入内核时
    // dispatch 检查信号位图并终止（SIGTERM 等）或继续（其它信号）。
    // 注意：`wake_pid` 只唤醒 Blocked（IPC 阻塞），不唤醒 Stopped ——
    // SIGSTOP 停止的进程只能由 SIGCONT（cont_pid）恢复。
    crate::sched_cfs::wake_pid(pid as u32);
    0
}

/// `setpriority(which, who, prio)`：调整进程调度优先级（nice）。
///
/// 支持 `which = PRIO_PROCESS(0)`：`who` 为 pid（0 = 当前进程）。
/// prio 取值范围 -20..19，映射到内核 nice（clamp -5..5，与 GUI 前台 −3 一致）。
/// 权限：root 可改任意进程；非 root 仅可改自身（与 setuid 同源规则）。
/// 供 lifd 按应用生命周期状态调整 CFS 权重（前台 −3 / 后台 +2），
/// 与 CFS 调度器集成但不替换其语义。
fn sys_setpriority(f: &mut TrapFrame) -> i64 {
    const PRIO_PROCESS: i32 = 0;
    let which = f.x[10] as i32;
    let who = f.x[11] as i32;
    let prio = f.x[12] as i32;
    if which != PRIO_PROCESS {
        return 0; // 组/用户优先级无对应机制：接受调用
    }
    let target = if who == 0 { process::current_pid() as i32 } else { who };
    if target <= 0 {
        return 0;
    }
    if !process::process_exists(target as u32) {
        return ESRCH;
    }
    if process::current_uid() != 0 && target != process::current_pid() as i32 {
        return EPERM;
    }
    // Linux prio(-20..19) → nice（-20..19）→ 内核 nice（clamp -5..5）
    let nice = prio.clamp(-20, 19) / 4;
    crate::sched_cfs::set_pid_nice(target as u32, nice);
    0
}

/// `setuid(uid)`：切换当前进程用户 ID。
///
/// POSIX 语义：仅 root 可切换到任意 uid；非 root 只能设为自身 uid（no-op）。
/// 供 launcher 在 execve 应用前调用，实现 per-app UID（Android 式沙箱地基）。
fn sys_setuid(f: &mut TrapFrame) -> i64 {
    let uid = f.x[10] as u32;
    if uid >= 65536 {
        return EINVAL;
    }
    if process::current_uid() != 0 && uid != process::current_uid() {
        return EPERM;
    }
    process::set_uid(uid);
    0
}

/// `setgid(gid)`：切换当前进程组 ID（权限规则同 setuid）。
fn sys_setgid(f: &mut TrapFrame) -> i64 {
    let gid = f.x[10] as u32;
    if gid >= 65536 {
        return EINVAL;
    }
    if process::current_gid() != 0 && gid != process::current_gid() {
        return EPERM;
    }
    process::set_gid(gid);
    0
}

/// `clone`：不带 `CLONE_THREAD` 时按 `fork` 处理（完整复制进程）；
/// 带 `CLONE_THREAD` 时创建共享地址空间的线程。
///
/// AArch64 参数：x0=flags, x1=child_stack, x2=parent_tidptr, x3=tls, x4=child_tidptr。
fn sys_clone(f: &mut TrapFrame) -> i64 {
    let flags = f.x[10];
    let child_stack = f.x[11] as usize;
    let parent_tidptr = f.x[12] as usize;
    let tls = f.x[13] as usize;
    let child_tidptr = f.x[14] as usize;

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
    let path_ptr = f.x[10];
    let argv_ptr = f.x[11];
    let envp_ptr = f.x[12];

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
    // 整个 argv/envp 指针数组与字符串读取处于 uaccess 区（PAN 放行）
    crate::mmu::uaccess_begin();
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
    crate::mmu::uaccess_end();
    n
}

/// `futex`：pthread 互斥量/条件变量的等待与唤醒。
fn sys_futex(f: &mut TrapFrame) -> i64 {
    let addr = f.x[10] as usize;
    let op = f.x[11] as u32;
    let val = f.x[12] as u32;
    let timeout = f.x[13];
    let addr2 = f.x[14] as usize;
    let val3 = f.x[15] as u32;

    let (base_op, private) = (op & 0x7F, op & FUTEX_PRIVATE_FLAG != 0);
    let _ = private;
    match base_op {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            // 超时：用户 timespec{sec,nsec} 指针 → 单调时钟截止点（µs）。
            // 此前 `let _timeout = f.x[13]` 直接忽略——futex_wait 无限阻塞，
            // 无竞争互斥量的应用（如 SDL 条件变量）永久挂起。
            let mut deadline = None;
            if timeout != 0 {
                if !user_ok(timeout, 16) {
                    return EFAULT;
                }
                unsafe {
                    crate::mmu::uaccess_begin();
                    let sec = *(timeout as *const i64);
                    let nsec = *((timeout + 8) as *const i64);
                    crate::mmu::uaccess_end();
                    deadline = Some(
                        crate::timer::now_us()
                            .saturating_add((sec.max(0) as u64).saturating_mul(1_000_000))
                            .saturating_add((nsec.max(0) as u64) / 1000),
                    );
                }
            }
            bionic_shim::futex_wait(addr, val, deadline) as i64
        }
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
    let addr = f.x[10] as usize;
    let r = if addr == 0 {
        bionic_shim::brk_get() as i64
    } else {
        bionic_shim::brk_set(addr) as i64
    };
    r
}

/// `mmap`：匿名映射（MAP_ANONYMOUS）分配新内存；`/dev/fb0` 映射到显存。
fn sys_mmap(f: &mut TrapFrame) -> i64 {
    let addr = f.x[10] as usize;
    let len = f.x[11] as usize;
    let prot = f.x[12] as u32;
    let flags = f.x[13] as u32;
    let fd = f.x[14] as i32;
    let _offset = f.x[15];

    if len == 0 {
        return EINVAL;
    }
    // ★ W^X 强化：拒绝"可写 + 可执行"与任何可执行匿名/文件映射 —— 本内核
    // 可执行代码只来自 ELF 加载（只读共享池页），运行时不再申请可执行内存，
    // 杜绝经典 RWX 堆喷射/代码注入路径（对标 iOS W^X 与 Android W^X 策略）。
    if prot & (PROT_WRITE | PROT_EXEC) == PROT_WRITE | PROT_EXEC || prot & PROT_EXEC != 0 {
        return EPERM;
    }
    if flags & MAP_ANONYMOUS == 0 {
        // 文件映射：帧缓冲设备 /dev/fb0 走显存直映；普通文件走私有页拷贝
        if let Some(rdev) = vfs::open_rdev(fd) {
            if rdev >> 16 == devfs::FB_MAJOR {
                return map_framebuffer(addr, len, flags);
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
        // MAP_FIXED_NOREPLACE 语义：目标范围已有映射时必须报 EEXIST，
        // 不得覆盖（此前与 MAP_FIXED 同等对待，调用方的"不得覆盖"契约
        // 被静默摧毁）。
        if flags & MAP_FIXED_NOREPLACE != 0
            && flags & MAP_FIXED == 0
            && vm::user_range_mapped(process::current_root(), addr, pages * vm::PAGE_SIZE)
        {
            return EEXIST;
        }
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
    let addr = f.x[10] as usize;
    let len = f.x[11] as usize;
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

/// `mprotect(addr, len, prot)`：修改 `[addr, addr+len)` 各页的访问权限。
///
/// 由原「恒返回 0 的 no-op 存根」升级为真实实现，并落实 W^X 策略：
/// - 拒绝任何"可写 + 可执行"组合（`EPERM`）；
/// - 拒绝授予 `PROT_EXEC`（本内核可执行内存仅来自 ELF 加载，W^X 前提下
///   运行时不可把数据页变为可执行，杜绝 JIT 式注入路径）；
/// - `PROT_NONE`（0）保持现状（不取消映射），返回成功；
/// - 授予写权限的 COW/共享只读页先 break 出本进程私有副本（防写穿共享页）。
fn sys_mprotect(f: &mut TrapFrame) -> i64 {
    let addr = f.x[10] as usize;
    let len = f.x[11] as usize;
    let prot = f.x[12] as u32;
    if addr % vm::PAGE_SIZE != 0 || len == 0 {
        return EINVAL;
    }
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return EINVAL;
    }
    // W^X：拒绝 W+X；拒绝任何 EXEC 请求
    if prot & (PROT_WRITE | PROT_EXEC) == PROT_WRITE | PROT_EXEC || prot & PROT_EXEC != 0 {
        return EPERM;
    }
    // PROT_NONE：保持现状（不取消映射）
    if prot == 0 {
        return 0;
    }
    let Some(end) = addr.checked_add(len) else { return EINVAL };
    if end <= addr || !vm::user_va_range_ok(addr, len) {
        return EINVAL;
    }
    // 新权限：可写 → RW；否则 → RO
    let ap = if prot & PROT_WRITE != 0 {
        vm::AP_USER_RW
    } else {
        vm::AP_USER_RO
    };
    let mut cur = addr;
    while cur < end {
        if !process::mprotect_page(cur, ap) {
            return ENOMEM;
        }
        cur += vm::PAGE_SIZE;
    }
    0
}

/// 帧缓冲映射的固定用户虚拟地址（低于匿名 mmap 游标 0x3000_0000，无冲突）。
const FB_MMAP_ADDR: usize = 0x2000_0000;

/// 普通文件 mmap 游标（每进程，从 0x2E000000 向上，低于匿名游标 0x30000000）。
static mut FILE_MMAP_NEXT: [usize; process::MAX_PROC] = [0x2E00_0000; process::MAX_PROC];

/// 进程退出/回收后复位该 pid 的文件 mmap 游标（pid 槽位复用防泄漏，
/// 与 bionic_shim::reset_brk_state 配套；游标单调增长不复位会永久 ENOMEM）。
pub fn reset_file_mmap(pid: usize) {
    if (pid as usize) < process::MAX_PROC {
        unsafe { FILE_MMAP_NEXT[pid] = 0x2E00_0000 };
    }
}

/// 文件映射（MAP_PRIVATE 语义）：读取文件内容到私有物理页并映射进用户空间。
/// 支持任意 offset/len；越界部分截断到文件末尾；文件为空或 offset 越界返回错误。
fn map_file(_f: &mut TrapFrame, len: usize, fd: i32, offset: usize) -> i64 {
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
    // 文件 mmap 游标与匿名 mmap 游标（MMAP_BASE=0x3000_0000）分别独立推进、
    // 互不感知，而 map_page_in 对已映射 VA 会静默覆盖 PTE —— 文件映射一旦
    // 越过 0x3000_0000，后续 anon mmap（malloc）会拿到同一 VA，堆数据与
    // 文件页互相踩踏。硬性把文件映射限制在匿名游标之下。
    if !vm::user_va_range_ok(va, pages * vm::PAGE_SIZE)
        || va + pages * vm::PAGE_SIZE > bionic_shim::MMAP_BASE
    {
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
                // 回滚游标：否则反复失败的 mmap 每次烧掉 pages*4KB 的
                // [0x2E000000,0x30000000) 区间，耗尽后该 pid 永久 ENOMEM。
                unsafe { FILE_MMAP_NEXT[pid as usize] = va };
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
            unsafe { FILE_MMAP_NEXT[pid as usize] = va };
            return ENOMEM;
        }
        mapped += 1;
    }
    va as i64
}

/// 把 `/dev/fb0` 的显存映射到当前进程用户地址空间，返回起始虚拟地址。
///
/// Linux 语义：`addr` 只在 MAP_FIXED/MAP_FIXED_NOREPLACE 下生效，否则仅为
/// 提示——此前无视 MAP_FIXED 把任意非 0 addr（比如调用方自己的堆地址）
/// 强制映射成显存，PTE 被静默覆盖且旧映射记录保留（记录与页表不一致）。
fn map_framebuffer(addr: usize, len: usize, flags: u32) -> i64 {
    let (base, fb_len) = devfs::fb_map_info();
    if base == 0 || fb_len == 0 {
        return ENODEV;
    }
    let map_len = len.min(fb_len);
    if map_len == 0 {
        // 冷路径诊断：launcher 偶发 mmap EINVAL 排查（BUG_REPORT 十一节未决项）
        crate::kinfo! {
            crate::uart_send_str("[fb-mmap] EINVAL len=");
            crate::print_hex(len as u64);
            crate::uart_send_str(" fb_len=");
            crate::print_hex(fb_len as u64);
            crate::uart_send_str("\n");
        }
        return EINVAL;
    }
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0
        && addr != 0
        && addr % vm::PAGE_SIZE == 0;
    let va = if fixed { addr } else { FB_MMAP_ADDR };
    if va % vm::PAGE_SIZE != 0 {
        crate::kinfo! {
            crate::uart_send_str("[fb-mmap] EINVAL 未对齐 va=");
            crate::print_hex(va as u64);
            crate::uart_send_str("\n");
        }
        return EINVAL;
    }
    let pages = map_len.div_ceil(vm::PAGE_SIZE);
    if !vm::user_va_range_ok(va, pages * vm::PAGE_SIZE) {
        crate::kinfo! {
            crate::uart_send_str("[fb-mmap] EINVAL va_range va=");
            crate::print_hex(va as u64);
            crate::uart_send_str(" pages=");
            crate::print_hex(pages as u64);
            crate::uart_send_str("\n");
        }
        return EINVAL;
    }
    if fixed {
        if flags & MAP_FIXED == 0
            && vm::user_range_mapped(process::current_root(), va, pages * vm::PAGE_SIZE)
        {
            return EEXIST;
        }
        // MAP_FIXED 语义：先释放目标范围内已有的旧映射（防页泄漏与双重映射）
        process::unmap_user_range(va, pages * vm::PAGE_SIZE);
    }
    let root = process::current_root();
    for i in 0..pages {
        // 页表 OOM：mmap 返回 -ENOMEM（此前中间级页表分配失败直接 panic）。
        if !vm::map_page_in(root, va + i * vm::PAGE_SIZE, base + i * vm::PAGE_SIZE, vm::FLAG_USER_DATA) {
            return ENOMEM;
        }
    }
    va as i64
}

// ==================== 时间 ====================

fn now_usec() -> u64 {
    // gettimeofday/clock_gettime 返回墙钟时间（有 RTC 时 = 纪元秒 + 单调时钟）
    crate::rtc::wall_time_us()
}

fn sys_gettimeofday(f: &mut TrapFrame) -> i64 {
    let tv = f.x[10];
    if tv != 0 {
        if !user_ok_writable(tv, 16) {
            return EFAULT;
        }
        let us = now_usec();
        // PAN 下 EL1 直写用户内存必须经 uaccess 放行（此前只 user_ok_writable
        // 校验后裸写——PAN 开启时写用户页触发 permission fault 挂死内核）
        unsafe {
            write_u64_to_user(tv, (us / 1_000_000) as u64);
            write_u64_to_user(tv + 8, (us % 1_000_000) as u64);
        }
    }
    0
}

fn sys_clock_gettime(f: &mut TrapFrame) -> i64 {
    let _clk = f.x[10] as i32;
    let tp = f.x[11];
    if tp != 0 {
        if !user_ok_writable(tp, 16) {
            return EFAULT;
        }
        let us = now_usec();
        // PAN 放行（同 sys_gettimeofday）
        unsafe {
            write_u64_to_user(tp, (us / 1_000_000) as u64);
            write_u64_to_user(tp + 8, ((us % 1_000_000) * 1000) as u64);
        }
    }
    0
}

fn sys_clock_getres(f: &mut TrapFrame) -> i64 {
    let tp = f.x[11];
    if tp != 0 {
        if !user_ok_writable(tp, 16) {
            return EFAULT;
        }
        // PAN 放行（同 sys_gettimeofday）
        unsafe {
            write_u64_to_user(tp, 0);
            write_u64_to_user(tp + 8, 1_000_000);
        }
    }
    0
}

/// pselect6：真实就绪检测。常规文件恒就绪；字符设备（/dev/tty）按
/// UART 待读字节判断。无就绪 fd 时按超时小睡（50ms 切片，提前返回 0
/// 合法——调用方会重巡 schedule 回调）。
fn sys_poll_family(f: &mut TrapFrame, tmo_reg: usize) -> i64 {
    let nfds = f.x[10] as usize;
    let readfds = f.x[11] as usize;
    let timeout = f.x[tmo_reg] as usize;
    let mut count = 0i64;
    if readfds != 0 && nfds > 0 {
        let nbytes = ((nfds + 7) / 8).min(128);
        if user_ok(readfds as u64, nbytes) {
            unsafe {
                let mut tmp = [0u8; 128];
                copy_from_user(readfds as u64, &mut tmp[..nbytes]);
                for fd in 0..nfds.min(1024) {
                    if tmp[fd / 8] & (1 << (fd % 8)) != 0 {
                        // 就绪判定：字符设备（tty）看 UART 待读；其余（常规文件等）恒就绪
                        let ready = match process::get_fd(fd as i32) {
                            Some(_) => match vfs::fstat(fd as i32) {
                                Some((t, _)) if t == crate::fs_simple::T_CHR => {
                                    crate::uart_rx_pending()
                                }
                                Some(_) => true,
                                None => false,
                            },
                            None => false,
                        };
                        if ready {
                            count += 1;
                        } else {
                            // 清掉该 fd 的就绪位（用户侧 FD_ISSET 判定）。
                            // 读改写全走 copy 辅助：裸写只做过读校验，COW
                            // 只读页会触发 S 态写故障（此前=整机挂死）。
                            let byte_idx = readfds as usize + fd / 8;
                            let mut b = [0u8; 1];
                            if unsafe { copy_from_user(byte_idx as u64, &mut b) } == 1 {
                                b[0] &= !(1u8 << (fd % 8));
                                unsafe { copy_to_user(byte_idx as u64, &b) };
                            }
                        }
                    }
                }
            }
        }
    }
    if count > 0 {
        return count;
    }
    // 无就绪 fd：小睡一段（≤50ms）后返回 0（提前超时合法，调用方重巡事件）
    let mut sleep_us = 50_000u64;
    if timeout != 0 && user_ok(timeout as u64, 16) {
        unsafe {
            let mut tb = [0u8; 16];
            copy_from_user(timeout as u64, &mut tb);
            let sec = u64::from_le_bytes(tb[0..8].try_into().unwrap_or([0; 8]));
            let nsec = u64::from_le_bytes(tb[8..16].try_into().unwrap_or([0; 8]));
            let total = sec.saturating_mul(1_000_000).saturating_add(nsec / 1000);
            if total == 0 {
                return 0; // 纯非阻塞探测
            }
            sleep_us = sleep_us.min(total);
        }
    } else if timeout == 0 {
        // timeout=NULL（阻塞语义）：退化为 50ms 轮询
    }
    crate::sched_cfs::sleep_until(crate::timer::now_us() + sleep_us);
    0
}

/// ppoll（aarch64 nr 73）：musl 的 poll() 走此调用。
///
/// ABI 与 pselect6 完全不同：x0 = `struct pollfd[]` **数组指针**、
/// x1 = 条数、x2 = `*timespec` 超时、x3 = sigmask（忽略）。
/// pollfd = { fd: i32, events: i16, revents: i16 }（8 字节）；
/// 就绪写 `revents`（copy_to_user 走可写校验 + COW break，不裸写）。
/// 就绪判定与 pselect6 同源：tty 看 UART 待读、常规文件恒就绪、
/// 管道/socket 暂不就绪（无就绪排队），fd<0 按 POSIX 忽略，
/// 未登记 fd 报 POLLNVAL。
const POLLIN: i16 = 0x001;
const POLLNVAL: i16 = 0x020;
/// pollfd 数组上限（256 项 = 2KB 内核栈读缓冲，musl NSOCK/标准 poll 远低于此）。
const PPOLL_MAX: usize = 256;

fn sys_ppoll(f: &mut TrapFrame) -> i64 {
    let fds_ptr = f.x[10] as u64;
    let nfds = f.x[11] as usize;
    let timeout = f.x[12] as usize;
    if fds_ptr == 0 || nfds == 0 {
        return 0;
    }
    let n = nfds.min(PPOLL_MAX);
    if !user_ok(fds_ptr, n * 8) {
        return EFAULT;
    }
    let mut buf = [0u8; PPOLL_MAX * 8];
    unsafe {
        copy_from_user(fds_ptr, &mut buf[..n * 8]);
    }
    let mut ready_count = 0i64;
    let mut changed = false;
    for i in 0..n {
        let fd = i32::from_le_bytes(buf[i * 8..i * 8 + 4].try_into().unwrap_or([0; 4]));
        let events = i16::from_le_bytes(buf[i * 8 + 4..i * 8 + 6].try_into().unwrap_or([0; 2]));
        if fd < 0 {
            continue; // POSIX：fd < 0 的项忽略，revents 置 0
        }
        let mut revents = 0i16;
        if process::get_fd(fd).is_some() {
            let readable = match vfs::fstat(fd) {
                Some((t, _)) if t == crate::fs_simple::T_CHR => crate::uart_rx_pending(),
                Some(_) => true,
                None => false,
            };
            if readable && (events & POLLIN) != 0 {
                revents |= POLLIN;
            }
        } else {
            revents |= POLLNVAL;
        }
        let old = i16::from_le_bytes(buf[i * 8 + 6..i * 8 + 8].try_into().unwrap_or([0; 2]));
        if revents != old {
            buf[i * 8 + 6..i * 8 + 8].copy_from_slice(&revents.to_le_bytes());
            changed = true;
        }
        if revents != 0 {
            ready_count += 1;
        }
    }
    if changed {
        unsafe {
            copy_to_user(fds_ptr, &buf[..n * 8]);
        }
    }
    if ready_count > 0 {
        return ready_count;
    }
    // 无就绪 fd：小睡一段（≤50ms）后返回 0（提前超时合法，调用方重巡）。
    let mut sleep_us = 50_000u64;
    if timeout != 0 && user_ok(timeout as u64, 16) {
        unsafe {
            let mut tb = [0u8; 16];
            copy_from_user(timeout as u64, &mut tb);
            let sec = u64::from_le_bytes(tb[0..8].try_into().unwrap_or([0; 8]));
            let nsec = u64::from_le_bytes(tb[8..16].try_into().unwrap_or([0; 8]));
            let total = sec.saturating_mul(1_000_000).saturating_add(nsec / 1000);
            if total == 0 {
                return 0; // 纯非阻塞探测
            }
            sleep_us = sleep_us.min(total);
        }
    }
    crate::sched_cfs::sleep_until(crate::timer::now_us() + sleep_us);
    0
}

fn sys_nanosleep(f: &mut TrapFrame) -> i64 {
    let req = f.x[10];
    if !user_ok(req, 16) {
        return EFAULT;
    }
    let (sec, nsec) = unsafe {
        // PAN 放行读取用户 timespec{sec,nsec}
        crate::mmu::uaccess_begin();
        let s = *(req as *const i64);
        let n = *((req + 8) as *const i64);
        crate::mmu::uaccess_end();
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
    let buf = f.x[10];
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
    let info = f.x[10];
    if info != 0 {
        unsafe {
            if !user_ok_writable(info, 112) {
                return EFAULT;
            }
            // PAN 放行：写入用户 struct sysinfo（此前只校验后裸写，PAN 下挂死内核）
            crate::mmu::uaccess_begin();
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
            crate::mmu::uaccess_end();
        }
    }
    0
}

fn sys_times(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    if buf != 0 {
        unsafe {
            // struct tms = 4 × clock_t = 32 字节（aarch64 Linux ABI）。
            // 此前按 4*16=64 字节清零，越界多清 32 字节用户相邻数据。
            if user_ok_writable(buf, 32) {
                // PAN 放行：清零用户 tms 缓冲（此前只校验后裸写，PAN 下挂死内核）
                crate::mmu::uaccess_begin();
                core::ptr::write_bytes(buf as *mut u8, 0, 32);
                crate::mmu::uaccess_end();
            }
        }
    }
    (now_usec() / 10_000) as i64
}

/// getrlimit：把默认资源限额写入用户缓冲区（RLIM_INFINITY），
/// 避免调用方读到未初始化的垃圾。此前该路径返回 0 但不写任何数据。
fn sys_getrlimit(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    if buf == 0 {
        return EFAULT;
    }
    if !user_ok_writable(buf, 16) {
        return EFAULT;
    }
    // struct rlimit { u64 rlim_cur; u64 rlim_max; }，无限限额。
    const RLIM_INFINITY: u64 = 0xFFFF_FFFF_FFFF_FFFF;
    // PAN 放行（此前只校验后裸写，PAN 下挂死内核）
    unsafe {
        write_u64_to_user(buf, RLIM_INFINITY);
        write_u64_to_user(buf + 8, RLIM_INFINITY);
    }
    0
}

/// prlimit64(pid, resource, new_rlim, old_rlim)：bionic/glibc 的
/// `getrlimit()` 即本调用的封装。ABI 与 getrlimit 完全不同 —— x0 是 pid、
/// x2 是 new_rlim、x3 才是 old_rlim 输出缓冲。此前复用 sys_getrlimit 把
/// x0 当输出指针：pid=0 → 空指针 EFAULT；pid=N → 地址 N 未映射 EFAULT，
/// 所有 getrlimit 调用方一律失败。
/// 本内核资源无上限：new_rlim 接受但不生效（write 侧调用方传 RLIM_INFINITY），
/// old_rlim 回填 RLIM_INFINITY。
fn sys_prlimit64(f: &mut TrapFrame) -> i64 {
    let pid = f.x[10] as i32;
    let _resource = f.x[11];
    let new_rlim = f.x[12];
    let old_rlim = f.x[13];
    // pid：0 = 当前进程；否则必须指向已存在进程（sig-0 探测语义）
    if pid != 0 && (pid < 0 || pid as usize >= process::MAX_PROC) {
        return ESRCH;
    }
    if pid > 0 && !process::process_exists(pid as u32) {
        return ESRCH;
    }
    if new_rlim != 0 && !user_ok(new_rlim, 16) {
        return EFAULT;
    }
    if old_rlim != 0 {
        if !user_ok_writable(old_rlim, 16) {
            return EFAULT;
        }
        const RLIM_INFINITY: u64 = 0xFFFF_FFFF_FFFF_FFFF;
        unsafe {
            write_u64_to_user(old_rlim, RLIM_INFINITY);
            write_u64_to_user(old_rlim + 8, RLIM_INFINITY);
        }
    }
    0
}

/// 当前进程 umask（全局近似；POSIX 语义：返回旧值并更新）。
static mut CUR_UMASK: u32 = 0o022;

/// umask：返回旧掩码并设置新掩码（此前恒返回常量、忽略参数）。
fn sys_umask(mask: u32) -> i64 {
    let m = mask & 0o777;
    unsafe {
        let old = CUR_UMASK;
        CUR_UMASK = m;
        old as i64
    }
}

// ==================== getrandom 混合熵池 ====================
//
// BUG_REPORT Low 项「getrandom 用时间做种子的确定性 LCG，无真实熵」的修复：
// 64 字节内核熵池 + 每次调用混入三类熵源——
//   (a) virtio-rng 硬件随机字节（1ms 限时非阻塞轮询，设备未就绪/忙/超时
//       只跳过该来源，绝不等设备）；
//   (b) 高分辨率时间计数器（`time` CSR 原始计数 + 墙钟微秒：前者区分两次
//       调用，后者区分两次启动）；
//   (c) 调用者 pid/tid、全局调用序号与内核栈地址（任务间可区分）。
// 输出仍基于移位/乘法类 PRNG（xorshift64*），但种子与状态全部来自熵池：
// 池随每次调用搅拌演变，两次调用、两次启动的输出序列均不同。无 libm、
// 纯四则/异或/移位；非密码学安全（无抗预测积累），但已消除纯时间种子的
// 确定性。

/// 熵池状态：8×u64 = 64 字节。初值取 π 的十六进制展开——非秘密常量，
/// 只保证初始状态非零且各槽不对称（全零态会让 xorshift 自锁）。
static mut ENTROPY_POOL: [u64; 8] = [
    0x243F_6A88_85A3_08D3,
    0x1319_8A2E_0370_7344,
    0xA409_3822_299F_31D0,
    0x082E_FA98_EC4E_6C89,
    0x4528_21E6_38D0_1377,
    0xBE54_66CF_34E9_0C6C,
    0xC0AC_29B7_C97C_50DD,
    0x3F84_D5B5_B547_0917,
];
/// getrandom 全局调用序号（单调递增，回绕安全：仅用于混熵与轮转槽位）。
static mut ENTROPY_SEQ: u64 = 0;
/// 熵池自旋锁（SMP 跨核互斥）。先屏蔽 IRQ 再抢锁：抢到后本核不可能被
/// 抢占（防「持有者被换下、同核等待者死自旋」）；等待者自旋的临界区是
/// 纯算术（微秒级），有界。
static ENTROPY_LOCK: AtomicBool = AtomicBool::new(false);

/// 保存 sstatus 并屏蔽 S 态 IRQ（与 heap.rs 同款；嵌套安全——恢复完整值）。
unsafe fn ent_irq_save() -> u64 {
    let d: u64;
    core::arch::asm!("csrr {}, sstatus", out(reg) d);
    core::arch::asm!("csrci sstatus, 2");
    d
}

unsafe fn ent_irq_restore(saved: u64) {
    core::arch::asm!("csrw sstatus, {}", in(reg) saved);
}

/// 抢熵池锁（返回调用前的 sstatus，供 [`ent_unlock`] 恢复）。
fn ent_lock() -> u64 {
    let saved = unsafe { ent_irq_save() };
    while ENTROPY_LOCK
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    saved
}

fn ent_unlock(saved: u64) {
    ENTROPY_LOCK.store(false, Ordering::Release);
    unsafe { ent_irq_restore(saved) };
}

/// SplitMix64 终结函数（纯乘法/异或/移位，no_std 无 libm 依赖）。
#[inline]
fn splitmix_fin(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 把一个熵字吸收进池：SplitMix64 扩散后异或进轮转槽（`slot` 前进一步）。
#[inline]
fn pool_absorb(pool: &mut [u64; 8], slot: &mut usize, word: u64) {
    pool[*slot] ^= splitmix_fin(word);
    *slot = (*slot + 1) % pool.len();
}

/// 搅拌熵池：每槽与左右邻居交叉扩散（3 轮达到雪崩效应；串行轮内读到的
/// 邻居可能已更新，属有意的扩散路径）。
fn pool_stir(pool: &mut [u64; 8]) {
    for _ in 0..3 {
        for i in 0..pool.len() {
            let prev = pool[(i + pool.len() - 1) % pool.len()];
            let next = pool[(i + 1) % pool.len()];
            let v = pool[i] ^ prev.rotate_left(23) ^ next.rotate_right(17);
            let v = v.wrapping_mul(0x2545_F491_4F6C_DD1D);
            pool[i] = v ^ (v >> 29);
        }
    }
}

/// 搅拌后从池状态扩展出 `out.len()` 字节输出（xorshift64*，状态恒非零），
/// 并把生成器末状态回混进池（同输入状态两次扩展的输出也不同）。
fn pool_extract(pool: &mut [u64; 8], out: &mut [u8]) {
    pool_stir(pool);
    let mut x = 0u64;
    for w in pool.iter() {
        x ^= *w;
    }
    x |= 1; // xorshift 状态非零
    for chunk in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
    }
    pool[0] ^= splitmix_fin(x);
}

fn sys_getrandom(f: &mut TrapFrame) -> i64 {
    let buf = f.x[10];
    let len = f.x[11] as usize;
    if len == 0 {
        return 0;
    }
    let n = len.min(256);
    let mut tmp = [0u8; 256];

    // (a) virtio-rng 硬件熵：1ms 限时非阻塞（未就绪/忙/超时 → hw_len = 0，
    //     只降熵、不报错、不等待）。
    let mut hw = [0u8; 64];
    let hw_len = match crate::virtio_rng::get_random_timed(&mut hw, 1_000) {
        r if r > 0 => r as usize,
        _ => 0,
    };
    // (b) 高分辨率时间：time CSR 原始计数（10MHz 单调，区分两次调用）+
    //     墙钟微秒（含 RTC 基准，区分两次启动）。
    let now_ticks = crate::timer::read_time();
    let wall_us = now_usec();
    // (c) 调用者身份：pid/tid、全局调用序号、内核栈地址（任务间可区分）。
    let pid_tid = ((process::current_pid() as u64) << 32) | sched_cfs::current_tid() as u64;
    let stack_hint = &tmp as *const _ as u64;

    // 临界区：取池副本 → 吸收三类熵 → 搅拌扩展 → 写回（读-改-写整体在
    // 锁内完成，跨核/跨任务不会撕裂池状态）。
    let saved = ent_lock();
    unsafe {
        let mut pool: [u64; 8] = core::ptr::read(core::ptr::addr_of!(ENTROPY_POOL));
        let seq = ENTROPY_SEQ;
        ENTROPY_SEQ = seq.wrapping_add(1);

        let mut slot = (seq as usize) % pool.len();
        // (a) 硬件熵按小端 8 字节字吸收（不足 8 字节尾部补零）
        let mut i = 0usize;
        while i < hw_len {
            let take = (hw_len - i).min(8);
            let mut w = 0u64;
            for b in 0..take {
                w |= (hw[i + b] as u64) << (8 * b);
            }
            pool_absorb(&mut pool, &mut slot, w);
            i += take;
        }
        // (b) 时间熵
        pool_absorb(&mut pool, &mut slot, now_ticks);
        pool_absorb(&mut pool, &mut slot, wall_us);
        // (c) 身份熵
        pool_absorb(&mut pool, &mut slot, pid_tid);
        pool_absorb(&mut pool, &mut slot, seq);
        pool_absorb(&mut pool, &mut slot, stack_hint);

        pool_extract(&mut pool, &mut tmp[..n]);
        core::ptr::write(core::ptr::addr_of_mut!(ENTROPY_POOL), pool);
    }
    ent_unlock(saved);

    if !user_ok_writable(buf, n) {
        return EFAULT;
    }
    unsafe {
        crate::mmu::uaccess_begin();
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf as *mut u8, n);
        crate::mmu::uaccess_end();
    }
    n as i64
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
    // PAN 放行：用户 `sockaddr_in` 的裸读需临时关 PAN（无 PAN 时空操作）。
    // 此前只做 `user_ok` 校验后直接解引用——PAN 使能核心上 EL1 读用户页
    // 必触发 data abort 挂死内核（用户态 DoS）。
    crate::mmu::uaccess_begin();
    let port = u16::from_be_bytes([*(addr as *const u8).add(2), *(addr as *const u8).add(3)]);
    let ip = u32::from_be_bytes([
        *(addr as *const u8).add(4),
        *(addr as *const u8).add(5),
        *(addr as *const u8).add(6),
        *(addr as *const u8).add(7),
    ]);
    crate::mmu::uaccess_end();
    (port, ip)
}

fn sys_socket(f: &mut TrapFrame) -> i64 {
    let domain = f.x[10] as i32;
    let ty = f.x[11] as i32;
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
    let fd = f.x[10] as i32;
    let (port, ip) = unsafe { read_sockaddr(f.x[11]) };
    socket::connect(fd, ip, port) as i64
}

fn sys_bind(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let (port, _ip) = unsafe { read_sockaddr(f.x[11]) };
    socket::bind(fd, port) as i64
}

fn sys_sendto(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let len = f.x[12] as usize;
    let (port, ip) = unsafe { read_sockaddr(f.x[14]) };
    // Linux 语义：零长度 send 合法（TCP 侧返回 0，不产生数据段）
    if len == 0 {
        if ip != 0 {
            return socket::send_to(fd, &[], ip, port) as i64;
        }
        return socket::send(fd, &[]) as i64;
    }
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
    let fd = f.x[10] as i32;
    let buf_ptr = f.x[11];
    let len = f.x[12] as usize;
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
    let fd = f.x[10] as i32;
    socket::close(fd);
    0
}

/// msghdr：`{ name: u64, namelen: u32, iov: u64, iovlen: u64, ... }`。
fn sys_sendmsg(f: &mut TrapFrame) -> i64 {
    let fd = f.x[10] as i32;
    let msg = f.x[11];
    if !user_ok(msg, 24) {
        return EFAULT;
    }
    unsafe {
        // PAN 放行读取 msghdr 与 iovBase/len（与 sys_recvmsg 同型）
        crate::mmu::uaccess_begin();
        let iov = *(msg as *const u64).add(2);
        if !user_ok(iov, 16) {
            crate::mmu::uaccess_end();
            return EFAULT;
        }
        let base = *(iov as *const u64);
        let len = *(iov as *const u64).add(1) as usize;
        crate::mmu::uaccess_end();
        // 零长度 send 合法（Linux 语义返回 0）
        if len == 0 {
            return socket::send(fd, &[]) as i64;
        }
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
    let fd = f.x[10] as i32;
    let msg = f.x[11];
    if !user_ok(msg, 24) {
        return EFAULT;
    }
    unsafe {
        // PAN 放行读取 msghdr{name, namelen, iov, ...} 头 24 字节
        crate::mmu::uaccess_begin();
        let name = *(msg as *const u64);
        let iov = *(msg as *const u64).add(2);
        if !user_ok(iov, 16) {
            crate::mmu::uaccess_end();
            return EFAULT;
        }
        let base = *(iov as *const u64);
        let len = *(iov as *const u64).add(1) as usize;
        crate::mmu::uaccess_end();
        let n = len.min(4096);
        let mut tmp = [0u8; 4096];
        let r = socket::recv(fd, &mut tmp[..n]);
        if r <= 0 {
            return r as i64;
        }
        // 数据拷贝失败：返回 EFAULT（此前忽略拷贝结果，超长数据被截断时仍报成功）
        let copied = copy_to_user(base, &tmp[..r as usize]);
        if copied != r as usize {
            return EFAULT;
        }
        // 提供 name（对端地址）时写回 msg_namelen：本内核 socket 层不返回对端
        // 地址，写 0 ——调用方读到确定值而非未初始化垃圾（Linux recvmsg 语义）。
        if name != 0 {
            let namelen_p = msg + 8;
            if user_ok_writable(namelen_p, 4) {
                write_u32_to_user(namelen_p, 0);
            }
        }
        r as i64
    }
}
