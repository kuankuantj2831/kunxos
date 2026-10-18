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
    asm!("csrr {}, sstatus", out(reg) d);
    asm!("csrci sstatus, 2");
    d
}

unsafe fn irq_restore(saved: u64) {
    asm!("csrw sstatus, {}", in(reg) saved);
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
        unsafe {
            // 绘制侧与进程侧 destroy/resize（IRQ 屏蔽临界区）互斥：屏蔽 IRQ
            // 防止本循环被定时器抢占，避免绘制途中表面被并发 kfree。
            let _d = irq_save();
            let px = c.pixel();
            for i in 0..(self.w as usize * self.h as usize) {
                *self.pixels.add(i) = px;
            }
            irq_restore(_d);
        }
    }

    /// 矩形填充（越界部分裁剪）。
    pub fn fill_rect(&self, r: Rect, c: Color) {
        unsafe {
            let _d = irq_save();
            let px = c.pixel();
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    self.set(x, y, px);
                }
            }
            irq_restore(_d);
        }
    }

    /// 矩形描边（`t` 像素边框）。
    pub fn draw_rect(&self, r: Rect, c: Color, t: u32) {
        unsafe {
            let _d = irq_save();
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
            irq_restore(_d);
        }
    }

    /// 把 `src` 表面按 1:1 拷贝到 `(dx, dy)`（越界裁剪）。
    pub fn blit(&self, dx: i32, dy: i32, src: &Surface) {
        if src.pixels.is_null() || self.pixels.is_null() {
            return;
        }
        unsafe {
            // 屏蔽 IRQ：防止整行拷贝循环被定时器抢占，从而避免绘制途中
            // 目标表面被进程侧 destroy/resize 释放（与 mutator 临界区互斥）。
            let _d = irq_save();
            for y in 0..src.h as i32 {
                for x in 0..src.w as i32 {
                    let v = *src.pixels.add(y as usize * src.w as usize + x as usize);
                    self.set(dx + x, dy + y, v);
                }
            }
            irq_restore(_d);
        }
    }

    /// 带源行距的 blit：目标矩形只裁剪**拷贝宽高**，源按 `src_stride`
    /// 推进行距。此前 Image 控件把源宽钳到目标宽（src.w = min(iw, rect.w)），
    /// 行距随之改变 —— iw > rect.w 时每行内容整体错位。
    pub fn blit_strided(&self, dx: i32, dy: i32, src: *mut u32, src_stride: u32, cp_w: u32, cp_h: u32) {
        if src.is_null() || self.pixels.is_null() || src_stride == 0 {
            return;
        }
        unsafe {
            let _d = irq_save();
            for y in 0..cp_h as i32 {
                for x in 0..cp_w as i32 {
                    let v = *src.add(y as usize * src_stride as usize + x as usize);
                    self.set(dx + x, dy + y, v);
                }
            }
            irq_restore(_d);
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

/// 合成器：持久画布 + 双缓冲扫描输出。
///
/// - `canvas`：**持久化应用画布**（`/dev/fb0` 的 mmap 目标，固定物理页、
///   不随翻转旋转）。应用把整帧画进 canvas，合成器按**脏矩形**把变更部分
///   拷贝到 `back` —— 拷贝量 = 脏区，而非整屏。
/// - `front` / `back`：两块旋转的扫描输出缓冲。`composite` 把脏区从 canvas
///   拷入 `back` 并混合内核覆盖窗口，`present` 页翻转（front/back 交换）。
/// - 脏区语义正确性（无残影）：每次 present 拷贝的是「本帧应用脏区 ∪ 上一
///   帧应用脏区」的并集（`PREV_APP`），覆盖 back 缓冲自上次使用以来（2 帧）
///   的全部陈旧区域；canvas 持久、始终持有最新正确帧，拷出的像素必然正确。
/// - 初始化时三块缓冲都填充背景色，保证任意时刻任一缓冲都持有正确帧。
struct Compositor {
    width: u32,
    height: u32,
    front: *mut u32,
    back: *mut u32,
    /// 持久应用画布（fb0 mmap 目标；脏区拷贝的源）。
    canvas: *mut u32,
    windows: [Window; MAX_WINDOWS],
    dirty: [Rect; MAX_DIRTY],
    dirty_count: usize,
    /// 最近一次 `composite` 消费的脏矩形（供 `present` 按脏区 clean 缓存行）。
    last_dirty: [Rect; MAX_DIRTY],
    last_dirty_count: usize,
    /// 应用自上次 present 以来累计提交的脏矩形（本帧应用脏区）。
    app_dirty: [Rect; MAX_DIRTY],
    app_dirty_count: usize,
    /// 上次 present 的应用脏区（与 `app_dirty` 构成 2 帧拷贝并集）。
    prev_app: [Rect; MAX_DIRTY],
    prev_app_count: usize,
}

const EMPTY: Compositor = Compositor {
    width: 0,
    height: 0,
    front: null_mut(),
    back: null_mut(),
    canvas: null_mut(),
    windows: [const { Window::empty() }; MAX_WINDOWS],
    dirty: [Rect::empty(); MAX_DIRTY],
    dirty_count: 0,
    last_dirty: [Rect::empty(); MAX_DIRTY],
    last_dirty_count: 0,
    app_dirty: [Rect::empty(); MAX_DIRTY],
    app_dirty_count: 0,
    prev_app: [Rect::empty(); MAX_DIRTY],
    prev_app_count: 0,
};

static mut C: Compositor = EMPTY;

/// 用户态帧缓冲直绘是否已接管显示（launcher 等进程调用过 `fb_present`）。
///
/// 接管后显示节奏完全由应用驱动：定时器 IRQ 的 `anim::frame_tick` 只推进
/// 动画并标记脏区域（在应用下一次 present 时合成），**不再自行翻转**，避免
/// 在应用绘制中途翻转导致撕裂/半帧上屏。
static mut FB_ACTIVE: bool = false;

/// 进程侧 `fb_present` 正处于「像素搬运阶段」（IRQ 开放）的标志。
///
/// 该阶段 back 缓冲由进程独占写入；IRQ 侧（frame_tick / gesture flush）的
/// composite 必须让路（见 [`composite`]），否则会与像素阶段的 canvas→back
/// 拷贝并发写同一缓冲。置位/复位均在 fb_present 的关中断窗口内完成。
///
/// 记录属主 tid 而非裸 bool：像素阶段开中断运行，tick 兑现 kill_req 可把
/// 当前任务直接置 Exited 切走（多线程兄弟 exit / launcher kill 应用），
/// 死亡属主永远走不到提交阶段的复位 → BUSY 永久卡死、所有后续呈现弃帧、
/// composite 恒让路，整机画面冻结。属主已死时后来者复位接管（与 fs_lock
/// 死持有者自愈同模式）。
static mut BUSY_TID: u32 = 0;

/// 缩放合成用的源 X 坐标预计算表：`init` 时按屏幕宽度堆分配（每列一项）。
///
/// composite 的子矩形恒先与屏幕求交（sub ⊆ screen），列数 ≤ 屏幕宽，因此
/// 表长 = 屏幕宽即可覆盖全部列。旧实现的 1024 固定上限在宽屏（>1024 列，
/// 如未来 1080p 横屏 / 外接显示）下会静默截断窗口右缘（`sw.min(table_len)`
/// 之外的列不绘制）。分配失败（表长 0）时回退为行内逐像素计算（慢但正确）。
static mut SX_TABLE: *mut u32 = null_mut();
static mut SX_TABLE_LEN: usize = 0;

// ==================== 脏区域 ====================

fn invalidate(r: Rect) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    unsafe {
        // 脏列表会被 IRQ 侧 composite() 读取：进程上下文写入前必须屏蔽 IRQ，
        // 否则可能在 IRQ 合成中途被改写（撕裂/漏合成）。此处可被已屏蔽的
        // move/resize 等临界区嵌套调用，irq_save/restore 保存-恢复原状态，安全。
        let _d = irq_save();
        // 与已有脏区域合并（重叠/相邻 → 取包围盒）
        for i in 0..C.dirty_count {
            let d = C.dirty[i];
            if d.intersects(&r) || (d.right() == r.x && d.y == r.y) {
                let x = d.x.min(r.x);
                let y = d.y.min(r.y);
                let rr = d.right().max(r.right());
                let bb = d.bottom().max(r.bottom());
                C.dirty[i] = Rect::new(x, y, (rr - x) as u32, (bb - y) as u32);
                irq_restore(_d);
                return;
            }
        }
        // 超出容量则退化为整屏刷新
        if C.dirty_count >= MAX_DIRTY {
            C.dirty_count = 1;
            C.dirty[0] = Rect::new(0, 0, C.width, C.height);
            irq_restore(_d);
            return;
        }
        C.dirty[C.dirty_count] = r;
        C.dirty_count += 1;
        irq_restore(_d);
    }
}

// ==================== alpha 混合 ====================

/// `v / 255` 的位运算精确等价（`0 ≤ v ≤ 65535` 恒等于地板除，Hacker's Delight）。
/// 混合热路径（页面淡入淡出 / 覆盖层动画逐像素调用）每像素省 3 次整数除法。
#[inline]
fn div255(v: u32) -> u32 {
    (v + 1 + (v >> 8)) >> 8
}

#[inline]
fn blend_pixel(dst: u32, src: u32, alpha: u8) -> u32 {
    if alpha == 255 {
        return src;
    }
    if alpha == 0 {
        return dst;
    }
    let a = alpha as u32;
    let inv = 255 - a;
    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;
    let r = div255(sr * a + dr * inv);
    let g = div255(sg * a + dg * inv);
    let b = div255(sb * a + db * inv);
    (r << 16) | (g << 8) | b
}

// ==================== 公共接口 ====================

/// 初始化合成器：分配持久画布 + 双扫描缓冲并填充初始背景。
///
/// `adopt_front` 为启动阶段 `/dev/fb0` 的显存物理地址（bootloader 启动画面或
/// 内核 `fb_init` 分配的缓冲，当前正被扫描输出）；合成器把它作为 `front`
/// （保持画面不中断），另分配 `back`（翻转目标）与 `canvas`（应用直绘画布）。
/// `/dev/fb0` 的 mmap 目标固定为 **canvas**（`fb_target`）：应用直接写入
/// canvas，`fb_present` 把脏区拷贝到 back、混合覆盖窗口后页翻转 ——
/// 应用直绘零整屏拷贝接入双缓冲翻转。
///
/// 初始化时三块缓冲都填背景色，保证任意时刻任一缓冲都持有正确帧。
pub fn init(adopt_front: usize) {
    let w = gpu_simple::width();
    let h = gpu_simple::height();
    if w == 0 || h == 0 {
        return;
    }
    let front = if adopt_front != 0 {
        adopt_front
    } else {
        gpu_simple::alloc_fb()
    };
    let back = gpu_simple::alloc_fb();
    let canvas = gpu_simple::alloc_fb();
    // 缩放查表按屏宽分配（见 SX_TABLE 注释）；失败保留空表 → 逐像素回退
    let cols = w as usize;
    let tbl = heap::kmalloc(cols * 4) as *mut u32;
    unsafe {
        SX_TABLE = tbl;
        SX_TABLE_LEN = if tbl.is_null() { 0 } else { cols };
    }
    // 一次性观测行：确认表长跟随分辨率（旧实现固定 1024）
    crate::kinfo! {
        if tbl.is_null() {
            crate::uart_send_str("[compositor] 缩放查表分配失败（回退逐像素）\n");
        } else {
            crate::uart_send_str("[compositor] 缩放查表 ");
            crate::print_dec(cols as u64);
            crate::uart_send_str(" 列（=屏宽）\n");
        }
    }
    unsafe {
        C.width = w;
        C.height = h;
        C.front = front as *mut u32;
        C.back = back as *mut u32;
        C.canvas = canvas as *mut u32;
        let n = (w as usize) * (h as usize);
        for i in 0..n {
            *C.front.add(i) = Color::DARK.pixel();
            *C.back.add(i) = Color::DARK.pixel();
            *C.canvas.add(i) = Color::DARK.pixel();
        }
    }
    // 三块缓冲均已正确：无脏区域待合成
    unsafe {
        C.dirty_count = 0;
        C.last_dirty_count = 0;
        C.app_dirty_count = 0;
        C.prev_app_count = 0;
    }
    // 扫描输出切到 front（adopt 的缓冲本就是扫描输出，重复写入幂等）
    gpu_simple::present(front);
    // fb0 mmap 目标登记为 canvas（持久，不随翻转旋转）
    crate::devfs::fb_set_target(canvas);
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

// ==================== 应用直绘接入（/dev/fb0 → 脏区域合成器） ====================
//
// 迁移路径：launcher 等应用不再「整屏 memcpy 到单缓冲」，而是
//   1. 直接写入 `/dev/fb0` 映射（= **持久 canvas**，`fb_target`，固定物理页）；
//   2. `FBIO_PUT_DAMAGE` 提交变更矩形（合并进 `app_dirty`）；
//   3. `FBIO_PRESENT` 触发 `fb_present`：把「本帧 ∪ 上一帧」应用脏区从
//      canvas 拷贝到 back（脏区拷贝，非整屏），混合内核覆盖窗口（Home
//      Indicator 等），页翻转（消除撕裂）。

/// 持久应用画布物理地址 —— `/dev/fb0` mmap 的目标（固定，不随翻转旋转）。
pub fn fb_target() -> usize {
    unsafe { C.canvas as usize }
}

/// 当前 front 缓冲物理地址（正在扫描输出的缓冲）。
#[allow(dead_code)]
pub fn front_buffer() -> usize {
    unsafe { C.front as usize }
}

/// 待翻转的 back 缓冲物理地址（合成器自绘/overlay 的正确绘制目标：
/// 画到 back 才会随 present 上屏；画到 canvas 会被应用下一帧覆盖）。
#[allow(dead_code)]
pub fn back_buffer() -> usize {
    unsafe { C.back as usize }
}

/// 应用直绘是否已接管显示（首帧 `fb_present` 后为 true）。
pub fn fb_active() -> bool {
    unsafe { FB_ACTIVE }
}

/// 整屏矩形（应用整帧重绘时作为单个脏矩形提交）。
pub fn fb_full_rect() -> Rect {
    unsafe { Rect::new(0, 0, C.width, C.height) }
}

/// 把矩形并入矩形列表（重叠/相邻合并；超限退化为整屏矩形）。
fn merge_rect(list: &mut [Rect; MAX_DIRTY], count: &mut usize, r: Rect) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    for i in 0..*count {
        let d = list[i];
        if d.intersects(&r) || (d.right() == r.x && d.y == r.y) {
            let x = d.x.min(r.x);
            let y = d.y.min(r.y);
            let rr = d.right().max(r.right());
            let bb = d.bottom().max(r.bottom());
            list[i] = Rect::new(x, y, (rr - x) as u32, (bb - y) as u32);
            return;
        }
    }
    if *count >= MAX_DIRTY {
        *count = 1;
        list[0] = fb_full_rect();
        return;
    }
    list[*count] = r;
    *count += 1;
}

/// 合并应用提交的脏矩形（`FBIO_PUT_DAMAGE`）。
///
/// 只登记应用画布上的变更区域，不触发呈现；随下一次 `fb_present` 一并拷贝。
pub fn fb_damage(rects: &[Rect]) {
    unsafe {
        if C.width == 0 {
            return;
        }
        // app_dirty 由进程上下文写入、随 fb_present 被 IRQ 侧合成消费：
        // 写入前屏蔽 IRQ，避免与 composite() 的脏区读取交错。
        let _d = irq_save();
        // 与 ioctl 侧一致：恰好 MAX_DIRTY 个矩形是合法提交（`>` 而非 `>=`），
        // 不必降级整屏。
        if rects.len() > MAX_DIRTY {
            C.app_dirty_count = 1;
            C.app_dirty[0] = fb_full_rect();
            irq_restore(_d);
            return;
        }
        for r in rects {
            merge_rect(&mut C.app_dirty, &mut C.app_dirty_count, *r);
        }
        irq_restore(_d);
    }
}

/// [perf] 最近一次 fb_present 各阶段耗时（µs）：元数据+stage2 拷贝 /
/// 阶段3 补拷+覆盖窗口合成 / present（overlay+缓存行 clean+页翻转）。
/// anim::perf_test 读取做耗时分解，定位与脏区面积无关的整屏级成本。
pub static mut STAGE_US: (u64, u64, u64) = (0, 0, 0);

/// 读取最近一次 fb_present 的阶段耗时分解（µs）。
pub fn last_stage_us() -> (u64, u64, u64) {
    unsafe { STAGE_US }
}

/// 应用请求呈现（`FBIO_PRESENT`）：脏区拷贝 + 覆盖窗口合成 + 页翻转。
///
/// 三阶段结构（关中断窗口只覆盖元数据与提交，像素搬运在开放 IRQ 下进行）：
///   1. **元数据阶段**（IRQ 关）：合并本帧应用脏区、快照拷贝区/合成区、
///      置 BUSY。此前本函数全程屏蔽 IRQ —— 整屏 2MB 级 canvas→back 拷贝 +
///      合成 + 翻转期间触摸 IRQ（gesture flush）、调度 tick 全部被拖延，
///      输入→画面延迟与调度抖动直接叠加每帧呈现耗时。
///   2. **像素阶段**（恢复调用方 IRQ 状态）：canvas→back 逐矩形拷贝。只读
///      canvas 与快照，不触及共享元数据；IRQ 侧 composite 见 BUSY 让路
///      （见 [`composite`]），本阶段对 back 的写独占。
///   3. **提交阶段**（IRQ 关）：阶段 2 期间 IRQ 侧新增的失效区先补 canvas
///      拷贝（canvas 是权威底图）再并入合成区；覆盖窗口合成、记录
///      prev_app、页翻转、清 BUSY。
///
/// 首次调用后进入应用驱动模式（`FB_ACTIVE = true`），定时器 IRQ 不再自行翻转。
pub fn fb_present(rects: &[Rect]) -> bool {
    unsafe {
        if C.width == 0 {
            return false;
        }
        // [perf] 应用直绘路径合成+呈现耗时度量（perf::note_app_render）
        let t0 = crate::timer::now_us();
        // ---- 阶段 1：元数据（IRQ 关）----
        // C.dirty/prev_app/app_dirty 与 IRQ 侧 composite()/invalidate() 的
        // 交错会消费「旧数组 + 新 count」的撕裂组合并把 dirty_count 清 0
        //（本帧覆盖层丢失/残影）。BUSY 的查检与置位也在本窗口：像素阶段
        // 允许定时器抢占调度，另一任务重入 fb_present 时见 BUSY 直接放弃
        // 本帧（其脏区留在 app_dirty，无丢失，下一次呈现消费）。
        let saved = irq_save();
        if BUSY_TID != 0 {
            // 属主已死（像素阶段被 kill）：复位接管，否则 BUSY 永久卡死。
            if crate::sched_cfs::tid_alive(BUSY_TID) {
                irq_restore(saved);
                return false;
            }
            BUSY_TID = 0;
        }
        FB_ACTIVE = true;
        fb_damage(rects);
        // 无应用脏区、无内核失效、无上一帧残留 → 无需呈现
        if C.app_dirty_count == 0 && C.dirty_count == 0 && C.prev_app_count == 0 {
            irq_restore(saved);
            return false;
        }
        // 拷贝区 = prev_app ∪ app_dirty ∪ 内核失效区（2 帧并集 + 内核动画）。
        // canvas 是应用内容的权威底图：内核失效区（窗口移动/透明度/缩放）
        // 若不先从 canvas 回填，composite 只把窗口像素混合到上一帧的合成
        // 结果上 —— alpha 窗口每 2 帧在同一 back 上重复混合（透明度逐帧
        // 加深），窗口移走后旧位置残留到应用恰好重绘该区域为止。
        let mut copy: [Rect; MAX_DIRTY] = [Rect::empty(); MAX_DIRTY];
        let mut copy_count = 0usize;
        // 本帧新变更集（应用脏区 ∪ 本帧内核失效区）：下一帧的 prev_app 取
        // 此快照而非历史拷贝并集——累积并集只增不减，一次整屏事件（窗口
        // 销毁/页面切换）后每帧都被锁死为整屏拷贝（perftest 实测局部脏区
        // 与整屏 meta+copy 同为 ~827µs 的根因）。
        let mut newd: [Rect; MAX_DIRTY] = [Rect::empty(); MAX_DIRTY];
        let mut newd_count = 0usize;
        for i in 0..C.prev_app_count {
            merge_rect(&mut copy, &mut copy_count, C.prev_app[i]);
        }
        for i in 0..C.app_dirty_count {
            merge_rect(&mut copy, &mut copy_count, C.app_dirty[i]);
            merge_rect(&mut newd, &mut newd_count, C.app_dirty[i]);
        }
        for i in 0..C.dirty_count {
            merge_rect(&mut copy, &mut copy_count, C.dirty[i]);
            merge_rect(&mut newd, &mut newd_count, C.dirty[i]);
        }
        // 合成区 = 拷贝区 ∪ 内核失效区（窗口动画等）
        let mut comp: [Rect; MAX_DIRTY] = [Rect::empty(); MAX_DIRTY];
        let mut comp_count = 0usize;
        for i in 0..copy_count {
            merge_rect(&mut comp, &mut comp_count, copy[i]);
        }
        for i in 0..C.dirty_count {
            merge_rect(&mut comp, &mut comp_count, C.dirty[i]);
        }
        // 本帧应用脏区已并入拷贝区快照，此处清零（语义同旧实现：由 prev_app
        // 记录拷贝区）。C.dirty 保留：阶段 2 的 IRQ 侧失效继续累加，阶段 3
        // 一并消费。
        C.app_dirty_count = 0;
        BUSY_TID = crate::sched_cfs::current_tid();
        // 像素阶段的稳定参数快照（present 前 back 不会交换：IRQ 侧被 BUSY
        // 挡住，进程侧重入被 BUSY 拒绝）
        let cw = C.width as usize;
        let canvas = C.canvas;
        let back = C.back;
        irq_restore(saved);

        // ---- 阶段 2：像素搬运（IRQ 状态 = 调用方进入时状态；IRQ 上下文
        // 调用时 DAIF.I 本就置位，irq_restore 恢复原状，不会中途开中断）----
        for i in 0..copy_count {
            copy_rect_pixels(canvas, back, copy[i], cw);
        }

        // ---- 阶段 3：提交（IRQ 关）----
        let t3 = crate::timer::now_us();
        let saved = irq_save();
        // 阶段 2 期间 IRQ 侧（gesture flush 的窗口移动 / frame_tick 动画）
        // 新增的失效区：先补 canvas→back 拷贝，再并入拷贝区/合成区。
        for i in 0..C.dirty_count {
            copy_rect_pixels(canvas, back, C.dirty[i], cw);
            merge_rect(&mut copy, &mut copy_count, C.dirty[i]);
            merge_rect(&mut newd, &mut newd_count, C.dirty[i]);
        }
        for i in 0..copy_count {
            merge_rect(&mut comp, &mut comp_count, copy[i]);
        }
        // 覆盖窗口合成（composite 消费 comp 作为脏区）
        C.dirty_count = comp_count;
        C.dirty[..comp_count].copy_from_slice(&comp[..comp_count]);
        composite_locked();
        // ★ prev_app 只取「本帧新变更」（上面维护的 newd = 应用脏区 ∪ 本帧
        // 内核失效区），维持 2 帧并集语义：present() 交换后下一次呈现写入的
        // back 是 2 帧前的旧缓冲，dirty_{N-1} ∪ dirty_N 恰好补齐这两帧间的
        // 全部变化；更早的变更已存在于该缓冲（清空 prev_app 才会全屏闪烁/
        // 光标残影）。累积历史并集则会把一次整屏事件放大为永久整屏拷贝。
        C.prev_app_count = newd_count;
        C.prev_app[..newd_count].copy_from_slice(&newd[..newd_count]);
        let t4 = crate::timer::now_us();
        present();
        let t5 = crate::timer::now_us();
        STAGE_US = (t3 - t0, t4 - t3, t5 - t4);
        BUSY_TID = 0;
        // [perf] 总耗时 + 关中断临界区耗时（元数据/提交阶段不含像素搬运，
        // 即 IRQ 被屏蔽的实际时长）
        let now = crate::timer::now_us();
        crate::perf::note_app_render(now.saturating_sub(t0), now.saturating_sub(t3));
        irq_restore(saved);
        true
    }
}

/// 把 `src` 缓冲的 `r` 矩形像素逐行拷贝到 `dst`（行内整行 memcpy，越界裁剪）。
fn copy_rect_pixels(src: *mut u32, dst: *mut u32, r: Rect, width: usize) {
    unsafe {
        let sw = C.width as i32;
        let sh = C.height as i32;
        let x0 = r.x.max(0);
        let y0 = r.y.max(0);
        let x1 = r.right().min(sw);
        let y1 = r.bottom().min(sh);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let row_w = (x1 - x0) as usize;
        for y in y0..y1 {
            copy_nonoverlapping(
                src.add(y as usize * width + x0 as usize),
                dst.add(y as usize * width + x0 as usize),
                row_w,
            );
        }
    }
}

/// [perf] 行拷贝对齐基线（anim::perf_test 场景 D）：同一矩形在指定 x0 下
/// canvas→back 逐行拷贝 `iters` 次的平均单次耗时（µs）。奇 x0 的行首为
/// ≡4 (mod 8) 形态：用于监控 memcpy 前置 4 字节对齐宽通道是否生效——生效时
/// 奇 x0 ≈ 偶 x0；退化字节循环时奇 x0 耗时数倍于偶 x0。
pub fn bench_row_copy(x0: i32, w: u32, h: u32, iters: u32) -> u64 {
    unsafe {
        if C.width == 0 {
            return 0;
        }
        let r = Rect::new(x0, 100, w, h);
        let t0 = crate::timer::now_us();
        for _ in 0..iters {
            copy_rect_pixels(C.canvas, C.back, r, C.width as usize);
        }
        let t1 = crate::timer::now_us();
        (t1 - t0) / iters as u64
    }
}

/// [perf] 重叠拷贝基线（anim::perf_test 场景 E）：同缓冲内把 320×h 块
/// 下移 16 字节（dst>src 全程重叠 → memmove 反向路径，滚动/平移形态）
/// 的平均单次耗时（µs）。返回 u64::MAX 表示正确性抽查失败（反向宽通道
/// 错序的信号哨兵）；抽查为「单次搬移后逐字核对序号模式」。
pub fn bench_overlap_copy(h: u32, iters: u32) -> u64 {
    unsafe {
        if C.width == 0 {
            return 0;
        }
        let words = 320usize * h as usize; // u32 字数（×4 = 字节）
        let scratch = heap::kmalloc((words + 4) * 4) as *mut u32;
        if scratch.is_null() {
            return 0;
        }
        let t0 = crate::timer::now_us();
        for _ in 0..iters {
            core::ptr::copy(scratch, scratch.add(4), words);
        }
        let t1 = crate::timer::now_us();
        // 正确性抽查：重填序号 → 单次下移 16B → dst[i] 应等于原 src[i]
        for i in 0..(words + 4) {
            *scratch.add(i) = i as u32;
        }
        core::ptr::copy(scratch, scratch.add(4), words);
        let mut ok = true;
        for i in 0..words {
            if *scratch.add(i + 4) != i as u32 {
                ok = false;
                break;
            }
        }
        heap::kfree(scratch as *mut u8);
        if !ok {
            return u64::MAX;
        }
        (t1 - t0) / iters as u64
    }
}

/// [perf] 跨奇偶行拷贝基线（anim::perf_test 场景 D2）：canvas(x=xs) →
/// back(x=xd) 的 320×h 块搬移，xs/xd 奇偶不同 → 混合对齐形态（一端
/// ≡0、一端 ≡4 mod 8）。监控 memcpy 的 u32 字循环分支。
pub fn bench_row_copy_cross(xs: i32, xd: i32, w: u32, h: u32, iters: u32) -> u64 {
    unsafe {
        if C.width == 0 {
            return 0;
        }
        let cw = C.width as usize;
        let row = w as usize;
        let t0 = crate::timer::now_us();
        for _ in 0..iters {
            for y in 0..h as usize {
                core::ptr::copy_nonoverlapping(
                    C.canvas.add(y * cw + xs as usize),
                    C.back.add(y * cw + xd as usize),
                    row,
                );
            }
        }
        let t1 = crate::timer::now_us();
        (t1 - t0) / iters as u64
    }
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
        // 窗口表/脏列表与 IRQ 侧 composite() 共享：读取窗口状态并登记脏区
        // 须屏蔽 IRQ（invalidate 内部已屏蔽，这里先屏蔽外层窗口表读取）。
        let _d = irq_save();
        if C.windows[id].active {
            invalidate(C.windows[id].visual_rect());
        }
        irq_restore(_d);
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
        let _d = irq_save();
        C.dirty_count = 1;
        C.dirty[0] = Rect::new(0, 0, C.width, C.height);
        irq_restore(_d);
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

/// 返回 z 序在 `(above_z, below_z)` 区间内最上层（z 最大）的可见窗口
/// （alpha > 0，供手势系统取当前应用）。
pub fn top_visible_window_between(above_z: i32, below_z: i32) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_z = above_z;
    unsafe {
        for i in 0..MAX_WINDOWS {
            let w = &C.windows[i];
            if w.active && w.alpha > 0 && w.z > best_z && w.z < below_z {
                best = Some(i);
                best_z = w.z;
            }
        }
    }
    best
}

/// 窗口 z 序（未激活返回 `None`）。
pub fn window_z(id: usize) -> Option<i32> {
    if id >= MAX_WINDOWS {
        return None;
    }
    unsafe {
        if C.windows[id].active {
            Some(C.windows[id].z)
        } else {
            None
        }
    }
}

/// 目标列 `x` 的源列坐标（最近邻采样：四舍五入 + 钳制到源宽内）。
///
/// 查表构建与「查表不可用」（`SX_TABLE` 分配失败）时的行内回退共用此式。
/// 钳制防取整落到 `src_w`（首行越界 1 像素）；i64 中间量：极端窗口几何
/// （16M 宽表面 × scale 1024）下 `(x - vr.x) * 256` 的 i32 乘法回绕。
#[inline]
fn src_x(x: i32, win_x: i32, vr_x: i32, scale: i32, src_w: u32) -> u32 {
    if scale == 256 {
        (x - win_x) as u32
    } else {
        ((((x - vr_x) as i64) * 256 + (scale as i64) / 2).div_euclid(scale as i64) as u32)
            .min(src_w - 1)
    }
}

/// 把脏区域内的内核覆盖窗口（Home Indicator / 变暗层等）合成到 `back`。
///
/// 返回是否实际合成了内容（供调用方决定是否 `present`）。
///
/// 语义（应用直绘接入后）：`back` 的**基底层是应用内容**（launcher 等经
/// `/dev/fb0` 映射直写），合成器**不再填充背景**（那会抹掉应用像素），只把
/// 与脏区相交的窗口按 z 序混合到既有内容之上。非脏区天然保持上一帧内容 ——
/// 双缓冲 + 脏区语义与旧 canvas 方案一致，但省掉了每帧整屏 canvas→back
/// 拷贝与整屏背景填充。
///
/// 快速路径（动画期间的主要开销优化）：
/// - `alpha == 0` 的窗口直接跳过；
/// - `alpha == 255 && scale == 256`（1:1 不透明）：行级 `copy_nonoverlapping`；
/// - `scale != 256`：最近邻采样按子矩形预计算一次源 X 映射表（表长 = 屏宽，
///   恒覆盖子矩形全部列；表不可用时回退行内计算），每行只做一次整数除法求
///   `sy`，不再每像素除法；
/// - `alpha in (0,255)`：保留逐像素 alpha 混合（仍复用 X 映射表）。
///
/// IRQ 上下文调用时若进程侧 `fb_present` 处于像素搬运阶段（BUSY），直接
/// 让路返回 false —— 脏区不丢（留在 `C.dirty`），由 fb_present 提交阶段
/// 或下一次呈现消费；避免与像素阶段并发写 back。
pub fn composite() -> bool {
    unsafe {
        if BUSY_TID != 0 && crate::sched_cfs::tid_alive(BUSY_TID) {
            return false;
        }
        // 属主已死（像素阶段被 kill 中断）：视为空闲放行合成——back 可能
        // 停在半更新状态，但 canvas 是权威底图，下一次 fb_present 会从
        // canvas 回填补齐；卡死让路才会让整机画面永久停更。
        composite_locked()
    }
}

/// [`composite`] 的主体（调用方保证不与 fb_present 像素阶段并发写 back）。
fn composite_locked() -> bool {
    unsafe {
        if C.dirty_count == 0 || C.width == 0 {
            return false;
        }
        // 合成期间屏蔽 IRQ：保持与进程侧 destroy/resize 等临界区互斥
        //（IRQ 上下文调用时 DAIF.I 本就置位，保存/恢复为无操作）。
        let _d = irq_save();
        let width = C.width as usize;
        // 记录本次消费的脏矩形（供 present 按脏区 clean 缓存行）
        C.last_dirty_count = C.dirty_count;
        C.last_dirty[..C.dirty_count].copy_from_slice(&C.dirty[..C.dirty_count]);
        let mut any = false;
        for i in 0..C.dirty_count {
            let r = C.dirty[i];
            // 裁剪到屏幕范围，避免窗口滑出屏幕后越界写
            let screen = Rect::new(0, 0, C.width, C.height);
            let Some(r) = r.intersect(&screen) else { continue };
            // 按 z 升序混合与 r 相交的窗口
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
                // 绘制前复核窗口仍在注册表且表面指针有效：destroy/resize 是
                // 先置空 surface 再 kfree，这里见到 null 说明已被并发销毁，
                // 跳过该窗口，避免解引用已释放内存（最小安全修复，H-12）。
                if !w.active || w.surface.pixels.is_null() || w.w == 0 || w.h == 0 {
                    continue;
                }
                any = true;
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
                    // scale != 256 或半透明：源 X 映射优先走预计算查表（每子
                    // 矩形构建一次，行内逐像素查表无除法）。表长 = 屏宽且
                    // sub ⊆ screen，列数必然被覆盖；表未分配（init 分配失败，
                    // 防御路径）时回退行内逐像素计算。
                    let use_table = sw <= SX_TABLE_LEN;
                    if use_table {
                        for (xi, x) in (sub.x..sub.right()).enumerate() {
                            *SX_TABLE.add(xi) = src_x(x, w.x, vr.x, scale, w.w);
                        }
                    }
                    let alpha = w.alpha;
                    for y in sub.y..sub.bottom() {
                        let sy = if scale == 256 {
                            (y - w.y) as u32
                        } else {
                            ((((y - vr.y) as i64) * 256 + (scale as i64) / 2)
                                .div_euclid(scale as i64)) as u32
                        };
                        if sy >= w.h {
                            continue;
                        }
                        let dst_row = C.back.add(y as usize * width + sub.x as usize);
                        let src_row_base = w.surface.pixels.add(sy as usize * w.w as usize);
                        if use_table {
                            for xi in 0..sw {
                                // sx 已在查表构建时钳制到 w.w-1，行内免边界检查
                                let sx = *SX_TABLE.add(xi);
                                let src = *src_row_base.add(sx as usize);
                                let d = dst_row.add(xi);
                                *d = if alpha == 255 {
                                    src
                                } else {
                                    blend_pixel(*d, src, alpha)
                                };
                            }
                        } else {
                            for (xi, x) in (sub.x..sub.right()).enumerate() {
                                let sx = src_x(x, w.x, vr.x, scale, w.w);
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
        }
        C.dirty_count = 0;
        irq_restore(_d);
        any
    }
}

/// 立即合成并呈现当前脏区域（低延迟输入路径）。
///
/// 供输入拖动等「无活动动画但有 UI 更新」的场景调用：否则窗口移动只标记
/// 脏区域，要等下一个动画 tick（8.33ms）才呈现，输入→画面延迟增加一整帧。
///
/// 应用直绘接管后（`fb_active`）**不再翻转**：显示节奏由应用的 `fb_present`
/// 决定（应用在帧边界提交，保证翻转不落在绘制中途）；本函数只把内核窗口
/// 的脏区合成进 back，等应用下一次 present 一并上屏。
pub fn flush() {
    unsafe {
        if C.dirty_count != 0 && C.width != 0 {
            composite();
            if !FB_ACTIVE {
                present();
            }
        }
    }
}

/// 把 canvas 的 [x,y,w,h] 区域恢复到 back（越界裁剪）。
///
/// 供直接画在 back 上的叠加层（perf overlay）先取干净底图：back 每 2 帧
/// 轮回复用，不恢复会把半透明叠加逐帧累积到底色全黑。
pub fn restore_back_from_canvas(x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        if C.width == 0 {
            return;
        }
        if w <= 0 || h <= 0 {
            return;
        }
        let r = Rect::new(x, y, w as u32, h as u32);
        copy_rect_pixels(C.canvas, C.back, r, C.width as usize);
    }
}

/// 呈现：把 `back` 翻转为扫描输出并交换 front/back（无整屏拷贝）。
///
/// 双缓冲语义：交换后 `back` 即上一帧完整正确的 `front`，下一帧合成只改脏区。
/// 翻转前按**脏矩形** clean 对应缓存行（整屏脏区时退化为整屏 clean）。
pub fn present() {
    unsafe {
        if C.width == 0 {
            return;
        }
        // [perf] 性能 overlay：在待翻转的 back 上绘制（随每帧上屏），并计数
        crate::perf::draw_overlay();
        // overlay 直写 back 但不在合成脏区集内：把其矩形并入 clean 集，
        // 否则真机上 overlay 像素滞留缓存不上屏（QEMU 无缓存一致性影响）。
        let overlay_clean = unsafe { crate::perf::OVERLAY_RECT }
            .map(|(x, y, w, h)| Rect::new(x, y, w, h));
        if let Some(r) = overlay_clean {
            let mut rects: [Rect; MAX_DIRTY] = [Rect::empty(); MAX_DIRTY];
            let cnt = C.last_dirty_count.min(MAX_DIRTY - 1);
            rects[..cnt].copy_from_slice(&C.last_dirty[..cnt]);
            let mut rc = cnt;
            merge_rect(&mut rects, &mut rc, r);
            gpu_simple::present_rects(C.back as usize, &rects[..rc]);
        } else {
            gpu_simple::present_rects(
                C.back as usize,
                &C.last_dirty[..C.last_dirty_count],
            );
        }
        core::mem::swap(&mut C.front, &mut C.back);
        crate::perf::note_frame();
    }
}

// ==================== 页面窗口（OOBE 等向导型 UI） ====================

/// 页面窗口最大宽度（px）。
pub const MAX_PAGE_W: u32 = 800;

/// 页面窗口宽度上限：屏幕宽度 * 9 / 10。
const PAGE_W_FRAC_NUM: u32 = 9;
const PAGE_W_FRAC_DEN: u32 = 10;

/// 页面窗口尺寸：宽度 = min(内容宽度, 屏幕宽度 * 0.9, 800px)，高度自适应内容。
pub fn page_window_size(content_w: u32, content_h: u32) -> (u32, u32) {
    let cap = (width() as u64 * PAGE_W_FRAC_NUM as u64 / PAGE_W_FRAC_DEN as u64)
        .min(MAX_PAGE_W as u64) as u32;
    (content_w.min(cap).max(1), content_h.max(1))
}

/// 默认窗口位置：x = (屏幕宽度 - 窗口宽度) / 2，y = (屏幕高度 - 窗口高度) / 2。
pub fn default_position(w: u32, h: u32) -> (i32, i32) {
    (
        (width() as i32 - w as i32) / 2,
        (height() as i32 - h as i32) / 2,
    )
}

/// 边界检查：窗口若部分或全部超出屏幕，强制拉回屏幕内。
pub fn clamp_position(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    let sw = width() as i32;
    let sh = height() as i32;
    let w = w.min(sw.max(0) as u32) as i32;
    let h = h.min(sh.max(0) as u32) as i32;
    let x = x.clamp(0, (sw - w).max(0));
    let y = y.clamp(0, (sh - h).max(0));
    (x, y)
}

/// 创建页面窗口：默认居中位置（含边界检查）、alpha 255、缩放 identity。
pub fn create_page_window(content_w: u32, content_h: u32, z: i32) -> usize {
    let (w, h) = page_window_size(content_w, content_h);
    let (x, y) = default_position(w, h);
    let (x, y) = clamp_position(x, y, w, h);
    create_window(x, y, w, h, z, 255)
}

/// 强制重置窗口 transform 为 identity：位置回到默认居中（含边界检查）、
/// 缩放 1.0（scale == 256）、透明度 255。
pub fn reset_window_transform(id: usize) {
    if id >= MAX_WINDOWS {
        return;
    }
    unsafe {
        let _d = irq_save();
        if !C.windows[id].active {
            irq_restore(_d);
            return;
        }
        let old = C.windows[id].visual_rect();
        let (w, h) = (C.windows[id].w, C.windows[id].h);
        let (dx, dy) = default_position(w, h);
        let (dx, dy) = clamp_position(dx, dy, w, h);
        C.windows[id].x = dx;
        C.windows[id].y = dy;
        C.windows[id].scale = 256;
        C.windows[id].alpha = 255;
        invalidate(old);
        invalidate(C.windows[id].visual_rect());
        irq_restore(_d);
    }
}
