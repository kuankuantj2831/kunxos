// KunxOS camerad：相机服务（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 init 经 fork + execve 拉起（/bin/camerad），与 launcher / 相机应用并发运行。
// 职责（对应 docs/camera.md 架构图「应用 ↔ Camera Service ↔ HAL ↔ 驱动/固件」）：
//
//   1. 持有 HAL 相机后端（QEMU 下为 SimCamera 软件传感器，Pi4 下为 PiCamera 骨架）
//   2. 命令通道 /var/run/camerad.ctl：相机应用写入命令行（带请求 id），
//      camerad 轮询处理并把响应追加到 /var/run/camerad.resp（应用侧读取后清空）
//   3. 预览：PREVIEW_START 后按配置帧率把帧写入 /tmp/cam/frame_0..5.yuv（固定
//      环形文件，避免 ramdisk 上反复创建/删除），/tmp/cam/latest 记录最新帧序号
//   4. 拍照：PHOTO → 单帧采集（RGB888）→ kunxjpeg 编码 → /data/photos/photo_XXXXXX.jpg
//   5. 录像（MP4/MOV 预留，当前为自描述容器）：REC_START/REC_STOP →
//      /data/videos/rec_XXXXXX.kxv（16B 文件头 + 每帧 16B 帧头 + YUV 数据）
//   6. 3A：固定参数（FixedThreeA），3A_GET/3A_SET 读写参数
//
// 协议（文本行，`#<reqid> <命令> <参数...>`；响应 `#<reqid> OK ...` / `#<reqid> ERR ...`）：
//   LIST | OPEN <cam> | CLOSE <cam> | CONFIG <cam> <w> <h> <fps> <fmt>
//   PREVIEW_START <cam> | PREVIEW_STOP <cam>
//   PHOTO <cam> [<w> <h>] | REC_START <cam> | REC_STOP <cam>
//   fmt ∈ {raw8, yuv420, rgb888}

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]


use kunxhal::camera::{
    CameraConfig, CameraDevice, CameraDeviceInfo, CameraFacing, CaptureRequest, PixelFormat,
};
use kunxjpeg::jpeg_encode_rgb888;

// ==================== 系统调用（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_MKDIRAT: usize = 34;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_CLOCK_GETTIME: usize = 113;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const SEEK_END: usize = 2;

// ==================== 路径 ====================

const CTL_PATH: &str = "/var/run/camerad.ctl";
const RESP_PATH: &str = "/var/run/camerad.resp";
const FRAME_DIR: &str = "/tmp/cam";
const LATEST_PATH: &str = "/tmp/cam/latest";
const PHOTO_DIR: &str = "/data/photos";
const VIDEO_DIR: &str = "/data/videos";

/// 预览环形帧文件数（固定文件复用，避免 ramdisk 反复建删）。
const FRAME_RING: usize = 6;

/// 默认预览配置（QEMU 下足够流畅）。
const DEF_W: usize = 320;
const DEF_H: usize = 240;
const DEF_FPS: usize = 15;

/// 照片分辨率（RGB888）。
const PHOTO_W: usize = 640;
const PHOTO_H: usize = 480;

/// JPEG 输出缓冲（640x480 质量 85 典型 40-150KB；4MB 覆盖最坏情况）。
const JPEG_BUF: usize = 4 * 1024 * 1024;

/// 帧文件头：magic(4) + seq(4) + ts_us(8) + w(2) + h(2) + fmt(1) + pad(1) = 22B。
const FRAME_HDR: usize = 22;

// ==================== 系统调用封装 ====================

use kunxrt::syscall::syscall1;

use kunxrt::syscall::syscall2;

use kunxrt::syscall::syscall3;

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

