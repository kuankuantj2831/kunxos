//! 共享粒子系统：定长粒子池 + 「列栈」沙堆模型（亮度滑块 / 星辰开关复用）。
//!
//! 设计要点（Cortex-A72 单核 60fps 预算）：
//! - 粒子池为 `[Particle; 500]`，无堆；空槽由逐帧紧致回收。
//! - 已沉降的沙子不参与逐像素势场：每列只维护堆顶高度 `top[col]`，
//!   沉降、漏沙、坡度翻滚都是 O(cols) 的整数表操作。
//! - 「漏沙」= 取走某列最底颗粒 → 其余整列下移一格（沙漏式排空），
//!   被放行的颗粒转 `Leak` 在半空渐隐蒸发。
//! - 泄漏 / 散落速度与外部滑动速度联动：控件通过 `add_pour / add_drain`
//!   传入「本帧要倒 / 漏几颗」的预算。
//! - `compact()` 在搬运活粒后必须把源槽清空，否则旧槽残留的
//!   `active=true` 会被下一帧反复复制，形成尾部幽灵副本无限增殖。

use crate::{Canvas, XorShift32};

/// 最大粒子数（单控件硬上限，任务规格：≤500）。
pub const MAX_PARTICLES: usize = 500;
/// 列栈最大列数（格宽 ≥3px 时对应 576px 轨道，够大）。
pub const MAX_COLS: usize = 192;
/// 列栈最大行数（128 行 * 3px = 384px 轨道高）。
pub const MAX_ROWS: u16 = 128;
/// 重力加速度（px/s²）。
const GRAVITY: f32 = 1500.0;
/// 倒沙队列上限（单帧灌入再多也只缓冲这么多；防止缺口欠账无限积累）。
const POUR_QUEUE_MAX: u32 = 360;

/// 粒子形态。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ParticleKind {
    /// 自由下落、等待沉降的沙粒。
    Falling,
    /// 已落入列栈的沙粒（一直存活，直到被漏沙放行）。
    Resting,
    /// 漏出 / 溢出 / 散落的沙粒：继续下落并在半空渐隐蒸发。
    Leak,
    /// 短命星点（光晕氛围，无重力）。
    Spark,
}

/// 单个粒子。
#[derive(Clone, Copy)]
pub struct Particle {
    pub active: bool,
    pub kind: ParticleKind,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// 剩余寿命（秒）；`Leak/Spark` 消耗，`Resting` 用 f32::MAX。
    pub life: f32,
    pub max_life: f32,
    /// 绘制半径（px）。
    pub size: f32,
    pub color: u32,
    /// 发光强度 0..=255（additive 径向光晕）。
    pub glow: u8,
    /// 沉降格（col, row）；自由态为 -1。
    pub col: i32,
    pub row: i32,
}

impl Particle {
    /// 空槽（const 构造用）。
    pub const fn dead() -> Particle {
        Particle {
            active: false,
            kind: ParticleKind::Leak,
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            life: 0.0,
            max_life: 1.0,
            size: 1.0,
            color: 0,
            glow: 0,
            col: -1,
            row: -1,
        }
    }
}

/// 沙堆网格 + 粒子池。
pub struct ParticleSystem {
    pub pool: [Particle; MAX_PARTICLES],
    /// 当前有效粒子数（0..=500，逐帧紧致）。
    pub len: usize,
    pub rng: XorShift32,

    // —— 列栈网格（屏幕坐标，row=0 在堆底） ——
    /// 沙堆容器左上角（px）。
    pub gx: i32,
    pub gy: i32,
    pub gw: i32,
    pub gh: i32,
    /// 容器底面 y（px）。
    pub bottom_y: f32,
    /// 每格边长（px）。
    pub cell: f32,
    /// 列数 / 行数。
    pub cols: usize,
    pub rows: usize,
    /// 每列堆高（格数，0..=rows）。
    pub top: [u16; MAX_COLS],
    /// 目标堆高（格数）：`top[col] > target` 的列会被漏沙。
    pub target_rows: u16,
    /// 竖排模式：列沿屏幕 y（自底向上），行沿屏幕 x。
    pub vertical: bool,

