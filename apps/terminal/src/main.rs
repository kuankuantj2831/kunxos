// KunxOS Terminal 应用（图形终端 / PTY 主侧）。
//
// 裸 syscall、no_std、无堆用户态程序。作为 PTY 主侧：
//   - 打开 /dev/tty1（会话终端：内核 Shell 绑定会话 0 为从侧），
//   - 把按键/触屏输入以原始字节/CSI 序列写入 tty（键码 → in_q），
//   - 读取 tty 屏幕输出字节流（out_q ← 内核 uart 镜像），
//   - 内含精简 VT 终端仿真器：文本网格 + SGR 颜色、光标移动/清屏/退格/Tab、
//     滚动回看（历史行 + 滚动条）、CJK/ASCII 混排渲染。
//
// 行编辑（光标移动、历史、Tab 补全、Ctrl+C/D/Z 信号）由内核 Shell 的
// read_line 完成（PTY 从侧规范模式）；本应用是哑终端：只转发按键并渲染
// 回显/输出字节流，不做本地回显 —— 与 Unix PTY 主从模型一致。
//
// 交互：
//   - 键盘：ASCII 键（Shift 大写）、Enter、Backspace、Tab、方向键（CSI）、
//     Ctrl+A/E/U/W/C/D/Z（控制字节）、ESC 原样转发；
//   - 触摸：标题栏返回键退出；在终端区竖直拖动滚动回看；滚动条表现滚动位置。
//   - 系统导航栏（Home/Back/Recents）与全面屏手势由 sys_nav 处理。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

#[path = "../../../shared/nav.rs"]
mod sys_nav;

#[path = "../../../shared/error_ui.rs"]
mod err_ui;

// ==================== 系统调用 ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_IOCTL: usize = 29;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_MMAP: usize = 222;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDWR: usize = 2;
const O_RDONLY: usize = 0;
const O_NONBLOCK: usize = 0x800;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const FBIOGET_VSCREENINFO: usize = 0x4600;
const FBIO_PRESENT: usize = 0x4603;
const TIOCSWINSZ: usize = 0x5414;

const INPUT_PATH: &str = "/dev/input/event0";
const TTY_PATH: &str = "/dev/tty1";

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, in("a5") a6, options(nostack));
    ret
}

fn write_fd(fd: usize, buf: &[u8]) -> usize {
    unsafe { syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len()) }
}

/// tty 全量写：TTY_FD 以 O_NONBLOCK 打开，方向键 3 字节 CSI 序列可能
/// 短写（内核 in_q 满）——剩余丢失会让 shell 收到孤立 ESC（行编辑乱码）。
/// 循环补写，最多重试 4 次。
fn write_fd_all(fd: usize, buf: &[u8]) {
    let mut off = 0usize;
    let mut tries = 0;
    while off < buf.len() && tries < 4 {
        let n = write_fd(fd, &buf[off..]) as isize;
        if n <= 0 {
            tries += 1;
            continue;
        }
        off += n as usize;
    }
}

fn now_us() -> u64 {
    // 单调钟：RTC 就绪前后墙钟会跳变，时间差判定（动画/超时）必须用
    // CLOCK_MONOTONIC（黑屏事故同族防御）。
    let mut tv = [0u8; 16];
    unsafe {
        syscall3(113, 1, tv.as_mut_ptr() as usize, 0); // CLOCK_GETTIME, MONOTONIC
    }
    let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
    let nsec = u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
    sec * 1_000_000 + nsec / 1_000
}
fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe { syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0) };
}

fn exit(code: usize) -> ! {
    unsafe { syscall1(SYS_EXIT, code) };
    loop {}
}

// ==================== 文件访问 ====================

static mut PATH_BUF: [u8; 128] = [0; 128];

fn open(path: &str, flags: usize) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
        PATH_BUF[n] = 0;
    }
    unsafe { syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize }
}

// ==================== 显示 ====================

#[path = "../../../shared/display.rs"]
mod display;
static mut VIEW_W: usize = 540;
static mut VIEW_H: usize = 960;
fn display_w() -> usize { unsafe { VIEW_W } }
fn display_h() -> usize { unsafe { VIEW_H } }

fn refresh_geometry() -> bool {
    unsafe {
        if FB_FD < 0 { return false; }
        if syscall3(SYS_IOCTL, FB_FD as usize, display::RESIZE_IOCTL, 0) != 0 { return false; }
        let mut data = [0u8; display::INFO_BYTES];
        if syscall3(SYS_IOCTL, FB_FD as usize, display::INFO_IOCTL, data.as_mut_ptr() as usize) != 0 { return false; }
        let Some(info) = display::DisplayMetrics::decode(&data) else { return false; };
        if info.width as usize == VIEW_W && info.height as usize == VIEW_H { return false; }
        VIEW_W = info.width as usize; VIEW_H = info.height as usize;

        sys_nav::set_viewport(VIEW_W as i32, VIEW_H as i32);
        true
    }
}

