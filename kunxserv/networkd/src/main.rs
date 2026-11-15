// KunxOS networkd：网络守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init_main 在 /init 结束后加载（/bin/networkd）。职责：
//   1. 向 /dev/tty（fd 1/2）打印启动日志，并写入 /var/log/networkd.log
//   2. 读取当前网络状态：内核已初始化 virtio-net + DHCP，但未提供 /proc/net
//      或网络 ioctl，故硬编码 QEMU slirp 默认值并打印提示
//   3. 创建命名管道 /var/run/networkd.pipe（内核无 mkfifo，用普通文件模拟）
//   4. 轮询管道处理命令：STATUS -> 返回 IP/MAC/GW/DNS；RENEW -> 打印 renew 请求
//   5. 主循环：定时检查链路状态（计数器模拟）
//
// 说明：当前内核为单 EL0 用户进程模型（调度器不保存/恢复 TTBR0），
// 守护进程无法与交互式 Shell 的 busybox 并发运行，因此演示若干轮后正常退出，
// 由内核继续加载下一个服务或内置 Shell。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]


// ==================== 系统调用号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

const SEEK_SET: usize = 0;

// ==================== 网络默认值（QEMU slirp 用户网络） ====================

const IP: [u8; 4] = [10, 0, 2, 15];
const GW: [u8; 4] = [10, 0, 2, 2];
const DNS: [u8; 4] = [10, 0, 2, 3];