    // —— 本帧预算 ——
    /// 待倒沙粒数（由控件按需灌入）。
    pub pour_pending: u32,
    /// 待漏沙粒数（由控件按滑动速度灌入）。
    pub drain_pending: u32,
    /// 漏沙优先方向：true 时从右侧列先漏（开关关闭动画「从右侧漏出」）。
    pub drain_right_first: bool,

    /// 倒沙嘴位置（屏幕坐标；沙粒从上方凭空生成处）。
    pub spout_x: f32,
    pub spout_y: f32,
}

impl ParticleSystem {
    /// 静态构造（可放入 no_std 的 `static mut`）。
    pub const fn new(seed: u32) -> ParticleSystem {
        ParticleSystem {
            pool: [Particle::dead(); MAX_PARTICLES],
            len: 0,
            rng: XorShift32::new(seed),
            gx: 0,
            gy: 0,
            gw: 0,
            gh: 0,
            bottom_y: 0.0,
            cell: 3.0,
            cols: 0,
            rows: 0,
            top: [0; MAX_COLS],
            target_rows: 0,
            vertical: false,
            pour_pending: 0,
            drain_pending: 0,
            drain_right_first: false,
            spout_x: 0.0,
            spout_y: 0.0,
        }
    }

    /// 配置沙堆网格。`cell` 为格边长（建议 3~4px，控制总粒数）。
    /// 首次初始化后调用一次；`reset_pile` 会把已沉降沙清空。
    pub fn setup_grid(&mut self, x: i32, y: i32, w: i32, h: i32, cell: f32, vertical: bool) {
        self.gx = x;
        self.gy = y;
        self.gw = w;
        self.gh = h;
        self.cell = cell.max(2.0);
        self.vertical = vertical;
        self.cols = ((w as f32 / self.cell) as usize).clamp(1, MAX_COLS);
        self.rows = ((h as f32 / self.cell) as usize).min(MAX_ROWS as usize).max(1);
        self.bottom_y = (y + h) as f32;
    }

    /// 清空堆与粒子（控件初始化时调用）。
    pub fn reset_pile(&mut self) {
        for c in 0..MAX_COLS {
            self.top[c] = 0;
        }
        for p in &mut self.pool {
            *p = Particle::dead();
        }
        self.len = 0;
        self.pour_pending = 0;
        self.drain_pending = 0;
    }

    /// 当前生效粒子数（测试 / 性能断言用）。
    pub fn alive(&self) -> usize {
        let mut n = 0;
        for i in 0..self.len {
            if self.pool[i].active {
                n += 1;
            }
        }
        n
    }

    /// 定位一个空槽（扫描空洞 → 追加池尾）。
    fn alloc_slot(&mut self) -> Option<usize> {
        for i in 0..self.len {
            if !self.pool[i].active {
                return Some(i);
            }
        }
        if self.len < MAX_PARTICLES {
            let i = self.len;
            self.pool[i] = Particle::dead();
            self.len += 1;
            Some(i)
        } else {
            None
        }
    }

    /// 屏幕坐标 → 列栈格 (col, row)。网格未启用时返回 None。
    pub fn col_row_at(&self, px: f32, py: f32) -> Option<(i32, i32)> {
        if self.cols == 0 || self.rows == 0 {
            return None;
        }
        let cx = px - self.gx as f32;
        let cbot = self.bottom_y - py;
        if self.vertical {
            Some(((cbot / self.cell) as i32, (cx / self.cell) as i32))
        } else {
            Some(((cx / self.cell) as i32, (cbot / self.cell) as i32))
        }
    }

    /// 把 `(col,row)` 换算成屏幕坐标（供沉降 / 下移 / 漏出位置用）。
    #[inline]
    fn cell_xy(&self, col: i32, row: i32) -> (f32, f32) {
        if self.vertical {
            (
                self.gx as f32 + (row as f32 + 0.5) * self.cell,
                self.bottom_y - (col as f32 + 0.5) * self.cell,
            )
        } else {
            (
                self.gx as f32 + (col as f32 + 0.5) * self.cell,
                self.bottom_y - (row as f32 + 0.5) * self.cell,
            )
        }
    }

    /// 列中心屏幕 x。
    #[inline]
    fn col_center_x(&self, col: i32) -> f32 {
        self.gx as f32 + (col as f32 + 0.5) * self.cell
    }