const TITLE_H: i32 = 44;
const NAV_H: i32 = 48;

static mut FB_PTR: *mut u32 = core::ptr::null_mut();
static mut FB_FD: isize = -1;
static mut INPUT_FD: isize = -1;
static mut TTY_FD: isize = -1;

// 白底浅色主题：终端背景纯白，默认前景/标题用深色文字。
const C_BG: u32 = 0x00_FF_FF_FF;
const C_TEXT: u32 = 0x00_1A_1C_22;
const C_TITLE_BG: u32 = 0x00_F2_F4_F8;
/// 光标块：白底上用深色块保证可见（原浅紫块在白底不可辨）。
const C_CURSOR: u32 = 0x00_1A_1C_22;
const C_SCROLLBAR: u32 = 0x00_E5_E9_F0;
const C_SCROLLBAR_THUMB: u32 = 0x00_6B_72_80;

/// ANSI 16 色（索引 0-7 常规 / 8-15 亮色）。
/// 白底主题：色相保留，过亮色（黄/白/青/亮绿等）压暗为白底可读色；
/// 索引 7（白）作为默认前景映射为主题深色 0x1A1C22。
const PALETTE: [u32; 16] = [
    0x00_30_30_30, 0x00_C9_1B_00, 0x00_1A_7F_37, 0x00_A1_79_08,
    0x00_00_22_C9, 0x00_8E_44_AD, 0x00_0E_74_90, 0x00_1A_1C_22,
    0x00_5C_5C_5C, 0x00_E7_4C_3C, 0x00_2D_9A_46, 0x00_B8_86_0B,
    0x00_25_63_EB, 0x00_A8_32_B0, 0x00_08_91_B2, 0x00_6B_72_80,
];

fn put_pixel(x: i32, y: i32, color: u32) {
    unsafe {
        if x >= 0 && y >= 0 && (x as usize) < display_w() && (y as usize) < display_h() {
            *FB_PTR.add(y as usize * display_w() + x as usize) = color;
        }
    }
}

fn fill_rect(x: i32, y: i32, w: i32, h: i32, color: u32) {
    if w <= 0 || h <= 0 {
        return;
    }
    // 先整体裁剪再逐行 slice::fill（宽通道 memset）：逐像素 put_pixel 的
    // 每像素 4 次边界检查降为每行一次。
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(display_w() as i32);
    let y1 = (y + h).min(display_h() as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    unsafe {
        for yy in y0..y1 {
            let base = yy as usize * display_w() + x0 as usize;
            core::slice::from_raw_parts_mut(FB_PTR.add(base), (x1 - x0) as usize)
                .fill(color);
        }
    }
}

fn fill_screen(color: u32) {
    unsafe {
        for p in 0..display_w() * display_h() {
            *FB_PTR.add(p) = color;
        }
    }
}

fn draw_text(x: i32, y: i32, s: &[u8], color: u32) {
    let mut cx = x;
    let mut i = 0usize;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += glyph_draw(cx, y, cp, color);
        i += n;
    }
}

/// 在 (x, y) 绘制字形，返回水平推进（像素）。
fn glyph_draw(x: i32, y: i32, cp: u32, color: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    match ttf_text::font_render::rasterize_char(ch, ttf_text::font_render::FONT_PX) {
        Some(g) => {
            let gx = x as i32 + g.xmin;
            let gy = y as i32;
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.data[row * g.width + col];
                    if a != 0 {
                        let px = gx + col as i32;
                        let py = gy + row as i32;
                        if px >= 0 && py >= 0 && (px as usize) < display_w() && (py as usize) < display_h() {
                            let bg = unsafe { *FB_PTR.add(py as usize * display_w() + px as usize) };
                            put_pixel(px, py, ttf_text::font_render::blend(bg, color, a));
                        }
                    }
                }
            }
            g.advance
        }
        None => if cp >= 0x80 { 16 } else { 8 },
    }
}

/// 单元格水平推进（宽字符按字形实际推进，光标/换行按此累计）。
fn glyph_advance(cp: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    match core::char::from_u32(cp).filter(|&c| c != '\0') {
        Some(ch) => match ttf_text::font_render::rasterize_char(ch, ttf_text::font_render::FONT_PX) {
            Some(g) => g.advance,
            None => if cp >= 0x80 { 16 } else { 8 },
        },
        None => 8,
    }
}

