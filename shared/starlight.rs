//! KunxOS 星光基底：无堆、静态星芒、按区域绘制；局部刷新与整屏结果一致。
pub const ACCENT: u32 = 0x005B63D9;
pub const LIGHT: (u32, u32, u32) = (0x00F2F5FF, 0x00CCD6F2, 0x009EA8EB);

fn blend(a: u32, b: u32, k: u32) -> u32 {
    let q = 255 - k;
    ((((a >> 16 & 255) * q + (b >> 16 & 255) * k) / 255) << 16)
        | ((((a >> 8 & 255) * q + (b >> 8 & 255) * k) / 255) << 8)
        | (((a & 255) * q + (b & 255) * k) / 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_refresh_matches_full_frame_at_multiple_sizes() {
        for (w, h) in [(1, 1), (540, 960), (960, 720), (1920, 1001)] {
            let mut full = vec![0; w * h];
            paint(&mut full, w, h, (0, 0, w as i32, h as i32), LIGHT);
            let mut tiled = vec![0; w * h];
            for y in (0..h).step_by(83) {
                for x in (0..w).step_by(71) {
                    paint(&mut tiled, w, h, (x as i32, y as i32, 71, 83), LIGHT);
                }
            }
            assert_eq!(full, tiled);
        }
    }

    #[test]
    fn clips_to_requested_rectangle_and_preserves_guards() {
        let mut buf = vec![0xDEADBEEF; 102];
        paint(&mut buf[1..101], 10, 10, (-3, -5, 7, 8), LIGHT);
        assert_eq!(buf[0], 0xDEADBEEF);
        assert_eq!(buf[101], 0xDEADBEEF);
        for y in 0..10 {
            for x in 0..10 {
                assert_eq!(buf[1 + y * 10 + x] != 0xDEADBEEF, x < 4 && y < 3);
            }
        }
    }
}

/// `rect` 使用屏幕坐标；星点固定在归一化坐标中，旋转/缩放后仍均匀分布。
pub fn paint(buf: &mut [u32], w: usize, h: usize, rect: (i32, i32, i32, i32), palette: (u32, u32, u32)) {
    if w == 0 || h == 0 || buf.len() < w * h { return; }
    let (x, y, rw, rh) = rect;
    let xa = x.clamp(0, w as i32) as usize;
    let ya = y.clamp(0, h as i32) as usize;
    let xb = x.saturating_add(rw.max(0)).clamp(0, w as i32) as usize;
    let yb = y.saturating_add(rh.max(0)).clamp(0, h as i32) as usize;
    for py in ya..yb {
        let ny = (py * 256 / h) as i32;
        let row = blend(palette.0, palette.1, (py * 255 / h) as u32);
        for px in xa..xb {
            let nx = (px * 256 / w) as i32;
            // 柔和椭圆光域，让磨砂面板下面有可辨识的色彩层次。
            let violet = (18000 - (nx - 224) * (nx - 224) - (ny - 72) * (ny - 72)).max(0) as u32 * 52 / 18000;
            let pearl = (14000 - (nx - 32) * (nx - 32) - (ny - 190) * (ny - 190)).max(0) as u32 * 112 / 14000;
            buf[py * w + px] = blend(blend(row, palette.2, violet), 0x00FFFFFF, pearl);
        }
    }
    let mut seed = 0x4B554E58u32;
    for i in 0..76 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let sx = ((seed >> 8) as usize % 1024 * w / 1024) as i32;
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let sy = ((seed >> 8) as usize % 1024 * h / 1024) as i32;
        let radius: i32 = if i % 13 == 0 { 4 } else { 1 };
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let distance = dx.abs() + dy.abs();
                if distance > radius || (dx != 0 && dy != 0) { continue; }
                let px = sx + dx;
                let py = sy + dy;
                if px < xa as i32 || px >= xb as i32 || py < ya as i32 || py >= yb as i32 { continue; }
                let k = if distance == 0 { 220 } else { (radius + 1 - distance) as u32 * 100 / radius as u32 };
                let index = py as usize * w + px as usize;
                buf[index] = blend(buf[index], 0x00FFFFFF, k);
            }
        }
    }
}
