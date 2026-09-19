// KunxOS sensord：传感器融合守护进程（EL0 用户态，裸 syscall、no_std）。
//
// 由 init 经 fork + execve 拉起（/bin/sensord），常驻运行。职责：
//   1. 打开 /dev/sensor0（内核 IMU 字符设备，每次 read 返回一个 40 字节样本）
//   2. 以 100Hz（游戏级）轮询原始样本，运行 Mahony 姿态融合
//      （含磁力计 → 地理旋转矢量；无磁力计 → 游戏旋转矢量）、
//      计步器（加速度阈值 + 峰值检测）、抬腕亮屏检测
//   3. 每 20ms 把最新状态写入 /var/run/sensord.state（ASCII 行，应用轮询读取）
//   4. 通过 /var/run/sensord.pipe 接受命令：STATUS / STEPS / RAISE / RESET / QUIT
//   5. 每秒轮询命令管道（STATUS / STEPS / RAISE / RESET / QUIT）
//
// 50Hz（UI）/ 100Hz（游戏）输出频率：融合在主循环以 100Hz 运行，
// 状态文件每 2 个周期（20ms = 50Hz）刷新一次，应用侧按需读取。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

use kunxhal::fusion::MahonyFilter;
use kunxhal::motion::{RaiseDetector, StepDetector};
use kunxhal::sensor::{SensorInfoC, SensorSample, SENSOR_IOCTL_GET_INFO};

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_FTRUNCATE: usize = 46;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_LSEEK: usize = 62;
const SYS_NANOSLEEP: usize = 101;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

const SENSOR_PATH: &str = "/dev/sensor0";
const STATE_PATH: &str = "/var/run/sensord.state";
const PIPE_PATH: &str = "/var/run/sensord.pipe";

/// 采样周期（毫秒）：100Hz。
const SAMPLE_MS: usize = 10;
/// 状态刷新周期（毫秒）：50Hz。
const STATE_MS: usize = 20;

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        options(nostack)
    );
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

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_dec(mut v: u64) {
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

fn print_i32(v: i64) {
    if v < 0 {
        print("-");
        print_dec((-v) as u64);
    } else {
        print_dec(v as u64);
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

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

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

unsafe fn unlink(path: &str) -> i32 {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall2(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize) as i32
}

fn rename_path(old: &str, new: &str) -> isize {
    let mut o = [0u8; 64];
    let mut q = [0u8; 64];
    let on = old.len().min(63);
    o[..on].copy_from_slice(&old.as_bytes()[..on]);
    let nn = new.len().min(63);
    q[..nn].copy_from_slice(&new.as_bytes()[..nn]);
    unsafe {
        syscall4(SYS_RENAMEAT, AT_FDCWD, o.as_ptr() as usize, AT_FDCWD, q.as_ptr() as usize)
            as isize
    }
}

/// 把一行写入文件（追加，O_TRUNC 语义：逐次覆盖）。
fn write_line(path: &str, line: &str) {
    unsafe {
        let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC);
        if fd >= 0 {
            write_fd(fd as usize, line.as_bytes());
            write_fd(fd as usize, b"\n");
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

/// 读取一个 40 字节传感器样本；成功返回 true。
fn read_sample(fd: isize, buf: &mut [u8; SensorSample::SIZE]) -> bool {
    let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
    (n as isize) == SensorSample::SIZE as isize
}

// ==================== 状态输出 ====================

/// 屏幕方向（自动旋转判定，基于重力在屏幕平面内的投影）：
/// 0=竖屏 1=横屏左 2=倒竖 3=横屏右。
fn screen_orientation(roll: f32, pitch: f32) -> u8 {
    use kunxhal::math::{cos, sin, to_rad};
    let gx = -sin(to_rad(pitch));
    let gy = sin(to_rad(roll)) * cos(to_rad(pitch));
    let ax = if gx < 0.0 { -gx } else { gx };
    let ay = if gy < 0.0 { -gy } else { gy };
    if ax > ay {
        if gx > 0.0 { 1 } else { 3 }
    } else if gy > 0.0 {
        2
    } else {
        0
    }
}

/// 把当前融合状态写入 /var/run/sensord.state（ASCII 行，整数）。
fn write_state(
    filter: &MahonyFilter,
    steps: u32,
    raises: u32,
    have_mag: bool,
) {
    let e = filter.euler();
    let h = filter.heading_deg() as i64;
    let q = filter.game_rotation_vector();
    let orient = screen_orientation(e.roll, e.pitch);
    // 定点：四元数 ×10000。
    let w = (q.w * 10000.0) as i64;
    let x = (q.x * 10000.0) as i64;
    let y = (q.y * 10000.0) as i64;
    let z = (q.z * 10000.0) as i64;

    let mut line = [0u8; 160];
    let mut i = 0usize;

    fn push(s: &mut [u8], i: &mut usize, text: &[u8]) {
        let n = text.len().min(s.len() - *i);
        s[*i..*i + n].copy_from_slice(&text[..n]);
        *i += n;
    }
    fn push_i(s: &mut [u8], i: &mut usize, v: i64) {
        let mut buf = [0u8; 20];
        let mut n = 0usize;
        if v == 0 {
            buf[0] = b'0';
            n = 1;
        } else {
            let mut v = if v < 0 { buf[0] = b'-'; n = 1; -v } else { v };
            let mut tmp = [0u8; 20];
            let mut m = 0usize;
            while v > 0 {
                tmp[m] = b'0' + (v % 10) as u8;
                m += 1;
                v /= 10;
            }
            while m > 0 {
                m -= 1;
                buf[n] = tmp[m];
                n += 1;
            }
        }
        push(s, i, &buf[..n]);
    }

    push(&mut line, &mut i, b"ORIENT");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, e.roll as i64);
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, e.pitch as i64);
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, e.yaw as i64);
    push(&mut line, &mut i, b"\nQUAT");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, w);
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, x);
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, y);
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, z);
    push(&mut line, &mut i, b"\nHEADING");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, h);
    push(&mut line, &mut i, b"\nSTEPS");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, steps as i64);
    push(&mut line, &mut i, b"\nRAISE");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, raises as i64);
    push(&mut line, &mut i, b"\nMODE");
    push(&mut line, &mut i, b" ");
    push(&mut line, &mut i, if have_mag { b"geo" } else { b"game" });
    push(&mut line, &mut i, b"\nORIENTATION");
    push(&mut line, &mut i, b" ");
    push_i(&mut line, &mut i, orient as i64);

    unsafe {
        let fd = open(STATE_PATH, O_CREAT | O_WRONLY | O_TRUNC);
        if fd >= 0 {
            write_fd(fd as usize, &line[..i]);
            syscall1(SYS_CLOSE, fd as usize);
        }
    }
}

