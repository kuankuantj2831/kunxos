//! 页面模型与页面渲染/命中/分发（核心）。
//!
//! 设置应用采用静态页栈（深度 <= 8）+ 每页滚动偏移；输入页/图案页/确认页
//! 共用上下文标记（Ctx）。本模块是唯一知道每页几何的地方：渲染设置
//! CONTENT_H、命中测试与动作分发（handle_tap / handle_key / 指针拖拽）。

use super::*;

// 共享对话框组件（VIVO OriginOS 风格：居中白卡 + 细分割线 + 文字按钮）。
#[path = "../../../shared/dialog.rs"]
pub(crate) mod dialog;

// ==================== 页面 ID ====================

pub(crate) const PG_ROOT: usize = 0;
pub(crate) const PG_NET: usize = 1;
pub(crate) const PG_WIFI: usize = 2;
pub(crate) const PG_IP: usize = 3;
pub(crate) const PG_BT: usize = 4;
pub(crate) const PG_MOBILE: usize = 5;
pub(crate) const PG_DISPLAY: usize = 6;
pub(crate) const PG_SOUND: usize = 7;
pub(crate) const PG_DND: usize = 8;
pub(crate) const PG_NOTIF: usize = 9;
pub(crate) const PG_PERAPP: usize = 10;
pub(crate) const PG_SEC: usize = 11;
pub(crate) const PG_PIN: usize = 12;
pub(crate) const PG_PASS: usize = 13;
pub(crate) const PG_PATTERN: usize = 14;
pub(crate) const PG_DURESS: usize = 15;
pub(crate) const PG_PERMS: usize = 16;
pub(crate) const PG_PERM: usize = 17;
pub(crate) const PG_ACCT: usize = 18;
pub(crate) const PG_CLOUD: usize = 19;
pub(crate) const PG_SYSTEM: usize = 20;
pub(crate) const PG_ABOUT: usize = 21;
pub(crate) const PG_UPDATE: usize = 22;
pub(crate) const PG_DT: usize = 23;
pub(crate) const PG_LANG: usize = 24;
pub(crate) const PG_RESET: usize = 25;
pub(crate) const PG_DEVOPS: usize = 26;
pub(crate) const PG_BATTERY: usize = 27;
pub(crate) const PG_STORAGE: usize = 28;
pub(crate) const PG_ACCESS: usize = 29;
pub(crate) const PG_INPUT: usize = 30;
pub(crate) const PG_CONFIRM: usize = 31;
pub(crate) const PG_VPN: usize = 32;
pub(crate) const PG_NFC: usize = 33;

// ==================== 上下文 ====================

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Ctx {
    None,
    Search,
    WifiSsid,
    WifiPsk,
    IpAddr,
    IpGw,
    IpDns,
    PinOld,
    PinNew1,
    PinNew2,
    PassOld,
    PassNew1,
    PassNew2,
    UserRename,
    DevName,
    CloudUser,
    CloudPass,
    ConfirmFactory,
    ConfirmReset,
    ConfirmRestart,
}

// ==================== 页面栈与滚动 ====================

pub(crate) static mut PAGE_STACK: [usize; 8] = [0; 8];
pub(crate) static mut PAGE_DEPTH: usize = 1;
pub(crate) static mut SCROLL: i32 = 0;
pub(crate) static mut CONTENT_H: i32 = 0;

pub(crate) static mut SLIDER_GRAB: (i32, i32) = (-1, -1);
static mut PRESS_Y: i32 = 0;
static mut SCROLL_AT_DOWN: i32 = 0;

pub(crate) static mut SEARCH_BUF: [u8; 32] = [0u8; 32];
pub(crate) static mut SEARCH_LEN: usize = 0;

pub(crate) static mut INPUT_BUF: [u8; 64] = [0u8; 64];
pub(crate) static mut INPUT_LEN: usize = 0;
pub(crate) static mut CURR_CTX: Ctx = Ctx::None;

/// 确认对话框（OriginOS 居中模态）：true 时在当前页之上渲染遮罩 + 居中卡片。
/// 不再走页栈 —— 模态弹出/关闭不携带页面转场，背景页保持原位。
pub(crate) static mut CONFIRM_OPEN: bool = false;
static mut CONFIRM_DLG_CTX: Ctx = Ctx::None;

pub(crate) static mut PATTERN_BUF: [u8; 9] = [0u8; 9];
pub(crate) static mut PATTERN_LEN: usize = 0;

/// 安全修改流程阶段：0=输入旧值（如需） 1=新值 2=确认。
pub(crate) static mut FLOW_STAGE: i32 = 0;

/// 权限详情当前应用下标。
pub(crate) static mut PERM_APP: usize = 0;
pub(crate) static mut PERM_BUF: [u8; 1024] = [0u8; 1024];
pub(crate) static mut PERM_LEN: usize = 0;

pub(crate) unsafe fn page_current() -> usize {
    unsafe {
        if PAGE_DEPTH == 0 {
            PG_ROOT
        } else {
            PAGE_STACK[PAGE_DEPTH - 1]
        }
    }
}

/// 主列表首屏入场错峰起点（0 = 不在入场期；P0-6，仅应用启动播放一次）。
pub(crate) static mut ENTER_T0: u64 = 0;

/// 首屏主列表入场错峰推进（P0-6）：每行错峰 25ms、自下方 20px 上滑入位
/// （200ms EaseOut）。阻塞 ~（25ms×行数 + 200ms），输入由内核输入栈缓冲
/// （与页面转场同模式）；滚动/翻页不重播（只在启动时调用一次）。
pub(crate) unsafe fn play_root_enter(rows: usize) {
    let t0 = now_us();
    ENTER_T0 = t0;
    let last_end = t0 + rows.saturating_sub(1) as u64 * 25_000 + 200_000;
    loop {
        crate::full_redraw();
        if now_us() >= last_end {
            ENTER_T0 = 0;
            crate::full_redraw();
            return;
        }
        sleep_ms(8);
    }
}

/// 页面转场播放（阻塞 ~240ms，输入由内核输入栈缓冲）：240ms `FastOutSlowIn`，
/// 每帧渲染新页 + 快照合成 + 翻转（帧预算 8ms，转场节奏稳定）。
unsafe fn play_page_transition(push: bool) {
    let t0 = now_us();
    loop {
        let p = motion::progress(t0, 240_000, now_us());
        let e = motion::Easing::FastOutSlowIn.eval(p);
        full_redraw_transition(e, push);
        if p >= 1.0 {
            break;
        }
        sleep_ms(8);
    }
}

pub(crate) unsafe fn page_push(p: usize) {
    unsafe {
        // 离场页快照（BACK_BUF 当前帧 = 旧页最后渲染结果）。
        snapshot_page();
        if PAGE_DEPTH < PAGE_STACK.len() {
            PAGE_STACK[PAGE_DEPTH] = p;
            PAGE_DEPTH += 1;
        } else {
            PAGE_STACK[PAGE_STACK.len() - 1] = p;
        }
    }
    unsafe { SCROLL = 0; }
    SLIDER_GRAB = (-1, -1);
    play_page_transition(true);
}

pub(crate) unsafe fn page_pop() {
    let popped = unsafe {
        if PAGE_DEPTH > 1 {
            snapshot_page();
            PAGE_DEPTH -= 1;
            true
        } else {
            false
        }
    };
    unsafe { SCROLL = 0; }
    SLIDER_GRAB = (-1, -1);
    if popped {
        play_page_transition(false);
    }
}

pub(crate) unsafe fn max_scroll() -> i32 {
    let vis = list_bottom() - LIST_TOP;
    let c = unsafe { CONTENT_H };
    if c > vis {
        c - vis
    } else {
        0
    }
}

// ==================== 指针反馈 ====================

/// 系统手势保留区标记：本次按下落在全面屏手势区（底部上滑热区
/// `BOTTOM_ZONE` / 左右边缘返回条 `EDGE_W`）时置位 —— 保留区内不参与
/// 滚动与滑条拖拽，抬起时让位给系统导航分类。否则底部上滑会被列表
/// 滚动吞成 sliding（`pointer_up` 返回 true 抑制导航分类），
/// 「上划回桌面」在全部设置页失效。
static mut NAV_EXCLUDE: bool = false;

/// 滑条抓取期间上一次写入的 x：纯竖直移动（x 未变）不算变化，不写配置
/// 不重绘（此前按住滑条上下晃每次 ABS_Y/REL_Y 都触发 cfg 落盘 + 整屏重绘）。
static mut GRAB_X: i32 = -1;

/// 按下：探测滑块抓取 / 记录滚动起点（系统手势保留区内不抓滑条）。
pub(crate) unsafe fn pointer_down(x: i32, y: i32) {
    unsafe {
        PRESS_Y = y;
        SCROLL_AT_DOWN = SCROLL;
        // 保留区仅在全面屏手势模式下有意义：buttons 模式 classify_swipe 恒
        // None、导航命中只看 NAV_Y 以下，保留区只会把列表最后 1~2 行的滚动
        // 起点变成死区（BOTTOM_ZONE=130 比导航条本身高 74px）。
        NAV_EXCLUDE = sys_nav::nav_mode() == sys_nav::MODE_GESTURE
            && (y >= display_h() as i32 - sys_nav::BOTTOM_ZONE
                || x < sys_nav::EDGE_W
                || x >= display_w() as i32 - sys_nav::EDGE_W);
    }
    SLIDER_GRAB = if unsafe { NAV_EXCLUDE || CONFIRM_OPEN } {
        (-1, -1)
    } else {
        slider_at(page_current(), x, y)
    };
    unsafe { GRAB_X = x };
}

/// 本次按下的 y（鼠标拖拽以「按下点 + REL 累计」合成绝对坐标用）。
pub(crate) fn press_y() -> i32 {
    unsafe { PRESS_Y }
}

/// 拖拽更新：滑块抓取时改值（用 CUR_X），否则滚动（保留区手势不滚动）。
/// 返回是否有实际变化 —— 调用方据此决定是否整屏重绘：无变化的手势帧
/// （如全面屏上滑/边缘返回）不得置 redraw，否则主循环的重绘分支会在
/// 导航检查之前 `continue`，把本次手势的 nav 动作整个吞掉。
pub(crate) unsafe fn drag_to(y: i32) -> bool {
    unsafe {
        // 确认对话框打开时禁止滚动/滑条拖拽（模态）。
        if CONFIRM_OPEN {
            return false;
        }
        if NAV_EXCLUDE {
            return false;
        }
        let (pg, si) = SLIDER_GRAB;
        if si >= 0 {
            let x = CUR_X;
            if x == unsafe { GRAB_X } {
                return false;
            }
            unsafe { GRAB_X = x };
            slider_set(pg, si, x);
            true
        } else {
            let ns = (SCROLL_AT_DOWN - (y - PRESS_Y)).clamp(0, max_scroll());
            let changed = ns != SCROLL;
            SCROLL = ns;
            changed
        }
    }
}

/// 滚轮滚动；返回是否实际变化（已钳在 0/max 时不再置 redraw）。
pub(crate) unsafe fn wheel_by(v: i32) -> bool {
    unsafe {
        let pg = page_current();
        if pg == PG_INPUT || pg == PG_PATTERN || unsafe { CONFIRM_OPEN } {
            return false;
        }
        let ns = (SCROLL - v * 48).clamp(0, max_scroll());
        let changed = ns != SCROLL;
        SCROLL = ns;
        changed
    }
}

/// 抬起：返回是否发生了拖拽（抑制点击）。保留区手势不算拖拽 ——
/// 导航分类必须照常执行（上滑=Home / 边缘横滑=返回）。
pub(crate) unsafe fn pointer_up(_px: i32, _py: i32, cx: i32, cy: i32) -> bool {
    let sliding = !unsafe { NAV_EXCLUDE }
        && ((cy - unsafe { PRESS_Y }).abs() > 10 || unsafe { SLIDER_GRAB.1 } >= 0);
    SLIDER_GRAB = (-1, -1);
    unsafe { NAV_EXCLUDE = false };
    let _ = cx;
    sliding
}

// ==================== 滑块映射 ====================

/// 返回 (页面, 滑块行号)：当前页哪个滑块位于 (x,y)。
unsafe fn slider_at(pg: usize, _x: i32, y: i32) -> (i32, i32) {
    match pg {
        PG_DISPLAY => {
            if y >= 64 && y < 120 {
                return (PG_DISPLAY as i32, 0);
            }
            (pg as i32, -1)
        }
        PG_SOUND => {
            let dy = y - 64;
            if dy >= 0 && dy < 4 * ROW_H {
                let row = dy / ROW_H;
                return (PG_SOUND as i32, row);
            }
            (pg as i32, -1)
        }
        _ => (pg as i32, -1),
    }
}

