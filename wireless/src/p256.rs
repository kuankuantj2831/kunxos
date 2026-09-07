//! NIST P-256（secp256r1）椭圆曲线：域运算、点运算（Jacobian 坐标）、
//! 标量乘、ECDH 共享密钥。用于 WPA3-SAE（dragonfly）与蓝牙 LE Secure
//! Connections（SMP f5/f6/g2）的 P-256 基础。
//!
//! 域元素为 `[u64; 4]` 小端字；乘法用 u128 累加 + 稀疏折叠约简
//! （p = 2^256 - 2^224 + 2^192 + 2^96 - 1 ⇒ 2^256 ≡ 2^224 - 2^192 - 2^96 + 1）。

/// P-256 素数 p。
const P: [u64; 4] = [
    0xffff_ffff_ffff_ffff,
    0x0000_0000_ffff_ffff,
    0x0000_0000_0000_0000,
    0xffff_ffff_0000_0001,
];

/// p - 2（用于费马小定理求逆）。
const P_MINUS_2: [u64; 4] = [
    0xffff_ffff_ffff_fffd,
    0x0000_0000_ffff_ffff,
    0x0000_0000_0000_0000,
    0xffff_ffff_0000_0001,
];

/// 曲线参数 a = -3 (mod p)。
const A: [u64; 4] = [
    0xffff_ffff_ffff_fffc,
    0x0000_0000_ffff_ffff,
    0x0000_0000_0000_0000,
    0xffff_ffff_0000_0001,
];

/// 曲线参数 b = 5AC635D8AA3A93E7 B3EBBD55769886BC 651D06B0CC53B0F6 3BCE3C3E27D2604B。
const B: [u64; 4] = [
    0x3bce_3c3e_27d2_604b,
    0x651d_06b0_cc53_b0f6,
    0xb3eb_bd55_7698_86bc,
    0x5ac6_35d8_aa3a_93e7,
];

/// 基点 G（小端字序）。
const GX: [u64; 4] = [
    0xf4a1_3945_d898_c296,
    0x7703_7d81_2deb_33a0,
    0xf8bc_e6e5_63a4_40f2,
    0x6b17_d1f2_e12c_4247,
];

const GY: [u64; 4] = [
    0xcbb6_4068_37bf_51f5,
    0x2bce_3357_6b31_5ece,
    0x8ee7_eb4a_7c0f_9e16,
    0x4fe3_42e2_fe1a_7f9b,
];

/// 从 32 字节大端解析域元素（小端字序）。
fn from_be_bytes(b: &[u8; 32]) -> [u64; 4] {
    let mut v = [0u64; 4];
    for i in 0..4 {
        let mut word: u64 = 0;
        for j in 0..8 {
            word = (word << 8) | b[i * 8 + j] as u64;
        }
        v[3 - i] = word;
    }
    v
}

/// 域元素序列化为 32 字节大端。
pub fn to_be_bytes(v: &[u64; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..4 {
        let word = v[3 - i];
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// 比较两个域元素（小端字序按高位优先）。
fn fe_cmp(a: &[u64; 4], b: &[u64; 4]) -> core::cmp::Ordering {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return if a[i] < b[i] { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater };
        }
    }
    core::cmp::Ordering::Equal
}

pub fn fe_add(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut r = [0u64; 4];
    let mut carry: u128 = 0;
    for i in 0..4 {
        let s = a[i] as u128 + b[i] as u128 + carry;
        r[i] = s as u64;
        carry = s >> 64;
    }
    // 条件减 p（最多一次，因为 a,b < p ⇒ a+b < 2p）。
    if carry != 0 || fe_cmp(&r, &P) != core::cmp::Ordering::Less {
        let mut borrow: u64 = 0;
        for i in 0..4 {
            let (d1, b1) = r[i].overflowing_sub(P[i]);
            let (d, b2) = d1.overflowing_sub(borrow);
            r[i] = d;
            borrow = (b1 as u64) | b2 as u64;
        }
    }
    r
}

pub fn fe_sub(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut r = [0u64; 4];
    let mut borrow: u64 = 0;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d, b2) = d1.overflowing_sub(borrow);
        r[i] = d;
        borrow = (b1 as u64) | b2 as u64;
    }
    if borrow != 0 {
        // 加回 p。
        let mut carry: u128 = 0;
        for i in 0..4 {
            let s = r[i] as u128 + P[i] as u128 + carry;
            r[i] = s as u64;
            carry = s >> 64;
        }
    }
    r
}

