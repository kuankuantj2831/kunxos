// KunxOS 浏览器应用（EL0 用户态，裸 syscall、no_std）。
//
// 由 launcher 经 fork + execve 拉起（/bin/browser）。职责：
//   1. 简化版浏览器引擎：HTML 解析、CSS 样式计算、流式布局、CPU 光栅化
//   2. 网络：HTTP/1.1 + TLS 1.2（ECDHE_RSA_AES128_GCM_SHA256，证书链验证）；file:// 与 kunxos://
//   3. 导航：地址栏、前进/后退、主页、书签；单标签（多标签预留）
//   4. 滚动：内容区拖拽滚动 + 右侧滚动条
//   5. 表单：文本输入聚焦编辑、复选框/单选钮切换、GET 表单提交
//   6. 安全基线：无 Cookie、无插件、无 JS；图片仅同源/内置加载
//
// 模块：url / tls / html / css / style / layout / paint / net / image / pages。

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

#[macro_use]
extern crate alloc;
extern crate kunxsec as kxsec;

use core::arch::global_asm;
use core::panic::PanicInfo;

#[path = "../../../shared/app_kit.rs"]
mod kit;

mod url;
#[path = "tls/mod.rs"]
mod tls;
mod html;
mod css;
mod style;
mod layout;
mod paint;
mod net;
mod image;
mod pages;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use kit::*;

use crate::url::Url;

// ==================== 配色（浅色玻璃风格） ====================

const C_BG: u32 = 0x00_F7_F8_FA;
const C_BAR: u32 = 0x00_FF_FF_FF;
const C_URL_BG: u32 = 0x00_FF_FF_FF;
const C_ACCENT: u32 = 0x00_00_B9_6B;
const C_LINK: u32 = 0x00_1A_73_E8;
const C_TITLE: u32 = 0x00_1A_1C_22;
const C_DIM: u32 = 0x00_6B_72_80;
const C_PAGE_BG: u32 = 0x00_FF_FF_FF;
/// 彩色表面（强调绿玻璃按钮）上的文字保持白色。
const C_ON_ACCENT: u32 = 0x00_FF_FF_FF;

// ==================== 布局常量 ====================

const ROW1_Y: i32 = 0;      // 导航行（后退/前进/主页 + 标题）
const ROW1_H: i32 = 44;
const ROW2_Y: i32 = 48;     // 地址栏
const ROW2_H: i32 = 42;
const ROW3_Y: i32 = 94;     // 书签行
const ROW3_H: i32 = 36;
const CONTENT_Y: i32 = 136; // 内容区
const CONTENT_BOTTOM: i32 = NAV_Y - 8; // = 896
const VIEWPORT_H: i32 = CONTENT_BOTTOM - CONTENT_Y; // 760

const BTN_SZ: i32 = 34;
const BTN_Y: i32 = 5;
const URL_X: i32 = 16;
const URL_W: i32 = DISPLAY_W as i32 - 104;
const GO_X: i32 = DISPLAY_W as i32 - 80;
const GO_W: i32 = 64;
const SB_W: i32 = 4; // 滚动条宽
const SB_X: i32 = DISPLAY_W as i32 - SB_W - 2;

// ==================== 书签 ====================

struct Bookmark {
    label: &'static str,
    url: &'static str,
}

static BOOKMARKS: [Bookmark; 7] = [
    Bookmark { label: "主页", url: "kunxos://home" },
    Bookmark { label: "新闻", url: "kunxos://news" },
    Bookmark { label: "应用", url: "kunxos://apps" },
    Bookmark { label: "表单", url: "kunxos://demo/form" },
    Bookmark { label: "关于", url: "kunxos://about" },
    // 仍支持纯 HTTP 的真实站点：主流网站明文端口普遍 302 强跳 HTTPS
    // （TLS 未启用打不开），这两个站点可在 HTTP 下直接浏览。
    Bookmark { label: "Example", url: "http://example.com/" },
    Bookmark { label: "CERN", url: "http://info.cern.ch/" },
];

