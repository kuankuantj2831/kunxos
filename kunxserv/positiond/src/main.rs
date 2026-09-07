// KunxOS positiond：定位守护进程（EL0 用户态，裸 syscall、no_std）。
//
// 由 init 经 fork + execve 拉起（/bin/positiond），常驻运行。职责：
//   1. 维护一个 NMEA 0183 数据源：
//      - QEMU：内置**模拟 GPS**——按行走路线（北京出发，1.2 m/s）逐秒生成
//        GGA + RMC 语句（含校验和），经 kunxhal::gps::GpsParser 解析，
//        完整走通「NMEA 生成 → 解析 → WGS84 → 状态输出」链路；
//      - Pi4：真实 GNSS 模块（UART2 /dev/ttyAMA1，如 Quectel L76 等）
//        接线为预留路径（本服务未接串口读取，见 docs/sensors.md）。
//   2. 每秒把最新定位写入 /var/run/positiond.state（ASCII 行，应用轮询）：
//      `FIX <lat×100000> <lon×100000> <alt_cm> <speed×10> <course×10> <sats> <seq>`
//      `UTC <hh> <mm> <ss>`；无定位时为 `NOFIX`。
//   3. 通过 /var/run/positiond.pipe 接受命令：STATUS / SIM <on|off> / QUIT。
//
// A-GPS 预留：kunxhal::gps::{A_GPS_RESERVED, ecef_from_lla} 已就绪，
// 网络辅助（星历注入 / 粗定位种子）未接，见 docs/sensors.md。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

use kunxhal::gps::{GpsFix, GpsParser};
use kunxhal::math::{cos, sin};

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_FTRUNCATE: usize = 46;
const SYS_LSEEK: usize = 62;
const SYS_NANOSLEEP: usize = 101;
const SYS_EXIT: usize = 93;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

const STATE_PATH: &str = "/var/run/positiond.state";
const PIPE_PATH: &str = "/var/run/positiond.pipe";

/// 模拟 GPS 起点：北京天安门（WGS84）。
const HOME_LAT: f32 = 39.9042;
const HOME_LON: f32 = 116.4074;
/// 行走速度（m/s）。
const WALK_SPEED: f32 = 1.2;

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

// ==================== 数字格式化 ====================

/// 追加 `v`（整数）到缓冲。
fn push_i(buf: &mut [u8], i: &mut usize, v: i64) {
    let mut tmp = [0u8; 24];
    let mut n = 0usize;
    let neg = v < 0;
    let mut v = if neg { -v } else { v };
    if v == 0 {
        tmp[0] = b'0';
        n = 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
    }
    if neg {
        buf[*i] = b'-';
        *i += 1;
    }
    while n > 0 {
        n -= 1;
        buf[*i] = tmp[n];
        *i += 1;
    }
}

/// 追加 `v`（保留 `dec` 位小数）到缓冲。
fn push_f(buf: &mut [u8], i: &mut usize, v: f32, dec: usize) {
    let mut scale = 1.0f32;
    for _ in 0..dec {
        scale *= 10.0;
    }
    let neg = v < 0.0;
    let av = if neg { -v } else { v };
    let scaled = (av * scale + 0.5) as i64;
    let int = scaled / scale as i64;
    let frac = scaled % scale as i64;
    if neg {
        buf[*i] = b'-';
        *i += 1;
    }
    push_i(buf, i, int);
    if dec > 0 {
        buf[*i] = b'.';
        *i += 1;
        // frac 的十进制数字（个位在前）。
        let mut fbuf = [0u8; 12];
        let mut n = 0usize;
        let mut f = frac;
        if f == 0 {
            fbuf[0] = b'0';
            n = 1;
        }
        while f > 0 {
            fbuf[n] = b'0' + (f % 10) as u8;
            n += 1;
            f /= 10;
        }
        // 从左到右输出：第 k 位（0 基）的权值 = 10^(dec-1-k)，对应 fbuf[p]。
        for k in 0..dec {
            let p = dec - 1 - k;
            buf[*i] = if p < n { fbuf[p] } else { b'0' };
            *i += 1;
        }
    }
}