/// 8 字（512 位）数组左移 `bits`（0 < bits < 64）。
fn shl8(v: &[u64; 8], bits: u32) -> [u64; 8] {
    let mut r = [0u64; 8];
    let limb = (bits / 64) as usize;
    let rem = bits % 64;
    for i in (limb..8).rev() {
        let mut val = v[i - limb] << rem;
        if rem != 0 && i - limb >= 1 {
            val |= v[i - limb - 1] >> (64 - rem);
        }
        r[i] = val;
    }
    r
}

/// 8 字数组相加。
fn add8(a: &[u64; 8], b: &[u64; 8]) -> [u64; 8] {
    let mut r = [0u64; 8];
    let mut carry: u128 = 0;
    for i in 0..8 {
        let s = a[i] as u128 + b[i] as u128 + carry;
        r[i] = s as u64;
        carry = s >> 64;
    }
    r
}

/// 8 字数组相减（要求 a ≥ b）。
fn sub8(a: &[u64; 8], b: &[u64; 8]) -> [u64; 8] {
    let mut r = [0u64; 8];
    let mut borrow: u64 = 0;
    for i in 0..8 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d, b2) = d1.overflowing_sub(borrow);
        r[i] = d;
        borrow = (b1 as u64) | b2 as u64;
    }
    r
}

/// 512 位积约简到 256 位 mod p（稀疏折叠 + 至多 4 次条件减）。
fn reduce512(t: &[u64; 8]) -> [u64; 4] {
    // c = 2^224 - 2^192 - 2^96 + 1
    let mut cur = *t;
    for _ in 0..10 {
        let mut hi = [0u64; 4];
        hi.copy_from_slice(&cur[4..8]);
        let mut lo = [0u64; 4];
        lo.copy_from_slice(&cur[0..4]);
        if hi == [0; 4] {
            break;
        }
        // s = (hi << 224) - (hi << 192) - (hi << 96) + hi，扩展到 8 字运算。
        let mut hi8 = [0u64; 8];
        hi8[..4].copy_from_slice(&hi);
        let sh224 = shl8(&hi8, 224);
        let sh192 = shl8(&hi8, 192);
        let sh96 = shl8(&hi8, 96);
        // s = sh224 - sh192 - sh96 + hi8（注意 sh224 ≥ sh192 ≥ sh96，且结果非负）
        let s1 = sub8(&sh224, &sh192);
        let s2 = sub8(&s1, &sh96);
        let s = add8(&s2, &hi8);
        cur = add8(&lo8(&lo), &s);
    }
    // 最终条件减 p（至多 4 次）。
    let mut r = [0u64; 4];
    r.copy_from_slice(&cur[0..4]);
    for _ in 0..4 {
        if fe_cmp(&r, &P) != core::cmp::Ordering::Less {
            r = fe_sub(&r, &P);
        } else {
            break;
        }
    }
    r
}

fn lo8(v: &[u64; 4]) -> [u64; 8] {
    let mut r = [0u64; 8];
    r[..4].copy_from_slice(v);
    r
}

/// 域乘法 a·b mod p。
pub fn fe_mul(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut t = [0u64; 8];
    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in 0..4 {
            let s = a[i] as u128 * b[j] as u128 + t[i + j] as u128 + carry;
            t[i + j] = s as u64;
            carry = s >> 64;
        }
        t[i + 4] = (t[i + 4] as u128 + carry) as u64;
    }
    reduce512(&t)
}

pub fn fe_sqr(a: &[u64; 4]) -> [u64; 4] {
    fe_mul(a, a)
}

/// 幂运算 a^e mod p（平方-乘）。
fn fe_pow(a: &[u64; 4], e: &[u64; 4]) -> [u64; 4] {
    let mut result = [1u64, 0, 0, 0];
    let mut base = *a;
    let mut exp = *e;
    while exp != [0; 4] {
        if exp[0] & 1 != 0 {
            result = fe_mul(&result, &base);
        }
        base = fe_sqr(&base);
        // exp >>= 1（低字位进位：来自高字 LSB）
        let mut carry = 0u64;
        for i in (0..4).rev() {
            let next = exp[i] & 1;
            exp[i] = (exp[i] >> 1) | (carry << 63);
            carry = next;
        }
    }
    result
}

