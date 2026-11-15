//! KunxOS TrueType outline rendering, implemented from the OpenType glyf/cmap
//! specifications. No third-party code or dependencies; no hinting interpreter.
//! Font bytes are shared immutably; outlines are decoded only when requested.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;
use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

mod outline;
mod raster;
#[cfg(test)]
mod tests;

/// Invalid, unsupported or excessively complex input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidFont,
    UnsupportedFormat,
    ComplexityLimit,
    InvalidSize,
}
type Result<T> = core::result::Result<T, Error>;

/// Bitmap origin is relative to the baseline, with positive Y pointing up.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance_width: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub new_line_size: f32,
}

/// Owned, thread-safe font; cloning only increments the byte buffer's refcount.
#[derive(Clone)]
pub struct Font {
    data: Arc<[u8]>,
    glyf: Range<usize>,
    loca: Range<usize>,
    hmtx: Range<usize>,
    cmap: Range<usize>,
    cmap_format: u16,
    units: u16,
    glyphs: u16,
    hmetrics: u16,
    long_loca: bool,
    ascent: i16,
    descent: i16,
    gap: i16,
}

fn bytes(data: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    data.get(offset..offset.checked_add(len).ok_or(Error::InvalidFont)?)
        .ok_or(Error::InvalidFont)
}
fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    let b = bytes(data, offset, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}
fn i16_at(data: &[u8], offset: usize) -> Result<i16> {
    Ok(u16_at(data, offset)? as i16)
}
fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    let b = bytes(data, offset, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

impl Font {
    /// Accept a standalone TrueType SFNT. CFF, collections, variations and WOFF
    /// are deliberately rejected rather than interpreted as TrueType outlines.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() > 64 * 1024 * 1024 {
            return Err(Error::ComplexityLimit);
        }
        if u32_at(data, 0)? != 0x0001_0000 {
            return Err(Error::UnsupportedFormat);
        }
        let count = u16_at(data, 4)? as usize;
        if count == 0 || count > 128 {
            return Err(Error::InvalidFont);
        }
        bytes(data, 12, count * 16)?;
        for i in 0..count {
            if &data[12 + i * 16..16 + i * 16] == b"fvar" {
                return Err(Error::UnsupportedFormat);
            }
        }
        let table = |tag: &[u8; 4]| -> Result<Range<usize>> {
            let mut found = None;
            for i in 0..count {
                let p = 12 + i * 16;
                let start = u32_at(data, p + 8)? as usize;
                let len = u32_at(data, p + 12)? as usize;
                bytes(data, start, len)?;
                if &data[p..p + 4] == tag {
                    if found.is_some() { return Err(Error::InvalidFont); }
                    found = Some(start..start + len);
                }
            }
            found.ok_or(Error::InvalidFont)
        };
        let head = &data[table(b"head")?];
        let hhea = &data[table(b"hhea")?];
        let maxp = &data[table(b"maxp")?];
        if u32_at(head, 12)? != 0x5f0f_3cf5 { return Err(Error::InvalidFont); }
        let units = u16_at(head, 18)?;
        let glyphs = u16_at(maxp, 4)?;
        let hmetrics = u16_at(hhea, 34)?;
        let loca_format = i16_at(head, 50)?;
        if !(16..=16384).contains(&units) || glyphs == 0 || hmetrics == 0
            || hmetrics > glyphs || !(0..=1).contains(&loca_format) {
            return Err(Error::InvalidFont);
        }
        let glyf = table(b"glyf")?;
        let loca = table(b"loca")?;
        let hmtx = table(b"hmtx")?;
        let long_loca = loca_format == 1;
        let stride = if long_loca { 4 } else { 2 };
        bytes(&data[loca.clone()], 0, (glyphs as usize + 1) * stride)?;
        bytes(&data[hmtx.clone()], 0, hmetrics as usize * 4 + (glyphs - hmetrics) as usize * 2)?;
        let mut previous = 0;
        for i in 0..=glyphs as usize {
            let offset = if long_loca { u32_at(&data[loca.clone()], i * 4)? as usize }
                else { u16_at(&data[loca.clone()], i * 2)? as usize * 2 };
            if offset < previous || offset > glyf.len() { return Err(Error::InvalidFont); }
            previous = offset;
        }
        let cmap_table = table(b"cmap")?;
        let cmap_data = &data[cmap_table.clone()];
        let records = u16_at(cmap_data, 2)? as usize;
        bytes(cmap_data, 4, records * 8)?;
        let mut selected = None;
        let mut priority = 0;
        for i in 0..records {
            let p = 4 + i * 8;
            let platform = u16_at(cmap_data, p)?;
            let encoding = u16_at(cmap_data, p + 2)?;
            if platform != 0 && !(platform == 3 && (encoding == 1 || encoding == 10)) { continue; }
            let offset = u32_at(cmap_data, p + 4)? as usize;
            let format = u16_at(cmap_data, offset)?;
            let rank = match format { 12 => 2, 4 => 1, _ => continue };
            if rank <= priority { continue; }
            let len = if format == 12 { u32_at(cmap_data, offset + 4)? as usize }
                else { u16_at(cmap_data, offset + 2)? as usize };
            let sub = bytes(cmap_data, offset, len)?;
            validate_cmap(sub, format, glyphs)?;
            selected = Some((cmap_table.start + offset..cmap_table.start + offset + len, format));
            priority = rank;
        }
        let (cmap, cmap_format) = selected.ok_or(Error::UnsupportedFormat)?;
        let (mut ascent, mut descent, mut gap) = (i16_at(hhea, 4)?, i16_at(hhea, 6)?, i16_at(hhea, 8)?);
        // OpenType USE_TYPO_METRICS takes precedence over hhea metrics.
        if let Ok(range) = table(b"OS/2") {
            let os2 = &data[range];
            if u16_at(os2, 62)? & 0x80 != 0 {
                ascent = i16_at(os2, 68)?;
                descent = i16_at(os2, 70)?;
                gap = i16_at(os2, 72)?;
            }
        }
        Ok(Self {
            data: Arc::from(data), glyf, loca, hmtx, cmap, cmap_format,
            units, glyphs, hmetrics, long_loca,
            ascent, descent, gap,
        })
    }

    /// Missing/invalid mapping returns glyph zero (.notdef).
    pub fn lookup_glyph_index(&self, ch: char) -> u16 {
        self.lookup(ch as u32).unwrap_or(0)
    }

    pub fn has_glyph(&self, ch: char) -> bool { self.lookup_glyph_index(ch) != 0 }

    fn lookup(&self, cp: u32) -> Option<u16> {
        let d = &self.data[self.cmap.clone()];
        let id = if self.cmap_format == 12 {
            let count = u32_at(d, 12).ok()? as usize;
            let (mut low, mut high) = (0, count);
            let mut found = 0;
            while low < high {
                let mid = low + (high - low) / 2;
                let p = 16 + mid * 12;
                let start = u32_at(d, p).ok()?;
                let end = u32_at(d, p + 4).ok()?;
                if cp < start { high = mid; }
                else if cp > end { low = mid + 1; }
                else { found = u32_at(d, p + 8).ok()?.checked_add(cp - start)?; break; }
            }
            found
        } else {
            if cp > 0xffff { return None; }
            let count = u16_at(d, 6).ok()? as usize / 2;
            let (mut low, mut high) = (0, count);
            while low < high {
                let mid = low + (high - low) / 2;
                if (u16_at(d, 14 + mid * 2).ok()? as u32) < cp { low = mid + 1; }
                else { high = mid; }
            }
            if low == count { return None; }
            let start = u16_at(d, 16 + count * 2 + low * 2).ok()? as u32;
            if cp < start { return None; }
            let delta = u16_at(d, 16 + count * 4 + low * 2).ok()?;
            let pos = 16 + count * 6 + low * 2;
            let offset = u16_at(d, pos).ok()? as usize;
            if offset == 0 { (cp as u16).wrapping_add(delta) as u32 }
            else {
                let raw = u16_at(d, pos.checked_add(offset)?.checked_add((cp - start) as usize * 2)?).ok()?;
                if raw == 0 { 0 } else { raw.wrapping_add(delta) as u32 }
            }
        };
        if id < self.glyphs as u32 { Some(id as u16) } else { None }
    }

    pub fn horizontal_line_metrics(&self, px: f32) -> Option<LineMetrics> {
        if !valid_size(px) { return None; }
        let scale = px / self.units as f32;
        Some(LineMetrics {
            ascent: self.ascent as f32 * scale,
            descent: self.descent as f32 * scale,
            line_gap: self.gap as f32 * scale,
            new_line_size: (self.ascent as i32 - self.descent as i32 + self.gap as i32) as f32 * scale,
        })
    }

    /// Antialiased bitmap, top row first. Invalid outlines return an error;
    /// callers can retain their existing missing-glyph fallback.
    pub fn rasterize(&self, ch: char, px: f32) -> Result<(Metrics, Vec<u8>)> {
        if !valid_size(px) { return Err(Error::InvalidSize); }
        let id = self.lookup_glyph_index(ch);
        let advance = u16_at(&self.data[self.hmtx.clone()], id.min(self.hmetrics - 1) as usize * 4)?;
        let scale = px / self.units as f32;
        let shape = outline::decode(self, id)?;
        raster::render(&shape, scale, advance as f32 * scale)
    }

    fn glyph_data(&self, id: u16) -> Result<&[u8]> {
        if id >= self.glyphs { return Err(Error::InvalidFont); }
        let loca = &self.data[self.loca.clone()];
        let offset = |id: usize| -> Result<usize> {
            if self.long_loca { Ok(u32_at(loca, id * 4)? as usize) }
            else { Ok(u16_at(loca, id * 2)? as usize * 2) }
        };
        let a = offset(id as usize)?;
        let b = offset(id as usize + 1)?;
        self.data[self.glyf.clone()].get(a..b).ok_or(Error::InvalidFont)
    }
}

