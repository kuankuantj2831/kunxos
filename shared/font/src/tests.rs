use super::*;
use alloc::{vec, vec::Vec};

fn put16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}
fn put32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn simple(contours: &[&[(i16, i16)]]) -> Vec<u8> {
    let mut data = vec![0; 10];
    put16(&mut data, 0, contours.len() as u16);
    let points: Vec<_> = contours.iter().flat_map(|c| c.iter()).copied().collect();
    let mut end = 0;
    for contour in contours {
        end += contour.len();
        data.extend_from_slice(&((end - 1) as u16).to_be_bytes());
    }
    data.extend_from_slice(&0u16.to_be_bytes()); // no instructions
    data.extend(core::iter::repeat(1).take(points.len())); // on-curve, signed deltas
    for axis in 0..2 {
        let mut previous = 0i16;
        for &(x, y) in &points {
            let value = if axis == 0 { x } else { y };
            data.extend_from_slice(&value.wrapping_sub(previous).to_be_bytes());
            previous = value;
        }
    }
    data
}

fn cmap12(glyphs: usize) -> Vec<u8> {
    let mut cmap = vec![0; 40];
    put16(&mut cmap, 2, 1); put16(&mut cmap, 4, 3); put16(&mut cmap, 6, 10);
    put32(&mut cmap, 8, 12); put16(&mut cmap, 12, 12); put32(&mut cmap, 16, 28);
    put32(&mut cmap, 24, 1); put32(&mut cmap, 28, 65);
    put32(&mut cmap, 32, 65 + glyphs as u32 - 1); put32(&mut cmap, 36, 1);
    cmap
}

fn font_data(glyphs: &[Vec<u8>], cmap: Option<Vec<u8>>) -> Vec<u8> {
    let mut head = vec![0; 54];
    put32(&mut head, 12, 0x5f0f3cf5); put16(&mut head, 18, 1024); put16(&mut head, 50, 1);
    let mut hhea = vec![0; 36]; put16(&mut hhea, 4, 1024); put16(&mut hhea, 34, glyphs.len() as u16 + 1);
    let mut maxp = vec![0; 6]; put16(&mut maxp, 4, glyphs.len() as u16 + 1);
    let mut loca = vec![0; 8]; // glyph zero is empty
    let mut glyf = Vec::new();
    for glyph in glyphs {
        glyf.extend_from_slice(glyph);
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
    }
    let mut hmtx = Vec::new();
    for _ in 0..=glyphs.len() { hmtx.extend_from_slice(&[4, 0, 0, 0]); }
    let tables = [
        (*b"head", head), (*b"hhea", hhea), (*b"maxp", maxp),
        (*b"loca", loca), (*b"glyf", glyf), (*b"hmtx", hmtx),
        (*b"cmap", cmap.unwrap_or_else(|| cmap12(glyphs.len()))),
    ];
    let mut data = vec![0; 12 + tables.len() * 16];
    put32(&mut data, 0, 0x10000); put16(&mut data, 4, tables.len() as u16);
    for (i, (tag, bytes)) in tables.into_iter().enumerate() {
        let len = bytes.len(); let offset = data.len(); let p = 12 + i * 16;
        data[p..p + 4].copy_from_slice(&tag);
        put32(&mut data, p + 8, offset as u32); put32(&mut data, p + 12, len as u32);
        data.extend(bytes);
    }
    data
}

fn square() -> Vec<u8> { simple(&[&[(0, 0), (1024, 0), (1024, 1024), (0, 1024)]]) }

#[test]
fn exact_square_and_nonzero_winding_hole() {
    let hole = simple(&[
        &[(0, 0), (1024, 0), (1024, 1024), (0, 1024)],
        &[(256, 256), (256, 768), (768, 768), (768, 256)],
    ]);
    let font = Font::from_bytes(&font_data(&[square(), hole], None)).unwrap();
    let (m, bitmap) = font.rasterize('A', 16.0).unwrap();
    assert_eq!((m.xmin, m.ymin, m.width, m.height), (0, 0, 16, 16));
    assert_eq!(bitmap, vec![255; 256]);
    let (_, bitmap) = font.rasterize('B', 16.0).unwrap();
    assert_eq!(bitmap.iter().map(|p| *p as usize).sum::<usize>(), (256 - 64) * 255);
    assert_eq!(bitmap[8 * 16 + 8], 0);
}