/// 域逆元 a^-1 mod p（费马小定理）。
pub fn fe_inv(a: &[u64; 4]) -> [u64; 4] {
    fe_pow(a, &P_MINUS_2)
}

/// 公开幂运算 a^e mod p（SAE sqrt 等使用）。
pub fn pow_mod(a: &[u64; 4], e: &[u64; 4]) -> [u64; 4] {
    fe_pow(a, e)
}

fn is_zero(v: &[u64; 4]) -> bool {
    v == &[0u64; 4]
}

fn is_one(v: &[u64; 4]) -> bool {
    v == &[1u64, 0, 0, 0]
}

// ==================== 点运算（Jacobian 坐标） ====================

#[derive(Clone, Copy, PartialEq)]
pub struct Point {
    pub x: [u64; 4],
    pub y: [u64; 4],
    pub z: [u64; 4],
}

impl Point {
    /// 仿射点。
    pub fn new(x: [u64; 4], y: [u64; 4]) -> Self {
        Point { x, y, z: [1, 0, 0, 0] }
    }

    /// 无穷远点。
    pub fn infinity() -> Self {
        Point { x: [0; 4], y: [1, 0, 0, 0], z: [0; 4] }
    }

    pub fn is_infinity(&self) -> bool {
        is_zero(&self.z)
    }

    /// 仿射化。
    pub fn to_affine(&self) -> Option<([u64; 4], [u64; 4])> {
        if self.is_infinity() {
            return None;
        }
        let z_inv = fe_inv(&self.z);
        let z_inv2 = fe_sqr(&z_inv);
        let z_inv3 = fe_mul(&z_inv2, &z_inv);
        let x = fe_mul(&self.x, &z_inv2);
        let y = fe_mul(&self.y, &z_inv3);
        Some((x, y))
    }

    /// 判定是否在曲线上（仿射坐标）。
    pub fn on_curve(x: &[u64; 4], y: &[u64; 4]) -> bool {
        // y² == x³ - 3x + b；A ≡ -3 (mod p)，故 x³ - 3x = x³ + A·x。
        let lhs = fe_sqr(y);
        let x2 = fe_sqr(x);
        let x3 = fe_mul(&x2, x);
        let ax = fe_mul(&A, x);
        let rhs = fe_add(&fe_add(&x3, &ax), &B);
        fe_cmp(&lhs, &rhs) == core::cmp::Ordering::Equal
    }
}

/// Jacobian 双倍（a = -3）。
fn point_double(p: &Point) -> Point {
    if p.is_infinity() {
        return *p;
    }
    let a = fe_sqr(&p.x); // A = X1²
    let b = fe_sqr(&p.y); // B = Y1²
    let c = fe_sqr(&b); // C = B²
    let xb = fe_add(&p.x, &b);
    let xb2 = fe_sqr(&xb);
    let a_plus_c = fe_add(&a, &c);
    // D = 2((X1+B)² − A − C)
    let t = fe_sub(&xb2, &a_plus_c);
    let d = fe_add(&t, &t);
    // E = 3·(X1² − Z1⁴)（a = -3：M = 3X1² + a·Z1⁴ = 3(X1² − Z1⁴)）
    let zz = fe_sqr(&p.z);
    let zzzz = fe_sqr(&zz);
    let e = fe_mul(&[3u64, 0, 0, 0], &fe_sub(&a, &zzzz));
    let f = fe_sqr(&e); // F = E²
    let x3 = fe_sub(&f, &fe_add(&d, &d)); // X3 = F - 2D
    let d_minus_x3 = fe_sub(&d, &x3);
    let e_dx = fe_mul(&e, &d_minus_x3);
    let eight_c = fe_add(&fe_add(&c, &c), &fe_add(&c, &c));
    let eight_c = fe_add(&eight_c, &eight_c);
    let y3 = fe_sub(&e_dx, &eight_c); // Y3 = E(D - X3) - 8C
    let z3 = fe_mul(&fe_add(&p.y, &p.y), &p.z); // Z3 = 2Y1·Z1
    Point { x: x3, y: y3, z: z3 }
}

