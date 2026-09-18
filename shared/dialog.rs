//! KunxOS 共享对话框组件（VIVO OriginOS 风格，no_std、无堆、固定内存）。
//!
//! 全系统统一的居中模态对话框视觉规范（对标 vivo OriginOS 全局对话框）：
//! - 白色卡片 + 超椭圆圆角（连续曲率，与系统毛玻璃同一几何）+ 细描边，
//!   屏幕正中弹出；卡片后方全屏半透明压暗遮罩；
//! - 标题居中加粗、正文居中灰色；
//! - 按钮区：1px 上分割线，双按钮等宽 + 中间竖分割线，纯文字按钮
//!   （取消 = 深灰、确认 = 蓝、危险 = 红）；
//! - 选择行：单选圆点 / 多选圆角方框靠右，行间 1px 分割线；
//! - 内嵌输入框（浅灰圆角 + 蓝色光标）与进度条（浅灰轨道 + 蓝色填充）。
//!
//! 通过 [`DialogUi`] trait 与各应用自绘管线对接（像素写入 / 背景读取 / 文本），
//! 本模块不依赖任何应用的绘制原语；圆角填充 / 描边由调用方提供
//! （各应用均已接入 shared/glass 的抗锯齿超椭圆实现）。
//!
//! # 使用方式（以某应用确认对话框为例）
//!
//! ```ignore
//! // 1. 计算几何：宽 400，高按内容行数确定
//! let w = dialog::DLG_W;
//! let h = dialog::DLG_TITLE_H + dialog::DLG_MSG_H + dialog::DLG_BTN_H;
//! let (x, y) = dialog::center_rect(DISPLAY_W as i32, DISPLAY_H as i32, w, h);
//! // 2. 遮罩 + 卡片 + 内容
//! dialog::scrim(&mut ui, DISPLAY_W as i32, DISPLAY_H as i32, 115);
//! dialog::draw_card(&mut ui, x, y, w, h);
//! dialog::draw_title(&mut ui, x, y, w, b"确认删除");
//! dialog::draw_message(&mut ui, x, y + dialog::DLG_TITLE_H, w, b"此操作不可恢复");
//! dialog::draw_buttons(&mut ui, x, y + h - dialog::DLG_BTN_H, w, b"取消", b"删除", true);
//! // 3. 命中测试（与绘制同一几何，永不漂移）
//! match dialog::hit_buttons(x, y + h - dialog::DLG_BTN_H, w, px, py) {
//!     Some(true) => { /* 确认 */ }
//!     Some(false) | None => { /* 取消 / 点外部关闭 */ }
//! }
//! ```

/// 对话框默认宽度（540 宽屏幕的 ~74%，OriginOS 观感）。
pub const DLG_W: i32 = 400;
/// 卡片圆角半径（OriginOS 大圆角连续曲率）。
pub const DLG_RADIUS: i32 = 28;
/// 按钮行高度。
pub const DLG_BTN_H: i32 = 50;
/// 选择行高度。
pub const DLG_OPT_H: i32 = 54;
/// 内嵌输入框高度。
pub const DLG_FIELD_H: i32 = 44;
/// 标题行区高（含上下留白）。
pub const DLG_TITLE_H: i32 = 52;
/// 正文行区高。
pub const DLG_MSG_H: i32 = 40;
/// 输入框行区高（框 + 上下留白）。
pub const DLG_FIELD_ROW_H: i32 = 60;
/// 进度条行区高。
pub const DLG_PROG_ROW_H: i32 = 44;

// ==================== 配色（OriginOS 浅色对话框） ====================

/// 卡片背景（纯白）。
pub const C_DLG_BG: u32 = 0x00_FF_FF_FF;
/// 标题文字（近黑）。
pub const C_DLG_TITLE: u32 = 0x00_1A_1A_1E;
/// 正文 / 次要文字（灰）。
pub const C_DLG_MSG: u32 = 0x00_7C_7C_82;
/// 分割线（发丝线）。
pub const C_DLG_LINE: u32 = 0x00_EA_EA_EC;
/// 卡片描边（比分割线略深，白色卡片在遮罩上保持轮廓）。
pub const C_DLG_BORDER: u32 = 0x00_E2_E2_E8;
/// 取消按钮文字（深灰）。
pub const C_DLG_CANCEL: u32 = 0x00_46_46_4C;
/// 确认按钮文字（OriginOS 蓝）。
pub const C_DLG_BLUE: u32 = 0x00_34_78_F6;
/// 危险操作文字（红）。
pub const C_DLG_RED: u32 = 0x00_FA_51_51;
/// 输入框底色（浅灰）。
pub const C_DLG_FIELD: u32 = 0x00_F2_F3_F5;
/// 进度条轨道。
pub const C_DLG_TRACK: u32 = 0x00_E9_E9_EE;
/// 单选圆点描边（未选中）。
pub const C_DLG_RADIO: u32 = 0x00_C4_C4_CA;