unsafe fn slider_set(_pg: i32, row: i32, x: i32) {
    let sx = ui::slider_x();
    let v = ui::slider_value(x, sx);
    if _pg as usize == PG_DISPLAY && row == 0 {
        unsafe {
            cfg::cfg_set_int(cfg::CONF_DISPLAY, b"brightness", v);
        }
    } else if _pg as usize == PG_SOUND && row >= 0 && row < 4 {
        let key: &[u8] = match row {
            0 => b"vol_media",
            1 => b"vol_ring",
            2 => b"vol_alarm",
            _ => b"vol_call",
        };
        unsafe {
            cfg::cfg_set_int(cfg::CONF_SOUND, key, v);
        }
    }
}

// ==================== 渲染分发 ====================

pub(crate) unsafe fn render() {
    let page = page_current();
    match page {
        PG_ROOT => render_root(),
        PG_NET => pages2::render_net(),
        PG_WIFI => pages2::render_wifi(),
        PG_IP => pages2::render_ip(),
        PG_BT => pages2::render_bt(),
        PG_MOBILE => pages2::render_mobile(),
        PG_VPN => pages2::render_vpn(),
        PG_NFC => pages2::render_nfc(),
        PG_DISPLAY => render_display(),
        PG_SOUND => render_sound(),
        PG_DND => pages2::render_dnd(),
        PG_NOTIF => render_notif(),
        PG_PERAPP => render_perapp(),
        PG_SEC => render_sec(),
        PG_PIN => render_pin(),
        PG_PASS => render_pass(),
        PG_PATTERN => render_pattern(),
        PG_DURESS => render_duress(),
        PG_PERMS => pages2::render_perms(),
        PG_PERM => pages2::render_perm(),
        PG_ACCT => pages2::render_acct(),
        PG_CLOUD => pages2::render_cloud(),
        PG_SYSTEM => pages2::render_system(),
        PG_ABOUT => pages2::render_about(),
        PG_UPDATE => pages2::render_update(),
        PG_DT => pages2::render_dt(),
        PG_LANG => pages2::render_lang(),
        PG_RESET => render_reset(),
        PG_DEVOPS => pages2::render_devops(),
        PG_BATTERY => pages2::render_battery(),
        PG_STORAGE => pages2::render_storage(),
        PG_ACCESS => pages2::render_access(),
        PG_INPUT => render_input(),
        _ => render_root(),
    }
    // 确认对话框（OriginOS 居中模态）覆盖在当前页之上。
    if unsafe { CONFIRM_OPEN } {
        render_confirm_dialog();
    }
}

// ==================== 主页面（搜索） ====================

pub(crate) unsafe fn draw_search_bar() {
    ui::draw_glass(SEARCH_X, SEARCH_Y, search_w(), SEARCH_H, SEARCH_R);
    unsafe {
        if SEARCH_LEN > 0 {
            let mut txt = [0u8; 40];
            txt[..SEARCH_LEN].copy_from_slice(&SEARCH_BUF[..SEARCH_LEN]);
            txt[SEARCH_LEN] = b'|';
            draw_text_px(
                SEARCH_X + 16,
                text_top(15.0, SEARCH_Y + SEARCH_H / 2),
                &txt[..SEARCH_LEN + 1],
                15.0,
                ui::c_text(),
            );
            // 清除叉号：Phosphor x 矢量字形（原 8x8 点阵 2 倍放大的中心不变）。
            if !draw_icon_glyph_centered(
                SEARCH_X + search_w() - 18,
                SEARCH_Y + 19,
                ttf_text::font_render::icons::settings::X,
                14.0,
                C_SUB,
            ) {
                draw_bits(SEARCH_X + search_w() - 26, SEARCH_Y + 11, &crate::FONT_X, 2, C_SUB);
            }
        } else {
            draw_text_px(
                SEARCH_X + 16,
                text_top(15.0, SEARCH_Y + SEARCH_H / 2),
                b"\xE6\x90\x9C\xE7\xB4\xA2\xE8\xAE\xBE\xE7\xBD\xAE",
                15.0,
                C_SUB,
            );
        }
    }
}

unsafe fn render_root() {
    ui::draw_gradient_bg();
    draw_search_bar();
    let s = unsafe { SCROLL };
    let y = LIST_TOP - s;
    unsafe {
        if SEARCH_LEN > 0 {
            let mut idx = 0usize;
            for e in SEARCH_LIST.iter() {
                if find_sub(e.title, &SEARCH_BUF[..SEARCH_LEN]) {
                    ui::draw_row(y + idx as i32 * ROW_H, e.icon, e.title, e.value, true, false);
                    idx += 1;
                }
            }
            CONTENT_H = idx as i32 * ROW_H;
            return;
        }
    }
    let cats: [(&[u8], usize, usize); 10] = [
        (b"\xE7\xBD\x91\xE7\xBB\x9C\xE4\xB8\x8E\xE8\xBF\x9E\xE6\x8E\xA5", I_WIFI, PG_NET),
        (b"\xE6\x98\xBE\xE7\xA4\xBA\xE4\xB8\x8E\xE4\xBA\xAE\xE5\xBA\xA6", I_SUN, PG_DISPLAY),
        (b"\xE5\xA3\xB0\xE9\x9F\xB3\xE4\xB8\x8E\xE6\x8C\xAF\xE5\x8A\xA8", I_SPEAK, PG_SOUND),
        (b"\xE9\x80\x9A\xE7\x9F\xA5", I_BELL, PG_NOTIF),
        (b"\xE5\xAE\x89\xE5\x85\xA8\xE4\xB8\x8E\xE9\x9A\x90\xE7\xA7\x81", I_LOCK, PG_SEC),
        (b"\xE8\xB4\xA6\xE6\x88\xB7", I_USER, PG_ACCT),
        (b"\xE7\xB3\xBB\xE7\xBB\x9F", I_GEAR, PG_SYSTEM),
        (b"\xE7\x94\xB5\xE6\xB1\xA0", I_BATT, PG_BATTERY),
        (b"\xE5\xAD\x98\xE5\x82\xA8", I_DISK, PG_STORAGE),
        (b"\xE8\xBE\x85\xE5\x8A\xA9\xE5\x8A\x9F\xE8\x83\xBD", I_ACC, PG_ACCESS),
    ];
    for (i, (name, icon, _)) in cats.iter().enumerate() {
        let mut ry = y + i as i32 * ROW_H;
        // 首屏入场错峰（P0-6）：行未到出场时间不绘制；出场后自下方 20px
        // 上滑入位（200ms EaseOut，行间错峰 25ms）。
        if ENTER_T0 != 0 {
            let p = motion::progress(ENTER_T0 + i as u64 * 25_000, 200_000, now_us());
            if p <= 0.0 {
                continue;
            }
            if p < 1.0 {
                ry += (20.0 * (1.0 - motion::Easing::ColorOSStandard.eval(p))) as i32;
            }
        }
        let value: &[u8] = match *name {
            b"\xE7\xBD\x91\xE7\xBB\x9C\xE4\xB8\x8E\xE8\xBF\x9E\xE6\x8E\xA5" => b"Wi-Fi / \xE8\x93\x9D\xE7\x89\x99 / \xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE",
            b"\xE6\x98\xBE\xE7\xA4\xBA\xE4\xB8\x8E\xE4\xBA\xAE\xE5\xBA\xA6" => b"\xE4\xBA\xAE\xE5\xBA\xA6 / \xE6\xB7\xB1\xE8\x89\xB2 / \xE5\xAD\x97\xE4\xBD\x93",
            b"\xE5\xA3\xB0\xE9\x9F\xB3\xE4\xB8\x8E\xE6\x8C\xAF\xE5\x8A\xA8" => b"\xE9\x9F\xB3\xE9\x87\x8F / \xE9\x93\x83\xE5\xA3\xB0 / \xE5\x8B\xBF\xE6\x89\xB0",
            b"\xE9\x80\x9A\xE7\x9F\xA5" => b"\xE6\x8E\xA8\xE9\x80\x81 / \xE9\xA2\x84\xE8\xA7\x88 / \xE5\xBA\x94\xE7\x94\xA8",
            b"\xE5\xAE\x89\xE5\x85\xA8\xE4\xB8\x8E\xE9\x9A\x90\xE7\xA7\x81" => b"\xE9\x94\x81\xE5\xB1\x8F / \xE6\x9D\x83\xE9\x99\x90 / \xE4\xBD\x8D\xE7\xBD\xAE",
            b"\xE8\xB4\xA6\xE6\x88\xB7" => b"\xE8\xAE\xBE\xE5\xA4\x87 / \xE4\xBA\x91 / \xE5\x90\x8C\xE6\xAD\xA5",
            b"\xE7\xB3\xBB\xE7\xBB\x9F" => b"\xE5\x85\xB3\xE4\xBA\x8E / \xE6\x9B\xB4\xE6\x96\xB0 / \xE6\x97\xB6\xE5\x8C\xBA",
            b"\xE7\x94\xB5\xE6\xB1\xA0" => b"\xE7\x94\xB5\xE9\x87\x8F / \xE4\xBD\x8E\xE7\x94\xB5\xE9\x87\x8F\xE6\xA8\xA1\xE5\xBC\x8F",
            b"\xE5\xAD\x98\xE5\x82\xA8" => b"\xE4\xBD\xBF\xE7\x94\xA8\xE6\x83\x85\xE5\x86\xB5 / \xE6\xB8\x85\xE7\x90\x86",
            _ => b"",
        };
        ui::draw_row(ry, *icon, name, value, true, false);
    }
    unsafe { CONTENT_H = 10 * ROW_H; }
}

// ==================== 显示页 ====================

unsafe fn font_size_value() -> &'static [u8] {
    match cfg::cfg_get_int(cfg::CONF_DISPLAY, b"font_size", 1) {
        0 => b"\xE5\xB0\x8F",
        2 => b"\xE5\xA4\xA7",
        3 => b"\xE8\xB6\x85\xE5\xA4\xA7",
        _ => b"\xE6\xA0\x87\xE5\x87\x86",
    }
}

unsafe fn render_display() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE6\x98\xBE\xE7\xA4\xBA\xE4\xB8\x8E\xE4\xBA\xAE\xE5\xBA\xA6");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    // 亮度行（滑块）。y: LIST_TOP ..+ROW_H，滑块中心 y+ROW_H-20。
    let br = cfg::brightness_level();
    let mut num = [0u8; 12];
    let bn = fmt_dec(&mut num, br as u32);
    let mut vv = [0u8; 16];
    vv[..bn].copy_from_slice(&num[..bn]);
    vv[bn] = b'%';
    ui::draw_glass(MARGIN, y, card_w(), ROW_H, CARD_R);
    draw_text_px(
        MARGIN + 4,
        text_top(font_px(), y + ROW_H / 2),
        b"\xE4\xBA\xAE\xE5\xBA\xA6",
        font_px(),
        ui::c_text(),
    );
    let vw = text_width_px(&vv[..bn + 1], 14.0);
    draw_text_px(
        display_w() as i32 - MARGIN - vw,
        text_top(14.0, y + ROW_H / 2),
        &vv[..bn + 1],
        14.0,
        C_SUB,
    );
    ui::draw_slider(ui::slider_x(), y + ROW_H - 18, br);
    y += ROW_H + HEADER_H;

    let dark = cfg::dark_on();
    let rows: [(&[u8], usize, usize); 5] = [
        (b"\xE6\xB7\xB1\xE8\x89\xB2\xE6\xA8\xA1\xE5\xBC\x8F", I_MOON, 0),
        (b"\xE5\xAD\x97\xE4\xBD\x93\xE5\xA4\xA7\xE5\xB0\x8F", I_FONT, 1),
        (b"\xE5\x88\x86\xE8\xBE\xA8\xE7\x8E\x87", I_SUN, 2),
        (b"\xE5\x88\xB7\xE6\x96\xB0\xE7\x8E\x87", I_CLOCK, 3),
        (b"\xE8\x87\xAA\xE5\x8A\xA8\xE6\x97\x8B\xE8\xBD\xAC", I_ACC, 4),
    ];
    let card_h = ui::card_h(5, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon, _)) in rows.iter().enumerate() {
        let value: &[u8] = match i {
            1 => font_size_value(),
            2 => b"540x960",
            3 => {
                if cfg::cfg_get_int(cfg::CONF_DISPLAY, b"refresh_rate", 120) == 60 {
                    b"60Hz"
                } else {
                    b"120Hz"
                }
            }
            _ => b"",
        };
        let sw = i == 0 || i == 4;
        if sw {
            if i == 0 {
                ui::draw_row(ry, *icon, title, value, false, false);
                ui::draw_switch(
                    display_w() as i32 - MARGIN - 48,
                    ry + (ROW_H - 28) / 2,
                    dark,
                );
            } else {
                let rot = cfg::cfg_get_int(cfg::CONF_DISPLAY, b"auto_rotate", 0) != 0;
                ui::draw_row(ry, *icon, title, value, false, false);
                ui::draw_switch(display_w() as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, rot);
            }
        } else {
            ui::draw_row(ry, *icon, title, value, true, false);
        }
        if i + 1 < 5 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    let _ = dark;
    y += card_h + HEADER_H;

    // 说明。
    ui::draw_group_header(y, b"\xE8\xAF\xB4\xE6\x98\x8E");
    y += HEADER_H;
    let card_h2 = ui::card_h(1, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h2, CARD_R);
    draw_text_px(
        MARGIN + 8,
        text_top(13.0, y + card_h2 / 2),
        b"\xE4\xBA\xAE\xE5\xBA\xA6 / \xE6\xB7\xB1\xE8\x89\xB2 / \xE5\xAD\x97\xE4\xBD\x93\xE5\x9C\xA8\xE6\x9C\xAC\xE5\xBA\x94\xE7\x94\xA8\xE5\x86\x85\xE5\x8D\xB3\xE6\x97\xB6\xE7\x94\x9F\xE6\x95\x88\xEF\xBC\x9B\xE5\x88\x86\xE8\xBE\xA8\xE7\x8E\x87\xE5\x9B\xBA\xE5\xAE\x9A 540x960\xEF\xBC\x9B\xE5\x88\xB7\xE6\x96\xB0\xE7\x8E\x87 120Hz",
        13.0,
        C_SUB,
    );
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 声音页 ====================

pub(crate) unsafe fn vol_value(key: &[u8]) -> i32 {
    cfg::cfg_get_int(cfg::CONF_SOUND, key, 60).clamp(0, 100)
}

unsafe fn rt_label() -> &'static [u8] {
    let mut r = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_SOUND, b"ringtone", b"classic", &mut r);
    if n > 0 {
        if r[0] == b'p' {
            return b"\xE8\x84\x89\xE5\x86\xB2";
        }
        if r[0] == b'd' {
            return b"\xE5\x8F\x8C\xE9\x9F\xB3";
        }
    }
    b"\xE7\xBB\x8F\xE5\x85\xB8"
}

