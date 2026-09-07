//! 受控实验：纯色 / 分块测试图 → out.jpg，供 PIL 检查。
use kunxjpeg::jpeg_encode_rgb888;

fn main() {
    // 1) 纯红 16x16
    let w = 16usize;
    let h = 16usize;
    let mut rgb = vec![255u8, 0, 0].repeat(w * h);
    let mut out = vec![0u8; 1 << 20];
    let n = jpeg_encode_rgb888(&mut out, w, h, &rgb, 85).unwrap();
    std::fs::write("out_red16.jpg", &out[..n]).unwrap();

    // 2) 32x32：左半红右半绿
    let w2 = 32usize;
    let h2 = 32usize;
    let mut rgb2 = vec![0u8; w2 * h2 * 3];
    for y in 0..h2 {
        for x in 0..w2 {
            let i = (y * w2 + x) * 3;
            if x < 16 {
                rgb2[i] = 255;
            } else {
                rgb2[i + 1] = 255;
            }
        }
    }
    let mut out2 = vec![0u8; 1 << 20];
    let n2 = jpeg_encode_rgb888(&mut out2, w2, h2, &rgb2, 85).unwrap();
    std::fs::write("out_split32.jpg", &out2[..n2]).unwrap();

    // 3) 320x240：7 条纯色竖带（同 640x480 但一半尺寸）
    let w3 = 320usize;
    let h3 = 240usize;
    let mut rgb3 = vec![0u8; w3 * h3 * 3];
    for y in 0..h3 {
        for x in 0..w3 {
            let i = (y * w3 + x) * 3;
            let band = (x * 7 / w3) as u8;
            let (r, g, b) = match band {
                0 => (255, 0, 0),
                1 => (0, 255, 0),
                2 => (0, 0, 255),
                3 => (255, 255, 0),
                4 => (0, 255, 255),
                5 => (255, 0, 255),
                _ => (128, 128, 128),
            };
            rgb3[i] = r;
            rgb3[i + 1] = g;
            rgb3[i + 2] = b;
        }
    }
    let mut out3 = vec![0u8; 1 << 20];
    let n3 = jpeg_encode_rgb888(&mut out3, w3, h3, &rgb3, 85).unwrap();
    std::fs::write("out_bands320.jpg", &out3[..n3]).unwrap();
    println!("done: {} / {} / {} bytes", n, n2, n3);
}
