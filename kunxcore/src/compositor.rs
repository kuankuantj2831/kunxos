//! 合成器：多窗口/图层管理、Z 序、透明度 alpha 混合与脏区域追踪。
//!
//! 设计：
//!   - `canvas`：持久化的完整画面（始终保存正确帧），脏区域只在这里重合成；
//!   - `front` / `back`：两块扫描输出缓冲，`present` 把 canvas 拷贝到 back
//!     再页翻转（`gpu_simple::present`），实现双缓冲无撕裂；
//!   - 窗口像素格式为 `0x00RRGGBB`（与 XRGB8888 帧缓冲一致），
//!     窗口透明度是每窗口独立的 `alpha`（0..255），混合采用直通 alpha 公式。

#![allow(static_mut_refs)]

use core::arch::asm;
use core::ptr::{copy_nonoverlapping, null_mut};

use crate::gpu_simple;
use crate::heap;

/// 屏蔽 IRQ 进入合成器临界区（窗口表 / 脏区域 / 表面生命周期）。
///
/// 合成与呈现由定时器 IRQ（`anim::frame_tick`）驱动，而窗口操作来自进程
/// 上下文（系统调用 / 内核 Shell）。两者并发时，进程侧必须先屏蔽 IRQ，
/// 保证「先摘除窗口再释放表面」等操作对 IRQ 侧原子可见，杜绝
/// `composite()` 读取已 `kfree` 的表面（use-after-free）。
unsafe fn irq_save() -> u64 {
    // 保存 DAIF 后屏蔽 IRQ：调用方可能来自 IRQ 上下文（anim::frame_tick →
    // apply → set_scale/move_window），无条件 daifclr 会在中断处理器中途
    // 重新开中断 → 嵌套中断 → 栈/状态破坏（跳转垃圾地址）。
    let d: u64;
    asm!("mrs {}, daif", out(reg) d);
    asm!("msr daifset, #2");
    d
}

unsafe fn irq_restore(saved: u64) {
    asm!("msr daif, {}", in(reg) saved);
}

/// 最大窗口（图层）数。
pub const MAX_WINDOWS: usize = 16;
/// 最大脏区域数（超出则整屏刷新）。
pub const MAX_DIRTY: usize = 32;
/// 窗口表面最大字节数：防止 `w*h*4` 在 64 位下回绕导致的小分配 + 大规模越界写。
const MAX_SURFACE_BYTES: usize = 64 * 1024 * 1024;

// ==================== 颜色 / 矩形 / 表面 ====================

/// 8 位 RGB 颜色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b }
    }

    /// 转为 `0x00RRGGBB` 像素。
    #[inline]
    pub fn pixel(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    #[allow(dead_code)]
    pub const RED: Color = Color::rgb(200, 40, 40);
    pub const GREEN: Color = Color::rgb(40, 180, 80);
    #[allow(dead_code)]
    pub const BLUE: Color = Color::rgb(50, 100, 220);
    pub const GRAY: Color = Color::rgb(120, 120, 120);
    pub const LIGHT: Color = Color::rgb(230, 230, 230);
    pub const DARK: Color = Color::rgb(30, 32, 40);
    pub const ACCENT: Color = Color::rgb(66, 133, 244);
}

/// 轴对齐矩形。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    pub const fn empty() -> Rect {
        Rect { x: 0, y: 0, w: 0, h: 0 }
    }

    #[inline]
    pub fn right(self) -> i32 {
        // 饱和加法：w 已由 MAX_SURFACE_BYTES 限制（< 2^31），x 来自窗口
        // 位置（触摸/动画），极端组合不再 i32 回绕成负值
        self.x.saturating_add(self.w as i32)
    }

    #[inline]
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.h as i32)
    }

    /// 点是否在矩形内（含边界）。
    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// 是否与另一矩形相交。
    pub fn intersects(self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }

    /// 求两矩形交集（不相交返回 `None`）。
    pub fn intersect(self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        if r <= x || b <= y {
            None
        } else {
            Some(Rect { x, y, w: (r - x) as u32, h: (b - y) as u32 })
        }
    }
}

/// 一块 ARGB/像素表面（`0x00RRGGBB`）。
#[derive(Clone, Copy)]
pub struct Surface {
    pub pixels: *mut u32,
    pub w: u32,
    pub h: u32,
}