    // ==================== 预算接口（控件驱动） ====================

    /// 本帧追加 `n` 颗倒沙（从 spout 上方凭空生成），队列封顶防欠账膨胀。
    pub fn add_pour(&mut self, n: u32) {
        self.pour_pending = self.pour_pending.saturating_add(n).min(POUR_QUEUE_MAX);
    }
    /// 本帧追加 `n` 颗漏沙（从堆底放行）。
    pub fn add_drain(&mut self, n: u32) {
        self.drain_pending = self.drain_pending.saturating_add(n);
    }
    /// 把「低于目标」的缺口颗数倒入倒沙预算。
    pub fn fill_needed(&mut self) {
        let d = self.deficit();
        if d > 0 {
            self.add_pour(d as u32);
        }
    }
    /// 把「高于目标」的超量颗数加入漏沙预算，并清空在途倒沙。
    pub fn drain_excess(&mut self) {
        let e = self.excess();
        if e > 0 {
            self.add_drain(e as u32);
        }
        self.pour_pending = 0;
    }
    /// 未达标缺口颗数（倒沙量参考）。
    pub fn deficit(&self) -> i32 {
        let t = self.target_rows as i32;
        let mut d = 0i32;
        for c in 0..self.cols {
            d += (t - self.top[c] as i32).max(0);
        }
        d
    }
    /// 超出目标的颗数（漏沙量参考）。
    pub fn excess(&self) -> i32 {
        let t = self.target_rows as i32;
        let mut e = 0i32;
        for c in 0..self.cols {
            e += (self.top[c] as i32 - t).max(0);
        }
        e
    }

    // ==================== 撒粒（NebulaInput 用） ====================

    /// 从 `(x,y)` 向四周撒 `n` 颗散沙（退格字符散落效果）。
    /// `spread` 为水平散开速度上限。
    pub fn scatter(&mut self, x: f32, y: f32, n: u32, spread: f32) {
        for _ in 0..n {
            let Some(slot) = self.alloc_slot() else {
                return;
            };
            let p = &mut self.pool[slot];
            p.active = true;
            p.kind = ParticleKind::Leak;
            p.x = x + self.rng.jitter() * 4.0;
            p.y = y + self.rng.jitter() * 3.0;
            p.vx = self.rng.jitter() * spread;
            p.vy = -(20.0 + self.rng.frac() * 90.0); // 先向上抛，重力随后拉回
            p.life = 0.5 + self.rng.frac() * 0.5;
            p.max_life = p.life;
            p.size = 1.2 + self.rng.frac() * 0.8;
            p.color = self.rng.sand_color();
            p.glow = 60 + self.rng.below(100) as u8;
            p.col = -1;
            p.row = -1;
        }
    }

    /// 一颗短命星点（光晕氛围）。
    pub fn spark(&mut self, x: f32, y: f32, tint: u32) {
        let Some(slot) = self.alloc_slot() else {
            return;
        };
        let p = &mut self.pool[slot];
        p.active = true;
        p.kind = ParticleKind::Spark;
        p.x = x + self.rng.jitter() * 6.0;
        p.y = y + self.rng.jitter() * 4.0;
        p.vx = self.rng.jitter() * 8.0;
        p.vy = self.rng.jitter() * 8.0;
        p.life = 0.25 + self.rng.frac() * 0.3;
        p.max_life = p.life;
        p.size = 1.0;
        p.color = tint;
        p.glow = 160 + self.rng.below(90) as u8;
        p.col = -1;
        p.row = -1;
    }

    // ==================== 物理推进 ====================

