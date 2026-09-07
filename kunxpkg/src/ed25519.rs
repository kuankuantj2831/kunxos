//! Ed25519 签名验证（RFC 8032）：纯 Rust、no_std、零依赖。
//!
//! 仅实现**验签**（设备端不持有任何私钥）。实现要点：
//! - 曲线 25519（p = 2^255 - 19），域元素用 5×51 位肢体（radix-2^51）；
//! - 点用扩展齐次坐标 (X:Y:Z:T)，加法用完整公式（HWCD-2008），任意输入合法；
//! - 验证方程 [S]B = R + [h]A（cofactorless，与 RFC 8032 及
//!   `cryptography`/`pynacl` 等主流实现一致）；
//! - 所有输入均为公开数据（公钥、签名、消息），不做恒定时间要求。
//!
//! 正确性由 `tools/kunxverify`（主机侧）用 RFC 8032 官方测试向量
//! 与 `cryptography` 库生成的向量交叉验证。

use crate::sha512::Sha512;

/// 签名长度（R‖S，各 32 字节）。
pub const SIG_LEN: usize = 64;
/// 公钥长度。
pub const PK_LEN: usize = 32;

// ==================== 域参数 ====================

const MASK51: u64 = (1 << 51) - 1;

/// 曲线参数 d = -121665/121666 mod p。
const D: Fe = Fe([
    929955233495203, 466365720129213, 1662059464998953, 2033849074728123, 1442794654840575,
]);
/// 2d（点加公式预计算）。
const D2: Fe = Fe([
    1859910466990425, 932731440258426, 1072319116312658, 1815898335770999, 633789495995903,
]);
/// 基点 B.x。
const BX: Fe = Fe([
    1738742601995546, 1146398526822698, 2070867633025821, 562264141797630, 587772402128613,
]);
/// 基点 B.y。
const BY: Fe = Fe([
    1801439850948184, 1351079888211148, 450359962737049, 900719925474099, 1801439850948198,
]);
/// sqrt(-1) mod p。
const SQRT_M1: Fe = Fe([
    1718705420411056, 234908883556509, 2233514472574048, 2117202627021982, 765476049583133,
]);
const ONE: Fe = Fe([1, 0, 0, 0, 0]);
const ZERO: Fe = Fe([0, 0, 0, 0, 0]);

/// 群阶 L = 2^252 + 27742317777372353535851937790883648493（小端）。
const L_BYTES: [u8; 32] = [
    237, 211, 245, 92, 26, 99, 18, 88, 214, 156, 247, 162, 222, 249, 222, 20, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 16,
];

/// 指数 (p-5)/8（点解压开方公式）。
const EXP_P58: [u64; 4] = [18446744073709551613, 18446744073709551615, 18446744073709551615, 1152921504606846975];
/// 指数 p-2（求逆）。
const EXP_PM2: [u64; 4] = [18446744073709551595, 18446744073709551615, 18446744073709551615, 9223372036854775807];

// ==================== 域元素（5×51 位肢体） ====================

#[derive(Clone, Copy, Debug)]
struct Fe([u64; 5]);

/// 按模比较：先完全归约再逐肢体比较（数值相等即相等，与表示形式无关）。
impl PartialEq for Fe {
    fn eq(&self, other: &Fe) -> bool {
        let mut a = *self;
        let mut b = *other;
        a.strong_reduce();
        b.strong_reduce();
        a.0 == b.0
    }
}

impl Fe {
    /// 从 32 字节小端编码解码（忽略第 255 位；输入最高位应已被调用方清零）。
    fn from_bytes(b: &[u8; 32]) -> Fe {
        let mut low = [0u64; 4];
        for i in 0..4 {
            low[i] = u64::from_le_bytes([
                b[i * 8], b[i * 8 + 1], b[i * 8 + 2], b[i * 8 + 3],
                b[i * 8 + 4], b[i * 8 + 5], b[i * 8 + 6], b[i * 8 + 7],
            ]);
        }
        Fe([
            low[0] & MASK51,
            ((low[0] >> 51) | (low[1] << 13)) & MASK51,
            ((low[1] >> 38) | (low[2] << 26)) & MASK51,
            ((low[2] >> 25) | (low[3] << 39)) & MASK51,
            (low[3] >> 12) & MASK51,
        ])
    }

