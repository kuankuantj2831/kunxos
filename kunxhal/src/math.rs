//! 极简 f32 数学库（no_std 纯实现，不依赖 libm）。
//!
//! `riscv64gc-unknown-none-elf` 目标下 `core` 不提供 `sqrt/sin/cos/atan2/asin`
//! （这些在 `std` 里由 libm 提供），而传感器融合（Mahony）、姿态角换算、
//! 模拟传感器旋转、WGS84 距离计算都需要它们。这里用纯 Rust 实现：
//!
//! - `sqrt`：位技巧初始估计 + 3 次牛顿迭代（误差 < 1e-6）。
//! - `sin/cos`：归约到 [-π/2, π/2] + 9 阶泰勒（误差 ~1e-8）。
//! - `atan2`：fdlibm 风格多项式 `atan` + 象限修正（误差 ~1e-7 rad）。
//! - `asin`：`atan2(x, sqrt(1-x²))`。
//!
//! 若未来需要固定点版本，可整体替换本模块为 Q16.16 实现，接口不变。

use core::f32::consts::PI;

/// 绝对值。
#[inline]
pub fn abs(x: f32) -> f32 {
    if x < 0.0 { -x } else { x }
}

/// 向下取整（`riscv64gc-unknown-none-elf` 的 `core` 不提供 `f32::floor`）。
pub fn floor(x: f32) -> f32 {
    let t = x as i64 as f32;
    if t <= x { t } else { t - 1.0 }
}

/// 取模（结果与除数同号；`core` 不提供 `f32::rem_euclid`）。
pub fn rem_euclid(a: f32, b: f32) -> f32 {
    let m = a - b * floor(a / b);
    if m < 0.0 { m + b } else { m }
}

/// 平方根：位技巧初始估计 + 3 次牛顿迭代。
pub fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    // 经典估计：i = 0x1FC00000 + (bits >> 1)（快速平方根的逆运算）。
    let bits = x.to_bits();
    let mut y = f32::from_bits((bits >> 1) + 0x1FC0_0000);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// 返回 `v` 在 `[lo, hi]` 内的值。
#[inline]
pub fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo { lo } else if v > hi { hi } else { v }
}

/// 弧度 → 角度。
#[inline]
pub fn to_deg(r: f32) -> f32 {
    r * 180.0 / PI
}

/// 角度 → 弧度。
#[inline]
pub fn to_rad(d: f32) -> f32 {
    d * PI / 180.0
}

/// 把任意角归约到 `[-π, π]`。
fn reduce_pi(x: f32) -> f32 {
    // 先用 2π 取模（rem_euclid 保证非负），再折叠到 [-π, π]。
    let two_pi = 2.0 * PI;
    let m = rem_euclid(x, two_pi);
    if m > PI { m - two_pi } else { m }
}

/// 正弦 + 余弦（|x| ≤ π/4 的泰勒展开；9 阶，误差 < 3e-8）。
fn sincos_small(x: f32) -> (f32, f32) {
    let x2 = x * x;
    let s = x
        * (1.0
            - x2 * (1.0 / 6.0)
            + x2 * x2 * (1.0 / 120.0)
            - x2 * x2 * x2 * (1.0 / 5040.0)
            + x2 * x2 * x2 * x2 * (1.0 / 362880.0));
    let c = 1.0
        - x2 * (1.0 / 2.0)
        + x2 * x2 * (1.0 / 24.0)
        - x2 * x2 * x2 * (1.0 / 720.0)
        + x2 * x2 * x2 * x2 * (1.0 / 40320.0);
    (s, c)
}

/// 正弦（两次折叠到 [-π/4, π/4] 后泰勒，误差 ~1e-8）。
pub fn sin(x: f32) -> f32 {
    let r0 = reduce_pi(x);
    // 折叠到 [-π/2, π/2]：sin 在此折叠下保持不变。
    let r1 = if r0 > PI / 2.0 {
        PI - r0
    } else if r0 < -PI / 2.0 {
        -PI - r0
    } else {
        r0
    };
    // 再折叠到 [-π/4, π/4]：sin(r1) = ±cos(π/2 ∓ r1)。
    if r1 > PI / 4.0 {
        let (_, c) = sincos_small(PI / 2.0 - r1);
        c
    } else if r1 < -PI / 4.0 {
        let (_, c) = sincos_small(r1 + PI / 2.0);
        -c
    } else {
        let (s, _) = sincos_small(r1);
        s
    }
}

/// 余弦（折叠到 [-π/4, π/4] 后泰勒，误差 ~3e-8）。
pub fn cos(x: f32) -> f32 {
    let r0 = reduce_pi(x);
    let fold = r0 > PI / 2.0 || r0 < -PI / 2.0;
    let r1 = if r0 > PI / 2.0 {
        PI - r0
    } else if r0 < -PI / 2.0 {
        -PI - r0
    } else {
        r0
    };
    let v = if r1 > PI / 4.0 {
        let (s, _) = sincos_small(PI / 2.0 - r1);
        s
    } else if r1 < -PI / 4.0 {
        let (s, _) = sincos_small(r1 + PI / 2.0);
        s
    } else {
        let (_, c) = sincos_small(r1);
        c
    };
    if fold { -v } else { v }
}

