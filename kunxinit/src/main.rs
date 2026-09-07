// KunxOS init（多进程版）：PID 1 用户态 init，经真正的 `fork`/`execve` 拉起
// 系统守护进程并回收子进程。
//
// 只依赖 Linux AArch64 系统调用（asm-generic 编号），不链接任何 libc、不使用堆。
// 启动流程：
//   1. 打印版本横幅
//   2. fork + execve 拉起常驻守护进程：networkd / wifid / camerad / audiod /
//      lifd / alarmd / sensord / rild（有真实消费者，见各自注释）
//   3. 检查 /data/oobe/.done：不存在（首次启动）则拉起 /bin/oobe 并 wait4
//      阻塞等待其写 .done 后退出；存在则跳过
//   4. fork + execve 拉起 /bin/launcher（OOBE 之后才启动，避免抢画 fb0）
//   5. 主循环 wait4(-1) 回收退出子进程；无子进程后退出
//
// 不再自动拉起的服务（保留于 /bin 供 shell 手动诊断，避免开机无谓常驻）：
//   - displayd：已废弃（launcher 直接操作 /dev/fb0），源码已删除
//   - logd：无应用读取其持久化输出，shell `log` 直接读 /dev/kmsg
//   - pushd：远程推送为占位，本地通知队列仅 lcapp 测试应用消费
//   - positiond：QEMU 下为模拟 GPS，Pi4 未接串口，仅 map 演示读取
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
const SYS_REBOOT: usize = 142;

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