/// 追加 `v` 并左补零到 `width` 位。
fn push_pad(buf: &mut [u8], i: &mut usize, v: i64, width: usize) {
    let mut tmp = [0u8; 12];
    let mut n = 0usize;
    let mut x = v;
    if x == 0 {
        tmp[0] = b'0';
        n = 1;
    }
    while x > 0 {
        tmp[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
    }
    while n < width && n < tmp.len() {
        tmp[n] = b'0';
        n += 1;
    }
    while n > 0 {
        n -= 1;
        buf[*i] = tmp[n];
        *i += 1;
    }
}

// ==================== NMEA 语句生成（模拟 GPS） ====================

/// 计算 NMEA 校验和（body 异或）。
fn nmea_checksum(body: &[u8]) -> u8 {
    let mut x = 0u8;
    for &c in body {
        x ^= c;
    }
    x
}

/// 追加带校验和的 NMEA 语句（`$body*HH`）到缓冲。
fn push_nmea(buf: &mut [u8], i: &mut usize, body: &[u8]) {
    buf[*i] = b'$';
    *i += 1;
    let n = body.len().min(buf.len() - *i - 3);
    buf[*i..*i + n].copy_from_slice(&body[..n]);
    *i += n;
    let ck = nmea_checksum(&body[..n]);
    buf[*i] = b'*';
    *i += 1;
    let hex = b"0123456789ABCDEF";
    buf[*i] = hex[(ck >> 4) as usize];
    *i += 1;
    buf[*i] = hex[(ck & 0xF) as usize];
    *i += 1;
    buf[*i] = b'\n';
    *i += 1;
}

/// 十进制度 → (ddmm 整数部分, mmmm 分钟小数, 南/西标志)。
fn to_nmea_coord(deg: f32) -> (i64, u32, bool) {
    let mut d = if deg < 0.0 { -deg } else { deg };
    let dd = d as i64;
    let minutes = (d - dd as f32) * 60.0;
    let mm = minutes as i64;
    let mmmm = ((minutes - mm as f32) * 10000.0 + 0.5) as u32;
    (dd * 100 + mm, mmmm, deg < 0.0)
}

/// 追加 NMEA 坐标字段（lat 宽度 4，lon 宽度 5；含符号与逗号）。
fn push_coord(buf: &mut [u8], i: &mut usize, deg: f32, is_lat: bool) {
    let (v, frac, neg) = to_nmea_coord(deg);
    push_pad(buf, i, v, if is_lat { 4 } else { 5 });
    buf[*i] = b'.';
    *i += 1;
    push_pad(buf, i, frac as i64, 4);
    buf[*i] = b',';
    *i += 1;
    buf[*i] = if neg {
        if is_lat { b'S' } else { b'W' }
    } else {
        if is_lat { b'N' } else { b'E' }
    };
    *i += 1;
    buf[*i] = b',';
    *i += 1;
}

/// 生成 GGA 语句（hhmmss, lat, lon, quality, sats, alt）。
fn build_gga(
    buf: &mut [u8],
    i: &mut usize,
    utc: u32,
    lat: f32,
    lon: f32,
    alt_m: f32,
    sats: u32,
) {
    let mut body = [0u8; 96];
    let mut b = 0usize;
    // 手写：GPGGA,hhmmss,llll.llll,N,yyyyy.yyyyy,E,1,sats,hdop,alt,M,0.0,M,,
    push_bytes(&mut body, &mut b, b"GPGGA,");
    push_utc(&mut body, &mut b, utc);
    push_bytes(&mut body, &mut b, b",");
    push_coord(&mut body, &mut b, lat, true);
    push_coord(&mut body, &mut b, lon, false);
    push_bytes(&mut body, &mut b, b"1,");
    push_i(&mut body, &mut b, sats as i64);
    push_bytes(&mut body, &mut b, b",0.9,");
    push_f(&mut body, &mut b, alt_m, 1);
    push_bytes(&mut body, &mut b, b",M,0.0,M,,");
    push_nmea(buf, i, &body[..b]);
}

/// 生成 RMC 语句（hhmmss,A,lat,lon,speed_knots,course,ddmmyy）。
fn build_rmc(
    buf: &mut [u8],
    i: &mut usize,
    utc: u32,
    date: u32,
    lat: f32,
    lon: f32,
    speed_kmh: f32,
    course: f32,
) {
    let mut body = [0u8; 96];
    let mut b = 0usize;
    push_bytes(&mut body, &mut b, b"GPRMC,");
    push_utc(&mut body, &mut b, utc);
    push_bytes(&mut body, &mut b, b",A,");
    push_coord(&mut body, &mut b, lat, true);
    push_coord(&mut body, &mut b, lon, false);
    push_f(&mut body, &mut b, speed_kmh / 1.852, 1); // km/h → 节
    push_bytes(&mut body, &mut b, b",");
    push_f(&mut body, &mut b, course, 1);
    push_bytes(&mut body, &mut b, b",");
    push_pad(&mut body, &mut b, date as i64, 6);
    push_bytes(&mut body, &mut b, b",,,A");
    push_nmea(buf, i, &body[..b]);
}

fn push_bytes(buf: &mut [u8], i: &mut usize, s: &[u8]) {
    let n = s.len().min(buf.len() - *i);
    buf[*i..*i + n].copy_from_slice(&s[..n]);
    *i += n;
}

/// UTC 秒数（自 00:00:00）→ 6 位 hhmmss。
fn push_utc(buf: &mut [u8], i: &mut usize, secs: u32) {
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    push_2(buf, i, h);
    push_2(buf, i, m);
    push_2(buf, i, s);
}

fn push_2(buf: &mut [u8], i: &mut usize, v: u32) {
    buf[*i] = b'0' + (v / 10) as u8;
    *i += 1;
    buf[*i] = b'0' + (v % 10) as u8;
    *i += 1;
}

// ==================== 模拟路线 ====================

/// 更新模拟位置：绕北京中心走矩形路线（东→北→西→南）。
fn advance_route(lat: &mut f32, lon: &mut f32, course: &mut f32, t: u32) {
    // 每 25s 转向一次（90°）。
    let phase = t / 25;
    *course = match phase % 4 {
        0 => 90.0,  // 东
        1 => 0.0,   // 北
        2 => 270.0, // 西
        _ => 180.0, // 南
    };
    let rad = *course * core::f32::consts::PI / 180.0;
    let dlat = WALK_SPEED * cos(rad) / 111_320.0;
    let dlon = WALK_SPEED * sin(rad) / (111_320.0 * cos(*lat));
    *lat += dlat;
    *lon += dlon;
}

// ==================== 状态输出 ====================

fn write_state(fix: Option<&GpsFix>) {
    let mut line = [0u8; 128];
    let mut i = 0usize;
    match fix {
        Some(f) => {
            push_bytes(&mut line, &mut i, b"FIX ");
            push_i(&mut line, &mut i, (f.lat_deg * 100000.0) as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, (f.lon_deg * 100000.0) as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, (f.alt_m * 100.0) as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, (f.speed_kmh * 10.0) as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, (f.course_deg * 10.0) as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, f.num_sats as i64);
            push_bytes(&mut line, &mut i, b" ");
            push_i(&mut line, &mut i, f.seq as i64);
            push_bytes(&mut line, &mut i, b"\nUTC ");
            push_2(&mut line, &mut i, f.utc.0);
            push_bytes(&mut line, &mut i, b" ");
            push_2(&mut line, &mut i, f.utc.1);
            push_bytes(&mut line, &mut i, b" ");
            push_2(&mut line, &mut i, f.utc.2);
        }
        None => {
            push_bytes(&mut line, &mut i, b"NOFIX");
        }
    }
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

fn handle_command(cmd: &[u8], fix: Option<&GpsFix>, sim: &mut bool, quit: &mut bool) {
    if bytes_starts_with(cmd, b"STATUS") {
        print("[positiond] STATUS: ");
        match fix {
            Some(f) => {
                print("fix lat=");
                print_i32((f.lat_deg * 100000.0) as i64);
                print(" lon=");
                print_i32((f.lon_deg * 100000.0) as i64);
                print(" sats=");
                print_dec(f.num_sats as u64);
                print(" seq=");
                print_dec(f.seq as u64);
            }
            None => print("无定位"),
        }
        print("\n");
    } else if bytes_starts_with(cmd, b"SIM on") {
        *sim = true;
        print("[positiond] 模拟定位已开启\n");
    } else if bytes_starts_with(cmd, b"SIM off") {
        *sim = false;
        print("[positiond] 模拟定位已关闭\n");
    } else if bytes_starts_with(cmd, b"QUIT") {
        print("[positiond] 已收到 QUIT 请求\n");
        *quit = true;
    } else if !cmd.is_empty() {
        print("[positiond] 未知命令\n");
    }
}

fn poll_pipe(fix: Option<&GpsFix>, sim: &mut bool, quit: &mut bool) {
    static mut BUF: [u8; 256] = [0u8; 256];
    static mut WARNED: bool = false;
    unsafe {
        let fd = open(PIPE_PATH, O_RDWR);
        if fd < 0 {
            if !WARNED {
                print("[positiond] WARNING: 无法打开管道 ");
                print(PIPE_PATH);
                print("\n");
                WARNED = true;
            }
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, 0);
        let n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len());
        syscall2(SYS_FTRUNCATE, fd as usize, 0);
        syscall3(SYS_LSEEK, fd as usize, 0, 0);
        if n > 0 && n <= BUF.len() {
            let data = &BUF[..n];
            let mut start = 0usize;
            for i in 0..=n {
                if i == n || data[i] == b'\n' {
                    if i > start {
                        handle_command(&data[start..i], fix, sim, quit);
                    }
                    start = i + 1;
                }
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[positiond] 启动 (NMEA 解析器 + QEMU 模拟 GPS)\n");

    // 创建命令管道。
    unsafe {
        let pfd = open(PIPE_PATH, O_CREAT | O_RDWR);
        if pfd >= 0 {
            syscall1(SYS_CLOSE, pfd as usize);
            print("[positiond] 正在监听 ");
            print(PIPE_PATH);
            print("\n");
        }
    }

    let mut parser = GpsParser::new();
    let mut sim = true;
    let mut lat = HOME_LAT;
    let mut lon = HOME_LON;
    let mut course = 90.0f32;
    let mut utc = 8 * 3600 + 12 * 60; // 08:12:00
    let mut date = 150624u32; // 2024-06-15
    let mut t = 0u32;
    let mut quit = false;

    loop {
        let mut buf = [0u8; 256];
        let mut i = 0usize;
        let mut fix: Option<GpsFix> = None;

        if sim {
            // 1. 推进模拟路线。
            advance_route(&mut lat, &mut lon, &mut course, t);
            // 2. 生成 GGA + RMC（含校验和）。
            build_gga(&mut buf, &mut i, utc, lat, lon, 54.6, 8);
            build_rmc(
                &mut buf,
                &mut i,
                utc,
                date,
                lat,
                lon,
                WALK_SPEED * 3.6,
                course,
            );
            // 3. 经 NMEA 解析器（完整解析路径）。
            fix = parser.feed(&buf[..i]);
        }

        // 4. 写状态文件。
        write_state(fix.as_ref());

        // 5. 每秒轮询命令。
        if t % 10 == 0 {
            poll_pipe(fix.as_ref(), &mut sim, &mut quit);
        }

        utc = (utc + 1) % 86400;
        date = date.wrapping_add(1);
        t += 1;

        if quit {
            break;
        }
        sleep_ms(1000);
    }

    print("[positiond] 正在退出\n");
    unsafe { exit(0); }
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
        write_fd(2, b"PANIC: positiond\n");
        if w.len > 0 {
            write_fd(2, &w.buf[..w.len]);;
        }
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