    /// 编码为 32 字节小端（先完全归约）。
    fn to_bytes(&self) -> [u8; 32] {
        let mut h = *self;
        h.strong_reduce();
        let mut out = [0u8; 32];
        // 肢体 → 4×u64 小端 → 32 字节
        let mut low = [0u64; 4];
        low[0] = h.0[0] | (h.0[1] << 51);
        low[1] = (h.0[1] >> 13) | (h.0[2] << 38);
        low[2] = (h.0[2] >> 26) | (h.0[3] << 25);
        low[3] = (h.0[3] >> 39) | (h.0[4] << 12);
        for i in 0..4 {
            out[i * 8..i * 8 + 8].copy_from_slice(&low[i].to_le_bytes());
        }
        out
    }

    /// 完全归约：反复进位直到每个肢体 < 2^51，随后若数值 ≥ p 则减 p，
    /// 得到**规范代表元**（编码与比较的前提）。
    /// 输入肢体 < 2^53 时进位链 ≤ 4 轮收敛；非规范代表元（如 2^255-18 ≡ 1）
    /// 由末尾的条件减法处理。
    fn strong_reduce(&mut self) {
        let m = MASK51;
        for _ in 0..4 {
            let mut c = self.0[0] >> 51;
            self.0[1] = self.0[1].wrapping_add(c);
            self.0[0] &= m;
            c = self.0[1] >> 51;
            self.0[2] = self.0[2].wrapping_add(c);
            self.0[1] &= m;
            c = self.0[2] >> 51;
            self.0[3] = self.0[3].wrapping_add(c);
            self.0[2] &= m;
            c = self.0[3] >> 51;
            self.0[4] = self.0[4].wrapping_add(c);
            self.0[3] &= m;
            c = self.0[4] >> 51;
            self.0[0] = self.0[0].wrapping_add(c.wrapping_mul(19));
            self.0[4] &= m;
        }
        // 若 value ≥ p = 2^255-19（p 肢体 = [2^51-19, 2^51-1 ×4]），减去 p。
        const P0: u64 = (1 << 51) - 19;
        const P1: u64 = (1 << 51) - 1;
        let mut borrow = 0i128;
        let mut t = [0i128; 5];
        for i in 0..5 {
            let pi = if i == 0 { P0 } else { P1 };
            let d = self.0[i] as i128 - pi as i128 - borrow;
            if d < 0 {
                t[i] = d + (1i128 << 51);
                borrow = 1;
            } else {
                t[i] = d;
                borrow = 0;
            }
        }
        if borrow == 0 {
            for i in 0..5 {
                self.0[i] = t[i] as u64;
            }
        }
    }

    fn is_zero(&self) -> bool {
        let mut h = *self;
        h.strong_reduce();
        h.0 == [0, 0, 0, 0, 0]
    }

    /// 模加：a + b（结果肢体 < 2^52，输入肢体 < 2^51 时）。
    fn add(&self, b: &Fe) -> Fe {
        let mut r = [0u64; 5];
        for i in 0..5 {
            r[i] = self.0[i] + b.0[i];
        }
        Fe(r)
    }

    /// 模减：a - b ≡ a + (p - b)（把减法化为加法的逆元，避免借位边界问题）。
    /// 结果肢体 < 2^52（非规范，满足后续运算的界）。
    fn sub(&self, b: &Fe) -> Fe {
        self.add(&b.neg())
    }

    /// 加法逆元：p - self（先规范归约；结果规范 < p，self=0 时归约为 0）。
    fn neg(&self) -> Fe {
        let x = self.reduced();
        let mut r = [0i128; 5];
        let mut borrow = 0i128;
        // p 的肢体 = [2^51-19, 2^51-1, 2^51-1, 2^51-1, 2^51-1]
        for i in 0..5 {
            let pi = if i == 0 { (1i128 << 51) - 19 } else { (1i128 << 51) - 1 };
            let d = pi - x.0[i] as i128 - borrow;
            if d < 0 {
                r[i] = d + (1i128 << 51);
                borrow = 1;
            } else {
                r[i] = d;
                borrow = 0;
            }
        }
        // x < p ⇒ 最终无借位；x = 0 ⇒ r = p ≡ 0，由归约处理
        Fe([r[0] as u64, r[1] as u64, r[2] as u64, r[3] as u64, r[4] as u64]).reduced()
    }

    fn sq(&self) -> Fe {
        self.mul(self)
    }