    /// 推进 `dt` 秒：消耗倒沙/漏沙预算 → 积分自由沙 → 沉降判定 →
    /// 漏沙放行 → 蒸发 / 星点 → 堆一致性重建 → 紧致池。
    pub fn update(&mut self, dt: f32) {
        // 1) 倒沙预算（每帧最多 12 颗，保证拖拽时不掉帧）
        let mut pb = self.pour_pending.min(12);
        self.pour_pending = self.pour_pending.saturating_sub(12);
        while pb > 0 {
            self.spawn_pour();
            pb -= 1;
        }

        // 2) 漏沙预算（每帧最多 10 颗）
        let mut db = self.drain_pending.min(10);
        self.drain_pending = self.drain_pending.saturating_sub(10);
        while db > 0 {
            match self.excess_column() {
                Some(c) => self.drain_column(c),
                None => {
                    self.drain_pending = 0;
                    break;
                }
            }
            db -= 1;
        }

        // 3) 自由沙粒积分 + 沉降（就地处理，不增删槽）
        let mut i = 0usize;
        while i < self.len {
            if !self.pool[i].active {
                i += 1;
                continue;
            }
            match self.pool[i].kind {
                ParticleKind::Falling => {
                    let p = &mut self.pool[i];
                    p.vy += GRAVITY * dt;
                    p.x += p.vx * dt;
                    p.y += p.vy * dt;
                    let (cx, cy) = (p.x, p.y);
                    // 沉降判定：格坐标 (c, r)。
                    // - c 出列（左右越出轨道）→ 溢出为漏沙颗粒；
                    // - r 允许暂时「高于」堆顶（从倒沙嘴进入网格的颗粒先悬浮下落），
                    //   一旦 r ≤ 堆顶即沉降；r < 0（已在底部以下）时直接落入堆底。
                    match self.col_row_at(cx, cy) {
                        Some((c, r)) => {
                            if c < 0 || c >= self.cols as i32 {
                                self.drop_to_leak(i);
                            } else if r <= self.top[c as usize] as i32 {
                                self.settle(i, c, r);
                            }
                        }
                        None => self.drop_to_leak(i),
                    }
                }
                ParticleKind::Leak => {
                    let p = &mut self.pool[i];
                    p.vy += GRAVITY * dt;
                    p.x += p.vx * dt;
                    p.y += p.vy * dt;
                    p.life -= dt;
                    if p.life <= 0.0 {
                        p.active = false;
                    }
                }
                ParticleKind::Spark => {
                    let p = &mut self.pool[i];
                    p.x += p.vx * dt;
                    p.y += p.vy * dt;
                    p.life -= dt;
                    if p.life <= 0.0 {
                        p.active = false;
                    }
                }
                ParticleKind::Resting => {}
            }
            i += 1;
        }

        // 4) 堆一致性重建：以池内 Resting 颗粒重算 top，并清掉「行号 ≥ 高度」的
        //    幽灵颗粒（转为漏沙，视作顺坡溢撒）。保证 top==0 ⟺ 该列无沙。
        self.reconcile_pile();

        // 5) 紧致粒子池（搬运活粒并把源槽清空，防止幽灵副本增殖）
        self.compact();
    }

    /// 从 spout 上方生成一颗「凭空出现」的沙粒。
    fn spawn_pour(&mut self) {
        let Some(slot) = self.alloc_slot() else {
            return;
        };
        let p = &mut self.pool[slot];
        p.active = true;
        p.kind = ParticleKind::Falling;
        p.x = self.spout_x + self.rng.jitter() * 8.0;
        p.y = self.spout_y;
        p.vx = self.rng.jitter() * 40.0;
        p.vy = 8.0 + self.rng.frac() * 24.0;
        p.life = f32::MAX;
        p.max_life = 1.0;
        p.size = (self.cell * 0.45).max(1.2);
        p.color = self.rng.sand_color();
        p.glow = 90 + self.rng.below(110) as u8;
        p.col = -1;
        p.row = -1;
    }