#[test]
fn composite_scale_translation_and_point_attachment() {
    // Glyph B: half-size A shifted right/up by 256 font units.
    let mut compound = vec![0; 10]; put16(&mut compound, 0, 0xffff);
    for word in [0x000bu16, 1, 256, 256, 8192] { compound.extend(word.to_be_bytes()); }
    // Glyph C: A followed by a second A attached at parent point 1 to child point 0.
    let mut attached = vec![0; 10]; put16(&mut attached, 0, 0xffff);
    for word in [0x0023u16, 1, 0, 0, 0x0001, 1, 1, 0] { attached.extend(word.to_be_bytes()); }
    let font = Font::from_bytes(&font_data(&[square(), compound, attached], None)).unwrap();
    let (m, pixels) = font.rasterize('B', 16.0).unwrap();
    assert_eq!((m.xmin, m.ymin, m.width, m.height), (4, 4, 8, 8));
    assert_eq!(pixels, vec![255; 64]);
    let (m, pixels) = font.rasterize('C', 16.0).unwrap();
    assert_eq!((m.width, m.height), (32, 16));
    assert_eq!(pixels, vec![255; 512]);
}

#[test]
fn cyclic_composite_and_bad_repeat_are_rejected() {
    let mut cycle = vec![0; 10]; put16(&mut cycle, 0, 0xffff);
    for word in [3u16, 2, 0, 0] { cycle.extend(word.to_be_bytes()); }
    let font = Font::from_bytes(&font_data(&[square(), cycle], None)).unwrap();
    assert_eq!(font.rasterize('B', 16.0), Err(Error::ComplexityLimit));
    let mut repeat = square(); repeat[14] = 9; repeat[15] = 255;
    let font = Font::from_bytes(&font_data(&[repeat], None)).unwrap();
    assert_eq!(font.rasterize('A', 16.0), Err(Error::InvalidFont));
}

#[test]
fn cmap4_range_offsets_zero_and_delta() {
    let mut cmap = vec![0; 48];
    put16(&mut cmap, 2, 1); put16(&mut cmap, 4, 3); put16(&mut cmap, 6, 1); put32(&mut cmap, 8, 12);
    let d = &mut cmap[12..];
    put16(d, 0, 4); put16(d, 2, 36); put16(d, 6, 4);
    put16(d, 14, 66); put16(d, 16, 0xffff);
    put16(d, 20, 65); put16(d, 22, 0xffff);
    put16(d, 24, 1); put16(d, 26, 1);
    put16(d, 28, 4); put16(d, 30, 0);
    put16(d, 32, 1); put16(d, 34, 0);
    let font = Font::from_bytes(&font_data(&[square(), square()], Some(cmap))).unwrap();
    assert_eq!(font.lookup_glyph_index('A'), 2);
    assert_eq!(font.lookup_glyph_index('B'), 0);
    assert_eq!(font.lookup_glyph_index('@'), 0);
    assert_eq!(font.lookup_glyph_index('\u{1f600}'), 0);
}

#[test]
fn cmap12_supplementary_and_empty_glyph() {
    let mut cmap = cmap12(1); put32(&mut cmap, 28, 0x20000); put32(&mut cmap, 32, 0x20000);
    let font = Font::from_bytes(&font_data(&[Vec::new()], Some(cmap))).unwrap();
    assert!(font.has_glyph('\u{20000}'));
    let (m, bitmap) = font.rasterize('\u{20000}', 16.0).unwrap();
    assert_eq!((m.width, m.height, m.advance_width), (0, 0, 16.0));
    assert!(bitmap.is_empty());
}

#[test]
fn invalid_sizes_and_header_ranges() {
    let data = font_data(&[square()], None);
    let font = Font::from_bytes(&data).unwrap();
    for px in [0.0, -1.0, 513.0, f32::NAN, f32::INFINITY] {
        assert_eq!(font.rasterize('A', px), Err(Error::InvalidSize));
        assert!(font.horizontal_line_metrics(px).is_none());
    }
    let mut bad = data.clone(); put32(&mut bad, 20, u32::MAX);
    assert!(Font::from_bytes(&bad).is_err());
    let mut bad = data.clone(); put32(&mut bad, 24, u32::MAX);
    assert!(Font::from_bytes(&bad).is_err());
    for end in 0..data.len() { let _ = Font::from_bytes(&data[..end]); }
}

#[test]
fn malformed_fonts_never_panic() {
    let original = font_data(&[square()], None);
    let mut seed = 0x82762591u32;
    for _ in 0..4000 {
        let mut data = original.clone();
        for _ in 0..3 {
            seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
            let p = seed as usize % data.len(); data[p] ^= (seed >> 16) as u8;
        }
        if let Ok(font) = Font::from_bytes(&data) {
            for ch in ['A', 'B', '\u{20000}'] { let _ = font.rasterize(ch, 16.0); }
        }
    }
}
