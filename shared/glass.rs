//! KunxOS 毛玻璃 / 液态玻璃绘制核心（no_std、无堆、固定内存）。
//!
//! 算法：对面板下方的背景做 4x 降采样（2x2 对角采样平均）→ 小缓冲 3x3 盒式
//! 模糊 → 双线性放大回原尺寸；期间叠加半透明着色（深色玻璃 / 浅色玻璃 /
//! 自定义着色）、顶部镜面高光、左上→右下斜向液态光泽与 1px 描边，并裁剪圆角。
//! 星光磨砂材质使用冷色着色与柔和高光，用于导航栏、标题栏、面板、
//! 卡片、按钮、键盘等一切界面元素。
//!
//! 使用方式：实现 [`GlassTarget`]（背景读取 + 像素写入），调用 [`glass_rect`] 或
//! [`glass_rect_style`]。本模块被 `nav.rs` 及各应用经 `#[path]` 共享，每进程只有
//! 一份降采样缓冲，无堆分配。

#[path = "starlight.rs"]
pub mod starlight;

/// 降采样缓冲宽度（540 / 4 上取整）。
pub const GLASS_DOWN_W: usize = 135;
/// 降采样缓冲高度（960 / 4 上取整）。
pub const GLASS_DOWN_H: usize = 240;
/// 4x 降采样缓冲（进程内共享；应用均为单线程，无并发问题）。
static mut GLASS_DOWN: [u32; GLASS_DOWN_W * GLASS_DOWN_H] = [0; GLASS_DOWN_W * GLASS_DOWN_H];

// ==================== 光随指动（P2-2，HarmonyOS 空间光效） ====================

/// 光源偏移跟随的目标最大幅度（px）。
const LIGHT_MAX_OFF: i32 = 24;

static mut LIGHT_X: f32 = 0.0; // 当前偏移（渲染用）
static mut LIGHT_Y: f32 = 0.0;
static mut LIGHT_TX: f32 = 0.0; // 目标偏移
static mut LIGHT_TY: f32 = 0.0;
static mut LIGHT_T0: u64 = 0; // 本段跟随起点时刻
static mut LIGHT_FX: f32 = 0.0; // 本段跟随起始偏移
static mut LIGHT_FY: f32 = 0.0;
static mut LIGHT_TOUCH_US: u64 = 0; // 最近一次触摸时刻（0 = 无触摸）
static mut LIGHT_DECAY: bool = false; // 回零段锚定标志（只起一次 tween）

/// 喂触摸点（屏幕坐标）与屏幕尺寸：按「相对屏幕中心位移 × 0.15」更新光源
/// 目标。无输入设备的应用不喂即保持默认 (0,0)。
pub fn light_track(px: i32, py: i32, w: i32, h: i32, now_us: u64) {
    unsafe {
        LIGHT_TOUCH_US = now_us;
        let tx = ((px - w / 2) as f32 * 0.15).clamp(-LIGHT_MAX_OFF as f32, LIGHT_MAX_OFF as f32);
        let ty = ((py - h / 2) as f32 * 0.15).clamp(-LIGHT_MAX_OFF as f32, LIGHT_MAX_OFF as f32);
        if (tx - LIGHT_TX).abs() + (ty - LIGHT_TY).abs() > 0.5 {
            // 新目标：从当前缓动位置起算 120ms FastOutSlowIn 跟随。
            LIGHT_FX = LIGHT_X;
            LIGHT_FY = LIGHT_Y;
            LIGHT_TX = tx;
            LIGHT_TY = ty;
            LIGHT_T0 = now_us;
        }
    }
}

