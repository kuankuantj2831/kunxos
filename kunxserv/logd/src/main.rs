// KunxOS logd：用户态日志守护进程（logcat 等价物的持久化侧）。
//
// EL0 用户态、裸 syscall、no_std、无堆、常驻。职责：
//   1. 打开 /dev/kmsg（内核结构化日志通道，read = 增量取一条记录）
//   2. 把每条记录追加写入日志文件（/data/log/logcat，无 /data 时
//      回退 /tmp/log/logcat、再退 /var/log/logcat），文件超 256KB 截断
//      （环形文件，防止 SFS 单文件上限 4.2MB 与磁盘膨胀）
//   3. 主循环：读 kmsg → 落盘 → 睡眠 100ms
//
// 应用/内核的 logcat 视图由 shell `log` 命令直接读 /dev/kmsg 提供；
// logd 提供的是**持久化**（重启后仍可查）。

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
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MKDIRAT: usize = 34;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;

const SEEK_SET: usize = 0;
const SEEK_CUR: usize = 1;
const SEEK_END: usize = 2;

const KMSG_PATH: &str = "/dev/kmsg";
/// 日志文件上限（256KB；超限截断为环形文件）。
const MAX_LOG_BYTES: usize = 262_144;
/// 主循环睡眠间隔（ms）。
const POLL_MS: usize = 100;

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

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
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

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

/// openat 封装（路径拷入静态缓冲，NUL 结尾）。
unsafe fn sys_open(path: &str, flags: usize) -> isize {
    static mut P: [u8; 96] = [0; 96];
    let p = &mut *core::ptr::addr_of_mut!(P);
    let plen = path.len().min(p.len() - 1);
    p[..plen].copy_from_slice(&path.as_bytes()[..plen]);
    p[plen] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, flags) as isize
}

/// mkdirat 封装（单段路径；内核忽略 dirfd）。
unsafe fn sys_mkdir(path: &str) -> isize {
    static mut P: [u8; 96] = [0; 96];
    let p = &mut *core::ptr::addr_of_mut!(P);
    let plen = path.len().min(p.len() - 1);
    p[..plen].copy_from_slice(&path.as_bytes()[..plen]);
    p[plen] = 0;
    syscall3(SYS_MKDIRAT, AT_FDCWD, p.as_ptr() as usize, 0) as isize
}

/// 睡眠（毫秒）。内核 sys_nanosleep 接收 {tv_sec, tv_nsec} 指针。
unsafe fn sleep_ms(ms: usize) {
    static mut REQ: [usize; 2] = [0; 2];
    let req = &mut *core::ptr::addr_of_mut!(REQ);
    req[0] = ms / 1000;
    req[1] = (ms % 1000) * 1_000_000;
    syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
}

/// 文件当前大小（lseek SEEK_END 返回），失败返回 -1。
unsafe fn file_size(fd: usize) -> isize {
    syscall3(SYS_LSEEK, fd, 0, SEEK_END) as isize
}

// ==================== 日志目录选择 ====================

/// 探测可用目录：/data（持久化分区）> /tmp（tmpfs）> /var（rootfs）。
/// 返回日志目录路径（静态字符串）。
fn log_dir() -> &'static str {
    if file_exists("/data") {
        let _ = unsafe { sys_mkdir("/data/log") };
        "/data/log"
    } else if file_exists("/tmp") {
        let _ = unsafe { sys_mkdir("/tmp/log") };
        "/tmp/log"
    } else {
        "/var/log"
    }
}

fn file_exists(path: &str) -> bool {
    unsafe {
        let fd = sys_open(path, O_RDONLY);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
            true
        } else {
            false
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[logd] 启动中\n");

    let dir = log_dir();
    print("[logd] 日志目录: ");
    print(dir);
    print("\n");

    // 打开日志文件（追加）
    let mut log_path = [0u8; 96];
    let mut n = 0usize;
    for &c in dir.as_bytes() {
        if n + 1 >= log_path.len() {
            break;
        }
        log_path[n] = c;
        n += 1;
    }
    if log_path[n - 1] != b'/' && n + 1 < log_path.len() {
        log_path[n] = b'/';
        n += 1;
    }
    for &c in b"logcat".iter() {
        if n + 1 < log_path.len() {
            log_path[n] = c;
            n += 1;
        }
    }
    log_path[n] = 0;
    let log_path_str = unsafe {
        core::str::from_utf8(core::slice::from_raw_parts(log_path.as_ptr(), n)).unwrap_or("")
    };

    unsafe {
        let log_fd = sys_open(log_path_str, O_WRONLY | O_CREAT);
        if log_fd < 0 {
            print("[logd] FATAL: 无法打开日志文件，退出\n");
            syscall1(SYS_EXIT, 1);
        }
        let kmsg_fd = sys_open(KMSG_PATH, O_RDONLY);
        if kmsg_fd < 0 {
            print("[logd] FATAL: 无法打开 /dev/kmsg，退出\n");
            syscall1(SYS_EXIT, 1);
        }

        let mut wrote = 0usize;
        loop {
            // 读一条 kmsg 记录（内核 log_read_next：无新记录返回 0）
            let mut buf = [0u8; 256];
            let r = syscall3(SYS_READ, kmsg_fd as usize, buf.as_mut_ptr() as usize, buf.len());
            // 读失败 -1（usize 天文数字）按 isize 判负：否则每 100ms 往日志
            // 文件写 256 字节零（把读错误当数据持久化，且日志被垃圾灌满）
            if (r as isize) > 0 {
                let n = (r as usize).min(buf.len());
                // 超长截断（理论上单条记录 ≤ 144B）
                let n = n.min(buf.len());
                // 环形文件：超 256KB 截断
                if file_size(log_fd as usize) as usize + n > MAX_LOG_BYTES {
                    syscall2(SYS_FTRUNCATE, log_fd as usize, 0);
                }
                // 定位末尾并写入
                syscall3(SYS_LSEEK, log_fd as usize, 0, SEEK_END);
                let w = syscall3(SYS_WRITE, log_fd as usize, buf.as_ptr() as usize, n);
                if w != n {
                    print("[logd] 写入失败\n");
                }
                wrote += 1;
                if wrote % 50 == 0 {
                    print("[logd] 已持久化 ");
                    print_dec(wrote);
                    print(" 条记录\n");
                }
            }
            sleep_ms(POLL_MS);
        }
    }
}

fn print_dec(mut v: usize) {
    let mut b = [0u8; 12];
    let mut i = b.len();
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    unsafe {
        write_fd(1, &b[i..]);
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
        write_fd(2, b"PANIC: logd\n");
        if w.len > 0 {
            write_fd(2, &w.buf[..w.len]);
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
