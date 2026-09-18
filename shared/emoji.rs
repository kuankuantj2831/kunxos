//! Noto Color Emoji（CBDT/CBLC 位图彩色字体）解析 + 最小 PNG 解码。
//!
//! 这是 KunxOS 文本引擎的 Emoji 后端：rustybuzz 对 emoji 字体做 shaping
//! （ZWJ 家庭序列 / 肤色修饰 → GSUB 连字 → 单个字形 id），本模块负责把
//! 字形 id 映射到 CBDT 位图条目，解码 PNG（inflate + unfilter + 调色板/
//! RGBA），并缩放/缓存为 RGBA 位图供绘制。
//!
//! 仅支持 Noto Color Emoji 实际使用的子集：bit depth 8、非隔行、
//! 颜色类型 2(RGB)/3(调色板)/6(RGBA)、CBDT 图像格式 17(PNG)。
//! 不校验 CRC（字体数据可信）。
//!
//! 宿主测试：本模块不依赖系统调用，可直接在 x86_64 上编译运行。

extern crate alloc;

use alloc::vec::Vec;

// ==================== CBLC/CBDT 二进制解析 ====================

/// 一个 emoji 位图（已解码 + 缩放为 RGBA）。
pub struct EmojiBitmap {
    /// RGBA8888 像素（行优先）。
    pub rgba: Vec<u8>,
    /// 缩放后位图宽/高（像素）。
    pub width: u32,
    pub height: u32,
    /// 相对基线的偏移（缩放后，像素）。
    pub bearing_x: i32,
    pub bearing_y: i32,
    /// 推进宽度（缩放后，像素）。
    pub advance: i32,
}

/// 一个 indexSubTable：覆盖 [first_glyph, last_glyph] 的字形位图位置。
struct SubTable {
    first_glyph: u16,
    last_glyph: u16,
    image_data_off: u32,
    /// indexFormat 1/2/3 的 locations 数组（每 glyph 起始偏移，末项为末尾）。
    locations: Vec<u32>,
}

/// 已解析的 CBDT/CBLC 字体。
pub struct EmojiFont {
    data: Vec<u8>,
    cbdt_off: usize,
    /// 第一个 strike 的位图尺寸表字段（像素/字体单位）。
    pub ppem: u16,
    subs: Vec<SubTable>,
}

fn rd_u16(d: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([d[off], d[off + 1]])
}