fn cell_width_base() -> i32 {
    match ttf_text::font_render::rasterize_char('M', ttf_text::font_render::FONT_PX) {
        Some(g) => g.advance,
        None => 8,
    }
}

// ==================== 终端仿真器核心 ====================

const TERM_COLS_MAX: usize = 96;
const TERM_ROWS_MAX: usize = 60;
const HIST_LINES: usize = 24;
const HIST_LINE_MAX: usize = 256;

#[derive(Clone, Copy)]
struct Cell {
    cp: u32,
    fg: u8,
    bg: u8,
    bold: bool,
}

const EMPTY_CELL: Cell = Cell { cp: 0, fg: 7, bg: 0, bold: false };

static mut SCREEN: [[Cell; TERM_COLS_MAX]; TERM_ROWS_MAX] = [[EMPTY_CELL; TERM_COLS_MAX]; TERM_ROWS_MAX];
static mut ROW_LEN: [usize; TERM_ROWS_MAX] = [0; TERM_ROWS_MAX];
static mut CUR_R: usize = 0;
static mut CUR_C: usize = 0;

static mut SGR_FG: u8 = 7;
static mut SGR_BG: u8 = 0;
static mut SGR_BOLD: bool = false;

static mut HIST_BUF: [[u8; HIST_LINE_MAX]; HIST_LINES] = [[0; HIST_LINE_MAX]; HIST_LINES];
static mut HIST_LEN: [usize; HIST_LINES] = [0; HIST_LINES];
static mut HIST_NEXT: usize = 0;
static mut HIST_CNT: usize = 0;

static mut SCROLL: usize = 0;

static mut VT_ESC: u8 = 0;
static mut VT_PARAMS: [u16; 8] = [0; 8];
static mut VT_NPARAM: usize = 0;
static mut VT_CUR: u16 = 0;
static mut VT_UTF8: [u8; 4] = [0; 4];
static mut VT_UTF8N: usize = 0;

fn scrollback_total() -> usize {
    unsafe { HIST_CNT + TERM_ROWS_MAX }
}

fn hist_push_row(r: usize) {
    unsafe {
        let len_row = ROW_LEN[r].min(TERM_COLS_MAX);
        let slot = HIST_NEXT;
        HIST_BUF[slot].fill(0);
        let mut out = 0usize;
        let mut i = 0usize;
        while i < len_row && out + 4 < HIST_LINE_MAX {
            let cell = SCREEN[r][i];
            if cell.cp != 0 {
                let mut b = [0u8; 4];
                let n = match core::char::from_u32(cell.cp) {
                    Some(ch) => ch.encode_utf8(&mut b).len(),
                    None => 1,
                };
                for k in 0..n {
                    if out < HIST_LINE_MAX - 1 {
                        HIST_BUF[slot][out] = b[k];
                        out += 1;
                    }
                }
            }
            i += 1;
        }
        HIST_LEN[slot] = out;
        HIST_NEXT = (HIST_NEXT + 1) % HIST_LINES;
        if HIST_CNT < HIST_LINES {
            HIST_CNT += 1;
        }
    }
}

fn screen_row_blank(r: usize) {
    unsafe {
        SCREEN[r] = [EMPTY_CELL; TERM_COLS_MAX];
        ROW_LEN[r] = 0;
    }
}

fn term_newline() {
    unsafe {
        CUR_C = 0;
        if CUR_R + 1 < TERM_ROWS_MAX {
            CUR_R += 1;
        } else {
            hist_push_row(0);
            let mut r = 0;
            while r + 1 < TERM_ROWS_MAX {
                SCREEN[r] = SCREEN[r + 1];
                ROW_LEN[r] = ROW_LEN[r + 1];
                r += 1;
            }
            screen_row_blank(TERM_ROWS_MAX - 1);
        }
        SCROLL = 0;
    }
}

fn put_cell(cp: u32) {
    unsafe {
        if CUR_C >= TERM_COLS_MAX {
            CUR_C = 0;
            term_newline();
        }
        SCREEN[CUR_R][CUR_C] = Cell { cp, fg: SGR_FG, bg: SGR_BG, bold: SGR_BOLD };
        if ROW_LEN[CUR_R] <= CUR_C {
            ROW_LEN[CUR_R] = CUR_C + 1;
        }
        CUR_C += 1;
        if CUR_C >= TERM_COLS_MAX {
            CUR_C = 0;
            term_newline();
        }
    }
}

