// KunxOS 应用启动动画与并行加载（对标 ColorOS 15 应用开启/打断节奏）。
//
// 点击反馈（按压 0.95 缩放，80ms 到位）→ 点击后立即 fork+execve 并行加载应用，
// 同时播放 spring(300ms, bounce≈0.07) 图标放大到全屏动画；动画结束应用未就绪时
// 显示占位页面（应用主题色背景 + 居中图标），直到应用首帧覆盖屏幕；应用退出后
// 以 ease-out 300ms 从全屏缩回图标原位。
//
// 打断（上划取消启动）：图标撕成上下两半向对侧飞出并淡出（撕裂间距随滑动速度
// 增大），1s 后两半弹簧飞回原位重新合拢（ColorOS 取消启动动画）。

use super::*;

/// 阻尼谐振子弹簧（iOS 风格：dampingRatio + initialVelocity）。
pub struct Spring {
    pub stiffness: f32,
    pub damping: f32,
    pub mass: f32,
    pub target: f32,
    pub pos: f32,
    pub vel: f32,
}

impl Spring {
    pub const fn fixed() -> Spring {
        Spring {
            stiffness: 0.0,
            damping: 0.0,
            mass: 1.0,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    pub fn ios(damping_ratio: f32, initial_velocity: f32) -> Spring {
        let stiffness = 280.0;
        let mass = 1.0;
        let crit = 2.0 * fsqrt(stiffness * mass);
        Spring {
            stiffness,
            mass,
            damping: crit * damping_ratio.clamp(0.05, 2.0),
            target: 0.0,
            pos: 0.0,
            vel: initial_velocity,
        }
    }
    /// ColorOS 15 应用开启弹簧：300ms 收敛，过冲 ≈7%（比 iOS 更利落）。
    pub fn launch() -> Spring {
        let zeta = 0.65; // bounce ≈0.07 的阻尼比（ColorOS 风格）
        let omega = 4.6 / (zeta * LAUNCH_DUR_S);
        Spring {
            stiffness: omega * omega,
            mass: 1.0,
            damping: 2.0 * zeta * omega,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    /// 文件夹打开/关闭弹簧：≈350ms 收敛，轻微过冲。
    pub fn folder() -> Spring {
        let zeta = 0.55;
        let omega = 4.6 / (zeta * 0.35);
        Spring {
            stiffness: omega * omega,
            mass: 1.0,
            damping: 2.0 * zeta * omega,
            target: 0.0,
            pos: 0.0,
            vel: 0.0,
        }
    }
    /// 桌面翻页吸附已改用 ColorOSStandard 定时 tween（B5，main.rs
    /// page_settle/page_advance），弹簧版本退役删除。
    /// ColorOS 15 打断回弹弹簧：刚度 210 N/m、阻尼比 0.45 —— 比 iOS 打断弹簧
    /// （刚度 280、阻尼 0.5）更绵软，图标从最远处弹回时越过原位约 15~20%，
    /// 再以 2~3 次衰减摇摆收敛（果冻回弹）。`initial_velocity` 为弹出段末速
    /// （px/s，沿飞出方向），让回弹在松手瞬间保持惯性、过冲更明显。
    fn coloros(initial_velocity: f32) -> Spring {
        let zeta = 0.45;
        let stiffness = 210.0;
        let mass = 1.0;
        let crit = 2.0 * fsqrt(stiffness * mass);
        Spring {
            stiffness,
            mass,
            damping: crit * zeta,
            target: 0.0,
            pos: 0.0,
            vel: initial_velocity,
        }
    }
    /// 推进 `ms` 毫秒，返回是否已稳定在目标附近。
    pub fn step_ms(&mut self, ms: f32) -> bool {
        // 参数防护（与内核 anim::Spring / shared/motion.rs 一致）：mass ≥ 0.1、
        // stiffness ≤ 20000（半隐式欧拉 ωh < 2），积分出非有限值强制收敛，
        // 防止动画永不结束并持续驱动整屏重绘。
        if !self.mass.is_finite() || self.mass <= 0.0 {
            self.mass = 1.0;
        } else if self.mass < 0.1 {
            self.mass = 0.1;
        }
        if !self.stiffness.is_finite() || self.stiffness < 0.0 {
            self.stiffness = 0.0;
        } else if self.stiffness > 20_000.0 {
            self.stiffness = 20_000.0;
        }
        if !self.damping.is_finite() || self.damping < 0.0 {
            self.damping = 0.0;
        }
        let mut t = ms / 1000.0;
        while t > 0.0 {
            let h = if t > 0.001 { 0.001 } else { t };
            let a =
                (-self.stiffness * (self.pos - self.target) - self.damping * self.vel) / self.mass;
            self.vel += a * h;
            self.pos += self.vel * h;
            t -= h;
            if !self.pos.is_finite() || !self.vel.is_finite() {
                self.pos = self.target;
                self.vel = 0.0;
                return true;
            }
        }
        (self.pos - self.target).abs() < 0.5 && self.vel.abs() < 1.0
    }
}

/// 运动阶段。
#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    Idle,
    /// 按压反馈（图标缩小）。
    PressIn,
    /// 松开回弹（弹簧）。
    PressOut,
    /// 应用启动转场（弹簧放大 + 白闪）。
    Launch,
    /// 应用返回转场（ease-out 缩回图标原位）。
    Return,
    /// 应用恢复转场（从后台恢复：应用末帧快照从图标原位放大到全屏，
    /// 源为 FRAME_BUF 中的快照；动画期间应用仍 SIGSTOP，完成后通知 lifd 恢复）。
    Resume,
    /// 打断动画：应用图标从桌面原位飞离（上划取消启动），
    /// 快出慢收惯性弹出、飞行中顺方向倾斜并缩小（ColorOS 15 风格）。
    FlyAway,
    /// 打断动画：图标带末速弹簧飞回原位，越过原位后果冻式摇摆收敛。
    FlyBack,
}

/// 运动状态机。
pub struct Motion {
    pub phase: Phase,
    pub app: usize,
    pub cx: i32,
    pub cy: i32,
    pub t0: u64,
    pub press_scale: u32,
    pub press_app: usize,
    pub press_spring: Spring,
    pub press_t: u64,
    pub cur_scale: u32,
    pub cur_fade: u32,
    pub fly_x: i32,
    pub fly_y: i32,
    pub fly_home_x: i32,
    pub fly_home_y: i32,
    pub fly_dir_x: f32,
    pub fly_dir_y: f32,
    pub fly_dist: f32,
    pub fly_t0: u64,
    pub fly_spring: Spring,
    /// 飞行当前缩放（1.0 = 原位；最远处缩到 FLY_MIN_SCALE，ColorOS 打断特征）。
    pub fly_scale: f32,
    /// 飞行当前倾斜角（度，顺飞行方向，ColorOS 打断特征）。
    pub fly_tilt: f32,
    /// 弹出段本帧位移（px，用于末速估算）。
    pub fly_last_d: f32,
    /// 弹出段末速（px/s，沿飞出方向；衔接回弹弹簧初速度）。
    pub fly_v: f32,
    /// 打断撕裂偏移（px）：图标撕成上下两半、各向对侧分离（ColorOS 取消启动）。
    pub fly_split: i32,
    /// 本次打断的最大撕裂间距（越快滑动撕得越开）。
    pub fly_split_max: i32,
    /// 启动弹簧（scale 标量，定点 1.0=256）。
    pub launch_spring: Spring,
    /// 启动弹簧上次推进时刻（µs）。
    pub launch_t: u64,
    /// 覆盖全屏的缩放目标（定点 1.0=256）。
    pub launch_target: u32,
    /// 启动瓦片纵向超前（‰，1000 = 等比；放大途中 ×1.03 → 落位 1.0，
    /// squash & stretch，见 docs/animations.md §3.2）。
    pub cur_stretch: u32,
}

pub static mut MOTION: Motion = Motion {
    phase: Phase::Idle,
    app: 0,
    cx: 0,
    cy: 0,
    t0: 0,
    press_scale: SCALE_1,
    press_app: usize::MAX,
    press_spring: Spring::fixed(),
    press_t: 0,
    cur_scale: SCALE_1,
    cur_fade: 0,
    fly_x: 0,
    fly_y: 0,
    fly_home_x: 0,
    fly_home_y: 0,
    fly_dir_x: 0.0,
    fly_dir_y: 0.0,
    fly_dist: 0.0,
    fly_t0: 0,
    fly_spring: Spring::fixed(),
    fly_scale: 1.0,
    fly_tilt: 0.0,
    fly_last_d: 0.0,
    fly_v: 0.0,
    fly_split: 0,
    fly_split_max: 0,
    launch_spring: Spring::fixed(),
    launch_t: 0,
    launch_target: SCALE_1,
    cur_stretch: 1000,
};

/// 按压反馈：80ms 内缩到 0.95。
const PRESS_MS: u64 = 80;
const PRESS_SCALE: u32 = 243;
/// 返回转场：300ms ease-out。
const RETURN_MS: u64 = 300;
/// 启动弹簧：300ms 收敛（ColorOS 15 应用开启节奏）。
const LAUNCH_DUR_S: f32 = 0.30;
const LAUNCH_DUR_US: u64 = 300_000;
/// 返回动画终点缩放：应用末帧缩到图标足迹大小（定点 1.0=256）。
const RETURN_END_SCALE: u32 = {
    let v = SCALE_1 * ICON_SIZE as u32 / DISPLAY_H as u32;
    if v < 1 {
        1
    } else {
        v
    }
};
/// 桌面快照（转场期间作为缩放源，避免转场中重绘桌面产生闪烁）。
static mut FRAME_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];

/// 桌面预渲染存档（返回转场专用）：start_return 时把整幅桌面（BACK_BUF）
/// 存档于此，转场各帧以它为底图合成缩回中的应用帧，转场结束整体恢复 ——
/// 消除转场后的整屏 draw_desktop_full 尾巴（TCG 下 50~150ms）。
static mut DESK_BUF: [u32; DISPLAY_W * DISPLAY_H] = [0u32; DISPLAY_W * DISPLAY_H];
/// Return 转场是否处于「桌面预渲染」模式（render_frame Return 分支据此选路）。
static mut DESK_PRERENDERED: bool = false;
/// 上一合成帧的应用帧矩形（恢复基准）：缩回矩形逐帧变小，若只写矩形内部
/// 会在四周留下上一帧的应用像素拖尾 —— 每帧先用 DESK_BUF 恢复该区域再合成。
static mut DESK_RET_PREV: [i32; 4] = [0; 4];

/// Return 转场当前是否以预渲染桌面为底图（main.rs render_frame 查询）。
pub fn desk_prerendered() -> bool {
    unsafe { DESK_PRERENDERED }
}

/// 供 app_life 写入 Resume 动画的缩放源（应用末帧快照）。
pub fn frame_buf_mut() -> *mut u32 {
    unsafe { FRAME_BUF.as_mut_ptr() }
}

/// 转场缩放块采样粒度（输出像素块边长）。
///
/// 2 → 4：每帧全屏缩放合成的迭代数降至 1/4。转场（启动/返回/恢复）只在
/// 350ms/300ms 内播放，块边 4px 的最近邻放大在 540×960 下肉眼差异很小，
/// 但把 TCG 慢宿主下「每帧 13 万次像素迭代」降到 ~3.3 万次，动画帧率直接翻倍。
const ZOOM_BLOCK: i32 = 4;

/// 压暗乘数 LUT（快照压暗：blend(c, 黑, 90) = c * 165 / 255）。
static mut DARK_LUT: [u32; 256] = [0; 256];
static mut DARK_LUT_READY: bool = false;

#[inline(always)]
fn darken_lut() -> &'static [u32; 256] {
    unsafe {
        if !DARK_LUT_READY {
            for v in 0..256u32 {
                DARK_LUT[v as usize] = (v * 165) / 255;
            }
            DARK_LUT_READY = true;
        }
        &DARK_LUT
    }
}

/// 就地把 FRAME_BUF 全量压暗（LUT，c×165/255）。
///
/// 压暗从「launch_blit 每帧一次全屏 LUT 扫描」改为「转场开始时一次」：
/// 快照在启动转场期间不变，逐帧重复压暗是纯浪费。就地修改安全 ——
/// FRAME_BUF 的其它读者（zoom_blit 的 Return/Resume 转场）在使用前都会
/// 重新快照覆盖（start_return / app_life 的 resume 快照路径）。
pub fn darken_frame_buf() {
    unsafe {
        let lut = darken_lut();
        for c in FRAME_BUF.iter_mut() {
            let v = *c;
            *c = (lut[((v >> 16) & 0xFF) as usize] << 16)
                | (lut[((v >> 8) & 0xFF) as usize] << 8)
                | lut[(v & 0xFF) as usize];
        }
    }
}

/// 白闪混合 LUT（每帧按 fade 重建一次，逐像素从乘除降为查表）。
#[inline(always)]
fn build_white_lut(f: u32) -> [u32; 256] {
    let mut lut = [0u32; 256];
    let inv = 255 - f.min(255);
    for v in 0..256u32 {
        lut[v as usize] = (255 * f.min(255) + v * inv) / 255;
    }
    lut
}

/// 以 `(cx, cy)` 为锚点把 `FRAME_BUF` 缩放（定点 `scale`，1.0=256）绘制到 BACK_BUF，
/// 可选白色覆盖 `fade`（0..255）。最近邻采样 + `ZOOM_BLOCK` 块采样。
pub fn zoom_blit(scale: u32, cx: i32, cy: i32, fade: u32) {
    let s = scale.max(1) as i64;
    let inv = (256i64 << 16) / s;
    let w = DISPLAY_W as i32;
    let h = DISPLAY_H as i32;
    let bw = DISPLAY_W as usize;
    let wlut = if fade > 0 {
        Some(build_white_lut(fade.min(255)))
    } else {
        None
    };
    let bs = ZOOM_BLOCK as usize;
    // 列采样表每帧预计算一次：sx 只依赖 px，此前每行重算（240 行 × 135 列
    // 的 i64 乘移位）降为 135 次；越界列以 -1 标记（输出 COLOR_BG）。
    let mut sx_tab = [-1i32; (DISPLAY_W / ZOOM_BLOCK as usize) + 1];
    for (ci, px) in (0..w).step_by(bs).enumerate() {
        let sx = cx + (((px - cx) as i64 * inv) >> 16) as i32;
        sx_tab[ci] = if sx >= 0 && sx < w { sx } else { -1 };
    }
    for py in (0..h).step_by(bs) {
        let sy = cy + (((py - cy) as i64 * inv) >> 16) as i32;
        let sy_ok = sy >= 0 && sy < h;
        // 块填充边界一次性钳制（末行/末列可能不满块），内层循环无边界分支
        let bh = bs.min(h as usize - py as usize);
        for (ci, px) in (0..w).step_by(bs).enumerate() {
            let mut c = if sy_ok && sx_tab[ci] >= 0 {
                unsafe { FRAME_BUF[sy as usize * bw + sx_tab[ci] as usize] }
            } else {
                COLOR_BG
            };
            if let Some(lut) = &wlut {
                c = (lut[((c >> 16) & 0xFF) as usize] << 16)
                    | (lut[((c >> 8) & 0xFF) as usize] << 8)
                    | lut[(c & 0xFF) as usize];
            }
            // ZOOM_BLOCK × ZOOM_BLOCK 整块填充（块尺寸与 step_by 必须一致，
            // 否则块间留缝隙 → 转场画面残留上一帧内容）。
            let px_u = px as usize;
            let py_u = py as usize;
            let bw_fill = bs.min(w as usize - px_u);
            unsafe {
                for dy in 0..bh {
                    let iy = (py_u + dy) * bw;
                    for dx in 0..bw_fill {
                        BACK_BUF[iy + px_u + dx] = c;
                    }
                }
            }
        }
    }
}

/// 返回转场（桌面预渲染模式）：把 FRAME_BUF 中的应用末帧按 `scale`（定点
/// 1.0=256）以 (cx, cy) 为锚点缩小后叠加到 BACK_BUF 的桌面上。缩小的矩形
/// 之外不再填 COLOR_BG（预渲染桌面就是背景），也无需白闪 —— ColorOS 式
/// 「应用缩回图标、桌面在四周浮现」。采样映射与 zoom_blit 一致（定点逆
/// 变换 + ZOOM_BLOCK 块采样），矩形边界取源映射落入画面的精确范围。
///
/// 每帧先用 DESK_BUF 恢复上一帧矩形（EaseOut 单调缩小，取两帧矩形包围盒
/// 防御非单调），再合成当前矩形 —— 否则缩小的矩形四周会留下应用像素拖尾。
pub fn return_blit_over_desktop(scale: u32, cx: i32, cy: i32) {
    unsafe {
        let s = scale.max(1) as i64;
        let inv = (256i64 << 16) / s;
        let w = DISPLAY_W as i32;
        let h = DISPLAY_H as i32;
        let bw = DISPLAY_W as usize;
        // 与 zoom_blit 的 sx/sy 判定互逆：源 [0..w) 映射回目标轴的落点范围。
        let x0 = (cx - ((cx as i64 * s) >> 8) as i32).max(0);
        let x1 = (cx + ((((w - 1 - cx) as i64) * s) >> 8) as i32).min(w - 1);
        let y0 = (cy - ((cy as i64 * s) >> 8) as i32).max(0);
        let y1 = (cy + ((((h - 1 - cy) as i64) * s) >> 8) as i32).min(h - 1);
        if x0 > x1 || y0 > y1 {
            return;
        }
        let bs = ZOOM_BLOCK as i32;
        // 1. 恢复上一帧矩形 ∪ 当前矩形（防御性包围盒）区域的桌面底图。
        let px0 = DESK_RET_PREV[0].min(x0).max(0);
        let py0 = DESK_RET_PREV[1].min(y0).max(0);
        let px1 = DESK_RET_PREV[2].max(x1).min(w - 1);
        let py1 = DESK_RET_PREV[3].max(y1).min(h - 1);
        if px0 <= px1 && py0 <= py1 {
            for y in py0..=py1 {
                let row = y as usize * bw;
                BACK_BUF[row + px0 as usize..row + px1 as usize + 1]
                    .copy_from_slice(&DESK_BUF[row + px0 as usize..row + px1 as usize + 1]);
            }
        }
        // 2. 合成当前矩形内的应用帧（块采样 + 块填充）。
        let mut py = y0;
        while py <= y1 {
            let sy = cy + (((py - cy) as i64 * inv) >> 16) as i32;
            let sy_ok = sy >= 0 && sy < h;
            let mut px = x0;
            while px <= x1 {
                let sx = cx + (((px - cx) as i64 * inv) >> 16) as i32;
                let c = if sy_ok && sx >= 0 && sx < w {
                    FRAME_BUF[sy as usize * bw + sx as usize]
                } else {
                    COLOR_BG
                };
                let bx_end = (px as usize + bs as usize).min((x1 + 1) as usize);
                let by_end = (py as usize + bs as usize).min((y1 + 1) as usize);
                let mut yy = py as usize;
                while yy < by_end {
                    let iy = yy * bw;
                    let mut xx = px as usize;
                    while xx < bx_end {
                        BACK_BUF[iy + xx] = c;
                        xx += 1;
                    }
                    yy += 1;
                }
                px += bs;
            }
            py += bs;
        }
        DESK_RET_PREV = [x0, y0, x1, y1];
    }
}

/// 启动动画：应用图标方块从图标位置/尺寸逐渐放大到覆盖整个屏幕，
/// 白色淡入衔接应用首帧。
///
/// 启动缩放动画的块填充写入：透明角像素跳过（保留桌面底图），半透明
/// 边缘像素（最高位字节 alpha）与底图混合。
#[inline(always)]
unsafe fn zoom_put(idx: usize, c: u32) {
    if c == 0 {
        return;
    }
    let a = (c >> 24) & 0xFF;
    BACK_BUF[idx] = if a > 0 {
        blend(BACK_BUF[idx], c & 0x00_FF_FF_FF, a)
    } else {
        c
    };
}

/// 桌面快照 FRAME_BUF 压暗作为背景，图标瓦片 ICON_TILE 以 `scale`（定点）
/// 缩放绘制，`fade` 为白色覆盖（0..255，结尾全白衔接应用）。
///
/// `stretch` 为纵向边长超前（‰，1000 = 等比）：放大途中 y 向轻微拉伸
/// （squash & stretch），横纵独立采样，落位恢复等比精确覆盖全屏。
/// 白闪 LUT 作用于单个像素的 RGB（保留 alpha 字节，供瓦片半透明边缘在
/// 混合前先行变白 —— 与「先合成后整屏过 LUT」的差异仅在图标边缘数像素）。
#[inline(always)]
fn white_pixel(lut: &[u32; 256], c: u32) -> u32 {
    let v = c & 0x00_FF_FF_FF;
    (c & 0xFF00_0000)
        | (lut[((v >> 16) & 0xFF) as usize] << 16)
        | (lut[((v >> 8) & 0xFF) as usize] << 8)
        | lut[(v & 0xFF) as usize]
}

pub fn launch_blit(scale: u32, cx: i32, cy: i32, fade: u32, stretch: u32) {
    unsafe {
        let w = DISPLAY_W as i32;
        let h = DISPLAY_H as i32;
        let total = DISPLAY_W * DISPLAY_H;
        // 白闪 LUT 整场只建一次；激活时熔进背景拷贝与瓦片采样（单趟完成
        // 「拷贝 + 变白」，替代原先拷贝后对整屏的第二趟扫描 —— 转场后段
        // 每帧少穿行 ~2MB，TCG 慢宿主下每帧省数十毫秒）。
        let wlut = if fade > 0 {
            Some(build_white_lut(fade.min(255)))
        } else {
            None
        };
        // 1. 已压暗的桌面快照作为背景（转场开始时 darken_frame_buf 压暗一次）；
        //    白闪激活时同趟完成变白。
        match &wlut {
            Some(lut) => {
                for i in 0..total {
                    let v = FRAME_BUF[i];
                    BACK_BUF[i] = (lut[((v >> 16) & 0xFF) as usize] << 16)
                        | (lut[((v >> 8) & 0xFF) as usize] << 8)
                        | lut[(v & 0xFF) as usize];
                }
            }
            None => {
                BACK_BUF[..total].copy_from_slice(&FRAME_BUF);
            }
        }
        // 2. 图标方块放大：ICON_TILE（ICON_SIZE x ICON_SIZE 圆角图标）按当前边长缩放，
        //    以图标中心 (cx, cy) 为锚点。2px 块 + 块中心预乘双线性采样：放大途中
        //    图标边缘平滑（最近邻 + 4px 块会呈现明显马赛克），迭代数仍远低于逐像素。
        let s = (ICON_SIZE as u32 * scale / SCALE_1).max(1) as i32;
        // 纵向边长按 stretch（‰）超前，横纵分离。
        let sy = ((s as i64 * stretch.max(1000).min(2000) as i64) / 1000).max(1) as i32;
        let half = s / 2;
        let half_y = sy / 2;
        let x0 = (cx - half).max(0);
        let y0 = (cy - half_y).max(0);
        let x1 = (cx + half).min(w - 1);
        let y1 = (cy + half_y).min(h - 1);
        let bs = 2usize;
        let isz = ICON_SIZE as u32;
        let sf = isz as f32 / s.max(1) as f32;
        let sfy = isz as f32 / sy.max(1) as f32;
        let mut py = y0;
        while py <= y1 {
            let mut px = x0;
            while px <= x1 {
                let dx = px - (cx - half); // 0..s
                let dy = py - (cy - half_y); // 0..sy
                // 块中心 (+0.5) 映射回源贴图连续坐标（横纵独立缩放）。
                let fx = (dx as f32 + 0.5) * sf - 0.5;
                let fy = (dy as f32 + 0.5) * sfy - 0.5;
                let c0 = sample_icon_bilinear_stride(
                    &ICON_TILE,
                    ICON_SIZE as usize,
                    ICON_SIZE as usize,
                    fx,
                    fy,
                );
                let c = match &wlut {
                    Some(lut) => white_pixel(lut, c0),
                    None => c0,
                };
                // bs×bs 整块填充（步进=块尺寸）。
                let bx_end = (px as usize + bs).min(w as usize);
                let by_end = (py as usize + bs).min(h as usize);
                let mut yy = py as usize;
                while yy < by_end {
                    let iy = yy * DISPLAY_W;
                    let mut xx = px as usize;
                    while xx < bx_end {
                        zoom_put(iy + xx, c);
                        xx += 1;
                    }
                    yy += 1;
                }
                px += bs as i32;
            }
            py += bs as i32;
        }
        // 3. 白色覆盖已熔进第 1/2 步（见 wlut）—— 无独立整屏扫描。
    }
}

/// 启动门控文件：动画播放期间写入 "0"（应用等待），动画完成写 "1"（放行）。
const LAUNCH_GATE: &str = "/data/oobe/.launch_gate";

/// 正在启动的应用子进程 pid（start_launch 中 fork，动画完成时 waitpid 回收）。
static mut LAUNCH_PID: isize = -1;
static mut LAUNCH_IDX: usize = 0;
/// 待回收的取消启动子进程（start_fly_away 登记，poll_cancel_reap 非阻塞回收）。
static mut CANCEL_PID: isize = -1;
static mut CANCEL_IDX: usize = 0;
/// WNOHANG（wait4 options）。
const WNOHANG: usize = 0x1;
/// SYS_KUNX_SCHED_SET_DEADLINE：当前任务一次性调度 deadline（相对 µs，0 清除）。
const SYS_KUNX_SCHED_SET_DEADLINE: usize = 414;
/// 启动窗口时长：子进程加载（ELF/字体/数据）+ 首帧渲染期间以交互期限
/// 参与 CFS 竞争，到期自动失效（潮汐引擎「启动线程优先」的等价物）。
const LAUNCH_BOOST_US: usize = 3_000_000;

/// 清除启动中记录的子 pid。应用在启动动画窗口内被 KILLED/EXITED（app_life
/// 事件路径回收僵尸）时调用——若不清除，动画完成时 `launch_complete` 会拿
/// 陈旧 pid 无条件进入前台态，launcher 永久卡在「前台轮询」黑屏假死。
pub fn clear_launch_pid() {
    unsafe { LAUNCH_PID = -1 };
}

/// 写启动门控（'0' 等待 / '1' 放行 / '2' 取消）。返回是否写盘成功。
fn gate_write(v: u8) -> bool {
    // 写 ASCII 字符 '0'/'1'/'2'（应用侧 wait_launch_gate 按 b'1'/b'2' 比较）。
    let ok = unsafe { write_file(LAUNCH_GATE, &[b'0' + v.min(9)]) };
    if !ok {
        print("[launcher] gate_write 失败（v=");
        print_dec(v as u32);
        print(")\n");
    }
    ok
}

/// 写放行门控 '1'，失败重试一次。
///
/// 放行写失败会让应用侧 wait_launch_gate 白等 3s 超时（或读到上次残留的
/// "2" 直接 exit(1)），表象"点击图标秒退无提示"，因此放行必须尽量落地。
fn gate_release() -> bool {
    if gate_write(1) {
        return true;
    }
    gate_write(1)
}

/// unlink 门控文件（SYS_UNLINKAT / AT_FDCWD）。返回是否成功（不存在也算成功）。
fn gate_unlink() -> bool {
    const SYS_UNLINKAT: usize = 35;
    unsafe {
        let mut p = [0u8; 96];
        let n = LAUNCH_GATE.len().min(p.len() - 1);
        p[..n].copy_from_slice(LAUNCH_GATE.as_bytes());
        p[n] = 0;
        let r = syscall3(SYS_UNLINKAT, AT_FDCWD, p.as_ptr() as usize, 0) as isize;
        r == 0 || r == -2 // -2 = -ENOENT：本就不存在，视为已清除
    }
}

/// 进入启动等待：unlink 旧门控后重建并写入 '0'，再重读校验内容确为 '0'。
/// 返回 false 表示门控不可用（不应 fork 应用：应用可能读到残留值误判取消）。
fn gate_prepare_wait() -> bool {
    // 先 unlink 再重建：若仅覆写失败，旧内容（"1"/"2"）会残留并被子进程读到
    // （读到 "2" 直接 exit(1)，表象"点击图标秒退"）；unlink 后文件不存在时
    // 应用按"非 launcher 启动"路径立即放行，语义安全。
    gate_unlink();
    if !gate_write(0) {
        return false;
    }
    // 重读校验：确认落盘内容确为 '0'（写"成功"不代表内容一致）。
    let mut buf = [0u8; 4];
    let n = unsafe { read_file(LAUNCH_GATE, &mut buf) };
    n == 1 && buf[0] == b'0'
}

/// 复位启动门控为放行态（launcher 启动时调用）。
pub fn gate_reset() {
    gate_release();
}

/// 读取 pkg 已安装记录中的 per-app UID（/var/lib/pkg/installed/<key> 的 uid= 字段）；
/// 无记录（系统内置应用）或 uid=0（旧版安装）回退 APP_UID。
fn app_uid(key: &str) -> usize {
    let mut path = [0u8; 96];
    let mut n = 0usize;
    for &c in b"/var/lib/pkg/installed/" {
        if n < path.len() - 1 {
            path[n] = c;
            n += 1;
        }
    }
    for &c in key.as_bytes() {
        if n < path.len() - 1 {
            path[n] = c;
            n += 1;
        }
    }
    path[n] = 0;
    let pstr = core::str::from_utf8(&path[..n]).unwrap_or("");
    let mut buf = [0u8; 512];
    let rlen = unsafe { read_file(pstr, &mut buf) };
    if rlen <= 0 {
        return APP_UID;
    }
    let rlen = rlen as usize;
    let mut i = 0usize;
    while i < rlen {
        let mut j = i;
        while j < rlen && buf[j] != b'\n' {
            j += 1;
        }
        let line = &buf[i..j];
        if line.len() > 4 && &line[..4] == b"uid=" {
            let mut v: usize = 0;
            for &c in &line[4..] {
                if !c.is_ascii_digit() {
                    break;
                }
                // 饱和累积：超长数字行回绕出错误 uid（debug 下整数溢出 panic）。
                v = v.saturating_mul(10).saturating_add((c - b'0') as usize);
            }
            return if v == 0 { APP_UID } else { v };
        }
        i = j + 1;
    }
    APP_UID
}

/// fork + execve 启动应用子进程（不阻塞）；动画期间应用并行加载。
/// 父进程返回子 pid；失败返回负值。
fn launch_child(app_idx: usize) -> isize {
    if app_idx >= APPS.len() {
        return -1;
    }
    let app = &APPS[app_idx];
    print("[launcher] 启动 ");
    print(app.name);
    print("\n");

    unsafe {
        let pid = fork();
        if pid == 0 {
            // 子进程：先降权到 per-app UID（Android 式沙箱），再 execve 应用。
            // pkg 安装的应用使用分配到的独立 UID；系统内置应用回退 APP_UID。
            // 降权失败（返回非 0）不能静默以 root 跑应用：直接退出。
            let uid = app_uid(app.key);
            if syscall1(SYS_SETUID, uid) != 0 {
                print("[launcher] setuid(");
                print_dec(uid as u32);
                print(") 失败，拒绝以 root 运行\n");
                exit(126);
            }
            // 启动加速：3s 一次性调度 deadline（相对 µs），覆盖字体加载与
            // 首帧渲染的启动窗口；首帧通常远早于 3s，deadline 到期自动清除。
            syscall1(SYS_KUNX_SCHED_SET_DEADLINE, LAUNCH_BOOST_US);
            let mut argv = [0usize; 2];
            let mut p = [0u8; PATH_CAP];
            let len = app.path.len().min(PATH_CAP - 1);
            p[..len].copy_from_slice(&app.path.as_bytes()[..len]);
            p[len] = 0;
            argv[0] = p.as_ptr() as usize;
            argv[1] = 0;
            execve(&p[..len + 1], argv.as_ptr(), core::ptr::null());
            // execve 失败：子进程直接退出。
            print("[launcher] execve ");
            print(app.path);
            print(" 失败\n");
            exit(127);
        }
        pid
    }
}

/// 触摸按下命中图标 → 开始按压反馈。
pub fn start_press(app_idx: usize) {
    unsafe {
        // PressOut（上一次按压回弹未收敛）可被打断：否则滑动结束后立刻点
        // 图标，按下与抬起都被静默吞掉（第一次点击无任何反应）。
        if MOTION.phase != Phase::Idle && MOTION.phase != Phase::PressOut {
            return;
        }
        MOTION.phase = Phase::PressIn;
        MOTION.app = app_idx;
        MOTION.press_app = app_idx;
        MOTION.t0 = now_us();
        MOTION.press_scale = SCALE_1;
        NEEDS_REDRAW = true;
    }
}

/// 取消按压（滑动/长按/点空）→ 弹簧回弹。
pub fn cancel_press() {
    unsafe {
        if MOTION.phase != Phase::PressIn || MOTION.press_app == usize::MAX {
            return;
        }
        MOTION.press_spring = Spring::ios(0.65, 0.0);
        MOTION.press_spring.pos = MOTION.press_scale as f32;
        MOTION.press_spring.target = SCALE_1 as f32;
        MOTION.press_t = now_us();
        MOTION.phase = Phase::PressOut;
        NEEDS_REDRAW = true;
    }
}

/// 开始应用启动转场：快照桌面 → 立即 fork+execve 并行加载应用（动画期间
/// 应用完成 ELF/字体加载）→ 图标方块弹簧放大覆盖屏幕 + 白闪 → 动画完成时
/// 放行应用绘制首帧；应用未就绪则显示占位页。
pub fn start_launch(app_idx: usize) {
    unsafe {
        // 连续启动打断（ColorOS 极光引擎）：返回转场中途点击新图标，不等
        // 300ms 播完 —— 以当前中途帧为背景快照，无缝切换为新应用的启动
        // 转场（快照时 MOTION.phase 仍是 Return，render_frame 走 zoom_blit
        // 路径定格返回中的画面）。Resume 不开放打断：被恢复应用须保持
        // SIGSTOP 且 launcher 侧单槽状态无法同时表达"挂起恢复 + 新启动"。
        let interrupt_return = MOTION.phase == Phase::Return;
        if MOTION.phase != Phase::Idle
            && MOTION.phase != Phase::PressIn
            && MOTION.phase != Phase::PressOut
            && !interrupt_return
        {
            return;
        }
        // 一次性取走转场锚点（最近任务卡片起点；None = 图标原位）。
        let anchor = super::take_launch_anchor();
        // 生命周期：应用存活（后台/挂起）→ 恢复而非重新启动。
        if super::app_life::app_alive(app_idx) {
            if super::app_life::resume_app_at(app_idx, anchor) {
                return;
            }
        }
        // 门控置 0（等待）：应用在 font_init 后轮询此文件，动画完成前不绘制。
        // unlink 重建 + 重读校验都成功才 fork：门控不可用时应用可能读到残留
        // 内容（如上次取消残留的 "2"）直接 exit(1)，表象"点击图标秒退无提示"。
        // 门控/fork 失败路径必须在动 Return 挂起状态与 FRAME_BUF 之前返回 ——
        // 打断路径下返回转场要以原快照原样继续播完（完成后照常开最近任务）。
        if !gate_prepare_wait() {
            print("[launcher] 启动门控写入失败，放弃启动\n");
            gate_release(); // 尽力恢复放行态（无人等待）
            show_toast(txt("Failed to launch app", "应用启动失败"));
            // 恢复图标按压态：否则图标永久卡在缩小态。
            cancel_press();
            return;
        }
        // 立即 fork + execve，应用加载（ELF + 字体）与启动动画并行进行。
        let pid = launch_child(app_idx);
        if pid <= 0 {
            print("[launcher] fork 失败，跳过启动\n");
            gate_release(); // 无人等待，直接放行
            show_toast(txt("Failed to launch app", "应用启动失败"));
            // 恢复图标按压态：否则图标永久卡在缩小态（此前只提示不清理）。
            cancel_press();
            return;
        }
        if interrupt_return {
            // Return 的完成回调不会再触发，清掉其挂起状态（BG_RECENTS
            // 残留会在下次返回完成时误开最近任务）；已计入的返回帧不属于
            // 即将开始的启动转场，一并清零。
            super::app_life::return_interrupted();
            TRANS_FRAMES = 0;
            print("[motion] 返回转场被新启动打断（连续启动）\n");
        }
        // 快照当前桌面（此刻仍为普通桌面渲染路径；打断路径 MOTION.phase 仍
        // 是 Return，render_frame 走桌面预渲染合成路径定格返回中的画面）
        render_frame();
        core::ptr::copy_nonoverlapping(
            BACK_BUF.as_ptr(),
            FRAME_BUF.as_mut_ptr(),
            DISPLAY_W * DISPLAY_H,
        );
        // 快照已定格，Return 阶段结束：退出桌面预渲染模式（新启动转场
        // 使用自己的快照路径，不受 Return 标志影响）。
        DESK_PRERENDERED = false;
        // 快照压暗一次化：launch_blit 每帧的「压暗桌面快照」背景在这里一次
        // 完成（见 darken_frame_buf 注释），转场各帧只做 memcpy。
        darken_frame_buf();
        // 预渲染应用图标瓦片（32x32 圆角图标），供启动动画放大使用。
        let app = &APPS[app_idx];
        TILE_MODE = true;
        draw_app_icon(app, 0, 0, ICON_SIZE);
        TILE_MODE = false;
        LAUNCH_PID = pid;
        LAUNCH_IDX = app_idx;
        // 生命周期：登记应用（lifd 建立注册表）并置启动中状态。
        super::app_life::lifd_register(app_idx, pid);
        super::app_life::set_launching(app_idx, pid);
        // 放大起点：锚点（最近任务卡片）优先，否则图标原位。
        let (cx, cy) = anchor.unwrap_or_else(|| icon_center(app_idx));
        // 目标缩放：图标方块边长放大到覆盖全屏所需的值（以图标中心为锚点）。
        let need = cx
            .max(DISPLAY_W as i32 - cx)
            .max(cy)
            .max(DISPLAY_H as i32 - cy);
        let cover_px = (need * 2 + 16) as u32; // 覆盖全屏的边长（px）
        let target = (cover_px * SCALE_1 / ICON_SIZE as u32).max(SCALE_1 + 1);
        MOTION.phase = Phase::Launch;
        MOTION.app = app_idx;
        MOTION.cx = cx;
        MOTION.cy = cy;
        MOTION.launch_target = target;
        // spring(300ms, bounce≈0.07)：从按压尺寸起跳放大到覆盖全屏（ColorOS）。
        // 打断路径没有本轮按压反馈（start_press 在 Return 期间被吞），
        // press_scale 是上一次按压的陈值，从原尺寸起跳。
        MOTION.launch_spring = Spring::launch();
        if interrupt_return {
            MOTION.press_scale = SCALE_1;
        }
        MOTION.launch_spring.pos = MOTION.press_scale as f32;
        MOTION.launch_spring.target = target as f32;
        MOTION.launch_t = now_us();
        // t0 必须在本次启动时刻刷新（对照 app_life::resume_app 的写法）：
        // t0 若停留在按压落指时刻，点按到 start_launch 间隔 >300ms 时（文件夹
        // 关闭动画后启动、最近任务卡片点击等）motion_advance 的 LAUNCH_DUR_US
        // 完成判定瞬间成立，动画被跳过、应用必然未就绪而直接进占位逻辑。
        MOTION.t0 = now_us();
        MOTION.cur_scale = MOTION.press_scale;
        MOTION.cur_fade = 0;
        MOTION.press_app = usize::MAX;
        RECENT_APP = app_idx;
        NEEDS_REDRAW = true;
    }
}

/// 开始返回转场：快照当前显存（应用最后一帧）→ 从全屏缩回图标原位。
/// 转场开始时就地预渲染桌面（ColorOS 极光引擎「转场与目标帧并行绘制」）：
/// 转场期间桌面是静态的，先把它画进 BACK_BUF 并存档 DESK_BUF，各帧只做
/// 「桌面 + 缩回应用帧」合成；结束时整体恢复，不再付整屏重绘尾巴。
pub fn start_return(app_idx: usize) {
    unsafe {
        if !FB.is_null() {
            let w = FB_W.min(DISPLAY_W);
            let h = FB_H.min(DISPLAY_H);
            for y in 0..h {
                core::ptr::copy_nonoverlapping(
                    FB.add(y * FB_STRIDE_PX),
                    FRAME_BUF.as_mut_ptr().add(y * DISPLAY_W),
                    w,
                );
            }
        }
        // 壁纸偏好同步挪到转场开始（原在 return_animation_done —— 转场后的
        // 关键路径上）；若调色板变化触发了淡入，直接结算到终态：预渲染是
        // 静态底图，淡入无从插帧。
        wallpaper_sync_from_prefs();
        WP_FADE_ACTIVE = false;
        draw_desktop_full();
        DESK_BUF.copy_from_slice(&BACK_BUF);
        DESK_PRERENDERED = true;
        // 恢复基准 = 全屏：首帧先把整幅桌面恢复进 BACK_BUF 再合成应用帧。
        DESK_RET_PREV = [0, 0, DISPLAY_W as i32 - 1, DISPLAY_H as i32 - 1];
        let (hx, hy) = icon_center(app_idx);
        MOTION.phase = Phase::Return;
        MOTION.t0 = now_us();
        MOTION.cx = hx;
        MOTION.cy = hy;
        MOTION.cur_scale = SCALE_1;
        MOTION.cur_fade = 0;
        NEEDS_REDRAW = true;
    }
}

/// 转场帧统计：统计每次启动/返回转场的实际渲染帧数，转场结束时打印一行。
static mut TRANS_FRAMES: u32 = 0;

/// 指定应用是否正在打断动画（飞出/飞回）中 —— 桌面上该图标原位被隐藏，
/// 由飞行动画绘制在飞行位置上。
pub fn app_is_flying(app_idx: usize) -> bool {
    unsafe {
        (MOTION.phase == Phase::FlyAway || MOTION.phase == Phase::FlyBack) && MOTION.app == app_idx
    }
}

/// 打断启动动画（ColorOS 取消启动风格）：应用图标从桌面原位沿滑动方向飞出，
/// 同时图标从中间撕成上下两半向对侧分离（撕裂间距随速度增大），
/// 1 秒后用弹簧飞回原位、两半重新合拢。
///
/// `dx/dy` 为上划手势的位移（px），`dur_ms` 为其持续时间；速度 = 位移/时长。
pub fn start_fly_away(app_idx: usize, dx: i32, dy: i32, dur_ms: u64) {
    unsafe {
        if MOTION.phase != Phase::Launch {
            return;
        }
        let (hx, hy) = icon_center(app_idx);
        let len = fsqrt((dx * dx + dy * dy) as f32).max(1.0);
        // 速度（px/ms）：越快 → 飞得越远、撕得越开
        let speed = len / (dur_ms.max(1) as f32);
        let raw = 120.0 + speed * 200.0;
        let dist = if raw < 150.0 {
            150.0
        } else if raw > 540.0 {
            540.0
        } else {
            raw
        };
        // 撕裂最大间距：慢滑 10px，快滑最多 30px（ColorOS 打断节奏）。
        let split_max = (10.0 + speed * 20.0).min(30.0) as i32;
        MOTION.phase = Phase::FlyAway;
        MOTION.fly_home_x = hx;
        MOTION.fly_home_y = hy;
        MOTION.fly_dir_x = dx as f32 / len;
        MOTION.fly_dir_y = dy as f32 / len;
        MOTION.fly_dist = dist;
        MOTION.fly_t0 = now_us();
        MOTION.fly_x = hx;
        MOTION.fly_y = hy;
        MOTION.fly_split = 0;
        MOTION.fly_split_max = split_max;
        MOTION.press_app = usize::MAX;
        // 取消启动：门控写 "2" 通知已 fork 的应用子进程退出（应用尚未绘制）。
        gate_write(2);
        let pid = LAUNCH_PID;
        LAUNCH_PID = -1;
        if pid > 0 {
            // 应用可能还在 execve / 字体加载（秒级），同步 waitpid 会冻结主循环、
            // 打断动画直接卡死：只登记待回收 pid，由主循环每帧经
            // poll_cancel_reap（wait4 WNOHANG）非阻塞回收。
            CANCEL_PID = pid;
            CANCEL_IDX = app_idx;
        }
        print("[motion] 启动中断，图标飞出 ");
        print_dec((dist + 0.5) as u32);
        print("px（速度=");
        print_dec((speed * 10.0) as u32);
        print(" ×0.1px/ms）\n");
        NEEDS_REDRAW = true;
    }
}

/// 非阻塞回收被取消启动的应用子进程（start_fly_away 登记的 CANCEL_PID）。
/// 由主循环每帧调用；应用读到门控 "2" 后自行 exit，退出前这里每帧空转一次
/// wait4(WNOHANG)（微秒级）。返回 true 表示本轮完成回收（lifd 登记已清理）。
pub fn poll_cancel_reap() -> bool {
    unsafe {
        let pid = CANCEL_PID;
        if pid <= 0 {
            return false;
        }
        let mut status = 0i32;
        // ABI：options 在第 3 参数（x2），rusage 在第 4 参数（x3）
        let r = syscall4(
            SYS_WAIT4,
            pid as usize,
            &mut status as *mut i32 as usize,
            WNOHANG,
            0,
        ) as isize;
        if r == 0 {
            return false; // 子进程仍在 execve/字体加载，下帧再试
        }
        CANCEL_PID = -1;
        let idx = CANCEL_IDX;
        if r != pid {
            // wait4 出错（子进程已被其它路径回收等）：仍需清理登记，避免残留。
            print("[launcher] 启动已取消，waitpid(pid=");
            print_dec(pid as u32);
            print(") 异常\n");
        } else {
            print("[launcher] 启动已取消，应用 ");
            print(APPS[idx].name);
            print(" 已退出\n");
        }
        // 生命周期：报告退出并复位状态（lifd 移除注册）
        super::app_life::cancel_launch(idx, pid);
        true
    }
}

/// 推进运动状态机；返回是否仍在动画中（需要持续重绘）。
pub fn motion_advance(now: u64) -> bool {
    unsafe {
        let m = &mut MOTION;
        if m.phase == Phase::Launch || m.phase == Phase::Return || m.phase == Phase::Resume {
            TRANS_FRAMES = TRANS_FRAMES.wrapping_add(1);
        }
        match m.phase {
            Phase::Idle => false,
            Phase::PressIn => {
                let t = now.saturating_sub(m.t0) as f32 / (PRESS_MS * 1000) as f32;
                let p = if t >= 1.0 {
                    1.0
                } else {
                    Easing::FastOutSlowIn.sample(t)
                };
                m.press_scale =
                    (SCALE_1 as f32 + (PRESS_SCALE as f32 - SCALE_1 as f32) * p + 0.5) as u32;
                true // 保持按压态直到抬起
            }
            Phase::PressOut => {
                // 真实 dt（µs）推进弹簧，回弹速度与帧间隔一致
                let dt_ms = now.saturating_sub(m.press_t) as f32 / 1000.0;
                m.press_t = now;
                if m.press_spring.step_ms(dt_ms) {
                    m.phase = Phase::Idle;
                    m.press_app = usize::MAX;
                    m.press_scale = SCALE_1;
                    return false;
                }
                m.press_scale = m.press_spring.pos as u32;
                true
            }
            Phase::Launch => {
                // spring(300ms, bounce≈0.07) 真实 dt 积分；
                // 300ms 弹簧时长到点即完成（弹簧已收敛到 1% 内，图标已覆盖全屏）。
                let dt_ms = now.saturating_sub(m.launch_t) as f32 / 1000.0;
                m.launch_t = now;
                let settled = m.launch_spring.step_ms(dt_ms);
                if settled || now.saturating_sub(m.t0) >= LAUNCH_DUR_US {
                    print(" [motion] 启动过渡：");
                    print_dec(TRANS_FRAMES);
                    print(" frames\n");
                    TRANS_FRAMES = 0;
                    m.phase = Phase::Idle;
                    m.cur_scale = m.launch_target;
                    m.cur_fade = 255;
                    let app = m.app;
                    // 放行应用绘制首帧（应用在 start_launch 已 fork+execve，
                    // 动画期间完成字体加载）。放行后不再阻塞轮询应用首帧
                    // （原 app_ready_within 最多卡主循环 60ms，恰在输入最敏感
                    // 时刻）：占位页立即兜底（ColorOS starting window 语义），
                    // 应用首帧就绪后自然覆盖占位页。
                    // ★ 首帧基线必须在放行**之前**采样（自比较恒假守卫使
                    // 占位页无条件覆盖应用首帧）。
                    let baseline = fb_sample();
                    gate_release();
                    let pid = LAUNCH_PID;
                    LAUNCH_PID = -1;
                    draw_placeholder(app, &baseline);
                    // 进入前台管理态（非阻塞；应用退出/后台化由 app_life 处理）
                    super::app_life::launch_complete(app, pid);
                    // 返回 false：本帧不应再触发主循环重绘 —— phase 已置 Idle，
                    // 若返回 true，主循环当帧 render_frame()+flip() 会把整个桌面
                    // 画上来，覆盖刚提交的占位页/应用首帧（应用慢启动期间屏幕
                    // 停在桌面，表象"点不开"）。占位页已由 draw_placeholder 直接
                    // flip 提交；应用首帧已在显存。Return/Resume 分支不受影响
                    // （它们完成后本就需要主循环画一次桌面/保持前台）。
                    return false;
                }
                m.cur_scale = m.launch_spring.pos as u32;
                // 白闪在动画后段才开始（衔接应用首帧，避免过早遮住图标）；
                // Emphasized（慢起-快中-缓收）替代线性：起闪更缓，减白闪突兀感。
                let span = (m.launch_target - SCALE_1).max(1) as f32;
                let prog = ((m.launch_spring.pos - SCALE_1 as f32) / span).clamp(0.0, 1.0);
                let fade_p = ((prog - 0.55) / 0.45).clamp(0.0, 1.0);
                m.cur_fade = (255.0 * Easing::Emphasized.sample(fade_p)) as u32;
                // squash & stretch：放大途中纵向边长 1.03 倍瞬时超前，随进度
                // 收敛回等比（落位 prog=1 时精确覆盖全屏）。
                m.cur_stretch = (1000.0 + 30.0 * (1.0 - prog)) as u32;
                true
            }
            Phase::Resume => {
                // 恢复转场：应用末帧快照（FRAME_BUF）从图标原位放大到全屏；
                // 动画期间应用仍 SIGSTOP，完成时通知 lifd 恢复（SIGCONT）。
                let dt_ms = now.saturating_sub(m.launch_t) as f32 / 1000.0;
                m.launch_t = now;
                let settled = m.launch_spring.step_ms(dt_ms);
                if settled || now.saturating_sub(m.t0) >= LAUNCH_DUR_US {
                    m.phase = Phase::Idle;
                    m.cur_scale = m.launch_target;
                    m.cur_fade = 255;
                    super::app_life::resume_animation_done();
                    NEEDS_REDRAW = true;
                    return false;
                }
                m.cur_scale = m.launch_spring.pos as u32;
                m.cur_fade = 0;
                true
            }
            Phase::Return => {
                let t = now.saturating_sub(m.t0) as f32 / (RETURN_MS * 1000) as f32;
                if t >= 1.0 {
                    print(" [motion] 返回过渡：");
                    print_dec(TRANS_FRAMES);
                    print(" frames\n");
                    TRANS_FRAMES = 0;
                    m.phase = Phase::Idle;
                    m.cur_scale = RETURN_END_SCALE;
                    m.cur_fade = 255;
                    // 桌面预渲染模式：BACK_BUF 整体恢复为纯桌面并直接提交，
                    // 不置 NEEDS_REDRAW —— 跳过转场后的整屏 draw_desktop_full
                    // （TCG 下 50~150ms），退出尾巴归零（ColorOS 无缝回桌面）。
                    if DESK_PRERENDERED {
                        let total = DISPLAY_W * DISPLAY_H;
                        BACK_BUF[..total].copy_from_slice(&DESK_BUF);
                        flip();
                        draw_cursor_overlay();
                        DESK_PRERENDERED = false;
                    } else {
                        NEEDS_REDRAW = true;
                    }
                    // 生命周期：返回动画完成（BG 原因为 RECENTS 时打开最近任务）
                    super::app_life::return_animation_done();
                    return false;
                }
                // ease-out：从全屏（1:1 应用末帧）缩回图标原位，白闪淡入收尾
                let p = Easing::EaseOut.sample(t);
                m.cur_scale =
                    (SCALE_1 as f32 + (RETURN_END_SCALE as f32 - SCALE_1 as f32) * p) as u32;
                m.cur_fade = (255.0 * p) as u32;
                true
            }
            Phase::FlyAway => {
                // 前 0.45s 沿滑动方向飞出（FastOutSlowIn）并同步撕裂（上下两半
                // 分离，撕裂间距随飞出进度增长），随后悬停至 1s，之后转入
                // 弹簧飞回合拢（总计约 1s + 回弹）。
                let t = now.saturating_sub(m.fly_t0) as f32 / 1_000_000.0;
                if t >= 1.0 {
                    m.phase = Phase::FlyBack;
                    m.fly_spring = Spring::ios(0.5, 0.0);
                    m.fly_spring.pos = m.fly_dist;
                    m.fly_spring.target = 0.0;
                    m.fly_t0 = now;
                    return true;
                }
                let out = (t / 0.45).min(1.0);
                let e = Easing::FastOutSlowIn.sample(out);
                let d = m.fly_dist * e;
                m.fly_x = m.fly_home_x + (m.fly_dir_x * d) as i32;
                m.fly_y = m.fly_home_y + (m.fly_dir_y * d) as i32;
                // 撕裂间距跟随飞出进度（ColorOS 打断：撕开 → 悬停保持 → 合拢）。
                m.fly_split = (m.fly_split_max as f32 * e) as i32;
                true
            }
            Phase::FlyBack => {
                // 弹簧从远处回弹到原位（轻微过冲后稳定），撕裂间距同步收回。
                let dt_ms = now.saturating_sub(m.fly_t0) as f32 / 1000.0;
                m.fly_t0 = now;
                if m.fly_spring.step_ms(dt_ms) {
                    m.phase = Phase::Idle;
                    m.fly_x = m.fly_home_x;
                    m.fly_y = m.fly_home_y;
                    m.fly_split = 0;
                    print("[motion] 图标返回主屏\n");
                    NEEDS_REDRAW = true;
                    return false;
                }
                let d = m.fly_spring.pos;
                m.fly_x = m.fly_home_x + (m.fly_dir_x * d) as i32;
                m.fly_y = m.fly_home_y + (m.fly_dir_y * d) as i32;
                // 撕裂间距与回弹位置成正比：飞回越多合拢越多。
                m.fly_split = (m.fly_split_max as f32 * d / m.fly_dist.max(1.0)) as i32;
                true
            }
        }
    }
}

/// 采样显存 12 个点（动画末帧基线；含四角 + 四边中点 + 四内点）。
fn fb_sample() -> [u32; 12] {
    unsafe {
        let w = FB_W.max(1) as i32;
        let h = FB_H.max(1) as i32;
        let pts: [(i32, i32); 12] = [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, 0),
            (w / 2, h - 1),
            (0, h / 2),
            (w - 1, h / 2),
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ];
        let mut s = [0u32; 12];
        if !FB.is_null() {
            for i in 0..12 {
                let (x, y) = pts[i];
                s[i] = *FB.add((y as usize) * FB_STRIDE_PX + x as usize);
            }
        }
        s
    }
}