    /// 沉降到列附近：在 c±2 内选最低的不满列（形成堆丘），坡度差 >1 时
    /// 滚向低邻（自然休止角）。全部满列时溢出为 `Leak`。
    fn settle(&mut self, i: usize, c: i32, _r: i32) {
        let from = (c - 2).max(0);
        let to = (c + 2).min(self.cols as i32 - 1);
        let mut best = from;
        let mut best_t = self.top[from as usize];
        let mut cc = from + 1;
        while cc <= to {
            let t = self.top[cc as usize];
            if t < best_t {
                best = cc;
                best_t = t;
            }
            cc += 1;
        }
        // 满列 → 溢出
        if best_t >= self.rows as u16 {
            self.drop_to_leak(i);
            return;
        }
        // 休止角翻滚（左右邻低 1 格以上则滚过去）
        if best > 0 && self.top[(best - 1) as usize] as u32 + 1 < best_t as u32 {
            best -= 1;
            best_t = self.top[best as usize];
        } else if best + 1 < self.cols as i32
            && self.top[(best + 1) as usize] as u32 + 1 < best_t as u32
        {
            best += 1;
            best_t = self.top[best as usize];
        }
        let row = best_t;
        if row >= self.rows as u16 {
            self.drop_to_leak(i);
            return;
        }
        let (sx, sy) = self.cell_xy(best, row as i32);
        let p = &mut self.pool[i];
        p.kind = ParticleKind::Resting;
        p.x = sx;
        p.y = sy;
        p.vx = 0.0;
        p.vy = 0.0;
        p.life = f32::MAX;
        p.max_life = 1.0;
        p.size = (self.cell * 0.5).max(1.2);
        p.color = self.rng.sand_color();
        // 部分沙粒带微光，整体呈现星云质感
        p.glow = if self.rng.frac() < 0.22 {
            140 + self.rng.below(90) as u8
        } else {
            30 + self.rng.below(40) as u8
        };
        p.col = best;
        p.row = row as i32;
        self.top[best as usize] = row + 1;
    }

    /// 当前需要漏沙的列（按 `drain_right_first` 方向）。
    fn excess_column(&self) -> Option<usize> {
        let t = self.target_rows;
        if self.drain_right_first {
            let mut c = self.cols;
            while c > 0 {
                c -= 1;
                if self.top[c] > t {
                    return Some(c);
                }
            }
            None
        } else {
            for c in 0..self.cols {
                if self.top[c] > t {
                    return Some(c);
                }
            }
            None
        }
    }

    /// 漏掉列 `c` 的最底一颗：放行为 `Leak`（从底部开口下落），
    /// 该列其余沙粒整体下移一格（沙漏式排空）。
    fn drain_column(&mut self, c: usize) {
        if self.top[c] == 0 {
            return;
        }
        // 找到最底颗粒 (c, 0)
        let mut take = None;
        for i in 0..self.len {
            let p = &self.pool[i];
            if p.active && p.kind == ParticleKind::Resting && p.col == c as i32 && p.row == 0 {
                take = Some(i);
                break;
            }
        }
        // 堆高减一，其余整列下移一格（沙漏式排空）
        self.top[c] -= 1;
        for i in 0..self.len {
            let p = &mut self.pool[i];
            if p.active && p.kind == ParticleKind::Resting && p.col == c as i32 && p.row > 0 {
                p.row -= 1;
                // 就地重算屏幕坐标（与 cell_xy 同几何；这里避免整对象借用）
                let (sx, sy) = if self.vertical {
                    (
                        self.gx as f32 + (p.row as f32 + 0.5) * self.cell,
                        self.bottom_y - (c as f32 + 0.5) * self.cell,
                    )
                } else {
                    (
                        self.gx as f32 + (c as f32 + 0.5) * self.cell,
                        self.bottom_y - (p.row as f32 + 0.5) * self.cell,
                    )
                };
                p.x = sx;
                p.y = sy;
            }
        }
        if let Some(t) = take {
            // 放行：从底部开口漏出
            let (ex_x, ex_y) = if self.vertical {
                (self.bottom_y + self.cell * 0.7, self.col_center_x(c as i32))
            } else {
                (self.col_center_x(c as i32), self.bottom_y + self.cell * 0.7)
            };
            let p = &mut self.pool[t];
            p.kind = ParticleKind::Leak;
            p.x = ex_x;
            p.y = ex_y;
            p.vx = self.rng.jitter() * 30.0;
            p.vy = 30.0 + self.rng.frac() * 90.0;
            p.life = 0.6 + self.rng.frac() * 0.45;
            p.max_life = p.life;
            p.size = (self.cell * 0.4).max(1.1);
            p.glow = 150;
            p.col = -1;
            p.row = -1;
        }
    }

    /// 把槽 `i` 转为漏沙（溢出 / 走偏出界；保持当前位置，继续下落）。
    fn drop_to_leak(&mut self, i: usize) {
        let p = &mut self.pool[i];
        p.kind = ParticleKind::Leak;
        // 水平方向偏向网格外（随缘左右）
        p.vx += self.rng.jitter() * 60.0;
        p.vy = 20.0 + self.rng.frac() * 60.0;
        p.life = 0.5 + self.rng.frac() * 0.4;
        p.max_life = p.life;
        p.size = (self.cell * 0.35).max(1.0);
        p.glow = 120;
        p.col = -1;
        p.row = -1;
    }

