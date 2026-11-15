#[cfg(test)]
mod tests {
    use kunxfont::Font;
    use std::{path::Path, sync::Arc};
    const NAMES: [&str; 8] = ["cn", "bold", "oobe-cn", "oobe-bold", "launcher-cn", "launcher-bold", "icon-fill", "icon-line"];
    fn fonts() -> Vec<Font> {
        NAMES.iter().map(|name| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../rootfs/fonts/{name}.ttf"));
            Font::from_bytes(&std::fs::read(path).unwrap()).unwrap()
        }).collect()
    }
    #[test]
    fn character_maps_match_captured_upstream() {
        let fonts = fonts();
        for line in include_str!("../data/upstream-mapping.txt").lines() {
            let values: Vec<u32> = line.split_whitespace().map(|v| v.parse().unwrap()).collect();
            let ch = char::from_u32(values[1]).unwrap();
            assert_eq!(fonts[values[0] as usize].lookup_glyph_index(ch), values[2] as u16, "{values:?}");
        }
        for font in &fonts { assert!(!font.has_glyph('\u{10ffff}')); }
    }
    #[test]
    fn metrics_and_grayscale_match_captured_upstream() {
        let fonts = fonts();
        let mut data = &include_bytes!("../data/upstream-glyphs.bin")[..];
        let mut count = 0;
        let mut max_error = 0.0f32;
        while !data.is_empty() {
            let fi = data[0] as usize;
            let px = data[1] as f32;
            let cp = u32::from_le_bytes(data[2..6].try_into().unwrap());
            let xmin = i32::from_le_bytes(data[6..10].try_into().unwrap());
            let ymin = i32::from_le_bytes(data[10..14].try_into().unwrap());
            let width = u16::from_le_bytes(data[14..16].try_into().unwrap()) as usize;
            let height = u16::from_le_bytes(data[16..18].try_into().unwrap()) as usize;
            let advance = f32::from_le_bytes(data[18..22].try_into().unwrap());
            let reference = &data[22..22 + width * height];
            data = &data[22 + width * height..];
            let (m, bitmap) = fonts[fi].rasterize(char::from_u32(cp).unwrap(), px).unwrap();
            assert!((m.advance_width - advance).abs() < 0.001, "advance font={fi} cp={cp} px={px}");
            assert!((m.xmin - xmin).abs() <= 1 && (m.ymin - ymin).abs() <= 1);
            assert!(m.width.abs_diff(width) <= 1 && m.height.abs_diff(height) <= 1);
            let mut error = 0u64;
            let mut area = 0;
            for y in ymin.min(m.ymin)..(ymin + height as i32).max(m.ymin + m.height as i32) {
                for x in xmin.min(m.xmin)..(xmin + width as i32).max(m.xmin + m.width as i32) {
                    let get = |ox: i32, oy: i32, w: usize, h: usize, buf: &[u8]| -> i32 {
                        let (xx, yy) = (x - ox, y - oy);
                        if xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h {
                            buf[(h - 1 - yy as usize) * w + xx as usize] as i32
                        } else { 0 }
                    };
                    error += (get(xmin, ymin, width, height, reference)
                        - get(m.xmin, m.ymin, m.width, m.height, &bitmap)).unsigned_abs() as u64;
                    area += 1;
                }
            }
            let mean = error as f32 / (area as f32).max(1.0);
            // Different scan conversion permits small coverage rounding differences.
            assert!(mean <= 8.0, "coverage font={fi} cp={cp:x} px={px} mean={mean}");
            max_error = max_error.max(mean);
            count += 1;
        }
        assert!(count > 3000);
        eprintln!("{count} glyphs compared; worst mean pixel difference {max_error:.3}/255");
    }
    #[test]
    fn line_metrics_and_concurrent_clone() {
        let fonts = fonts();
        let m = fonts[0].horizontal_line_metrics(16.0).unwrap();
        assert!((m.ascent - 15.488).abs() < 0.001);
        assert!((m.descent + 3.856).abs() < 0.001);
        let m = fonts[6].horizontal_line_metrics(16.0).unwrap();
        assert_eq!((m.ascent, m.descent), (15.0, -1.0));
        let font = Arc::new(fonts[0].clone());
        drop(fonts);
        let threads: Vec<_> = (0..8).map(|_| {
            let font = font.clone();
            std::thread::spawn(move || font.rasterize('动', 24.0).unwrap())
        }).collect();
        let reference = font.rasterize('动', 24.0).unwrap();
        assert!(threads.into_iter().all(|thread| thread.join().unwrap() == reference));
    }
    #[test]
    fn all_original_font_outlines_decode() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts");
        for name in ["uiputong", "bold", "Phosphor-Fill", "Phosphor-Bold"] {
            let bytes = std::fs::read(root.join(format!("{name}.ttf"))).unwrap();
            let font = Font::from_bytes(&bytes).unwrap();
            for cp in 0..=0x10ffff {
                let Some(ch) = char::from_u32(cp) else { continue; };
                if font.has_glyph(ch) {
                    let (m, pixels) = font.rasterize(ch, 16.0).unwrap_or_else(|err| panic!("{name} U+{cp:04x}: {err:?}"));
                    assert_eq!(pixels.len(), m.width * m.height);
                }
            }
        }
    }
}
