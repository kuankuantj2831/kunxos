#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Instant};
    fn fonts(name: &str) -> Vec<u8> {
        std::fs::read(format!("../../rootfs/fonts/{name}.ttf")).unwrap()
    }
    #[test]
    fn matches_upstream_metrics_and_pixels() {
        for name in ["cn", "bold", "icon-fill", "icon-line"] {
            let data = fonts(name);
            let start = Instant::now();
            let original = eager::Font::from_bytes(data.as_slice(), eager::FontSettings::default()).unwrap();
            let eager_us = start.elapsed().as_micros();
            let start = Instant::now();
            let cached = lazy::Font::from_bytes(data.as_slice(), lazy::FontSettings::default()).unwrap();
            eprintln!("{name}: eager={eager_us}us lazy={}us", start.elapsed().as_micros());
            for ch in "Hello, KunXOS! 设置网络应用动画中文测试缺字🦀".chars().chain((0xe000..0xe080).filter_map(char::from_u32)) {
                for size in [12.0, 16.0, 28.0] {
                    let (a, pixels_a) = original.rasterize(ch, size);
                    let (b, pixels_b) = cached.rasterize(ch, size);
                    assert_eq!(format!("{a:?}"), format!("{b:?}"), "{name} {ch}");
                    assert_eq!(pixels_a, pixels_b, "{name} {ch}");
                    assert_eq!(cached.rasterize(ch, size).1, pixels_a);
                }
            }
            let clone = cached.clone();
            drop(cached);
            assert_eq!(clone.rasterize('设', 18.0).1, original.rasterize('设', 18.0).1);
        }
    }
    #[test]
    fn concurrent_first_use_publishes_complete_geometry() {
        let font = Arc::new(lazy::Font::from_bytes(fonts("cn"), lazy::FontSettings::default()).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8).map(|_| {
            let font = font.clone(); let barrier = barrier.clone();
            std::thread::spawn(move || { barrier.wait(); font.rasterize('动', 24.0).1 })
        }).collect();
        let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert!(results.iter().all(|pixels| pixels == &results[0]));
    }
}
