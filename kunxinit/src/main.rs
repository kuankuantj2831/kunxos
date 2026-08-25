// KunxOS init（多进程版）：PID 1 用户态 init，经真正的 `fork`/`execve` 拉起
// 系统守护进程并回收子进程。
//
// 只依赖 Linux AArch64 系统调用（asm-generic 编号），不链接任何 libc、不使用堆。
// 启动流程：
//   1. 打印版本横幅
//   2. fork + execve 拉起 /bin/networkd、/bin/displayd
//   3. 检查 /data/oobe/.done：不存在（首次启动）则拉起 /bin/oobe 并 wait4
//      阻塞等待其写 .done 后退出；存在则跳过
//   4. fork + execve 拉起 /bin/launcher（OOBE 之后才启动，避免抢画 fb0）
//   5. 主循环 wait4(-1) 回收退出子进程；无子进程后退出
//
// fork 经 clone(SIGCHLD) 实现（内核把不带 CLONE_THREAD 的 clone 视为 fork），
// 子进程从 fork 返回 0，随后 execve 替换为对应守护进程映像。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;
const SYS_OPENAT: usize = 56;
const SYS_READ: usize = 63;
const SYS_CLOSE: usize = 57;

// ==================== 常量 ====================

/// clone 标志：SIGCHLD（标准 fork 语义，无 CLONE_THREAD/VM）。
const SIGCHLD: usize = 17;

const PATH_CAP: usize = 96;

/// openat 的 dirfd 魔数（单根文件系统，内核忽略 dirfd，仅保持 ABI 兼容）。
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;

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

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
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

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

/// openat 封装：把 `path` 拷进静态缓冲（NUL 结尾）后调用 openat(AT_FDCWD)。
/// 返回 fd（>=0）或负 errno。
unsafe fn sys_open(path: &str, flags: usize) -> isize {
    let mut p = [0u8; PATH_CAP];
    let plen = path.len().min(PATH_CAP - 1);
    p[..plen].copy_from_slice(&path.as_bytes()[..plen]);
    p[plen] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, flags) as isize
}

/// 检查路径是否存在：open 成功即存在（随后 close 释放 fd）。
/// OOBE 完成标志 /data/oobe/.done 的读取端。
unsafe fn file_exists(path: &str) -> bool {
    let fd = sys_open(path, O_RDONLY);
    if fd >= 0 {
        syscall1(SYS_CLOSE, fd as usize);
        true
    } else {
        false
    }
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
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

/// 退出当前进程（不返回）。
unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
}

// ==================== 进程原语 ====================

/// fork：AArch64 无独立 fork 系统调用，经 clone(SIGCHLD) 实现。
/// 成功父进程返回子 pid（>=0），子进程返回 0；失败返回负 errno。
unsafe fn fork() -> isize {
    syscall5(SYS_CLONE, SIGCHLD, 0, 0, 0, 0) as isize
}

/// execve：替换当前进程映像；失败返回负 errno。
unsafe fn execve(path: &[u8], argv: *const usize, envp: *const usize) -> isize {
    syscall3(SYS_EXECVE, path.as_ptr() as usize, argv as usize, envp as usize) as isize
}

/// wait4：阻塞等待指定子进程（pid = -1 表示任意），返回退出码；失败返回 -1。
unsafe fn wait4(pid: i32, status: *mut i32) -> isize {
    syscall4(SYS_WAIT4, pid as usize, status as usize, 0, 0) as isize
}

// ==================== 服务拉起 ====================

/// fork 一个子进程并在其中 execve `path`；父进程打印子 pid 并返回子 pid。
/// 失败返回负 errno（fork 失败）。
unsafe fn spawn_service(name: &str, path: &str) -> isize {
    let pid = fork();
    if pid == 0 {
        // 子进程：构造 argv（argv[0] = 程序名，argv[1] = NULL）并 execve。
        //
        // ★ 关键：p / name_buf / argv 必须用**静态**缓冲而非栈局部。fork
        // 子进程从 fork 返回时沿用父进程当时的用户栈指针，而 init 栈很浅
        // （sp 距 USER_STACK_TOP=0x3F000000 仅 ~176 字节余量）；若编译器把
        // 两个 PATH_CAP(96B) 缓冲放在 sp 上方（如 [sp+96..192]），子进程的
        // 192 字节清零/填充会越过栈顶写入未映射区（内核把栈顶之上映射为
        // EL1-only 块 → EL0 写权限故障；或覆盖 argv 区引发后续损坏，表现为
        // 子进程偶发 PC=0 / LR 损坏）。静态缓冲彻底消除「向 sp 上方写」。
        static mut P: [u8; PATH_CAP] = [0; PATH_CAP];
        static mut NAME_BUF: [u8; PATH_CAP] = [0; PATH_CAP];
        static mut ARGV: [usize; 2] = [0; 2];
        let p = &mut *core::ptr::addr_of_mut!(P);
        let name_buf = &mut *core::ptr::addr_of_mut!(NAME_BUF);
        let argv = &mut *core::ptr::addr_of_mut!(ARGV);
        let plen = path.len().min(PATH_CAP - 1);
        p[..plen].copy_from_slice(&path.as_bytes()[..plen]);
        p[plen] = 0;
        let nlen = name.len().min(PATH_CAP - 1);
        name_buf[..nlen].copy_from_slice(&name.as_bytes()[..nlen]);
        name_buf[nlen] = 0;
        argv[0] = name_buf.as_ptr() as usize;
        argv[1] = 0;
        execve(&p[..plen + 1], argv.as_ptr(), core::ptr::null());
        // execve 失败：退出子进程
        exit(127);
    } else if pid > 0 {
        print("[init] fork ");
        print(name);
        print(" pid=");
        print_dec(pid as usize);
        print("\n");
    } else {
        print("[init] fork ");
        print(name);
        print(" failed\n");
    }
    pid
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[init] KunxOS init v0.2 (multi-process) starting\n");

    unsafe {
        spawn_service("networkd", "/bin/networkd");
        // displayd（已废弃的合成器演示）不再拉起：launcher 直接 open/mmap /dev/fb0
        // 绘制；displayd 的 30fps demo 依赖动画定时器 TIMER2（已禁用），拉起会
        // 永久阻塞 init 的子进程回收，导致 shell 永远不出现。

        // OOBE 启动链：/data/oobe/.done 不存在 => 首次启动，先运行 OOBE，
        // 并阻塞等待其写 .done 后退出，再启动 launcher（避免两者同时抢画整屏 fb0）。
        if !file_exists("/data/oobe/.done") {
            print("[init] /data/oobe/.done missing, starting OOBE\n");
            let oobe_pid = spawn_service("oobe", "/bin/oobe");
            if oobe_pid > 0 {
                let mut status = 0i32;
                let r = wait4(oobe_pid as i32, &mut status);
                print("[init] OOBE (pid ");
                print_dec(oobe_pid as usize);
                print(") exited, r=");
                print_dec(r as usize);
                print(" status=");
                print_dec(((status as u32) >> 8) as usize);
                print("\n");
            }
        } else {
            print("[init] /data/oobe/.done present, skipping OOBE\n");
        }

        spawn_service("launcher", "/bin/launcher");
    }

    // 主循环：回收退出子进程。
    loop {
        let mut status = 0i32;
        let r = unsafe { wait4(-1, &mut status) };
        if r <= 0 {
            break;
        }
        print("[init] reaped pid=");
        print_dec(r as usize);
        print(" status=");
        print_dec((status as u32) as usize);
        print("\n");
    }

    print("[init] all children reaped, exiting\n");
    0
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

// ==================== panic ====================

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: kunxinit\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