/// 关机（reboot(RB_POWER_OFF) → 内核 PSCI SYSTEM_OFF；QEMU -no-reboot 下退出）。
unsafe fn poweroff() -> ! {
    syscall3(SYS_REBOOT, 0xFEE1_DEAD, 0x2812_1969, 0x4321_FEDC);
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

/// wait4(WNOHANG)：有已退出子进程立即回收（返回 pid），否则返回 0。
/// ABI：wait4(pid, status, options, rusage) —— options 在第 3 参数（x2）。
unsafe fn wait4_nohang(pid: i32, status: *mut i32) -> isize {
    syscall4(SYS_WAIT4, pid as usize, status as usize, 1, 0) as isize
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
        // execve 成功则不返回；走到这里即失败——打印原因再退出，
        // 否则子进程静默消失，表象是"应用/桌面起不来"而非报错。
        let r = execve(&p[..plen + 1], argv.as_ptr(), core::ptr::null());
        print("[init] execve ");
        print(name);
        print(" 失败 r=");
        print_dec(r as usize);
        print("\n");
        exit(127);
    } else if pid > 0 {
        // 成功拉起：不再打印（保持控制台简洁）；失败路径仍然可见。
    } else {
        print("[init] fork ");
        print(name);
        print(" 失败\n");
    }
    pid
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[init] KunxOS init v0.2（多进程）正在启动\n");

    unsafe {
        let net_pid = spawn_service("networkd", "/bin/networkd");
        if net_pid <= 0 {
            print("[init] FATAL: 无法启动 networkd\n");
        }
        // wifid：Wi-Fi 守护进程（/dev/wifi 事件 + 配置存储 /data/wifi/networks +
        // 状态机，见 docs/wifi.md）。与 networkd 并行，扫描/连接由内核 wifi 模块推进。
        let wifi_pid = spawn_service("wifid", "/bin/wifid");
        if wifi_pid <= 0 {
            print("[init] FATAL: 无法启动 wifid\n");
        }
        // camerad：相机服务（QEMU 软件传感器 / Pi4 硬件骨架）。先于 launcher
        // 启动，相机应用打开时命令通道已就绪。
        let cam_pid = spawn_service("camerad", "/bin/camerad");
        if cam_pid <= 0 {
            print("[init] FATAL: 无法启动 camerad\n");
        }
        // audiod：音频守护进程（焦点管理 + 多应用混音），常驻。
        // 必须在 launcher/OOBE 之前拉起，保证应用开流时服务已就绪。
        let audiod_pid = spawn_service("audiod", "/bin/audiod");
        if audiod_pid <= 0 {
            print("[init] FATAL: 无法启动 audiod\n");
        }
        // lifd：应用生命周期守护进程（状态机/后台任务/LMK）。必须先于
        // launcher 启动——应用启动时会经 launcher 向 lifd 登记。
        let lifd_pid = spawn_service("lifd", "/bin/lifd");
        if lifd_pid <= 0 {
            print("[init] FATAL: 无法启动 lifd\n");
        }
        // alarmd：闹钟守护进程（监控 /data/alarms.txt、双音铃声响铃、状态写入
        // /var/run/alarmd.state 供时钟应用叠加响铃界面；命令信箱 alarmd.ctl）。
        // 必须在 launcher 之前拉起，保证时钟应用打开时信箱已就绪。常驻。
        let alarmd_pid = spawn_service("alarmd", "/bin/alarmd");
        if alarmd_pid <= 0 {
            print("[init] FATAL: 无法启动 alarmd\n");
        }
        // sensord：传感器融合守护进程（/dev/sensor0 → Mahony 融合 + 计步 +
        // 抬腕亮屏，状态写入 /var/run/sensord.state，应用轮询）。常驻。
        let sensord_pid = spawn_service("sensord", "/bin/sensord");
        if sensord_pid <= 0 {
            print("[init] FATAL: 无法启动 sensord\n");
        }
        // rild：RIL 守护进程（电话/短信子系统）。打开 /dev/modem（QEMU 虚拟
        // 调制解调器 / Pi4 UART2），实现 AT 命令层、通话状态机、SMS PDU
        // 编解码与持久化；应用经 /var/run/rild.cmd + rild.state 交互。
        let rild_pid = spawn_service("rild", "/bin/rild");

        // pushd：本地通知队列（/data/push/in/<pid>_<seq> → lifd DELIVER）。
        // 此前被移出启动列表导致通知子系统开机不可用（lifecycle_core_test
        // 的推送链路断言全部依赖 pushd 运行）。
        let _pushd_pid = spawn_service("pushd", "/bin/pushd");
        if rild_pid <= 0 {
            print("[init] FATAL: 无法启动 rild\n");
        }
        // displayd（已废弃的合成器演示）不再拉起：launcher 直接 open/mmap /dev/fb0
        // 绘制；displayd 的 30fps demo 依赖动画定时器 TIMER2（已禁用），拉起会
        // 永久阻塞 init 的子进程回收，导致 shell 永远不出现。

        // 电话/短信测试模式：/data/modemtest.flag 存在（自动化测试构建）时跳过
        // OOBE 与 launcher，改跑 /bin/modemtest（无头自检：拨号/来电/短信/
        // 紧急呼叫全链路），结束后经 reboot(2) 关机（QEMU -no-reboot 退出）。
        if file_exists("/data/modemtest.flag") {
            print("[init] modem 测试模式：正在运行 modemtest\n");
            let test_pid = spawn_service("modemtest", "/bin/modemtest");
            if test_pid > 0 {
                let mut status = 0i32;
                let r = wait4(test_pid as i32, &mut status);
                let code = ((status as u32) >> 8) as usize;
                print("[init] modemtest (pid ");
                print_dec(test_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print(" status=");
                print_dec(code);
                print("\n");
                if r > 0 {
                    poweroff();
                }
            } else {
                print("[init] FATAL: 无法启动 modemtest\n");
            }
        }

        // SFS 并发回归模式：/sfsrace-test 标记（rootfs 根目录）存在时，跳过
        // OOBE 与 launcher，改跑 /bin/sfsrace（多进程并发创建/读写/截断/删除
        // /var/run 文件，校验 SFS v2 无 inode/块串线、txn_commit 无除零崩溃），
        // 结束后经 reboot(2) 关机（QEMU -no-reboot 退出）。
        if file_exists("/sfsrace-test") {
            print("[init] SFS 并发回归测试模式：正在运行 sfsrace\n");
            let test_pid = spawn_service("sfsrace", "/bin/sfsrace");
            if test_pid > 0 {
                let mut status = 0i32;
                let r = wait4(test_pid as i32, &mut status);
                let code = ((status as u32) >> 8) as usize;
                print("[init] sfsrace (pid ");
                print_dec(test_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print(" status=");
                print_dec(code);
                print("\n");
                if r > 0 {
                    poweroff();
                }
            } else {
                print("[init] FATAL: 无法启动 sfsrace\n");
            }
        }

        // 音频测试模式：/etc/audio-test 标记存在时，先跑多应用混音 + 焦点
        // 抢占场景与延迟测量（audiod 已在上面启动）。测试输出进串口日志，
        // 供 tools/ 的音频测试报告采集。
        if file_exists("/etc/audio-test") {
            print("[init] 音频测试模式：正在运行 audiomix\n");
            let mix_pid = spawn_service("audiomix", "/bin/audiomix");
            if mix_pid > 0 {
                let mut status = 0i32;
                let r = wait4(mix_pid as i32, &mut status);
                print("[init] audiomix (pid ");
                print_dec(mix_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print("\n");
            }
            print("[init] 音频测试模式：正在运行 audiolat 5\n");
            let lat_pid = spawn_service("audiolat", "/bin/audiolat");
            if lat_pid > 0 {
                let mut status = 0i32;
                let r = wait4(lat_pid as i32, &mut status);
                print("[init] audiolat (pid ");
                print_dec(lat_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print("\n");
            }
        }

        // OOBE 启动链：/data/oobe/.done 不存在 => 首次启动，先运行 OOBE，
        // 并阻塞等待其写 .done 后退出，再启动 launcher（避免两者同时抢画整屏 fb0）。
        if !file_exists("/data/oobe/.done") {
            print("[init] /data/oobe/.done 不存在，正在启动 OOBE\n");
            let oobe_pid = spawn_service("oobe", "/bin/oobe");
            if oobe_pid > 0 {
                let mut status = 0i32;
                let r = wait4(oobe_pid as i32, &mut status);
                let code = ((status as u32) >> 8) as usize;
                print("[init] OOBE (pid ");
                print_dec(oobe_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print(" status=");
                print_dec(code);
                print("\n");
                if code != 0 || r <= 0 {
                    // OOBE 未正常完成：.done 未写入，下次启动会重跑；
                    // 明确告警，避免误以为已完成设置。
                    print("[init] WARNING: OOBE 未完成设置(.done 缺失)\n");
                }
            } else {
                print("[init] FATAL: 无法启动 OOBE\n");
            }
        } else {
            print("[init] /data/oobe/.done 存在，跳过 OOBE\n");
        }

        let launcher_pid = spawn_service("launcher", "/bin/launcher");
        if launcher_pid <= 0 {
            print("[init] FATAL: 无法启动 launcher\n");
        }

        // 先回收一遍已退出的守护进程：pid 特定的阻塞 wait4 收不到其它子
        // 进程的退出，它们会一直挂着变僵尸（占进程槽位；wait4(-1) 才能收）。
        loop {
            let mut st = 0i32;
            let r = wait4_nohang(-1, &mut st);
            if r <= 0 {
                break;
            }
            print("[init] 已回收退出的子进程 pid=");
            print_dec(r as usize);
            print("\n");
        }

        // 主循环：等待 launcher 退出（GUI 常驻时不返回）。launcher 退出
        // （无帧缓冲 / 用户退出）后，init 退出并把 daemon（networkd/audiod）
        // 留作孤儿继续运行 —— 内核随即回退到内置 Shell，可继续交互调试
        // （如 `run /bin/audiotest ...`），audiod 仍提供焦点/混音服务。
        let mut status = 0i32;
        if launcher_pid > 0 {
            let r = wait4(launcher_pid as i32, &mut status);
            if r < 0 {
                // ECHILD(-10)：launcher 在上面的收尸循环里已被回收（如无头
                // 模式秒退），打印原始寄存器值会是天文数字
                print("[init] launcher (pid ");
                print_dec(launcher_pid as usize);
                print(") 已在此前回收（wait4 返回 ");
                print_dec(r as usize);
                print("）\n");
            } else {
                let code = ((status as u32) >> 8) as usize;
                print("[init] launcher (pid ");
                print_dec(launcher_pid as usize);
                print(") 已退出, r=");
                print_dec(r as usize);
                print(" status=");
                print_dec(code);
                print("\n");
            }
        }
    }

    print("[init] 正在退出（守护进程作为孤儿继续运行，内核 Shell 随后接管）\n");
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