// ==================== 绘制目标 trait ====================

/// 对话框绘制目标。由各应用适配自己的像素 / 文本管线：
/// - `put`：把颜色写入当前 back buffer（全不透明）；
/// - `back`：读取当前像素（遮罩 / 淡入混合的输入源）；
/// - `text` / `text_bold`：以指定字号绘制一行文本（y 为字形顶部）；
/// - `text_w`：指定字号的行宽；
/// - `text_top`：垂直居中于 `cy` 的文本顶部 y（各应用字形度量略有差异，
///   由适配方给出与自身 draw_text 一致的换算）。
pub trait DialogUi {
    fn put(&mut self, x: i32, y: i32, color: u32);
    fn back(&self, x: i32, y: i32) -> u32;
    fn text(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32);
    fn text_bold(&mut self, x: i32, y: i32, s: &[u8], px: f32, color: u32);
    fn text_w(&self, s: &[u8], px: f32) -> i32;
    fn text_top(&self, px: f32, cy: i32) -> i32;

    /// 以 (color, alpha) 混合填充一整行 [x0, x1)（遮罩热路径：52 万像素
    /// 逐像素走 trait 虚调用太慢，适配方应以行切片混合覆盖本方法）。
    fn fill_row(&mut self, y: i32, x0: i32, x1: i32, color: u32, alpha: u32) {
        let mut x = x0;
        while x < x1 {
            let bg = self.back(x, y);
            self.put(x, y, blend_into(bg, color, alpha));
            x += 1;
        }
    }
}

/// 标准 alpha 混合（结果 = bg*(1-t) + fg*t，t ∈ 0..=255）。
#[inline]
pub fn blend_into(bg: u32, fg: u32, t: u32) -> u32 {
    let t = t.min(255);
    let inv = 255 - t;
    let r = (((bg >> 16) & 0xFF) * inv + ((fg >> 16) & 0xFF) * t) / 255;
    let g = (((bg >> 8) & 0xFF) * inv + ((fg >> 8) & 0xFF) * t) / 255;
    let b = ((bg & 0xFF) * inv + (fg & 0xFF) * t) / 255;
    (r << 16) | (g << 8) | b
}

// ==================== 几何 ====================

/// 居中矩形左上角（给定屏幕尺寸与矩形尺寸）。
pub fn center_rect(sw: i32, sh: i32, w: i32, h: i32) -> (i32, i32) {
    ((sw - w) / 2, (sh - h) / 2)
}

/// 命中测试结果：`Some(true)` = 确认按钮，`Some(false)` = 取消按钮，`None` = 未命中。
pub fn hit_buttons(btn_x: i32, btn_y: i32, w: i32, x: i32, y: i32) -> Option<bool> {
    if y < btn_y || y >= btn_y + DLG_BTN_H {
        return None;
    }
    if x < btn_x || x >= btn_x + w {
        return None;
    }
    Some((x - btn_x) >= w / 2)
}

/// 选择行命中测试：返回行下标（0..n），未命中返回 None。
pub fn hit_options(x: i32, y: i32, row_x: i32, row_y: i32, w: i32, n: usize) -> Option<usize> {
    if x < row_x || x >= row_x + w {
        return None;
    }
    if y < row_y {
        return None;
    }
    let idx = ((y - row_y) / DLG_OPT_H) as usize;
    if idx < n {
        Some(idx)
    } else {
        None
    }
}

/// 点是否在矩形内（点外部关闭对话框用）。
pub fn outside(x: i32, y: i32, rx: i32, ry: i32, w: i32, h: i32) -> bool {
    x < rx || x >= rx + w || y < ry || y >= ry + h
}

// ==================== 绘制 ====================

/// 全屏压暗遮罩（OriginOS 弹窗背景，`alpha` 0..=255）。
pub fn scrim(ui: &mut dyn DialogUi, sw: i32, sh: i32, alpha: u32) {
    for y in 0..sh {
        // 亮度缩到 ~40% 同时向冷色偏移（比纯乘法更接近系统遮罩观感）。
        ui.fill_row(y, 0, sw, 0x00_05_0A_14, alpha);
    }
}