// ==================== 全局状态 ====================

static mut URL_BUF: [u8; 64] = [0; 64];
static mut URL_LEN: usize = 0;
static mut URL_FOCUS: bool = false;
static mut CURSOR_BLINK: bool = false;
static mut BLINK_TICK: u32 = 0;

static mut HISTORY: Vec<String> = Vec::new();
static mut HIST_IDX: usize = 0;

/// 当前页面。
static mut PAGE: Option<PageData> = None;

/// 错误提示（PAGE 为 None 时显示）。
static mut ERROR_MSG: Option<String> = None;

/// 页面运行时数据（堆分配）。
struct PageData {
    dom: html::Dom,
    styles: style::StyleTree,
    lt: layout::Layout,
    cmds: Vec<paint::Cmd>,
    /// lt.images 对齐的图片数据（存储格式：8 字节头 + RGB）。
    images: Vec<Option<Vec<u8>>>,
    /// 控件文本值（下标 = 控件下标）。
    values: Vec<Vec<u8>>,
    /// 复选框/单选钮状态（下标 = 控件下标）。
    checked: Vec<bool>,
    /// 当前聚焦的文本控件。
    focus: Option<usize>,
    scroll: i32,
    max_scroll: i32,
    title: String,
    url_display: String,
    base: Url,
}

// 拖拽滚动状态
static mut DRAG_LAST_Y: i32 = 0;
static mut DRAG_ACTIVE: bool = false;
static mut DRAG_MOVED: bool = false;

// ==================== 工具 ====================

fn set_url(u: &str) {
    unsafe {
        let n = u.len().min(URL_BUF.len());
        URL_BUF[..n].copy_from_slice(&u.as_bytes()[..n]);
        URL_LEN = n;
    }
}

fn url_str() -> &'static [u8] {
    unsafe { &URL_BUF[..URL_LEN] }
}

/// 清空页面（错误页/无页）。
fn clear_page() {
    unsafe { PAGE = None; }
}

/// 记录错误提示。
fn set_error(msg: &str) {
    unsafe { ERROR_MSG = Some(msg.to_string()); }
}

// ==================== 页面构建 ====================

/// 图片尺寸回调（布局用）。
fn img_size_of(map: &[(String, Option<Vec<u8>>)]) -> impl Fn(&str) -> Option<(f32, f32)> + '_ {
    move |src: &str| {
        for (u, d) in map {
            if u == src {
                if let Some(data) = d {
                    if data.len() >= 8 {
                        let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as f32;
                        let h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as f32;
                        return Some((w, h));
                    }
                }
                return None;
            }
        }
        None
    }
}

/// 预抓取页面图片（同源或内置；跨源图片以占位框渲染，不发请求）。
fn prefetch_images(dom: &html::Dom, base: &Url) -> Vec<(String, Option<Vec<u8>>)> {
    let mut map: Vec<(String, Option<Vec<u8>>)> = Vec::new();
    let imgs = dom.elements_by_tag("img");
    for &n in &imgs {
        let Some(src) = dom.nodes[n].attr("src") else { continue };
        if src.is_empty() {
            continue;
        }
        let abs = match url::resolve(base, src) {
            Some(u) => u,
            None => continue,
        };
        let key = abs.display();
        if map.iter().any(|(k, _)| k == &key) {
            continue;
        }
        // 同源基线落实（net.rs/module 注释声明「仅同源/内置加载」但此前
        // 未接线）：远程页面可借 <img> 对任意内网/外网地址发 GET（追踪/
        // 探测），file:// 页可读任意本地 PPM。仅放行同源与 kunxos:// 内置。
        let data = if abs.same_origin(base) || abs.scheme == url::Scheme::Kunxos {
            net::fetch_image_bytes(&abs)
        } else {
            None
        };
        map.push((key, data));
    }
    map
}

