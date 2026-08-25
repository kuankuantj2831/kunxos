// KunxOS 应用商店（Store，骨架）：no_std 裸 syscall 用户态程序。
//
// 骨架职责：
//   1. 读取 /var/lib/pkg/available 并统计可用包数量（软件目录）
//   2. 连接 displayd -> 创建窗口 surface -> 绘制紫色色块
//   3. 3 秒后销毁 surface 并退出，供 launcher 验证窗口回收
//
// 完整功能（安装/卸载/搜索）由包管理器 CLI（/bin/pkg）提供；本程序作为
// 桌面入口的 GUI 骨架，验证「点击应用商店图标 -> 启动 /bin/store」链路。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// 共享 CJK 16x16 点阵字库（GB2312 一级，由 tools/gen_cjk_font.py 生成）。
#[path = "../../../shared/cjk_font_store.rs"]
mod cjk_font;

const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_READ: usize = 63;
const SYS_LSEEK: usize = 62;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;
const SYS_IOCTL: usize = 29;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_NONBLOCK: usize = 0x800;
const SEEK_END: usize = 2;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const FBIOGET_VSCREENINFO: usize = 0x4600;
const FB_PATH: &str = "/dev/fb0";
const INPUT_PATH: &str = "/dev/input/event0";

const PIPE_PATH: &str = "/var/run/displayd.pipe";
const AVAILABLE_PATH: &str = "/var/lib/pkg/available";
const APP_NAME: &str = "store";
/// 窗口色块颜色（紫色）。
const COLOR: u32 = 0x00_7D_3C_98;
const WIN_W: u32 = 240;
const WIN_H: u32 = 320;
/// 假定 surface id（desktop=0，首个应用窗口=1）。
const SURFACE_ID: i32 = 1;
const LIFETIME_MS: usize = 3000;

/// 返回键（左上角）。
const BACK_X: i32 = 4;
const BACK_Y: i32 = 4;
const BACK_SZ: i32 = 36;

// linux input_event 事件类型 / 码。
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a; // 330
const BTN_LEFT: u16 = 0x110; // 272

/// 鼠标指针位图（9x9，'1' 为实心像素）：经典箭头，黑描边 + 白填充。
static CURSOR_BITS: [u16; 9] = [
    0b100000000,
    0b110000000,
    0b101000000,
    0b100100000,
    0b100010000,
    0b100001000,
    0b111111100,
    0b100010100,
    0b100010010,
];

/// 色块帧缓冲（64x64）。
const BLOCK: usize = 64;
static mut FRAMEBUFFER: [u32; BLOCK * BLOCK] = [0; BLOCK * BLOCK];

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
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, in("x4") a5, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret,
        in("x1") a2, in("x2") a3, in("x3") a4, in("x4") a5, in("x5") a6, options(nostack));
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

// ==================== 文件操作与 displayd 协议 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

fn pipe_send(cmd: &[u8]) {
    unsafe {
        let fd = open(PIPE_PATH, O_RDWR);
        if fd < 0 {
            return;
        }
        syscall3(SYS_LSEEK, fd as usize, 0, SEEK_END);
        write_fd(fd as usize, cmd);
        syscall1(SYS_CLOSE, fd as usize);
    }
}

fn push_dec(buf: &mut [u8], pos: usize, mut v: u32) -> usize {
    if v == 0 {
        buf[pos] = b'0';
        return pos + 1;
    }
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    buf[pos..pos + n].copy_from_slice(&tmp[i..]);
    pos + n
}

fn surf_create(w: u32, h: u32) {
    let mut buf = [0u8; 64];
    let mut n = 0usize;
    for &c in b"CREATE " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, w);
    buf[n] = b' ';
    n += 1;
    n = push_dec(&mut buf, n, h);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

fn surf_flush(id: i32) {
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    for &c in b"FLUSH " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, id as u32);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

fn surf_destroy(id: i32) {
    let mut buf = [0u8; 32];
    let mut n = 0usize;
    for &c in b"DESTROY " {
        buf[n] = c;
        n += 1;
    }
    n = push_dec(&mut buf, n, id as u32);
    buf[n] = b'\n';
    n += 1;
    pipe_send(&buf[..n]);
}