unsafe fn exit(code: usize) -> ! {
    syscall1(SYS_EXIT, code);
    loop {}
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

/// 单调时钟（微秒）。
fn now_us() -> u64 {
    let mut tp = [0u8; 16];
    unsafe {
        syscall2(SYS_CLOCK_GETTIME, 1, tp.as_mut_ptr() as usize);
    }
    let sec = u64::from_le_bytes(tp[..8].try_into().unwrap());
    let nsec = u64::from_le_bytes(tp[8..].try_into().unwrap());
    sec * 1_000_000 + nsec / 1000
}

// ==================== 文件操作 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

unsafe fn mkdir(path: &str) -> i32 {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall2(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize) as i32
}

unsafe fn unlink(path: &str) -> i32 {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall2(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize) as i32
}

use kunxrt::syscall::syscall4;

/// renameat（信箱认领用）。
fn rename_path(old: &str, new: &str) -> isize {
    let mut o = [0u8; 64];
    let mut q = [0u8; 64];
    let on = old.len().min(63);
    o[..on].copy_from_slice(&old.as_bytes()[..on]);
    let nn = new.len().min(63);
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

unsafe fn write_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return false;
    }
    let n = syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len());
    syscall1(SYS_CLOSE, fd as usize);
    n == data.len()
}

unsafe fn append_file(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_WRONLY | O_CREAT);
    if fd < 0 {
        return false;
    }
    syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
    let n = syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize, data.len());
    syscall1(SYS_CLOSE, fd as usize);
    n == data.len()
}

/// 构造 `dir/name` 路径到 `buf`（NUL 结尾），返回路径长度。
fn build_path(buf: &mut [u8], dir: &str, name: &str) -> usize {
    let mut n = 0;
    for b in dir.as_bytes() {
        buf[n] = *b;
        n += 1;
    }
    buf[n] = b'/';
    n += 1;
    for b in name.as_bytes() {
        buf[n] = *b;
        n += 1;
    }
    n
}