/// 推进光源跟随（120ms FastOutSlowIn；触摸离开 300ms 后目标回零）。
/// 返回偏移是否有变化（应用据此触发重绘）。
pub fn light_frame(now_us: u64) -> bool {
    unsafe {
        // 离手回零：锚定一次 tween（此前触发条件置 0 后恒真，每帧重设
        // FX/T0 → 实际是逐帧指数收敛，而非一次 120ms 缓动）。
        if LIGHT_TOUCH_US != 0
            && now_us.saturating_sub(LIGHT_TOUCH_US) > 300_000
            && (LIGHT_X.abs() + LIGHT_Y.abs() > 0.05 || LIGHT_TX != 0.0 || LIGHT_TY != 0.0)
        {
            LIGHT_TOUCH_US = 0;
            LIGHT_FX = LIGHT_X;
            LIGHT_FY = LIGHT_Y;
            LIGHT_TX = 0.0;
            LIGHT_TY = 0.0;
            LIGHT_T0 = now_us;
            LIGHT_DECAY = true;
        }
        if LIGHT_TOUCH_US != 0 {
            // 触摸中：新目标由 light_track 锚定，衰减标志复位
            LIGHT_DECAY = false;
        }
        if LIGHT_T0 == 0 {
            return false;
        }
        let el = now_us.saturating_sub(LIGHT_T0);
        if el >= 120_000 {
            let done = (LIGHT_X - LIGHT_TX).abs() + (LIGHT_Y - LIGHT_TY).abs() < 0.05;
            LIGHT_X = LIGHT_TX;
            LIGHT_Y = LIGHT_TY;
            if done && LIGHT_DECAY && LIGHT_TX == 0.0 && LIGHT_TY == 0.0 {
                // 回零段完成：清锚定，停止重绘
                LIGHT_DECAY = false;
                LIGHT_T0 = 0;
                return false;
            }
            return true;
        }
        // FastOutSlowIn 近似（0.4,0,0.2,1）的 8 段等距折线查表，避免每帧解贝塞尔。
        const Y: [f32; 9] = [0.0, 0.33, 0.60, 0.78, 0.89, 0.95, 0.98, 0.995, 1.0];
        let t = el as f32 / 120_000.0;
        let i = ((t * 8.0) as usize).min(7);
        let lt = (t * 8.0) - i as f32;
        let e = Y[i] + (Y[i + 1] - Y[i]) * lt;
        let nx = LIGHT_FX + (LIGHT_TX - LIGHT_FX) * e;
        let ny = LIGHT_FY + (LIGHT_TY - LIGHT_FY) * e;
        let moved = (nx - LIGHT_X).abs() + (ny - LIGHT_Y).abs() >= 0.05;
        LIGHT_X = nx;
        LIGHT_Y = ny;
        moved
    }
}

/// 当前光源偏移（整数 px，渲染路径用）。
pub fn light_offset() -> (i32, i32) {
    unsafe { (LIGHT_X as i32, LIGHT_Y as i32) }
}

/// 玻璃绘制目标：调用方提供背景读取与像素写入。
pub trait GlassTarget {
    /// 读取屏幕坐标 (x, y) 处的背景像素（模糊输入源）。
    fn backdrop(&self, x: i32, y: i32) -> u32;
    /// 写入屏幕坐标 (x, y) 处的最终像素。
    fn put(&mut self, x: i32, y: i32, color: u32);
}

/// 标准 alpha 混合：结果 = a * (1 - t) + b * t，t ∈ 0..=255。
/// t ≥ 255 直通返回 b（AA 填充内部整块走此快路径）。
#[inline]
pub fn blend(a: u32, b: u32, t: u32) -> u32 {
    if t >= 255 {
        return b;
    }
    let inv = 255 - t;
    let r = (((a >> 16) & 0xFF) * inv + ((b >> 16) & 0xFF) * t) / 255;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * t) / 255;
    let bl = ((a & 0xFF) * inv + (b & 0xFF) * t) / 255;
    (r << 16) | (g << 8) | bl
}

/// 三像素通道平均。
#[inline]
fn avg3(a: u32, b: u32, c: u32) -> u32 {
    let r = (((a >> 16) & 0xFF) + ((b >> 16) & 0xFF) + ((c >> 16) & 0xFF)) / 3;
    let g = (((a >> 8) & 0xFF) + ((b >> 8) & 0xFF) + ((c >> 8) & 0xFF)) / 3;
    let bl = ((a & 0xFF) + (b & 0xFF) + (c & 0xFF)) / 3;
    (r << 16) | (g << 8) | bl
}

/// 点 (dx, dy)（相对圆角矩形左上角，按像素中心采样）是否在圆角矩形 (w, h, r) 内。
/// 几何与抗锯齿填充的超椭圆圆角一致（对角线更饱满，ColorOS 15 观感）。
#[inline]
pub fn in_round_rect(dx: i32, dy: i32, w: i32, h: i32, r: i32) -> bool {
    if w <= 0 || h <= 0 {
        return false;
    }
    let r = r.max(0).min(w / 2).min(h / 2) as f32;
    rr_sub_inside(
        dx as f32 + 0.5,
        dy as f32 + 0.5,
        w as f32,
        h as f32,
        r,
        rr_k(r),
    )
}

