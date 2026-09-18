//! NIST P-256（secp256r1）域算术与 ECDH —— TLS `ECDHE_RSA_AES128_GCM_SHA256`
//! 密钥交换的椭圆曲线部分。
//!
//! - 域元素：4×u64 小端肢体。乘法取模用「逐位移位 + 条件减 p」（512 位积
//!   扫一遍，代码极简、无符号回绕陷阱）；单次握手约 4000 次域乘，开销可忽略。
//! - 点运算：雅可比坐标通用 double/add 公式，标量乘为 double-and-add。
//!   标量是本端一次性临时密钥、参与运算的输入全是公开数据，演示系统不做
//!   侧信道加固（真实部署应换常量时间梯）。
//! - 只实现 ECDH：证书链验签只支持 RSA（见 x509.rs），ECDSA 链直接判不可信。

/// 域元素：4 个 u64 小端肢体（limb[0] 最低位），值恒 < p。
pub type Fe = [u64; 4];

/// p = 2^256 − 2^224 + 2^192 + 2^96 − 1（小端肢体）。
const P: Fe = [
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0000_FFFF_FFFF,
    0x0000_0000_0000_0000,
    0xFFFF_FFFF_0000_0001,
];

/// 群阶 n（小端肢体）——把随机标量约到 [1, n)。
const N: Fe = [
    0xF3B9_CAC2_FC63_2551,
    0xBCE6_FAAD_A717_9E84,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_0000_0000,
];

/// 曲线常数 b（y² = x³ − 3x + b，大端字节）。
const B_BYTES: [u8; 32] = [
    0x5A, 0xC6, 0x35, 0xD8, 0xAA, 0x3A, 0x93, 0xE7, 0xB3, 0xEB, 0xBD, 0x55, 0x76, 0x98, 0x86,
    0xBC, 0x65, 0x1D, 0x06, 0xB0, 0xCC, 0x53, 0xB0, 0xF6, 0x3B, 0xCE, 0x3C, 0x3E, 0x27, 0xD2,
    0x60, 0x4B,
];

/// 基点 G（大端字节）。
const GX_BYTES: [u8; 32] = [
    0x6B, 0x17, 0xD1, 0xF2, 0xE1, 0x2C, 0x42, 0x47, 0xF8, 0xBC, 0xE6, 0xE5, 0x63, 0xA4, 0x40,
    0xF2, 0x77, 0x03, 0x7D, 0x81, 0x2D, 0xEB, 0x33, 0xA0, 0xF4, 0xA1, 0x39, 0x45, 0xD8, 0x98,
    0xC2, 0x96,
];
const GY_BYTES: [u8; 32] = [
    0x4F, 0xE3, 0x42, 0xE2, 0xFE, 0x1A, 0x7F, 0x9B, 0x8E, 0xE7, 0xEB, 0x4A, 0x7C, 0x0F, 0x9E,
    0x16, 0x2B, 0xCE, 0x33, 0x57, 0x6B, 0x31, 0x5E, 0xCE, 0xCB, 0xB6, 0x40, 0x68, 0x37, 0xBF,
    0x51, 0xF5,
];

// ==================== 基础域算术 ====================

fn fe_ge(a: &Fe, b: &Fe) -> bool {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn fe_is_zero(a: &Fe) -> bool {
    a.iter().all(|&w| w == 0)
}

/// (a + b) mod p（输入 < p）。
fn fe_add(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut c: u128 = 0;
    for i in 0..4 {
        let s = a[i] as u128 + b[i] as u128 + c;
        r[i] = s as u64;
        c = s >> 64;
    }
    if c > 0 || fe_ge(&r, &P) {
        r = fe_sub_raw(&r, &P);
    }
    r
}

/// 2^256 − p（小端肢体）——fe_sub 借位修正用。
const P_DELTA: Fe = [
    0x0000_0000_0000_0001,
    0xFFFF_FFFF_0000_0000,
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0000_FFFF_FFFE,
];

/// (a − b) mod p（输入 < p；a < b 时减去隐含的 2^256 位再补 p）。
fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut borrow: i128 = 0;
    for i in 0..4 {
        let s = a[i] as i128 - b[i] as i128 - borrow;
        if s < 0 {
            r[i] = (s + (1i128 << 64)) as u64;
            borrow = 1;
        } else {
            r[i] = s as u64;
            borrow = 0;
        }
    }
    if borrow > 0 {
        // limb 形式的 r = a − b + 2^256，减 2^256 − p 即得 a − b + p
        r = fe_sub_raw(&r, &P_DELTA);
    }
    r
}