/// 导航并构建页面。`push`：是否压入历史栈。
fn navigate(input: &str, push: bool) {
    print("[browser] 导航: ");
    print(input);
    print("\n");
    let parsed = match url::parse(input) {
        Some(u) => u,
        None => {
            clear_page();
            set_url(input);
            return;
        }
    };
    if push {
        unsafe {
            HISTORY.truncate(HIST_IDX + 1);
            HISTORY.push(parsed.display());
            HIST_IDX = HISTORY.len() - 1;
        }
    }
    set_url(&parsed.display());
    let result = net::fetch(&parsed);
    match result {
        net::FetchResult::Ok { bytes, final_url } => {
            unsafe { ERROR_MSG = None; }
            build_page(parsed, &bytes, &final_url);
        }
        net::FetchResult::Err { msg } => {
            clear_page();
            set_error(&msg);
        }
    }
}

fn build_page(base: Url, bytes: &[u8], final_url: &str) {
    let dom = html::parse(bytes);
    let author = style::collect_stylesheet(&dom);
    let styles = style::compute(&dom, &author);
    let title = dom.title();
    // 图片预抓取
    let img_map = prefetch_images(&dom, &base);
    let img_size = img_size_of(&img_map);
    let lt = layout::layout(&dom, &styles, layout::VIEWPORT_W, &base, &img_size);
    // 图片数据对齐 lt.images
    let images: Vec<Option<Vec<u8>>> = lt
        .images
        .iter()
        .map(|im| {
            for (k, d) in &img_map {
                if k == &im.src {
                    return d.clone();
                }
            }
            None
        })
        .collect();
    // 表单初始状态
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut checked: Vec<bool> = Vec::new();
    for (i, c) in lt.controls.iter().enumerate() {
        match &c.kind {
            layout::ControlKind::TextInput { value_idx, .. } => {
                let mut v: Vec<u8> = Vec::new();
                // 初始值：value 属性
                if let Some(attr) = dom.nodes[c.node].attr("value") {
                    v.extend_from_slice(attr.as_bytes());
                }
                while values.len() <= *value_idx {
                    values.push(Vec::new());
                }
                values[*value_idx] = v;
                checked.push(false);
            }
            layout::ControlKind::Checkbox { checked: chk, .. } => {
                checked.push(*chk);
            }
            layout::ControlKind::Radio { checked: chk, .. } => {
                checked.push(*chk);
            }
            layout::ControlKind::Button { .. } => {
                checked.push(false);
            }
        }
        let _ = i;
    }
    // 钳制文档高度换算与滚动量：CSS 已限 8192px，但元素数量无上限，
    // 不设顶会回到 i32 加/乘回绕簇（滚动条 thumb 位置错乱）。
    let max_scroll = (((lt.height as i64) - VIEWPORT_H as i64).max(0) as i32).min(1_000_000);
    let cmds = paint::build_paint_list(&dom, &lt);
    let page = PageData {
        dom,
        styles,
        lt,
        cmds,
        images,
        values,
        checked,
        focus: None,
        scroll: 0,
        max_scroll,
        title,
        url_display: final_url.to_string(),
        base,
    };
    unsafe { PAGE = Some(page); }
    print("[browser] 页面就绪\n");
}

/// 简单错误页（渲染时显示）。
fn error_page(msg: &str) {
    draw_gradient_bg(C_BG, glass_dark());
    draw_text(24, CONTENT_Y + 30, "无法打开页面".as_bytes(), C_TITLE);
    draw_text(24, CONTENT_Y + 70, msg.as_bytes(), C_LINK);
    draw_text(24, CONTENT_Y + 100, "检查地址后重试，或点击书签浏览内置页面".as_bytes(), C_DIM);
}

// ==================== 渲染 ====================