fn term_cp(cp: u32) {
    match cp {
        0x0d => unsafe { CUR_C = 0 },
        0x0a => term_newline(),
        0x09 => unsafe {
            let next = (CUR_C / 8 + 1) * 8;
            CUR_C = next.min(TERM_COLS_MAX);
        },
        0x08 => unsafe {
            if CUR_C > 0 {
                CUR_C -= 1;
            }
        },
        0x0c => term_clear_screen(),
        _ => put_cell(cp),
    }
}

fn term_clear_screen() {
    unsafe {
        for r in 0..TERM_ROWS_MAX {
            hist_push_row(r);
        }
        for r in 0..TERM_ROWS_MAX {
            screen_row_blank(r);
        }
        CUR_R = 0;
        CUR_C = 0;
        SCROLL = 0;
    }
}

fn term_put(c: u8) {
    unsafe {
        let esc = VT_ESC;
        if esc == 0 {
            if c == 0x1b {
                decode_utf8_pending();
                VT_ESC = 1;
            } else if c < 0x80 {
                // ASCII 先排空 pending：未完成序列后紧跟 ASCII 时，滞留的
                // 2 字节序列（°/· 等）会晚于已打印的 ASCII 渲染（乱序）。
                decode_utf8_pending();
                term_cp(c as u32);
            } else {
                // 高位字节：UTF-8 组装。按首字节位型决定期望长度 —— 此前
                // 「凑满 4 字节才解码」与 CJK 的 3 字节序列错位：第 4 字节
                // 是下一字的 lead byte，触发 flush 后余量循环只转发 ASCII，
                // lead byte 被静默丢弃，纯中文流只剩第一个字上屏。
                VT_UTF8[VT_UTF8N] = c;
                VT_UTF8N += 1;
                let lead = VT_UTF8[0];
                let expect = match lead {
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => 0, // 孤立 continuation：排空丢弃
                };
                if expect == 0 || VT_UTF8N >= expect {
                    decode_utf8_pending();
                }
            }
        } else if esc == 1 {
            if c == b'[' {
                VT_ESC = 2;
                VT_NPARAM = 0;
                VT_CUR = 0;
            } else {
                VT_ESC = 0;
                if c != 0x1b {
                    term_cp(c as u32);
                }
            }
        } else {
            // esc == 2：CSI 参数收集；esc == 3：DEC 私有参数（忽略）。
            match c {
                b'0'..=b'9' => VT_CUR = VT_CUR.saturating_mul(10).saturating_add((c - b'0') as u16),
                b';' => {
                    push_param();
                    VT_CUR = 0;
                }
                b'?' => {
                    push_param();
                    VT_ESC = 3;
                    VT_NPARAM = 0;
                    VT_CUR = 0;
                }
                _ => {
                    push_param();
                    let was_private = esc == 3;
                    VT_ESC = 0;
                    if !was_private {
                        csi_exec(c);
                    }
                }
            }
        }
    }
}

fn push_param() {
    unsafe {
        if VT_NPARAM < VT_PARAMS.len() {
            VT_PARAMS[VT_NPARAM] = VT_CUR;
            VT_NPARAM += 1;
        }
        VT_CUR = 0;
    }
}

fn param(idx: usize, default: u16) -> u16 {
    unsafe {
        if idx < VT_NPARAM {
            VT_PARAMS[idx]
        } else {
            default
        }
    }
}

fn csi_exec(final_byte: u8) {
    unsafe {
        let p0 = param(0, 0);
        let p1 = param(1, 0);
        let n = if p0 == 0 { 1 } else { p0 };
        match final_byte {
            b'A' => CUR_R = CUR_R.saturating_sub(n as usize),
            b'B' => CUR_R = (CUR_R + n as usize).min(TERM_ROWS_MAX - 1),
            b'C' => CUR_C = (CUR_C + n as usize).min(TERM_COLS_MAX),
            b'D' => CUR_C = CUR_C.saturating_sub(n as usize),
            b'H' | b'f' => {
                CUR_R = (p1 as usize).saturating_sub(1).min(TERM_ROWS_MAX - 1);
                CUR_C = (p0 as usize).saturating_sub(1).min(TERM_COLS_MAX);
            }
            b'G' => CUR_C = (p0 as usize).saturating_sub(1).min(TERM_COLS_MAX),
            b'd' => CUR_R = (p0 as usize).saturating_sub(1).min(TERM_ROWS_MAX - 1),
            b'J' => match p0 {
                2 => term_clear_screen(),
                0 => {
                    for r in CUR_R..TERM_ROWS_MAX {
                        let from = if r == CUR_R { CUR_C } else { 0 };
                        for c in from..TERM_COLS_MAX {
                            SCREEN[r][c] = EMPTY_CELL;
                        }
                        ROW_LEN[r] = ROW_LEN[r].min(from);
                    }
                }
                _ => {}
            },
            b'K' => {
                let from = if p0 == 2 { 0 } else { CUR_C };
                for c in from..TERM_COLS_MAX {
                    SCREEN[CUR_R][c] = EMPTY_CELL;
                }
                ROW_LEN[CUR_R] = ROW_LEN[CUR_R].min(from);
            }
            b'm' => sgr_run(),
            b's' => {
                VT_PARAMS[0] = CUR_C as u16;
                VT_PARAMS[1] = CUR_R as u16;
                VT_NPARAM = 2;
            }
            b'u' => {
                CUR_C = (param(0, 0) as usize).min(TERM_COLS_MAX);
                CUR_R = (param(1, 0) as usize).min(TERM_ROWS_MAX - 1);
            }
            _ => {}
        }
    }
}

