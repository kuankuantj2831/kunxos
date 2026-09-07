//! 多源定位融合（GPS / Wi-Fi / 基站 / SIM 模拟）。
//!
//! 约束：`no_std`、无堆、无 panic——空输入返回 `None`，所有计算有界。

/// 地球半径（米）。
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// 数据源类型。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SourceKind {
    Gps,
    Wifi,
    Cell,
    Sim,
}

/// 一次观测（Wi-Fi AP 扫描 / 基站 / GPS fix 统一表示）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observation {
    pub source: SourceKind,
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64, // 精度（米），越小越可信
    pub strength: i32,   // 信号强度 dBm（GPS 源可填 0）
    pub timestamp_ms: u64,
}

/// 融合结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocationEstimate {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
    pub source_count: usize,
    pub best_source: SourceKind,
}

/// 加权质心融合：按 accuracy 加权（1/accuracy²），GPS 源权重乘以 3。
///
/// 返回 `None` 若无观测。结果精度为按相同权重加权平均的各观测精度。
/// accuracy ≤ 0 的观测视为极高可信（权重被钳制到有限大值，避免除零/NaN）。
pub fn fuse(obs: &[Observation]) -> Option<LocationEstimate> {
    if obs.is_empty() {
        return None;
    }
    let mut w_lat = 0.0f64;
    let mut w_lon = 0.0f64;
    let mut w_acc = 0.0f64;
    let mut w_sum = 0.0f64;
    let mut src_w = [0.0f64; 4]; // Gps, Wifi, Cell, Sim

    for o in obs {
        let acc = if o.accuracy_m > 0.0 { o.accuracy_m } else { 1e-9 };
        let mut w = 1.0 / (acc * acc);
        if o.source == SourceKind::Gps {
            w *= 3.0;
        }
        w_lat += w * o.lat;
        w_lon += w * o.lon;
        w_acc += w * acc;
        w_sum += w;
        src_w[source_index(o.source)] += w;
    }

    let mut best = SourceKind::Gps;
    let mut best_w = src_w[0];
    for (i, &w) in src_w.iter().enumerate().skip(1) {
        if w > best_w {
            best_w = w;
            best = kind_from_index(i);
        }
    }

    Some(LocationEstimate {
        lat: w_lat / w_sum,
        lon: w_lon / w_sum,
        accuracy_m: w_acc / w_sum,
        source_count: obs.len(),
        best_source: best,
    })
}

/// 经纬度两点间大圆距离（Haversine），返回米。
///
/// 本工具链的 `core` 不提供稳定版 `f64::sin/cos/sqrt/atan2`（`core_float_math`
/// 仍为 unstable），因此这里用自带软件实现（精度 ~1e-14，远高于 1 m 需求）。
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dlat = (lat2 - lat1) * PI / 180.0;
    let dlon = (lon2 - lon1) * PI / 180.0;
    let la1 = lat1 * PI / 180.0;
    let la2 = lat2 * PI / 180.0;
    let sh = sin_f(dlat / 2.0);
    let sl = sin_f(dlon / 2.0);
    let a = sh * sh + cos_f(la1) * cos_f(la2) * (sl * sl);
    // 浮点舍入可能使 a 略超 [0,1]，钳制避免 sqrt 负参。
    let a = if a > 1.0 { 1.0 } else if a < 0.0 { 0.0 } else { a };
    let c = 2.0 * atan2_f(sqrt_f(a), sqrt_f(1.0 - a));
    EARTH_RADIUS_M * c
}

// ===== 软件浮点数学（替代 core 未稳定的 f64::sin/cos/sqrt/atan2） =====

use core::f64::consts::{FRAC_PI_2, PI};

/// 软件 `sqrt`：经典 double 位魔法初值 + Newton 迭代。x < 0 → NaN。
fn sqrt_f(x: f64) -> f64 {
    if x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    let g0 = f64::from_bits(0x1FF7_A3BE_A91D_9B1Bu64 + (bits >> 1));
    let mut g = g0;
    for _ in 0..12 {
        g = 0.5 * (g + x / g);
    }
    g
}

/// 软件 `sin`：约化到 [-π/2, π/2] 后 Taylor 级数（~1e-16 精度）。
fn sin_f(x: f64) -> f64 {
    let mut x = x % (2.0 * PI);
    if x > PI {
        x -= 2.0 * PI;
    } else if x < -PI {
        x += 2.0 * PI;
    }
    if x > FRAC_PI_2 {
        x = PI - x;
    } else if x < -FRAC_PI_2 {
        x = -PI - x;
    }
    let x2 = x * x;
    let mut s = x;
    let mut term = x;
    let mut k = 1u32;
    while k <= 9 {
        term = -term * x2 / ((2 * k) as f64 * (2 * k + 1) as f64);
        s += term;
        k += 1;
    }
    s
}