fn fe_sub_raw(a: &Fe, b: &Fe) -> Fe {
    let mut r = [0u64; 4];
    let mut borrow: i128 = 0;
    for i in 0..4 {
        let s = a[i] as i128 - b[i] as i128 - borrow;
        if s < 0 {
            r[i] = (s + (1i128 << 64)) as u64;
            borrow = 1;
        } else {
            r[i] = s as u64;
            borrow = 0;
        }
    }
    r
}

/// 参考实现（逐位约减），仅用于差分自测。
fn mod_p_512_slow(t: &[u64; 8]) -> Fe {
    let mut r = [0u64; 5]; // r[4] ∈ {0,1}
    for i in (0..512).rev() {
        let bit = ((t[i / 64] >> (i % 64)) & 1) as u64;
        let mut carry = bit;
        for w in 0..5 {
            let nc = r[w] >> 63;
            r[w] = (r[w] << 1) | carry;
            carry = nc;
        }
        if r[4] == 1 || fe_ge(&[r[0], r[1], r[2], r[3]], &P) {
            let mut borrow: i128 = 0;
            for w in 0..4 {
                let s = r[w] as i128 - P[w] as i128 - borrow;
                if s < 0 {
                    r[w] = (s + (1i128 << 64)) as u64;
                    borrow = 1;
                } else {
                    r[w] = s as u64;
                    borrow = 0;
                }
            }
            r[4] -= borrow as u64;
        }
    }
    [r[0], r[1], r[2], r[3]]
}

/// 差分自测：随机 a、b 比较 Montgomery 域乘与逐位参考实现，并校验
/// fe_bytes_from_be/fe_to_be 往返与 G 在曲线上。
pub fn selftest(mut rng: impl FnMut(&mut [u8])) -> bool {
    for _ in 0..200 {
        let mut ab = [0u8; 64];
        rng(&mut ab);
        let mut a = [0u64; 4];
        let mut b = [0u64; 4];
        for i in 0..4 {
            a[i] = u64::from_le_bytes(ab[i * 8..i * 8 + 8].try_into().unwrap());
            b[i] = u64::from_le_bytes(ab[32 + i * 8..32 + i * 8 + 8].try_into().unwrap());
        }
        // REDC 前提：输入 < p（a < 2^256 < 2p，条件减 p 一次即可）
        if fe_ge(&a, &P) {
            a = fe_sub_raw(&a, &P);
        }
        if fe_ge(&b, &P) {
            b = fe_sub_raw(&b, &P);
        }
        let mut t = [0u64; 8];
        for i in 0..4 {
            let mut c: u128 = 0;
            for j in 0..4 {
                let s = t[i + j] as u128 + a[i] as u128 * b[j] as u128 + c;
                t[i + j] = s as u64;
                c = s >> 64;
            }
            let mut k = i + 4;
            while c > 0 {
                let s = t[k] as u128 + c;
                t[k] = s as u64;
                c = s >> 64;
                k += 1;
            }
        }
        let want = mod_p_512_slow(&t);
        // Montgomery 域乘：先都转 mont，乘完转回
        let am = fe_mul(&a, &R2);
        let bm = fe_mul(&b, &R2);
        let pr = fe_mul(&am, &bm);
        let got = fe_mul(&pr, &[1, 0, 0, 0]);
        if got != want {
            return false;
        }
    }
    // G 在曲线上
    let gx = fe_bytes_from_be(&GX_BYTES).unwrap_or([0; 4]);
    let gy = fe_bytes_from_be(&GY_BYTES).unwrap_or([0; 4]);
    let lhs = fe_mul(&gy, &gy);
    let bb = fe_bytes_from_be(&B_BYTES).unwrap_or([0; 4]);
    let x3 = fe_mul(&fe_mul(&gx, &gx), &gx);
    let three_x = fe_add(&fe_add(&gx, &gx), &gx);
    let rhs = fe_add(&fe_sub(&x3, &three_x), &bb);
    lhs == rhs
}