/// 双线性插值（f32，用于放大回原尺寸）。
#[inline]
fn bilerp(a: u32, b: u32, c: u32, d: u32, tx: f32, ty: f32) -> u32 {
    let lerp = |x: u32, y: u32, t: f32| (x as f32 + (y as f32 - x as f32) * t) as u32;
    let ar = (a >> 16) & 0xFF;
    let ag = (a >> 8) & 0xFF;
    let ab = a & 0xFF;
    let br = (b >> 16) & 0xFF;
    let bg = (b >> 8) & 0xFF;
    let bb = b & 0xFF;
    let cr = (c >> 16) & 0xFF;
    let cg = (c >> 8) & 0xFF;
    let cb = c & 0xFF;
    let dr = (d >> 16) & 0xFF;
    let dg = (d >> 8) & 0xFF;
    let db = d & 0xFF;
    let tr = lerp(ar, br, tx);
    let tg = lerp(ag, bg, tx);
    let tb = lerp(ab, bb, tx);
    let br2 = lerp(cr, dr, tx);
    let bg2 = lerp(cg, dg, tx);
    let bb2 = lerp(cb, db, tx);
    let r = lerp(tr, br2, ty);
    let g = lerp(tg, bg2, ty);
    let b = lerp(tb, bb2, ty);
    (r << 16) | (g << 8) | b
}

/// 玻璃材质样式：着色 + 高光 + 斜向光泽 + 描边。
#[derive(Clone, Copy)]
pub struct GlassStyle {
    /// 半透明着色（0x00RRGGBB）。
    pub tint: u32,
    /// 着色透明度（0..=255）。
    pub alpha: u32,
    /// 顶部镜面高光最大透明度（0..=255）。
    pub spec: u32,
    /// 左上→右下斜向液态光泽最大透明度（0..=255）。
    pub sheen: u32,
    /// 1px 描边颜色（0x00RRGGBB）。
    pub border: u32,
    /// 描边透明度（0..=255）。
    pub border_a: u32,
}

/// 深色玻璃：暗蓝着色 + 弱高光（系统默认，导航栏 / 深色应用）。
pub const STYLE_DARK: GlassStyle = GlassStyle {
    tint: 0x00_20_26_45,
    alpha: 166,
    spec: 26,
    sheen: 14,
    border: 0x00_7C_92_B8,
    border_a: 130,
};

/// 浅色星光玻璃：冷白半透明着色、柔和高光与细亮边。
pub const STYLE_LIGHT: GlassStyle = GlassStyle {
    tint: 0x00_F6_F7_FF,
    alpha: 156,
    spec: 38,
    sheen: 12,
    border: 0x00_FF_FF_FF,
    border_a: 180,
};

/// 强调色玻璃（主按钮 / 开关）：半透明星光蓝紫 + 高光。
pub const STYLE_ACCENT: GlassStyle = GlassStyle {
    tint: starlight::ACCENT,
    alpha: 238,
    spec: 46,
    sheen: 22,
    border: 0x00_C8_CE_FF,
    border_a: 160,
};

/// 红/警告玻璃（删除按钮等）。
pub const STYLE_DANGER: GlassStyle = GlassStyle {
    tint: 0x00_C8_3B_2E,
    alpha: 238,
    spec: 42,
    sheen: 20,
    border: 0x00_F0_9A_90,
    border_a: 150,
};

/// 按默认样式绘制毛玻璃面板（dark = 深色玻璃，false = 浅色玻璃）。
pub fn glass_rect<T: GlassTarget>(
    t: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    dark: bool,
    border: bool,
) {
    let st = if dark { &STYLE_DARK } else { &STYLE_LIGHT };
    glass_rect_style(t, x, y, w, h, r, st, border);
}