impl Surface {
    pub const fn empty() -> Surface {
        Surface { pixels: null_mut(), w: 0, h: 0 }
    }

    #[inline]
    pub(crate) fn set(&self, x: i32, y: i32, c: u32) {
        if !self.pixels.is_null() && x >= 0 && y >= 0 && (x as u32) < self.w && (y as u32) < self.h {
            unsafe {
                *self.pixels.add(y as usize * self.w as usize + x as usize) = c;
            }
        }
    }

    /// 整屏填充。
    pub fn fill(&self, c: Color) {
        if self.pixels.is_null() {
            return;
        }
        let px = c.pixel();
        for i in 0..(self.w as usize * self.h as usize) {
            unsafe {
                *self.pixels.add(i) = px;
            }
        }
    }

    /// 矩形填充（越界部分裁剪）。
    pub fn fill_rect(&self, r: Rect, c: Color) {
        let px = c.pixel();
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                self.set(x, y, px);
            }
        }
    }

    /// 矩形描边（`t` 像素边框）。
    pub fn draw_rect(&self, r: Rect, c: Color, t: u32) {
        for i in 0..t as i32 {
            let rr = Rect::new(r.x + i, r.y + i, (r.w as i32 - 2 * i).max(0) as u32, (r.h as i32 - 2 * i).max(0) as u32);
            // 上下边
            for x in rr.x..rr.right() {
                self.set(x, rr.y, c.pixel());
                self.set(x, rr.bottom() - 1, c.pixel());
            }
            // 左右边
            for y in rr.y..rr.bottom() {
                self.set(rr.x, y, c.pixel());
                self.set(rr.right() - 1, y, c.pixel());
            }
        }
    }

    /// 把 `src` 表面按 1:1 拷贝到 `(dx, dy)`（越界裁剪）。
    pub fn blit(&self, dx: i32, dy: i32, src: &Surface) {
        if src.pixels.is_null() || self.pixels.is_null() {
            return;
        }
        for y in 0..src.h as i32 {
            for x in 0..src.w as i32 {
                let v = unsafe { *src.pixels.add(y as usize * src.w as usize + x as usize) };
                self.set(dx + x, dy + y, v);
            }
        }
    }
}

// ==================== 窗口 ====================

/// 一个窗口（图层）。
pub struct Window {
    #[allow(dead_code)]
    pub id: usize,
    pub active: bool,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub z: i32,
    pub alpha: u8,
    /// 窗口缩放（定点：1.0 = 256），以窗口中心为锚点，供动画引擎使用。
    pub scale: u32,
    /// 窗口内容表面（w*h 像素，由 UI 框架绘制）。
    pub surface: Surface,
}

impl Window {
    const fn empty() -> Window {
        Window {
            id: 0,
            active: false,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            z: 0,
            alpha: 255,
            scale: 256,
            surface: Surface::empty(),
        }
    }

    fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.w, self.h)
    }

    /// 缩放后的可视矩形（以窗口中心为锚点；scale == 256 时与 `rect` 一致）。
    fn visual_rect(&self) -> Rect {
        if self.scale == 256 {
            return self.rect();
        }
        // u64 运算防 w*scale 溢出（宽扁窗口 w 可达 16M 像素 × scale 1024 ≈ 16G）
        let sw = ((self.w as u64 * self.scale as u64) / 256).min(u32::MAX as u64) as u32;
        let sh = ((self.h as u64 * self.scale as u64) / 256).min(u32::MAX as u64) as u32;
        let cx = self.x + self.w as i32 / 2;
        let cy = self.y + self.h as i32 / 2;
        Rect::new(cx - sw as i32 / 2, cy - sh as i32 / 2, sw, sh)
    }
}

// ==================== 合成器状态 ====================

/// 合成器：双缓冲直写模式。
///
/// - `front` / `back`：两块扫描输出缓冲。`composite` 直接合成到 `back`
///   （**不再有 canvas 中间层**），`present` 只做页翻转并交换 front/back。
/// - 由于每次翻转后 `back` 就是上一帧完整正确的 `front`，脏区域合成只需
///   覆盖变化部分，非脏区域天然保持上一帧内容 —— 双缓冲语义与窗口/
///   脏区域逻辑与旧 canvas 方案完全一致，但省掉了每帧整屏 canvas→back 拷贝。
/// - 初始化时两块缓冲都填充背景色，保证任意时刻任一缓冲都持有正确帧。
struct Compositor {
    width: u32,
    height: u32,
    front: *mut u32,
    back: *mut u32,
    windows: [Window; MAX_WINDOWS],
    dirty: [Rect; MAX_DIRTY],
    dirty_count: usize,
}

