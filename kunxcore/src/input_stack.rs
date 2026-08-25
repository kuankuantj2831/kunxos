//! 输入栈：指针/触摸事件、坐标转换、窗口命中与手势识别（点击/拖动/滑动）。
//!
//! - 坐标转换：设备原始坐标 → 屏幕坐标（固定点缩放 + 偏移）→ 窗口本地坐标。
//! - 窗口命中：`compositor::top_window_at` 取最上层窗口。
//! - 手势：按下时抓取窗口，拖动直接调用 `compositor::move_window` 实现流畅拖动；
//!   抬手时按位移与速度分类为点击 / 拖动 / 滑动。

use core::arch::asm;

use crate::compositor;

/// 手势分类阈值（像素）。
const TAP_DIST: i32 = 8;
/// 滑动判定速度阈值（像素/秒）。
const SWIPE_VEL: i32 = 300;

/// 事件类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Down,
    Move,
    Up,
}

/// 指针事件（设备原始坐标）。
#[derive(Clone, Copy)]
pub struct PointerEvent {
    pub kind: InputKind,
    pub x: i32,
    pub y: i32,
}

/// 识别出的手势。
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Gesture {
    /// 点击（含命中的窗口 id 与屏幕坐标）。
    Tap { window: Option<usize>, x: i32, y: i32 },
    /// 拖动结束（窗口 id 与总位移）。
    Drag { window: Option<usize>, dx: i32, dy: i32 },
    /// 滑动（窗口 id、位移与释放速度）。
    Swipe { window: Option<usize>, dx: i32, dy: i32, vx: i32, vy: i32 },
}

/// 输入栈状态。
struct InputStack {
    /// 屏幕坐标变换：`screen = (dev * scale + (1 << 9)) >> 10 + offset`。
    scale: i32,
    off_x: i32,
    off_y: i32,

    pressed: bool,
    press_x: i32,
    press_y: i32,
    cur_x: i32,
    cur_y: i32,
    /// 拖动抓取的窗口与抓取点相对窗口原点的偏移。
    grab: Option<usize>,
    grab_dx: i32,
    grab_dy: i32,
    /// 按下以来累计位移（判定 tap/drag）。
    total_dx: i32,
    total_dy: i32,
    /// 最近一次移动样本（用于速度）。
    last_t: u64,
    last_x: i32,
    last_y: i32,
    vx: i32,
    vy: i32,
    /// 最近一次完成的手势（单缓冲，供上层轮询）。
    gesture: Option<Gesture>,
}

static mut S: InputStack = InputStack {
    scale: 1024,
    off_x: 0,
    off_y: 0,
    pressed: false,
    press_x: 0,
    press_y: 0,
    cur_x: 0,
    cur_y: 0,
    grab: None,
    grab_dx: 0,
    grab_dy: 0,
    total_dx: 0,
    total_dy: 0,
    last_t: 0,
    last_x: 0,
    last_y: 0,
    vx: 0,
    vy: 0,
    gesture: None,
};

/// 读取物理计数频率。
unsafe fn cntfrq() -> u64 {
    let v: u64;
    asm!("mrs {}, cntfrq_el0", out(reg) v);
    v
}

/// 读取物理计数（单调递增）。
unsafe fn cntpct() -> u64 {
    let v: u64;
    asm!("mrs {}, cntpct_el0", out(reg) v);
    v
}

/// 当前时间（微秒）。
fn now_us() -> u64 {
    let freq = unsafe { cntfrq() };
    if freq == 0 {
        return 0;
    }
    // 先除后乘：避免 `cntpct * 1_000_000` 在长时间运行后 u64 溢出回绕
    let d = freq / 1_000_000;
    if d == 0 {
        return 0;
    }
    (unsafe { cntpct() }) / d
}

/// 设置屏幕坐标变换（固定点：`screen = dev * scale / 1024 + offset`）。
#[allow(dead_code)]
pub fn set_transform(scale: i32, off_x: i32, off_y: i32) {
    unsafe {
        S.scale = if scale <= 0 { 1024 } else { scale };
        S.off_x = off_x;
        S.off_y = off_y;
    }
}