// ==================== 软件目录 ====================

static mut INDEX_BUF: [u8; 8192] = [0; 8192];

/// 读取可用索引并统计包数量。
fn count_packages() -> u32 {
    unsafe {
        let fd = open(AVAILABLE_PATH, O_RDONLY);
        if fd < 0 {
            return 0;
        }
        let mut total = 0usize;
        loop {
            let space = INDEX_BUF.len() - total;
            if space == 0 {
                break;
            }
            let chunk = space.min(4096);
            let n = syscall3(SYS_READ, fd as usize, INDEX_BUF.as_mut_ptr() as usize + total, chunk)
                as isize;
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);

        // 统计 "Package:" 行
        let mut count = 0u32;
        let mut i = 0usize;
        while i < total {
            if INDEX_BUF[i] == b'P' && total - i >= 8 && &INDEX_BUF[i..i + 8] == b"Package:" {
                count += 1;
            }
            while i < total && INDEX_BUF[i] != b'\n' {
                i += 1;
            }
            if i < total {
                i += 1;
            }
        }
        count
    }
}

// ==================== 绘制 ====================

fn draw_block() {
    unsafe {
        for p in FRAMEBUFFER.iter_mut() {
            *p = COLOR;
        }
    }
    let mut lit = 0usize;
    unsafe {
        for &p in FRAMEBUFFER.iter() {
            if p != 0 {
                lit += 1;
            }
        }
    }
    print("[");
    print(APP_NAME);
    print("] color block ");
    print_dec(lit as u32);
    print(" px\n");
}

// ==================== 目录界面 ====================

static mut STORE_FB: *mut u32 = core::ptr::null_mut();
static mut FB_STRIDE_PX: usize = 540;
static mut BACK_BUF: [u32; 540 * 960] = [0u32; 540 * 960];
static mut STORE_STATUS: [u8; 64] = [0; 64];
static mut STORE_STATUS_LEN: usize = 0;

/// 鼠标指针状态。
static mut CUR_X: i32 = 270;
static mut CUR_Y: i32 = 480;
static mut TOUCH_DOWN: bool = false;
static mut MOUSE_DOWN: bool = false;