/// 当前显存与基线样本是否一致（一致 = 应用尚未绘制首帧）。
fn fb_matches(sample: &[u32; 12]) -> bool {
    unsafe {
        if FB.is_null() {
            return true;
        }
        let w = FB_W.max(1) as i32;
        let h = FB_H.max(1) as i32;
        let pts: [(i32, i32); 12] = [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, 0),
            (w / 2, h - 1),
            (0, h / 2),
            (w - 1, h / 2),
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ];
        for i in 0..12 {
            let (x, y) = pts[i];
            if *FB.add((y as usize) * FB_STRIDE_PX + x as usize) != sample[i] {
                return false;
            }
        }
        true
    }
}

/// 占位页居中图标边长（px）。
const PLACE_ICON_S: i32 = 96;

/// 占位页面（应用主题色背景 + 居中图标）：应用 350ms 内未就绪时显示，
/// 应用首帧绘制到显存后自然覆盖。`baseline` 是放行应用**之前**采的显存
/// 基线（此刻显存必然还是启动动画末帧 = 基线本身；与基线一致 = 应用
/// 尚未画首帧，允许覆盖）。
///
/// 绘制完成后武装骨架屏呼吸（B10）：应用首帧未就绪的等待期内，图标下方
/// 骨架块以 1.2s 三角波呼吸（ColorOS 加载占位风格），由主循环前台分支
/// 经 `placeholder_breathe` 逐帧推进。
fn draw_placeholder(app_idx: usize, baseline: &[u32; 12]) {
    unsafe {
        if !fb_matches(baseline) {
            return; // 应用已绘制首帧，不再覆盖
        }
        let app = &APPS[app_idx];
        for i in 0..DISPLAY_W * DISPLAY_H {
            BACK_BUF[i] = app.color;
        }
        let s = PLACE_ICON_S;
        let ox = (DISPLAY_W as i32 - s) / 2;
        let oy = (DISPLAY_H as i32 - s) / 2;
        draw_app_icon(app, ox, oy, s);
        flip();
        // B10：武装呼吸态。基线在占位页上屏**之后**采样（12 个采样点均在
        // 图标与骨架区之外），应用首帧写入显存后采样偏离即自动停止。
        PLACE_APP = app_idx;
        PLACE_T0 = now_us();
        PLACE_LAST = PLACE_T0;
        PLACE_BASE = fb_sample();
        PLACE_ON = true;
    }
}