/// 软件 `cos`：`cos x = sin(x + π/2)`。
fn cos_f(x: f64) -> f64 {
    sin_f(x + FRAC_PI_2)
}

/// 软件 `atan`：半角约化（3 次）后 Taylor 级数，覆盖任意实参。
fn atan_f(u: f64) -> f64 {
    let neg = u < 0.0;
    let mut v = u.abs();
    let mut inv = false;
    if v > 1.0 {
        v = 1.0 / v;
        inv = true;
    }
    // atan(v) = 2·atan(v / (1 + √(1+v²)))，3 次约化后 v' ≤ tan(π/16)。
    let mut w = v;
    for _ in 0..3 {
        w = w / (1.0 + sqrt_f(1.0 + w * w));
    }
    let w2 = w * w;
    let mut s = w;
    let mut term = w;
    let mut k = 1u32;
    while k <= 10 {
        term = -term * w2;
        s += term / (2 * k + 1) as f64;
        k += 1;
    }
    let mut r = s * 8.0; // 3 次倍角 → ×2³
    if inv {
        r = FRAC_PI_2 - r;
    }
    if neg {
        -r
    } else {
        r
    }
}

/// 软件 `atan2(y, x)`：四象限反正切（与 C 语义一致，含 x=0 边界）。
fn atan2_f(y: f64, x: f64) -> f64 {
    if x > 0.0 {
        atan_f(y / x)
    } else if x < 0.0 {
        if y >= 0.0 {
            atan_f(y / x) + PI
        } else {
            atan_f(y / x) - PI
        }
    } else if y > 0.0 {
        FRAC_PI_2
    } else if y < 0.0 {
        -FRAC_PI_2
    } else {
        0.0
    }
}

fn source_index(k: SourceKind) -> usize {
    match k {
        SourceKind::Gps => 0,
        SourceKind::Wifi => 1,
        SourceKind::Cell => 2,
        SourceKind::Sim => 3,
    }
}