/// 本机 MAC 地址：来自 HAL（QEMU virt 后端返回 virtio-net 的固定 MAC）。
fn mac() -> [u8; 6] {
    match kunxhal::net_init() {
        Some(n) => n.mac,
        None => [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
    }
}

const LOG_PATH: &str = "/var/log/networkd.log";
const PIPE_PATH: &str = "/var/run/networkd.pipe";

/// 演示主循环轮数（每轮约 200ms，避免拖慢启动）。

// ==================== 系统调用封装 ====================

use kunxrt::syscall::syscall1;

use kunxrt::syscall::syscall2;

use kunxrt::syscall::syscall3;

/// renameat 四参调用（命令管道认领取走）。
use kunxrt::syscall::syscall4;

/// 同目录改名：目标已存在时内核按 POSIX 语义原子替换。成功 0。
fn rename_path(old: &str, new: &str) -> isize {
    let mut o = [0u8; 96];
    let mut q = [0u8; 96];
    let on = old.len().min(95);
    o[..on].copy_from_slice(&old.as_bytes()[..on]);
    let nn = new.len().min(95);
    q[..nn].copy_from_slice(&new.as_bytes()[..nn]);
    unsafe {
        syscall4(
            SYS_RENAMEAT,
            AT_FDCWD,
            o.as_ptr() as usize,
            AT_FDCWD,
            q.as_ptr() as usize,
        ) as isize
    }
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

fn print_bytes(b: &[u8]) {
    unsafe {
        write_fd(1, b);
    }
}

fn print_dec(mut v: usize) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
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

/// 打印 IPv4（点分十进制）。
fn print_ip(ip: &[u8; 4]) {
    for (i, b) in ip.iter().enumerate() {
        if i > 0 {
            print(".");
        }
        print_dec(*b as usize);
    }
}

/// 打印 MAC（冒号分隔十六进制）。
fn print_mac(mac: &[u8; 6]) {
    for (i, b) in mac.iter().enumerate() {
        if i > 0 {
            print(":");
        }
        let hi = b >> 4;
        let lo = b & 0xF;
        let hex = b"0123456789abcdef";
        print_bytes(&[hex[hi as usize], hex[lo as usize]]);
    }
}

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 文件操作 ====================

/// 路径缓冲（确保传给 syscall 的路径以 NUL 结尾）。
static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

/// 把一行写入日志文件（追加，多行文本合并成一次写）。
fn log_to_file(line: &str) {
    unsafe {
        let fd = open(LOG_PATH, O_CREAT | O_WRONLY | O_TRUNC);
        if fd >= 0 {
            // 由于逐条日志都 open(O_TRUNC)，这里仅演示「写入日志文件」语义。
            write_fd(fd as usize, line.as_bytes());
            write_fd(fd as usize, b"\n");
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

// ==================== 命令处理 ====================

fn bytes_starts_with(a: &[u8], prefix: &[u8]) -> bool {
    a.len() >= prefix.len() && a[..prefix.len()].iter().zip(prefix).all(|(x, y)| x == y)
}

fn handle_command(cmd: &[u8]) {
    if bytes_starts_with(cmd, b"STATUS") {
        print("[networkd] STATUS: IP=");
        print_ip(&IP);
        print(", MAC=");
        print_mac(&mac());
        print(", GW=");
        print_ip(&GW);
        print(", DNS=");
        print_ip(&DNS);
        print("\n");
    } else if bytes_starts_with(cmd, b"RENEW") {
        print("[networkd] 已请求 DHCP renew\n");
    } else if !cmd.is_empty() {
        print("[networkd] 未知命令\n");
    }
}

/// 读取管道文件内容并逐行处理（普通文件模拟命名管道）。
fn poll_pipe() {
    // 256B 缓冲会截断长命令（管道文件为普通文件模拟）。1024B 覆盖全部
    // 现有命令与扩展命令；循环读到 EOF，长命令尾部不截断丢失。
    static mut BUF: [u8; 1024] = [0u8; 1024];
    static mut WARNED: bool = false;
    // 认领名 "/var/run/networkd.pipe.r"
    let mut taken = [0u8; 32];
    let cb = PIPE_PATH.as_bytes();
    taken[..cb.len()].copy_from_slice(cb);
    taken[cb.len()] = b'.';
    taken[cb.len() + 1] = b'r';
    let tpath = core::str::from_utf8(&taken[..cb.len() + 2]).unwrap_or("");
    // 清掉上次崩溃残留的认领文件（存在则 rename 失败）
    unsafe { syscall3(SYS_UNLINKAT, AT_FDCWD, taken.as_ptr() as usize, 0) };
    // rename 认领取走：旧的「read → ftruncate 清空」两步之间写入方追加的
    // 命令会被 ftruncate 一并抹掉（丢命令）；认领后新命令落在重生的管道
    // 文件上，留待下一轮（真 FIFO 语义）。
    if rename_path(PIPE_PATH, tpath) != 0 {
        // 无管道文件：主循环稍后重建（k_main 启动路径 O_CREAT 创建）。
        return;
    }
    unsafe {
        let mut total = 0usize;
        let fd = open(tpath, O_RDWR);
        if fd < 0 {
            if !WARNED {
                print("[networkd] 警告：无法打开认领的管道 ");
                print(tpath);
                print("\n");
                WARNED = true;
            }
            // ★ 不能提前 return：认领文件残留会被下轮残留清理删掉、
            // rename 因 PIPE_PATH 不存在恒失败 → 管道永久失效。跳过读取、
            // 继续走清认领 + 重建（wifid/audiod/alarmd 同款纪律）。
        } else {
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
            // 循环读到 EOF：单次 read 长度有限，长命令尾部不能截断丢失。
            loop {
                if total >= BUF.len() {
                    print("[networkd] 警告：管道命令过长，尾部已丢弃\n");
                    break;
                }
                let want = (BUF.len() - total).min(256);
                let n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr().add(total) as usize, want);
                if n == 0 {
                    break; // 读至文件末尾（EOF）
                }
                if n > want {
                    break; // 读取出错（异常返回值），本轮丢弃
                }
                total += n as usize;
            }
            syscall1(SYS_CLOSE, fd as usize);
        }
        syscall3(SYS_UNLINKAT, AT_FDCWD, taken.as_ptr() as usize, 0);
        // 重建空管道文件（无 O_TRUNC：认领后写入方已追加的新命令不截断）
        let fd2 = open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd2 >= 0 {
            syscall1(SYS_CLOSE, fd2 as usize);
        }
        if total > 0 {
            let data = &BUF[..total];
            let mut start = 0usize;
            for i in 0..=total {
                if i == total || data[i] == b'\n' {
                    if i > start {
                        handle_command(&data[start..i]);
                    }
                    start = i + 1;
                }
            }
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[networkd] 已启动\n");

    // 网络状态（内核已由 virtio-net + DHCP 初始化，此处硬编码 QEMU 默认值）
    print("[networkd] IP=");
    print_ip(&IP);
    print(", MAC=");
    print_mac(&mac());
    print(", GW=");
    print_ip(&GW);
    print("\n");
    print("[networkd]（无 /proc/net；使用 QEMU slirp 默认值）\n");

    // 写日志文件
    log_to_file("[networkd] IP=10.0.2.15 MAC=52:54:00:12:34:56 GW=10.0.2.2");

    // 创建命名管道（普通文件模拟）
    unsafe {
        let fd = open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
            print("[networkd] 正在监听 ");
            print(PIPE_PATH);
            print("\n");
        } else {
            print("[networkd] 致命错误：无法创建管道 ");
            print(PIPE_PATH);
            print(" (OOBE/launcher 网络操作将不可用)\n");
        }
    }

    // 主循环：轮询管道命令。常驻服务：OOBE/设置会随时经管道发 STATUS/配置
    // 命令，限时轮次退出（3×200ms）后所有命令静默丢失，写入者无从得知。
    loop {
        poll_pipe();
        sleep_ms(200);
    }
}

// ==================== 入口 ====================

kunxrt::entry!();

kunxrt::panic_handler!("networkd");