// ==================== B10：占位页骨架屏呼吸 ====================

/// 骨架屏呼吸状态（应用首帧到达或超时前由主循环前台分支推进）。
static mut PLACE_ON: bool = false;
static mut PLACE_T0: u64 = 0;
static mut PLACE_LAST: u64 = 0;
static mut PLACE_APP: usize = usize::MAX;
static mut PLACE_BASE: [u32; 12] = [0; 12];

/// 呼吸周期（µs）：与锁屏充电光晕同款 1.2s 三角波。
const PLACE_PERIOD_US: u64 = 1_200_000;
/// 呼吸帧节流（µs）：1.2s 周期下 30fps 足够平滑。
const PLACE_FRAME_US: u64 = 33_000;
/// 最长等待（µs）：超过后退回静态占位 —— 应用首帧始终未到（加载失败/
/// 极慢）时不再与显存竞争，也让呼吸状态必然收敛。
const PLACE_MAX_US: u64 = 6_000_000;
/// 骨架块呼吸 alpha 范围（主题色底上整块混合，颜色每帧算一次）。
const PLACE_SK_MIN: u32 = 60;
const PLACE_SK_MAX: u32 = 150;
/// 骨架块半宽与总高（px）：图标下方三行内容占位（标题 + 两行文本）。
const PLACE_SK_HALF_W: i32 = 90;
const PLACE_SK_H: i32 = 60;