fn status(s: &[u8]) { unsafe { STORE_STATUS_LEN = s.len().min(STORE_STATUS.len()); STORE_STATUS[..STORE_STATUS_LEN].copy_from_slice(&s[..STORE_STATUS_LEN]); } }
fn store_pixel(x: usize, y: usize, c: u32) { unsafe { if x < 540 && y < 960 { BACK_BUF[y * 540 + x] = c; } } }
fn store_fill(x: usize, y: usize, w: usize, h: usize, c: u32) { for yy in y..(y+h).min(960) { for xx in x..(x+w).min(540) { store_pixel(xx, yy, c); } } }
fn store_flip() {
    unsafe {
        if STORE_FB.is_null() {
            return;
        }
        core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), STORE_FB, 540 * 960);
    }
}
fn store_draw_rect(x: i32, y: i32, w: i32, h: i32, c: u32) {
    for dx in 0..w {
        store_pixel((x + dx) as usize, y as usize, c);
        store_pixel((x + dx) as usize, (y + h - 1) as usize, c);
    }
    for dy in 0..h {
        store_pixel(x as usize, (y + dy) as usize, c);
        store_pixel((x + w - 1) as usize, (y + dy) as usize, c);
    }
}
/// 鼠标指针：先画黑色描边（8 邻域），再画白色实心。
fn store_cursor() {
    let (x, y) = unsafe { (CUR_X, CUR_Y) };
    const OUTLINE: [(i32, i32); 8] =
        [(-1, -1), (1, -1), (-1, 1), (1, 1), (0, -1), (0, 1), (-1, 0), (1, 0)];
    for (ox, oy) in OUTLINE {
        store_cursor_bits(x + ox, y + oy, 0x00_00_00_00);
    }
    store_cursor_bits(x, y, 0x00_FF_FF_FF);
}
fn store_cursor_bits(x: i32, y: i32, color: u32) {
    for (row, bits) in CURSOR_BITS.iter().enumerate() {
        for col in 0..9 {
            if bits & (0x100 >> col) != 0 {
                store_pixel((x + col as i32) as usize, (y + row as i32) as usize, color);
            }
        }
    }
}
fn store_glyph(c: u8) -> [u8; 5] { match c { b'A' => [0x1E,0x05,0x1F,0x11,0x11], b'C' => [0x0F,0x10,0x10,0x10,0x0F], b'E' => [0x1F,0x10,0x1E,0x10,0x1F], b'I' => [0x1F,0x04,0x04,0x04,0x1F], b'L' => [0x10,0x10,0x10,0x10,0x1F], b'N' => [0x11,0x19,0x15,0x13,0x11], b'O' => [0x0E,0x11,0x11,0x11,0x0E], b'R' => [0x1E,0x11,0x1E,0x14,0x12], b'T' => [0x1F,0x04,0x04,0x04,0x04], b'K' => [0x11,0x12,0x1C,0x12,0x11], b'U' => [0x11,0x11,0x11,0x11,0x0E], b'X' => [0x11,0x0A,0x04,0x0A,0x11], b'0' => [0x0E,0x13,0x15,0x19,0x0E], b'1' => [0x04,0x0C,0x04,0x04,0x0E], b'2' => [0x1E,0x01,0x06,0x18,0x1F], b'3' => [0x1E,0x01,0x06,0x01,0x1E], b' ' => [0;5], _ => [0;5] } }
fn store_cjk(x: usize, y: usize, idx: usize, c: u32) {
    let base = idx * 32;
    for row in 0..16usize {
        let b0 = cjk_font::CJK_BITMAPS[base + row * 2];
        let b1 = cjk_font::CJK_BITMAPS[base + row * 2 + 1];
        let bits = ((b0 as u16) << 8) | b1 as u16;
        for col in 0..16usize {
            if bits & (0x8000u16 >> col) != 0 {
                store_fill(x + col, y + row, 1, 1, c);
            }
        }
    }
}
fn store_text(x: usize, y: usize, s: &[u8], c: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = cjk_font::utf8_next(s, i);
        if cp >= 0x80 {
            if let Some(idx) = cjk_font::cjk_index(cp as u16) {
                store_cjk(cx, y, idx, c);
            }
            cx += 16;
        } else {
            let g = store_glyph(cp as u8);
            for (ry, b) in g.iter().enumerate() {
                for rx in 0..5 {
                    if b & (1 << (4 - rx)) != 0 {
                        store_fill(cx + rx, y + ry, 1, 1, c);
                    }
                }
            }
            cx += 8;
        }
        i += n;
    }
}
fn store_render() { unsafe { store_fill(0,0,540,960,0x00101820); } store_draw_rect(BACK_X, BACK_Y, BACK_SZ - 8, BACK_SZ - 8, 0x00_FF_FF_FF); store_text(14, 14, "<".as_bytes(), 0x00F1FAEE); store_text(24,24,"应用商店".as_bytes(),0x00F1FAEE); store_text(24,64,"可用软件包".as_bytes(),0x00A8DADC); let names:[&[u8];3]=["计算器".as_bytes(),"终端".as_bytes(),"编辑器".as_bytes()]; for (i,n) in names.iter().enumerate() { let y=112+i*72; store_fill(20,y,500,56,0x00233650); store_text(36,y+18,n,0x00F1FAEE); store_text(420,y+18,"安装".as_bytes(),0x00A8DADC); } unsafe { if STORE_STATUS_LEN > 0 { store_text(24,360,&STORE_STATUS[..STORE_STATUS_LEN],0x00F1FAEE); } } store_cursor(); }

fn run_pkg(name: &[u8]) { let mut path=[0u8;32]; path[..8].copy_from_slice(b"/bin/pkg"); path[8]=0; let mut arg=[0u8;32]; arg[..7].copy_from_slice(b"install"); arg[7]=0; let mut nm=[0u8;32]; let n=name.len().min(31); nm[..n].copy_from_slice(&name[..n]); nm[n]=0; let argv=[path.as_ptr() as usize,arg.as_ptr() as usize,nm.as_ptr() as usize,0]; unsafe { let pid=syscall5(SYS_CLONE,17,0,0,0,0) as isize; if pid==0 { syscall3(SYS_EXECVE,path.as_ptr() as usize,argv.as_ptr() as usize,0); syscall1(SYS_EXIT,127); loop{} } if pid>0 { let mut st=0i32; syscall4(SYS_WAIT4,pid as usize,&mut st as *mut i32 as usize,0,0); status(if ((st>>8)&0xff)==0 { "已安装".as_bytes() } else { "安装失败".as_bytes() }); } else { status("无法启动包管理器".as_bytes()); } } }