fn rd_u32(d: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

/// 越界安全的读取（畸形/截断字体不 panic，返回 None 由调用方放弃解析）。
fn rd_u16c(d: &[u8], off: usize) -> Option<u16> {
    if off + 2 <= d.len() {
        Some(rd_u16(d, off))
    } else {
        None
    }
}

fn rd_u32c(d: &[u8], off: usize) -> Option<u32> {
    if off + 4 <= d.len() {
        Some(rd_u32(d, off))
    } else {
        None
    }
}

impl EmojiFont {
    /// 从 TTF/OTF 字节解析 CBDT/CBLC。无 CBDT/CBLC 表返回 None。
    pub fn from_bytes(data: &[u8]) -> Option<EmojiFont> {
        if data.len() < 12 {
            return None;
        }
        let num_tables = rd_u16(data, 4) as usize;
        let mut cblc_off = None;
        let mut cbdt_off = None;
        for i in 0..num_tables {
            let base = 12 + i * 16;
            if base + 16 > data.len() {
                break;
            }
            let tag = &data[base..base + 4];
            let off = rd_u32(data, base + 8) as usize;
            let len = rd_u32(data, base + 12) as usize;
            if off + len > data.len() {
                continue;
            }
            match tag {
                b"CBLC" => cblc_off = Some(off),
                b"CBDT" => cbdt_off = Some(off),
                _ => {}
            }
        }
        let cblc = cblc_off?;
        let cbdt = cbdt_off?;

        // ---- CBLC：第一个 strike 的 BitmapSizeTable（48 字节） ----
        // 头：version(4) + numSizes(4)，之后紧邻 BitmapSizeTable 数组。
        // ★ 全链越界安全：截断的 emoji.ttf 直接放弃解析（None），不 panic。
        let num_sizes = rd_u32c(data, cblc.checked_add(4)?)? as usize;
        if num_sizes == 0 {
            return None;
        }
        let size_base = cblc.checked_add(8)?;
        let sub_array_off = rd_u32c(data, size_base)? as usize; // 相对 CBLC 基址
        let sub_count = rd_u32c(data, size_base.checked_add(8)?)? as usize;
        let ppem = if size_base + 44 < data.len() {
            data[size_base + 44] as u16
        } else {
            return None;
        };

        // ---- 解析全部 indexSubTable，统一字形范围 ----
        if sub_count == 0 {
            return None;
        }
        let arr = cblc.checked_add(sub_array_off)?;
        let mut subs = Vec::with_capacity(sub_count.min(1024));
        for e in 0..sub_count {
            let first_glyph = rd_u16c(data, arr.checked_add(e * 8)?)?;
            let last_glyph = rd_u16c(data, arr.checked_add(e * 8 + 2)?)?;
            // additionalOffsetToIndexSubtable 相对 indexSubTableArray 起点。
            let sub_rel = rd_u32c(data, arr.checked_add(e * 8 + 4)?)? as usize;
            let sub = cblc.checked_add(sub_array_off)?.checked_add(sub_rel)?;
            let index_format = rd_u16c(data, sub)?;
            let image_format = rd_u16c(data, sub.checked_add(2)?)?;
            let image_data_off = rd_u32c(data, sub.checked_add(4)?)?;
            if image_format != 17 {
                continue; // 仅支持 PNG 位图（Noto Color Emoji 全部为 17）
            }
            let count = (last_glyph - first_glyph + 1) as usize;
            let locations = match index_format {
                1 => {
                    // indexFormat 1：sbitLineMetrics(12) 后跟 u32 offsets[count+1]
                    let mut locs = Vec::with_capacity(count + 1);
                    for i in 0..=count {
                        locs.push(rd_u32(data, sub + 12 + i * 4));
                    }
                    locs
                }
                2 => {
                    // indexFormat 2：bigGlyphMetrics(8) 后跟 u32 offsets[count+1]
                    let mut locs = Vec::with_capacity(count + 1);
                    for i in 0..=count {
                        locs.push(rd_u32(data, sub + 8 + i * 4));
                    }
                    locs
                }
                3 => {
                    // indexFormat 3：u16 offsets[count+1]
                    let mut locs = Vec::with_capacity(count + 1);
                    for i in 0..=count {
                        locs.push(rd_u16(data, sub + 8 + i * 2) as u32);
                    }
                    locs
                }
                _ => continue,
            };
            subs.push(SubTable {
                first_glyph,
                last_glyph,
                image_data_off,
                locations,
            });
        }
        if subs.is_empty() {
            return None;
        }

        Some(EmojiFont {
            data: data.to_vec(),
            cbdt_off: cbdt,
            ppem,
            subs,
        })
    }

    /// 该字体是否包含 glyph id 的位图。
    pub fn has_glyph(&self, gid: u16) -> bool {
        self.subs
            .iter()
            .any(|s| gid >= s.first_glyph && gid <= s.last_glyph)
    }

    /// 取 glyph id 的 CBDT 条目（smallGlyphMetrics + PNG 数据）。
    /// 返回 (metrics5, png_data)。
    fn raw_entry(&self, gid: u16) -> Option<([u8; 5], &[u8])> {
        for s in self.subs.iter() {
            if gid < s.first_glyph || gid > s.last_glyph {
                continue;
            }
            let idx = (gid - s.first_glyph) as usize;
            if idx + 1 >= s.locations.len() {
                return None;
            }
            let start = s.locations[idx] as usize;
            let end = s.locations[idx + 1] as usize;
            if end <= start + 9 {
                return None;
            }
            let abs = self.cbdt_off + s.image_data_off as usize + start;
            let d = &self.data;
            if abs + (end - start) > d.len() {
                return None;
            }
            let mut m = [0u8; 5];
            m.copy_from_slice(&d[abs..abs + 5]);
            // format 17：smallGlyphMetrics(5) + uint32 dataLen + PNG
            let png_len = rd_u32(d, abs + 5) as usize;
            let png_start = abs + 9;
            if png_start + png_len > abs + (end - start) || png_start + png_len > d.len() {
                return None;
            }
            return Some((m, &d[png_start..png_start + png_len]));
        }
        None
    }

    /// 解码 glyph id 到位图，按 `target_h`（目标像素高度）等比缩放。
    /// 无位图/解码失败返回 None。结果按需缓存（调用方负责）。
    pub fn glyph(&self, gid: u16, target_h: u32) -> Option<EmojiBitmap> {
        let (m, png) = self.raw_entry(gid)?;
        let (h, w) = (m[0] as u32, m[1] as u32);
        let bearing_x = m[2] as i8 as i32;
        let bearing_y = m[3] as i8 as i32;
        let advance_units = m[4] as u32; // 字体单位（≈hmtx advance）

        let rgba = decode_png(png)?;
        if rgba.len() != (w * h * 4) as usize {
            return None;
        }
        // 缩放：target_h 目标像素高度（原始位图高度 h）
        let scale = target_h as f32 / h as f32;
        let tw = (w as f32 * scale).max(1.0) as u32;
        let th = (h as f32 * scale).max(1.0) as u32;
        let out = downscale_rgba(&rgba, w, h, tw, th);
        // advance：字体单位 → 像素。字体 upem 未知时按 ppem 缩放：
        //   ppem 下的像素推进 = advance_units / upem * ppem ≈ advance_units
        // 目标字号 target_h 下推进 = advance_units * scale
        Some(EmojiBitmap {
            rgba: out,
            width: tw,
            height: th,
            bearing_x: (bearing_x as f32 * scale).round() as i32,
            bearing_y: (bearing_y as f32 * scale).round() as i32,
            advance: (advance_units as f32 * scale).round() as i32,
        })
    }
}

// ==================== 最小 PNG 解码器 ====================

/// 解码 PNG（bit depth 8、非隔行、颜色类型 2/3/6）为 RGBA8888。
pub fn decode_png(data: &[u8]) -> Option<Vec<u8>> {
    if !(data.len() > 8 && &data[..8] == b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut pos = 8usize;
    let mut w = 0u32;
    let mut h = 0u32;
    let mut color_type = 0u8;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new(); // 调色板 alpha 或 RGB key
    let mut idat: Vec<u8> = Vec::new();
    let mut saw_ihdr = false;
    while pos + 8 <= data.len() {
        let len = rd_u32(data, pos) as usize;
        let ctype = &data[pos + 4..pos + 8];
        let body = pos + 8;
        if body + len > data.len() {
            return None;
        }
        match ctype {
            b"IHDR" => {
                if len < 13 {
                    return None;
                }
                w = rd_u32(data, body);
                h = rd_u32(data, body + 4);
                let bit_depth = data[body + 8];
                let interlace = data[body + 12];
                if bit_depth != 8 || interlace != 0 {
                    return None;
                }
                color_type = data[body + 9];
                if !(color_type == 2 || color_type == 3 || color_type == 6) {
                    return None;
                }
                saw_ihdr = true;
            }
            b"PLTE" => {
                if len % 3 != 0 {
                    return None;
                }
                for i in (0..len).step_by(3) {
                    palette.push([data[body + i], data[body + i + 1], data[body + i + 2]]);
                }
            }
            b"tRNS" => {
                trns.extend_from_slice(&data[body..body + len]);
            }
            b"IDAT" => {
                idat.extend_from_slice(&data[body..body + len]);
            }
            b"IEND" => break,
            _ => {}
        }
        pos = body + len + 4; // 跳过 CRC
    }
    if !saw_ihdr || w == 0 || h == 0 {
        return None;
    }

    // ---- 解压（inflate） ----
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).ok()?;
    let bpp = match color_type {
        2 => 3,
        3 => 1,
        6 => 4,
        _ => return None,
    };
    let stride = (w as usize) * bpp;
    if raw.len() < h as usize * (stride + 1) {
        return None;
    }

    // ---- 反滤波（unfilter） ----
    let mut out = vec![0u8; h as usize * stride];
    for y in 0..h as usize {
        let ft = raw[y * (stride + 1)];
        let row_in = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let (prev_part, cur_part) = out.split_at_mut(y * stride);
        let row_out = &mut cur_part[..stride];
        let prev: &[u8] = if y == 0 { &[] } else { &prev_part[(y - 1) * stride..y * stride] };
        for x in 0..stride {
            let raw_v = row_in[x];
            let left = if x >= bpp { row_out[x - bpp] } else { 0 };
            let up = if y == 0 { 0 } else { prev[x] };
            let ul = if y == 0 || x < bpp { 0 } else { prev[x - bpp] };
            let v = match ft {
                0 => raw_v,
                1 => raw_v.wrapping_add(left),
                2 => raw_v.wrapping_add(up),
                3 => raw_v.wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => {
                    // Paeth
                    let p = left as i32 + up as i32 - ul as i32;
                    let pa = (p - left as i32).abs();
                    let pb = (p - up as i32).abs();
                    let pc = (p - ul as i32).abs();
                    let pred = if pa <= pb && pa <= pc {
                        left
                    } else if pb <= pc {
                        up
                    } else {
                        ul
                    };
                    raw_v.wrapping_add(pred)
                }
                _ => return None,
            };
            row_out[x] = v;
        }
    }

    // ---- 颜色转换到 RGBA ----
    let n = (w * h) as usize;
    let mut rgba = vec![0u8; n * 4];
    match color_type {
        2 => {
            // RGB + 可选 tRNS 单色透明
            let key = if trns.len() >= 3 {
                Some((trns[0], trns[1], trns[2]))
            } else {
                None
            };
            for i in 0..n {
                let r = out[i * 3];
                let g = out[i * 3 + 1];
                let b = out[i * 3 + 2];
                let a = if key == Some((r, g, b)) { 0 } else { 255 };
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[r, g, b, a]);
            }
        }
        3 => {
            for i in 0..n {
                let idx = out[i] as usize;
                let (r, g, b) = if idx < palette.len() {
                    (palette[idx][0], palette[idx][1], palette[idx][2])
                } else {
                    (0, 0, 0)
                };
                let a = if idx < trns.len() { trns[idx] } else { 255 };
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[r, g, b, a]);
            }
        }
        6 => {
            if out.len() != rgba.len() {
                return None;
            }
            rgba.copy_from_slice(&out);
        }
        _ => return None,
    }
    Some(rgba)
}

/// 双线性缩小 RGBA 位图。
pub fn downscale_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if dw == sw && dh == sh {
        return src.to_vec();
    }
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        let sy = (y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5;
        let y0 = sy.floor().max(0.0) as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let fy = (sy - y0 as f32).clamp(0.0, 1.0);
        for x in 0..dw {
            let sx = (x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5;
            let x0 = sx.floor().max(0.0) as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let fx = (sx - x0 as f32).clamp(0.0, 1.0);
            for c in 0..4 {
                let a = src[((y0 * sw + x0) * 4 + c) as usize] as f32;
                let b = src[((y0 * sw + x1) * 4 + c) as usize] as f32;
                let cc = src[((y1 * sw + x0) * 4 + c) as usize] as f32;
                let d = src[((y1 * sw + x1) * 4 + c) as usize] as f32;
                let top = a + (b - a) * fx;
                let bot = cc + (d - cc) * fx;
                out[((y * dw + x) * 4 + c) as usize] = (top + (bot - top) * fy).round() as u8;
            }
        }
    }
    out
}