/// 停止占位页呼吸（应用退出/后台化/恢复转场起点由 app_life 调用；
/// 重复调用无害）。不清显存 —— 画面停在当前占位帧，由后续路径
/// （返回动画 / 应用首帧）自然接管。
pub fn placeholder_stop() {
    unsafe {
        PLACE_ON = false;
        PLACE_APP = usize::MAX;
    }
}

/// 占位页骨架屏呼吸帧（B10）：主循环前台等待分支每帧调用（内部 33ms
/// 节流）。骨架块为纯色矩形 —— 在均匀主题色底上按帧 alpha 整块混合成
/// 单色后 `fill_rect`（O(面积) memset，无逐像素混合），仅骨架区域脏矩形
/// flip。应用首帧写入显存（基线 12 采样点偏离）或超过 6s 后自动停止。
/// 返回 true 表示仍在呼吸。
pub fn placeholder_breathe(now: u64) -> bool {
    unsafe {
        if !PLACE_ON {
            return false;
        }
        if now.saturating_sub(PLACE_LAST) < PLACE_FRAME_US {
            return true;
        }
        PLACE_LAST = now;
        if PLACE_APP >= APPS.len()
            || !fb_matches(&PLACE_BASE)
            || now.saturating_sub(PLACE_T0) > PLACE_MAX_US
        {
            placeholder_stop();
            return false;
        }
        let app = &APPS[PLACE_APP];
        // 三角波呼吸（0..1..0）：骨架块亮度 60..150 混合白色。
        let br = super::motion::triangle(now.saturating_sub(PLACE_T0), PLACE_PERIOD_US);
        let a = PLACE_SK_MIN + ((PLACE_SK_MAX - PLACE_SK_MIN) as f32 * br) as u32;
        let sk = blend(app.color, 0x00_FF_FF_FF, a.min(255));
        let cx = DISPLAY_W as i32 / 2;
        let y0 = (DISPLAY_H as i32 - PLACE_ICON_S) / 2 + PLACE_ICON_S + 28;
        // 三行骨架：标题宽块 + 两行文本块（ColorOS 加载占位层次）。
        fill_rect(cx - PLACE_SK_HALF_W, y0, PLACE_SK_HALF_W * 2, 12, sk);
        fill_rect(cx - PLACE_SK_HALF_W, y0 + 20, PLACE_SK_HALF_W * 2, 12, sk);
        fill_rect(cx - PLACE_SK_HALF_W, y0 + 40, 120, 12, sk);
        mark_dirty(
            cx - PLACE_SK_HALF_W,
            y0,
            (PLACE_SK_HALF_W * 2) as u32,
            PLACE_SK_H as u32,
        );
        flip();
        true
    }
}