/// `atan(x)` 的 fdlibm 修正多项式（|x| ≤ 0.5 时：atan(x) = x - x·(s1+s2)）。
fn atan_poly(x: f32) -> f32 {
    // 系数来自 fdlibm atan.c 的 aT[]，用于修正级数 s1+s2 ≈ 1 - atan(x)/x。
    const A: [f32; 11] = [
        3.33333333333329318027e-01,
        -1.99999999998764832476e-01,
        1.42857142725034663711e-01,
        -1.11111104054623557880e-01,
        9.09088713343650656196e-02,
        -7.69187620504482999495e-02,
        6.66107313738753120669e-02,
        -5.83357013379057348645e-02,
        4.97687799461593236017e-02,
        -3.65315727442169155270e-02,
        1.62858201153657823623e-02,
    ];
    let z = x * x;
    let w = z * z;
    // s1 = z·(A0 + w·(A2 + w·(A4 + w·(A6 + w·(A8 + w·A10)))))
    let s1 = z
        * (A[0]
            + w * (A[2] + w * (A[4] + w * (A[6] + w * (A[8] + w * A[10])))));
    // s2 = w·(A1 + w·(A3 + w·(A5 + w·(A7 + w·A9))))
    let s2 = w * (A[1] + w * (A[3] + w * (A[5] + w * (A[7] + w * A[9]))));
    x - x * (s1 + s2)
}

/// 非负输入的反正切（递归降阶：v>0.5 用 atan(v) = π/4 + atan((v-1)/(v+1))）。
fn atan_pos(v: f32) -> f32 {
    if v <= 0.5 {
        atan_poly(v)
    } else if v <= 1.0 {
        PI / 4.0 + atan_poly((v - 1.0) / (v + 1.0))
    } else {
        PI / 2.0 - atan_pos(1.0 / v)
    }
}

/// 反正切（弧度，范围 [-π/2, π/2]）。
pub fn atan(x: f32) -> f32 {
    let r = atan_pos(abs(x));
    if x < 0.0 { -r } else { r }
}

/// 四象限反正切 `atan2(y, x)`（弧度，范围 [-π, π]）。
pub fn atan2(y: f32, x: f32) -> f32 {
    if x == 0.0 {
        if y > 0.0 {
            return PI / 2.0;
        } else if y < 0.0 {
            return -PI / 2.0;
        }
        return 0.0;
    }
    let a = atan(y / x);
    if x > 0.0 {
        a
    } else if y >= 0.0 {
        a + PI
    } else {
        a - PI
    }
}

/// 反正弦（弧度，范围 [-π/2, π/2]；|x| > 1 时截断）。
pub fn asin(x: f32) -> f32 {
    let c = clamp(x, -1.0, 1.0);
    atan2(c, sqrt(1.0 - c * c))
}

/// 反正余弦（弧度，范围 [0, π]）。
pub fn acos(x: f32) -> f32 {
    let c = clamp(x, -1.0, 1.0);
    PI / 2.0 - asin(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f32, b: f32, eps: f32) {
        let d = abs(a - b);
        assert!(d < eps, "a={a} b={b} diff={d} eps={eps}");
    }

    #[test]
    fn test_sqrt() {
        for v in [0.0f32, 1.0, 4.0, 2.0, 100.0, 0.25, 1e-6, 1e6] {
            assert_close(sqrt(v), v.sqrt(), 1e-4 * (1.0 + v));
        }
    }

    #[test]
    fn test_trig() {
        for deg in [0.0f32, 30.0, 45.0, 60.0, 90.0, 120.0, 180.0, 270.0, -45.0, 720.0] {
            let r = to_rad(deg);
            let (s, c) = (sin(r), cos(r));
            assert_close(s * s + c * c, 1.0, 1e-6);
        }
        assert_close(sin(0.0), 0.0, 1e-7);
        assert_close(cos(0.0), 1.0, 1e-7);
        assert_close(sin(to_rad(90.0)), 1.0, 1e-6);
        assert_close(cos(to_rad(180.0)), -1.0, 1e-6);
    }

    #[test]
    fn test_atan2() {
        assert_close(atan2(0.0, 1.0), 0.0, 1e-6);
        assert_close(atan2(1.0, 0.0), PI / 2.0, 1e-6);
        assert_close(atan2(1.0, 1.0), PI / 4.0, 1e-6);
        assert_close(atan2(-1.0, -1.0), -3.0 * PI / 4.0, 1e-6);
        assert_close(atan2(0.0, -1.0), PI, 1e-6);
    }

    #[test]
    fn test_asin() {
        assert_close(asin(0.0), 0.0, 1e-6);
        assert_close(asin(1.0), PI / 2.0, 1e-5);
        assert_close(asin(0.5), PI / 6.0, 1e-5);
    }
}