const EMPTY: Compositor = Compositor {
    width: 0,
    height: 0,
    front: null_mut(),
    back: null_mut(),
    windows: [const { Window::empty() }; MAX_WINDOWS],
    dirty: [Rect::empty(); MAX_DIRTY],
    dirty_count: 0,
};

static mut C: Compositor = EMPTY;

/// 缩放合成用的源 X 坐标预计算表（宽 ≤ 屏幕宽 540，留余量防越界）。
static mut SX_TABLE: [u32; 1024] = [0; 1024];

// ==================== 脏区域 ====================

fn invalidate(r: Rect) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    unsafe {
        // 与已有脏区域合并（重叠/相邻 → 取包围盒）
        for i in 0..C.dirty_count {
            let d = C.dirty[i];
            if d.intersects(&r) || (d.right() == r.x && d.y == r.y) {
                let x = d.x.min(r.x);
                let y = d.y.min(r.y);
                let rr = d.right().max(r.right());
                let bb = d.bottom().max(r.bottom());
                C.dirty[i] = Rect::new(x, y, (rr - x) as u32, (bb - y) as u32);
                return;
            }
        }
        // 超出容量则退化为整屏刷新
        if C.dirty_count >= MAX_DIRTY {
            C.dirty_count = 1;
            C.dirty[0] = Rect::new(0, 0, C.width, C.height);
            return;
        }
        C.dirty[C.dirty_count] = r;
        C.dirty_count += 1;
    }
}

// ==================== alpha 混合 ====================

#[inline]
fn blend_pixel(dst: u32, src: u32, alpha: u8) -> u32 {
    if alpha == 255 {
        return src;
    }
    if alpha == 0 {
        return dst;
    }
    let a = alpha as u32;
    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let r = (sr * a + dr * (255 - a)) / 255;
    let g = (sg * a + dg * (255 - a)) / 255;
    let b = (sb * a + db * (255 - a)) / 255;
    (r << 16) | (g << 8) | b
}

// ==================== 公共接口 ====================

/// 初始化合成器：分配两块扫描缓冲并填充初始背景。
///
/// 注意：初始化阶段**不**翻转扫描输出（`/dev/fb0` 的缓冲仍由 launcher 等
/// 用户态进程直接绘制）；只有当动画运行时 `present` 才接管显示。
/// 两块缓冲都填满背景，保证任意时刻翻转后的 `back` 都持有完整正确帧。
pub fn init() {
    let w = gpu_simple::width();
    let h = gpu_simple::height();
    if w == 0 || h == 0 {
        return;
    }
    let front = gpu_simple::alloc_fb();
    let back = gpu_simple::alloc_fb();
    unsafe {
        C.width = w;
        C.height = h;
        C.front = front as *mut u32;
        C.back = back as *mut u32;
        let n = (w as usize) * (h as usize);
        for i in 0..n {
            *C.front.add(i) = Color::DARK.pixel();
            *C.back.add(i) = Color::DARK.pixel();
        }
    }
    // 两块缓冲均已正确：无脏区域待合成
    unsafe {
        C.dirty_count = 0;
    }
}

/// 合成器是否可用。
pub fn ready() -> bool {
    unsafe { C.width != 0 }
}

pub fn width() -> u32 {
    unsafe { C.width }
}

pub fn height() -> u32 {
    unsafe { C.height }
}