fn kind_from_index(i: usize) -> SourceKind {
    match i {
        0 => SourceKind::Gps,
        1 => SourceKind::Wifi,
        2 => SourceKind::Cell,
        _ => SourceKind::Sim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(source: SourceKind, lat: f64, lon: f64, acc: f64) -> Observation {
        Observation {
            source,
            lat,
            lon,
            accuracy_m: acc,
            strength: -60,
            timestamp_ms: 1_000,
        }
    }

    #[test]
    fn fuse_empty_returns_none() {
        assert_eq!(fuse(&[]), None);
    }

    #[test]
    fn fuse_weighted_centroid() {
        // 两个同精度观测 → 加权质心 = 中点。
        let a = obs(SourceKind::Wifi, 10.0, 20.0, 30.0);
        let b = obs(SourceKind::Cell, 20.0, 40.0, 30.0);
        let e = fuse(&[a, b]).expect("fuse");
        assert!((e.lat - 15.0).abs() < 1e-9);
        assert!((e.lon - 30.0).abs() < 1e-9);
        assert_eq!(e.source_count, 2);
        // 更精确的观测占比更大：acc 10 vs 30 → 权重比 9:1。
        let c = obs(SourceKind::Wifi, 0.0, 0.0, 10.0);
        let d = obs(SourceKind::Cell, 100.0, 100.0, 30.0);
        let e2 = fuse(&[c, d]).expect("fuse");
        let expected = 0.0 + (100.0 - 0.0) * (1.0 / 900.0) / (1.0 / 100.0 + 1.0 / 900.0);
        assert!((e2.lat - expected).abs() < 1e-9);
    }

    #[test]
    fn fuse_gps_weight_triples() {
        // GPS 权重 ×3：同精度时 GPS 贡献 3/4。
        let g = obs(SourceKind::Gps, 0.0, 0.0, 100.0);
        let w = obs(SourceKind::Wifi, 100.0, 100.0, 100.0);
        let e = fuse(&[g, w]).expect("fuse");
        assert!((e.lat - 25.0).abs() < 1e-9);
        assert!((e.lon - 25.0).abs() < 1e-9);
        assert_eq!(e.best_source, SourceKind::Gps);
        // 精度差距足够大时 best_source 翻转：wifi acc20 权重 1/400，
        // GPS acc200 权重 3/40000 → 纬度 = 100·(1/400)/(1/400+3/40000) = 10000/103。
        let g2 = obs(SourceKind::Gps, 0.0, 0.0, 200.0);
        let w2 = obs(SourceKind::Wifi, 100.0, 100.0, 20.0);
        let e2 = fuse(&[g2, w2]).expect("fuse");
        assert_eq!(e2.best_source, SourceKind::Wifi);
        let expect = 10_000.0 / 103.0;
        assert!((e2.lat - expect).abs() < 1e-9, "lat = {}", e2.lat);
    }

    #[test]
    fn fuse_single_observation() {
        let g = obs(SourceKind::Gps, 12.34, 56.78, 5.0);
        let e = fuse(&[g]).expect("fuse");
        assert!((e.lat - 12.34).abs() < 1e-12);
        assert!((e.lon - 56.78).abs() < 1e-12);
        assert_eq!(e.best_source, SourceKind::Gps);
        assert_eq!(e.source_count, 1);
        assert!((e.accuracy_m - 5.0).abs() < 1e-9);
    }

    #[test]
    fn haversine_known_distances() {
        // 赤道上经度 1° ≈ 111195 m（R = 6371000）。
        let d1 = haversine_m(0.0, 0.0, 0.0, 1.0);
        assert!((d1 - 111_195.0).abs() < 1.0, "d1 = {}", d1);
        // 子午线上纬度 1° ≈ 111195 m。
        let d2 = haversine_m(40.0, 0.0, 41.0, 0.0);
        assert!((d2 - 111_195.0).abs() < 1.0, "d2 = {}", d2);
        // 同一点 → 0；半圆（180°）≈ π·R。
        assert!(haversine_m(0.0, 0.0, 0.0, 0.0) < 1e-6);
        let half = haversine_m(0.0, 0.0, 0.0, 180.0);
        assert!((half - core::f64::consts::PI * EARTH_RADIUS_M).abs() < 1.0);
        // 任意两点距离对称。
        let d3 = haversine_m(31.2, 121.5, 39.9, 116.4);
        let d4 = haversine_m(39.9, 116.4, 31.2, 121.5);
        assert!((d3 - d4).abs() < 1e-6);
    }

    #[test]
    fn software_math_matches_std() {
        // sin / cos：跨多周期采样。
        let mut x = -30.0;
        while x <= 30.0 {
            let se = (sin_f(x) - x.sin()).abs();
            let ce = (cos_f(x) - x.cos()).abs();
            assert!(se < 1e-12, "sin err at {}: {}", x, se);
            assert!(ce < 1e-12, "cos err at {}: {}", x, ce);
            x += 0.37;
        }
        // sqrt：跨数量级（含 [0,2] 与巨大/微小值）。
        let mut e = -300;
        while e <= 300 {
            let x = 1.5 * 10f64.powi(e);
            let err = ((sqrt_f(x) - x.sqrt()) / x.sqrt()).abs();
            assert!(err < 1e-10, "sqrt err at {}: {}", x, err);
            e += 17;
        }
        assert!((sqrt_f(2.0) - 2.0f64.sqrt()).abs() < 1e-15);
        assert!((sqrt_f(0.0) - 0.0).abs() < 1e-300);
        assert!(sqrt_f(-1.0).is_nan());
        // atan2：四象限网格。
        let mut y = -3.0;
        while y <= 3.0 {
            let mut x = -3.0;
            while x <= 3.0 {
                let err = (atan2_f(y, x) - y.atan2(x)).abs();
                assert!(err < 1e-12, "atan2 err at ({},{}): {}", y, x, err);
                x += 0.43;
            }
            y += 0.31;
        }
        assert!((atan2_f(1.0, 0.0) - FRAC_PI_2).abs() < 1e-15);
        assert!((atan2_f(-1.0, 0.0) + FRAC_PI_2).abs() < 1e-15);
        assert_eq!(atan2_f(0.0, 0.0), 0.0);
        // haversine 与 std 参考实现的整段一致性。
        let cases: [(f64, f64, f64, f64); 5] = [
            (0.0, 0.0, 0.0, 1.0),
            (40.0, 0.0, 41.0, 0.0),
            (31.2, 121.5, 39.9, 116.4),
            (-33.9, 151.2, 35.7, 139.7),
            (0.0, 0.0, 0.0, 180.0),
        ];
        for (la1, lo1, la2, lo2) in cases {
            let dl1 = (la2 - la1).to_radians();
            let dl2 = (lo2 - lo1).to_radians();
            let a = (dl1 / 2.0).sin().powi(2)
                + la1.to_radians().cos() * la2.to_radians().cos() * (dl2 / 2.0).sin().powi(2);
            let a = a.min(1.0).max(0.0);
            let refd = EARTH_RADIUS_M * 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
            let err = (haversine_m(la1, lo1, la2, lo2) - refd).abs();
            assert!(err < 1e-6, "haversine err: {}", err);
        }
    }
}