/// 按自定义样式绘制毛玻璃 / 液态玻璃面板。
pub fn glass_rect_style<T: GlassTarget>(
    t: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    st: &GlassStyle,
    border: bool,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = r.max(0).min(w / 2).min(h / 2);
    let ri = (r - 1).max(0);
    const DOWN: i32 = 4;
    let dw = ((w + DOWN - 1) / DOWN).max(1);
    let dh = ((h + DOWN - 1) / DOWN).max(1);
    if dw as usize > GLASS_DOWN_W || dh as usize > GLASS_DOWN_H {
        // 超出缓冲容量（极少见的大区域）：退化为半透明着色（不模糊）。
        glass_rect_solid(t, x, y, w, h, r, st, border);
        return;
    }
    let wu = w as usize;
    let hu = h as usize;
    let dwu = dw as usize;
    let dhu = dh as usize;
    unsafe {
        let buf = core::ptr::addr_of_mut!(GLASS_DOWN) as *mut u32;
        // 1. 降采样：每个 4x4 块取 (0,0)/(2,0)/(0,2)/(2,2) 四点平均（省一半读取）。
        for j in 0..dhu {
            let sy = y + j as i32 * DOWN;
            for i in 0..dwu {
                let sx = x + i as i32 * DOWN;
                let mut rs = 0u32;
                let mut gs = 0u32;
                let mut bs = 0u32;
                let c0 = t.backdrop(sx, sy);
                let c1 = t.backdrop(sx + 2, sy);
                let c2 = t.backdrop(sx, sy + 2);
                let c3 = t.backdrop(sx + 2, sy + 2);
                rs += (c0 >> 16) & 0xFF;
                rs += (c1 >> 16) & 0xFF;
                rs += (c2 >> 16) & 0xFF;
                rs += (c3 >> 16) & 0xFF;
                gs += (c0 >> 8) & 0xFF;
                gs += (c1 >> 8) & 0xFF;
                gs += (c2 >> 8) & 0xFF;
                gs += (c3 >> 8) & 0xFF;
                bs += c0 & 0xFF;
                bs += c1 & 0xFF;
                bs += c2 & 0xFF;
                bs += c3 & 0xFF;
                *buf.add(j * dwu + i) = ((rs / 4) << 16) | ((gs / 4) << 8) | (bs / 4);
            }
        }
        // 2. 小缓冲 3x3 盒式模糊（水平一趟 + 垂直一趟，就地）。
        for j in 0..dhu {
            let row = j * dwu;
            for i in 0..dwu {
                let idx = row + i;
                let c0 = *buf.add(row + i.saturating_sub(1));
                let c1 = *buf.add(idx);
                let c2 = *buf.add(row + (i + 1).min(dwu - 1));
                *buf.add(idx) = avg3(c0, c1, c2);
            }
        }
        for i in 0..dwu {
            for j in 0..dhu {
                let idx = j * dwu + i;
                let c0 = *buf.add(j.saturating_sub(1) * dwu + i);
                let c1 = *buf.add(idx);
                let c2 = *buf.add((j + 1).min(dhu - 1) * dwu + i);
                *buf.add(idx) = avg3(c0, c1, c2);
            }
        }
        // 3. 双线性放大 + 着色 + 顶部高光 + 斜向光泽 + 抗锯齿圆角 + 描边。
        // 光随指动：高光带随光源垂直偏移、斜向光泽随其对角偏移（P2-2）。
        let (lx, ly) = light_offset();
        let spec_h = (h / 3).max(1);
        let sheen_len = ((w + h) / 3).max(1);
        let white = 0x00_FF_FF_FF;
        for dy in 0..hu {
            let fy = (dy as f32 / DOWN as f32) - 0.5;
            let j0 = (fy as i32).max(0).min(dhu as i32 - 1);
            let j1 = (j0 + 1).min(dhu as i32 - 1);
            // 顶/左边缘 fy<0 时向零截断得负 ty → bilerp 线性外插，暗背景
            // 下负值饱和为 0（亮背景顶行/左列暗边）。钳 [0,1]。
            let ty = (fy - j0 as f32).clamp(0.0, 1.0);
            for dx in 0..wu {
                // 覆盖度：仅四个 r×r 圆角盒需要超采样，内部恒为不透明。
                let in_box = (dx < r as usize || dx >= w as usize - r as usize)
                    && (dy < r as usize || dy >= h as usize - r as usize);
                let cov = if in_box {
                    rr_coverage(dx as f32, dy as f32, w, h, r)
                } else {
                    255
                };
                if cov == 0 {
                    continue;
                }
                let fx = (dx as f32 / DOWN as f32) - 0.5;
                let i0 = (fx as i32).max(0).min(dwu as i32 - 1);
                let i1 = (i0 + 1).min(dwu as i32 - 1);
                let tx = (fx - i0 as f32).clamp(0.0, 1.0);
                let a = *buf.add(j0 as usize * dwu + i0 as usize);
                let b = *buf.add(j0 as usize * dwu + i1 as usize);
                let c = *buf.add(j1 as usize * dwu + i0 as usize);
                let d = *buf.add(j1 as usize * dwu + i1 as usize);
                let mut out = blend(bilerp(a, b, c, d, tx, ty), st.tint, st.alpha);
                // 顶部镜面高光（液态玻璃的“受光面”，随光源上下移动）。
                let sdy = dy as i32 - ly;
                if sdy >= 0 && sdy < spec_h {
                    let k = ((spec_h - sdy) * st.spec as i32 / spec_h) as u32;
                    out = blend(out, white, k);
                }
                // 左上→右下斜向光泽带（随光源沿对角线移动）。
                let sheen_d = dx as i32 + dy as i32 - (lx + ly);
                if sheen_d >= 0 && sheen_d < sheen_len {
                    let k = ((sheen_len - sheen_d) * st.sheen as i32 / sheen_len) as u32;
                    out = blend(out, white, k);
                }
                // 抗锯齿描边：直边保持锐利 1px，圆角段随曲线淡出。
                if border {
                    let on_straight =
                        dx == 0 || dy == 0 || dx == w as usize - 1 || dy == h as usize - 1;
                    let ba = if on_straight {
                        st.border_a
                    } else if in_box {
                        let inner = rr_coverage(dx as f32 - 1.0, dy as f32 - 1.0, w - 2, h - 2, ri);
                        st.border_a * (255 - inner) / 255
                    } else {
                        0
                    };
                    if ba > 0 {
                        out = blend(out, st.border, ba);
                    }
                }
                // 圆角边缘按覆盖度与背景混合（平滑轮廓，无锯齿阶梯）。
                if cov < 255 {
                    let bg = t.backdrop(x + dx as i32, y + dy as i32);
                    out = blend(bg, out, cov);
                }
                t.put(x + dx as i32, y + dy as i32, out);
            }
        }
    }
}

