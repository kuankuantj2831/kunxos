// KunxOS 备忘录应用 UI 模块：列表页（含搜索）+ 编辑器页。
//
// 全部组件来自统一设计系统（ds）：56px 导航栏、56px 列表单元格、
// 24px 系统图标、空状态（64px 图标 + 17px 说明文字）。

use crate::kit::*;
use crate::ds;
use crate::ds::*;

use crate::{
    NOTE_COUNT, NOTE_NAME_LENS, NOTE_NAMES, NOTE_PREVIEW_LENS, NOTE_PREVIEWS,
    NOTE_TITLE_LENS, NOTE_TITLES, PAGE, EDIT_IDX, EDIT_BUF, EDIT_LEN, SEARCH, SEARCH_LEN,
    SEARCH_FOCUS, save_note, delete_note,
};

const LIST_TOP: i32 = ds::NAV_H + 8;
const SEARCH_H: i32 = 40;

/// 列表页：导航栏 + 搜索框 + 备忘录列表 / 空状态。
pub fn render_list() {
    ds_nav_bar(b"\xe5\xa4\x87\xe5\xbf\x98\xe5\xbd\x95", false); // 备忘录
    // 新增按钮（导航栏右侧）
    ds_icon(Icon::Plus, display_w() as i32 - 16 - ICON_SZ, (ds::NAV_H - ICON_SZ) / 2, ICON_SZ, ds::C_ACCENT);

    // 搜索框（液态玻璃输入区，聚焦时补强调色描边）
    let sy = ds::NAV_H + 10;
    draw_glass(16, sy, display_w() as i32 - 32, SEARCH_H, 14);
    let focus = unsafe { SEARCH_FOCUS };
    if focus {
        draw_rect(16, sy, display_w() as i32 - 32, SEARCH_H, ds::C_ACCENT);
    }
    ds_icon(Icon::Search, 24, sy + (SEARCH_H - ICON_SZ) / 2, ICON_SZ, ds::C_DIM);
    let sl = unsafe { SEARCH_LEN };
    let prompt: &[u8] = if sl == 0 && !focus {
        b"\xe6\x90\x9c\xe7\xb4\xa2\xe5\xa4\x87\xe5\xbf\x98\xe5\xbd\x95"
    } else {
        b""
    };
    if !prompt.is_empty() {
        ds_text_px(56, sy + (SEARCH_H - 20) / 2, prompt, 15.0, ds::C_DIM);
    } else {
        let s = unsafe { &SEARCH[..sl] };
        ds_text_px(56, sy + (SEARCH_H - 20) / 2, s, 15.0, ds_text());
    }

    let count = unsafe { NOTE_COUNT };
    if count == 0 {
        if focus && unsafe { SEARCH_LEN } > 0 {
            ds_empty_state(Icon::Search, b"\xe6\x97\xa0\xe5\x8c\xb9\xe9\x85\x8d\xe7\xbb\x93\xe6\x9e\x9c");
        } else {
            ds_empty_state(Icon::Note, b"\xe6\x9a\x82\xe6\x97\xa0\xe5\xa4\x87\xe5\xbf\x98\xe5\xbd\x95");
        }
        return;
    }
    let mut shown = 0usize;
    let mut y = sy + SEARCH_H + 12;
    for i in 0..count {
        if shown >= 10 || y + CELL_H > content_bottom() {
            break;
        }
        if !match_search(i) {
            continue;
        }
        let title = unsafe { &NOTE_TITLES[i][..NOTE_TITLE_LENS[i]] };
        let preview = unsafe { &NOTE_PREVIEWS[i][..NOTE_PREVIEW_LENS[i]] };
        let title_cap = title.len().min(20);
        ds_list_cell(0, y, display_w() as i32, Some(Icon::Note), &title[..title_cap], preview, true);
        ds_cell_sep(0, y + CELL_H, display_w() as i32);
        y += CELL_H;
        shown += 1;
    }
    if shown == 0 {
        ds_empty_state(Icon::Search, b"\xe6\x97\xa0\xe5\x8c\xb9\xe9\x85\x8d\xe7\xbb\x93\xe6\x9e\x9c");
    }
}