/// 白色卡片：淡投影（3 圈低透明描边）+ 白色填充 + 细描边。
pub fn draw_card(ui: &mut dyn DialogUi, fill_rr: RrFn, stroke_rr: RrFn, x: i32, y: i32, w: i32, h: i32) {
    // 淡投影：向外扩散 3 圈，透明度快速衰减（OriginOS 卡片悬浮感）。
    for (k, a) in [(1, 26), (2, 16), (3, 8)] {
        stroke_rr(ui, x - k, y - k, w + 2 * k, h + 2 * k, DLG_RADIUS + k, 0x00_0A_0D_14, a);
    }
    fill_rr(ui, x, y, w, h, DLG_RADIUS, C_DLG_BG, 255);
    stroke_rr(ui, x, y, w, h, DLG_RADIUS, C_DLG_BORDER, 255);
}

/// 圆角矩形填充 / 描边函数类型（各应用适配 glass::smooth_fill_rr / smooth_stroke_rr）。
pub type RrFn = fn(&mut dyn DialogUi, i32, i32, i32, i32, i32, u32, u32);

/// 居中标题（加粗）。
pub fn draw_title(ui: &mut dyn DialogUi, x: i32, y: i32, w: i32, title: &[u8]) {
    let cy = y + DLG_TITLE_H / 2;
    let tw = ui.text_w(title, 17.0);
    ui.text_bold(x + (w - tw) / 2, ui.text_top(17.0, cy), title, 17.0, C_DLG_TITLE);
}

/// 居中正文（灰）。
pub fn draw_message(ui: &mut dyn DialogUi, x: i32, y: i32, w: i32, msg: &[u8], px: f32) {
    let cy = y + DLG_MSG_H / 2;
    let tw = ui.text_w(msg, px);
    ui.text(x + (w - tw) / 2, ui.text_top(px, cy), msg, px, C_DLG_MSG);
}

/// 按钮行：1px 上分割线；`ok` 为 None 时绘制单个居中按钮，否则双按钮等宽
/// + 竖分割线。取消深灰，确认蓝（danger = 红）。
pub fn draw_buttons(
    ui: &mut dyn DialogUi,
    x: i32,
    y: i32,
    w: i32,
    cancel: &[u8],
    ok: Option<(&[u8], bool)>,
) {
    // 上分割线。
    for dx in 0..w {
        ui.put(x + dx, y, C_DLG_LINE);
    }
    let cy = y + DLG_BTN_H / 2;
    match ok {
        Some((ok_label, danger)) => {
            // 竖分割线。
            let mx = x + w / 2;
            for dy in 1..DLG_BTN_H {
                ui.put(mx, y + dy, C_DLG_LINE);
            }
            let cw = ui.text_w(cancel, 16.0);
            ui.text(
                x + w / 4 - cw / 2,
                ui.text_top(16.0, cy),
                cancel,
                16.0,
                C_DLG_CANCEL,
            );
            let oc = if danger { C_DLG_RED } else { C_DLG_BLUE };
            let ow = ui.text_w(ok_label, 16.0);
            ui.text(
                x + w * 3 / 4 - ow / 2,
                ui.text_top(16.0, cy),
                ok_label,
                16.0,
                oc,
            );
        }
        None => {
            let cw = ui.text_w(cancel, 16.0);
            ui.text(
                x + w / 2 - cw / 2,
                ui.text_top(16.0, cy),
                cancel,
                16.0,
                C_DLG_BLUE,
            );
        }
    }
}

/// 单选圆（OriginOS：未选中灰圈，选中蓝圈 + 蓝芯）。
pub fn draw_radio(ui: &mut dyn DialogUi, cx: i32, cy: i32, selected: bool) {
    for dy in -9..=9 {
        for dx in -9..=9 {
            let d = dx * dx + dy * dy;
            if d <= 81 && d >= 49 {
                ui.put(cx + dx, cy + dy, if selected { C_DLG_BLUE } else { C_DLG_RADIO });
            }
        }
    }
    if selected {
        for dy in -4..=4 {
            for dx in -4..=4 {
                if dx * dx + dy * dy <= 16 {
                    ui.put(cx + dx, cy + dy, C_DLG_BLUE);
                }
            }
        }
    }
}

