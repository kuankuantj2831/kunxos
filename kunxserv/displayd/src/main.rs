// KunxOS displayd：显示守护进程（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由内核 init_main 在 /init 与 networkd 结束后加载（/bin/displayd）。职责：
//   1. 打开 /dev/fb0 帧缓冲：内核暂未向用户态暴露 framebuffer 设备文件，
//      故用静态缓冲模拟显存（逻辑分辨率 540x960，与 ramfb 默认一致）
//   2. 清屏为纯黑，并用 8x8 位图字体渲染 "KunxOS" logo 与作者署名（白色加粗）
//   3. 维护最多 8 个 surface 的图层表（静态分配）
//   4. 创建 /var/run/displayd.pipe 接收命令（普通文件模拟命名管道）
//   5. 命令协议：CREATE w h / DESTROY id / FLUSH id / MOVE id x y
//   6. 主循环：轮询管道 + 30fps 定时刷新
//
// 说明：当前内核为单 EL0 用户进程模型，守护进程演示若干轮后正常退出，
// 由内核继续加载内置 Shell。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;

const SEEK_SET: usize = 0;

// ==================== 显示参数 ====================

/// 逻辑分辨率：来自 HAL（QEMU virt 后端返回 ramfb 的 540x960）。
fn display_size() -> (u32, u32) {
    match kunxhal::display_init() {
        Some(d) => (d.width, d.height),
        None => (540, 960),
    }
}

/// 模拟显存尺寸（静态缓冲，避免 540x960x4 ≈ 2MB 的浪费）。
const FB_W: usize = 128;
const FB_H: usize = 128;

/// 开机 logo 步骤配色：纯黑背景 + 纯白前景（0x00RRGGBB）。
const COLOR_BG: u32 = 0x00_00_00_00;
const COLOR_FG: u32 = 0x00_FF_FF_FF;

/// 图层表上限与单个 surface 缓冲尺寸（64x64）。
const MAX_SURFACES: usize = 8;
const SURF_PIXELS_SQRT: usize = 64;
const SURF_PIXELS: usize = SURF_PIXELS_SQRT * SURF_PIXELS_SQRT;

const PIPE_PATH: &str = "/var/run/displayd.pipe";

/// 主循环演示轮数（30fps 约 33ms/轮）。
const DEMO_ROUNDS: usize = 6;

// ==================== 8x8 位图字体（logo + 作者署名） ====================

/// logo "KunxOS" 与作者署名 "kuankuantj" 用到的字形。
static FONT_K: [u8; 8] = [0x44, 0x48, 0x50, 0x60, 0x60, 0x50, 0x48, 0x44];
static FONT_U: [u8; 8] = [0x00, 0x00, 0x44, 0x44, 0x44, 0x44, 0x44, 0x38];
static FONT_N: [u8; 8] = [0x00, 0x00, 0x64, 0x94, 0x94, 0x94, 0x94, 0x94];
static FONT_X: [u8; 8] = [0x00, 0x00, 0x88, 0x50, 0x20, 0x20, 0x50, 0x88];
static FONT_O: [u8; 8] = [0x00, 0x38, 0x44, 0x82, 0x82, 0x82, 0x44, 0x38];
static FONT_S: [u8; 8] = [0x00, 0x3C, 0x40, 0x40, 0x38, 0x04, 0x04, 0x78];
static FONT_A: [u8; 8] = [0x00, 0x00, 0x3C, 0x02, 0x3E, 0x42, 0x3E, 0x00];
static FONT_T: [u8; 8] = [0x10, 0x10, 0x7C, 0x10, 0x10, 0x10, 0x0E, 0x00];
static FONT_J: [u8; 8] = [0x04, 0x00, 0x0C, 0x04, 0x04, 0x44, 0x38, 0x00];
static FONT_SPACE: [u8; 8] = [0x00; 8];

