// KunxOS 系统导航（全面屏手势 / 三键虚拟按键）共享实现。
//
// 供 launcher 与所有桌面应用复用，统一两种导航方式：
//   - 三键虚拟按键（Android 经典导航栏）：底部返回 / 桌面 / 最近任务三个按钮
//   - 全面屏手势（Android 10 风格）：底部白色指示条，上滑=桌面、
//     上滑悬停=最近任务、左右边缘内滑=返回
//
// 导航方式持久化在 /data/oobe/nav_mode（"gesture" 或 "buttons"），
// OOBE 首次启动时选择，设置应用中可随时更改；文件缺失时默认手势导航。
//
// 本模块只依赖裸 syscall 与调用方提供的画布（`NavCanvas`），no_std、无堆，
// 通过 `#[path]` 引入（如 launcher 的 `mod sys_nav;` 与 app_kit 的 `pub mod nav;`）。

// 共享毛玻璃绘制核心（每进程一份降采样缓冲；nav.rs 是共享入口，app_kit/launcher
// 经 `use nav::*` / `use sys_nav::*` 一并获得 glass 模块）。
#[path = "glass.rs"]
pub mod glass;
pub use glass::*;

// ==================== 常量 ====================

/// 导航栏高度（px）。
pub const NAV_H: i32 = 56;
/// 导航栏顶部 y（540x960 逻辑分辨率下 = 904）。
pub const NAV_Y: i32 = 960 - NAV_H;
use core::sync::atomic::{AtomicI32, Ordering};
static VIEW_W: AtomicI32 = AtomicI32::new(540);
static VIEW_H: AtomicI32 = AtomicI32::new(960);
pub fn set_viewport(w: i32, h: i32) {
    VIEW_W.store(w, Ordering::Relaxed);
    VIEW_H.store(h, Ordering::Relaxed);
}
pub fn nav_y() -> i32 { VIEW_H.load(Ordering::Relaxed) - NAV_H }

/// 边缘手势触发宽度（px）：距左右边缘小于该值的滑动判定为「返回」。
pub const EDGE_W: i32 = 24;
/// 底部上滑判定距离（px）。
pub const SWIPE_UP_DIST: i32 = 48;
/// 底部手势热区高度（px）：起点 y >= 960 - BOTTOM_ZONE 视为从底部发起。
pub const BOTTOM_ZONE: i32 = 130;
/// 上滑悬停（最近任务）最小持续时间（ms）。
pub const HOLD_MS: u64 = 450;
/// 边缘返回最小水平位移（px）。
pub const EDGE_SWIPE_DIST: i32 = 30;

/// 导航模式。
pub const MODE_GESTURE: u8 = 0;
pub const MODE_BUTTONS: u8 = 1;

const NAV_MODE_PATH: &[u8] = b"/data/oobe/nav_mode";

/// 系统导航动作。
#[derive(Clone, Copy, PartialEq)]
pub enum NavAction {
    None,
    /// 返回：应用内返回（本系统单窗口，等价退出应用）；桌面上无操作。
    Back,
    /// 桌面：回到主屏。
    Home,
    /// 最近任务：显示最近应用列表。
    Recents,
}

/// 导航动作对应的进程退出码（launcher 据此决定回到桌面后的状态）：
/// 0 = 普通返回，1 = 回桌面首页，2 = 打开最近任务。
pub fn nav_exit_code(a: NavAction) -> usize {
    match a {
        NavAction::Back => 0,
        NavAction::Home => 1,
        NavAction::Recents => 2,
        NavAction::None => 0,
    }
}

// ==================== 系统调用与文件读取 ====================

const SYS_READ: usize = 63;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;

use kunxrt::syscall::syscall3;

