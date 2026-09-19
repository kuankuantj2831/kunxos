// KunxOS 备忘录应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/notes）。职责：
//   1. 备忘录列表：扫描 /data/notes 目录，展示标题 + 预览，支持搜索过滤
//   2. 编辑器：键盘输入（字符/退格/回车），保存写入文件，删除移除
//   3. 空状态：无备忘录时居中显示图标与提示
//   4. 返回键 / ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_notes.rs"]
mod cjk_font;

#[path = "../../../shared/app_kit.rs"]
mod kit;

#[path = "../../../shared/ds.rs"]
mod ds;

mod ui;

use kit::*;
use ds::*;

#[inline(always)]
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

// ==================== 常量 ====================

const SYS_GETDENTS64: usize = 61;
const SYS_NEWFSTATAT: usize = 79;
const SYS_UNLINKAT: usize = 35;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_WRONLY: usize = 1;

const MAX_NOTES: usize = 32;
const NOTES_DIR: &str = "/data/notes";

// ==================== 状态 ====================

/// 文件名（不含扩展名，最长 24 字节）。
static mut NOTE_NAMES: [[u8; 24]; MAX_NOTES] = [[0; 24]; MAX_NOTES];
static mut NOTE_NAME_LENS: [usize; MAX_NOTES] = [0; MAX_NOTES];
/// 首行标题（最多 20 字节）。
static mut NOTE_TITLES: [[u8; 20]; MAX_NOTES] = [[0; 20]; MAX_NOTES];
static mut NOTE_TITLE_LENS: [usize; MAX_NOTES] = [0; MAX_NOTES];
/// 内容预览（最多 40 字节，来自第二行）。
static mut NOTE_PREVIEWS: [[u8; 40]; MAX_NOTES] = [[0; 40]; MAX_NOTES];
static mut NOTE_PREVIEW_LENS: [usize; MAX_NOTES] = [0; MAX_NOTES];
static mut NOTE_COUNT: usize = 0;

/// 页面：0 = 列表，1 = 编辑器。
static mut PAGE: u8 = 0;
/// 当前编辑的备忘录索引（usize::MAX = 新建）。
static mut EDIT_IDX: usize = usize::MAX;
/// 编辑缓冲。
static mut EDIT_BUF: [u8; 512] = [0; 512];
static mut EDIT_LEN: usize = 0;
/// 搜索词。
static mut SEARCH: [u8; 24] = [0; 24];
static mut SEARCH_LEN: usize = 0;
/// 搜索框是否聚焦（键盘输入进入搜索）。
static mut SEARCH_FOCUS: bool = false;

// ==================== 文件操作 ====================

pub static mut PATH_BUF: [u8; 128] = [0u8; 128];

/// 拼接 /data/notes/<name>.txt 到 PATH_BUF（NUL 结尾）。
pub unsafe fn note_path(name: &[u8]) -> usize {
    let mut n = 0;
    for &c in NOTES_DIR.as_bytes() {
        if n < PATH_BUF.len() - 1 {
            PATH_BUF[n] = c;
            n += 1;
        }
    }
    PATH_BUF[n] = b'/';
    n += 1;
    for &c in name {
        if n < PATH_BUF.len() - 1 {
            PATH_BUF[n] = c;
            n += 1;
        }
    }
    PATH_BUF[n] = b'.';
    n += 1;
    PATH_BUF[n] = b't';
    n += 1;
    PATH_BUF[n] = b'x';
    n += 1;
    PATH_BUF[n] = b't';
    n += 1;
    PATH_BUF[n] = 0;
    n
}

fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return fd;
        }
        let n = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
        syscall1(SYS_CLOSE, fd as usize);
        n
    }
}

fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
        if fd < 0 {
            return false;
        }
        let mut off = 0usize;
        while off < data.len() {
            let n = syscall3(
                SYS_WRITE,
                fd as usize,
                data.as_ptr().add(off) as usize,
                data.len() - off,
            ) as isize;
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        off == data.len()
    }
}

