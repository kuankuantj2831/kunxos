// KunxOS 视频应用 UI 模块：视频库列表页 + 播放器页。
//
// 全部组件来自统一设计系统（ds）：56px 导航栏、56px 列表单元格、
// 24px 系统图标、空状态（64px 图标 + 17px 说明文字）。

use crate::kit::*;
use crate::ds;
use crate::ds::*;

use crate::{VIDEO_COUNT, VIDEO_LENS, VIDEO_SIZES, VIDEOS, PAGE, PLAY_IDX, PLAYING, PROGRESS, DURATION, TICK};

const LIST_TOP: i32 = ds::NAV_H + 8;

/// 视频库列表页：导航栏 + 视频列表 / 空状态。
pub fn render_library() {
    ds_nav_bar(b"\xe8\xa7\x86\xe9\xa2\x91", true); // 视频
    let count = unsafe { VIDEO_COUNT };
    if count == 0 {
        ds_empty_state(Icon::Video, b"\xe6\x9a\x82\xe6\x97\xa0\xe8\xa7\x86\xe9\xa2\x91");
        return;
    }
    let show = count.min(12);
    for i in 0..show {
        let y = LIST_TOP + i as i32 * CELL_H;
        if y + CELL_H > content_bottom() {
            break;
        }
        // 名称（截断到 20 字节避免溢出）
        let name = unsafe { &VIDEOS[i][..VIDEO_LENS[i]] };
        let name_cap = name.len().min(18);
        // 副标题：大小 + 时长
        let mut sub = [0u8; 32];
        let mut n = 0;
        n = push_size(&mut sub, n, unsafe { VIDEO_SIZES[i] });
        sub[n] = b' ';
        n += 1;
        n = push_dur(&mut sub, n, duration_of(i));
        ds_list_cell(0, y, display_w() as i32, Some(Icon::Video), &name[..name_cap], &sub[..n], true);
        ds_cell_sep(0, y + CELL_H, display_w() as i32);
    }
}

/// 播放器页：导航栏 + 模拟画面 + 进度条 + 播放/暂停。
pub fn render_player() {
    ds_nav_bar(b"\xe6\x92\xad\xe6\x94\xbe", true); // 播放
    let idx = unsafe { PLAY_IDX };
    let name = unsafe { &VIDEOS[idx][..VIDEO_LENS[idx]] };

    // 模拟画面：玻璃面板容器（16px 圆角），画面内容绘制在玻璃之上
    draw_glass(16, ds::NAV_H + 16, display_w() as i32 - 32, 360, 16);
    let icon_size = 64;
    ds_icon(
        Icon::Video,
        (display_w() as i32 - icon_size) / 2,
        ds::NAV_H + 16 + (360 - icon_size) / 2,
        icon_size,
        ds::C_DIM,
    );
    let name_cap = name.len().min(24);
    let tw = ds_text_width_px(&name[..name_cap], 17.0);
    ds_title_text((display_w() as i32 - tw) / 2, ds::NAV_H + 396, &name[..name_cap], ds_text());

    // 进度条：玻璃轨道 + 强调色进度填充（绘制于玻璃之上）
    let bx = 32;
    let bw = display_w() as i32 - 64;
    let by = ds::NAV_H + 440;
    draw_glass(bx, by, bw, 6, 3);
    let d = unsafe { DURATION }.max(1) as i32;
    let p = unsafe { PROGRESS } as i32;
    let fill_w = (bw * p / d).min(bw);
    fill_rect(bx, by, fill_w, 6, ds::C_ACCENT);

    // 时间
    let mut t = [0u8; 32];
    let mut n = 0;
    n = push_time(&mut t, n, unsafe { PROGRESS });
    t[n] = b' ';
    n += 1;
    t[n] = b'/';
    n += 1;
    t[n] = b' ';
    n += 1;
    n = push_time(&mut t, n, unsafe { DURATION });
    let tw2 = ds_text_width_px(&t[..n], 13.0);
    ds_text_px((display_w() as i32 - tw2) / 2, by + 16, &t[..n], 13.0, ds::C_DIM);

    // 播放/暂停按钮
    let bw2 = 160;
    let bh = 52;
    let bxx = (display_w() as i32 - bw2) / 2;
    let byy = by + 56;
    let label: &[u8] = if unsafe { PLAYING } {
        b"\xe6\x9a\x82\xe5\x81\x9c"
    } else {
        b"\xe6\x92\xad\xe6\x94\xbe"
    };
    ds_button(bxx, byy, bw2, bh, label, true);
}

fn duration_of(i: usize) -> u32 {
    crate::derive_duration(unsafe { VIDEO_SIZES[i] })
}

/// 视频库列表页点击。
pub fn tap_library(x: i32, y: i32) {
    if ds_back_hit(x, y) {
        print("[video] 返回 → 退出\n");
        unsafe { exit(0); }
    }
    let count = unsafe { VIDEO_COUNT };
    if count == 0 {
        return;
    }
    let show = count.min(12);
    for i in 0..show {
        let cy = LIST_TOP + i as i32 * CELL_H;
        if cy + CELL_H > content_bottom() {
            break;
        }
        if x >= 0 && x < display_w() as i32 && y >= cy && y < cy + CELL_H {
            unsafe {
                PLAY_IDX = i;
                PROGRESS = 0;
                TICK = 0;
                PLAYING = true;
                DURATION = crate::derive_duration(VIDEO_SIZES[i]);
                PAGE = 1;
            }
            print("[video] 播放 #");
            print_dec(i as u64);
            print("\n");
            return;
        }
    }
}

/// 播放器页点击。
pub fn tap_player(x: i32, y: i32) {
    if ds_back_hit(x, y) {
        unsafe { PAGE = 0; }
        return;
    }
    // 播放/暂停按钮
    let bw = 160;
    let bh = 52;
    let bx = (display_w() as i32 - bw) / 2;
    let by = ds::NAV_H + 440 + 56;
    if ds_button_hit(x, y, bx, by, bw, bh) {
        unsafe {
            PLAYING = !PLAYING;
            if PLAYING && PROGRESS >= DURATION {
                PROGRESS = 0;
            }
        }
    }
}

// ==================== 数字格式化 ====================

fn push_size(buf: &mut [u8; 32], mut n: usize, size: u32) -> usize {
    if size >= 1024 * 1024 {
        n = push_num(buf, n, size / (1024 * 1024));
        buf[n] = b'M';
        n += 1;
        buf[n] = b'B';
        n += 1;
    } else if size >= 1024 {
        n = push_num(buf, n, size / 1024);
        buf[n] = b'K';
        n += 1;
        buf[n] = b'B';
        n += 1;
    } else {
        n = push_num(buf, n, size);
        buf[n] = b'B';
        n += 1;
    }
    n
}

fn push_dur(buf: &mut [u8; 32], mut n: usize, secs: u32) -> usize {
    buf[n] = b'(';
    n += 1;
    n = push_time(buf, n, secs);
    buf[n] = b')';
    n += 1;
    n
}

fn push_time(buf: &mut [u8; 32], mut n: usize, secs: u32) -> usize {
    let m = secs / 60;
    let s = secs % 60;
    n = push_num(buf, n, m);
    buf[n] = b':';
    n += 1;
    if s < 10 {
        buf[n] = b'0';
        n += 1;
    }
    n = push_num(buf, n, s);
    n
}

fn push_num(buf: &mut [u8; 32], mut n: usize, mut v: u32) -> usize {
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    if v == 0 {
        buf[n] = b'0';
        return n + 1;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    while i < tmp.len() {
        buf[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}