/// 多选方框（OriginOS：未选中灰描边圆角方，选中蓝填充 + 白勾）。
pub fn draw_check(ui: &mut dyn DialogUi, x: i32, y: i32, checked: bool) {
    // 20x20 圆角方框（r=6）。
    let s = 20;
    let r = 6;
    for dy in 0..s {
        for dx in 0..s {
            let in_corner = (dx < r || dx >= s - r) && (dy < r || dy >= s - r);
            if !in_corner {
                if checked {
                    ui.put(x + dx, y + dy, C_DLG_BLUE);
                } else if dx == 0 || dx == s - 1 || dy == 0 || dy == s - 1 {
                    ui.put(x + dx, y + dy, C_DLG_RADIO);
                }
            } else {
                // 圆角盒：中心距判定（与 20px/r6 几何近似，视觉平滑）。
                let qx = if dx < r { r - dx } else { dx - (s - 1 - r) };
                let qy = if dy < r { r - dy } else { dy - (s - 1 - r) };
                let inside = qx * qx + qy * qy <= r * r;
                if inside {
                    if checked {
                        ui.put(x + dx, y + dy, C_DLG_BLUE);
                    } else if qx * qx + qy * qy >= (r - 1) * (r - 1) {
                        // 环带描边（欧氏距离）：原 qx+qy>r 的 L1 判定漏掉
                        // 弧中点，四角有 1px 级缺口。
                        ui.put(x + dx, y + dy, C_DLG_RADIO);
                    }
                }
            }
        }
    }
    if checked {
        for i in 0..5 {
            ui.put(x + 4 + i, y + 10 + i, C_DLG_BG);
            ui.put(x + 5 + i, y + 10 + i, C_DLG_BG);
        }
        for i in 0..7 {
            ui.put(x + 8 + i, y + 14 - i, C_DLG_BG);
            ui.put(x + 9 + i, y + 14 - i, C_DLG_BG);
        }
    }
}

/// 选择行组：n 行等高（[`DLG_OPT_H`]），行间 1px 分割线；右侧单选 / 多选指示。
/// 返回底部 y。`multi = false` 时 `sel` 为选中行下标，`multi = true` 时为位掩码。
pub fn draw_options(
    ui: &mut dyn DialogUi,
    x: i32,
    y: i32,
    w: i32,
    items: &[&[u8]],
    sel: u32,
    multi: bool,
) -> i32 {
    let n = items.len();
    for (i, item) in items.iter().enumerate() {
        let ry = y + i as i32 * DLG_OPT_H;
        if i > 0 {
            for dx in 0..w {
                ui.put(x + dx, ry, C_DLG_LINE);
            }
        }
        let selected = if multi {
            sel >> i & 1 == 1
        } else {
            sel as usize == i
        };
        ui.text(
            x + 24,
            ui.text_top(15.0, ry + DLG_OPT_H / 2),
            item,
            15.0,
            if selected { C_DLG_TITLE } else { C_DLG_MSG },
        );
        if multi {
            draw_check(ui, x + w - 24 - 20, ry + (DLG_OPT_H - 20) / 2, selected);
        } else {
            draw_radio(ui, x + w - 24 - 9, ry + DLG_OPT_H / 2, selected);
        }
    }
    y + n as i32 * DLG_OPT_H
}

/// 内嵌输入框（浅灰圆角 + 深字 + 蓝色光标）。返回文本基线供调用方续画。
pub fn draw_input_field(ui: &mut dyn DialogUi, fill_rr: RrFn, x: i32, y: i32, w: i32, buf: &[u8]) {
    fill_rr(ui, x, y, w, DLG_FIELD_H, 12, C_DLG_FIELD, 255);
    let cy = y + DLG_FIELD_H / 2;
    ui.text(x + 14, ui.text_top(15.0, cy), buf, 15.0, C_DLG_TITLE);
    let tw = ui.text_w(buf, 15.0);
    for dy in -8..=8 {
        ui.put(x + 14 + tw + 2, cy + dy, C_DLG_BLUE);
    }
}

/// 进度条（浅灰轨道 r4 + 蓝色填充）+ 百分比文字。`frac` 0..=100。
pub fn draw_progress(ui: &mut dyn DialogUi, fill_rr: RrFn, x: i32, y: i32, w: i32, frac: u32) -> i32 {
    fill_rr(ui, x, y, w, 8, 4, C_DLG_TRACK, 255);
    let fw = ((w as u32 * frac.min(100) / 100) as i32).max(8).min(w);
    if frac > 0 {
        fill_rr(ui, x, y, fw, 8, 4, C_DLG_BLUE, 255);
    }
    y + 8
}