fn valid_size(px: f32) -> bool { px.is_finite() && px > 0.0 && px <= 512.0 }

fn validate_cmap(d: &[u8], format: u16, glyphs: u16) -> Result<()> {
    if format == 12 {
        let n = u32_at(d, 12)? as usize;
        if n > 0x110000 { return Err(Error::ComplexityLimit); }
        bytes(d, 16, n * 12)?;
        let mut previous = None;
        for i in 0..n {
            let p = 16 + i * 12;
            let (a, b, id) = (u32_at(d, p)?, u32_at(d, p + 4)?, u32_at(d, p + 8)?);
            if a > b || b > 0x10ffff || previous.is_some_and(|end| a <= end)
                || id.checked_add(b - a).ok_or(Error::InvalidFont)? >= glyphs as u32 {
                return Err(Error::InvalidFont);
            }
            previous = Some(b);
        }
    } else {
        let n2 = u16_at(d, 6)? as usize;
        if n2 == 0 || n2 % 2 != 0 { return Err(Error::InvalidFont); }
        let n = n2 / 2;
        bytes(d, 0, 16 + n * 8)?;
        let mut previous = None;
        for i in 0..n {
            let end = u16_at(d, 14 + i * 2)?;
            let start = u16_at(d, 16 + n * 2 + i * 2)?;
            let p = 16 + n * 6 + i * 2;
            let offset = u16_at(d, p)? as usize;
            if start > end || previous.is_some_and(|last| start <= last) || offset % 2 != 0 {
                return Err(Error::InvalidFont);
            }
            if offset != 0 { bytes(d, p + offset, (end - start) as usize * 2 + 2)?; }
            previous = Some(end);
        }
    }
    Ok(())
}