// ==================== 命令管道 ====================

fn bytes_starts_with(a: &[u8], prefix: &[u8]) -> bool {
    a.len() >= prefix.len() && a[..prefix.len()].iter().zip(prefix).all(|(x, y)| x == y)
}

/// 处理管道命令。
fn handle_command(
    cmd: &[u8],
    steps: &mut StepDetector,
    raises: &mut RaiseDetector,
    have_mag: bool,
    quit: &mut bool,
) {
    // RESET 需要检测器内部状态（峰值基准等）：以可变借用透传，原签名只给
    // u32 拷贝、RESET 分支是空操作（打印后计数继续累加）。
    if bytes_starts_with(cmd, b"STATUS") {
        print("[sensord] STATUS: mode=");
        print(if have_mag { "geo" } else { "game" });
        print(" steps=");
        print_dec(steps.count as u64);
        print(" raises=");
        print_dec(raises.count as u64);
        print("\n");
    } else if bytes_starts_with(cmd, b"STEPS") {
        print("[sensord] steps=");
        print_dec(steps.count as u64);
        print("\n");
    } else if bytes_starts_with(cmd, b"RAISE") {
        print("[sensord] raises=");
        print_dec(raises.count as u64);
        print("\n");
    } else if bytes_starts_with(cmd, b"RESET") {
        // 真正重置检测器内部状态（峰值基准等）：此前只打印，计数继续累加。
        steps.reset();
        raises.reset();
        print("[sensord] 计步器已重置\n");
    } else if bytes_starts_with(cmd, b"QUIT") {
        print("[sensord] 收到 QUIT 请求\n");
        *quit = true;
    } else if !cmd.is_empty() {
        print("[sensord] 未知命令\n");
    }
}

/// 读取管道文件并逐行处理（普通文件模拟命名管道）。
fn poll_pipe(steps: &mut StepDetector, raises: &mut RaiseDetector, have_mag: bool, quit: &mut bool) {
    static mut BUF: [u8; 512] = [0u8; 512];
    unsafe {
        // rename 认领（rild/camerad 同款）：「read → ftruncate」两步之间写入
        // 的命令会被 ftruncate 抹掉（QUIT 丢失 → 守护进程停不下来；RESET
        // 丢失 → 计步基准不重置）。认领后新命令落在重生的管道文件上。
        let mut taken = [0u8; 32];
        let cb = PIPE_PATH.as_bytes();
        taken[..cb.len()].copy_from_slice(cb);
        taken[cb.len()] = b'.';
        taken[cb.len() + 1] = b'r';
        let tpath = core::str::from_utf8(&taken[..cb.len() + 2]).unwrap_or("");
        unlink(tpath); // 清崩溃残留
        if rename_path(PIPE_PATH, tpath) != 0 {
            return; // 无命令
        }
        let mut n: isize = 0;
        let fd = open(tpath, O_RDWR);
        if fd >= 0 {
            syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET_0);
            n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len()) as isize;
            syscall1(SYS_CLOSE, fd as usize);
        }
        unlink(tpath);
        let fd2 = open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd2 >= 0 {
            syscall1(SYS_CLOSE, fd2 as usize);
        }
        if n > 0 && (n as usize) <= BUF.len() {
            let data = &BUF[..n as usize];
            let mut start = 0usize;
            for i in 0..data.len() {
                if i == data.len() || data[i] == b'\n' {
                    if i > start {
                        handle_command(&data[start..i], steps, raises, have_mag, quit);
                    }
                    start = i + 1;
                }
            }
        }
    }
}