unsafe fn render_sound() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xA3\xB0\xE9\x9F\xB3\xE4\xB8\x8E\xE6\x8C\xAF\xE5\x8A\xA8");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    unsafe { CONTENT_H = 0; }

    let keys: [(&[u8], &[u8]); 4] = [
        (b"\xE5\xAA\x92\xE4\xBD\x93\xE9\x9F\xB3\xE9\x87\x8F", b"vol_media"),
        (b"\xE9\x93\x83\xE5\xA3\xB0\xE9\x9F\xB3\xE9\x87\x8F", b"vol_ring"),
        (b"\xE9\x97\xB9\xE9\x92\x9F\xE9\x9F\xB3\xE9\x87\x8F", b"vol_alarm"),
        (b"\xE9\x80\x9A\xE8\xAF\x9D\xE9\x9F\xB3\xE9\x87\x8F", b"vol_call"),
    ];
    let card_h = ui::card_h(4, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, key)) in keys.iter().enumerate() {
        let v = vol_value(key);
        let mut num = [0u8; 12];
        let bn = fmt_dec(&mut num, v as u32);
        let mut vv = [0u8; 16];
        vv[..bn].copy_from_slice(&num[..bn]);
        vv[bn] = b'%';
        draw_text_px(
            MARGIN + 4,
            text_top(font_px(), ry + ROW_H / 2),
            title,
            font_px(),
            ui::c_text(),
        );
        let vw = text_width_px(&vv[..bn + 1], 14.0);
        draw_text_px(
            display_w() as i32 - MARGIN - vw,
            text_top(14.0, ry + ROW_H / 2),
            &vv[..bn + 1],
            14.0,
            C_SUB,
        );
        ui::draw_slider(ui::slider_x(), ry + ROW_H - 18, v);
        if i + 1 < 4 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;

    // 铃声 / 振动 / 勿扰。
    let rows: [(&[u8], usize, usize); 3] = [
        (b"\xE9\x93\x83\xE5\xA3\xB0", I_BELL, 0),
        (b"\xE6\x8C\xAF\xE5\x8A\xA8", I_ACC, 1),
        (b"\xE5\x8B\xBF\xE6\x89\xB0\xE6\xA8\xA1\xE5\xBC\x8F", I_MOON, 2),
    ];
    let card_h2 = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon, _)) in rows.iter().enumerate() {
        match i {
            0 => ui::draw_row(ry, *icon, title, rt_label(), true, false),
            1 => {
                let on = cfg::cfg_get_int(cfg::CONF_SOUND, b"vibration", 1) != 0;
                ui::draw_row(ry, *icon, title, b"", false, false);
                ui::draw_switch(display_w() as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
            }
            _ => {
                let on = cfg::cfg_get_int(cfg::CONF_SOUND, b"dnd_enabled", 0) != 0;
                ui::draw_row(ry, *icon, title, if on { b"\xE5\xB7\xB2\xE5\xBC\x80\xE5\x90\xAF" } else { b"\xE5\xB7\xB2\xE5\x85\xB3\xE9\x97\xAD" }, true, false);
            }
        }
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h2 + HEADER_H;
    unsafe { CONTENT_H = y - LIST_TOP; }
}

// ==================== 通知页 ====================

unsafe fn preview_label() -> &'static [u8] {
    let mut p = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_NOTIF, b"preview", b"full", &mut p);
    if n > 0 {
        if p[0] == b'h' {
            return b"\xE9\x9A\x90\xE8\x97\x8F\xE6\xA0\x87\xE9\xA2\x98";
        }
        if p[0] == b'o' {
            return b"\xE5\x85\xB3\xE9\x97\xAD";
        }
    }
    b"\xE6\x98\xBE\xE7\xA4\xBA\xE5\x86\x85\xE5\xAE\xB9"
}

unsafe fn render_notif() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE9\x80\x9A\xE7\x9F\xA5");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;
    let rows: [(&[u8], usize, usize); 3] = [
        (b"\xE6\x8E\xA8\xE9\x80\x81\xE6\x9C\x8D\xE5\x8A\xA1", I_BELL, 0),
        (b"\xE9\x94\x81\xE5\xB1\x8F\xE9\xA2\x84\xE8\xA7\x88", I_LOCK, 1),
        (b"\xE5\xBA\x94\xE7\x94\xA8\xE9\x80\x9A\xE7\x9F\xA5\xE7\xAE\xA1\xE7\x90\x86", I_APPS, 2),
    ];
    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon, _)) in rows.iter().enumerate() {
        match i {
            0 => {
                let on = cfg::cfg_get_int(cfg::CONF_NOTIF, b"push_enabled", 1) != 0;
                ui::draw_row(ry, *icon, title, b"", false, false);
                ui::draw_switch(display_w() as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
            }
            1 => ui::draw_row(ry, *icon, title, preview_label(), true, false),
            _ => ui::draw_row(ry, *icon, title, b"", true, false),
        }
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;
    CONTENT_H = y - LIST_TOP;
}

unsafe fn style_label(v: i32) -> &'static [u8] {
    match v {
        0 => b"\xE9\x9D\x99\xE9\xBB\x98",
        2 => b"\xE5\x85\xB3\xE9\x97\xAD",
        _ => b"\xE6\xA8\xAA\xE5\xB9\x85",
    }
}

unsafe fn render_perapp() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xBA\x94\xE7\x94\xA8\xE9\x80\x9A\xE7\x9F\xA5\xE7\xAE\xA1\xE7\x90\x86");
    let s = unsafe { SCROLL };
    let y = LIST_TOP - s;
    if APP_COUNT == 0 {
        let card_h = ui::card_h(1, false);
        ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
        ui::draw_row(y + CARD_PAD, I_INFO, b"\xE6\x97\xA0\xE5\xBA\x94\xE7\x94\xA8\xE6\xB8\x85\xE5\x8D\x95", b"", false, false);
        CONTENT_H = card_h;
        return;
    }
    let card_h = ui::card_h(APP_COUNT, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    for i in 0..APP_COUNT {
        let name = &APP_NAMES[i];
        let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let mut key = [0u8; 32];
        let kn = fmt_app_key(&name[..ln], &mut key);
        let mut kv = [0u8; 16];
        cfg::cfg_get_str(cfg::CONF_NOTIF, &key[..kn], b"banner", &mut kv);
        let style = if kv[0] == b's' {
            0
        } else if kv[0] == b'o' {
            2
        } else {
            1
        };
        let ry = y + CARD_PAD + i as i32 * ROW_H;
        ui::draw_row(ry, I_APPS, &name[..ln], style_label(style), false, false);
        if i + 1 < APP_COUNT {
            ui::draw_divider(ry);
        }
    }
    CONTENT_H = card_h;
}

/// 生成 per-app 配置 key（app_<name>）到 out，返回长度。
unsafe fn fmt_app_key(name: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    for &c in b"app_" {
        out[n] = c;
        n += 1;
    }
    let ln = name.len().min(out.len() - n);
    out[n..n + ln].copy_from_slice(&name[..ln]);
    n + ln
}

// ==================== 安全页 ====================

unsafe fn lock_type_label() -> &'static [u8] {
    let mut t = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_SEC, b"lock_type", b"slide", &mut t);
    if n > 0 {
        if t[0] == b'p' && n > 1 && t[1] == b'i' {
            return b"PIN";
        }
        if t[0] == b'p' && n > 2 && t[1] == b'a' && t[2] == b't' {
            return b"\xE5\x9B\xBE\xE6\xA1\x88";
        }
        if t[0] == b'p' && n > 2 && t[1] == b'a' && t[2] == b's' {
            return b"\xE5\xAF\x86\xE7\xA0\x81";
        }
    }
    b"\xE6\xBB\x91\xE5\x8A\xA8"
}

unsafe fn duress_label() -> &'static [u8] {
    let mut a = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_SEC, b"duress_action", b"fake_system", &mut a);
    if n > 0 {
        if a[0] == b'w' {
            return b"\xE6\x93\xA6\xE9\x99\xA4\xE6\x95\xB0\xE6\x8D\xAE";
        }
        if a[0] == b'a' {
            return b"\xE5\x8F\x91\xE9\x80\x81\xE8\xAD\xA6\xE6\x8A\xA5";
        }
    }
    b"\xE4\xBC\xAA\xE8\xA3\x85\xE7\xB3\xBB\xE7\xBB\x9F"
}

unsafe fn render_sec() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE5\xAE\x89\xE5\x85\xA8\xE4\xB8\x8E\xE9\x9A\x90\xE7\xA7\x81");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;

    let rows: [(&[u8], usize); 5] = [
        (b"\xE9\x94\x81\xE5\xB1\x8F\xE6\x96\xB9\xE5\xBC\x8F", I_LOCK),
        (b"\xE4\xBF\xAE\xE6\x94\xB9 PIN", I_LOCK),
        (b"\xE4\xBF\xAE\xE6\x94\xB9\xE5\xAF\x86\xE7\xA0\x81", I_INPUT),
        (b"\xE4\xBF\xAE\xE6\x94\xB9\xE5\x9B\xBE\xE6\xA1\x88", I_PAT),
        (b"\xE8\x83\x81\xE8\xBF\xAB PIN \xE4\xB8\x8E\xE5\x8A\xA8\xE4\xBD\x9C", I_FINGER),
    ];
    let card_h = ui::card_h(5, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon)) in rows.iter().enumerate() {
        let value: &[u8] = if i == 0 { lock_type_label() } else if i == 4 { duress_label() } else { b"" };
        ui::draw_row(ry, *icon, title, value, true, false);
        if i + 1 < 5 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;

    let rows2: [(&[u8], usize); 3] = [
        (b"\xE5\xBA\x94\xE7\x94\xA8\xE6\x9D\x83\xE9\x99\x90", I_APPS),
        (b"\xE4\xBD\x8D\xE7\xBD\xAE\xE6\x9C\x8D\xE5\x8A\xA1", I_EYE),
        (b"\xE9\x87\x8D\xE7\xBD\xAE\xE5\xB9\xBF\xE5\x91\x8A\xE6\xA0\x87\xE8\xAF\x86\xE7\xAC\xA6", I_INFO),
    ];
    let card_h2 = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon)) in rows2.iter().enumerate() {
        if i == 1 {
            let on = cfg::cfg_get_int(cfg::CONF_SEC, b"location", 1) != 0;
            ui::draw_row(ry, *icon, title, b"", false, false);
            ui::draw_switch(display_w() as i32 - MARGIN - 48, ry + (ROW_H - 28) / 2, on);
        } else {
            ui::draw_row(ry, *icon, title, b"", true, false);
        }
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h2 + HEADER_H;
    CONTENT_H = y - LIST_TOP;
}

// ==================== 锁屏修改页（PIN/密码） ====================

unsafe fn render_pin() {
    render_secret_input(b"\xE4\xBF\xAE\xE6\x94\xB9 PIN", true);
}