// ==================== 抗锯齿超椭圆圆角（对标 ColorOS 15 圆滑度） ====================

/// 超椭圆混合系数：圆角半径越大越接近 n=4 连续曲率（squircle），
/// 小半径退回正圆弧。ColorOS 15 / iOS 的连续圆角观感来自这种「更饱满的对角线」。
#[inline]
pub fn rr_k(r: f32) -> f32 {
    (r / 24.0).min(1.0)
}

/// 单个亚像素采样点 (px, py)（相对圆角矩形左上角，f32）是否在超椭圆圆角矩形内。
/// 隐函数：`(1-k)·(qx²+qy²)/r² + k·(qx⁴+qy⁴)/r⁴ ≤ 1`；
/// k→0 为正圆弧，k→1 为 n=4 超椭圆。只用整数次幂，no_std 可用。
#[inline]
pub fn rr_sub_inside(px: f32, py: f32, w: f32, h: f32, r: f32, k: f32) -> bool {
    if px < 0.0 || py < 0.0 || px >= w || py >= h {
        return false;
    }
    if r <= 0.0 {
        return true;
    }
    let qx = if px < r {
        r - px
    } else if px > w - r {
        px - (w - r)
    } else {
        0.0
    };
    let qy = if py < r {
        r - py
    } else if py > h - r {
        py - (h - r)
    } else {
        0.0
    };
    if qx <= 0.0 && qy <= 0.0 {
        return true;
    }
    let d2 = (qx * qx + qy * qy) / (r * r);
    let d4 = d2 * d2;
    (1.0 - k) * d2 + k * d4 <= 1.0
}

/// 亚像素坐标 (fx, fy)（相对圆角矩形左上角）处的覆盖度（0..=255）。
/// 4x4 = 16 超采样；只应在圆角盒 / 边缘带上调用（内部直线段恒为 255）。
pub fn rr_coverage(fx: f32, fy: f32, w: i32, h: i32, r: i32) -> u32 {
    if w <= 0 || h <= 0 {
        return 0;
    }
    let w = w as f32;
    let h = h as f32;
    let r = r.max(0) as f32;
    let r = if r * 2.0 > w { w * 0.5 } else { r };
    let r = if r * 2.0 > h { h * 0.5 } else { r };
    let k = rr_k(r);
    let mut hits = 0u32;
    for sy in 0..4 {
        for sx in 0..4 {
            if rr_sub_inside(
                fx + (sx as f32 + 0.5) * 0.25,
                fy + (sy as f32 + 0.5) * 0.25,
                w,
                h,
                r,
                k,
            ) {
                hits += 1;
            }
        }
    }
    hits * 255 / 16
}