fn render() {
    // 背景
    draw_gradient_bg(C_BG, glass_dark());

    // ---- 导航行 ----
    draw_glass(0, ROW1_Y, DISPLAY_W as i32, ROW1_H, 0);
    // 后退 / 前进 / 主页
    draw_glass(6, BTN_Y, BTN_SZ, BTN_SZ, 10);
    draw_text(6 + 11, BTN_Y + 8, "<".as_bytes(), C_TEXT);
    draw_glass(6 + BTN_SZ + 4, BTN_Y, BTN_SZ, BTN_SZ, 10);
    draw_text(6 + BTN_SZ + 4 + 11, BTN_Y + 8, ">".as_bytes(), C_TEXT);
    draw_glass(6 + (BTN_SZ + 4) * 2, BTN_Y, BTN_SZ, BTN_SZ, 10);
    draw_home_icon(6 + (BTN_SZ + 4) * 2 + 8, BTN_Y + 7);
    // 标题
    let title = unsafe { PAGE.as_ref().map(|p| p.title.as_bytes()).unwrap_or(b"KunxOS Browser") };
    draw_text(6 + (BTN_SZ + 4) * 3 + 12, BTN_Y + 9, title, C_TEXT);

    // ---- 地址栏 ----
    draw_glass(URL_X, ROW2_Y, URL_W, ROW2_H, 12);
    let focused = unsafe { URL_FOCUS };
    if focused {
        draw_rect(URL_X, ROW2_Y, URL_W, ROW2_H, C_ACCENT);
    }
    let url = url_str();
    if url.is_empty() {
        draw_text(URL_X + 12, ROW2_Y + 13, "输入网址（http:// / file:// / kunxos://）".as_bytes(), C_DIM);
    } else {
        draw_text(URL_X + 12, ROW2_Y + 13, url, C_TEXT);
    }
    if focused && unsafe { CURSOR_BLINK } {
        let cx = URL_X + 12 + text_width(url);
        fill_rect(cx, ROW2_Y + 10, 2, 22, C_TEXT);
    }
    draw_glass_style(GO_X, ROW2_Y + 3, GO_W, ROW2_H - 6, 10, &STYLE_ACCENT);
    draw_text(GO_X + (GO_W - text_width("前往".as_bytes())) / 2, ROW2_Y + 14, "前往".as_bytes(), C_ON_ACCENT);

    // ---- 书签行 ----
    let mut bx = 16;
    for b in BOOKMARKS.iter() {
        let w = text_width(b.label.as_bytes()) + 22;
        draw_glass(bx, ROW3_Y, w, ROW3_H - 4, 16);
        draw_text(bx + 11, ROW3_Y + 11, b.label.as_bytes(), C_LINK);
        bx += w + 8;
    }

    // ---- 内容区 ----
    fill_rect(0, CONTENT_Y, DISPLAY_W as i32, VIEWPORT_H, C_PAGE_BG);
    if let Some(page) = unsafe { PAGE.as_ref() } {
        let scroll = page.scroll;
        let ctx = paint::PaintCtx {
            dom: &page.dom,
            images: &paint::ImageStore { data: &page.images },
            form: &paint::FormView {
                values: &page.values,
                checked: &page.checked,
                focus: page.focus,
                cursor_on: unsafe { CURSOR_BLINK },
            },
            scroll_y: scroll,
            clip_x: 0,
            clip_y: CONTENT_Y,
            clip_w: DISPLAY_W as i32 - SB_W - 2,
            clip_h: VIEWPORT_H,
        };
        paint::execute(&page.cmds, &ctx);
        // 滚动条
        if page.max_scroll > 0 {
            let thumb_h = (VIEWPORT_H * VIEWPORT_H / (page.max_scroll + VIEWPORT_H)).max(24);
            let thumb_y = CONTENT_Y
                + (((VIEWPORT_H - thumb_h) as i64 * scroll as i64 / page.max_scroll as i64) as i32);
            fill_rect(SB_X, CONTENT_Y, SB_W, VIEWPORT_H, 0x00_E5_E9_F0);
            fill_rect(SB_X, thumb_y, SB_W, thumb_h, C_ACCENT);
        }
    } else if let Some(msg) = unsafe { ERROR_MSG.as_ref() } {
        draw_text(24, CONTENT_Y + 30, "无法打开页面".as_bytes(), 0x00_1A_1C_22);
        draw_text(24, CONTENT_Y + 70, msg.as_bytes(), C_LINK);
        draw_text(24, CONTENT_Y + 100, "检查地址后重试，或点击书签浏览内置页面".as_bytes(), 0x00_6B_72_80);
    }

    draw_sys_nav();
}