fn sgr_run() {
    unsafe {
        let mut i = 0usize;
        while i < VT_NPARAM {
            let n = VT_PARAMS[i];
            match n {
                0 => {
                    SGR_FG = 7;
                    SGR_BG = 0;
                    SGR_BOLD = false;
                }
                1 => SGR_BOLD = true,
                22 => SGR_BOLD = false,
                30..=37 => SGR_FG = (n - 30) as u8,
                39 => SGR_FG = 7,
                40..=47 => SGR_BG = (n - 40) as u8,
                49 => SGR_BG = 0,
                90..=97 => SGR_FG = (n - 90 + 8) as u8,
                100..=107 => SGR_BG = (n - 100 + 8) as u8,
                _ => {}
            }
            i += 1;
        }
    }
}

fn decode_utf8_pending() {
    unsafe {
        let n = VT_UTF8N;
        if n == 0 {
            return;
        }
        VT_UTF8N = 0;
        // 逐序列消费整个缓冲（utf8_next 对非法首字节返回 (0,1) 丢弃一字节
        // 续读 —— 不再只解第一个序列、丢弃非 ASCII 余量）。
        let mut k = 0usize;
        while k < n {
            let (cp, used) = ttf_text::font_render::utf8_next(&VT_UTF8[..n], k);
            if used == 0 {
                break; // 无进展（防御）：放弃剩余
            }
            if cp >= 0x20 {
                term_cp(cp);
            }
            k += used;
        }
    }
}

// ==================== 渲染 ====================

fn fg_color(idx: u8, bold: bool) -> u32 {
    unsafe {
        let p = if bold && idx < 8 { idx + 8 } else { idx };
        PALETTE[p as usize]
    }
}

fn bg_color(idx: u8) -> u32 {
    PALETTE[idx as usize]
}

fn draw_row(x0: i32, y: i32, row: &[Cell], len: usize) {
    let mut cx = x0;
    let mut i = 0usize;
    while i < len && (cx as usize) < display_w() {
        let cell = row[i];
        let ch = cell.cp;
        if ch != 0 {
            if cell.bg != 0 {
                let adv = glyph_advance(ch);
                fill_rect(cx, y, adv, 16, bg_color(cell.bg));
            }
            draw_cp(cx, y, &cell);
        }
        cx += glyph_advance(ch);
        i += 1;
    }
}

fn draw_cp(x: i32, y: i32, cell: &Cell) {
    if cell.cp == 0 {
        return;
    }
    let color = fg_color(cell.fg, cell.bold);
    glyph_draw(x, y, cell.cp, color);
}

fn draw_hist_row(x0: i32, y: i32, slot: usize) {
    unsafe {
        let len = HIST_LEN[slot];
        let mut i = 0usize;
        let mut cx = x0;
        while i < len && (cx as usize) < display_w() {
            let (cp, n) = ttf_text::font_render::utf8_next(&HIST_BUF[slot][..len], i);
            if cp != 0 {
                draw_cp(cx, y, &Cell { cp, fg: 7, bg: 0, bold: false });
            }
            cx += glyph_advance(cp);
            i += n;
        }
    }
}