/// 扫描 /data/notes 目录，读每个 .txt 的首两行作为标题与预览。
unsafe fn scan_notes() {
    NOTE_COUNT = 0;
    let fd = open(NOTES_DIR, O_RDONLY);
    if fd < 0 {
        return;
    }
    let mut raw = [0u8; 2048];
    loop {
        let n = syscall3(SYS_GETDENTS64, fd as usize, raw.as_mut_ptr() as usize, raw.len());
        // 失败返回 -errno（usize 巨大值）：按 isize 判负，否则错误被当作
        // 成功继续解析残留旧数据。
        if (n as isize) <= 0 {
            break;
        }
        let n = (n as usize).min(raw.len());
        let mut off = 0usize;
        while off + 19 <= n {
            let reclen = u16::from_le_bytes([raw[off + 16], raw[off + 17]]) as usize;
            if reclen < 19 || off + reclen > n {
                break;
            }
            let dtyp = raw[off + 18];
            let name = &raw[off + 19..off + reclen];
            let elen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let nm = &name[..elen];
            // 仅 .txt 文件
            if dtyp == 8 && nm.len() > 4 && nm.ends_with(b".txt") && nm[0] != b'.'
                && NOTE_COUNT < MAX_NOTES
            {
                let base = &nm[..nm.len() - 4];
                let i = NOTE_COUNT;
                let bl = base.len().min(23);
                NOTE_NAMES[i][..bl].copy_from_slice(&base[..bl]);
                NOTE_NAME_LENS[i] = bl;
                let p = note_path(&NOTE_NAMES[i][..bl]);
                let mut content = [0u8; 200];
                let cn = read_file_path(&PATH_BUF[..p + 1], &mut content);
                if cn > 0 {
                    let c = &content[..cn as usize];
                    let mut li = 0;
                    // 首行 = 标题
                    let mut ti = 0;
                    while li < c.len() && c[li] != b'\n' && ti < 20 {
                        NOTE_TITLES[i][ti] = c[li];
                        ti += 1;
                        li += 1;
                    }
                    NOTE_TITLE_LENS[i] = ti;
                    // 跳过换行，第二行 = 预览
                    if li < c.len() && c[li] == b'\n' {
                        li += 1;
                    }
                    let mut pi = 0;
                    while li < c.len() && c[li] != b'\n' && pi < 40 {
                        NOTE_PREVIEWS[i][pi] = c[li];
                        pi += 1;
                        li += 1;
                    }
                    NOTE_PREVIEW_LENS[i] = pi;
                }
                NOTE_COUNT += 1;
            }
            off += reclen;
        }
    }
    syscall1(SYS_CLOSE, fd as usize);
}

/// 按 NUL 结尾路径读文件（内部辅助）。
pub unsafe fn read_file_path(path: &[u8], buf: &mut [u8]) -> isize {
    let mut p = [0u8; 128];
    let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    p[..n].copy_from_slice(&path[..n]);
    p[n] = 0;
    let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_RDONLY) as isize;
    if fd < 0 {
        return fd;
    }
    let r = syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) as isize;
    syscall1(SYS_CLOSE, fd as usize);
    r
}

/// 取未占用的最小编号（note_<n> 中的 n）。
unsafe fn next_note_id() -> u32 {
    let mut n = 1u32;
    loop {
        let mut name = [0u8; 24];
        let nl = note_name_from_id(n, &mut name);
        let mut taken = false;
        for i in 0..NOTE_COUNT {
            if NOTE_NAME_LENS[i] == nl && &NOTE_NAMES[i][..nl] == &name[..nl] {
                taken = true;
                break;
            }
        }
        if !taken {
            return n;
        }
        n += 1;
    }
}