/// 绘制小房子图标（几何绘制，不依赖字体字形）。
fn draw_home_icon(x: i32, y: i32) {
    let color = C_TEXT;
    // 屋顶
    draw_line(x + 2, y + 7, x + 9, y, color);
    draw_line(x + 9, y, x + 16, y + 7, color);
    draw_line(x + 2, y + 7, x + 16, y + 7, color);
    // 墙体
    fill_rect(x + 4, y + 7, 10, 7, color);
    // 门
    fill_rect(x + 8, y + 10, 3, 4, C_BAR);
}

// ==================== 命中测试 ====================

fn btn_hit(x: i32, y: i32, x0: i32) -> bool {
    x >= x0 && x < x0 + BTN_SZ && y >= BTN_Y && y < BTN_Y + BTN_SZ
}

fn in_content(x: i32, y: i32) -> bool {
    x >= 0 && x < DISPLAY_W as i32 && y >= CONTENT_Y && y < CONTENT_BOTTOM
}

// ==================== 表单交互 ====================

fn toggle_checkbox(page: &mut PageData, ctrl: usize) {
    let is_radio = matches!(page.lt.controls[ctrl].kind, layout::ControlKind::Radio { .. });
    let name = match &page.lt.controls[ctrl].kind {
        layout::ControlKind::Checkbox { name, .. } => name.clone(),
        layout::ControlKind::Radio { name, .. } => name.clone(),
        _ => return,
    };
    if is_radio {
        // 同组互斥
        for (i, c) in page.lt.controls.iter().enumerate() {
            if let layout::ControlKind::Radio { name: n, .. } = &c.kind {
                if n == &name {
                    page.checked[i] = i == ctrl;
                }
            }
        }
    } else {
        page.checked[ctrl] = !page.checked[ctrl];
    }
}

/// GET 表单提交：收集表单内控件拼查询串并导航（用全局 PAGE，避开借用冲突）。
fn submit_form_at(form: Option<usize>, submit_ctrl: usize) {
    let Some(page) = (unsafe { PAGE.as_ref() }) else {
        return; // 尚无页面（PAGE 未初始化）时安全返回，避免 unwrap panic
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (i, c) in page.lt.controls.iter().enumerate() {
        if c.form != form {
            continue;
        }
        match &c.kind {
            layout::ControlKind::TextInput { name, .. } if !name.is_empty() => {
                let v = page.values.get(i).cloned().unwrap_or_default();
                pairs.push((name.clone(), String::from_utf8_lossy(&v).into_owned()));
            }
            layout::ControlKind::Checkbox { name, .. } if !name.is_empty() && page.checked.get(i).copied().unwrap_or(false) => {
                let v = page.dom.nodes[c.node].attr("value").unwrap_or("on");
                pairs.push((name.clone(), v.to_string()));
            }
            layout::ControlKind::Radio { name, .. } if !name.is_empty() && page.checked.get(i).copied().unwrap_or(false) => {
                let v = page.dom.nodes[c.node].attr("value").unwrap_or("on");
                pairs.push((name.clone(), v.to_string()));
            }
            layout::ControlKind::Button { name, submit, .. } if i == submit_ctrl && *submit && !name.is_empty() => {
                let v = page.dom.nodes[c.node].attr("value").unwrap_or("提交");
                pairs.push((name.clone(), v.to_string()));
            }
            _ => {}
        }
    }
    let action = form.and_then(|f| page.dom.nodes[f].attr("action").map(|s| s.to_string()));
    let target = match action {
        Some(a) => url::resolve(&page.base, &a).unwrap_or_else(|| page.base.clone()),
        None => {
            let mut u = page.base.clone();
            u.query.clear();
            u
        }
    };
    let mut full = target.display();
    let mut q = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            q.push('&');
        }
        q.push_str(&url::form_encode(k));
        q.push('=');
        q.push_str(&url::form_encode(v));
    }
    if !q.is_empty() {
        full.push('?');
        full.push_str(&q);
    }
    print("[browser] 提交: ");
    print(&full);
    print("\n");
    navigate(&full, true);
}