/// 设备坐标 → 屏幕坐标。
///
/// 用 i64 中间量做定点缩放，避免 `x * scale` 在 i32 下溢出回绕
///（设备坐标来自外部输入，可达 u16 全量程 0..65535）。
fn to_screen(x: i32, y: i32) -> (i32, i32) {
    unsafe {
        let sx = ((x as i64 * S.scale as i64 + (1 << 9)) >> 10) as i32;
        let sy = ((y as i64 * S.scale as i64 + (1 << 9)) >> 10) as i32;
        (sx + S.off_x, sy + S.off_y)
    }
}

/// 处理一个指针事件，内部完成坐标转换、命中与手势状态机。
pub fn handle(ev: PointerEvent) {
    let (sx, sy) = to_screen(ev.x, ev.y);
    match ev.kind {
        InputKind::Down => on_down(sx, sy),
        InputKind::Move => on_move(sx, sy),
        InputKind::Up => on_up(sx, sy),
    }
}

fn on_down(sx: i32, sy: i32) {
    unsafe {
        S.pressed = true;
        S.press_x = sx;
        S.press_y = sy;
        S.cur_x = sx;
        S.cur_y = sy;
        S.total_dx = 0;
        S.total_dy = 0;
        S.vx = 0;
        S.vy = 0;
        S.last_t = now_us();
        S.last_x = sx;
        S.last_y = sy;
        // 窗口命中：抓取最上层窗口（用于拖动）
        S.grab = compositor::top_window_at(sx, sy);
        if let Some(w) = S.grab {
            if let Some((wx, wy, _, _)) = compositor::window_geom(w) {
                S.grab_dx = sx - wx;
                S.grab_dy = sy - wy;
            }
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

        // 抓取到窗口则直接拖动（平滑跟随）
        if let Some(w) = S.grab {
            compositor::move_window(w, sx - S.grab_dx, sy - S.grab_dy);
            // 低延迟输入路径：无活动动画时立即呈现拖动帧，
            // 不等下一个动画 tick（输入→画面延迟降低一整帧）
            if !crate::anim::active() {
                compositor::flush();
            }
        }

        // 速度估计（平滑）
        let t = now_us();
        let dt = t.saturating_sub(S.last_t).max(1000) as i64; // ≥1ms
        if dt > 0 {
            let dx = sx - S.last_x;
            let dy = sy - S.last_y;
            let nvx = (dx as i64 * 1_000_000 / dt) as i32;
            let nvy = (dy as i64 * 1_000_000 / dt) as i32;
            // 指数平滑，降低抖动
            S.vx = (S.vx * 2 + nvx) / 3;
            S.vy = (S.vy * 2 + nvy) / 3;
        }
        S.last_t = t;
        S.last_x = sx;
        S.last_y = sy;
        S.cur_x = sx;
        S.cur_y = sy;
    }
}

fn on_up(sx: i32, sy: i32) {
    unsafe {
        if !S.pressed {
            return;
        }
        S.pressed = false;
        let dx = S.total_dx;
        let dy = S.total_dy;
        let moved = dx.abs() + dy.abs();
        let g = if moved <= TAP_DIST {
            Gesture::Tap { window: S.grab, x: sx, y: sy }
        } else if S.vx.abs() > SWIPE_VEL || S.vy.abs() > SWIPE_VEL {
            Gesture::Swipe { window: S.grab, dx, dy, vx: S.vx, vy: S.vy }
        } else {
            Gesture::Drag { window: S.grab, dx, dy }
        };
        S.gesture = Some(g);
        S.grab = None;
    }
}

/// 轮询最近完成的手势（返回后清空）。
pub fn poll_gesture() -> Option<Gesture> {
    unsafe {
        let g = S.gesture;
        S.gesture = None;
        g
    }
}

/// 屏幕坐标 → 窗口本地坐标（未命中窗口返回 `None`）。
#[allow(dead_code)]
pub fn to_window_local(w: usize, sx: i32, sy: i32) -> Option<(i32, i32)> {
    compositor::window_geom(w).map(|(wx, wy, ww, wh)| {
        let lx = sx - wx;
        let ly = sy - wy;
        if lx >= 0 && ly >= 0 && (lx as u32) < ww && (ly as u32) < wh {
            Some((lx, ly))
        } else {
            None
        }
    }).flatten()
}

/// 命中测试便捷封装。
#[allow(dead_code)]
pub fn top_window_at(sx: i32, sy: i32) -> Option<usize> {
    compositor::top_window_at(sx, sy)
}
