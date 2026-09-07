// KunxOS 系统鼠标指针贴图（no_std，无堆）。
//
// 指针图像来自项目源图 `tubiao/shubiao.png`（256x256 RGBA），由
// `tools/make_cursor.py` 面积平均缩放为 24x24 RGBA8888 裸像素写入
// `shared/cursor.rgba`，在此 `include_bytes!` 编译期内嵌进各 GUI 应用，
// 替代此前各处硬编码的 9x9 位图箭头（黑描边 + 白填充）。
//
// 热点取源图指针尖端（不透明包围盒左上角 (56,16)）的等比映射。
// 更换指针图后重跑 `python3 tools/make_cursor.py` 并同步下列常量。
//
// 各光标绘制点（shared/cursor_overlay.rs / shared/app_kit.rs /
// apps/launcher / apps/oobe）统一经本模块 blit；备份/脏区几何用
// `under_x()/under_y()/CUR_UW/CUR_UH` 计算（贴图外扩 1px，与旧
// 9x9+描边 = 11x11 的约定相同）。

/// 贴图宽（必须与 tools/make_cursor.py 的 DST_SIZE 一致）。
pub const CUR_W: i32 = 24;
/// 贴图高（必须与 tools/make_cursor.py 的 DST_SIZE 一致）。
pub const CUR_H: i32 = 24;
/// 指针热点：贴图内「尖端」像素坐标（指针位置对齐此处，点击点即热点）。
pub const CUR_HOT_X: i32 = 5;
pub const CUR_HOT_Y: i32 = 2;
/// 备份/脏区尺寸：贴图外扩 1px。
pub const CUR_UW: i32 = CUR_W + 2;
pub const CUR_UH: i32 = CUR_H + 2;

/// 指针位置 (px,py) 对应的备份/脏区左上角。
#[inline(always)]
pub fn under_x(px: i32) -> i32 {
    px - CUR_HOT_X - 1
}

#[inline(always)]
pub fn under_y(py: i32) -> i32 {
    py - CUR_HOT_Y - 1
}

/// 24x24 RGBA8888 裸像素（每像素 R,G,B,A 各 1 字节，行主序）。
static CURSOR_RGBA: [u8; (CUR_W * CUR_H * 4) as usize] = *include_bytes!("cursor.rgba");

/// 把指针贴图画到 `dst`（XRGB8888 行主序），指针位置 `px/py` 与热点对齐。
///
/// `lut` 在混合前对贴图像素 RGB 做变换（launcher 亮度 LUT 与整屏亮度
/// 匹配用），其余调用方传 `None`。越界自动裁剪，alpha 按像素混合。
pub fn draw_at(
    dst: *mut u32,
    stride_px: usize,
    dw: i32,
    dh: i32,
    px: i32,
    py: i32,
    lut: Option<fn(u32) -> u32>,
) {
    blit(dst, stride_px, dw, dh, px - CUR_HOT_X, py - CUR_HOT_Y, lut);
}

/// 贴图左上角对齐 (x,y) 的 alpha 混合 blit（越界裁剪）。
pub fn blit(
    dst: *mut u32,
    stride_px: usize,
    dw: i32,
    dh: i32,
    x: i32,
    y: i32,
    lut: Option<fn(u32) -> u32>,
) {
    if dst.is_null() {
        return;
    }
    for cy in 0..CUR_H {
        let dy = y + cy;
        if dy < 0 || dy >= dh {
            continue;
        }
        for cx in 0..CUR_W {
            let dx = x + cx;
            if dx < 0 || dx >= dw {
                continue;
            }
            let off = ((cy * CUR_W + cx) * 4) as usize;
            let a = CURSOR_RGBA[off + 3] as u32;
            if a == 0 {
                continue;
            }
            let mut src: u32 = ((CURSOR_RGBA[off] as u32) << 16)
                | ((CURSOR_RGBA[off + 1] as u32) << 8)
                | (CURSOR_RGBA[off + 2] as u32);
            if let Some(f) = lut {
                src = f(src) & 0x00_FF_FF_FF;
            }
            unsafe {
                let p = dst.add(dy as usize * stride_px + dx as usize);
                let v = if a >= 255 {
                    src
                } else {
                    let d = *p;
                    0x00_00_00_00
                        | (mix((src >> 16) & 0xFF, (d >> 16) & 0xFF, a) << 16)
                        | (mix((src >> 8) & 0xFF, (d >> 8) & 0xFF, a) << 8)
                        | mix(src & 0xFF, d & 0xFF, a)
                };
                *p = v;
            }
        }
    }
}

/// 单通道 alpha 混合（四舍五入）。
#[inline(always)]
fn mix(s: u32, d: u32, a: u32) -> u32 {
    (s * a + d * (255 - a) + 127) / 255
}