unsafe fn render_pass() {
    render_secret_input(b"\xE4\xBF\xAE\xE6\x94\xB9\xE5\xAF\x86\xE7\xA0\x81", false);
}

unsafe fn input_masked() -> bool {
    match unsafe { CURR_CTX } {
        Ctx::WifiPsk
        | Ctx::PinOld
        | Ctx::PinNew1
        | Ctx::PinNew2
        | Ctx::PassOld
        | Ctx::PassNew1
        | Ctx::PassNew2
        | Ctx::CloudPass => true,
        _ => false,
    }
}

unsafe fn input_numeric() -> bool {
    matches!(
        unsafe { CURR_CTX },
        Ctx::PinOld | Ctx::PinNew1 | Ctx::PinNew2
    )
}

unsafe fn render_secret_input(title: &[u8], numeric: bool) {
    ui::draw_gradient_bg();
    ui::draw_title_bar(title);
    let fx = 24;
    let fw = display_w() as i32 - 2 * fx;
    let fy = 84;
    ui::draw_glass(fx, fy, fw, 52, 12);
    let mut disp = [0u8; 64];
    let mut dn = 0;
    for i in 0..unsafe { INPUT_LEN } {
        if dn >= disp.len() {
            break;
        }
        disp[dn] = if input_masked() { b'*' } else { unsafe { INPUT_BUF[i] } };
        dn += 1;
    }
    if dn < disp.len() {
        disp[dn] = b'|';
        dn += 1;
    }
    draw_text_px(fx + 12, text_top(17.0, fy + 26), &disp[..dn], 17.0, ui::c_text());

    let hint: &[u8] = match unsafe { FLOW_STAGE } {
        1 => b"\xE8\xAF\xB7\xE8\xBE\x93\xE5\x85\xA5\xE6\x96\xB0\xE5\x80\xBC",
        2 => b"\xE8\xAF\xB7\xE5\x86\x8D\xE6\xAC\xA1\xE8\xBE\x93\xE5\x85\xA5\xE7\xA1\xAE\xE8\xAE\xA4",
        _ => b"\xE8\xAF\xB7\xE8\xBE\x93\xE5\x85\xA5\xE5\xBD\x93\xE5\x89\x8D\xE5\x80\xBC\xEF\xBC\x88\xE5\xA6\x82\xE6\x97\xA0\xE5\xAF\x86\xE7\xA0\x81\xE5\x88\x99\xE7\x9B\xB4\xE6\x8E\xA5\xE8\xBE\x93\xE5\x85\xA5\xE6\x96\xB0\xE5\x80\xBC\xEF\xBC\x89",
    };
    draw_text_px(fx + 4, fy + 64, hint, 13.0, C_SUB);

    if numeric {
        draw_secret_keypad_hint();
    }
    draw_text_px(fx + 4, fy + 96, b"\xE6\x94\xAF\xE6\x8C\x81\xE7\x89\xA9\xE7\x90\x86\xE9\x94\xAE\xE7\x9B\x98\xEF\xBC\x9BEnter \xE7\xA1\xAE\xE8\xAE\xA4 / Backspace \xE5\x88\xA0\xE9\x99\xA4 / Esc \xE8\xBF\x94\xE5\x9B\x9E", 12.0, C_SUB);
    CONTENT_H = fy + 120;
}

unsafe fn draw_secret_keypad_hint() {
    // 3x4 数字键盘。
    let kx = (display_w() as i32 - 3 * 90 - 2 * 16) / 2;
    let ky = 180;
    for r in 0..4 {
        for c in 0..3 {
            let x = kx + c * 106;
            let y2 = ky + r * 66;
            ui::draw_glass(x, y2, 90, 56, 14);
            let mut tmp = [0u8; 4];
            let ch: &[u8] = match (r, c) {
                (3, 0) => b"<-",
                (3, 1) => b"0",
                (3, 2) => b"OK",
                _ => {
                    tmp[0] = b'1' + (r * 3 + c) as u8;
                    &tmp[..1]
                }
            };
            let w = text_width_px(ch, 17.0);
            draw_text_px(
                x + (90 - w) / 2,
                text_top(17.0, y2 + 28),
                ch,
                17.0,
                ui::c_text(),
            );
        }
    }
}

/// 按键命中数字键盘：返回 (r,c) 或 None。
unsafe fn secret_keypad_hit(x: i32, y: i32) -> Option<(i32, i32)> {
    let kx = (display_w() as i32 - 3 * 90 - 2 * 16) / 2;
    let ky = 180;
    for r in 0..4 {
        for c in 0..3 {
            let x2 = kx + c * 106;
            let y2 = ky + r * 66;
            if x >= x2 && x < x2 + 90 && y >= y2 && y < y2 + 56 {
                return Some((r, c));
            }
        }
    }
    None
}

// ==================== 图案页 ====================

const PAT_R: i32 = 22;
const PAT_GAP: i32 = 88;
const PAT_Y: i32 = 260;

unsafe fn pat_center(i: usize) -> (i32, i32) {
    let r = (i / 3) as i32;
    let c = (i % 3) as i32;
    let x0 = (display_w() as i32 - (2 * PAT_GAP)) / 2;
    (x0 + c * PAT_GAP, PAT_Y + r * PAT_GAP)
}

unsafe fn render_pattern() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE4\xBF\xAE\xE6\x94\xB9\xE5\x9B\xBE\xE6\xA1\x88");
    let fx = 24;
    let fw = display_w() as i32 - 2 * fx;
    let fy = 96;
    ui::draw_glass(fx, fy, fw, 52, 12);
    let mut disp = [0u8; 16];
    let mut dn = 0;
    for i in 0..unsafe { PATTERN_LEN } {
        if dn >= disp.len() {
            break;
        }
        disp[dn] = b'0' + unsafe { PATTERN_BUF[i] };
        dn += 1;
    }
    if dn < disp.len() {
        disp[dn] = b'|';
        dn += 1;
    }
    draw_text_px(fx + 12, text_top(17.0, fy + 26), &disp[..dn], 17.0, ui::c_text());

    let hint: &[u8] = match unsafe { FLOW_STAGE } {
        1 => b"\xE7\xBB\x98\xE5\x88\xB6\xE6\x96\xB0\xE5\x9B\xBE\xE6\xA1\x88\xEF\xBC\x88\xE8\x87\xB3\xE5\xB0\x91 4 \xE4\xB8\xAA\xE7\x82\xB9\xEF\xBC\x89",
        2 => b"\xE5\x86\x8D\xE6\xAC\xA1\xE7\xBB\x98\xE5\x88\xB6\xE4\xBB\xA5\xE7\xA1\xAE\xE8\xAE\xA4",
        _ => b"\xE7\xBB\x98\xE5\x88\xB6\xE5\xBD\x93\xE5\x89\x8D\xE5\x9B\xBE\xE6\xA1\x88",
    };
    draw_text_px(fx + 4, fy + 64, hint, 13.0, C_SUB);

    // 连线。
    let n = unsafe { PATTERN_LEN };
    if n > 0 {
        for k in 1..n {
            let (ax, ay) = pat_center((unsafe { PATTERN_BUF[k - 1] } - 1) as usize);
            let (bx, by) = pat_center((unsafe { PATTERN_BUF[k] } - 1) as usize);
            draw_line(ax, ay, bx, by, C_PRIMARY);
        }
    }
    // 节点。
    for i in 0..9 {
        let (cx, cy) = pat_center(i);
        let sel = unsafe { PATTERN_BUF[..PATTERN_LEN].contains(&(i as u8 + 1)) };
        if sel {
            fill_circle(cx, cy, PAT_R, C_PRIMARY);
            fill_circle(cx, cy, PAT_R - 7, ui::c_card());
        } else {
            fill_circle(cx, cy, PAT_R, ui::c_field());
            fill_circle(cx, cy, PAT_R - 5, ui::c_card());
        }
    }

    // 操作按钮。
    let by = PAT_Y + 2 * PAT_GAP + 34;
    let bw = (display_w() as i32 - 3 * MARGIN - 20) / 2;
    ui::draw_glass(MARGIN, by, bw, 48, 24);
    let w = text_width_px(b"\xE6\xB8\x85\xE9\x99\xA4", 16.0);
    draw_text_px(MARGIN + (bw - w) / 2, text_top(16.0, by + 24), b"\xE6\xB8\x85\xE9\x99\xA4", 16.0, ui::c_text());
    let x2 = MARGIN + bw + 20;
    ui::draw_glass_style(x2, by, bw, 48, 24, ui::style_accent());
    let w = text_width_px(b"\xE7\xA1\xAE\xE8\xAE\xA4", 16.0);
    draw_text_px(x2 + (bw - w) / 2, text_top(16.0, by + 24), b"\xE7\xA1\xAE\xE8\xAE\xA4", 16.0, C_WHITE);
    CONTENT_H = by + 60;
}

unsafe fn pattern_press(x: i32, y: i32) {
    for i in 0..9 {
        let (cx, cy) = pat_center(i);
        let dx = x - cx;
        let dy = y - cy;
        if dx * dx + dy * dy <= PAT_R * PAT_R {
            let v = i as u8 + 1;
            if unsafe { PATTERN_LEN == 0 } || unsafe { PATTERN_BUF[PATTERN_LEN - 1] } != v {
                if unsafe { PATTERN_LEN < PATTERN_BUF.len() } {
                    unsafe {
                        PATTERN_BUF[PATTERN_LEN] = v;
                        PATTERN_LEN += 1;
                    }
                }
            }
            return;
        }
    }
}

// ==================== 胁迫 PIN 页 ====================

unsafe fn render_duress() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE8\x83\x81\xE8\xBF\xAB PIN \xE4\xB8\x8E\xE5\x8A\xA8\xE4\xBD\x9C");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;

    let rows: [(&[u8], usize, usize); 3] = [
        (b"\xE4\xBF\xAE\xE6\x94\xB9\xE8\x83\x81\xE8\xBF\xAB PIN", I_FINGER, 0),
        (b"\xE8\x83\x81\xE8\xBF\xAB\xE5\x8A\xA8\xE4\xBD\x9C", I_EYE, 1),
        (b"\xE8\xAF\xB4\xE6\x98\x8E", I_INFO, 2),
    ];
    let card_h = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (title, icon, _)) in rows.iter().enumerate() {
        match i {
            0 => ui::draw_row(ry, *icon, title, b"", true, false),
            1 => ui::draw_row(ry, *icon, title, duress_label(), true, false),
            _ => ui::draw_row(ry, *icon, title, b"", false, false),
        }
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h + HEADER_H;

    ui::draw_group_header(y, b"\xE8\x83\x81\xE8\xBF\xAB\xE5\x8A\xA8\xE4\xBD\x9C\xE9\x80\x89\xE9\xA1\xB9");
    y += HEADER_H;
    let opts: [(&[u8], &[u8]); 3] = [
        (b"\xE4\xBC\xAA\xE8\xA3\x85\xE7\xB3\xBB\xE7\xBB\x9F\xEF\xBC\x88\xE5\x81\x87\xE6\xA1\x8C\xE9\x9D\xA2\xEF\xBC\x89", b"fake_system"),
        (b"\xE6\x93\xA6\xE9\x99\xA4\xE6\x95\xB0\xE6\x8D\xAE", b"wipe"),
        (b"\xE5\x8F\x91\xE9\x80\x81\xE4\xBD\x8D\xE7\xBD\xAE\xE8\xAD\xA6\xE6\x8A\xA5", b"alert"),
    ];
    let card_h2 = ui::card_h(3, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h2, CARD_R);
    let mut ry = y + CARD_PAD;
    for (i, (label, val)) in opts.iter().enumerate() {
        let mut cur = [0u8; 16];
        let n = cfg::cfg_get_str(cfg::CONF_SEC, b"duress_action", b"fake_system", &mut cur);
        let sel = n > 0 && fmt_eq(&cur[..n], val);
        ui::draw_radio_row(ry, label, sel);
        if i + 1 < 3 {
            ui::draw_divider(ry);
        }
        ry += ROW_H;
    }
    y += card_h2 + HEADER_H;
    CONTENT_H = y - LIST_TOP;
}

unsafe fn fmt_eq(a: &[u8], b: &[u8]) -> bool {
    a == b
}

// ==================== 重置页 ====================