fn draw_scrollbar() {
    unsafe {
        let total = scrollback_total();
        if total <= TERM_ROWS_MAX && SCROLL == 0 {
            return;
        }
        let bar_x = display_w() as i32 - 4;
        fill_rect(bar_x, TITLE_H + 2, 3, display_h() as i32 - TITLE_H - NAV_H - 4, C_SCROLLBAR);
        let view_h = (display_h() as i32 - TITLE_H - NAV_H - 4) as usize;
        let thumb_h = (view_h * TERM_ROWS_MAX / total).max(20);
        let max_scroll = total - TERM_ROWS_MAX;
        let sc = SCROLL.min(max_scroll);
        let thumb_y = TITLE_H + 2 + ((view_h - thumb_h) * sc / max_scroll.max(1)) as i32;
        fill_rect(bar_x, thumb_y, 3, thumb_h as i32, C_SCROLLBAR_THUMB);
    }
}

pub fn draw_terminal() {
    unsafe {
        fill_screen(C_BG);
        fill_rect(0, 0, display_w() as i32, TITLE_H, C_TITLE_BG);
        draw_text(12, 12, b"KunxOS Terminal", C_TEXT);
        draw_text(6, 13, b"<", C_TEXT);
        let total = scrollback_total();
        let top = total.saturating_sub(SCROLL + TERM_ROWS_MAX);
        let y0 = TITLE_H + 6;
        for r in 0..TERM_ROWS_MAX {
            let idx = top + r;
            let y = y0 + r as i32 * 16;
            if idx < HIST_CNT {
                // 环形缓冲：逻辑序号 idx 的最新行写在 slot (HIST_NEXT+idx)
                // % HIST_LINES —— 此前 idx % HIST_LINES 在环回绕后（滚屏/
                // clear 一次推 24+ 行）最新行会出现在最旧行上方（乱序）。
                let slot = (HIST_NEXT + idx) % HIST_LINES;
                draw_hist_row(8, y, slot);
            } else {
                let s = idx - HIST_CNT;
                if s < TERM_ROWS_MAX {
                    let row = SCREEN[s];
                    draw_row(8, y, &row, ROW_LEN[s].min(TERM_COLS_MAX));
                }
            }
        }
        if SCROLL == 0 {
            let mut cursor_x = 8i32;
            let cur_row = CUR_R;
            for c in 0..CUR_C.min(ROW_LEN[cur_row]) {
                cursor_x += glyph_advance(SCREEN[cur_row][c].cp);
            }
            if CUR_C >= ROW_LEN[cur_row] {
                cursor_x += cell_width_base() * ((CUR_C - ROW_LEN[cur_row]) as i32);
            }
            let cursor_y = y0 + CUR_R as i32 * 16;
            let cw = if CUR_C < ROW_LEN[cur_row] {
                glyph_advance(SCREEN[cur_row][CUR_C].cp).max(4)
            } else {
                cell_width_base()
            };
            if (now_us() / 500_000) % 2 == 0 {
                fill_rect(cursor_x, cursor_y, cw, 16, C_CURSOR);
            }
        }
        draw_scrollbar();
        fill_rect(0, display_h() as i32 - NAV_H - 4, display_w() as i32, 4, C_TITLE_BG);
    }
}

// ==================== 输入 ====================

const KEY_ENTER: u16 = 28;
const KEY_BACKSPACE: u16 = 14;
const KEY_TAB: u16 = 15;
const KEY_ESC: u16 = 1;
const KEY_UP: u16 = 103;
const KEY_DOWN: u16 = 108;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const BTN_TOUCH: u16 = 0x14a;

static mut SHIFT: bool = false;
static mut CTRL: bool = false;
static mut TP_X: i32 = 0;
static mut TP_Y: i32 = 0;
static mut TP_DOWN: bool = false;
static mut TP_PY: i32 = 0;

fn key_char(code: u16, shifted: bool) -> Option<u8> {
    let c = match code {
        2 => b'1', 3 => b'2', 4 => b'3', 5 => b'4', 6 => b'5', 7 => b'6',
        8 => b'7', 9 => b'8', 10 => b'9', 11 => b'0',
        16 => b'q', 17 => b'w', 18 => b'e', 19 => b'r', 20 => b't', 21 => b'y',
        22 => b'u', 23 => b'i', 24 => b'o', 25 => b'p',
        30 => b'a', 31 => b's', 32 => b'd', 33 => b'f', 34 => b'g', 35 => b'h',
        36 => b'j', 37 => b'k', 38 => b'l',
        44 => b'z', 45 => b'x', 46 => b'c', 47 => b'v', 48 => b'b', 49 => b'n', 50 => b'm',
        57 => b' ', 12 => b'-', 13 => b'=', 52 => b'.', 53 => b'/', 51 => b',', 77 => b'@',
        _ => return None,
    };
    Some(if shifted { c.to_ascii_uppercase() } else { c })
}