// ==================== 输入处理 ====================

fn handle_tap(x: i32, y: i32) -> bool {
    // 导航行按钮
    if btn_hit(x, y, 6) {
        go_back();
        return true;
    }
    if btn_hit(x, y, 6 + BTN_SZ + 4) {
        go_forward();
        return true;
    }
    if btn_hit(x, y, 6 + (BTN_SZ + 4) * 2) {
        unsafe { URL_FOCUS = false; }
        navigate("kunxos://home", true);
        return true;
    }
    // 地址栏聚焦
    if x >= URL_X && x < URL_X + URL_W && y >= ROW2_Y && y < ROW2_Y + ROW2_H {
        unsafe { URL_FOCUS = true; }
        if let Some(page) = unsafe { PAGE.as_mut() } {
            page.focus = None;
        }
        return true;
    }
    // 前往
    if x >= GO_X && x < GO_X + GO_W && y >= ROW2_Y + 3 && y < ROW2_Y + ROW2_H - 3 {
        unsafe { URL_FOCUS = false; }
        let u = core::str::from_utf8(url_str()).unwrap_or("").to_string();
        navigate(&u, true);
        return true;
    }
    // 书签
    if y >= ROW3_Y && y < ROW3_Y + ROW3_H {
        let mut bx = 16;
        for b in BOOKMARKS.iter() {
            let w = text_width(b.label.as_bytes()) + 22;
            if x >= bx && x < bx + w {
                unsafe { URL_FOCUS = false; }
                navigate(b.url, true);
                return true;
            }
            bx += w + 8;
        }
    }
    // 滚动条
    if x >= SB_X && x < SB_X + SB_W && in_content(x, y) {
        if let Some(page) = unsafe { PAGE.as_mut() } {
            if page.max_scroll > 0 {
                let t = (y - CONTENT_Y).clamp(0, VIEWPORT_H) as i64;
                let max = page.max_scroll as i64;
                page.scroll = (t * max / VIEWPORT_H as i64) as i32;
            }
        }
        return true;
    }
    // 页面内容
    if in_content(x, y) {
        let hit = {
            let Some(page) = (unsafe { PAGE.as_ref() }) else {
                unsafe { URL_FOCUS = false; }
                return true;
            };
            let (dx, dy) = (x as f32, (y + page.scroll - CONTENT_Y) as f32);
            layout::hit_test(&page.lt, dx, dy)
        };
        match hit {
            layout::HitResult::Link(u) => {
                unsafe { URL_FOCUS = false; }
                let url = u;
                navigate(&url, true);
            }
            layout::HitResult::Control(ci) => {
                if let Some(page) = unsafe { PAGE.as_mut() } {
                    handle_control_tap(page, ci);
                }
            }
            layout::HitResult::None => {
                if let Some(page) = unsafe { PAGE.as_mut() } {
                    page.focus = None;
                }
            }
        }
        return true;
    }
    unsafe { URL_FOCUS = false; }
    if let Some(page) = unsafe { PAGE.as_mut() } {
        page.focus = None;
    }
    false
}