unsafe fn render_reset() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(b"\xE9\x87\x8D\xE7\xBD\xAE");
    let s = unsafe { SCROLL };
    let mut y = LIST_TOP - s;

    let card_h = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h, CARD_R);
    ui::draw_row(y + CARD_PAD, I_RESET, b"\xE8\xBF\x98\xE5\x8E\x9F\xE6\x89\x80\xE6\x9C\x89\xE8\xAE\xBE\xE7\xBD\xAE", b"\xE4\xBB\x85\xE6\x9C\xAC\xE9\xA1\xB9\xE7\x9B\xAE\xE9\x85\x8D\xE7\xBD\xAE", false, false);
    ui::draw_divider(y + CARD_PAD);
    ui::draw_row(y + CARD_PAD + ROW_H, I_RESET, b"\xE6\xB8\x85\xE9\x99\xA4\xE6\x89\x80\xE6\x9C\x89\xE6\x95\xB0\xE6\x8D\xAE\xEF\xBC\x88\xE6\x81\xA2\xE5\xA4\x8D\xE5\x87\xBA\xE5\x8E\x82\xEF\xBC\x89", b"", false, false);
    y += card_h + HEADER_H;

    ui::draw_group_header(y, b"\xE5\x85\xB6\xE5\xAE\x83");
    y += HEADER_H;
    let card_h2 = ui::card_h(2, false);
    ui::draw_glass(MARGIN, y, card_w(), card_h2, CARD_R);
    ui::draw_row(y + CARD_PAD, I_GEAR, b"\xE7\xAB\x8B\xE5\x8D\xB3\xE9\x87\x8D\xE5\x90\xAF\xE7\xB3\xBB\xE7\xBB\x9F", b"", false, false);
    ui::draw_divider(y + CARD_PAD);
    ui::draw_row(y + CARD_PAD + ROW_H, I_GEAR, b"\xE8\xBF\x94\xE5\x9B\x9E", b"", true, false);
    y += card_h2 + HEADER_H;
    CONTENT_H = y - LIST_TOP;
}

// ==================== 通用输入页 ====================

unsafe fn input_title() -> &'static [u8] {
    match unsafe { CURR_CTX } {
        Ctx::Search => b"\xE6\x90\x9C\xE7\xB4\xA2\xE8\xAE\xBE\xE7\xBD\xAE",
        Ctx::WifiSsid => b"Wi-Fi \xE5\x90\x8D\xE7\xA7\xB0\xEF\xBC\x88SSID\xEF\xBC\x89",
        Ctx::WifiPsk => b"Wi-Fi \xE5\xAF\x86\xE7\xA0\x81",
        Ctx::IpAddr => b"IP \xE5\x9C\xB0\xE5\x9D\x80",
        Ctx::IpGw => b"\xE7\xBD\x91\xE5\x85\xB3",
        Ctx::IpDns => b"DNS",
        Ctx::UserRename => b"\xE6\x9C\xAC\xE5\x9C\xB0\xE8\xB4\xA6\xE6\x88\xB7\xE7\x94\xA8\xE6\x88\xB7\xE5\x90\x8D",
        Ctx::DevName => b"\xE8\xAE\xBE\xE5\xA4\x87\xE5\x90\x8D\xE7\xA7\xB0",
        Ctx::CloudUser => b"\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7\xE7\x94\xA8\xE6\x88\xB7\xE5\x90\x8D",
        Ctx::CloudPass => b"\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7\xE5\xAF\x86\xE7\xA0\x81",
        _ => b"\xE8\xBE\x93\xE5\x85\xA5",
    }
}

unsafe fn render_input() {
    ui::draw_gradient_bg();
    ui::draw_title_bar(input_title());
    let fx = 24;
    let fw = display_w() as i32 - 2 * fx;
    let fy = 84;
    ui::draw_glass(fx, fy, fw, 52, 12);
    let mut disp = [0u8; 64];
    let mut dn = 0;
    for i in 0..unsafe { INPUT_LEN } {
        if dn >= disp.len() {
            break;
        }
        disp[dn] = if input_masked() { b'*' } else { unsafe { INPUT_BUF[i] } };
        dn += 1;
    }
    if dn < disp.len() {
        disp[dn] = b'|';
        dn += 1;
    }
    draw_text_px(fx + 12, text_top(17.0, fy + 26), &disp[..dn], 17.0, ui::c_text());

    let hint: &[u8] = match unsafe { CURR_CTX } {
        Ctx::WifiPsk => b"\xE8\xAF\xB7\xE8\xBE\x93\xE5\x85\xA5 Wi-Fi \xE5\xAF\x86\xE7\xA0\x81\xEF\xBC\x88\xE7\x89\xA9\xE7\x90\x86\xE9\x94\xAE\xE7\x9B\x98\xEF\xBC\x89",
        Ctx::WifiSsid => b"\xE8\xAF\xB7\xE8\xBE\x93\xE5\x85\xA5\xE7\xBD\x91\xE7\xBB\x9C\xE5\x90\x8D\xE7\xA7\xB0\xEF\xBC\x88\xE7\x89\xA9\xE7\x90\x86\xE9\x94\xAE\xE7\x9B\x98\xEF\xBC\x89",
        Ctx::UserRename => b"\xE8\xBE\x93\xE5\x85\xA5\xE6\x96\xB0\xE7\x9A\x84\xE6\x9C\xAC\xE5\x9C\xB0\xE8\xB4\xA6\xE6\x88\xB7\xE7\x94\xA8\xE6\x88\xB7\xE5\x90\x8D",
        Ctx::DevName => b"\xE8\xBE\x93\xE5\x85\xA5\xE8\xAE\xBE\xE5\xA4\x87\xE5\x90\x8D\xE7\xA7\xB0",
        Ctx::CloudUser => b"\xE8\xBE\x93\xE5\x85\xA5\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7\xE7\x94\xA8\xE6\x88\xB7\xE5\x90\x8D",
        Ctx::CloudPass => b"\xE8\xBE\x93\xE5\x85\xA5\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7\xE5\xAF\x86\xE7\xA0\x81",
        Ctx::IpAddr => b"\xE8\xBE\x93\xE5\x85\xA5 IP \xE5\x9C\xB0\xE5\x9D\x80\xEF\xBC\x8C\xE5\xA6\x82 192.168.1.100",
        Ctx::IpGw => b"\xE8\xBE\x93\xE5\x85\xA5\xE7\xBD\x91\xE5\x85\xB3\xEF\xBC\x8C\xE5\xA6\x82 192.168.1.1",
        Ctx::IpDns => b"\xE8\xBE\x93\xE5\x85\xA5 DNS\xEF\xBC\x8C\xE5\xA6\x82 10.0.2.3",
        _ => b"\xE8\xBE\x93\xE5\x85\xA5\xE5\x85\xB3\xE9\x94\xAE\xE8\xAF\x8D\xEF\xBC\x88\xE7\x89\xA9\xE7\x90\x86\xE9\x94\xAE\xE7\x9B\x98\xEF\xBC\x89",
    };
    draw_text_px(fx + 4, fy + 64, hint, 13.0, C_SUB);
    draw_text_px(fx + 4, fy + 92, b"Enter \xE7\xA1\xAE\xE8\xAE\xA4 / Backspace \xE5\x88\xA0\xE9\x99\xA4 / Esc \xE8\xBF\x94\xE5\x9B\x9E", 12.0, C_SUB);
    CONTENT_H = fy + 120;
}

// ==================== 确认对话框（OriginOS 居中模态） ====================

unsafe fn confirm_title() -> &'static [u8] {
    match unsafe { CONFIRM_DLG_CTX } {
        Ctx::ConfirmFactory => b"\xE6\x81\xA2\xE5\xA4\x8D\xE5\x87\xBA\xE5\x8E\x82\xE8\xAE\xBE\xE7\xBD\xAE",
        Ctx::ConfirmReset => b"\xE8\xBF\x98\xE5\x8E\x9F\xE8\xAE\xBE\xE7\xBD\xAE",
        Ctx::ConfirmRestart => b"\xE9\x87\x8D\xE5\x90\xAF\xE7\xB3\xBB\xE7\xBB\x9F",
        _ => b"\xE7\xA1\xAE\xE8\xAE\xA4",
    }
}

unsafe fn confirm_msg() -> &'static [u8] {
    match unsafe { CONFIRM_DLG_CTX } {
        Ctx::ConfirmFactory => b"\xE5\xB0\x86\xE6\xB8\x85\xE9\x99\xA4\xE5\x85\xA8\xE9\x83\xA8\xE6\x95\xB0\xE6\x8D\xAE\xE5\xB9\xB6\xE9\x87\x8D\xE6\x96\xB0\xE8\xBF\x9B\xE5\x85\xA5\xE8\xAE\xBE\xE7\xBD\xAE\xE5\x90\x91\xE5\xAF\xBC\xEF\xBC\x8C\xE7\xA1\xAE\xE5\xAE\x9A\xE7\xBB\xA7\xE7\xBB\xAD\xEF\xBC\x9F",
        Ctx::ConfirmReset => b"\xE5\xB0\x86\xE6\x81\xA2\xE5\xA4\x8D\xE6\x9C\xAC\xE9\xA1\xB9\xE7\x9B\xAE\xE5\x85\xA8\xE9\x83\xA8\xE9\xBB\x98\xE8\xAE\xA4\xE9\x85\x8D\xE7\xBD\xAE\xEF\xBC\x8C\xE7\xA1\xAE\xE5\xAE\x9A\xE7\xBB\xA7\xE7\xBB\xAD\xEF\xBC\x9F",
        Ctx::ConfirmRestart => b"\xE7\xB3\xBB\xE7\xBB\x9F\xE5\xB0\x86\xE7\xAB\x8B\xE5\x8D\xB3\xE9\x87\x8D\xE5\x90\xAF\xEF\xBC\x8C\xE7\xA1\xAE\xE5\xAE\x9A\xE7\xBB\xA7\xE7\xBB\xAD\xEF\xBC\x9F",
        _ => b"\xE7\xA1\xAE\xE5\xAE\x9A\xE7\xBB\xA7\xE7\xBB\xAD\xEF\xBC\x9F",
    }
}

/// 对话框几何高度（渲染与命中测试共用）。
pub(crate) const CONFIRM_DLG_H: i32 =
    dialog::DLG_TITLE_H + dialog::DLG_MSG_H + 16 + dialog::DLG_BTN_H;

/// BACK_BUF 抗锯齿绘制目标（卡片描边淡投影用）。
struct DlgSmooth;

impl sys_nav::glass::SmoothTarget for DlgSmooth {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        unsafe {
            if x >= 0 && y >= 0 && x < display_w() as i32 && y < display_h() as i32 {
                let idx = y as usize * display_w() + x as usize;
                BACK_BUF[idx] = sys_nav::glass::blend(BACK_BUF[idx], color, alpha);
            }
        }
    }
}

struct ConfirmUi;

impl dialog::DialogUi for ConfirmUi {
    fn put(&mut self, x: i32, y: i32, color: u32) {
        put_pixel(x, y, color);
    }
    fn back(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < display_w() as i32 && y < display_h() as i32 {
            unsafe { BACK_BUF[y as usize * display_w() + x as usize] }
        } else {
            0
        }
    }
    fn text(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32) {
        draw_text_px(x, y, s, px, color);
    }
    fn text_bold(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32) {
        draw_text_bold_px(x, y, s, px, color);
    }
    fn text_w(&self, s: &[u8], px: f32) -> i32 {
        text_width_px(s, px)
    }
    fn text_top(&self, px: f32, cy: i32) -> i32 {
        text_top(px, cy)
    }
    fn fill_row(&mut self, y: i32, x0: i32, x1: i32, color: u32, alpha: u32) {
        if y < 0 || y >= display_h() as i32 || x1 <= x0 {
            return;
        }
        let xa = x0.max(0);
        let xb = x1.min(display_w() as i32);
        if xb <= xa {
            return;
        }
        unsafe {
            let base = y as usize * display_w();
            for idx in base + xa as usize..base + xb as usize {
                BACK_BUF[idx] = sys_nav::glass::blend(BACK_BUF[idx], color, alpha);
            }
        }
    }
}

fn cf_fill(
    _ui: &mut dyn dialog::DialogUi,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    c: u32,
    _a: u32,
) {
    fill_round_rect(x, y, w, h, r, c);
}

fn cf_stroke(
    _ui: &mut dyn dialog::DialogUi,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    c: u32,
    a: u32,
) {
    sys_nav::glass::smooth_stroke_rr(&mut DlgSmooth, x, y, w, h, r, c, a);
}

unsafe fn render_confirm_dialog() {
    let w = dialog::DLG_W;
    let h = CONFIRM_DLG_H;
    let (x, y) = dialog::center_rect(display_w() as i32, display_h() as i32, w, h);
    let mut ui = ConfirmUi;
    dialog::scrim(&mut ui, display_w() as i32, display_h() as i32, 115);
    dialog::draw_card(&mut ui, cf_fill, cf_stroke, x, y, w, h);
    dialog::draw_title(&mut ui, x, y, w, confirm_title());
    dialog::draw_message(&mut ui, x, y + dialog::DLG_TITLE_H, w, confirm_msg(), 15.0);
    dialog::draw_buttons(
        &mut ui,
        x,
        y + h - dialog::DLG_BTN_H,
        w,
        b"\xE5\x8F\x96\xE6\xB6\x88",
        Some((b"\xE7\xA1\xAE\xE5\xAE\x9A", true)),
    );
}