    /// 乘法：学校式 10 项 + 2^255 ≡ 19 折叠 + 进位链。
    /// 输入肢体 < 2^53 时全部中间量 < 2^111，u128 安全；输出肢体 < 2^51。
    fn mul(&self, b: &Fe) -> Fe {
        let a = &self.0;
        let b = &b.0;
        let t0 = a[0] as u128 * b[0] as u128;
        let t1 = a[0] as u128 * b[1] as u128 + a[1] as u128 * b[0] as u128;
        let t2 = a[0] as u128 * b[2] as u128 + a[1] as u128 * b[1] as u128 + a[2] as u128 * b[0] as u128;
        let t3 = a[0] as u128 * b[3] as u128 + a[1] as u128 * b[2] as u128 + a[2] as u128 * b[1] as u128
            + a[3] as u128 * b[0] as u128;
        let t4 = a[0] as u128 * b[4] as u128 + a[1] as u128 * b[3] as u128 + a[2] as u128 * b[2] as u128
            + a[3] as u128 * b[1] as u128 + a[4] as u128 * b[0] as u128;
        let t5 = a[1] as u128 * b[4] as u128 + a[2] as u128 * b[3] as u128 + a[3] as u128 * b[2] as u128
            + a[4] as u128 * b[1] as u128;
        let t6 = a[2] as u128 * b[4] as u128 + a[3] as u128 * b[3] as u128 + a[4] as u128 * b[2] as u128;
        let t7 = a[3] as u128 * b[4] as u128 + a[4] as u128 * b[3] as u128;
        let t8 = a[4] as u128 * b[4] as u128;

        // 2^255 ≡ 19：把高 5 项折叠进低 5 项
        let mut h = [
            t0 + 19 * t5,
            t1 + 19 * t6,
            t2 + 19 * t7,
            t3 + 19 * t8,
            t4,
        ];

        // 进位链（四轮，结果肢体 < 2^51）
        let m = MASK51 as u128;
        for _ in 0..4 {
            let mut c = h[0] >> 51;
            h[1] += c;
            h[0] &= m;
            c = h[1] >> 51;
            h[2] += c;
            h[1] &= m;
            c = h[2] >> 51;
            h[3] += c;
            h[2] &= m;
            c = h[3] >> 51;
            h[4] += c;
            h[3] &= m;
            c = h[4] >> 51;
            h[0] += c * 19;
            h[4] &= m;
        }

        Fe([h[0] as u64, h[1] as u64, h[2] as u64, h[3] as u64, h[4] as u64]).reduced()
    }

    /// 规范归约后的副本。
    fn reduced(&self) -> Fe {
        let mut r = *self;
        r.strong_reduce();
        r
    }

    /// 幂运算（平方-乘，从最高位开始）。指数为 256 位小端 4×u64。
    fn pow(&self, exp: &[u64; 4]) -> Fe {
        let mut acc = ONE;
        for i in (0..256).rev() {
            acc = acc.sq();
            if (exp[i / 64] >> (i % 64)) & 1 == 1 {
                acc = acc.mul(self);
            }
        }
        acc
    }

    fn invert(&self) -> Fe {
        self.pow(&EXP_PM2)
    }
}

// ==================== 扩展齐次坐标点 ====================

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    fn identity() -> Point {
        Point { x: ZERO, y: ONE, z: ONE, t: ZERO }
    }

    fn neg(&self) -> Point {
        Point { x: self.x.neg(), y: self.y, z: self.z, t: self.t.neg() }
    }

    /// 完整加法公式（HWCD-2008，a = -1）。适用于任意输入（含恒等元）。
    fn add(&self, q: &Point) -> Point {
        let yx1 = self.y.sub(&self.x);
        let yx2 = q.y.sub(&q.x);
        let yy1 = self.y.add(&self.x);
        let yy2 = q.y.add(&q.x);
        let a = yx1.mul(&yx2);
        let b = yy1.mul(&yy2);
        let c = self.t.mul(&q.t).mul(&D2);
        let z1z2 = self.z.mul(&q.z);
        let d = z1z2.add(&z1z2);
        let e = b.sub(&a);
        let f = d.sub(&c);
        let g = d.add(&c);
        let h = b.add(&a);
        Point {
            x: e.mul(&f),
            y: g.mul(&h),
            t: e.mul(&h),
            z: f.mul(&g),
        }
    }

    /// 加倍（a = -1 专用公式）。
    fn double(&self) -> Point {
        let a = self.x.sq();
        let b = self.y.sq();
        let c = self.z.sq().add(&self.z.sq());
        let d = a.neg();
        let e = self.x.add(&self.y).sq().sub(&a).sub(&b);
        let g = d.add(&b);
        let f = g.sub(&c);
        let h = d.sub(&b);
        Point {
            x: e.mul(&f),
            y: g.mul(&h),
            t: e.mul(&h),
            z: f.mul(&g),
        }
    }

    /// 双倍加（对公开标量：平方-乘，非恒定时间，验签场景可接受）。
    fn scalarmult(p: &Point, s: &[u8; 32]) -> Point {
        let mut acc = Point::identity();
        for i in (0..256).rev() {
            acc = acc.double();
            if (s[i / 8] >> (i % 8)) & 1 == 1 {
                acc = acc.add(p);
            }
        }
        acc
    }

    /// 固定基点标量乘。
    fn scalarmult_base(s: &[u8; 32]) -> Point {
        let base = Point { x: BX, y: BY, z: ONE, t: BX.mul(&BY) };
        Point::scalarmult(&base, s)
    }

    /// 解压 32 字节编码（含符号位）→ 点；非曲线点返回 None。
    fn decompress(enc: &[u8; 32]) -> Option<Point> {
        let sign = (enc[31] >> 7) & 1;
        let mut yb = *enc;
        yb[31] &= 0x7f;
        let y = Fe::from_bytes(&yb);
        let y2 = y.sq();
        let u = y2.sub(&ONE);
        let v = y2.mul(&D).add(&ONE);
        let v3 = v.sq().mul(&v);
        let v7 = v3.sq().mul(&v);
        let uv7 = u.mul(&v7);
        let mut x = u.mul(&v3).mul(&uv7.pow(&EXP_P58));
        let vx2 = v.mul(&x.sq());
        if vx2 == u {
            // x^2 = u/v，正好
        } else if vx2 == u.neg() {
            // x^2 = -u/v → x *= sqrt(-1)
            x = x.mul(&SQRT_M1);
        } else {
            return None;
        }
        if x.is_zero() && sign == 1 {
            return None; // 拒绝 -0
        }
        // 按符号位调整 x
        let x = if (x.to_bytes()[0] & 1) != sign { x.neg() } else { x };
        Some(Point { x, y, z: ONE, t: x.mul(&y) })
    }

    /// 编码为 32 字节（y | (x & 1) << 255）。
    fn to_bytes(&self) -> [u8; 32] {
        let zinv = self.z.invert();
        let x = self.x.mul(&zinv);
        let y = self.y.mul(&zinv);
        let mut out = y.to_bytes();
        out[31] |= (x.to_bytes()[0] & 1) << 7;
        out
    }
}

