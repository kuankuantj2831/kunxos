//! 全屏手势系统：底部上滑返回桌面 / 悬停进入多任务、左右边缘内滑返回上一级。
//!
//! 手势区域：底部 30px、左右边缘 20px；起始点落入已登记可滚动区域时优先滚动。
//! 动画：返回桌面 300ms 下滑退出（ease-out）；多任务 250ms 缩放 0.85 并上移、
//! 背景变暗；返回上一级 250ms 右滑退出（ease-out）。底部绘制 5px Home Indicator。

#![allow(static_mut_refs)]

use crate::anim;
use crate::compositor;
use crate::timer;

// ==================== 手势区域 ====================

/// 底部手势区高度（像素）。
const BOTTOM_ZONE_H: i32 = 30;
/// 左右边缘手势区宽度（像素）。
const EDGE_ZONE_W: i32 = 20;

// ==================== 手势阈值 ====================

/// 底部上滑判定位移（像素）。
const BOTTOM_MIN_DIST: i32 = 80;
/// 返回桌面最小上滑速度（像素/秒）。
const HOME_MIN_VEL: i32 = 500;
/// 多任务悬停时长（微秒）。
const RECENT_HOLD_US: u64 = 300_000;
/// 悬停抖动容忍（像素）。
const HOLD_JITTER: i32 = 12;
/// 边缘内滑判定位移（像素）。
const BACK_MIN_DIST: i32 = 50;
/// 边缘内滑最小水平速度（像素/秒）。
const BACK_MIN_VEL: i32 = 300;
/// 手势方向回退容忍（像素，回退超过则取消跟踪）。
const CANCEL_BACK: i32 = 8;

// ==================== 动画参数 ====================

/// 返回桌面动画时长（微秒）。
const HOME_DUR_US: u64 = 300_000;
/// 多任务动画时长（微秒）。
const RECENT_DUR_US: u64 = 250_000;
/// 返回上一级动画时长（微秒）。
const BACK_DUR_US: u64 = 250_000;
/// 多任务缩放目标（0.85，定点 1.0 = 256）。
const RECENT_SCALE: f32 = 218.0;
/// 多任务上移量（像素）。
const RECENT_UP: i32 = 48;
/// 多任务背景变暗层透明度。
const RECENT_DIM_ALPHA: f32 = 150.0;

// ==================== Home Indicator ====================

/// 指示器高度（像素）。
const IND_H: u32 = 5;
/// 指示器宽度（像素）。
const IND_W: u32 = 160;
/// 指示器距屏幕底边的距离（像素）。
const IND_MARGIN: i32 = 8;
/// 指示器透明度（0.3 × 255）。
const IND_ALPHA: u8 = 77;

// ==================== 事件 ====================

/// 事件类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Down,
    Move,
    Up,
}

/// 指针事件（屏幕坐标）。
#[derive(Clone, Copy)]
pub struct PointerEvent {
    pub kind: InputKind,
    pub x: i32,
    pub y: i32,
}

// ==================== 状态 ====================

/// 跟踪阶段。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Bottom,
    EdgeLeft,
    EdgeRight,
}

/// 可滚动区域登记项。
#[derive(Clone, Copy)]
struct Scrollable {
    window: usize,
    r: compositor::Rect,
}

const MAX_SCROLLABLES: usize = 16;

struct GestureState {
    phase: Phase,
    pressed: bool,
    press_x: i32,
    press_y: i32,
    cur_x: i32,
    cur_y: i32,
    total_dx: i32,
    total_dy: i32,
    last_t: u64,
    last_x: i32,
    last_y: i32,
    /// 上滑峰值速度（像素/秒，≥0）。
    pk_up: i32,
    /// 内滑峰值速度（像素/秒，≥0）。
    pk_in: i32,
    /// 底部上滑是否已越过位移阈值。
    crossed: bool,
    /// 悬停起始时刻（越过阈值且未抖动时记录）。
    hold_t: u64,
    hold_x: i32,
    hold_y: i32,
    /// 起始点落在可滚动区域内（优先滚动）。
    scroll_override: bool,
    /// 手势抓取的应用窗口。
    app: Option<usize>,
    app_x: i32,
    app_y: i32,
    /// 多任务背景变暗层。
    dim: Option<usize>,
    multitask_active: bool,
}