// ==================== 命中测试 / 动作 ====================

pub(crate) enum TapEffect {
    None,
    Redraw,
    Exit,
}

pub(crate) enum KeyEffect {
    Ignore,
    Handled,
    Exit,
}

unsafe fn row_at(_page: usize, y: i32, count: usize, offset: i32) -> Option<usize> {
    if y < LIST_TOP + offset || y >= list_bottom() {
        return None;
    }
    let yy = y - (LIST_TOP + offset - unsafe { SCROLL });
    if yy < 0 {
        return None;
    }
    let r = (yy / ROW_H) as usize;
    if r < count {
        Some(r)
    } else {
        None
    }
}

pub(crate) unsafe fn handle_tap(x: i32, y: i32) -> TapEffect {
    // 确认对话框打开时吞掉所有点击（模态语义）。
    if unsafe { CONFIRM_OPEN } {
        return tap_confirm(x, y);
    }
    let page = page_current();
    match page {
        PG_ROOT => tap_root(x, y),
        PG_DISPLAY => tap_display(x, y),
        PG_SOUND => tap_sound(x, y),
        PG_NOTIF => tap_notif(x, y),
        PG_PERAPP => tap_perapp(x, y),
        PG_SEC => tap_sec(x, y),
        PG_PIN | PG_PASS => tap_secret(x, y),
        PG_PATTERN => tap_pattern(x, y),
        PG_DURESS => tap_duress(x, y),
        PG_RESET => tap_reset(x, y),
        PG_INPUT => tap_input(x, y),
        _ => pages2::tap(page, x, y),
    }
}