/// 创建窗口，返回窗口 id（失败返回 usize::MAX）。
pub fn create_window(x: i32, y: i32, w: u32, h: u32, z: i32, alpha: u8) -> usize {
    let id = (0..MAX_WINDOWS).find(|&i| unsafe { !C.windows[i].active });
    let Some(id) = id else { return usize::MAX };
    let size = w as usize * h as usize;
    // 拒绝 0 尺寸与超大尺寸（防 `size*4` 回绕 → 小分配 + fill 越界写）
    if w == 0 || h == 0 || size > MAX_SURFACE_BYTES / 4 {
        return usize::MAX;
    }
    let pixels = heap::kmalloc(size * 4) as *mut u32;
    if pixels.is_null() {
        return usize::MAX;
    }
    unsafe {
        let _d = irq_save();
        C.windows[id] = Window {
            id,
            active: true,
            x,
            y,
            w,
            h,
            z,
            alpha,
            scale: 256,
            surface: Surface { pixels, w, h },
        };
        // 默认填充浅色，立即可见
        C.windows[id].surface.fill(Color::WHITE);
        irq_restore(_d);
    }
    invalidate(Rect::new(x, y, w, h));
    id
}

/// 销毁窗口。
#[allow(dead_code)]
pub fn destroy_window(id: usize) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        // 屏蔽 IRQ：先摘除窗口再释放表面，杜绝 IRQ 侧 composite 读已释放内存。
        let _d = irq_save();
        // 取消该窗口的全部动画：id 会被复用，残留动画会驱动新窗口（H-12/动画残留）
        crate::anim::cancel_window(id);
        if C.windows[id].active {
            let w = &mut C.windows[id];
            invalidate(w.visual_rect());
            w.active = false;
            let px = w.surface.pixels as *mut u8;
            w.surface = Surface::empty();
            heap::kfree(px);
        }
        irq_restore(_d);
    }
}

/// 获取窗口内容表面（UI 框架绘制目标）。
pub fn window_surface(id: usize) -> Option<Surface> {
    if id >= MAX_WINDOWS {
        return None;
    }
    unsafe {
        if C.windows[id].active {
            Some(C.windows[id].surface)
        } else {
            None
        }
    }
}

/// 窗口几何（屏幕坐标）。
pub fn window_geom(id: usize) -> Option<(i32, i32, u32, u32)> {
    if id >= MAX_WINDOWS {
        return None;
    }
    unsafe {
        if C.windows[id].active {
            let w = &C.windows[id];
            Some((w.x, w.y, w.w, w.h))
        } else {
            None
        }
    }
}

/// 移动窗口（使旧/新区域失效）。
pub fn move_window(id: usize, x: i32, y: i32) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        // 屏蔽 IRQ：与 IRQ 侧 composite 读窗口状态互斥（防止撕裂）
        let _d = irq_save();
        if !C.windows[id].active {
            irq_restore(_d);
            return;
        }
        let old = C.windows[id].visual_rect();
        if old.x == x && old.y == y {
            irq_restore(_d);
            return;
        }
        invalidate(old);
        C.windows[id].x = x;
        C.windows[id].y = y;
        invalidate(C.windows[id].visual_rect());
        irq_restore(_d);
    }
}

/// 调整窗口大小（重分配内容表面）。
#[allow(dead_code)]
pub fn resize_window(id: usize, w: u32, h: u32) {
    if id >= MAX_WINDOWS || w == 0 || h == 0 {
        return;
    }
    // 防尺寸回绕（同 create_window）
    let size = w as usize * h as usize;
    if size > MAX_SURFACE_BYTES / 4 {
        return;
    }
    unsafe {
        let _d = irq_save();
        if !C.windows[id].active || (C.windows[id].w == w && C.windows[id].h == h) {
            irq_restore(_d);
            return;
        }
        invalidate(C.windows[id].visual_rect());
        let old = C.windows[id].surface;
        let np = heap::kmalloc(size * 4) as *mut u32;
        if np.is_null() {
            invalidate(C.windows[id].visual_rect());
            irq_restore(_d);
            return;
        }
        let ns = Surface { pixels: np, w, h };
        ns.fill(Color::WHITE);
        // 拷贝旧内容（取交集）
        let cw = old.w.min(w);
        let ch = old.h.min(h);
        for y in 0..ch {
            copy_nonoverlapping(
                old.pixels.add(y as usize * old.w as usize),
                np.add(y as usize * w as usize),
                cw as usize,
            );
        }
        // 先切换 surface 再释放旧表面：composite 要么看到旧表面（kfree 前
        // 已完成），要么看到新表面（切换后），不会读到已释放内存。
        C.windows[id].w = w;
        C.windows[id].h = h;
        C.windows[id].surface = ns;
        heap::kfree(old.pixels as *mut u8);
        invalidate(C.windows[id].visual_rect());
        irq_restore(_d);
    }
}