/// 读取当前导航模式：文件内容含 "buttons" → 三键，否则手势（默认）。
pub fn nav_mode() -> u8 {
    static mut MODE_BUF: [u8; 16] = [0u8; 16];
    static mut MODE_PATH: [u8; 32] = [0u8; 32];
    let n = unsafe { NAV_MODE_PATH.len().min(MODE_PATH.len() - 1) };
    unsafe {
        MODE_PATH[..n].copy_from_slice(&NAV_MODE_PATH[..n]);
        MODE_PATH[n] = 0;
        let fd = syscall3(SYS_OPENAT, AT_FDCWD, MODE_PATH.as_ptr() as usize, O_RDONLY) as isize;
        if fd < 0 {
            return MODE_GESTURE;
        }
        let mut total = 0usize;
        while total < MODE_BUF.len() {
            let r = syscall3(
                SYS_READ,
                fd as usize,
                MODE_BUF.as_ptr().add(total) as usize,
                MODE_BUF.len() - total,
            ) as isize;
            if r <= 0 {
                break;
            }
            total += r as usize;
        }
        syscall3(SYS_CLOSE, fd as usize, 0, 0);
        let data = &MODE_BUF[..total];
        if data.len() >= 7 && &data[..7] == b"buttons" {
            MODE_BUTTONS
        } else {
            MODE_GESTURE
        }
    }
}

// ==================== 画布抽象 ====================

/// 导航栏绘制画布：调用方只需实现单像素写入（毛玻璃背景读取可覆盖
/// [`NavCanvas::backdrop`]，默认返回 0 → 仅半透明着色、不模糊）。
pub trait NavCanvas {
    fn put(&mut self, x: i32, y: i32, color: u32);

    /// 读取屏幕坐标 (x, y) 处的背景像素（导航栏毛玻璃的模糊输入源）。
    /// 未实现时返回 0（导航栏退化为纯着色玻璃）。
    fn backdrop(&self, _x: i32, _y: i32) -> u32 {
        0
    }
}

/// `NavCanvas` 即毛玻璃目标（背景读取 + 像素写入），供 `glass_rect` 直接使用。
impl<C: NavCanvas> glass::GlassTarget for C {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        NavCanvas::backdrop(self, x, y)
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        NavCanvas::put(self, x, y, color)
    }
}

/// 在画布上填充矩形（由 put 组合，避免重复实现）。
fn canvas_fill<C: NavCanvas>(c: &mut C, x: i32, y: i32, w: i32, h: i32, color: u32) {
    for dy in 0..h {
        for dx in 0..w {
            c.put(x + dx, y + dy, color);
        }
    }
}

// ==================== 图标绘制（Android 风格，浅色主题深色几何） ====================

/// 浅色主题下的导航图标 / 手势指示条颜色。
const ICON_WHITE: u32 = 0x00_1A_1C_22;

/// Bresenham 直线（由 put 组合）。
fn canvas_line<C: NavCanvas>(c: &mut C, mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: u32) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        c.put(x0, y0, color);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

/// 返回箭头（左指向三角，`<` 轮廓）。
fn draw_back_icon<C: NavCanvas>(c: &mut C, cx: i32, cy: i32) {
    canvas_line(c, cx - 8, cy - 8, cx + 1, cy, ICON_WHITE);
    canvas_line(c, cx - 8, cy + 8, cx + 1, cy, ICON_WHITE);
    canvas_line(c, cx - 3, cy - 5, cx - 3, cy + 5, ICON_WHITE);
}

/// 桌面按钮（空心圆）。
fn draw_home_icon<C: NavCanvas>(c: &mut C, cx: i32, cy: i32) {
    let r = 9;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = dx * dx + dy * dy;
            if d <= r * r && d >= (r - 2) * (r - 2) {
                c.put(cx + dx, cy + dy, ICON_WHITE);
            }
        }
    }
}

/// 最近任务按钮（空心圆角方块：用矩形 + 四角圆点近似）。
fn draw_recents_icon<C: NavCanvas>(c: &mut C, cx: i32, cy: i32) {
    let half = 9;
    // 四条边
    for i in -half..=half {
        c.put(cx + i, cy - half, ICON_WHITE);
        c.put(cx + i, cy + half, ICON_WHITE);
        c.put(cx - half, cy + i, ICON_WHITE);
        c.put(cx + half, cy + i, ICON_WHITE);
    }
    // 圆角近似：四角各补一个 2x2
    for dx in -1..=1 {
        for dy in -1..=1 {
            c.put(cx - half + dx, cy - half + dy, ICON_WHITE);
            c.put(cx + half + dx, cy - half + dy, ICON_WHITE);
            c.put(cx - half + dx, cy + half + dy, ICON_WHITE);
            c.put(cx + half + dx, cy + half + dy, ICON_WHITE);
        }
    }
}