const SEEK_SET_0: usize = 0;

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[sensord] 正在启动\n");

    // 1. 打开 /dev/sensor0。
    let fd = unsafe { open(SENSOR_PATH, O_RDONLY) };
    if fd < 0 {
        print("[sensord] 致命错误：未找到 /dev/sensor0（内核传感器设备缺失）\n");
        unsafe { exit(1); }
    }

    // 2. 读取设备信息（量程灵敏度 / 磁力计）。
    let mut info_buf = [0u8; 256];
    let r = unsafe {
        syscall3(SYS_IOCTL, fd as usize, SENSOR_IOCTL_GET_INFO as usize, info_buf.as_mut_ptr() as usize)
    };
    let mut info = SensorInfoC::default();
    if (r as isize) >= 0 {
        if let Some(i) = SensorInfoC::from_bytes(&info_buf[..SensorInfoC::SIZE]) {
            info = i;
        }
    }
    let have_mag = info.have_mag != 0;
    print("[sensord] 传感器：source=");
    print_dec(info.source as u64);
    print(" rate=");
    print_dec(info.sample_rate as u64);
    print("Hz mag=");
    print(if have_mag { "有" } else { "无" });
    print("\n");

    // 3. 创建命令管道（普通文件模拟）。
    unsafe {
        let pfd = open(PIPE_PATH, O_CREAT | O_RDWR);
        if pfd >= 0 {
            syscall1(SYS_CLOSE, pfd as usize);
            print("[sensord] 正在监听 ");
            print(PIPE_PATH);
            print("\n");
        }
    }

    // 4. 融合 / 计步 / 抬腕状态。
    let mut filter = MahonyFilter::new();
    filter.kp = 0.5;
    filter.ki = 0.05;
    let mut steps = StepDetector::new();
    let mut raises = RaiseDetector::new();

    let mut sample = [0u8; SensorSample::SIZE];
    let mut last_ts: u64 = 0;
    let mut have_last = false;
    let mut tick = 0u64;
    let mut quit = false;

    loop {
        if !read_sample(fd, &mut sample) {
            // 设备读取失败：稍等重试。
            sleep_ms(50);
            continue;
        }
        let sm = match SensorSample::from_bytes(&sample) {
            Some(s) => s,
            None => continue,
        };
        let (accel, gyro, mag) =
            sm.to_physical(info.accel_lsb_per_g, info.gyro_lsb_per_dps);

        // dt 由样本时间戳推算（限幅防跳变）。
        let dt = if have_last && sm.ts_us > last_ts {
            let d = (sm.ts_us - last_ts) as f32 / 1_000_000.0;
            if d > 0.001 && d < 0.2 { d } else { 0.01 }
        } else {
            0.01
        };
        last_ts = sm.ts_us;
        have_last = true;

        // 融合：有磁力计 → 地理旋转矢量（航向=磁北）；否则游戏旋转矢量。
        if have_mag {
            filter.update_mag(gyro, accel, mag, dt);
        } else {
            filter.update_imu(gyro, accel, dt);
        }

        // 计步 + 抬腕。
        let _ = steps.update(accel, dt);
        let _ = raises.update(accel, gyro, dt);

        tick += 1;

        // 每 20ms 刷新状态文件（50Hz UI 频率）。
        if tick % ((STATE_MS / SAMPLE_MS) as u64) == 0 {
            write_state(&filter, steps.count, raises.count, have_mag);
        }

        // 轮询命令管道（处理 STATUS / STEPS / RAISE / RESET / QUIT）。
        if tick % 100 == 0 {
            poll_pipe(&mut steps, &mut raises, have_mag, &mut quit);
        }

        if quit {
            break;
        }
        sleep_ms(SAMPLE_MS);
    }

    print("[sensord] 正在退出\n");
    unsafe { exit(0); }
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
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
        write_fd(2, b"PANIC: sensord\n");
        if w.len > 0 {
            write_fd(2, &w.buf[..w.len]);;
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