/// Jacobian 点加（一般情况）。
pub fn point_add(p: &Point, q: &Point) -> Point {
    if p.is_infinity() {
        return *q;
    }
    if q.is_infinity() {
        return *p;
    }
    let z1z1 = fe_sqr(&p.z);
    let z2z2 = fe_sqr(&q.z);
    let u1 = fe_mul(&p.x, &z2z2);
    let u2 = fe_mul(&q.x, &z1z1);
    let s1 = fe_mul(&fe_mul(&p.y, &q.z), &z2z2);
    let s2 = fe_mul(&fe_mul(&q.y, &p.z), &z1z1);
    let h = fe_sub(&u2, &u1);
    let r = fe_sub(&s2, &s1);
    if is_zero(&h) {
        if is_zero(&r) {
            return point_double(p);
        }
        return Point::infinity();
    }
    // 经典 Jacobian 点加（Handbuch 算法；已代数验证）：
    //   I = (2H)² = 4H², J = H·I = 4H³, r = 2(S2−S1), V = U1·I = 4U1H²
    //   X3 = r² − J − 2V
    //   Y3 = r·(V − X3) − 2·S1·J
    //   Z3 = Z1·Z2·H
    let h2 = fe_sqr(&h);
    let i = fe_add(&h2, &h2);
    let i = fe_add(&i, &i); // I = 4H²
    let j = fe_mul(&h, &i); // J = 4H³
    let r = fe_add(&r, &r); // r = 2(S2 − S1)
    let v = fe_mul(&u1, &i);
    let r2 = fe_sqr(&r);
    let x3 = fe_sub(&fe_sub(&r2, &j), &fe_add(&v, &v));
    let v_minus_x3 = fe_sub(&v, &x3);
    let r_vx = fe_mul(&r, &v_minus_x3);
    let s1j = fe_mul(&s1, &j);
    let s1j2 = fe_add(&s1j, &s1j);
    let y3 = fe_sub(&r_vx, &s1j2);
    let z1z2 = fe_mul(&p.z, &q.z);
    let z3 = fe_mul(&fe_add(&z1z2, &z1z2), &h); // Z3 = 2·Z1·Z2·H
    Point { x: x3, y: y3, z: z3 }
}

/// 标量乘 k·P（简单 double-and-add；非恒定时间，仅用于协议功能）。
pub fn scalar_mul(k: &[u64; 4], p: &Point) -> Point {
    let mut result = Point::infinity();
    let mut addend = *p;
    let mut exp = *k;
    while exp != [0; 4] {
        if exp[0] & 1 != 0 {
            result = point_add(&result, &addend);
        }
        addend = point_double(&addend);
        let mut carry = 0u64;
        for i in (0..4).rev() {
            let next = exp[i] & 1;
            exp[i] = (exp[i] >> 1) | (carry << 63);
            carry = next;
        }
    }
    result
}

// ==================== 公开接口 ====================

/// 从 32 字节私钥派生公钥（G 的标量乘）。
pub fn public_key_from_private(d: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let d_fe = from_be_bytes(d);
    let g = Point::new(GX, GY);
    let q = scalar_mul(&d_fe, &g);
    let (x, y) = q.to_affine().expect("invalid scalar (zero?)");
    (to_be_bytes(&x), to_be_bytes(&y))
}

/// ECDH 共享密钥：Z = x(d · Q_peer)。
/// 校验对端点在曲线上；无穷远或非法点返回 `None`。
pub fn ecdh_shared_secret(d: &[u8; 32], peer_x: &[u8; 32], peer_y: &[u8; 32]) -> Option<[u8; 32]> {
    let d_fe = from_be_bytes(d);
    let x = from_be_bytes(peer_x);
    let y = from_be_bytes(peer_y);
    if !Point::on_curve(&x, &y) {
        return None;
    }
    let q = Point::new(x, y);
    let z = scalar_mul(&d_fe, &q);
    let affine = z.to_affine()?;
    Some(to_be_bytes(&affine.0))
}