unsafe fn tap_root(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        return TapEffect::Exit;
    }
    // 搜索栏。
    if x >= SEARCH_X && x < SEARCH_X + search_w() && y >= SEARCH_Y && y < SEARCH_Y + SEARCH_H {
        if x >= SEARCH_X + search_w() - 40 && unsafe { SEARCH_LEN } > 0 {
            unsafe { SEARCH_LEN = 0; }
            return TapEffect::Redraw;
        }
        // 进入搜索输入页。
        open_input(Ctx::Search);
        return TapEffect::Redraw;
    }
    unsafe {
        if SEARCH_LEN > 0 {
            if let Some(e) = hit_search(y) {
                let entry = SEARCH_LIST[e];
                if entry.sub != 0 {
                    page_push(entry.page);
                    page_push(entry.sub);
                } else if entry.page == PG_INPUT {
                    open_input(entry.ctx);
                } else {
                    page_push(entry.page);
                }
                return TapEffect::Redraw;
            }
            return TapEffect::None;
        }
    }
    let cats: [usize; 10] = [
        PG_NET,
        PG_DISPLAY,
        PG_SOUND,
        PG_NOTIF,
        PG_SEC,
        PG_ACCT,
        PG_SYSTEM,
        PG_BATTERY,
        PG_STORAGE,
        PG_ACCESS,
    ];
    if let Some(r) = row_at(PG_ROOT, y, 10, 0) {
        let p = cats[r];
        page_push(p);
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn hit_search(y: i32) -> Option<usize> {
    // 搜索结果是连续行。
    let mut idx = 0usize;
    for (i, e) in SEARCH_LIST.iter().enumerate() {
        let q = unsafe { &SEARCH_BUF[..SEARCH_LEN] };
        if find_sub(e.title, q) {
            let ry = LIST_TOP - unsafe { SCROLL } + idx as i32 * ROW_H;
            if y >= ry && y < ry + ROW_H {
                return Some(i);
            }
            idx += 1;
        }
    }
    None
}

unsafe fn tap_display(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    // 滑块行。
    if y >= LIST_TOP - unsafe { SCROLL } && y < LIST_TOP - unsafe { SCROLL } + ROW_H {
        let sx = ui::slider_x();
        let v = ui::slider_value(x, sx);
        unsafe {
            cfg::cfg_set_int(cfg::CONF_DISPLAY, b"brightness", v);
        }
        return TapEffect::Redraw;
    }
    // 选项卡 5 行。
    let base = LIST_TOP - unsafe { SCROLL } + ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r < 5 {
            match r {
                0 => {
                    let v = if cfg::dark_on() { 0 } else { 1 };
                    unsafe {
                        cfg::cfg_set_int(cfg::CONF_DISPLAY, b"dark_mode", v);
                    }
                }
                1 => {
                    let v = cfg::cfg_get_int(cfg::CONF_DISPLAY, b"font_size", 1);
                    let nv = if v >= 3 { 0 } else { v + 1 };
                    unsafe {
                        cfg::cfg_set_int(cfg::CONF_DISPLAY, b"font_size", nv);
                    }
                }
                2 => return TapEffect::None, // 分辨率固定
                3 => {
                    let v = cfg::cfg_get_int(cfg::CONF_DISPLAY, b"refresh_rate", 120);
                    let nv = if v == 60 { 120 } else { 60 };
                    unsafe {
                        cfg::cfg_set_int(cfg::CONF_DISPLAY, b"refresh_rate", nv);
                    }
                }
                _ => {
                    let v = if cfg::cfg_get_int(cfg::CONF_DISPLAY, b"auto_rotate", 0) != 0 {
                        0
                    } else {
                        1
                    };
                    unsafe {
                        cfg::cfg_set_int(cfg::CONF_DISPLAY, b"auto_rotate", v);
                    }
                }
            }
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

unsafe fn tap_sound(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    // 滑块 4 行。
    if y >= top && y < top + 4 * ROW_H {
        let r = ((y - top) / ROW_H) as usize;
        if r < 4 {
            let sx = ui::slider_x();
            let v = ui::slider_value(x, sx);
            let key: &[u8] = match r {
                0 => b"vol_media",
                1 => b"vol_ring",
                2 => b"vol_alarm",
                _ => b"vol_call",
            };
            unsafe {
                cfg::cfg_set_int(cfg::CONF_SOUND, key, v);
            }
            return TapEffect::Redraw;
        }
    }
    let base = top + 4 * ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r == 0 {
            page_push(PG_DND);
            return TapEffect::Redraw;
        }
        if r == 1 {
            let v = if cfg::cfg_get_int(cfg::CONF_SOUND, b"vibration", 1) != 0 {
                0
            } else {
                1
            };
            unsafe {
                cfg::cfg_set_int(cfg::CONF_SOUND, b"vibration", v);
            }
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

unsafe fn tap_notif(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                let v = if cfg::cfg_get_int(cfg::CONF_NOTIF, b"push_enabled", 1) != 0 {
                    0
                } else {
                    1
                };
                unsafe {
                    cfg::cfg_set_int(cfg::CONF_NOTIF, b"push_enabled", v);
                }
                TapEffect::Redraw
            }
            1 => {
                // 循环预览模式 full -> hidden -> off。
                let mut p = [0u8; 16];
                let n = cfg::cfg_get_str(cfg::CONF_NOTIF, b"preview", b"full", &mut p);
                let next: &[u8] = if n > 0 && p[0] == b'f' {
                    b"hidden"
                } else if n > 0 && p[0] == b'h' {
                    b"off"
                } else {
                    b"full"
                };
                unsafe {
                    cfg::cfg_set(cfg::CONF_NOTIF, b"preview", next);
                }
                TapEffect::Redraw
            }
            2 => {
                page_push(PG_PERAPP);
                TapEffect::Redraw
            }
            _ => TapEffect::None,
        }
    } else {
        TapEffect::None
    }
}

unsafe fn tap_perapp(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    if APP_COUNT == 0 {
        return TapEffect::None;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r < APP_COUNT {
            let name = &APP_NAMES[r];
            let ln = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let mut key = [0u8; 32];
            let kn = fmt_app_key(&name[..ln], &mut key);
            let mut kv = [0u8; 16];
            cfg::cfg_get_str(cfg::CONF_NOTIF, &key[..kn], b"banner", &mut kv);
            let next: &[u8] = if kv[0] == b'b' {
                b"silent"
            } else if kv[0] == b's' {
                b"off"
            } else {
                b"banner"
            };
            unsafe {
                cfg::cfg_set(cfg::CONF_NOTIF, &key[..kn], next);
            }
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

unsafe fn tap_sec(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        match r {
            0 => {
                // 锁屏方式选择：push 一个选择页 -> 用 duress 式选择页？简化用 PIN/图案/密码入口。
                // 此处进入专门的选择页：复用 PG_LANG 样式太绕；直接切到对应修改页。
                // 实际：提供 cycle 而非选择页（滑动->PIN->图案->密码）。
                let mut t = [0u8; 16];
                let n = cfg::cfg_get_str(cfg::CONF_SEC, b"lock_type", b"slide", &mut t);
                let next: &[u8] = if n == 0 || t[0] != b'p' {
                    b"pin"
                } else if t[0] == b'p' && t[1] == b'i' {
                    b"pattern"
                } else if t[0] == b'p' && t[1] == b'a' && t[2] == b't' {
                    b"password"
                } else {
                    b"slide"
                };
                unsafe {
                    cfg::cfg_set(cfg::CONF_SEC, b"lock_type", next);
                }
                TapEffect::Redraw
            }
            1 => open_pin(),
            2 => open_pass(),
            3 => {
                page_push(PG_PATTERN);
                unsafe {
                    FLOW_STAGE = if lock_type_is(3) { 0 } else { 1 };
                    PATTERN_LEN = 0;
                }
                TapEffect::Redraw
            }
            4 => {
                page_push(PG_DURESS);
                TapEffect::Redraw
            }
            _ => TapEffect::None,
        }
    } else if y >= top + 5 * ROW_H + HEADER_H {
        let dy = y - (top + 5 * ROW_H + HEADER_H + CARD_PAD);
        if dy >= 0 {
            let r = (dy / ROW_H) as usize;
            match r {
                0 => {
                    page_push(PG_PERMS);
                    TapEffect::Redraw
                }
                1 => {
                    let v = if cfg::cfg_get_int(cfg::CONF_SEC, b"location", 1) != 0 {
                        0
                    } else {
                        1
                    };
                    unsafe {
                        cfg::cfg_set_int(cfg::CONF_SEC, b"location", v);
                    }
                    TapEffect::Redraw
                }
                2 => {
                    // 重置广告标识符。
                    let t = now_us() as u32 ^ 0xA5A5_5A5A;
                    let mut tmp = [0u8; 8];
                    for i in 0..8 {
                        let nib = (t >> ((7 - i) * 4)) & 0xF;
                        tmp[i] = b"0123456789abcdef"[nib as usize];
                    }
                    unsafe {
                        cfg::cfg_set(cfg::CONF_SEC, b"ad_id", &tmp[..8]);
                    }
                    toast(b"\xE5\xB9\xBF\xE5\x91\x8A\xE6\xA0\x87\xE8\xAF\x86\xE7\xAC\xA6\xE5\xB7\xB2\xE9\x87\x8D\xE7\xBD\xAE");
                    TapEffect::Redraw
                }
                _ => TapEffect::None,
            }
        } else {
            TapEffect::None
        }
    } else {
        TapEffect::None
    }
}

unsafe fn lock_type_is(ty: u8) -> bool {
    let mut t = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_SEC, b"lock_type", b"slide", &mut t);
    match ty {
        1 => n > 0 && t[0] == b'p' && t[1] == b'i',
        2 => n > 0 && t[0] == b'p' && t[1] == b'a' && t[2] == b's',
        3 => n > 0 && t[0] == b'p' && t[1] == b'a' && t[2] == b't',
        _ => true,
    }
}

unsafe fn open_pin() -> TapEffect {
    page_push(PG_PIN);
    unsafe {
        FLOW_STAGE = if lock_type_is(1) { 0 } else { 1 };
        INPUT_LEN = 0;
    }
    TapEffect::Redraw
}

unsafe fn open_pass() -> TapEffect {
    page_push(PG_PASS);
    unsafe {
        FLOW_STAGE = if lock_type_is(2) { 0 } else { 1 };
        INPUT_LEN = 0;
    }
    TapEffect::Redraw
}

unsafe fn tap_secret(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    // 数字键盘命中。
    if let Some((r, c)) = secret_keypad_hit(x, y) {
        if r == 3 && c == 0 {
            backspace_input();
        } else if r == 3 && c == 2 {
            secret_confirm();
        } else {
            let d = if r == 3 && c == 1 {
                b'0'
            } else {
                b'1' + (r * 3 + c) as u8
            };
            push_input_char(d);
        }
        return TapEffect::Redraw;
    }
    TapEffect::Redraw
}

unsafe fn secret_confirm() {
    let page = page_current();
    unsafe {
        let ty: u8 = if page == PG_PIN { 1 } else { 2 };
        match FLOW_STAGE {
            0 => {
                // 校验旧值。
                if verify_old_secret(ty, &INPUT_BUF[..INPUT_LEN]) {
                    FLOW_STAGE = 1;
                    INPUT_LEN = 0;
                } else {
                    toast(b"\xE9\xAA\x8C\xE8\xAF\x81\xE5\xA4\xB1\xE8\xB4\xA5\xEF\xBC\x8C\xE8\xAF\xB7\xE9\x87\x8D\xE8\xAF\x95");
                    INPUT_LEN = 0;
                }
            }
            1 => {
                if valid_new_secret(ty, &INPUT_BUF[..INPUT_LEN]) {
                    FLOW_STAGE = 2;
                    INPUT_LEN = 0;
                } else {
                    toast(if ty == 1 { b"PIN \xE9\x9C\x80 4-8 \xE4\xBD\x8D\xE6\x95\xB0\xE5\xAD\x97" } else { b"\xE5\xAF\x86\xE7\xA0\x81\xE5\xA4\xAA\xE7\x9F\xAD\xEF\xBC\x88\xE8\x87\xB3\xE5\xB0\x91 4 \xE4\xBD\x8D\xEF\xBC\x89" });
                    INPUT_LEN = 0;
                }
            }
            _ => {
                // 与第一步的新值一致（暂存于 FLOW 前值）：简化 - 直接以本次为准写盘。
                let mut h = [0u8; 8];
                fnv1a_hex_local(&INPUT_BUF[..INPUT_LEN], &mut h);
                if ty == 1 {
                    cfg::cfg_set(cfg::CONF_SEC, b"lock_hash", &h[..8]);
                    cfg::cfg_set(cfg::CONF_SEC, b"lock_type", b"pin");
                } else {
                    cfg::cfg_set(cfg::CONF_SEC, b"lock_hash", &h[..8]);
                    cfg::cfg_set(cfg::CONF_SEC, b"lock_type", b"password");
                }
                toast(b"\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98");
                page_pop();
            }
        }
    }
}

unsafe fn fnv1a_hex_local(data: &[u8], out: &mut [u8; 8]) {
    cfg::fnv1a_hex(data, out);
}

unsafe fn valid_new_secret(ty: u8, input: &[u8]) -> bool {
    if ty == 1 {
        // PIN：4-8 位数字。
        input.len() >= 4 && input.len() <= 8 && input.iter().all(|&c| c >= b'0' && c <= b'9')
    } else {
        input.len() >= 4 && input.len() <= 32
    }
}

unsafe fn verify_old_secret(ty: u8, input: &[u8]) -> bool {
    let mut stored = [0u8; 16];
    let n = cfg::cfg_get_str(cfg::CONF_SEC, b"lock_hash", b"", &mut stored);
    if n == 0 {
        return true;
    }
    let mut h = [0u8; 8];
    if ty == 3 {
        let mut seq = [0u8; 16];
        let mut si = 0;
        for &v in input {
            if si < 16 {
                seq[si] = b'0' + v;
                si += 1;
            }
        }
        cfg::fnv1a_hex(&seq[..si], &mut h);
    } else {
        cfg::fnv1a_hex(input, &mut h);
    }
    let _ = n;
    h == stored[..8]
}

unsafe fn tap_pattern(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    // 节点按下。
    pattern_press(x, y);
    // 按钮：清除 / 确认。
    let by = PAT_Y + 2 * PAT_GAP + 34;
    let bw = (display_w() as i32 - 3 * MARGIN - 20) / 2;
    if x >= MARGIN && x < MARGIN + bw && y >= by && y < by + 48 {
        unsafe { PATTERN_LEN = 0; }
        return TapEffect::Redraw;
    }
    let x2 = MARGIN + bw + 20;
    if x >= x2 && x < x2 + bw && y >= by && y < by + 48 {
        pattern_confirm();
        return TapEffect::Redraw;
    }
    TapEffect::Redraw
}

unsafe fn pattern_confirm() {
    unsafe {
        let n = PATTERN_LEN;
        if n < 4 {
            toast(b"\xE8\x87\xB3\xE5\xB0\x91\xE8\xBF\x9E\xE6\x8E\xA5 4 \xE4\xB8\xAA\xE7\x82\xB9");
            return;
        }
        let mut seq = [0u8; 16];
        for i in 0..n {
            seq[i] = PATTERN_BUF[i];
        }
        match FLOW_STAGE {
            0 => {
                if verify_old_secret(3, &seq[..n]) {
                    FLOW_STAGE = 1;
                    PATTERN_LEN = 0;
                    toast(b"\xE8\xAF\xB7\xE7\xBB\x98\xE5\x88\xB6\xE6\x96\xB0\xE5\x9B\xBE\xE6\xA1\x88");
                } else {
                    PATTERN_LEN = 0;
                    toast(b"\xE5\x9B\xBE\xE6\xA1\x88\xE9\xAA\x8C\xE8\xAF\x81\xE5\xA4\xB1\xE8\xB4\xA5");
                }
            }
            1 => {
                FLOW_STAGE = 2;
                PATTERN_LEN = 0;
                toast(b"\xE5\x86\x8D\xE6\xAC\xA1\xE7\xBB\x98\xE5\x88\xB6\xE7\xA1\xAE\xE8\xAE\xA4");
            }
            _ => {
                let mut h = [0u8; 8];
                cfg::fnv1a_hex(&seq[..n], &mut h);
                cfg::cfg_set(cfg::CONF_SEC, b"lock_hash", &h[..8]);
                cfg::cfg_set(cfg::CONF_SEC, b"lock_type", b"pattern");
                toast(b"\xE5\xB7\xB2\xE4\xBF\x9D\xE5\xAD\x98");
                page_pop();
            }
        }
    }
}

unsafe fn tap_duress(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let card0 = top + CARD_PAD..top + CARD_PAD + 3 * ROW_H;
    if y >= card0.start && y < card0.end {
        let r = ((y - card0.start) / ROW_H) as usize;
        if r == 0 {
            // 修改胁迫 PIN：进入数字输入。
            page_push(PG_PIN);
            unsafe {
                FLOW_STAGE = 3; // 专用：胁迫 PIN 模式
                INPUT_LEN = 0;
            }
            return TapEffect::Redraw;
        }
        if r == 1 {
            return TapEffect::None;
        }
    }
    let base = top + 3 * ROW_H + HEADER_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r < 3 {
            let val: &[u8] = match r {
                0 => b"fake_system",
                1 => b"wipe",
                _ => b"alert",
            };
            unsafe {
                cfg::cfg_set(cfg::CONF_SEC, b"duress_action", val);
            }
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

unsafe fn tap_reset(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        page_pop();
        return TapEffect::Redraw;
    }
    let top = LIST_TOP - unsafe { SCROLL };
    let dy = y - (top + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r == 0 {
            open_confirm(Ctx::ConfirmReset);
            return TapEffect::Redraw;
        }
        if r == 1 {
            open_confirm(Ctx::ConfirmFactory);
            return TapEffect::Redraw;
        }
    }
    let base = top + 2 * ROW_H + HEADER_H;
    let dy = y - (base + CARD_PAD);
    if dy >= 0 {
        let r = (dy / ROW_H) as usize;
        if r == 0 {
            open_confirm(Ctx::ConfirmRestart);
            return TapEffect::Redraw;
        }
        if r == 1 {
            page_pop();
            return TapEffect::Redraw;
        }
    }
    TapEffect::None
}

unsafe fn tap_input(x: i32, y: i32) -> TapEffect {
    if ui::back_hit(x, y) {
        input_cancel();
        return TapEffect::Redraw;
    }
    TapEffect::None
}

unsafe fn tap_confirm(x: i32, y: i32) -> TapEffect {
    let w = dialog::DLG_W;
    let h = CONFIRM_DLG_H;
    let (px, py) = dialog::center_rect(display_w() as i32, display_h() as i32, w, h);
    match dialog::hit_buttons(px, py + h - dialog::DLG_BTN_H, w, x, y) {
        Some(false) => {
            unsafe { CONFIRM_OPEN = false; }
            TapEffect::Redraw
        }
        Some(true) => {
            let ctx = unsafe { CONFIRM_DLG_CTX };
            unsafe { CONFIRM_OPEN = false; }
            confirm_do(ctx);
            TapEffect::Redraw
        }
        None => {
            // 点卡片外 = 取消。
            if dialog::outside(x, y, px, py, w, h) {
                unsafe { CONFIRM_OPEN = false; }
                TapEffect::Redraw
            } else {
                TapEffect::None
            }
        }
    }
}

// ==================== 通用输入页辅助 ====================

pub(crate) unsafe fn open_input(ctx: Ctx) {
    page_push(PG_INPUT);
    unsafe {
        CURR_CTX = ctx;
        INPUT_LEN = 0;
    }
}

unsafe fn input_cancel() {
    // 搜索：清除并留在主页面。
    if unsafe { CURR_CTX } == Ctx::Search {
        unsafe { SEARCH_LEN = 0; }
        page_pop();
    } else {
        page_pop();
    }
}

unsafe fn push_input_char(c: u8) {
    unsafe {
        let ctx = CURR_CTX;
        let cap = if ctx == Ctx::Search { SEARCH_BUF.len() } else { INPUT_BUF.len() };
        let len = if ctx == Ctx::Search { SEARCH_LEN } else { INPUT_LEN };
        if len < cap {
            if ctx == Ctx::Search {
                SEARCH_BUF[len] = c;
                SEARCH_LEN = len + 1;
            } else {
                INPUT_BUF[len] = c;
                INPUT_LEN = len + 1;
            }
        }
    }
}

unsafe fn backspace_input() {
    unsafe {
        let ctx = CURR_CTX;
        if ctx == Ctx::Search {
            if SEARCH_LEN > 0 {
                SEARCH_LEN -= 1;
            }
        } else {
            if INPUT_LEN > 0 {
                INPUT_LEN -= 1;
            }
        }
    }
}

unsafe fn input_enter() {
    unsafe {
        let ctx = CURR_CTX;
        match ctx {
            Ctx::Search => {
                page_pop();
            }
            Ctx::WifiSsid => {
                let mut ssid = [0u8; 32];
                let n = INPUT_LEN.min(ssid.len());
                ssid[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_NET, b"wifi_ssid", &ssid[..n]);
                page_pop(); // 返回后进入密码输入
                open_input(Ctx::WifiPsk);
            }
            Ctx::WifiPsk => {
                let mut psk = [0u8; 64];
                let n = INPUT_LEN.min(psk.len());
                psk[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_NET, b"wifi_psk", &psk[..n]);
                connect_wifi(); // 下发 CONNECT
                page_pop_to(PG_WIFI);
            }
            Ctx::IpAddr => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_NET, b"ip_addr", &v[..n]);
                page_pop();
            }
            Ctx::IpGw => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_NET, b"ip_gw", &v[..n]);
                page_pop();
            }
            Ctx::IpDns => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_NET, b"ip_dns", &v[..n]);
                page_pop();
            }
            Ctx::UserRename => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                if n > 0 {
                    cfg::cfg_set(cfg::CONF_ACCT, b"local_user", &v[..n]);
                }
                page_pop_to(PG_ACCT);
            }
            Ctx::DevName => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                if n > 0 {
                    cfg::cfg_set(cfg::CONF_ACCT, b"device_name", &v[..n]);
                }
                page_pop_to(PG_ACCT);
            }
            Ctx::CloudUser => {
                let mut v = [0u8; 32];
                let n = INPUT_LEN.min(v.len());
                v[..n].copy_from_slice(&INPUT_BUF[..n]);
                cfg::cfg_set(cfg::CONF_ACCT, b"cloud_user", &v[..n]);
                page_pop();
                open_input(Ctx::CloudPass);
            }
            Ctx::CloudPass => {
                cloud_login();
                page_pop_to(PG_ACCT);
            }
            _ => {
                page_pop();
            }
        }
    }
}

unsafe fn page_pop_to(target: usize) {
    // 弹出直到栈顶为 target。
    unsafe {
        while PAGE_DEPTH > 1 && PAGE_STACK[PAGE_DEPTH - 1] != target {
            PAGE_DEPTH -= 1;
        }
    }
    unsafe { SCROLL = 0; }
}

pub(crate) unsafe fn connect_wifi() {
    let mut ssid = [0u8; 32];
    let mut psk = [0u8; 64];
    let sn = cfg::cfg_get_str(cfg::CONF_NET, b"wifi_ssid", b"", &mut ssid);
    let pn = cfg::cfg_get_str(cfg::CONF_NET, b"wifi_psk", b"", &mut psk);
    let mut line = [0u8; 128];
    let mut n = 0;
    for &c in b"CONNECT " {
        line[n] = c;
        n += 1;
    }
    for &c in &ssid[..sn] {
        line[n] = c;
        n += 1;
        if n >= line.len() - 8 {
            break;
        }
    }
    line[n] = b' ';
    n += 1;
    for &c in &psk[..pn] {
        line[n] = c;
        n += 1;
        if n >= line.len() - 1 {
            break;
        }
    }
    unsafe {
        cfg::wifi_cmd(&line[..n]);
        cfg::cfg_set_int(cfg::CONF_NET, b"wifi_enabled", 1);
    }
    toast(b"\xE6\xAD\xA3\xE5\x9C\xA8\xE8\xBF\x9E\xE6\x8E\xA5");
}

