// KunxOS 视频应用（EL0 用户态，裸 syscall、no_std、无堆）。
//
// 由 launcher 经 fork + execve 拉起（/bin/video）。职责：
//   1. 视频库列表：扫描 /data/videos 目录，展示本地视频（名称 + 大小）
//   2. 播放器：模拟播放（进度条推进 + 播放/暂停 + 返回列表）
//   3. 空状态：无视频时居中显示图标与提示
//   4. 返回键 / ESC 逐级退出，由 launcher wait4 回收

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[path = "../../../shared/cjk_font_video.rs"]
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

// ==================== 常量 ====================

const SYS_GETDENTS64: usize = 61;
const SYS_NEWFSTATAT: usize = 79;
const MAX_VIDEOS: usize = 32;
const VIDEOS_PATH: &str = "/data/videos";

// ==================== 状态 ====================

static mut VIDEOS: [[u8; 32]; MAX_VIDEOS] = [[0; 32]; MAX_VIDEOS];
static mut VIDEO_LENS: [usize; MAX_VIDEOS] = [0; MAX_VIDEOS];
static mut VIDEO_SIZES: [u32; MAX_VIDEOS] = [0; MAX_VIDEOS];
static mut VIDEO_COUNT: usize = 0;

/// 页面：0 = 视频库，1 = 播放器。
static mut PAGE: u8 = 0;
/// 当前播放的视频索引。
static mut PLAY_IDX: usize = 0;
/// 是否正在播放。
static mut PLAYING: bool = false;
/// 播放进度（秒）。
static mut PROGRESS: u32 = 0;
/// 播放总时长（秒，由文件大小推导）。
static mut DURATION: u32 = 0;
/// 播放计时器（帧计数）。
static mut TICK: u32 = 0;

// ==================== 文件扫描 ====================

unsafe fn scan_videos() {
    VIDEO_COUNT = 0;
    let fd = open(VIDEOS_PATH, O_RDONLY);
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
            if dtyp == 8 && nm.len() > 0 && nm[0] != b'.' && VIDEO_COUNT < MAX_VIDEOS {
                let i = VIDEO_COUNT;
                VIDEOS[i][..nm.len().min(31)].copy_from_slice(&nm[..nm.len().min(31)]);
                VIDEO_LENS[i] = nm.len().min(31);
                VIDEO_SIZES[i] = stat_size(VIDEOS_PATH, nm);
                VIDEO_COUNT += 1;
            }
            off += reclen;
        }
    }
    syscall1(SYS_CLOSE, fd as usize);
}

unsafe fn stat_size(dir: &str, name: &[u8]) -> u32 {
    let mut path = [0u8; 128];
    let mut n = 0;
    for &c in dir.as_bytes() {
        if n < 127 {
            path[n] = c;
            n += 1;
        }
    }
    path[n] = b'/';
    n += 1;
    for &c in name {
        if n < 127 {
            path[n] = c;
            n += 1;
        }
    }
    path[n] = 0;
    let mut st = [0u8; 128];
    let r = syscall4(
        SYS_NEWFSTATAT,
        AT_FDCWD,
        path.as_ptr() as usize,
        st.as_mut_ptr() as usize,
        0,
    );
    if r != 0 {
        return 0;
    }
    let size = i64::from_le_bytes([
        st[48], st[49], st[50], st[51], st[52], st[53], st[54], st[55],
    ]);
    size.max(0) as u32
}

/// 由文件大小推导模拟时长（30s ~ 5min）。
fn derive_duration(size: u32) -> u32 {
    if size == 0 {
        return 60;
    }
    30 + (size % 270)
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    ds_init();
    print("[video] 启动中\n");
    if !fb_init() {
        print("[video] /dev/fb0 mmap 失败，无帧缓冲\n");
        unsafe { exit(1); }
    }
    unsafe { scan_videos(); }
    print("[video] 就绪，共 ");
    print_dec(unsafe { VIDEO_COUNT as u64 });
    print(" 个视频\n");

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            if unsafe { PAGE == 0 } {
                ui::tap_library(x, y);
            } else {
                ui::tap_player(x, y);
            }
            dirty = true;
        }
        match ev.key {
            KeyAction::Esc => {
                print("[video] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[video] ESC -> 退出\n");
            break;
        }

        // 播放计时：每秒推进一次进度。
        if unsafe { PLAYING } {
            unsafe {
                TICK += 1;
                if TICK >= 60 {
                    TICK = 0;
                    PROGRESS += 1;
                    if PROGRESS >= DURATION {
                        PROGRESS = DURATION;
                        PLAYING = false;
                    }
                    dirty = true;
                }
            }
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

    print("[video] 正在退出\n");
    unsafe { exit(0); }
}

fn render() {
    // 液态玻璃基底：整屏渐变背景，替代纯色填充，供面板玻璃模糊取样。
    draw_gradient_bg(ds_bg(), glass_dark());
    if unsafe { PAGE == 0 } {
        ui::render_library();
    } else {
        ui::render_player();
    }
    draw_sys_nav();
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
        write_fd(2, b"PANIC: video\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