fn send_key(code: u16, value: i32) {
    unsafe {
        if TTY_FD < 0 {
            return;
        }
        match code {
            KEY_ENTER => {
                if value == 1 {
                    write_fd(TTY_FD as usize, b"\r");
                }
            }
            KEY_BACKSPACE => {
                if value == 1 {
                    write_fd(TTY_FD as usize, b"\x7f");
                }
            }
            KEY_TAB => {
                if value == 1 {
                    write_fd(TTY_FD as usize, b"\t");
                }
            }
            KEY_UP => {
                if value == 1 {
                    write_fd_all(TTY_FD as usize, b"\x1b[A");
                }
            }
            KEY_DOWN => {
                if value == 1 {
                    write_fd_all(TTY_FD as usize, b"\x1b[B");
                }
            }
            KEY_LEFT => {
                if value == 1 {
                    write_fd_all(TTY_FD as usize, b"\x1b[D");
                }
            }
            KEY_RIGHT => {
                if value == 1 {
                    write_fd_all(TTY_FD as usize, b"\x1b[C");
                }
            }
            KEY_LEFTCTRL | KEY_RIGHTCTRL => CTRL = value == 1,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => SHIFT = value == 1,
            KEY_ESC => {
                if value == 1 {
                    write_fd_all(TTY_FD as usize, b"\x1b");
                }
            }
            _ => {
                if value == 1 {
                    if let Some(c) = key_char(code, SHIFT) {
                        if CTRL && (b'a'..=b'z').contains(&c) {
                            write_fd(TTY_FD as usize, &[c - b'a' + 1]);
                        } else {
                            write_fd(TTY_FD as usize, &[c]);
                        }
                    }
                }
            }
        }
    }
}

/// 读取一个输入事件批次；返回 (nav, tap)。
fn read_event(fd: isize) -> (sys_nav::NavAction, Option<(i32, i32)>) {
    let mut buf = [0u8; 48];
    let mut nav = sys_nav::NavAction::None;
    let mut tap = None;
    unsafe {
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        if n < 24 {
            return (nav, tap);
        }
        let mut i = 0usize;
        while i + 24 <= n as usize {
            let ty = u16::from_le_bytes([buf[i + 16], buf[i + 17]]);
            let code = u16::from_le_bytes([buf[i + 18], buf[i + 19]]);
            let value = i32::from_le_bytes([buf[i + 20], buf[i + 21], buf[i + 22], buf[i + 23]]);
            match ty {
                3 => {
                    if code == 0 {
                        TP_X = value;
                    } else if code == 1 {
                        TP_Y = value;
                    }
                }
                1 => {
                    if code == BTN_TOUCH {
                        if value == 1 {
                            TP_DOWN = true;
                            TP_PY = TP_Y;
                        } else if value == 0 && TP_DOWN {
                            TP_DOWN = false;
                            let mode = sys_nav::nav_mode();
                            let a = sys_nav::nav_hit(TP_X, TP_Y, mode);
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                tap = Some((TP_X, TP_Y));
                            }
                        }
                    } else if code != 0x110 {
                        send_key(code, value);
                    }
                }
                _ => {}
            }
            i += 24;
        }
    }
    (nav, tap)
}

/// 触摸竖直拖动 → 滚动回看（正值=向上看更早内容）。
fn handle_drag() {
    unsafe {
        if !TP_DOWN {
            return;
        }
        let dy = TP_Y - TP_PY;
        if dy.abs() >= 8 {
            let rows = dy / 16;
            let total = scrollback_total();
            let max_scroll = total.saturating_sub(TERM_ROWS_MAX);
            let mut s = SCROLL as isize + rows as isize;
            if s < 0 {
                s = 0;
            }
            let ms = max_scroll as isize;
            if s > ms {
                s = ms;
            }
            SCROLL = s as usize;
            TP_PY = TP_Y;
        }
    }
}

// ==================== 毛玻璃 / 系统导航 ====================

use sys_nav::glass::{GlassStyle, GlassTarget, STYLE_DANGER, glass_rect_style};

struct TermGlass;
impl GlassTarget for TermGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && (x as usize) < display_w() && (y as usize) < display_h() {
            unsafe { *FB_PTR.add(y as usize * display_w() + x as usize) }
        } else {
            0
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
}

fn draw_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut TermGlass, x, y, w, h, r, st, true);
}

fn draw_error_banner() {
    if !err_ui::err_active(now_us()) {
        return;
    }
    let msg = err_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    draw_glass_style(0, 48, display_w() as i32, 44, 0, &STYLE_DANGER);
    draw_text(8, 62, msg, 0x00_FF_FF_FF);
}