/// 抗锯齿绘制目标：把颜色以 alpha（0..=255）混合到目标像素。
pub trait SmoothTarget {
    fn put_aa(&mut self, x: i32, y: i32, color: u32, alpha: u32);
}

/// 抗锯齿超椭圆圆角矩形填充：内部直线段整像素快路径，仅四个 r×r 圆角盒
/// 逐像素 16 超采样混合。形状与 [`in_round_rect`] 的超椭圆几何一致。
pub fn smooth_fill_rr<T: SmoothTarget>(
    t: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    color: u32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = r.max(0).min(w / 2).min(h / 2);
    if r == 0 {
        for dy in 0..h {
            for dx in 0..w {
                t.put_aa(x + dx, y + dy, color, 255);
            }
        }
        return;
    }
    // 1. 中带（上下圆角盒之间）整行不透明。
    for dy in r..h - r {
        for dx in 0..w {
            t.put_aa(x + dx, y + dy, color, 255);
        }
    }
    // 2. 上 / 下圆角带：中间列整段不透明，左右圆角盒按覆盖度混合。
    for band in 0..2 {
        let by = if band == 0 { 0 } else { h - r };
        for dy in 0..r {
            for dx in r..w - r {
                t.put_aa(x + dx, y + by + dy, color, 255);
            }
        }
        for dy in 0..r {
            for dx in 0..r {
                let cov = rr_coverage(dx as f32, (by + dy) as f32, w, h, r);
                if cov > 0 {
                    t.put_aa(x + dx, y + by + dy, color, cov);
                }
                let cov = rr_coverage((w - r + dx) as f32, (by + dy) as f32, w, h, r);
                if cov > 0 {
                    t.put_aa(x + w - r + dx, y + by + dy, color, cov);
                }
            }
        }
    }
}

/// 圆角矩形 1px 描边像素的透明度：外轮廓覆盖度 − 内缩 1px 覆盖度。
fn rr_edge_alpha(fx: f32, fy: f32, w: i32, h: i32, r: i32, ri: i32) -> u32 {
    let outer = rr_coverage(fx, fy, w, h, r);
    if outer == 0 {
        return 0;
    }
    let inner = rr_coverage(fx - 1.0, fy - 1.0, w - 2, h - 2, ri);
    outer.saturating_sub(inner)
}

/// 抗锯齿实心圆：内部整块不透明，圆周 1px 环带 16 超采样混合。
pub fn smooth_fill_circle<T: SmoothTarget>(t: &mut T, cx: i32, cy: i32, r: i32, color: u32) {
    if r <= 0 {
        return;
    }
    let rf = r as f32;
    let r2 = rf * rf;
    let r2in = (r - 1) * (r - 1);
    let r2out = (r + 1) * (r + 1);
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 <= r2in {
                t.put_aa(cx + dx, cy + dy, color, 255);
            } else if d2 <= r2out {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = dx as f32 - 0.5 + (sx as f32 + 0.5) * 0.25;
                        let py = dy as f32 - 0.5 + (sy as f32 + 0.5) * 0.25;
                        if px * px + py * py <= r2 {
                            hits += 1;
                        }
                    }
                }
                let a = hits * 255 / 16;
                if a > 0 {
                    t.put_aa(cx + dx, cy + dy, color, a);
                }
            }
        }
    }
}

/// f32 平方根（位技巧初值 + 3 次牛顿迭代，no_std 可用）。
#[inline]
fn f32_sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    let mut y = f32::from_bits((bits >> 1) + 0x1FC0_0000);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// 抗锯齿 1px 圆环：像素对环带 [r-½, r+½] 的覆盖度按距离线性近似。
pub fn smooth_stroke_circle<T: SmoothTarget>(t: &mut T, cx: i32, cy: i32, r: i32, color: u32) {
    if r <= 0 {
        return;
    }
    let rf = r as f32;
    let r_in2 = (r - 1) * (r - 1);
    let r_out2 = (r + 1) * (r + 1);
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 < r_in2 || d2 > r_out2 {
                continue;
            }
            let d = f32_sqrt(d2 as f32);
            let cov = 1.0 - (d - rf).abs();
            if cov > 0.0 {
                t.put_aa(cx + dx, cy + dy, color, (cov * 255.0) as u32);
            }
        }
    }
}