/// 设置 Z 序（越大越靠上）。
#[allow(dead_code)]
pub fn set_z(id: usize, z: i32) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        let _d = irq_save();
        if C.windows[id].active && C.windows[id].z != z {
            invalidate(C.windows[id].visual_rect());
            C.windows[id].z = z;
            invalidate(C.windows[id].visual_rect());
        }
        irq_restore(_d);
    }
}

/// 设置窗口透明度（0..255）。
#[allow(dead_code)]
pub fn set_alpha(id: usize, a: u8) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        let _d = irq_save();
        if C.windows[id].active && C.windows[id].alpha != a {
            invalidate(C.windows[id].visual_rect());
            C.windows[id].alpha = a;
        }
        irq_restore(_d);
    }
}

/// 设置窗口缩放（定点：1.0 = 256，0 视为 256），以窗口中心为锚点。
pub fn set_scale(id: usize, scale: u32) {
    if id >= MAX_WINDOWS {
        return;
    }
    // 钳制缩放（1.0 = 256）：0 视为 1.0；上限 1024（4.0x）防止
    // visual_rect 的 w*scale 在极端值下回绕产生巨大可视矩形
    let s = if scale == 0 { 256 } else { scale.clamp(1, 1024) };
    unsafe {
        let _d = irq_save();
        if C.windows[id].active && C.windows[id].scale != s {
            invalidate(C.windows[id].visual_rect());
            C.windows[id].scale = s;
            invalidate(C.windows[id].visual_rect());
        }
        irq_restore(_d);
    }
}

/// 使窗口区域失效（UI 重绘后调用）。
#[allow(dead_code)]
pub fn invalidate_window(id: usize) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        if C.windows[id].active {
            invalidate(C.windows[id].visual_rect());
        }
    }
}

/// 使任意屏幕矩形失效。
#[allow(dead_code)]
pub fn invalidate_rect(r: Rect) {
    invalidate(r);
}

/// 使整屏失效。
pub fn invalidate_all() {
    unsafe {
        C.dirty_count = 1;
        C.dirty[0] = Rect::new(0, 0, C.width, C.height);
    }
}

/// 命中测试：返回 `(x, y)` 处最上层（z 最大）可见窗口的 id。
pub fn top_window_at(x: i32, y: i32) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_z = i32::MIN;
    unsafe {
        for i in 0..MAX_WINDOWS {
            let w = &C.windows[i];
            if w.active && w.alpha > 0 && w.visual_rect().contains(x, y) && w.z >= best_z {
                best = Some(i);
                best_z = w.z;
            }
        }
    }
    best
}