    /// 重建堆一致性：以池内 Resting 颗粒重算每列高度，并清掉
    /// 「行号 ≥ 高度」的幽灵颗粒（转为漏沙，视作顺坡溢撒）。
    fn reconcile_pile(&mut self) {
        // 1) 以池内 Resting 颗粒为准重算每列高度
        for c in 0..self.cols {
            let mut n = 0u16;
            for i in 0..self.len {
                let p = &self.pool[i];
                if p.active && p.kind == ParticleKind::Resting && p.col == c as i32 {
                    n += 1;
                }
            }
            self.top[c] = n.min(self.rows as u16);
        }
        // 2) 行号越界 / 超过列高度的颗粒视为溢出（转为漏沙）
        let mut i = 0usize;
        while i < self.len {
            let ghost = {
                let p = &self.pool[i];
                p.active
                    && p.kind == ParticleKind::Resting
                    && (p.col < 0
                        || p.col >= self.cols as i32
                        || p.row < 0
                        || p.row as u16 >= self.top[p.col as usize])
            };
            if ghost {
                self.drop_to_leak(i);
            } else {
                i += 1;
            }
        }
    }

    /// 逐帧紧致：把有效粒子搬到池前部，并把被搬走的源槽清空，
    /// 防止旧槽残留 `active=true` 被下一帧当作活粒重复复制（幽灵副本）。
    fn compact(&mut self) {
        let mut w = 0usize;
        for r in 0..MAX_PARTICLES {
            if self.pool[r].active {
                if w != r {
                    self.pool[w] = self.pool[r];
                    // 关键：清空源槽
                    self.pool[r] = Particle::dead();
                }
                w += 1;
            }
        }
        self.len = w;
    }

    // ==================== 渲染 ====================

    /// 绘制全部粒子。每颗都是实时计算：沙粒为圆点，发光为径向渐变 add。
    pub fn render(&self, canvas: &mut dyn Canvas, show_resting: bool) {
        for i in 0..self.len {
            let p = &self.pool[i];
            if !p.active {
                continue;
            }
            match p.kind {
                ParticleKind::Resting => {
                    if !show_resting {
                        continue;
                    }
                    let r = p.size as i32;
                    let sx = p.x as i32;
                    let sy = p.y as i32;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if dx * dx + dy * dy <= r * r {
                                canvas.draw_pixel(sx + dx, sy + dy, p.color);
                            }
                        }
                    }
                    // 微光沙粒：小半径 additive 光晕
                    if p.glow > 120 {
                        crate::draw_glow(canvas, p.x, p.y, 2, p.color, p.glow as u32 / 2);
                    }
                }
                ParticleKind::Falling => {
                    let r = p.size as i32;
                    let sx = p.x as i32;
                    let sy = p.y as i32;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if dx * dx + dy * dy <= r * r {
                                canvas.draw_pixel(sx + dx, sy + dy, p.color);
                            }
                        }
                    }
                    if p.glow > 100 {
                        crate::draw_glow(canvas, p.x, p.y, 1, p.color, p.glow as u32 / 3);
                    }
                }
                ParticleKind::Leak => {
                    // 蒸发：透明度、粒径随剩余寿命衰减
                    if p.life <= 0.0 {
                        continue;
                    }
                    let t = (p.life / p.max_life).clamp(0.0, 1.0);
                    let r = ((p.size * t).max(1.0)) as i32;
                    let a = (t * 235.0) as u32;
                    let sx = p.x as i32;
                    let sy = p.y as i32;
                    for dy in -r..=r {
                        for dx in -r..=r {
                            if dx * dx + dy * dy <= r * r {
                                canvas.blend_pixel(sx + dx, sy + dy, p.color, a);
                            }
                        }
                    }
                    if t > 0.55 {
                        crate::draw_glow(canvas, p.x, p.y, 1, p.color, (t * 120.0) as u32);
                    }
                }
                ParticleKind::Spark => {
                    let t = (p.life / p.max_life).clamp(0.0, 1.0);
                    crate::draw_glow(canvas, p.x, p.y, 3, p.color, (t * p.glow as f32) as u32);
                }
            }
        }
    }
}