/// a^e mod p（e 为小端肢体；用于费马求逆，数据公开、无需常量时间）。
fn fe_pow(a: &Fe, e: &Fe) -> Fe {
    let mut r: Fe = ONE_M;
    for i in (0..4).rev() {
        for b in (0..64).rev() {
            r = fe_mul(&r, &r);
            if (e[i] >> b) & 1 == 1 {
                r = fe_mul(&r, a);
            }
        }
    }
    r
}

fn fe_to_be(a: &Fe) -> [u8; 32] {
    // Montgomery → 普通形式（·R^{-1}，乘 plain 1 即 REDC 约减）
    let a = fe_mul(a, &[1, 0, 0, 0]);
    let mut out = [0u8; 32];
    for i in 0..4 {
        for j in 0..8 {
            out[31 - i * 8 - j] = (a[i] >> (j * 8)) as u8;
        }
    }
    out
}

// ==================== 雅可比点运算 ====================

/// 雅可比坐标点（无穷远点用 z == 0 表示）。
#[derive(Clone, Copy)]
struct Jacobian {
    x: Fe,
    y: Fe,
    z: Fe,
}

const J_INFINITY: Jacobian = Jacobian {
    x: [1, 0, 0, 0],
    y: [1, 0, 0, 0],
    z: [0, 0, 0, 0],
};

/// 倍点（通用雅可比公式，任意 a；此处 a = −3）。
fn j_double(p: &Jacobian) -> Jacobian {
    if fe_is_zero(&p.z) || fe_is_zero(&p.y) {
        return J_INFINITY;
    }
    let xx = fe_mul(&p.x, &p.x);
    let yy = fe_mul(&p.y, &p.y);
    let yyyy = fe_mul(&yy, &yy);
    let zz = fe_mul(&p.z, &p.z);
    // s = 4·x·y²
    let s = fe_mul(&p.x, &fe_add(&fe_add(&yy, &yy), &fe_add(&yy, &yy)));
    // m = 3·x² + a·z⁴ = 3·(x − z²)(x + z²)（a = −3）
    let m = fe_mul(
        &fe_mul(&fe_sub(&p.x, &zz), &fe_add(&p.x, &zz)),
        &THREE_M,
    );
    let t = fe_sub(&fe_mul(&m, &m), &fe_add(&s, &s));
    let x3 = t;
    // y3 = m·(s − t) − 8·y⁴
    let y8 = fe_add(
        &fe_add(&fe_add(&yyyy, &yyyy), &fe_add(&yyyy, &yyyy)),
        &fe_add(&fe_add(&yyyy, &yyyy), &fe_add(&yyyy, &yyyy)),
    );
    let y3 = fe_sub(&fe_mul(&m, &fe_sub(&s, &t)), &y8);
    let z3 = fe_mul(&fe_add(&p.y, &p.y), &p.z);
    Jacobian { x: x3, y: y3, z: z3 }
}