/// 曲线阶 n。
pub const ORDER: [u64; 4] = [
    0xf3b9_cac2_fc63_2551,
    0xbce6_faad_a717_9e84,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_0000_0000,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_basic() {
        // 1 + 1 = 2
        let one = [1u64, 0, 0, 0];
        let two = fe_add(&one, &one);
        assert_eq!(two, [2u64, 0, 0, 0]);
        // p - 1 + 1 = 0
        let pm1 = fe_sub(&P, &one);
        let zero = fe_add(&pm1, &one);
        assert!(is_zero(&zero));
        // a * a⁻¹ = 1
        let a = [0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321, 1, 0xffff_ffff_0000_0000];
        let inv = fe_inv(&a);
        let prod = fe_mul(&a, &inv);
        assert!(is_one(&prod));
    }

    #[test]
    fn base_point_double() {
        let g = Point::new(GX, GY);
        let two_g = point_double(&g);
        let (x, y) = two_g.to_affine().unwrap();
        assert_eq!(
            to_be_bytes(&x),
            [
                0x7c, 0xf2, 0x7b, 0x18, 0x8d, 0x03, 0x4f, 0x7e, 0x8a, 0x52, 0x38, 0x03, 0x04,
                0xb5, 0x1a, 0xc3, 0xc0, 0x89, 0x69, 0xe2, 0x77, 0xf2, 0x1b, 0x35, 0xa6, 0x0b,
                0x48, 0xfc, 0x47, 0x66, 0x99, 0x78
            ]
        );
        assert_eq!(
            to_be_bytes(&y),
            [
                0x07, 0x77, 0x55, 0x10, 0xdb, 0x8e, 0xd0, 0x40, 0x29, 0x3d, 0x9a, 0xc6, 0x9f,
                0x74, 0x30, 0xdb, 0xba, 0x7d, 0xad, 0xe6, 0x3c, 0xe9, 0x82, 0x29, 0x9e, 0x04,
                0xb7, 0x9d, 0x22, 0x78, 0x73, 0xd1
            ]
        );
    }

    #[test]
    fn order_multiplication_is_infinity() {
        let g = Point::new(GX, GY);
        let n_g = scalar_mul(&ORDER, &g);
        assert!(n_g.is_infinity());
        // (n-1)·G = -G（x 相同，y = p - Gy）
        let mut nm1 = ORDER;
        nm1[0] -= 1;
        let r = scalar_mul(&nm1, &g);
        let (x, y) = r.to_affine().unwrap();
        assert_eq!(x, GX);
        assert_eq!(y, fe_sub(&P, &GY));
    }

    /// 简易 hex 宏（测试专用）。
    macro_rules! hex {
        ($s:literal) => {{
            let s = $s;
            let mut out = [0u8; 32];
            for i in 0..32 {
                let hi = hex_val(s.as_bytes()[2 * i]);
                let lo = hex_val(s.as_bytes()[2 * i + 1]);
                out[i] = (hi << 4) | lo;
            }
            out
        }};
    }

    fn hex_val(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => 0,
        }
    }

    #[test]
    fn ecdh_vector() {
        let d_a: [u8; 32] = hex!("c88f01f510d9ac3f70a292daa2316de544e9aab8afe84049c62a9c57862d1433");
        let d_b: [u8; 32] = hex!("adae70cfc6c8fc01d139e3a1b1e5d8d7f8a9b0c1d2e3f40596f1d2c3b4a59607");
        let (qax, qay) = public_key_from_private(&d_a);
        let (qbx, qby) = public_key_from_private(&d_b);
        assert_eq!(
            qax,
            hex!("dad0b65394221cf9b051e1feca5787d098dfe637fc90b9ef945d0c3772581180")
        );
        assert_eq!(
            qay,
            hex!("5271a0461cdb8252d61f1c456fa3e59ab1f45b33accf5f58389e0577b8990bb3")
        );
        let z1 = ecdh_shared_secret(&d_a, &qbx, &qby).unwrap();
        let z2 = ecdh_shared_secret(&d_b, &qax, &qay).unwrap();
        assert_eq!(z1, z2);
        assert_eq!(
            z1,
            hex!("a214c6b39ebadf8690fa1bed5f35c41f4d4f99af03ecd4506d71824d157f966c")
        );
    }
}
