fn glyph(c: u8) -> &'static [u8; 8] {
    match c {
        b'K' => &FONT_K,
        b'u' => &FONT_U,
        b'n' => &FONT_N,
        b'x' => &FONT_X,
        b'O' => &FONT_O,
        b'S' => &FONT_S,
        b'a' => &FONT_A,
        b't' => &FONT_T,
        b'j' => &FONT_J,
        _ => &FONT_SPACE,
    }
}

/// 在 (x, y) 绘制一个 8x8 字形。
fn draw_char(x: i32, y: i32, c: u8, color: u32) {
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..8 {
            if bits & (0x80 >> col) != 0 {
                let px = x + col as i32;
                let py = y + row as i32;
                if px >= 0 && py >= 0 && px < FB_W as i32 && py < FB_H as i32 {
                    unsafe {
                        FRAMEBUFFER[py as usize * FB_W + px as usize] = color;
                    }
                }
            }
        }
    }
}

/// 常规文本（8px 字符间距）。
fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    for &c in s {
        draw_char(cx, y, c, color);
        cx += 8;
    }
}

/// 粗体文本：每个字形横向重复绘制一次（右移 1 像素）模拟加粗。
fn draw_text_bold(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    for &c in s {
        draw_char(cx, y, c, color);
        draw_char(cx + 1, y, c, color);
        cx += 8;
    }
}

// ==================== 图层 ====================

/// 图层元数据（缓冲像素另存于平坦数组 `SURF_BUF`，以保持 ELF 中仅元数据进入 .data）。
#[derive(Clone, Copy)]
struct SurfaceMeta {
    id: i32,
    used: bool,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    alpha: u8,
}

const EMPTY_META: SurfaceMeta = SurfaceMeta {
    id: -1,
    used: false,
    x: 0,
    y: 0,
    w: 0,
    h: 0,
    alpha: 255,
};

static mut SURF_META: [SurfaceMeta; MAX_SURFACES] = [EMPTY_META; MAX_SURFACES];
/// 平坦像素缓冲：每个 surface 占 SURF_PIXELS 个 u32（零初始化，位于 .bss，不占 ELF 文件）。
static mut SURF_BUF: [u32; MAX_SURFACES * SURF_PIXELS] = [0; MAX_SURFACES * SURF_PIXELS];
/// 模拟显存（零初始化，位于 .bss）。
static mut FRAMEBUFFER: [u32; FB_W * FB_H] = [0; FB_W * FB_H];

#[inline(always)]
fn surf_buf(i: usize) -> &'static mut [u32] {
    unsafe {
        core::slice::from_raw_parts_mut(
            SURF_BUF.as_mut_ptr().add(i * SURF_PIXELS),
            SURF_PIXELS,
        )
    }
}

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

fn print_bytes(b: &[u8]) {
    unsafe {
        write_fd(1, b);
    }
}

fn print_dec(mut v: u32) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 12];
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

// ==================== 文本工具 ====================

fn bytes_starts_with(a: &[u8], prefix: &[u8]) -> bool {
    a.len() >= prefix.len() && a[..prefix.len()].iter().zip(prefix).all(|(x, y)| x == y)
}

fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    let mut any = false;
    for &c in s {
        if (b'0'..=b'9').contains(&c) {
            v = v.wrapping_mul(10).wrapping_add((c - b'0') as u32);
            any = true;
        } else {
            break;
        }
    }
    if any {
        Some(v)
    } else {
        None
    }
}

// ==================== 图层操作 ====================

fn alloc_surface() -> i32 {
    unsafe {
        for i in 0..MAX_SURFACES {
            if !SURF_META[i].used {
                SURF_META[i].used = true;
                // id 从 1 开始（0 预留给桌面/背景），与应用侧约定一致，
                // 修复「应用 FLUSH 1 / DESTROY 1 但实际 id=0」导致窗口永不合成/槽位泄漏
                SURF_META[i].id = (i + 1) as i32;
                SURF_META[i].x = 0;
                SURF_META[i].y = 0;
                SURF_META[i].w = 0;
                SURF_META[i].h = 0;
                SURF_META[i].alpha = 255;
                for p in surf_buf(i).iter_mut() {
                    *p = 0;
                }
                return (i + 1) as i32;
            }
        }
    }
    -1
}

