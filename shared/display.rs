//! Display contract shared by the kernel and user interfaces (no allocation).
//! Sizes are physical pixels; density is in 1/1024 units, rates in millihertz.
//! A timer deadline is a pacing target, never a claim that scanout has VSYNC.

pub const INFO_IOCTL: usize = 0x4604;
/// Opt-in clients commit the latest host request between frames, then reread INFO.
pub const RESIZE_IOCTL: usize = 0x4605;
pub const MAX_WIDTH: usize = 1920;
pub const MAX_HEIGHT: usize = 1200;
pub const MAX_PIXELS: usize = MAX_WIDTH * MAX_HEIGHT;
pub const INFO_VERSION: u32 = 1;
pub const INFO_BYTES: usize = 40;
pub const CAP_VSYNC: u32 = 1;
pub const CAP_RESIZE: u32 = 2;
/// The host draws an independent pointer; applications must suppress software sprites.
pub const CAP_HOST_CURSOR: u32 = 4;
pub const DEFAULT_REFRESH_MHZ: u32 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMetrics {
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub density: u32,
    pub refresh_millihz: u32,
    pub target_millihz: u32,
    pub capabilities: u32,
}

impl DisplayMetrics {
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < INFO_BYTES { return None; }
        let word = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        if word(0) != INFO_VERSION { return None; }
        let result = Self {
            generation: word(4), width: word(8), height: word(12),
            stride_bytes: word(16), density: word(20),
            refresh_millihz: word(24), target_millihz: word(28),
            capabilities: word(32),
        };
        if result.width == 0 || result.height == 0 || result.width > 4096
            || result.height > 4096 || result.stride_bytes < result.width * 4
            || result.stride_bytes % 4 != 0 || result.stride_bytes > 65536
            || !(256..=8192).contains(&result.density)
            || (result.refresh_millihz != 0 && !(1_000..=1_000_000).contains(&result.refresh_millihz))
            || result.target_millihz == 0
            || result.target_millihz > 1_000_000
            || (result.refresh_millihz != 0 && result.target_millihz > result.refresh_millihz)
        { return None; }
        Some(result)
    }

    pub fn encode(self) -> [u8; INFO_BYTES] {
        let mut out = [0; INFO_BYTES];
        let words = [INFO_VERSION, self.generation, self.width, self.height,
            self.stride_bytes, self.density, self.refresh_millihz,
            self.target_millihz, self.capabilities, 0];
        for (i, value) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        out
    }

    pub fn layout(self) -> Layout {
        Layout {
            width_dp: (self.width as u64 * 1024 / self.density as u64) as u32,
            height_dp: (self.height as u64 * 1024 / self.density as u64) as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout { pub width_dp: u32, pub height_dp: u32 }

impl Layout {
    /// Fit as many 80dp cells as the available width allows, with 24dp margins.
    pub fn columns(self) -> u32 { (self.width_dp.saturating_sub(48) / 80).clamp(1, 12) }
    pub fn wide(self) -> bool { self.width_dp >= 840 }
}

/// Absolute frame deadlines avoid accumulating render time into every interval.
/// Missed slots are skipped; there is no burst of catch-up frames after a stall.
pub struct FrameClock {
    rate: u32,
    epoch: u64,
    slot: u64,
    initialized: bool,
}

impl FrameClock {
    pub const fn new() -> Self {
        Self { rate: 0, epoch: 0, slot: 0, initialized: false }
    }

    pub fn reset(&mut self) { self.initialized = false; }

    /// Call once at frame end. Returns the remaining sleep, in microseconds.
    /// `start` excludes sleeping so a slow frame never earns an extra full wait.
    pub fn wait_us(&mut self, start: u64, end: u64, rate: u32) -> u64 {
        let rate = rate.clamp(1_000, 1_000_000);
        if !self.initialized || self.rate != rate || end < self.epoch || start < self.epoch {
            self.rate = rate;
            self.epoch = start;
            self.slot = 1;
            self.initialized = true;
        } else {
            let previous = self.epoch.saturating_add(self.slot.saturating_mul(1_000_000_000) / rate as u64);
            if start >= previous {
                self.slot = (self.slot + 1).max(start.saturating_sub(self.epoch).saturating_mul(rate as u64)
                    / 1_000_000_000 + 1);
            }
        }
        let deadline = self.epoch.saturating_add(self.slot.saturating_mul(1_000_000_000) / rate as u64);
        if end >= deadline {
            self.slot = end.saturating_sub(self.epoch).saturating_mul(rate as u64)
                / 1_000_000_000 + 1;
            return 0;
        }
        deadline - end
    }
}

/// Bilinear XRGB sampling for a transformed snapshot, using signed 16.16 coords.
/// Clamp the second tap at image edges so a fractional last pixel stays valid.
pub fn sample_xrgb(pixels: &[u32], width: usize, height: usize, x: i64, y: i64) -> Option<u32> {
    if width == 0 || height == 0 || x < 0 || y < 0 { return None; }
    let ix = (x >> 16) as usize; let iy = (y >> 16) as usize;
    if ix >= width || iy >= height || width.checked_mul(height)? > pixels.len() { return None; }
    let nx = (ix + 1).min(width - 1); let ny = (iy + 1).min(height - 1);
    let fx = ((x >> 8) & 255) as u32; let fy = ((y >> 8) & 255) as u32;
    let p00 = pixels[iy * width + ix]; let p10 = pixels[iy * width + nx];
    let p01 = pixels[ny * width + ix]; let p11 = pixels[ny * width + nx];
    let mut out = 0;
    for shift in [0, 8, 16] {
        let a = ((p00 >> shift) & 255) * (256 - fx) + ((p10 >> shift) & 255) * fx;
        let b = ((p01 >> shift) & 255) * (256 - fx) + ((p11 >> shift) & 255) * fx;
        out |= ((a * (256 - fy) + b * fy + 32768) >> 16) << shift;
    }
    Some(out)
}

/// Prefer sharp transitions. Reduce work only after sustained missed budgets;
/// recover slowly to prevent quality oscillation on a borderline device.
pub struct AnimationQuality { block: usize, late: u16, early: u16 }

impl AnimationQuality {
    pub const fn new() -> Self { Self { block: 1, late: 0, early: 0 } }
    pub fn block(&self) -> usize { self.block }
    pub fn observe(&mut self, render_us: u64, budget_us: u64) {
        if render_us > budget_us {
            self.early = 0;
            self.late += 1;
            if self.late >= 4 { self.block = (self.block * 2).min(4); self.late = 0; }
        } else if render_us.saturating_mul(2) < budget_us {
            self.late = 0;
            self.early += 1;
            if self.early >= 60 { self.block = (self.block / 2).max(1); self.early = 0; }
        } else { self.late = 0; self.early = 0; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metrics() -> DisplayMetrics {
        DisplayMetrics { generation: 1, width: 1920, height: 1080,
            stride_bytes: 7680, density: 1024, refresh_millihz: 144_000,
            target_millihz: 144_000, capabilities: 0 }
    }
    #[test] fn abi_and_validation() {
        let mut m = metrics();
        assert_eq!(DisplayMetrics::decode(&m.encode()), Some(m));
        assert!(DisplayMetrics::decode(&[0; INFO_BYTES]).is_none());
        m.stride_bytes = 10;
        assert!(DisplayMetrics::decode(&m.encode()).is_none());
        m = metrics(); m.width = u32::MAX;
        assert!(DisplayMetrics::decode(&m.encode()).is_none());
        m = metrics(); m.target_millihz = 240_000;
        assert!(DisplayMetrics::decode(&m.encode()).is_none());
    }
    #[test] fn responsive_layout_uses_density() {
        let mut m = metrics();
        assert!(m.layout().wide()); assert_eq!(m.layout().columns(), 12);
        m.width = 1080; m.height = 2400; m.density = 3072;
        assert_eq!(m.layout(), Layout { width_dp: 360, height_dp: 800 });
        assert_eq!(m.layout().columns(), 3);
        assert_eq!(Layout { width_dp: 1, height_dp: 1 }.columns(), 1);
    }
    #[test] fn clock_does_not_drift_at_fractional_refresh() {
        let mut clock = FrameClock::new(); let mut start = 0;
        for _ in 0..144 { let end = start + 1000; start = end + clock.wait_us(start, end, 144_000); }
        assert_eq!(start, 1_000_000);
    }
    #[test] fn stall_does_not_sleep_or_catch_up() {
        let mut clock = FrameClock::new();
        assert_eq!(clock.wait_us(0, 500, 120_000), 7833);
        assert_eq!(clock.wait_us(8333, 100_000, 120_000), 0);
        assert_eq!(clock.wait_us(100_000, 101_000, 120_000), 7333);
    }
    #[test] fn refresh_change_and_clock_reset() {
        let mut clock = FrameClock::new();
        clock.wait_us(0, 100, 60_000);
        assert_eq!(clock.wait_us(20_000, 21_000, 120_000), 7333);
        assert_eq!(clock.wait_us(0, 100, 120_000), 8233);
    }
    #[test] fn quality_has_hysteresis() {
        let mut q = AnimationQuality::new();
        for _ in 0..3 { q.observe(10_000, 8333); }
        assert_eq!(q.block(), 1);
        q.observe(10_000, 8333); assert_eq!(q.block(), 2);
        for _ in 0..59 { q.observe(1000, 8333); }
        assert_eq!(q.block(), 2);
        q.observe(1000, 8333); assert_eq!(q.block(), 1);
    }
    #[test] fn bilinear_sampling_preserves_pixels_and_edges() {
        let pixels = [0x000000, 0xFF0000, 0x00FF00, 0x0000FF];
        assert_eq!(sample_xrgb(&pixels, 2, 2, 0, 0), Some(0));
        assert_eq!(sample_xrgb(&pixels, 2, 2, 32768, 32768), Some(0x404040));
        assert_eq!(sample_xrgb(&pixels, 2, 2, 98304, 98304), Some(0x0000FF));
        assert_eq!(sample_xrgb(&pixels, 2, 2, -1, 0), None);
        assert_eq!(sample_xrgb(&pixels[..1], 2, 2, 0, 0), None);
    }
    #[test] fn interrupted_wait_does_not_accumulate_future_sleep() {
        let mut clock = FrameClock::new();
        for start in 0..100 { clock.wait_us(start, start + 1, 60_000); clock.reset(); }
        assert_eq!(clock.wait_us(100, 101, 60_000), 16665);
    }
    #[test] fn early_input_wakeup_keeps_the_same_deadline() {
        let mut clock = FrameClock::new();
        assert_eq!(clock.wait_us(0, 1000, 60_000), 15666);
        // Input wakes the frame sleep at 2000us, before its deadline.
        assert_eq!(clock.wait_us(2000, 3000, 60_000), 13666);
        assert_eq!(clock.wait_us(16666, 17000, 60_000), 16333);
    }
}