/// 抗锯齿圆角矩形 1px 描边：直边保持锐利 1px，圆角段随曲线淡出。
pub fn smooth_stroke_rr<T: SmoothTarget>(
    t: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    color: u32,
    alpha: u32,
) {
    if w <= 0 || h <= 0 || alpha == 0 {
        return;
    }
    let r = r.max(0).min(w / 2).min(h / 2);
    if r == 0 {
        for dx in 0..w {
            t.put_aa(x + dx, y, color, alpha);
            t.put_aa(x + dx, y + h - 1, color, alpha);
        }
        for dy in 0..h {
            t.put_aa(x, y + dy, color, alpha);
            t.put_aa(x + w - 1, y + dy, color, alpha);
        }
        return;
    }
    // 四条直边。
    for dx in r..w - r {
        t.put_aa(x + dx, y, color, alpha);
        t.put_aa(x + dx, y + h - 1, color, alpha);
    }
    for dy in r..h - r {
        t.put_aa(x, y + dy, color, alpha);
        t.put_aa(x + w - 1, y + dy, color, alpha);
    }
    // 四个圆角盒。
    let ri = r - 1;
    for dy in 0..r {
        for dx in 0..r {
            for corner in 0..4 {
                let (fx, fy) = match corner {
                    0 => (dx as f32, dy as f32),
                    1 => ((w - r + dx) as f32, dy as f32),
                    2 => (dx as f32, (h - r + dy) as f32),
                    _ => ((w - r + dx) as f32, (h - r + dy) as f32),
                };
                let a = rr_edge_alpha(fx, fy, w, h, r, ri);
                if a > 0 {
                    t.put_aa(
                        x + fx as i32,
                        y + fy as i32,
                        color,
                        alpha.min(alpha * a / 255),
                    );
                }
            }
        }
    }
}

/// 半透明着色面板（无模糊；用于超出降采样缓冲容量的超大区域）。
pub fn glass_rect_solid<T: GlassTarget>(
    t: &mut T,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    st: &GlassStyle,
    border: bool,
) {
    let spec_h = (h / 3).max(1);
    let sheen_len = ((w + h) / 3).max(1);
    let white = 0x00_FF_FF_FF;
    let r = r.max(0).min(w / 2).min(h / 2);
    let ri = (r - 1).max(0);
    let (lx, ly) = light_offset();
    for dy in 0..h {
        for dx in 0..w {
            let in_box = (dx < r || dx >= w - r) && (dy < r || dy >= h - r);
            let cov = if in_box {
                rr_coverage(dx as f32, dy as f32, w, h, r)
            } else {
                255
            };
            if cov == 0 {
                continue;
            }
            let bg = t.backdrop(x + dx, y + dy);
            let mut out = blend(bg, st.tint, st.alpha);
            let sdy = dy - ly;
            if sdy >= 0 && sdy < spec_h {
                let k = ((spec_h - sdy) * st.spec as i32 / spec_h) as u32;
                out = blend(out, white, k);
            }
            let sheen_d = dx + dy - (lx + ly);
            if sheen_d >= 0 && sheen_d < sheen_len {
                let k = ((sheen_len - sheen_d) * st.sheen as i32 / sheen_len) as u32;
                out = blend(out, white, k);
            }
            if border {
                let on_straight = dx == 0 || dy == 0 || dx == w - 1 || dy == h - 1;
                let ba = if on_straight {
                    st.border_a
                } else if in_box {
                    let inner = rr_coverage(dx as f32 - 1.0, dy as f32 - 1.0, w - 2, h - 2, ri);
                    st.border_a * (255 - inner) / 255
                } else {
                    0
                };
                if ba > 0 {
                    out = blend(out, st.border, ba);
                }
            }
            if cov < 255 {
                out = blend(bg, out, cov);
            }
            t.put(x + dx, y + dy, out);
        }
    }
}