struct TermCanvas;
impl sys_nav::NavCanvas for TermCanvas {
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && (x as usize) < display_w() && (y as usize) < display_h() {
            unsafe { *FB_PTR.add(y as usize * display_w() + x as usize) }
        } else {
            0
        }
    }
}

fn draw_sys_nav() {
    draw_error_banner();
    let mut cv = TermCanvas;
    sys_nav::draw_nav_bar(&mut cv, sys_nav::nav_mode());
}

fn present_frame() {
    unsafe {
        if FB_FD < 0 {
            return;
        }
        let mut dbuf = [0u8; 4 + 16];
        dbuf[0..4].copy_from_slice(&1u32.to_le_bytes());
        dbuf[4..8].copy_from_slice(&0i32.to_le_bytes());
        dbuf[8..12].copy_from_slice(&0i32.to_le_bytes());
        dbuf[12..16].copy_from_slice(&(display_w() as u32).to_le_bytes());
        dbuf[16..20].copy_from_slice(&(display_h() as u32).to_le_bytes());
        syscall3(SYS_IOCTL, FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

// ==================== 主流程 ====================

fn tty_attach() -> bool {
    unsafe {
        if TTY_FD < 0 {
            let fd = open(TTY_PATH, O_RDWR | O_NONBLOCK);
            if fd < 0 {
                return false;
            }
            TTY_FD = fd;
            // 上报窗口尺寸（列×行）：Shell 排布与 TIOCGWINSZ 查询用。
            let cols = (((display_w() as usize - 24) / cell_width_base() as usize).max(20))
                .min(TERM_COLS_MAX);
            let rows = (((display_h() as usize - TITLE_H as usize - NAV_H as usize - 10) / 16)
                .max(10))
                .min(TERM_ROWS_MAX);
            let mut ws = [0u8; 8];
            ws[0..2].copy_from_slice(&(rows as u16).to_le_bytes());
            ws[2..4].copy_from_slice(&(cols as u16).to_le_bytes());
            syscall3(SYS_IOCTL, fd as usize, TIOCSWINSZ, ws.as_ptr() as usize);
        }
        true
    }
}

fn app_main() {
    unsafe {
        let fd = open("/dev/fb0", O_RDWR);
        if fd < 0 {
            write_fd(1, b"terminal: framebuffer unavailable\n");
            return;
        }
        let mut info = [0u8; 24];
        syscall3(SYS_IOCTL, fd as usize, FBIOGET_VSCREENINFO, info.as_mut_ptr() as usize);
        let len = display::MAX_PIXELS * 4;
        let base = syscall6(SYS_MMAP, 0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd as usize, 0);
        if (base as isize) < 0 {
            write_fd(1, b"terminal: fb mmap failed\n");
            return;
        }
        FB_PTR = base as *mut u32;
        FB_FD = fd;
        refresh_geometry();
    }
    if unsafe { FB_PTR.is_null() } {
        return;
    }

    draw_terminal();
    draw_sys_nav();
    present_frame();
    ttf_text::font_init();
    draw_terminal();
    draw_sys_nav();
    present_frame();

    let input = unsafe { open(INPUT_PATH, O_RDONLY | O_NONBLOCK) };
    if input >= 0 {
        unsafe { INPUT_FD = input; }
    }
    if !tty_attach() {
        unsafe {
            write_fd(1, b"terminal: /dev/tty1 unavailable\n");
        }
    }

    let mut frame = 0u32;
    loop {
        if refresh_geometry() { draw_terminal(); draw_sys_nav(); present_frame(); }
        let (nav, tap) = read_event(input);
        if nav != sys_nav::NavAction::None {
            let c = sys_nav::nav_exit_code(nav);
            exit(c);
        }
        if let Some((x, y)) = tap {
            if x < 44 && y < TITLE_H {
                exit(0);
            }
        }
        handle_drag();

        unsafe {
            if TTY_FD >= 0 {
                let mut obuf = [0u8; 128];
                loop {
                    let n = syscall3(
                        SYS_READ,
                        TTY_FD as usize,
                        obuf.as_mut_ptr() as usize,
                        obuf.len(),
                    ) as isize;
                    if n <= 0 {
                        break;
                    }
                    let m = n as usize;
                    let mut k = 0usize;
                    while k < m {
                        term_put(obuf[k]);
                        k += 1;
                    }
                }
            }
        }

        draw_terminal();
        draw_sys_nav();
        frame += 1;
        if frame % 5 == 0 {
            present_frame();
        }
        sleep_ms(8);
    }
}

// ==================== 入口 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    app_main();
    0
}

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
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: terminal\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