// ==================== 单元测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_never_exceeds_cap() {
        let mut ps = ParticleSystem::new(1);
        ps.setup_grid(10, 10, 240, 24, 4.0, false);
        ps.spout_x = 130.0;
        ps.spout_y = 4.0;
        ps.target_rows = 6;
        // 猛灌大量预算（超过池容量）
        ps.add_pour(100_000);
        for _ in 0..400 {
            ps.update(0.016);
        }
        assert!(ps.len <= MAX_PARTICLES);
        assert!(ps.alive() <= MAX_PARTICLES);
    }

    #[test]
    fn sand_settles_then_drains() {
        let mut ps = ParticleSystem::new(2);
        ps.setup_grid(10, 10, 80, 16, 4.0, false); // 20 列 x 4 行
        ps.spout_x = 50.0;
        ps.spout_y = 4.0;
        ps.target_rows = 4;
        // 填满 20*4=80 颗
        ps.add_pour(500);
        for _ in 0..150 {
            ps.update(0.016);
        }
        // 堆高不超过行数
        for c in 0..ps.cols {
            assert!(ps.top[c] <= 4);
        }
        assert_eq!(ps.excess(), 0);
        let settled = ps.alive();
        assert!(settled > 0, "应当沉积出沙堆");
        // 目标降到 0 → 全部漏光
        ps.target_rows = 0;
        ps.drain_excess();
        for _ in 0..200 {
            ps.update(0.016);
        }
        for c in 0..ps.cols {
            assert_eq!(ps.top[c], 0, "col {c} 未漏净");
        }
        assert_eq!(ps.excess(), 0);
        assert_eq!(ps.alive(), 0, "漏沙应最终全部蒸发");
    }

    #[test]
    fn drain_releases_leaks_and_expire() {
        let mut ps = ParticleSystem::new(3);
        ps.setup_grid(0, 0, 60, 12, 3.0, false);
        ps.spout_x = 30.0;
        ps.spout_y = 0.0;
        ps.target_rows = 4;
        ps.add_pour(300);
        for _ in 0..150 {
            ps.update(0.016);
        }
        assert!(ps.alive() > 0);
        // 漏光后：池里不应再有 Resting，且一定帧数后全部蒸发
        ps.target_rows = 0;
        ps.drain_excess();
        for _ in 0..150 {
            ps.update(0.016);
        }
        for i in 0..ps.len {
            assert_ne!(ps.pool[i].kind, ParticleKind::Resting);
        }
        for _ in 0..150 {
            ps.update(0.016);
        }
        assert_eq!(ps.alive(), 0, "漏出的沙粒应在半空蒸发殆尽");
    }

    #[test]
    fn render_does_not_crash() {
        let mut buf = [0u32; 540 * 200];
        let mut ps = ParticleSystem::new(4);
        ps.setup_grid(10, 10, 240, 24, 4.0, false);
        ps.spout_x = 130.0;
        ps.spout_y = 4.0;
        ps.target_rows = 6;
        ps.add_pour(60);
        for _ in 0..30 {
            ps.update(0.016);
        }
        ps.scatter(100.0, 100.0, 30, 60.0);
        ps.spark(120.0, 40.0, 0xFFE9FF);
        let mut canvas = crate::MemCanvas {
            w: 540,
            h: 200,
            buf: &mut buf,
        };
        ps.render(&mut canvas, true);
        let mut n = 0;
        for px in buf.iter() {
            if *px != 0 {
                n += 1;
            }
        }
        assert!(n > 0);
    }

    #[test]
    fn vertical_grid_col_row_mapping() {
        let mut ps = ParticleSystem::new(5);
        // 竖排：容器 (x=250..270, y=50..350)，列沿 y，行沿 x
        ps.setup_grid(250, 50, 20, 300, 5.0, true);
        // 网格中央 (260, 200)：距底 150px → col = 30，距左 10px → row = 2
        let (c, r) = ps.col_row_at(260.0, 200.0).unwrap();
        assert_eq!((c, r), (30, 2));
    }
}