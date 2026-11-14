// KunxOS 共享光标 overlay 快速路径（no_std，无堆）。
//
// 各 GUI 应用此前在鼠标移动时整屏 render()+flip()（TTF 文字重新栅格化 +
// 2MB 显存拷贝），QEMU 软件渲染下光标严重不跟手。本模块提供与 launcher
// 相同的 overlay 方案：光标不画进 back buffer，而是直接写显存 FB；移动时
// 只恢复旧位置的 26x26 像素、备份新位置像素、再画光标，全程不触屏整屏重绘。
//
// 用法（调用方持有自己的 FB 映射与光标坐标）：
//   - 整屏 render()+flip() 后调用 `overlay_redraw(fb, stride, w, h, x, y)`
//   - 指针移动时调用 `overlay_move(fb, stride, w, h, x, y)`
// `x/y` 为指针热点位置（即各应用的 CUR_X/CUR_Y，屏幕像素坐标），贴图尖端
// 对齐该点。光标贴图与几何常量见 `cursor_sprite`（tubiao/shubiao.png 生成）。
// 光标下 26x26 备份区是本模块的 static（单实例足够：同一时刻只有一个
// 应用进程在运行）。

#[path = "cursor_sprite.rs"]
mod cursor_sprite;

const UW: usize = cursor_sprite::CUR_UW as usize;
const UH: usize = cursor_sprite::CUR_UH as usize;

/// 光标下 UW x UH 区域的备份（贴图 24x24 + 1px 边距）。
static mut CUR_UNDER: [u32; UW * UH] = [0u32; UW * UH];
static mut CUR_UNDER_X: i32 = -1;
static mut CUR_UNDER_Y: i32 = -1;

#[inline(always)]
fn fb_px(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32) -> u32 {
    unsafe {
        if x < 0 || y < 0 || x >= fw || y >= fh {
            0
        } else {
            *fb.add(y as usize * stride_px + x as usize)
        }
    }
}

#[inline(always)]
fn fb_set_px(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32, color: u32) {
    unsafe {
        if x >= 0 && y >= 0 && x < fw && y < fh {
            *fb.add(y as usize * stride_px + x as usize) = color;
        }
    }
}

fn save_under(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32) {
    unsafe {
        let sx = cursor_sprite::under_x(x);
        let sy = cursor_sprite::under_y(y);
        CUR_UNDER_X = sx;
        CUR_UNDER_Y = sy;
        for dy in 0..UH as i32 {
            for dx in 0..UW as i32 {
                CUR_UNDER[(dy * UW as i32 + dx) as usize] =
                    fb_px(fb, stride_px, fw, fh, sx + dx, sy + dy);
            }
        }
    }
}

fn restore_under(fb: *mut u32, stride_px: usize, fw: i32, fh: i32) {
    unsafe {
        let x = CUR_UNDER_X;
        let y = CUR_UNDER_Y;
        // 首次 overlay_redraw 之前收到 move 时备份还是全 0：恢复会把 25×25
        // 黑块写到 (0,0) 附近（负坐标被裁剪）——跳过（下一整帧重绘自愈）。
        if x < 0 || y < 0 {
            return;
        }
        for dy in 0..UH as i32 {
            for dx in 0..UW as i32 {
                fb_set_px(
                    fb,
                    stride_px,
                    fw,
                    fh,
                    x + dx,
                    y + dy,
                    CUR_UNDER[(dy * UW as i32 + dx) as usize],
                );
            }
        }
    }
}

fn draw_arrow(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32) {
    cursor_sprite::draw_at(fb, stride_px, fw, fh, x, y, None);
}

/// 整屏重绘后调用：备份光标下像素并以 overlay 画回显存。
pub fn overlay_redraw(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32) {
    if fb.is_null() || cursor_sprite::host_cursor() {
        return;
    }
    save_under(fb, stride_px, fw, fh, x, y);
    draw_arrow(fb, stride_px, fw, fh, x, y);
}

/// 光标移动快速路径：恢复旧位置 → 备份新位置 → 画光标。
pub fn overlay_move(fb: *mut u32, stride_px: usize, fw: i32, fh: i32, x: i32, y: i32) {
    if fb.is_null() || cursor_sprite::host_cursor() {
        return;
    }
    restore_under(fb, stride_px, fw, fh);
    save_under(fb, stride_px, fw, fh, x, y);
    draw_arrow(fb, stride_px, fw, fh, x, y);
}

/// 光标位置 (px,py) 对应的画布脏矩形（备份区，含 1px 外扩）。
pub fn under_rect(px: i32, py: i32) -> (i32, i32, u32, u32) {
    (cursor_sprite::under_x(px), cursor_sprite::under_y(py), UW as u32, UH as u32)
}

/// 光标移动快速路径 + 返回本次写入画布的脏矩形。
///
/// 返回 `[旧位置区, 新位置区]`（`(x, y, w, h)`；尚未备份过旧位置时旧区为
/// 零矩形，内核对零尺寸脏区忽略）。调用方把返回值经 FBIO_PRESENT 提交呈现
/// —— 光标移动只拷贝 ~676 像素 ×2，替代整屏 2MB 提交（QEMU 软件渲染下
/// 整屏提交是「鼠标不跟手」的直接原因）。
pub fn overlay_move_rects(
    fb: *mut u32,
    stride_px: usize,
    fw: i32,
    fh: i32,
    x: i32,
    y: i32,
) -> [(i32, i32, u32, u32); 2] {
    if fb.is_null() || cursor_sprite::host_cursor() {
        return [(0, 0, 0, 0); 2];
    }
    let old = unsafe {
        if CUR_UNDER_X >= 0 {
            (CUR_UNDER_X, CUR_UNDER_Y, UW as u32, UH as u32)
        } else {
            (0, 0, 0, 0)
        }
    };
    restore_under(fb, stride_px, fw, fh);
    save_under(fb, stride_px, fw, fh, x, y);
    draw_arrow(fb, stride_px, fw, fh, x, y);
    let new = unsafe { (CUR_UNDER_X, CUR_UNDER_Y, UW as u32, UH as u32) };
    [old, new]
}

/// 光标移动 + 立即提交呈现（小脏矩形）。
///
/// `present` 为调用方的 FBIO_PRESENT 提交函数（接收脏矩形列表）；传 `None`
/// 时只画不提交（等价旧 [`overlay_move`]，呈现由调用方自理）。
pub fn overlay_move_present(
    fb: *mut u32,
    stride_px: usize,
    fw: i32,
    fh: i32,
    x: i32,
    y: i32,
    present: Option<fn(&[(i32, i32, u32, u32)])>,
) {
    if fb.is_null() || cursor_sprite::host_cursor() {
        return;
    }
    let rects = overlay_move_rects(fb, stride_px, fw, fh, x, y);
    if let Some(p) = present {
        p(&rects);
    }
}