/// 由编号构造 note_<n> 名称，返回名称长度。
fn note_name_from_id(id: u32, name: &mut [u8; 24]) -> usize {
    let mut nl = 0;
    name[nl] = b'n';
    nl += 1;
    name[nl] = b'o';
    nl += 1;
    name[nl] = b't';
    nl += 1;
    name[nl] = b'e';
    nl += 1;
    name[nl] = b'_';
    nl += 1;
    let mut tmp = [0u8; 8];
    let mut ti = tmp.len();
    let mut v = id;
    while v > 0 {
        ti -= 1;
        tmp[ti] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    while ti < tmp.len() {
        name[nl] = tmp[ti];
        nl += 1;
        ti += 1;
    }
    nl
}

/// 保存当前编辑缓冲到文件，并重新扫描。
pub unsafe fn save_note() {
    if EDIT_IDX == usize::MAX {
        // 新建：生成唯一文件名后落盘。
        let id = next_note_id();
        let mut name = [0u8; 24];
        let nl2 = note_name_from_id(id, &mut name);
        let p = note_path(&name[..nl2]);
        let ok = write_file_path(&PATH_BUF[..p + 1], &EDIT_BUF[..EDIT_LEN]);
        if ok {
            print("[notes] 已保存新备忘\n");
        } else {
            show_error("保存失败".as_bytes());
        }
    } else {
        let i = EDIT_IDX;
        let p = note_path(&NOTE_NAMES[i][..NOTE_NAME_LENS[i]]);
        let ok = write_file_path(&PATH_BUF[..p + 1], &EDIT_BUF[..EDIT_LEN]);
        if ok {
            print("[notes] 已保存\n");
        } else {
            show_error("保存失败".as_bytes());
        }
    }
    scan_notes();
    EDIT_IDX = usize::MAX;
    EDIT_LEN = 0;
    PAGE = 0;
}

unsafe fn write_file_path(path: &[u8], data: &[u8]) -> bool {
    let mut p = [0u8; 128];
    let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    p[..n].copy_from_slice(&path[..n]);
    p[n] = 0;
    let fd = syscall3(SYS_OPENAT, AT_FDCWD, p.as_ptr() as usize, O_CREAT | O_TRUNC | O_WRONLY) as isize;
    if fd < 0 {
        return false;
    }
    let mut off = 0usize;
    while off < data.len() {
        let r = syscall3(
            SYS_WRITE,
            fd as usize,
            data.as_ptr().add(off) as usize,
            data.len() - off,
        ) as isize;
        if r <= 0 {
            break;
        }
        off += r as usize;
    }
    syscall1(SYS_CLOSE, fd as usize);
    off == data.len()
}

/// 删除当前备忘录文件。
pub unsafe fn delete_note() {
    if EDIT_IDX == usize::MAX {
        return;
    }
    let i = EDIT_IDX;
    let _p = note_path(&NOTE_NAMES[i][..NOTE_NAME_LENS[i]]);
    syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0);
    scan_notes();
    EDIT_IDX = usize::MAX;
    EDIT_LEN = 0;
    PAGE = 0;
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    ds_init();
    print("[notes] 启动中\n");
    if !fb_init() {
        print("[notes] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    unsafe { scan_notes(); }
    print("[notes] 就绪，共 ");
    print_dec(unsafe { NOTE_COUNT as u64 });
    print(" 条备忘\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            // 编辑器内系统返回（手势/三键）= 回列表，与标题栏返回、ESC 一致；
            // 桌面/最近任务仍退出应用。
            if ev.nav == NavAction::Back && unsafe { PAGE == 1 } {
                print("[notes] 系统返回 -> 列表\n");
                unsafe {
                    PAGE = 0;
                    SEARCH_FOCUS = false;
                }
                dirty = true;
            } else {
                print("[nav] 系统导航 -> 退出\n");
                unsafe { exit(nav_exit_code(ev.nav)); }
            }
        }

        if let Some((x, y)) = ev.tap {
            if unsafe { PAGE == 0 } {
                ui::tap_list(x, y);
            } else {
                ui::tap_editor(x, y);
            }
            dirty = true;
        }
        // 键盘输入
        match ev.key {
            KeyAction::Char(c) => {
                if unsafe { PAGE == 1 } {
                    unsafe {
                        if EDIT_LEN < EDIT_BUF.len() {
                            EDIT_BUF[EDIT_LEN] = c;
                            EDIT_LEN += 1;
                        }
                    }
                    dirty = true;
                } else if unsafe { SEARCH_FOCUS } {
                    unsafe {
                        if SEARCH_LEN < SEARCH.len() {
                            SEARCH[SEARCH_LEN] = c;
                            SEARCH_LEN += 1;
                        }
                    }
                    dirty = true;
                }
            }
            KeyAction::Backspace => {
                unsafe {
                    if PAGE == 1 {
                        if EDIT_LEN > 0 {
                            EDIT_LEN -= 1;
                        }
                    } else if SEARCH_FOCUS {
                        if SEARCH_LEN > 0 {
                            SEARCH_LEN -= 1;
                        }
                    }
                }
                dirty = true;
            }
            KeyAction::Enter => {
                unsafe {
                    if PAGE == 1 {
                        if EDIT_LEN < EDIT_BUF.len() {
                            EDIT_BUF[EDIT_LEN] = b'\n';
                            EDIT_LEN += 1;
                        }
                    }
                }
                dirty = true;
            }
            KeyAction::Esc => {
                // 编辑器内 ESC = 回列表（逐级返回，标题栏返回已是分级）；
                // 列表页 ESC 才退出应用。
                if unsafe { PAGE == 1 } {
                    unsafe {
                        PAGE = 0;
                        SEARCH_FOCUS = false;
                    }
                    print("[notes] ESC → 返回列表\n");
                    dirty = true;
                } else {
                    print("[notes] ESC -> 退出\n");
                    break;
                }
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[notes] ESC -> 退出\n");
            break;
        }
        if dirty {
            render();
            flip();
            cursor_overlay_redraw();
            dirty = false;
            kit::font_init_checked();
            if kit::font_just_refreshed() { dirty = true; }
        }
        sleep_ms(16);
    }

    print("[notes] 正在退出\n");
    unsafe { exit(0); }
}

fn render() {
    draw_gradient_bg(ds_bg(), glass_dark());
    if unsafe { PAGE == 0 } {
        ui::render_list();
    } else {
        ui::render_editor();
    }
    draw_sys_nav();
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
        write_fd(2, b"PANIC: notes\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