fn match_search(i: usize) -> bool {
    let sl = unsafe { SEARCH_LEN };
    if sl == 0 {
        return true;
    }
    let s = unsafe { &SEARCH[..sl] };
    let title = unsafe { &NOTE_TITLES[i][..NOTE_TITLE_LENS[i]] };
    let preview = unsafe { &NOTE_PREVIEWS[i][..NOTE_PREVIEW_LENS[i]] };
    contains_ci(title, s) || contains_ci(preview, s)
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    for i in 0..=hay.len() - needle.len() {
        let mut ok = true;
        for j in 0..needle.len() {
            if lower(hay[i + j]) != lower(needle[j]) {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
    }
    false
}

fn lower(c: u8) -> u8 {
    if c.is_ascii_uppercase() { c + 32 } else { c }
}

/// 编辑器页：导航栏 + 文本区 + 保存/删除按钮。
pub fn render_editor() {
    ds_nav_bar(b"\xe7\xbc\x96\xe8\xbe\x91", true); // 编辑
    let editing = unsafe { EDIT_IDX != usize::MAX };
    let title: &[u8] = if editing {
        let i = unsafe { EDIT_IDX };
        unsafe { &NOTE_TITLES[i][..NOTE_TITLE_LENS[i]] }
    } else {
        b"\xe6\x96\xb0\xe5\xbb\xba\xe5\xa4\x87\xe5\xbf\x98"
    };
    // 标题行（编辑缓冲首行，最多 24 字节）
    let mut tbuf = [0u8; 24];
    let tl = first_line(unsafe { &EDIT_BUF[..EDIT_LEN] }, &mut tbuf);
    let cap = if tl > 0 { tl } else { title.len().min(24) };
    let disp: &[u8] = if tl > 0 { &tbuf[..tl] } else { &title[..cap] };
    let tw = ds_text_width_px(disp, 17.0);
    ds_title_text((display_w() as i32 - tw) / 2, (ds::NAV_H - 20) / 2, disp, ds_text());

    // 文本区
    let ax = 16;
    let ay = ds::NAV_H + 16;
    let aw = display_w() as i32 - 32;
    let ah = 380;
    draw_glass(ax, ay, aw, ah, 16);
    let buf = unsafe { &EDIT_BUF[..EDIT_LEN] };
    let mut yy = ay + 12;
    let mut line = 0usize;
    let mut start = 0usize;
    while line < 16 && start < buf.len() && yy + 22 < ay + ah {
        let mut end = start;
        while end < buf.len() && buf[end] != b'\n' {
            end += 1;
        }
        let ln = &buf[start..end];
        let lc = ln.len().min(40);
        ds_text_px(ax + 12, yy, &ln[..lc], 15.0, ds_text());
        yy += 22;
        line += 1;
        start = end + 1;
    }
    // 光标
    let cy = ay + 12 + (line as i32).min(15) * 22;
    fill_rect(ax + 12, cy - 2, 2, 18, ds::C_ACCENT);

    // 底部提示
    let tip = b"\xe9\x94\xae\xe7\x9b\x98\xe8\xbe\x93\xe5\x85\xa5\xef\xbc\x8c\xe5\x9b\x9e\xe8\xbd\xa6\xe6\x8d\xa2\xe8\xa1\x8c";
    let tw2 = ds_text_width_px(tip, 13.0);
    ds_text_px((display_w() as i32 - tw2) / 2, ay + ah + 14, tip, 13.0, ds::C_DIM);

    // 保存 / 删除
    let bw = (display_w() as i32 - 16 * 3) / 2;
    let bh = 52;
    let by = ay + ah + 56;
    ds_button(16, by, bw, bh, b"\xe4\xbf\x9d\xe5\xad\x98", true);
    if editing {
        // 删除：危险色玻璃按钮
        let dx = 16 + bw + 16;
        draw_glass_style(dx, by, bw, bh, 14, &STYLE_DANGER);
        let tw3 = ds_text_width_px(b"\xe5\x88\xa0\xe9\x99\xa4", 17.0);
        ds_title_text(dx + (bw - tw3) / 2, by + (bh - 20) / 2, b"\xe5\x88\xa0\xe9\x99\xa4", ds::C_TEXT_DARK);
    }
}

fn first_line(buf: &[u8], out: &mut [u8; 24]) -> usize {
    let mut n = 0;
    for &c in buf {
        if c == b'\n' || n >= out.len() {
            break;
        }
        out[n] = c;
        n += 1;
    }
    n
}

/// 列表页点击。
pub fn tap_list(x: i32, y: i32) {
    if ds_back_hit(x, y) {
        print("[notes] 返回 → 退出\n");
        unsafe { exit(0); }
    }
    // 新增按钮
    if x >= display_w() as i32 - 16 - ICON_SZ && x < display_w() as i32 - 16
        && y >= (ds::NAV_H - ICON_SZ) / 2 && y < (ds::NAV_H + ICON_SZ) / 2
    {
        unsafe {
            EDIT_IDX = usize::MAX;
            EDIT_LEN = 0;
            PAGE = 1;
        }
        print("[notes] 打开编辑器（新建）\n");
        return;
    }
    // 搜索框
    let sy = ds::NAV_H + 10;
    if x >= 16 && x < display_w() as i32 - 16 && y >= sy && y < sy + SEARCH_H {
        unsafe { SEARCH_FOCUS = true; }
        return;
    }
    // 列表项
    let count = unsafe { NOTE_COUNT };
    let mut shown = 0usize;
    let mut yy = sy + SEARCH_H + 12;
    for i in 0..count {
        if shown >= 10 || yy + CELL_H > content_bottom() {
            break;
        }
        if !match_search(i) {
            continue;
        }
        if x >= 0 && x < display_w() as i32 && y >= yy && y < yy + CELL_H {
            unsafe {
                EDIT_IDX = i;
                load_into_editor(i);
                PAGE = 1;
            }
            print("[notes] 打开编辑器\n");
            return;
        }
        yy += CELL_H;
        shown += 1;
    }
}

/// 把备忘录 i 的内容读入编辑缓冲。
fn load_into_editor(i: usize) {
    unsafe {
        EDIT_LEN = 0;
        let p = crate::note_path(&NOTE_NAMES[i][..NOTE_NAME_LENS[i]]);
        let mut content = [0u8; 500];
        let cn = crate::read_file_path(&crate::PATH_BUF[..p + 1], &mut content);
        if cn > 0 {
            let c = &content[..cn as usize];
            let n = c.len().min(EDIT_BUF.len());
            EDIT_BUF[..n].copy_from_slice(&c[..n]);
            EDIT_LEN = n;
        }
    }
}

/// 编辑器页点击。
pub fn tap_editor(x: i32, y: i32) {
    if ds_back_hit(x, y) {
        print("[notes] 返回 → 列表\n");
        unsafe { PAGE = 0; }
        return;
    }
    let bw = (display_w() as i32 - 16 * 3) / 2;
    let bh = 52;
    let by = ds::NAV_H + 16 + 380 + 56;
    if ds_button_hit(x, y, 16, by, bw, bh) {
        print("[notes] 保存\n");
        unsafe { save_note(); }
        return;
    }
    if unsafe { EDIT_IDX != usize::MAX }
        && ds_button_hit(x, y, 16 + bw + 16, by, bw, bh)
    {
        print("[notes] 删除\n");
        unsafe { delete_note(); }
        return;
    }
    // 点文本区进入键盘编辑
    if x >= 16 && x < display_w() as i32 - 16 && y >= ds::NAV_H + 16 && y < ds::NAV_H + 16 + 380 {
        unsafe { SEARCH_FOCUS = false; }
    }
}
