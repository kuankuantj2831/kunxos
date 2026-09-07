//! 图片解码：PPM P6（未压缩 RGB）最小实现。
//!
//! 存储格式：8 字节头（宽/高各 4 字节大端）+ RGB 像素，供 paint.rs 直接读取。
//! PNG/JPEG 解码预留（初始范围：占位框 + PPM）。

extern crate alloc;

use alloc::vec::Vec;

/// 判断是否为 PPM P6 数据。
pub fn is_ppm(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == b'P' && (data[1] == b'6')
}

/// 解码 PPM P6 → 存储格式（8 字节头 + RGB）。失败返回 None。
pub fn decode_ppm(data: &[u8]) -> Option<Vec<u8>> {
    if !is_ppm(data) {
        return None;
    }
    let mut i = 2usize;
    // 跳过注释与空白，读三个数字
    let mut nums = [0u32; 3];
    let mut ni = 0usize;
    while ni < 3 {
        skip_ws_and_comments(data, &mut i);
        let start = i;
        while i < data.len() && data[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let v: u32 = core::str::from_utf8(&data[start..i]).ok()?.parse().ok()?;
        nums[ni] = v;
        ni += 1;
    }
    let (w, h, maxv) = (nums[0], nums[1], nums[2]);
    if w == 0 || h == 0 || w > 4096 || h > 4096 || maxv == 0 || maxv > 255 {
        return None;
    }
    // 单字节空白
    if i < data.len() && (data[i] == b'\n' || data[i] == b'\r' || data[i] == b' ' || data[i] == b'\t') {
        i += 1;
    }
    let need = w as usize * h as usize * 3;
    if data.len() < i + need {
        return None;
    }
    let mut out = Vec::with_capacity(8 + need);
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&data[i..i + need]);
    Some(out)
}

fn skip_ws_and_comments(data: &[u8], i: &mut usize) {
    loop {
        while *i < data.len() && (data[*i] == b' ' || data[*i] == b'\t' || data[*i] == b'\n' || data[*i] == b'\r') {
            *i += 1;
        }
        if *i < data.len() && data[*i] == b'#' {
            while *i < data.len() && data[*i] != b'\n' {
                *i += 1;
            }
        } else {
            break;
        }
    }
}

/// 生成演示渐变图（kunxos://gfx/*）：对角渐变 RGB，返回存储格式。
pub fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + (w * h * 3) as usize);
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    for y in 0..h {
        for x in 0..w {
            let r = (x * 255 / w.max(1)) as u8;
            let g = (y * 255 / h.max(1)) as u8;
            let b = ((x + y) * 255 / (w + h).max(1)) as u8;
            out.push(r);
            out.push(g);
            out.push(b);
        }
    }
    out
}