static mut S: GestureState = GestureState {
    phase: Phase::Idle,
    pressed: false,
    press_x: 0,
    press_y: 0,
    cur_x: 0,
    cur_y: 0,
    total_dx: 0,
    total_dy: 0,
    last_t: 0,
    last_x: 0,
    last_y: 0,
    pk_up: 0,
    pk_in: 0,
    crossed: false,
    hold_t: 0,
    hold_x: 0,
    hold_y: 0,
    scroll_override: false,
    app: None,
    app_x: 0,
    app_y: 0,
    dim: None,
    multitask_active: false,
};

/// 桌面窗口（返回桌面的目标；未设置时桌面位于 z 序最低端）。
static mut DESKTOP: Option<usize> = None;

static mut SCROLLABLES: [Scrollable; MAX_SCROLLABLES] =
    [Scrollable { window: usize::MAX, r: compositor::Rect::empty() }; MAX_SCROLLABLES];
static mut SCROLLABLE_COUNT: usize = 0;

/// Home Indicator 窗口。
static mut INDICATOR: Option<usize> = None;

// ==================== 初始化 ====================

/// 初始化手势系统：创建 Home Indicator 与多任务变暗层（常驻，按需显隐）。
pub fn init() {
    if !compositor::ready() {
        return;
    }
    let w = compositor::width();
    let h = compositor::height();
    // Home Indicator：底部 5px 白色细线，透明度 0.3
    let iw = IND_W.min(w.saturating_sub(20)).max(40);
    let ix = ((w - iw) / 2) as i32;
    let iy = h as i32 - IND_H as i32 - IND_MARGIN;
    let ind = compositor::create_window(ix, iy, iw, IND_H, i32::MAX, IND_ALPHA);
    if ind != usize::MAX {
        if let Some(s) = compositor::window_surface(ind) {
            s.fill(compositor::Color::WHITE);
        }
        unsafe { INDICATOR = Some(ind); }
    }
    // 多任务背景变暗层：全屏黑色，初始透明且位于最低 z
    let dim = compositor::create_window(0, 0, w, h, i32::MIN, 0);
    if dim != usize::MAX {
        if let Some(s) = compositor::window_surface(dim) {
            s.fill(compositor::Color::BLACK);
        }
        unsafe { S.dim = Some(dim); }
    }
}

/// 指定桌面窗口。
pub fn set_desktop(id: usize) {
    unsafe { DESKTOP = Some(id); }
}

// ==================== 可滚动区域（冲突处理） ====================

/// 登记可滚动区域：手势起始点落入时优先滚动，不触发系统手势。
pub fn register_scrollable(window: usize, r: compositor::Rect) {
    unsafe {
        for i in 0..SCROLLABLE_COUNT {
            if SCROLLABLES[i].window == window && SCROLLABLES[i].r == r {
                return;
            }
        }
        if SCROLLABLE_COUNT < MAX_SCROLLABLES {
            SCROLLABLES[SCROLLABLE_COUNT] = Scrollable { window, r };
            SCROLLABLE_COUNT += 1;
        }
    }
}

/// 注销窗口登记的全部可滚动区域。
pub fn unregister_scrollable(window: usize) {
    unsafe {
        let mut n = 0;
        for i in 0..SCROLLABLE_COUNT {
            if SCROLLABLES[i].window != window {
                SCROLLABLES[n] = SCROLLABLES[i];
                n += 1;
            }
        }
        SCROLLABLE_COUNT = n;
    }
}

fn point_in_scrollable(x: i32, y: i32) -> bool {
    unsafe {
        for i in 0..SCROLLABLE_COUNT {
            if SCROLLABLES[i].r.contains(x, y) {
                return true;
            }
        }
    }
    false
}

// ==================== 窗口查询 ====================

fn desktop_z() -> i32 {
    unsafe {
        match DESKTOP {
            Some(id) => compositor::window_z(id).unwrap_or(i32::MIN),
            None => i32::MIN,
        }
    }
}

/// 当前应用：桌面之上最上层的可见窗口（跳过指示器与变暗层）。
fn current_app() -> Option<usize> {
    unsafe {
        let floor = desktop_z();
        let mut ceil = i32::MAX;
        loop {
            let Some(id) = compositor::top_visible_window_between(floor, ceil) else {
                return None;
            };
            if Some(id) != INDICATOR && Some(id) != S.dim {
                return Some(id);
            }
            let Some(z) = compositor::window_z(id) else { return None };
            ceil = z;
        }
    }
}

// ==================== 事件处理 ====================