/// 轮询输入：处理指针移动（更新光标）与完整点击；返回 `(点击坐标, 指针是否移动)`。
fn store_input(fd: isize) -> (Option<(i32, i32)>, bool) {
    let mut click = None;
    let mut moved = false;
    unsafe {
        loop {
            let mut b = [0u8; 24];
            let n = syscall3(SYS_READ, fd as usize, b.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return (click, moved);
            }
            let ty = u16::from_le_bytes([b[16], b[17]]);
            let code = u16::from_le_bytes([b[18], b[19]]);
            let val = i32::from_le_bytes([b[20], b[21], b[22], b[23]]);
            match ty {
                EV_REL => {
                    if code == REL_X {
                        CUR_X = (CUR_X + val).clamp(0, 539);
                        moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + val).clamp(0, 959);
                        moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X {
                        CUR_X = val.clamp(0, 539);
                        moved = true;
                    } else if code == ABS_Y {
                        CUR_Y = val.clamp(0, 959);
                        moved = true;
                    }
                }
                EV_KEY => {
                    if code == BTN_TOUCH {
                        if val == 1 {
                            TOUCH_DOWN = true;
                        } else if val == 0 && TOUCH_DOWN {
                            TOUCH_DOWN = false;
                            click = Some((CUR_X, CUR_Y));
                        }
                    } else if code == BTN_LEFT {
                        if val == 1 {
                            MOUSE_DOWN = true;
                        } else if val == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            click = Some((CUR_X, CUR_Y));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 轮询 /dev/tty（fd 0）：检测 ESC。
fn store_poll_key() -> bool {
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, 0, buf.as_mut_ptr() as usize, buf.len()) };
    if n <= 0 {
        return false;
    }
    for i in 0..(n as usize) {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}

fn store_back_hit(x: i32, y: i32) -> bool {
    x >= BACK_X && x < BACK_X + BACK_SZ && y >= BACK_Y && y < BACK_Y + BACK_SZ
}

fn interactive_store() -> bool {
    unsafe {
        let fd = open(FB_PATH, O_RDWR);
        if fd < 0 {
            return false;
        }
        let base = syscall6(SYS_MMAP, 0, 540 * 960 * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd as usize, 0);
        STORE_FB = base as *mut u32;
        syscall1(SYS_CLOSE, fd as usize);
    }
    if unsafe { STORE_FB.is_null() } {
        return false;
    }
    store_render();
    store_flip();
    let input = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    loop {
        if input >= 0 {
            let (click, moved) = store_input(input);
            if moved {
                store_render();
                store_flip();
            }
            if let Some((x, y)) = click {
                print("[store] tap (");
                print_dec(x as u32);
                print(",");
                print_dec(y as u32);
                print(")\n");
                if store_back_hit(x, y) {
                    print("[store] back -> exit\n");
                    return true;
                }
                if y > 112 && y < 168 && x > 400 {
                    run_pkg(b"calculator");
                    store_render();
                    store_flip();
                } else if y >= 184 && y < 240 && x > 400 {
                    run_pkg(b"terminal");
                    store_render();
                    store_flip();
                } else if y >= 256 && y < 312 && x > 400 {
                    run_pkg(b"editor");
                    store_render();
                    store_flip();
                }
            }
        }
        if store_poll_key() {
            print("[store] ESC -> exit\n");
            return true;
        }
        sleep_ms(16);
    }
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[");
    print(APP_NAME);
    print("] starting\n");

    let n = count_packages();
    print("[");
    print(APP_NAME);
    print("] catalog: ");
    print_dec(n);
    print(" packages available\n");

    print("[store] touch INSTALL to run signed pkg installer\n");
    interactive_store();
    0
}

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
        write_fd(2, b"PANIC: store\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