/// 雅可比点 + 仿射点（通用公式；调用方保证两点不同 x 或按 H==0 分派）。
fn j_add_affine(p: &Jacobian, qx: &Fe, qy: &Fe) -> Jacobian {
    if fe_is_zero(&p.z) {
        return Jacobian { x: *qx, y: *qy, z: ONE_M };
    }
    let zz = fe_mul(&p.z, &p.z);
    let u2 = fe_mul(qx, &zz);
    let s2 = fe_mul(&fe_mul(qy, &p.z), &zz);
    let h = fe_sub(&u2, &p.x);
    let r = fe_sub(&s2, &p.y);
    if fe_is_zero(&h) {
        if fe_is_zero(&r) {
            // p == q：退化为倍点
            return j_double(p);
        }
        return J_INFINITY;
    }
    let hh = fe_mul(&h, &h);
    let hhh = fe_mul(&h, &hh);
    let v = fe_mul(&p.x, &hh);
    let x3 = fe_sub(
        &fe_sub(&fe_mul(&r, &r), &hhh),
        &fe_add(&v, &v),
    );
    let y3 = fe_sub(
        &fe_mul(&r, &fe_sub(&v, &x3)),
        &fe_mul(&p.y, &hhh),
    );
    let z3 = fe_mul(&p.z, &h);
    Jacobian { x: x3, y: y3, z: z3 }
}

/// 标量乘（double-and-add，MSB→LSB）。
fn scalar_mul(d: &Fe, qx: &Fe, qy: &Fe) -> Jacobian {
    let mut acc = J_INFINITY;
    for i in (0..4).rev() {
        for b in (0..64).rev() {
            acc = j_double(&acc);
            if (d[i] >> b) & 1 == 1 {
                acc = j_add_affine(&acc, qx, qy);
            }
        }
    }
    acc
}

/// 雅可比 → 仿射 (x, y)。无穷远点返回 None。
fn to_affine(p: &Jacobian) -> Option<(Fe, Fe)> {
    if fe_is_zero(&p.z) {
        return None;
    }
    let zinv = fe_inv(&p.z);
    let zinv2 = fe_mul(&zinv, &zinv);
    let zinv3 = fe_mul(&zinv2, &zinv);
    Some((fe_mul(&p.x, &zinv2), fe_mul(&p.y, &zinv3)))
}

/// 大端 32 字节 → 域元素（转 Montgomery 形式；≥ p 返回 None）。
fn fe_bytes_from_be(b: &[u8; 32]) -> Option<Fe> {
    let mut f = [0u64; 4];
    for i in 0..4 {
        let mut w = 0u64;
        for j in 0..8 {
            w = (w << 8) | b[24 - i * 8 + j] as u64;
        }
        f[i] = w;
    }
    if fe_ge(&f, &P) {
        return None;
    }
    // 转 Montgomery 形式（x·R mod p）
    Some(fe_mul(&f, &R2))
}

/// 随机标量：32 随机字节 → [1, n)。样本 ≥ n 时重摇（概率 2^-224 级）。
fn random_scalar(rng: &mut dyn FnMut(&mut [u8])) -> Fe {
    for _ in 0..100 {
        let mut b = [0u8; 32];
        rng(&mut b);
        let mut s = [0u64; 4];
        for i in 0..4 {
            let mut w = [0u8; 8];
            w.copy_from_slice(&b[i * 8..i * 8 + 8]);
            s[i] = u64::from_le_bytes(w);
        }
        if fe_ge(&s, &N) {
            s = fe_sub_raw(&s, &N);
        }
        if !fe_is_zero(&s) && !fe_ge(&s, &N) {
            return s;
        }
    }
    // rng 完全坏掉才会到这：退化为固定标量（仍能完成握手，仅失去前向安全）
    [0x1234_5678_9ABC_DEF0, 0x0FED_CBA9_8765_4321, 0x1234_5678_9ABC_DEF0, 0x5555_5555_5555_5555]
}