/// 处理一个指针事件（屏幕坐标）。
pub fn handle(ev: PointerEvent) {
    match ev.kind {
        InputKind::Down => on_down(ev.x, ev.y),
        InputKind::Move => on_move(ev.x, ev.y),
        InputKind::Up => on_up(),
    }
}

fn on_down(sx: i32, sy: i32) {
    unsafe {
        if S.pressed {
            reset_tracking();
        }
        S.pressed = true;
        S.press_x = sx;
        S.press_y = sy;
        S.cur_x = sx;
        S.cur_y = sy;
        S.total_dx = 0;
        S.total_dy = 0;
        S.pk_up = 0;
        S.pk_in = 0;
        S.last_t = timer::now_us();
        S.last_x = sx;
        S.last_y = sy;
        S.crossed = false;
        S.hold_t = 0;
        S.scroll_override = point_in_scrollable(sx, sy);

        // 多任务态下任意触摸先退出多任务
        if S.multitask_active {
            dismiss_multitask();
        }

        // 冲突处理：可滚动区域内优先滚动；边缘手势按起始位置判定，
        // 起始点在页面内部时交给页面水平滚动
        if S.scroll_override {
            S.phase = Phase::Idle;
            return;
        }
        let w = compositor::width() as i32;
        let h = compositor::height() as i32;
        S.phase = if sy >= h - BOTTOM_ZONE_H {
            Phase::Bottom
        } else if sx < EDGE_ZONE_W {
            Phase::EdgeLeft
        } else if sx >= w - EDGE_ZONE_W {
            Phase::EdgeRight
        } else {
            Phase::Idle
        };
        if S.phase != Phase::Idle {
            S.app = current_app();
        }
    }
}

fn on_move(sx: i32, sy: i32) {
    unsafe {
        if !S.pressed {
            return;
        }
        S.total_dx += sx - S.cur_x;
        S.total_dy += sy - S.cur_y;
        S.cur_x = sx;
        S.cur_y = sy;

        // 瞬时速度（dt 计算，采样率无关）
        let t = timer::now_us();
        let dt = t.saturating_sub(S.last_t).max(1000) as i64;
        if dt > 0 {
            let dx = sx - S.last_x;
            let dy = sy - S.last_y;
            let ivx = (dx as i64 * 1_000_000 / dt) as i32;
            let ivy = (dy as i64 * 1_000_000 / dt) as i32;
            if ivy < 0 {
                S.pk_up = S.pk_up.max(-ivy);
            }
            match S.phase {
                Phase::EdgeLeft => {
                    if ivx > 0 {
                        S.pk_in = S.pk_in.max(ivx);
                    }
                }
                Phase::EdgeRight => {
                    if ivx < 0 {
                        S.pk_in = S.pk_in.max(-ivx);
                    }
                }
                _ => {}
            }
        }
        S.last_t = t;
        S.last_x = sx;
        S.last_y = sy;

        match S.phase {
            Phase::Bottom => track_bottom(),
            Phase::EdgeLeft | Phase::EdgeRight => track_edge(),
            Phase::Idle => {}
        }
    }
}

fn on_up() {
    unsafe {
        if !S.pressed {
            return;
        }
        match S.phase {
            Phase::Bottom => {
                let dist = S.press_y - S.cur_y;
                if dist > BOTTOM_MIN_DIST {
                    let held = S.hold_t != 0
                        && timer::now_us().saturating_sub(S.hold_t) >= RECENT_HOLD_US;
                    if held {
                        trigger_multitask();
                    } else if S.pk_up > HOME_MIN_VEL {
                        trigger_home();
                    }
                }
            }
            Phase::EdgeLeft => {
                if S.total_dx > BACK_MIN_DIST && S.pk_in > BACK_MIN_VEL {
                    trigger_back();
                }
            }
            Phase::EdgeRight => {
                if -S.total_dx > BACK_MIN_DIST && S.pk_in > BACK_MIN_VEL {
                    trigger_back();
                }
            }
            Phase::Idle => {}
        }
        reset_tracking();
    }
}

// ==================== 跟踪 ====================

/// 底部手势跟踪：越过位移阈值后检测悬停（抖动容忍），悬停超时触发多任务。
fn track_bottom() {
    unsafe {
        if S.cur_y - S.press_y > CANCEL_BACK {
            S.phase = Phase::Idle;
            return;
        }
        let dist = S.press_y - S.cur_y;
        if !S.crossed {
            if dist > BOTTOM_MIN_DIST {
                S.crossed = true;
                S.hold_t = timer::now_us();
                S.hold_x = S.cur_x;
                S.hold_y = S.cur_y;
            }
            return;
        }
        let jx = S.cur_x - S.hold_x;
        let jy = S.cur_y - S.hold_y;
        if jx.abs() > HOLD_JITTER || jy.abs() > HOLD_JITTER {
            S.hold_t = timer::now_us();
            S.hold_x = S.cur_x;
            S.hold_y = S.cur_y;
        } else if timer::now_us().saturating_sub(S.hold_t) >= RECENT_HOLD_US {
            trigger_multitask();
            S.phase = Phase::Idle;
        }
    }
}