// ==================== 导航栏绘制 ====================

pub const NAV_BG: u32 = 0x00_FF_FF_FF;
pub const NAV_BORDER: u32 = 0x00_D9_DE_E6;

/// 在任意位置绘制一条导航栏（宽 540）：液态玻璃背景（背景模糊 + 浅色着色 +
/// 顶部 1px 高光描边）+ 三键或手势指示条。
/// 供全屏底部导航栏（`draw_nav_bar`）与设置/OOBE 预览复用。
pub fn draw_nav_bar_at<C: NavCanvas>(c: &mut C, y: i32, mode: u8) {
    // 毛玻璃底：模糊导航栏下方的应用/桌面内容，再叠浅色着色与顶部高光描边。
    let w = VIEW_W.load(Ordering::Relaxed);
    glass::glass_rect(c, 0, y, w, NAV_H, 0, false, true);
    let cy = y + NAV_H / 2;
    if mode == MODE_BUTTONS {
        draw_back_icon(c, w / 6, cy);
        draw_home_icon(c, w / 2, cy);
        draw_recents_icon(c, w * 5 / 6, cy);
    } else {
        // 手势指示条（白色圆角药丸，居中偏下）
        let pill_w = 108;
        let pill_h = 4;
        let px = (w - pill_w) / 2;
        let py = y + NAV_H - 18;
        canvas_fill(c, px, py, pill_w, pill_h, ICON_WHITE);
    }
}

/// 绘制底部导航栏（按当前模式）：三键或手势指示条。
pub fn draw_nav_bar<C: NavCanvas>(c: &mut C, mode: u8) {
    draw_nav_bar_at(c, nav_y(), mode);
}

// ==================== 命中测试 ====================

/// 三键模式下点击导航栏：按三等分区域判定动作。
/// 手势模式下导航栏点击无动作（返回 None）。
pub fn nav_hit(x: i32, y: i32, mode: u8) -> NavAction {
    if y < nav_y() {
        return NavAction::None;
    }
    if mode == MODE_GESTURE {
        return NavAction::None;
    }
    let w = VIEW_W.load(Ordering::Relaxed);
    if x < w / 3 {
        NavAction::Back
    } else if x < w * 2 / 3 {
        NavAction::Home
    } else {
        NavAction::Recents
    }
}

// ==================== 手势分类 ====================

/// 把一次触摸（按下点 → 抬起点，持续 dur_ms）分类为系统导航动作。
///
/// 规则（仅手势模式生效）：
///   - 底部上滑（起点在底部热区、位移向上且以竖直为主）：
///       短滑 → 桌面；悬停（≥ HOLD_MS）→ 最近任务
///   - 左右边缘内滑（起点距边 < EDGE_W、水平位移 ≥ EDGE_SWIPE_DIST）→ 返回
///   - 其余 → None
pub fn classify_swipe(
    press_x: i32,
    press_y: i32,
    cur_x: i32,
    cur_y: i32,
    dur_ms: u64,
    mode: u8,
) -> NavAction {
    if mode != MODE_GESTURE {
        return NavAction::None;
    }
    let dx = cur_x - press_x;
    let dy = cur_y - press_y;
    // 底部上滑
    if press_y >= VIEW_H.load(Ordering::Relaxed) - BOTTOM_ZONE && dy <= -SWIPE_UP_DIST && dx.abs() <= dy.abs() {
        if dur_ms >= HOLD_MS {
            NavAction::Recents
        } else {
            NavAction::Home
        }
    } else if (press_x < EDGE_W || press_x >= VIEW_W.load(Ordering::Relaxed) - EDGE_W)
        && dx.abs() >= EDGE_SWIPE_DIST
        && dx.abs() > dy.abs()
    {
        NavAction::Back
    } else {
        NavAction::None
    }
}