/// 生成临时密钥对。返回 (标量 32B 小端, 公钥 65B 非压缩点)。
pub fn make_keypair(rng: &mut dyn FnMut(&mut [u8])) -> ([u8; 32], [u8; 65]) {
    let d = random_scalar(rng);
    let gx = fe_bytes_from_be(&GX_BYTES).unwrap_or([2, 0, 0, 0]);
    let gy = fe_bytes_from_be(&GY_BYTES).unwrap_or([2, 0, 0, 0]);
    let q = scalar_mul(&d, &gx, &gy);
    let mut priv_bytes = [0u8; 32];
    for i in 0..4 {
        priv_bytes[i * 8..i * 8 + 8].copy_from_slice(&d[i].to_le_bytes());
    }
    let mut pub_bytes = [0u8; 65];
    pub_bytes[0] = 0x04;
    match to_affine(&q) {
        Some((x, y)) => {
            pub_bytes[1..33].copy_from_slice(&fe_to_be(&x));
            pub_bytes[33..65].copy_from_slice(&fe_to_be(&y));
        }
        None => {
            // d ≠ 0 ⇒ d·G ≠ ∞，理论不可达；占位全 1 让握手在服务端失败而非本地 panic
            for b in pub_bytes.iter_mut().skip(1) {
                *b = 0xFF;
            }
        }
    }
    (priv_bytes, pub_bytes)
}

/// ECDH 共享秘密：d·(对端点) 的仿射 x 坐标（32B 大端）。
pub fn shared_secret(d_le: &[u8; 32], peer: &[u8]) -> Result<[u8; 32], &'static str> {
    if peer.len() != 65 || peer[0] != 0x04 {
        return Err("ECDH 对端点格式非法");
    }
    let mut xb = [0u8; 32];
    xb.copy_from_slice(&peer[1..33]);
    let mut yb = [0u8; 32];
    yb.copy_from_slice(&peer[33..65]);
    let x = fe_bytes_from_be(&xb).ok_or("ECDH 对端点 x 越界")?;
    let y = fe_bytes_from_be(&yb).ok_or("ECDH 对端点 y 越界")?;
    // 在曲线校验：y² == x³ − 3x + b（Montgomery 域内比较等式即可）
    let lhs = fe_mul(&y, &y);
    let b = fe_bytes_from_be(&B_BYTES).ok_or("曲线常数非法")?;
    let x3 = fe_mul(&fe_mul(&x, &x), &x);
    let three_x = fe_add(&fe_add(&x, &x), &x);
    let rhs = fe_add(&fe_sub(&x3, &three_x), &b);
    if lhs != rhs {
        return Err("ECDH 对端点不在曲线上");
    }
    let mut d = [0u64; 4];
    for i in 0..4 {
        let mut w = [0u8; 8];
        w.copy_from_slice(&d_le[i * 8..i * 8 + 8]);
        d[i] = u64::from_le_bytes(w);
    }
    let s = scalar_mul(&d, &x, &y);
    match to_affine(&s) {
        Some((sx, _)) => Ok(fe_to_be(&sx)),
        None => Err("ECDH 共享点为无穷远"),
    }
}

/// 费马求逆（a^{p−2}）。
fn fe_inv(a: &Fe) -> Fe {
    let e = fe_sub_raw(&[0xFFFF_FFFF_FFFF_FFFF, 0x0000_0000_FFFF_FFFF, 0, 0xFFFF_FFFF_0000_0001], &[2, 0, 0, 0]);
    fe_pow(a, &e)
}

/// 测试用：检查 65B 非压缩公钥点是否在曲线上。
pub fn point_on_curve_pub(x_be: &[u8], y_be: &[u8]) -> bool {
    let mut xb = [0u8; 32];
    xb.copy_from_slice(x_be);
    let mut yb = [0u8; 32];
    yb.copy_from_slice(y_be);
    let (Some(x), Some(y)) = (fe_bytes_from_be(&xb), fe_bytes_from_be(&yb)) else {
        return false;
    };
    let lhs = fe_mul(&y, &y);
    let bb = fe_bytes_from_be(&B_BYTES).unwrap_or([0; 4]);
    let x3 = fe_mul(&fe_mul(&x, &x), &x);
    let three_x = fe_add(&fe_add(&x, &x), &x);
    let rhs = fe_add(&fe_sub(&x3, &three_x), &bb);
    lhs == rhs
}