fn handle_control_tap(page: &mut PageData, ctrl: usize) {
    match &page.lt.controls[ctrl].kind {
        layout::ControlKind::TextInput { .. } => {
            page.focus = Some(ctrl);
            unsafe { URL_FOCUS = false; }
        }
        layout::ControlKind::Checkbox { .. } | layout::ControlKind::Radio { .. } => {
            toggle_checkbox(page, ctrl);
        }
        layout::ControlKind::Button { submit, .. } => {
            if *submit {
                let form = page.lt.controls[ctrl].form;
                submit_form_at(form, ctrl);
            } else {
                page.focus = None;
            }
        }
    }
}

fn go_back() {
    unsafe {
        if HIST_IDX > 0 {
            HIST_IDX -= 1;
            let u = HISTORY[HIST_IDX].clone();
            navigate(&u, false);
        }
    }
}

fn go_forward() {
    unsafe {
        if HIST_IDX + 1 < HISTORY.len() {
            HIST_IDX += 1;
            let u = HISTORY[HIST_IDX].clone();
            navigate(&u, false);
        }
    }
}

// ==================== 主循环 ====================

#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    // 无头自测：`run /bin/browser --selftest <url>`。跳过 GUI（不需要
    // /dev/fb0），直接走 net::fetch 全链路（http/https/file/kunxos），
    // 结果打到串口，供无头 QEMU 断言。退出码：0 成功 / 1 失败。
    let arg1 = unsafe {
        if argc > 1 && *argv.add(1) != 0 {
            let mut p = *argv.add(1) as *const u8;
            let mut s = String::new();
            while *p != 0 {
                s.push(*p as char);
                p = p.add(1);
            }
            s
        } else {
            String::new()
        }
    };
    if arg1 == "--selftest" {
        let url = unsafe {
            if argc > 2 && *argv.add(2) != 0 {
                let mut p = *argv.add(2) as *const u8;
                let mut s = String::new();
                while *p != 0 {
                    s.push(*p as char);
                    p = p.add(1);
                }
                s
            } else {
                String::from("https://www.baidu.com")
            }
        };
        print("[browser-selftest] URL: ");
        print(&url);
        print("\n");
        let parsed = match url::parse(&url) {
            Some(u) => u,
            None => {
                print("[browser-selftest] FAIL: URL 解析失败\n");
                unsafe { exit(1); }
                unreachable!();
            }
        };
        let t0 = kit::now_us();
        match net::fetch(&parsed) {
            net::FetchResult::Ok { bytes, final_url } => {
                let dt = (kit::now_us() - t0) / 1000;
                let dom = html::parse(&bytes);
                let title = dom.title();
                print("[browser-selftest] OK ");
                print_dec(bytes.len() as u64);
                print(" 字节 ");
                print_dec(dt);
                print("ms 标题: ");
                print(&title);
                print(" 最终URL: ");
                print(&final_url);
                print("\n[browser-selftest] PASS\n");
                unsafe { exit(0); }
                unreachable!();
            }
            net::FetchResult::Err { msg } => {
                print("[browser-selftest] FAIL: ");
                print(&msg);
                print("\n");
                unsafe { exit(1); }
                unreachable!();
            }
        }
    }
    print("[browser] 启动中\n");
    if !fb_init() {
        print("[browser] /dev/fb0 mmap 失败\n");
        unsafe { exit(1); }
    }
    print("[browser] 就绪\n");

    // 起始页
    navigate("kunxos://home", true);

    let mut dirty = true;
    loop {
        let ev = poll_input();
        if ev.moved {
            // 拖拽滚动
            let dragging = unsafe { DRAG_ACTIVE };
            let down = unsafe { MOUSE_DOWN || TOUCH_DOWN };
            if dragging && down {
                let cy = unsafe { CUR_Y };
                let last = unsafe { DRAG_LAST_Y };
                let delta = last - cy;
                if delta != 0 {
                    unsafe { DRAG_LAST_Y = cy; }
                    if delta.abs() > 2 {
                        unsafe { DRAG_MOVED = true; }
                    }
                    if let Some(page) = unsafe { PAGE.as_mut() } {
                        let ns = (page.scroll + delta).clamp(0, page.max_scroll);
                        if ns != page.scroll {
                            page.scroll = ns;
                            dirty = true;
                        }
                    }
                }
            }
            cursor_overlay_move();
        }
        if ev.nav != NavAction::None {
            print("[nav] 系统导航 -> 退出\n");
            unsafe { exit(nav_exit_code(ev.nav)); }
        }

        if let Some((x, y)) = ev.tap {
            let was_drag = unsafe { DRAG_MOVED };
            unsafe {
                DRAG_ACTIVE = false;
                DRAG_MOVED = false;
            }
            if !was_drag {
                handle_tap(x, y);
                dirty = true;
            }
        }

        // 指针按下：记录拖拽起点
        if !unsafe { DRAG_ACTIVE } && unsafe { MOUSE_DOWN || TOUCH_DOWN } {
            let (x, y) = unsafe { (CUR_X, CUR_Y) };
            if in_content(x, y) && !(x >= SB_X && x < SB_X + SB_W) {
                unsafe {
                    DRAG_ACTIVE = true;
                    DRAG_LAST_Y = y;
                    DRAG_MOVED = false;
                }
            }
        }

        // 键盘输入
        match ev.key {
            KeyAction::Char(c) => {
                if unsafe { URL_FOCUS } {
                    if unsafe { URL_LEN < URL_BUF.len() } {
                        unsafe {
                            URL_BUF[URL_LEN] = c;
                            URL_LEN += 1;
                        }
                        dirty = true;
                    }
                } else if let Some(page) = unsafe { PAGE.as_mut() } {
                    if let Some(f) = page.focus {
                        let v = &mut page.values[f];
                        if v.len() < 64 {
                            v.push(c);
                        }
                        dirty = true;
                    }
                }
            }
            KeyAction::Backspace => {
                if unsafe { URL_FOCUS } && unsafe { URL_LEN > 0 } {
                    unsafe { URL_LEN -= 1; }
                    dirty = true;
                } else if let Some(page) = unsafe { PAGE.as_mut() } {
                    if let Some(f) = page.focus {
                        let v = &mut page.values[f];
                        if !v.is_empty() {
                            v.pop();
                        }
                        dirty = true;
                    }
                }
            }
            KeyAction::Enter => {
                if unsafe { URL_FOCUS } {
                    unsafe { URL_FOCUS = false; }
                    let u = core::str::from_utf8(url_str()).unwrap_or("").to_string();
                    navigate(&u, true);
                    dirty = true;
                } else if let Some(page) = unsafe { PAGE.as_mut() } {
                    if let Some(f) = page.focus {
                        let form = page.lt.controls[f].form;
                        let c = f;
                        submit_form_at(form, c);
                        dirty = true;
                    }
                }
            }
            KeyAction::Esc => {
                print("[browser] ESC -> 退出\n");
                break;
            }
            _ => {}
        }
        if poll_tty_esc() {
            print("[browser] ESC -> 退出\n");
            break;
        }
        // 光标闪烁
        unsafe { BLINK_TICK += 1; }
        if unsafe { BLINK_TICK % 20 == 0 } {
            unsafe { CURSOR_BLINK = !CURSOR_BLINK; }
            let need = unsafe { URL_FOCUS } || unsafe { PAGE.as_ref().map(|p| p.focus.is_some()).unwrap_or(false) };
            if need {
                dirty = true;
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

    print("[browser] 正在退出\n");
    unsafe { exit(0); }
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
        write_fd(2, b"PANIC: browser\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