/// 把脏区域重合成到 `back` 缓冲（双缓冲直写，无 canvas 中间层）。
///
/// 返回是否实际合成了内容（供调用方决定是否 `present`）。
///
/// 快速路径（动画期间的主要开销优化）：
/// - `alpha == 0` 的窗口直接跳过；
/// - `alpha == 255 && scale == 256`（1:1 不透明）：行级 `copy_nonoverlapping`；
/// - `scale != 256`：最近邻采样改为**按子矩形预计算一次源 X 映射表**，
///   每行只做一次整数除法求 `sy`，不再每像素除法；
/// - `alpha in (0,255)`：保留逐像素 alpha 混合（仍复用 X 映射表）。
pub fn composite() -> bool {
    unsafe {
        if C.dirty_count == 0 || C.width == 0 {
            return false;
        }
        let width = C.width as usize;
        for i in 0..C.dirty_count {
            let r = C.dirty[i];
            // 裁剪到屏幕范围，避免窗口滑出屏幕后越界写
            let screen = Rect::new(0, 0, C.width, C.height);
            let Some(r) = r.intersect(&screen) else { continue };
            // 1. 填充背景（按行以 u64 双像素写，减少一半存储指令）
            let bg = Color::DARK.pixel();
            let bg64 = bg as u64 | ((bg as u64) << 32);
            let row_start = r.y as usize * width + r.x as usize;
            let row_w = r.w as usize;
            let mut off = row_start;
            let (q, rem) = (row_w / 2, row_w % 2);
            for _ in r.y..r.bottom() {
                let base = C.back.add(off);
                for k in 0..q {
                    *(base.add(k * 2) as *mut u64) = bg64;
                }
                if rem == 1 {
                    *base.add(row_w - 1) = bg;
                }
                off += width;
            }
            // 2. 按 z 升序混合与 r 相交的窗口
            // 收集活动窗口并按 z 排序（简单插入排序）
            let mut ids = [usize::MAX; MAX_WINDOWS];
            let mut n = 0;
            for wid in 0..MAX_WINDOWS {
                if C.windows[wid].active && C.windows[wid].visual_rect().intersects(&r) {
                    ids[n] = wid;
                    n += 1;
                }
            }
            for a in 1..n {
                let key = ids[a];
                let mut b = a;
                while b > 0 && C.windows[ids[b - 1]].z > C.windows[key].z {
                    ids[b] = ids[b - 1];
                    b -= 1;
                }
                ids[b] = key;
            }
            for k in 0..n {
                let w = &C.windows[ids[k]];
                if w.alpha == 0 {
                    continue; // 全透明窗口：跳过
                }
                let vr = w.visual_rect();
                let Some(sub) = r.intersect(&vr) else { continue };
                let sw = sub.w as usize;
                let scale = w.scale as i32;
                if w.alpha == 255 && scale == 256 {
                    // 1:1 不透明：整行拷贝（sub ⊆ vr == rect，源索引必然合法）
                    for y in sub.y..sub.bottom() {
                        let sy = (y - w.y) as usize;
                        let src = w
                            .surface
                            .pixels
                            .add(sy * w.w as usize + (sub.x - w.x) as usize);
                        let dst = C.back.add(y as usize * width + sub.x as usize);
                        copy_nonoverlapping(src, dst, sw);
                    }
                } else {
                    // scale != 256：预计算源 X 映射（每子矩形一次），
                    // 行内逐像素只做查表，无整数除法
                    let table_len = sw.min(SX_TABLE.len());
                    if scale == 256 {
                        for (xi, x) in (sub.x..sub.right()).enumerate() {
                            if xi >= table_len {
                                break;
                            }
                            SX_TABLE[xi] = (x - w.x) as u32;
                        }
                    } else {
                        for (xi, x) in (sub.x..sub.right()).enumerate() {
                            if xi >= table_len {
                                break;
                            }
                            SX_TABLE[xi] =
                                ((x - vr.x) * 256 + scale / 2).div_euclid(scale) as u32;
                        }
                    }
                    let alpha = w.alpha;
                    for y in sub.y..sub.bottom() {
                        let sy = if scale == 256 {
                            (y - w.y) as u32
                        } else {
                            ((y - vr.y) * 256 + scale / 2).div_euclid(scale) as u32
                        };
                        if sy >= w.h {
                            continue;
                        }
                        let dst_row = C.back.add(y as usize * width + sub.x as usize);
                        let src_row_base = w.surface.pixels.add(sy as usize * w.w as usize);
                        for xi in 0..sw.min(table_len) {
                            let sx = SX_TABLE[xi];
                            if sx >= w.w {
                                continue;
                            }
                            let src = *src_row_base.add(sx as usize);
                            let d = dst_row.add(xi);
                            *d = if alpha == 255 {
                                src
                            } else {
                                blend_pixel(*d, src, alpha)
                            };
                        }
                    }
                }
            }
        }
        C.dirty_count = 0;
        true
    }
}

/// 立即合成并呈现当前脏区域（低延迟输入路径）。
///
/// 供输入拖动等「无活动动画但有 UI 更新」的场景调用：否则窗口移动只标记
/// 脏区域，要等下一个动画 tick（8ms）才呈现，输入→画面延迟增加一整帧。
pub fn flush() {
    unsafe {
        if C.dirty_count != 0 && C.width != 0 {
            composite();
            present();
        }
    }
}

/// 呈现：把 `back` 翻转为扫描输出并交换 front/back（无整屏拷贝）。
///
/// 双缓冲语义：交换后 `back` 即上一帧完整正确的 `front`，下一帧合成只改脏区。
pub fn present() {
    unsafe {
        if C.width == 0 {
            return;
        }
        gpu_simple::present(C.back as usize);
        core::mem::swap(&mut C.front, &mut C.back);
    }
}