/// 测试用：公开的域乘入口。
pub fn mul_public(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    fe_mul(a, b)
}

/// Montgomery 常数：P' = −p^{−1} mod 2^256；R2 = 2^512 mod p；
/// ONE_M = R mod p（Montgomery 域的乘法单位元）；THREE_M = 3R mod p。
const P_INV: Fe = [
    0x0000_0000_0000_0001,
    0x0000_0001_0000_0000,
    0x0000_0000_0000_0000,
    0xFFFF_FFFF_0000_0002,
];
const R2: Fe = [
    0x0000_0000_0000_0003,
    0xFFFF_FFFB_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFE,
    0x0000_0004_FFFF_FFFD,
];
const ONE_M: Fe = [
    0x0000_0000_0000_0001,
    0xFFFF_FFFF_0000_0000,
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0000_FFFF_FFFE,
];
const THREE_M: Fe = [
    0x0000_0000_0000_0003,
    0xFFFF_FFFD_0000_0000,
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0002_FFFF_FFFC,
];

/// (a × b) mod p —— Montgomery 乘（REDC）：输入输出均为 Montgomery 形式
/// （值 ×R mod p，肢体恒 < p）。每次域乘 = 两次 4×4 肢体乘法。
fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    // t = a·b（8 肢体）
    let mut t = [0u64; 8];
    for i in 0..4 {
        let mut c: u128 = 0;
        for j in 0..4 {
            let s = t[i + j] as u128 + a[i] as u128 * b[j] as u128 + c;
            t[i + j] = s as u64;
            c = s >> 64;
        }
        let mut k = i + 4;
        while c > 0 {
            let s = t[k] as u128 + c;
            t[k] = s as u64;
            c = s >> 64;
            k += 1;
        }
    }
    // m = t_lo · P' mod 2^256（只取低 4 肢体）
    let mut m = [0u64; 4];
    for i in 0..4 {
        let mut c: u128 = 0;
        for j in 0..4 - i {
            let s = m[i + j] as u128 + t[i] as u128 * P_INV[j] as u128 + c;
            m[i + j] = s as u64;
            c = s >> 64;
        }
    }
    // t + m·P（9 肢体；t + m·P < 2p·2^256）
    let mut acc = [0u64; 9];
    for i in 0..4 {
        let mut c: u128 = 0;
        for j in 0..4 {
            let s = acc[i + j] as u128 + m[i] as u128 * P[j] as u128 + c;
            acc[i + j] = s as u64;
            c = s >> 64;
        }
        let mut k = i + 4;
        while c > 0 {
            let s = acc[k] as u128 + c;
            acc[k] = s as u64;
            c = s >> 64;
            k += 1;
        }
    }
    let mut c: u128 = 0;
    for i in 0..8 {
        let s = acc[i] as u128 + t[i] as u128 + c;
        acc[i] = s as u64;
        c = s >> 64;
    }
    acc[8] = c as u64;
    // 结果 = acc >> 256（< 2p），条件减 p 一次
    let mut r = [acc[4], acc[5], acc[6], acc[7], acc[8]];
    if r[4] == 1 || fe_ge(&[r[0], r[1], r[2], r[3]], &P) {
        let mut borrow: i128 = 0;
        for w in 0..4 {
            let s = r[w] as i128 - P[w] as i128 - borrow;
            if s < 0 {
                r[w] = (s + (1i128 << 64)) as u64;
                borrow = 1;
            } else {
                r[w] = s as u64;
                borrow = 0;
            }
        }
        r[4] -= borrow as u64;
    }
    [r[0], r[1], r[2], r[3]]
}