// ==================== 标量工具 ====================

/// 256 位小端比较 a < b（仅用于公开值）。
fn lt(a: &[u8; 32], b: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

/// 512 位小端值 mod L → 32 字节小端。
/// 用长除法（256 位余数移位-比较-减），正确性直观，仅用于公开哈希。
fn sc_reduce(s: &[u8; 64]) -> [u8; 32] {
    let mut r = [0u8; 32];
    for i in (0..512).rev() {
        // r = r << 1 | bit_i
        let bit = (s[i / 8] >> (i % 8)) & 1;
        let mut carry = bit;
        for j in 0..32 {
            let nb = ((r[j] as u16) << 1) | carry as u16;
            r[j] = (nb & 0xff) as u8;
            carry = (nb >> 8) as u8;
        }
        // if r >= L: r -= L
        if !lt(&r, &L_BYTES) {
            let mut borrow = 0i32;
            for j in 0..32 {
                let d = r[j] as i32 - L_BYTES[j] as i32 - borrow;
                if d < 0 {
                    r[j] = (d + 256) as u8;
                    borrow = 1;
                } else {
                    r[j] = d as u8;
                    borrow = 0;
                }
            }
        }
    }
    r
}

// ==================== 验签入口 ====================

/// 验证 Ed25519 签名（RFC 8032）：
/// `sig` = R‖S（各 32 字节），`pk` = 32 字节公钥，`msg` = 消息。
///
/// 返回 true 当且仅当：
/// 1. S < L（拒绝非规范标量）；
/// 2. 公钥可解压为曲线点；
/// 3. [S]B = R + [h]A，其中 h = SHA-512(R ‖ A ‖ msg) mod L。
pub fn verify(pk: &[u8; PK_LEN], msg: &[u8], sig: &[u8; SIG_LEN]) -> bool {
    // 1. S < L
    let s: &[u8; 32] = &sig[32..64].try_into().unwrap();
    if !lt(s, &L_BYTES) {
        return false;
    }
    // 2. 公钥解压
    let Some(a) = Point::decompress(pk) else {
        return false;
    };
    // 3. h = SHA-512(R ‖ A ‖ msg) mod L
    let mut sh = Sha512::new();
    sh.update(&sig[..32]);
    sh.update(pk);
    sh.update(msg);
    let digest = sh.finalize();
    let h = sc_reduce(&digest);
    // 4. R_check = [S]B - [h]A
    let s_arr: [u8; 32] = sig[32..64].try_into().unwrap();
    let sb = Point::scalarmult_base(&s_arr);
    let ha = Point::scalarmult(&a, &h);
    let r_check = sb.add(&ha.neg());
    // 5. 编码比较
    r_check.to_bytes() == sig[..32]
}