/// 边缘手势跟踪：向外滑动超过容忍则取消。
fn track_edge() {
    unsafe {
        let inward = match S.phase {
            Phase::EdgeLeft => S.total_dx,
            _ => -S.total_dx,
        };
        if inward < -CANCEL_BACK {
            S.phase = Phase::Idle;
        }
    }
}

fn reset_tracking() {
    unsafe {
        S.phase = Phase::Idle;
        S.pressed = false;
        S.crossed = false;
        S.hold_t = 0;
        S.pk_up = 0;
        S.pk_in = 0;
    }
}

// ==================== 手势动作 ====================

/// 返回桌面：当前应用 300ms 下滑退出（y → 屏幕高度），ease-out。
fn trigger_home() {
    unsafe {
        let Some(app) = S.app.or_else(current_app) else { return };
        if S.multitask_active {
            dismiss_multitask();
        }
        let Some((_, y, _, _)) = compositor::window_geom(app) else { return };
        anim::cancel_window(app);
        anim::tween(
            app,
            anim::AnimProp::Y,
            y as f32,
            compositor::height() as f32,
            HOME_DUR_US,
            anim::Easing::ColorOSStandard,
            Some(finish_hide),
        );
        compositor::flush();
    }
}

/// 进入多任务：当前应用 250ms 缩放至 0.85 并上移，背景变暗。
fn trigger_multitask() {
    unsafe {
        if S.multitask_active {
            return;
        }
        let Some(app) = S.app.or_else(current_app) else { return };
        let Some((x, y, _, _)) = compositor::window_geom(app) else { return };
        let Some(az) = compositor::window_z(app) else { return };
        anim::cancel_window(app);
        S.app_x = x;
        S.app_y = y;
        anim::tween(
            app,
            anim::AnimProp::Scale,
            256.0,
            RECENT_SCALE,
            RECENT_DUR_US,
            anim::Easing::ColorOSStandard,
            None,
        );
        anim::tween(
            app,
            anim::AnimProp::Y,
            y as f32,
            (y - RECENT_UP) as f32,
            RECENT_DUR_US,
            anim::Easing::ColorOSStandard,
            None,
        );
        if let Some(dim) = S.dim {
            compositor::set_z(dim, az.saturating_sub(1));
            anim::cancel_window(dim);
            anim::tween(
                dim,
                anim::AnimProp::Alpha,
                0.0,
                RECENT_DIM_ALPHA,
                RECENT_DUR_US,
                anim::Easing::ColorOSStandard,
                None,
            );
        }
        S.multitask_active = true;
        compositor::flush();
    }
}

/// 退出多任务：恢复应用位置与缩放，变暗层复位。
fn dismiss_multitask() {
    unsafe {
        if !S.multitask_active {
            return;
        }
        if let Some(app) = S.app {
            anim::cancel_window(app);
            compositor::move_window(app, S.app_x, S.app_y);
            compositor::set_scale(app, 256);
        }
        if let Some(dim) = S.dim {
            anim::cancel_window(dim);
            compositor::set_alpha(dim, 0);
            compositor::set_z(dim, i32::MIN);
        }
        S.multitask_active = false;
        compositor::flush();
    }
}

/// 返回上一级：当前页面 250ms 右滑退出（x → 屏幕宽度），ease-out。
fn trigger_back() {
    unsafe {
        let Some(app) = S.app.or_else(current_app) else { return };
        if S.multitask_active {
            dismiss_multitask();
        }
        let Some((x, _, _, _)) = compositor::window_geom(app) else { return };
        anim::cancel_window(app);
        anim::tween(
            app,
            anim::AnimProp::X,
            x as f32,
            compositor::width() as f32,
            BACK_DUR_US,
            anim::Easing::ColorOSStandard,
            Some(finish_hide),
        );
        compositor::flush();
    }
}

/// 退出动画完成：隐藏窗口。
fn finish_hide(window: usize) {
    compositor::set_alpha(window, 0);
}
