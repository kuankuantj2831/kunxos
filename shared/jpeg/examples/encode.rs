//! 宿主验证示例：生成彩条+移动白块测试图并编码为 JPEG 写入 out.jpg。
//! 用法：cargo run --example encode --manifest-path shared/jpeg/Cargo.toml

use kunxjpeg::jpeg_encode_rgb888;

fn main() {
    let w = 640usize;
    let h = 480usize;
    let mut rgb = vec![0u8; w * h * 3];
    // 纯 SMPTE 风格彩条：7 段均匀色带
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let band = (x * 7 / w) as u8;
            let (r, g, b) = match band {
                0 => (255, 0, 0),
                1 => (0, 255, 0),
                2 => (0, 0, 255),
                3 => (255, 255, 0),
                4 => (0, 255, 255),
                5 => (255, 0, 255),
                _ => (128, 128, 128),
            };
            rgb[i] = r;
            rgb[i + 1] = g;
            rgb[i + 2] = b;
        }
    }
    // 中心附近一个移动白块
    let cx = (w as i32 / 2) + 30;
    let cy = h as i32 / 2;
    for y in (cy - 20) as usize..(cy + 20) as usize {
        for x in (cx - 20) as usize..(cx + 20) as usize {
            let i = (y * w + x) * 3;
            rgb[i] = 255;
            rgb[i + 1] = 255;
            rgb[i + 2] = 255;
        }
    }

    let mut out = vec![0u8; 4 * 1024 * 1024];
    let n = jpeg_encode_rgb888(&mut out, w, h, &rgb, 85).expect("encode failed");
    std::fs::write("out.jpg", &out[..n]).expect("write failed");
    println!("encoded {}x{} -> {} bytes", w, h, n);
}