fn find_surface(id: i32) -> Option<usize> {
    unsafe {
        for i in 0..MAX_SURFACES {
            if SURF_META[i].used && SURF_META[i].id == id {
                return Some(i);
            }
        }
    }
    None
}

fn cmd_create(w: u32, h: u32) -> i32 {
    let w = w.min(SURF_PIXELS_SQRT as u32);
    let h = h.min(SURF_PIXELS_SQRT as u32);
    let id = alloc_surface();
    if id < 0 {
        print("[displayd] CREATE failed: no free surface\n");
        return -1;
    }
    unsafe {
        // id 为 1 基（alloc_surface 返回 i+1），数组下标须回退 1，
        // 否则 w/h 写入相邻槽位、第 8 个 surface 越界写坏 SURF_BUF。
        SURF_META[(id - 1) as usize].w = w;
        SURF_META[(id - 1) as usize].h = h;
    }
    print("[displayd] CREATE ");
    print_dec(w);
    print("x");
    print_dec(h);
    print(" -> id ");
    print_dec(id as u32);
    print("\n");
    id
}

fn cmd_destroy(id: i32) {
    match find_surface(id) {
        Some(i) => {
            unsafe {
                SURF_META[i] = EMPTY_META;
            }
            print("[displayd] DESTROY ");
            print_dec(id as u32);
            print("\n");
        }
        None => print("[displayd] DESTROY: bad id\n"),
    }
}

fn cmd_move(id: i32, x: i32, y: i32) {
    match find_surface(id) {
        Some(i) => {
            unsafe {
                SURF_META[i].x = x;
                SURF_META[i].y = y;
            }
            print("[displayd] MOVE ");
            print_dec(id as u32);
            print(" -> (");
            print_dec(x as u32);
            print(",");
            print_dec(y as u32);
            print(")\n");
        }
        None => print("[displayd] MOVE: bad id\n"),
    }
}

/// 把 surface 合成到 framebuffer（按 Z 序即图层顺序，简单覆盖写）。
fn cmd_flush(id: i32) {
    match find_surface(id) {
        Some(i) => {
            let (sx, sy, w, h) = unsafe {
                (
                    SURF_META[i].x,
                    SURF_META[i].y,
                    SURF_META[i].w,
                    SURF_META[i].h,
                )
            };
            for py in 0..h {
                for px in 0..w {
                    let fx = sx + px as i32;
                    let fy = sy + py as i32;
                    if fx < 0 || fy < 0 || fx >= FB_W as i32 || fy >= FB_H as i32 {
                        continue;
                    }
                    let src = surf_buf(i)[py as usize * SURF_PIXELS_SQRT + px as usize];
                    unsafe {
                        FRAMEBUFFER[fy as usize * FB_W + fx as usize] = src;
                    }
                }
            }
            print("[displayd] FLUSH ");
            print_dec(id as u32);
            print(" composited to framebuffer\n");
        }
        None => print("[displayd] FLUSH: bad id\n"),
    }
}

// ==================== 渲染 ====================

/// 清屏为纯黑，并渲染 "KunxOS" logo 与其下方作者署名（白色加粗、水平居中）。
fn render_logo() {
    unsafe {
        for p in FRAMEBUFFER.iter_mut() {
            *p = COLOR_BG;
        }
    }
    let logo: &[u8] = b"KunxOS";
    let author: &[u8] = b"kuankuantj";
    let lw = (logo.len() as i32) * 8;
    let aw = (author.len() as i32) * 8;
    let lx = ((FB_W as i32) - lw) / 2;
    let ax = ((FB_W as i32) - aw) / 2;
    let ly = 50;
    let ay = 68;
    draw_text_bold(lx, ly, logo, COLOR_FG);
    draw_text_bold(ax, ay, author, COLOR_FG);
    // 统计点亮像素数（读取 framebuffer，保证渲染结果被真实使用）。
    let mut lit = 0usize;
    unsafe {
        for &p in FRAMEBUFFER.iter() {
            if p != COLOR_BG {
                lit += 1;
            }
        }
    }
    print("[displayd] logo pixels lit: ");
    print_dec(lit as u32);
    print("\n");
}