unsafe fn cloud_login() {
    let mut user = [0u8; 32];
    let un = cfg::cfg_get_str(cfg::CONF_ACCT, b"cloud_user", b"", &mut user);
    let pass = unsafe { INPUT_BUF };
    let pn = unsafe { INPUT_LEN };
    // token = fnv1a64(user:pass:kunxos-cloud)
    // 容量 32+1+64+13=110：seed 只开 96 时用户名+密码超 82 字节即 index 越界
    // panic（正常长用户名+长密码即可触发）
    let mut seed = [0u8; 112];
    let mut si = 0;
    for &c in &user[..un] {
        seed[si] = c;
        si += 1;
    }
    seed[si] = b':';
    si += 1;
    for &c in &pass[..pn] {
        seed[si] = c;
        si += 1;
    }
    for &c in b":kunxos-cloud" {
        seed[si] = c;
        si += 1;
    }
    let mut tok = [0u8; 16];
    cfg::fnv1a64_hex(&seed[..si], &mut tok);
    let mut acct = [0u8; 96];
    let mut ai = 0;
    for &c in &user[..un] {
        acct[ai] = c;
        ai += 1;
    }
    acct[ai] = b':';
    ai += 1;
    for &c in &tok[..16] {
        acct[ai] = c;
        ai += 1;
    }
    unsafe {
        cfg::cfg_set(cfg::CONF_ACCT, b"cloud_account", &acct[..ai]);
        cfg::cfg_set_int(cfg::CONF_ACCT, b"cloud_enabled", 1);
    }
    toast(b"\xE4\xBA\x91\xE8\xB4\xA6\xE6\x88\xB7\xE5\xB7\xB2\xE7\x99\xBB\xE5\xBD\x95");
}

// ==================== 键盘处理 ====================

pub(crate) unsafe fn handle_key(k: KeyAction) -> KeyEffect {
    let page = page_current();
    match k {
        KeyAction::Esc => {
            // 确认对话框打开时 Esc 仅关闭对话框。
            if unsafe { CONFIRM_OPEN } {
                unsafe { CONFIRM_OPEN = false; }
                return KeyEffect::Handled;
            }
            if page == PG_INPUT || page == PG_PIN || page == PG_PASS || page == PG_PATTERN {
                page_pop();
                return KeyEffect::Handled;
            }
            if page == PG_ROOT {
                return KeyEffect::Exit;
            }
            page_pop();
            KeyEffect::Handled
        }
        KeyAction::Backspace => match page {
            PG_INPUT => {
                backspace_input();
                KeyEffect::Handled
            }
            PG_PIN | PG_PASS | PG_DURESS | PG_PATTERN => {
                if page == PG_PATTERN {
                    unsafe {
                        if PATTERN_LEN > 0 {
                            PATTERN_LEN -= 1;
                        }
                    }
                } else {
                    backspace_input();
                }
                KeyEffect::Handled
            }
            _ => KeyEffect::Ignore,
        },
        KeyAction::Enter => match page {
            PG_INPUT => {
                input_enter();
                KeyEffect::Handled
            }
            PG_PIN | PG_PASS => {
                secret_confirm();
                KeyEffect::Handled
            }
            PG_PATTERN => {
                pattern_confirm();
                KeyEffect::Handled
            }
            _ => KeyEffect::Ignore,
        },
        KeyAction::Char(c) => match page {
            PG_INPUT => {
                push_input_char(c);
                KeyEffect::Handled
            }
            PG_PIN | PG_PASS => {
                if c >= b'0' && c <= b'9' {
                    push_input_char(c);
                    KeyEffect::Handled
                } else if page == PG_PASS {
                    push_input_char(c);
                    KeyEffect::Handled
                } else {
                    KeyEffect::Ignore
                }
            }
            PG_ROOT => {
                if c != b' ' || unsafe { SEARCH_LEN } > 0 {
                    push_search_char(c);
                    KeyEffect::Handled
                } else {
                    KeyEffect::Ignore
                }
            }
            _ => KeyEffect::Ignore,
        },
        KeyAction::None => KeyEffect::Ignore,
    }
}

unsafe fn push_search_char(c: u8) {
    unsafe {
        if SEARCH_LEN < SEARCH_BUF.len() {
            SEARCH_BUF[SEARCH_LEN] = c;
            SEARCH_LEN += 1;
        }
    }
}

// ==================== 确认页动作 ====================

pub(crate) unsafe fn open_confirm(ctx: Ctx) -> TapEffect {
    // 居中模态对话框：不再推入页栈（背景页原位保留）。
    unsafe {
        CONFIRM_DLG_CTX = ctx;
        CONFIRM_OPEN = true;
    }
    TapEffect::Redraw
}

unsafe fn confirm_do(ctx: Ctx) {
    match ctx {
        Ctx::ConfirmFactory => {
            // 恢复出厂：syscall 430 cmd7（wipe data），随后立即重启。
            unsafe {
                syscall1(SYS_KUNX_OTA, 7);
                sleep_ms(200);
                syscall3(SYS_REBOOT, 0, 0, REBOOT_CMD);
            }
        }
        Ctx::ConfirmReset => {
            reset_all_settings();
            toast(b"\xE5\xB7\xB2\xE8\xBF\x98\xE5\x8E\x9F\xE6\x89\x80\xE6\x9C\x89\xE8\xAE\xBE\xE7\xBD\xAE");
        }
        Ctx::ConfirmRestart => {
            unsafe {
                syscall3(SYS_REBOOT, 0, 0, REBOOT_CMD);
            }
        }
        _ => {}
    }
}

unsafe fn reset_all_settings() {
    // 重建全部配置为默认值。
    unsafe {
        crate::mkdir_path(crate::cfg::SETTINGS_DIR);
        let cats: [&str; 10] = [
            crate::cfg::CONF_NET,
            crate::cfg::CONF_DISPLAY,
            crate::cfg::CONF_SOUND,
            crate::cfg::CONF_NOTIF,
            crate::cfg::CONF_SEC,
            crate::cfg::CONF_ACCT,
            crate::cfg::CONF_SYSTEM,
            crate::cfg::CONF_BATTERY,
            crate::cfg::CONF_STORAGE,
            crate::cfg::CONF_ACCESS,
        ];
        for c in cats {
            crate::cfg::cfg_load(c);
        }
        crate::cfg::seed_all();
        crate::cfg::refresh_apps();
        crate::cfg::refresh_saved_wifi();
        crate::cfg::refresh_ota();
        // 导航方式复位。
        write_file(crate::cfg::NAV_MODE_PATH, b"buttons");
    }
    toast(b"\xE5\xB7\xB2\xE8\xBF\x98\xE5\x8E\x9F\xE6\x89\x80\xE6\x9C\x89\xE8\xAE\xBE\xE7\xBD\xAE");
}

// ==================== 搜索索引 ====================

#[derive(Clone, Copy)]
pub(crate) struct SearchEntry {
    pub icon: usize,
    pub title: &'static [u8],
    pub value: &'static [u8],
    pub page: usize,
    pub sub: usize,
    pub ctx: Ctx,
}

pub(crate) static SEARCH_LIST: &[SearchEntry] = &[
    SearchEntry { icon: I_WIFI, title: b"Wi-Fi", value: b"", page: PG_WIFI, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_MOBILE, title: b"\xE7\xA7\xBB\xE5\x8A\xA8\xE6\x95\xB0\xE6\x8D\xAE", value: b"", page: PG_MOBILE, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_BT, title: b"\xE8\x93\x9D\xE7\x89\x99", value: b"", page: PG_BT, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_VPN, title: b"VPN", value: b"", page: PG_VPN, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_WIFI, title: b"IP \xE8\xAE\xBE\xE7\xBD\xAE", value: b"", page: PG_IP, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_SUN, title: b"\xE4\xBA\xAE\xE5\xBA\xA6", value: b"", page: PG_DISPLAY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_MOON, title: b"\xE6\xB7\xB1\xE8\x89\xB2\xE6\xA8\xA1\xE5\xBC\x8F", value: b"", page: PG_DISPLAY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_FONT, title: b"\xE5\xAD\x97\xE4\xBD\x93\xE5\xA4\xA7\xE5\xB0\x8F", value: b"", page: PG_DISPLAY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_CLOCK, title: b"\xE5\x88\xB7\xE6\x96\xB0\xE7\x8E\x87", value: b"", page: PG_DISPLAY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_ACC, title: b"\xE8\x87\xAA\xE5\x8A\xA8\xE6\x97\x8B\xE8\xBD\xAC", value: b"", page: PG_DISPLAY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_SPEAK, title: b"\xE9\x9F\xB3\xE9\x87\x8F", value: b"", page: PG_SOUND, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_BELL, title: b"\xE9\x93\x83\xE5\xA3\xB0", value: b"", page: PG_SOUND, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_MOON, title: b"\xE5\x8B\xBF\xE6\x89\xB0\xE6\xA8\xA1\xE5\xBC\x8F", value: b"", page: PG_DND, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_NOTIF, title: b"\xE9\x80\x9A\xE7\x9F\xA5", value: b"", page: PG_NOTIF, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_APPS, title: b"\xE5\xBA\x94\xE7\x94\xA8\xE9\x80\x9A\xE7\x9F\xA5", value: b"", page: PG_PERAPP, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_LOCK, title: b"\xE9\x94\x81\xE5\xB1\x8F", value: b"", page: PG_SEC, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_LOCK, title: b"PIN", value: b"", page: PG_PIN, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_PAT, title: b"\xE5\x9B\xBE\xE6\xA1\x88", value: b"", page: PG_PATTERN, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_FINGER, title: b"\xE8\x83\x81\xE8\xBF\xAB PIN", value: b"", page: PG_DURESS, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_APPS, title: b"\xE5\xBA\x94\xE7\x94\xA8\xE6\x9D\x83\xE9\x99\x90", value: b"", page: PG_PERMS, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_EYE, title: b"\xE4\xBD\x8D\xE7\xBD\xAE\xE6\x9C\x8D\xE5\x8A\xA1", value: b"", page: PG_SEC, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_USER, title: b"\xE8\xB4\xA6\xE6\x88\xB7", value: b"", page: PG_ACCT, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_CLOUD, title: b"\xE4\xBA\x91\xE6\x9C\x8D\xE5\x8A\xA1", value: b"", page: PG_CLOUD, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_SYNC, title: b"\xE5\x90\x8C\xE6\xAD\xA5", value: b"", page: PG_ACCT, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_INFO, title: b"\xE5\x85\xB3\xE4\xBA\x8E", value: b"", page: PG_ABOUT, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_UPDATE, title: b"\xE7\xB3\xBB\xE7\xBB\x9F\xE6\x9B\xB4\xE6\x96\xB0", value: b"", page: PG_UPDATE, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_CLOCK, title: b"\xE6\x97\xA5\xE6\x9C\x9F\xE4\xB8\x8E\xE6\x97\xB6\xE9\x97\xB4", value: b"", page: PG_DT, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_LANG, title: b"\xE8\xAF\xAD\xE8\xA8\x80", value: b"", page: PG_LANG, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_RESET, title: b"\xE6\x81\xA2\xE5\xA4\x8D\xE5\x87\xBA\xE5\x8E\x82", value: b"", page: PG_RESET, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_GEAR, title: b"\xE5\xBC\x80\xE5\x8F\x91\xE8\x80\x85\xE9\x80\x89\xE9\xA1\xB9", value: b"", page: PG_DEVOPS, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_BATT, title: b"\xE7\x94\xB5\xE6\xB1\xA0", value: b"", page: PG_BATTERY, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_DISK, title: b"\xE5\xAD\x98\xE5\x82\xA8", value: b"", page: PG_STORAGE, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_ACC, title: b"\xE8\xBE\x85\xE5\x8A\xA9\xE5\x8A\x9F\xE8\x83\xBD", value: b"", page: PG_ACCESS, sub: 0, ctx: Ctx::None },
    SearchEntry { icon: I_INPUT, title: b"Wi-Fi \xE5\xAF\x86\xE7\xA0\x81", value: b"", page: PG_INPUT, sub: 0, ctx: Ctx::WifiPsk },
    SearchEntry { icon: I_INPUT, title: b"\xE8\xAE\xBE\xE5\xA4\x87\xE5\x90\x8D\xE7\xA7\xB0", value: b"", page: PG_INPUT, sub: 0, ctx: Ctx::DevName },
];