/// 构造 `dir/prefixNNNNNN.suffix`（6 位零填充数字）路径，返回长度。
fn build_path_num(buf: &mut [u8], dir: &str, prefix: &str, num: u32, suffix: &str) -> usize {
    let mut n = build_path(buf, dir, prefix);
    let mut digs = [0u8; 6];
    let mut v = num;
    for i in (0..6).rev() {
        digs[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &b in &digs {
        buf[n] = b;
        n += 1;
    }
    for b in suffix.as_bytes() {
        buf[n] = *b;
        n += 1;
    }
    n
}

/// 把十进制数写进 `out`，返回写入的字节数。
fn fmt_dec(out: &mut [u8], mut v: u64) -> usize {
    let mut i = out.len();
    // i > 0 下界护栏：v 的位数超过缓冲时 i 下溢回绕（右对齐定宽缓冲的
    // 潜伏缺陷，调用点缓冲 8~12 字节）。
    while i > 0 {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.len() - i
}

// ==================== 相机状态 ====================

/// 当前采集配置（预览用；PHOTO 时临时切换后恢复）。
static mut CAM_CONFIG: CameraConfig =
    CameraConfig::new(DEF_W, DEF_H, DEF_FPS, PixelFormat::Yuv420);
/// 预览流是否运行。
static mut STREAMING: bool = false;
/// 录像流是否运行。
static mut RECORDING: bool = false;
/// 当前录像文件的 KXRV 文件头是否已写入（容器头只写一次）。
static mut REC_HDR_WRITTEN: bool = false;
/// 照片/录像文件名序号。
static mut PHOTO_N: u32 = 1;
static mut REC_N: u32 = 1;

/// 帧渲染缓冲（RGB888 640x480 = 921600B，覆盖 YUV420 情形）。
static mut FRAME_BUF: [u8; 640 * 480 * 3] = [0u8; 640 * 480 * 3];
/// JPEG 输出缓冲。
static mut JPEG_OUT: [u8; JPEG_BUF] = [0u8; JPEG_BUF];

/// 拿相机后端；无后端（非 qemu/pi4 构建）时返回 None——调用方回
/// "ERR no backend" 而不是 panic 杀掉整个守护进程（k_main 对无后端是
/// 优雅降级设计，此前第一条相机命令就把 camerad 干掉）。
fn camera_backend_opt() -> Option<&'static mut dyn CameraDevice> {
    kunxhal::camera::camera_backend()
}

/// 写入一帧到环形文件：22B 头 + 数据。
unsafe fn write_ring_frame(result: &kunxhal::camera::CaptureResult, payload: &[u8]) {
    let idx = (result.seq as usize) % FRAME_RING;
    let mut hdr = [0u8; FRAME_HDR];
    hdr[0..4].copy_from_slice(b"KXFR");
    hdr[4..8].copy_from_slice(&result.seq.to_le_bytes());
    hdr[8..16].copy_from_slice(&result.ts_us.to_le_bytes());
    hdr[16..18].copy_from_slice(&(result.width as u16).to_le_bytes());
    hdr[18..20].copy_from_slice(&(result.height as u16).to_le_bytes());
    hdr[20] = 1; // fmt: yuv420
    let mut path = [0u8; 64];
    let mut name = [0u8; 16];
    name[..6].copy_from_slice(b"frame_");
    name[6] = b'0' + idx as u8;
    name[7..11].copy_from_slice(b".yuv");
    let plen = build_path(&mut path, FRAME_DIR, core::str::from_utf8(&name[..11]).unwrap_or(""));
    path[plen] = 0;
    let fd = syscall3(
        SYS_OPENAT,
        AT_FDCWD,
        path.as_ptr() as usize,
        O_WRONLY | O_CREAT | O_TRUNC,
    ) as isize;
    if fd < 0 {
        return;
    }
    let w1 = syscall3(SYS_WRITE, fd as usize, hdr.as_ptr() as usize, hdr.len()) as isize;
    let w2 = syscall3(SYS_WRITE, fd as usize, payload.as_ptr() as usize, payload.len()) as isize;
    syscall1(SYS_CLOSE, fd as usize);
    // 写失败（如 tmpfs 空间不足）或被截断时不更新 latest，避免消费者读到空帧/半帧
    if w1 != hdr.len() as isize || w2 != payload.len() as isize {
        return;
    }
    // 更新 latest：十进制 seq
    let mut seq_buf = [0u8; 16];
    let n = fmt_dec(&mut seq_buf, result.seq as u64);
    write_file(LATEST_PATH, &seq_buf[seq_buf.len() - n..]);
}

/// 写入录像帧（自描述容器：16B 文件头由首帧写入 + 每帧 16B 帧头 + 数据）。
unsafe fn write_rec_frame(result: &kunxhal::camera::CaptureResult, payload: &[u8]) {
    let mut path = [0u8; 64];
    let mut name = [0u8; 24];
    name[..4].copy_from_slice(b"rec_");
    let mut digs = [0u8; 6];
    let mut v = REC_N;
    for i in (0..6).rev() {
        digs[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    name[4..10].copy_from_slice(&digs);
    name[10..14].copy_from_slice(b".kxv");
    let plen = build_path(&mut path, VIDEO_DIR, core::str::from_utf8(&name[..14]).unwrap_or(""));
    path[plen] = 0;
    // ★ 首帧重建容器（O_TRUNC）：REC_N 是内存静态量（重启归 1），漏
    // REC_STOP/中途崩溃后同名 rec_XXXXXX.kxv 残留 —— 无 TRUNC 时 16B 头
    // 覆写旧文件头、新帧追加旧数据尾，容器交错损坏。
    let flags = if unsafe { REC_HDR_WRITTEN } {
        O_WRONLY | O_CREAT
    } else {
        O_WRONLY | O_CREAT | O_TRUNC
    };
    let fd = syscall3(SYS_OPENAT, AT_FDCWD, path.as_ptr() as usize, flags) as isize;
    if fd < 0 {
        return;
    }
    // 文件头（仅在当前录像的首帧写入一次）
    if !REC_HDR_WRITTEN {
        let mut fh = [0u8; 16];
        fh[0..4].copy_from_slice(b"KXRV");
        fh[4..6].copy_from_slice(&(result.width as u16).to_le_bytes());
        fh[6..8].copy_from_slice(&(result.height as u16).to_le_bytes());
        fh[8..12].copy_from_slice(&(CAM_CONFIG.fps as u32).to_le_bytes());
        fh[12] = 1; // fmt: yuv420
        syscall3(SYS_WRITE, fd as usize, fh.as_ptr() as usize, fh.len());
        REC_HDR_WRITTEN = true;
    }
    // 帧头
    let mut frm = [0u8; 16];
    frm[0..4].copy_from_slice(&result.seq.to_le_bytes());
    frm[4..12].copy_from_slice(&result.ts_us.to_le_bytes());
    frm[12..14].copy_from_slice(&(result.width as u16).to_le_bytes());
    frm[14..16].copy_from_slice(&(result.height as u16).to_le_bytes());
    syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
    syscall3(SYS_WRITE, fd as usize, frm.as_ptr() as usize, frm.len());
    syscall3(SYS_WRITE, fd as usize, payload.as_ptr() as usize, payload.len());
    syscall1(SYS_CLOSE, fd as usize);
}

// ==================== 命令处理 ====================

/// 请求 id → 响应前缀 "#<id>"（静态缓冲，调用后立即使用）。
fn fmt_tag(id: u32) -> &'static str {
    static mut TAG_BUF: [u8; 12] = [0u8; 12];
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(TAG_BUF) };
    let mut i = 0;
    buf[i] = b'#';
    i += 1;
    let mut v = id;
    let mut digits = [0u8; 10]; // u32 最多 10 位（应用 REQ_ID 递增不回绕）
    let mut d = 0;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    while d > 0 {
        d -= 1;
        buf[i] = digits[d];
        i += 1;
    }
    unsafe { core::str::from_utf8(&buf[..i]) }.unwrap_or("")
}

unsafe fn respond(tag: &str, body: &str) {
    let mut line = [0u8; 192];
    let mut n = 0;
    for &b in tag.as_bytes() {
        line[n] = b;
        n += 1;
    }
    line[n] = b' ';
    n += 1;
    for &b in body.as_bytes() {
        if n >= line.len() - 1 {
            break;
        }
        line[n] = b;
        n += 1;
    }
    line[n] = b'\n';
    n += 1;
    append_file(RESP_PATH, &line[..n]);
}

/// 响应：`#id <bytes...>`（二进制/数字内容用）。
unsafe fn respond_bytes(tag: &str, body: &[u8]) {
    let mut line = [0u8; 192];
    let mut n = 0;
    for &b in tag.as_bytes() {
        line[n] = b;
        n += 1;
    }
    line[n] = b' ';
    n += 1;
    for &b in body {
        if n >= line.len() - 1 {
            break;
        }
        line[n] = b;
        n += 1;
    }
    line[n] = b'\n';
    n += 1;
    append_file(RESP_PATH, &line[..n]);
}

/// 解析命令行：`#id CMD 参数...`。
fn parse_cmd(line: &[u8]) -> Option<(u32, &[u8], &[u8])> {
    if line.len() < 3 || line[0] != b'#' {
        return None;
    }
    let mut id = 0u32;
    let mut i = 1;
    while i < line.len() && line[i].is_ascii_digit() {
        // 应用侧 REQ_ID 每命令 +1 不回绕（支持 10 位）：此处只防溢出，
        // 不能再硬拒 >999 —— 长会话第 1000 条命令起会被整体丢弃，
        // 相机应用每个操作都超时直到重启。
        match id.checked_mul(10).and_then(|v| v.checked_add((line[i] - b'0') as u32)) {
            Some(v) => id = v,
            None => return None,
        }
        i += 1;
    }
    if i >= line.len() || line[i] != b' ' {
        return None;
    }
    let rest = &line[i + 1..];
    match rest.iter().position(|&b| b == b' ') {
        Some(p) => Some((id, &rest[..p], &rest[p + 1..])),
        None => Some((id, rest, &[])),
    }
}

fn token<'a>(args: &'a [u8], idx: usize) -> Option<&'a [u8]> {
    let mut cur = 0usize;
    let mut start = 0usize;
    for (i, &b) in args.iter().enumerate() {
        if b == b' ' {
            if cur == idx {
                return Some(&args[start..i]);
            }
            cur += 1;
            start = i + 1;
        }
    }
    if cur == idx {
        Some(&args[start..])
    } else {
        None
    }
}