// ==================== 命令分发 ====================

fn dispatch(line: &[u8]) {
    // 提取第一个词（命令名）
    let mut i = 0usize;
    while i < line.len() && is_space(line[i]) {
        i += 1;
    }
    let wstart = i;
    while i < line.len() && !is_space(line[i]) {
        i += 1;
    }
    let cmd = &line[wstart..i];

    // 收集后续数字参数
    let mut args = [0u32; 4];
    let mut argc = 0usize;
    while i < line.len() && argc < 4 {
        while i < line.len() && is_space(line[i]) {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let astart = i;
        while i < line.len() && !is_space(line[i]) {
            i += 1;
        }
        if let Some(v) = parse_u32(&line[astart..i]) {
            args[argc] = v;
            argc += 1;
        }
    }

    if bytes_starts_with(cmd, b"CREATE") {
        if argc >= 2 {
            cmd_create(args[0], args[1]);
        } else {
            print("[displayd] CREATE: usage CREATE w h\n");
        }
    } else if bytes_starts_with(cmd, b"DESTROY") {
        if argc >= 1 {
            cmd_destroy(args[0] as i32);
        }
    } else if bytes_starts_with(cmd, b"FLUSH") {
        if argc >= 1 {
            cmd_flush(args[0] as i32);
        }
    } else if bytes_starts_with(cmd, b"MOVE") {
        if argc >= 3 {
            cmd_move(args[0] as i32, args[1] as i32, args[2] as i32);
        }
    } else if !cmd.is_empty() {
        print("[displayd] unknown command\n");
    }
}

fn poll_pipe() {
    static mut BUF: [u8; 256] = [0u8; 256];
    unsafe {
        let fd = open(PIPE_PATH, O_RDWR);
        if fd < 0 {
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        let n = syscall3(SYS_READ, fd as usize, BUF.as_mut_ptr() as usize, BUF.len());
        // 清空管道文件：避免同一命令每轮重复执行、文件无限增长、
        // 以及 >256 字节后新命令永远读不到（真 FIFO 语义）。
        // 注意：ftruncate 必须显式传 length=0（x1 寄存器残留值不可依赖，
        // 否则内核按「length != 0」拒绝，管道文件从不被清空）。
        syscall2(SYS_FTRUNCATE, fd as usize, 0);
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_SET);
        if n > 0 && n <= BUF.len() {
            let data = &BUF[..n];
            let mut start = 0usize;
            for i in 0..=n {
                if i == n || data[i] == b'\n' {
                    if i > start {
                        dispatch(&data[start..i]);
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
    let (w, h) = display_size();
    print("[displayd] display ");
    print_dec(w);
    print("x");
    print_dec(h);
    print(", framebuffer mapped\n");

    // 清屏 + 渲染 logo
    render_logo();
    print("[displayd] KunxOS logo rendered\n");

    // 创建命令管道
    unsafe {
        let fd = open(PIPE_PATH, O_CREAT | O_RDWR);
        if fd >= 0 {
            syscall1(SYS_CLOSE, fd as usize);
            print("[displayd] listening on ");
            print(PIPE_PATH);
            print("\n");
        }
    }

    // 自检：走一遍 CREATE -> FLUSH -> MOVE -> DESTROY 协议
    let id = cmd_create(48, 16);
    if id >= 0 {
        cmd_flush(id);
        cmd_move(id, 20, 20);
        cmd_destroy(id);
    }

    // 主循环：轮询管道 + 30fps 刷新
    let mut frame = 0usize;
    for _ in 0..DEMO_ROUNDS {
        frame += 1;
        poll_pipe();
        print("[displayd] refresh frame ");
        print_dec(frame as u32);
        print(" @30fps\n");
        sleep_ms(33);
    }

    print("[displayd] exiting (single-EL0 demo)\n");
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

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: displayd\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