fn parse_u32(t: &[u8]) -> Option<u32> {
    if t.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &b in t {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(v)
}

fn parse_fmt(t: &[u8]) -> Option<PixelFormat> {
    match t {
        b"raw8" => Some(PixelFormat::RawBayer8),
        b"yuv420" => Some(PixelFormat::Yuv420),
        b"rgb888" => Some(PixelFormat::Rgb888),
        _ => None,
    }
}

fn cam_info() -> [CameraDeviceInfo; 4] {
    [CameraDeviceInfo {
        id: 0,
        name: "",
        facing: CameraFacing::Back,
        max_width: 0,
        max_height: 0,
        formats: 0,
    }; 4]
}

unsafe fn handle_cmd(line: &[u8]) {
    let Some((id, cmd, args)) = parse_cmd(line) else {
        print("[camerad] 命令格式错误\n");
        return;
    };
    let tag = fmt_tag(id);
    match cmd {
        b"LIST" => {
            let mut info = cam_info();
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            let n = cam.enumerate(&mut info);
            if n > 0 {
                let mut body = [0u8; 64];
                let l = fmt_dec(&mut body, n as u64);
                respond_bytes(tag, &body[body.len() - l..]);
            } else {
                respond(tag, "0");
            }
        }
        b"OPEN" => {
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            let r = cam.open(0);
            respond(tag, if r.is_ok() { "OK" } else { "ERR busy" });
        }
        b"CLOSE" => {
            if let Some(cam) = camera_backend_opt() { cam.close().ok(); }
            respond(tag, "OK");
        }
        b"CONFIG" => {
            match (
                token(args, 1).and_then(parse_u32),
                token(args, 2).and_then(parse_u32),
                token(args, 3).and_then(parse_u32),
                token(args, 4).and_then(parse_fmt),
            ) {
                (Some(w), Some(h), Some(fps), Some(fmt)) => {
                    let cfg = CameraConfig::new(w as usize, h as usize, fps as usize, fmt);
                    let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
                    let r = cam.configure(&cfg);
                    respond(tag, if r.is_ok() { "OK" } else { "ERR config" });
                }
                _ => respond(tag, "ERR bad args"),
            }
        }
        b"PREVIEW_START" => {
            if STREAMING {
                respond(tag, "ERR already");
                return;
            }
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            let r = cam.start_stream();
            if r.is_ok() {
                STREAMING = true;
                respond(tag, "OK");
            } else {
                respond(tag, "ERR start");
            }
        }
        b"PREVIEW_STOP" => {
            if let Some(cam) = camera_backend_opt() { cam.stop_stream().ok(); }
            STREAMING = false;
            respond(tag, "OK");
        }
        b"PHOTO" => {
            // 临时切到 RGB888 拍照分辨率 → 采集 → JPEG → 落盘 → 恢复预览配置
            let saved = CAM_CONFIG;
            let photo_cfg = CameraConfig::new(PHOTO_W, PHOTO_H, 1, PixelFormat::Rgb888);
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            if cam.configure(&photo_cfg).is_err() {
                respond(tag, "ERR config");
                return;
            }
            let req = CaptureRequest {
                camera_id: 0,
                config: photo_cfg,
                single: true,
                buffer_mask: 0,
            };
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            match cam.capture_single(&req) {
                Ok(result) => {
                    let buf = &mut *core::ptr::addr_of_mut!(FRAME_BUF);
                    match cam.read_frame(&result, buf) {
                        Ok(size) => {
                            let out = &mut *core::ptr::addr_of_mut!(JPEG_OUT);
                            match jpeg_encode_rgb888(out, PHOTO_W, PHOTO_H, &buf[..size], 85) {
                                Ok(jn) => {
                                    let mut path = [0u8; 64];
                                    let plen = build_path_num(
                                        &mut path,
                                        PHOTO_DIR,
                                        "photo_",
                                        PHOTO_N,
                                        ".jpg",
                                    );
                                    path[plen] = 0;
                                    let fd = syscall3(
                                        SYS_OPENAT,
                                        AT_FDCWD,
                                        path.as_ptr() as usize,
                                        O_WRONLY | O_CREAT | O_TRUNC,
                                    ) as isize;
                                    if fd >= 0 {
                                        syscall3(
                                            SYS_WRITE,
                                            fd as usize,
                                            out.as_ptr() as usize,
                                            jn,
                                        );
                                        syscall1(SYS_CLOSE, fd as usize);
                                        let mut body = [0u8; 64];
                                        let mut n = 0;
                                        let mut num = [0u8; 8];
                                        let nl = fmt_dec(&mut num, PHOTO_N as u64);
                                        body[n..n + nl]
                                            .copy_from_slice(&num[num.len() - nl..]);
                                        n += nl;
                                        body[n] = b' ';
                                        n += 1;
                                        let mut sz = [0u8; 12];
                                        let sl = fmt_dec(&mut sz, jn as u64);
                                        body[n..n + sl].copy_from_slice(&sz[sz.len() - sl..]);
                                        n += sl;
                                        body[n] = b' ';
                                        n += 1;
                                        let mut sq = [0u8; 12];
                                        let ql = fmt_dec(&mut sq, result.seq as u64);
                                        body[n..n + ql].copy_from_slice(&sq[sq.len() - ql..]);
                                        n += ql;
                                        respond_bytes(tag, &body[..n]);
                                        PHOTO_N += 1;
                                    } else {
                                        respond(tag, "ERR write");
                                    }
                                }
                                Err(_) => respond(tag, "ERR jpeg"),
                            }
                        }
                        Err(_) => respond(tag, "ERR frame"),
                    }
                }
                Err(_) => respond(tag, "ERR capture"),
            }
            if let Some(cam) = camera_backend_opt() {
                cam.configure(&saved).ok();
            }
        }
        b"REC_START" => {
            if RECORDING {
                respond(tag, "ERR already");
                return;
            }
            REC_HDR_WRITTEN = false;
            if !STREAMING {
                if camera_backend_opt().map_or(false, |cam| cam.start_stream().is_ok()) {
                    STREAMING = true;
                }
            }
            RECORDING = true;
            respond(tag, "OK");
        }
        b"REC_STOP" => {
            RECORDING = false;
            REC_N += 1;
            respond(tag, "OK");
        }
        b"3A_GET" => {
            let Some(cam) = camera_backend_opt() else { return respond(tag, "ERR no backend"); };
            let p = cam.get_3a();
            let mut body = [0u8; 96];
            let mut n = 0;
            for &b in b"OK ae " {
                body[n] = b;
                n += 1;
            }
            let mut t = [0u8; 12];
            let l = fmt_dec(&mut t, p.ae.exposure_us as u64);
            body[n..n + l].copy_from_slice(&t[t.len() - l..]);
            n += l;
            body[n] = b' ';
            n += 1;
            let l = fmt_dec(&mut t, p.ae.iso as u64);
            body[n..n + l].copy_from_slice(&t[t.len() - l..]);
            n += l;
            body[n] = b' ';
            n += 1;
            let l = fmt_dec(&mut t, p.awb.temp_k as u64);
            body[n..n + l].copy_from_slice(&t[t.len() - l..]);
            n += l;
            body[n] = b' ';
            n += 1;
            let l = fmt_dec(&mut t, p.af.position as u64);
            body[n..n + l].copy_from_slice(&t[t.len() - l..]);
            n += l;
            respond_bytes(tag, &body[..n]);
        }
        b"3A_SET" => {
            // 固定参数实现：接受命令（参数解析省略），行为不变
            respond(tag, "OK fixed");
        }
        _ => respond(tag, "ERR unknown"),
    }
}

/// 轮询命令文件并处理所有行。
///
/// rename 认领方案（rild/wifid/audiod 同款）：旧的「read → ftruncate 清空」
/// 两步之间，相机应用（cam_send）追加写入的新命令会被 ftruncate 一并抹掉
/// → 应用侧 cam_cmd 满超时（拍照/预览偶发卡死）。认领后新命令落在重生的
/// 命令文件上，留待下一轮。
unsafe fn poll_commands() {
    static mut BUF: [u8; 1024] = [0u8; 1024];
    // 认领名 "/var/run/camerad.ctl.r"
    let mut taken = [0u8; 32];
    let cb = CTL_PATH.as_bytes();
    taken[..cb.len()].copy_from_slice(cb);
    taken[cb.len()] = b'.';
    taken[cb.len() + 1] = b'r';
    let tpath = core::str::from_utf8(&taken[..cb.len() + 2]).unwrap_or("");
    // 清上次崩溃残留（存在则 rename 失败）
    unlink(tpath);
    if rename_path(CTL_PATH, tpath) != 0 {
        return; // 无命令文件
    }
    let mut n: isize = 0;
    let fd = open(tpath, O_RDWR);
    if fd >= 0 {
        syscall3(SYS_LSEEK, fd as usize, 0, 0);
        n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
    }
    unlink(tpath);
    // 重建空命令文件（写方按「文件常驻」假设 O_CREAT 追加）。
    let fd2 = open(CTL_PATH, O_CREAT | O_RDWR);
    if fd2 >= 0 {
        syscall1(SYS_CLOSE, fd2 as usize);
    }
    if n > 0 && (n as usize) <= BUF.len() {
        let data = &BUF[..n as usize];
        let mut start = 0usize;
        for i in 0..data.len() {
            if i == data.len() || data[i] == b'\n' {
                if i > start {
                    handle_cmd(&data[start..i]);
                }
                start = i + 1;
            }
        }
    }
}

/// 预览/录像帧投递：若流运行且有新帧，渲染并写入环形文件。
unsafe fn pump_frames() {
    if !STREAMING {
        return;
    }
    let Some(cam) = camera_backend_opt() else {
        STREAMING = false;
        return;
    };
    while let Some(result) = cam.poll_frame() {
        let buf = &mut *core::ptr::addr_of_mut!(FRAME_BUF);
        match cam.read_frame(&result, buf) {
            Ok(size) => {
                write_ring_frame(&result, &buf[..size]);
                if RECORDING {
                    write_rec_frame(&result, &buf[..size]);
                }
            }
            Err(_) => {}
        }
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[camerad] KunxOS 相机服务启动中\n");

    unsafe {
        // 目录准备
        mkdir(FRAME_DIR);
        mkdir(PHOTO_DIR);
        mkdir(VIDEO_DIR);
        let _ = unlink(LATEST_PATH);
        // 命令信箱启动即创建（O_TRUNC 清陈旧命令）：poll_commands 以
        // O_RDWR 打开，若等相机应用首写才存在，主循环每 10ms 一次
        // open 失败，串口被 `[vfs] open 失败` 刷屏。
        write_file(CTL_PATH, b"");

        // 相机后端（QEMU：SimCamera；Pi4：PiCamera 骨架）
        match kunxhal::camera::camera_backend() {
            Some(cam) => {
                print("[camerad] 后端: ");
                let mut info = cam_info();
                let n = cam.enumerate(&mut info);
                if n > 0 {
                    print(info[0].name);
                    print(" (");
                    print_dec(info[0].max_width as u64);
                    print("x");
                    print_dec(info[0].max_height as u64);
                    print(")\n");
                } else {
                    print("无\n");
                }
            }
            None => print("[camerad] WARNING: 无相机后端\n"),
        }

        // 注入时钟（SimCamera 需要真实单调时钟；Pi4 骨架无此入口）
        #[cfg(feature = "qemu")]
        kunxhal::sim_camera_set_clock(now_us);
        #[cfg(feature = "pi4")]
        let _ = now_us;

        // 打开并配置默认预览
        if camera_backend_opt().map_or(false, |cam| cam.open(0).is_ok()) {
            print("[camerad] 相机 0 已打开\n");
        }
        let cfg = CameraConfig::new(DEF_W, DEF_H, DEF_FPS, PixelFormat::Yuv420);
        if camera_backend_opt().map_or(false, |cam| cam.configure(&cfg).is_ok()) {
            CAM_CONFIG = cfg;
            print("[camerad] 已配置 ");
            print_dec(DEF_W as u64);
            print("x");
            print_dec(DEF_H as u64);
            print("@");
            print_dec(DEF_FPS as u64);
            print(" yuv420\n");
        }
    }

    // 主循环
    loop {
        unsafe {
            poll_commands();
            pump_frames();
        }
        sleep_ms(10);
    }
}

// ==================== 入口 ====================

kunxrt::entry!();

kunxrt::panic_handler!("camerad");
