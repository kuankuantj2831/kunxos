//! Ed25519 验签（RFC 8032）自研实现，零依赖、`no_std`。
//!
//! 仅实现验签（设备侧不需要私钥；签名由主机侧 `tools/kunxrepo/ed25519.py`
//! 或 `tools/kunxsign` 生成，二者算法同构）。验签输入全部是公开数据
//! （公钥/签名/消息），因此不要求常量时间（常量时间比较仅用于最终签名比对）。
//!
//! 域参数：p = 2^255 - 19，L = 2^252 + 0x14def9dea2f79cd65812631a5cf5d3ed，
//! d = -121665/121666，基点 B 由 y = 4/5 恢复。内部哈希：SHA-512。
//!
//! 域元素用 4 个 64 位肢体（基 2^64）表示，规范形式 < p。

use crate::sha512::Sha512;


/// 域素数 p = 2^255 - 19（64 位小端肢体）。
const P: [u64; 4] = [
    0xFFFF_FFFF_FFFF_FFED,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
    0x7FFF_FFFF_FFFF_FFFF,
];

/// 群阶 L = 2^252 + 0x14def9dea2f79cd65812631a5cf5d3ed（64 位小端肢体）。
const L: [u64; 4] = [
    0x5812_631A_5CF5_D3ED,
    0x14DE_F9DE_A2F7_9CD6,
    0x0000_0000_0000_0000,
    0x1000_0000_0000_0000,
];

/// p 的 8 肢体形式（用于 512 位输入约简判断）。
const P_EXT: [u64; 8] = [
    0xFFFF_FFFF_FFFF_FFED,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
    0x7FFF_FFFF_FFFF_FFFF,
    0,
    0,
    0,
    0,
];

/// L 的 8 肢体形式。
const L_EXT: [u64; 8] = [L[0], L[1], L[2], L[3], 0, 0, 0, 0];

/// 域元素：4 个 64 位肢体（基 2^64），规范化后 < p。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fe([u64; 4]);

impl Fe {
    const ZERO: Fe = Fe([0, 0, 0, 0]);
    const ONE: Fe = Fe([1, 0, 0, 0]);
}

/// 小整数 → 域元素（限 v < 2^64，自动对 p 约简）。
fn fe_from_u64(v: u64) -> Fe {
    let mut f = Fe([v, 0, 0, 0]);
    fe_canon(&mut f);
    f
}

/// 32 字节小端 → 域元素（无约简；调用方保证结果 < 2^256）。
fn fe_from_bytes_le(b: &[u8; 32]) -> Fe {
    let mut f = Fe::ZERO;
    for i in 0..4 {
        let mut v = 0u64;
        for j in 0..8 {
            v |= (b[i * 8 + j] as u64) << (8 * j);
        }
        f.0[i] = v;
    }
    f
}

/// 域元素 → 32 字节小端（规范化）。
fn fe_to_bytes_le(f: &Fe) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..i * 8 + 8].copy_from_slice(&f.0[i].to_le_bytes());
    }
    out
}

/// 判断 `f >= p`。
fn ge_p(f: &Fe) -> bool {
    for i in (0..4).rev() {
        if f.0[i] > P[i] {
            return true;
        }
        if f.0[i] < P[i] {
            return false;
        }
    }
    true
}

/// `f -= p`（调用方保证 `f >= p`）。
fn sub_p(f: &mut Fe) {
    let mut borrow = 0u64;
    for i in 0..4 {
        let (d, b1) = f.0[i].overflowing_sub(P[i]);
        let (d, b2) = d.overflowing_sub(borrow);
        f.0[i] = d;
        borrow = (b1 as u8 + b2 as u8).min(1) as u64;
    }
}

/// 约简到规范形式（< p）：循环减去 p（至多 4 次）。
fn fe_canon(f: &mut Fe) {
    let mut guard = 0;
    while ge_p(f) && guard < 4 {
        sub_p(f);
        guard += 1;
    }
}

/// 把跨过 2^256 的进位 `c` 折叠进 `r`：c·2^256 ≡ c·38 (mod p)。
fn fold_carry(r: &mut Fe, mut c: u128) {
    for _ in 0..2 {
        if c == 0 {
            break;
        }
        let mut n = c.wrapping_mul(38);
        for i in 0..4 {
            let s = r.0[i] as u128 + n;
            r.0[i] = s as u64;
            n = s >> 64;
        }
        c = n;
    }
}

/// 加：`a + b`（mod p）。
fn fe_add(a: &Fe, b: &Fe) -> Fe {
    let mut r = Fe::ZERO;
    let mut carry = 0u128;
    for i in 0..4 {
        let s = a.0[i] as u128 + b.0[i] as u128 + carry;
        r.0[i] = s as u64;
        carry = s >> 64;
    }
    fold_carry(&mut r, carry);
    fe_canon(&mut r);
    r
}

/// 减：`a - b`（mod p）。
///
/// 用 4 个有符号肢体精确表示 V = a − b，取最高非零肢体的符号判断正负；
/// 负则加 p（V ∈ (−p, p) ⇒ V + p ∈ (0, 2p)），再做 2^64 基进位规范化与
/// 规范约简。不经过 2^256 补码，避免 2^256 ≢ 0 (mod p) 造成的偏差。
fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    let mut s = [0i128; 4];
    for i in 0..4 {
        s[i] = a.0[i] as i128 - b.0[i] as i128;
    }
    // 符号：从最高位搜第一个非零肢体
    let mut neg = false;
    for i in (0..4).rev() {
        if s[i] > 0 {
            break;
        }
        if s[i] < 0 {
            neg = true;
            break;
        }
    }
    if neg {
        const PI: [i128; 4] = [
            0xFFFF_FFFF_FFFF_FFED,
            0xFFFF_FFFF_FFFF_FFFF,
            0xFFFF_FFFF_FFFF_FFFF,
            0x7FFF_FFFF_FFFF_FFFF,
        ];
        for i in 0..4 {
            s[i] += PI[i];
        }
    }
    // 2^64 基进位规范化（结果已非负）
    let mut r = Fe::ZERO;
    let mut carry = 0i128;
    for i in 0..4 {
        let total = s[i] + carry;
        r.0[i] = (total & 0xFFFF_FFFF_FFFF_FFFF) as u64;
        carry = total >> 64;
    }
    if carry > 0 {
        fold_carry(&mut r, carry as u128);
    }
    fe_canon(&mut r);
    r
}

/// 取负：`-a`（mod p）。
fn fe_neg(a: &Fe) -> Fe {
    fe_sub(&Fe::ZERO, a)
}

/// 平方。
fn fe_sq(a: &Fe) -> Fe {
    fe_mul(a, a)
}

/// 乘：`a * b`（mod p）。逐项 64×64 → 128 位乘，高位肢体 ×38 折叠。
fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    let mut t = [0u128; 8];
    for i in 0..4 {
        for j in 0..4 {
            let p = a.0[i] as u128 * b.0[j] as u128;
            let s = t[i + j] + p;
            t[i + j] = s & 0xFFFF_FFFF_FFFF_FFFF;
            t[i + j + 1] += s >> 64;
        }
    }
    let mut r = Fe::ZERO;
    let mut carry = 0u128;
    for i in 0..4 {
        let s = t[i] + t[i + 4].wrapping_mul(38) + carry;
        r.0[i] = s as u64;
        carry = s >> 64;
    }
    // carry·2^256 ≡ carry·38 (mod p)
    fold_carry(&mut r, carry);
    fe_canon(&mut r);
    r
}

/// 模幂：`base^exp`（`exp` 为 256 位小端肢体）。
fn fe_pow(base: &Fe, exp: &[u64; 4]) -> Fe {
    let mut acc = Fe::ONE;
    let b = *base;
    // 平方-乘：从最高位（word3 的 bit63）到最低位（word0 的 bit0）逐位处理
    for word in (0..4).rev() {
        let e = exp[word];
        for bit in (0..64).rev() {
            acc = fe_sq(&acc);
            if (e >> bit) & 1 != 0 {
                acc = fe_mul(&acc, &b);
            }
        }
    }
    acc
}

/// 模逆：`a^(p-2)`。
fn fe_inv(a: &Fe) -> Fe {
    // p - 2 = 2^255 - 21
    let e = [
        0xFFFF_FFFF_FFFF_FFEB,
        0xFFFF_FFFF_FFFF_FFFF,
        0xFFFF_FFFF_FFFF_FFFF,
        0x7FFF_FFFF_FFFF_FFFF,
    ];
    fe_pow(a, &e)
}

/// 平方根恢复：给定 y 与符号位，求 x² = (y²-1)/(d·y²+1) 的根。
/// p ≡ 5 (mod 8)：先求 x = (u/v)^((p+3)/8)，再按 ±√(-1) 校正。
fn recover_x(y: &Fe, sign: bool) -> Option<Fe> {
    if ge_p(y) {
        return None; // y >= p 非法
    }
    let y2 = fe_sq(y);
    let u = fe_sub(&y2, &Fe::ONE);
    let v = fe_add(&fe_mul(&curve_d(), &y2), &Fe::ONE);
    // x = u·v³·(u·v⁷)^((p-5)/8)
    let v3 = fe_mul(&fe_sq(&v), &v);
    let v7 = fe_mul(&fe_sq(&v3), &v);
    let uv7 = fe_mul(&u, &v7);
    let e = [
        0xFFFF_FFFF_FFFF_FFFD,
        0xFFFF_FFFF_FFFF_FFFF,
        0xFFFF_FFFF_FFFF_FFFF,
        0x0FFF_FFFF_FFFF_FFFF,
    ]; // (p-5)/8 = 2^252 - 3
    let x = fe_mul(&fe_mul(&u, &v3), &fe_pow(&uv7, &e));
    let vx2 = fe_mul(&v, &fe_sq(&x));
    let x = if vx2 == u {
        x
    } else if vx2 == fe_neg(&u) {
        let xc = fe_mul(&x, &sqrt_m1());
        if fe_mul(&v, &fe_sq(&xc)) != u {
            return None;
        }
        xc
    } else {
        return None;
    };
    // 符号校正：x 的奇偶须匹配 sign（用返回值的 let 绑定，避免遮蔽丢失）
    let odd = |f: &Fe| f.0[0] & 1 != 0;
    let x = if odd(&x) != sign {
        let nx = fe_neg(&x);
        if odd(&nx) != sign {
            return None;
        }
        nx
    } else {
        x
    };
    Some(x)
}

/// 曲线常数 d = -121665/121666。
fn curve_d() -> Fe {
    let n = fe_from_u64(121665);
    let m = fe_from_u64(121666);
    fe_neg(&fe_mul(&n, &fe_inv(&m)))
}

/// √(-1) mod p = 2^((p-1)/4) = 2^(2^253 - 5)。
fn sqrt_m1() -> Fe {
    let e = [
        0xFFFF_FFFF_FFFF_FFFB,
        0xFFFF_FFFF_FFFF_FFFF,
        0xFFFF_FFFF_FFFF_FFFF,
        0x1FFF_FFFF_FFFF_FFFF,
    ];
    fe_pow(&fe_from_u64(2), &e)
}

/// 基点 B：y = 4/5，x = recover_x(y, false)。
fn base_point() -> Point {
    let y = fe_mul(&fe_from_u64(4), &fe_inv(&fe_from_u64(5)));
    let x = recover_x(&y, false).expect("base point recover");
    Point {
        x,
        y,
        z: Fe::ONE,
        t: fe_mul(&x, &y),
    }
}

/// 仿射点（扩展齐次坐标）：x = X/Z，y = Y/Z，t = T/Z。
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// 单位元 (0, 1, 1, 0)。
fn point_identity() -> Point {
    Point {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    }
}

/// 点加（RFC 8032 §5.1.4 完整公式，a = -1）。
fn point_add(p: &Point, q: &Point) -> Point {
    let a = fe_mul(&fe_sub(&p.y, &p.x), &fe_sub(&q.y, &q.x));
    let b = fe_mul(&fe_add(&p.y, &p.x), &fe_add(&q.y, &q.x));
    let c = fe_mul(&fe_mul(&fe_mul(&fe_from_u64(2), &p.t), &q.t), &curve_d());
    let dd = fe_mul(&fe_mul(&fe_from_u64(2), &p.z), &q.z);
    let e = fe_sub(&b, &a);
    let f = fe_sub(&dd, &c);
    let g = fe_add(&dd, &c);
    let h = fe_add(&b, &a);
    Point {
        x: fe_mul(&e, &f),
        y: fe_mul(&g, &h),
        z: fe_mul(&f, &g),
        t: fe_mul(&e, &h),
    }
}

/// 标量乘：`[s]P`，s 为 256 位小端肢体。
fn scalarmult(p: &Point, s: &[u64; 4]) -> Point {
    let mut acc = point_identity();
    // 从左至右（MSB→LSB）逐位：先倍加，再按位加
    for word in (0..4).rev() {
        let e = s[word];
        for bit in (0..64).rev() {
            acc = point_add(&acc, &acc);
            if (e >> bit) & 1 != 0 {
                acc = point_add(&acc, p);
            }
        }
    }
    acc
}

/// 点 → 32 字节压缩编码。
fn point_encode(p: &Point) -> [u8; 32] {
    let zi = fe_inv(&p.z);
    let x = fe_mul(&p.x, &zi);
    let y = fe_mul(&p.y, &zi);
    let mut out = fe_to_bytes_le(&y);
    if x.0[0] & 1 != 0 {
        out[31] |= 0x80;
    }
    out
}

/// 32 字节压缩编码 → 点；失败返回 `None`。
/// 拒绝：y >= p、符号负零、不在曲线上。
fn point_decode(b: &[u8; 32]) -> Option<Point> {
    let sign = b[31] >> 7 != 0;
    let mut yb = *b;
    yb[31] &= 0x7F;
    let y = fe_from_bytes_le(&yb);
    let x = recover_x(&y, sign)?;
    Some(Point {
        x,
        y,
        z: Fe::ONE,
        t: fe_mul(&x, &y),
    })
}

// ==================== 标量（256 位小端肢体）运算 ====================

/// `a >= b`（256 位）。
fn scalar_ge(a: &[u64; 4], b: &[u64; 4]) -> bool {
    for i in (0..4).rev() {
        if a[i] > b[i] {
            return true;
        }
        if a[i] < b[i] {
            return false;
        }
    }
    true
}

/// `a -= b`（256 位，自动回绕）。
fn scalar_sub(a: &mut [u64; 4], b: &[u64; 4]) {
    let mut borrow = 0u64;
    for i in 0..4 {
        let (d, b1) = a[i].overflowing_sub(b[i]);
        let (d, b2) = d.overflowing_sub(borrow);
        a[i] = d;
        borrow = (b1 as u8 + b2 as u8).min(1) as u64;
    }
}

/// 任意 ≤ 512 位数（8 肢体小端）⊖ mod L（逐位二进制约简）。
fn mod_l(x: &[u64; 8]) -> [u64; 4] {
    let mut r = [0u64; 4];
    const BIT_P: &[u64; 8] = &P_EXT;
    let _ = BIT_P; // P_EXT 保留供日后扩展，当前仅 L 需要
    for word in (0..8).rev() {
        for bit in (0..64).rev() {
            // r = (r << 1) | (x 的当前位)：进位来自低肢体向高肢体
            let b = (x[word] >> bit) & 1;
            let mut nr = [0u64; 4];
            let mut carry = 0u64;
            for i in 0..4 {
                nr[i] = (r[i] << 1) | carry;
                carry = r[i] >> 63;
            }
            nr[0] |= b;
            r = nr;
            if scalar_ge(&r, &L) {
                scalar_sub(&mut r, &L);
            }
        }
    }
    r
}

/// 32 字节 → 256 位小端肢体。
fn limbs_from_bytes_le(b: &[u8; 32]) -> [u64; 4] {
    let mut limbs = [0u64; 4];
    for i in 0..4 {
        for j in 0..8 {
            limbs[i] |= (b[i * 8 + j] as u64) << (8 * j);
        }
    }
    limbs
}

// ==================== 验签 API ====================

/// 验签（纯 Ed25519，RFC 8032 §5.1.7）。
///
/// 返回 `true` 当且仅当：A、R 可解码（在曲线上）、S < L，
/// 且群方程 [S]B = R + [h]A 成立（h = SHA-512(R ‖ A ‖ M) mod L）。
pub fn verify(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let a = match point_decode(pk) {
        Some(p) => p,
        None => return false,
    };
    let mut r_bytes = [0u8; 32];
    r_bytes.copy_from_slice(&sig[0..32]);
    let r = match point_decode(&r_bytes) {
        Some(p) => p,
        None => return false,
    };
    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&sig[32..64]);
    let s = limbs_from_bytes_le(&s_bytes);
    if scalar_ge(&s, &L) {
        return false;
    }

    // h = SHA-512(R ‖ A ‖ M) mod L（完整 64 字节摘要 ⊖ mod L）
    let mut sha = Sha512::new();
    sha.update(&r_bytes);
    sha.update(pk);
    sha.update(msg);
    let d = sha.finalize();
    let mut dlimbs = [0u64; 8];
    for i in 0..8 {
        for j in 0..8 {
            dlimbs[i] |= (d[i * 8 + j] as u64) << (8 * j);
        }
    }
    let h = mod_l(&dlimbs);

    // [S]B == R + [h]A
    let g = base_point();
    let sb = scalarmult(&g, &s);
    let ha = scalarmult(&a, &h);
    let sum = point_add(&r, &ha);
    crate::ct_eq(&point_encode(&sb), &point_encode(&sum))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::println;
    use std::string::String;
    use std::vec::Vec;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let mut out = Vec::new();
        for i in 0..s.len() / 2 {
            out.push(u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap());
        }
        out
    }

    fn pk32(s: &str) -> [u8; 32] {
        hex(s).try_into().unwrap()
    }

    fn sig64(s: &str) -> [u8; 64] {
        hex(s).try_into().unwrap()
    }

    fn case(_secret_hex: &str, pk_hex: &str, msg_hex: &str, sig_hex: &str) {
        let pk = pk32(pk_hex);
        let msg = hex(msg_hex);
        let sig = sig64(sig_hex);
        assert!(verify(&pk, &msg, &sig), "vector must verify");
        let mut bad = sig;
        bad[0] ^= 1;
        assert!(!verify(&pk, &msg, &bad), "tampered signature must fail");
        if !msg.is_empty() {
            let mut badmsg = msg.clone();
            badmsg[0] ^= 1;
            assert!(!verify(&pk, &badmsg, &sig), "tampered message must fail");
        }
    }

    #[test]
    fn rfc8032_test1_empty() {
        case(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            "",
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
    }

    #[test]
    fn rfc8032_test2() {
        case(
            "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
            "72",
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        );
    }

    #[test]
    fn rfc8032_test3() {
        case(
            "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
            "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
            "af82",
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        );
    }

    #[test]
    fn rfc8032_sha_abc() {
        // 消息 = SHA-512("abc")
        case(
            "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
            "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
        );
    }

    #[test]
    fn rfc8032_test1024() {
        let msg_hex: &[&str] = &[
            "08b8b2b733424243760fe426a4b54908",
            "632110a66c2f6591eabd3345e3e4eb98",
            "fa6e264bf09efe12ee50f8f54e9f77b1",
            "e355f6c50544e23fb1433ddf73be84d8",
            "79de7c0046dc4996d9e773f4bc9efe57",
            "38829adb26c81b37c93a1b270b20329d",
            "658675fc6ea534e0810a4432826bf58c",
            "941efb65d57a338bbd2e26640f89ffbc",
            "1a858efcb8550ee3a5e1998bd177e93a",
            "7363c344fe6b199ee5d02e82d522c4fe",
            "ba15452f80288a821a579116ec6dad2b",
            "3b310da903401aa62100ab5d1a36553e",
            "06203b33890cc9b832f79ef80560ccb9",
            "a39ce767967ed628c6ad573cb116dbef",
            "efd75499da96bd68a8a97b928a8bbc10",
            "3b6621fcde2beca1231d206be6cd9ec7",
            "aff6f6c94fcd7204ed3455c68c83f4a4",
            "1da4af2b74ef5c53f1d8ac70bdcb7ed1",
            "85ce81bd84359d44254d95629e9855a9",
            "4a7c1958d1f8ada5d0532ed8a5aa3fb2",
            "d17ba70eb6248e594e1a2297acbbb39d",
            "502f1a8c6eb6f1ce22b3de1a1f40cc24",
            "554119a831a9aad6079cad88425de6bd",
            "e1a9187ebb6092cf67bf2b13fd65f270",
            "88d78b7e883c8759d2c4f5c65adb7553",
            "878ad575f9fad878e80a0c9ba63bcbcc",
            "2732e69485bbc9c90bfbd62481d9089b",
            "eccf80cfe2df16a2cf65bd92dd597b07",
            "07e0917af48bbb75fed413d238f5555a",
            "7a569d80c3414a8d0859dc65a46128ba",
            "b27af87a71314f318c782b23ebfe808b",
            "82b0ce26401d2e22f04d83d1255dc51a",
            "ddd3b75a2b1ae0784504df543af8969b",
            "e3ea7082ff7fc9888c144da2af58429e",
            "c96031dbcad3dad9af0dcbaaaf268cb8",
            "fcffead94f3c7ca495e056a9b47acdb7",
            "51fb73e666c6c655ade8297297d07ad1",
            "ba5e43f1bca32301651339e22904cc8c",
            "42f58c30c04aafdb038dda0847dd988d",
            "cda6f3bfd15c4b4c4525004aa06eeff8",
            "ca61783aacec57fb3d1f92b0fe2fd1a8",
            "5f6724517b65e614ad6808d6f6ee34df",
            "f7310fdc82aebfd904b01e1dc54b2927",
            "094b2db68d6f903b68401adebf5a7e08",
            "d78ff4ef5d63653a65040cf9bfd4aca7",
            "984a74d37145986780fc0b16ac451649",
            "de6188a7dbdf191f64b5fc5e2ab47b57",
            "f7f7276cd419c17a3ca8e1b939ae49e4",
            "88acba6b965610b5480109c8b17b80e1",
            "b7b750dfc7598d5d5011fd2dcc5600a3",
            "2ef5b52a1ecc820e308aa342721aac09",
            "43bf6686b64b2579376504ccc493d97e",
            "6aed3fb0f9cd71a43dd497f01f17c0e2",
            "cb3797aa2a2f256656168e6c496afc5f",
            "b93246f6b1116398a346f1a641f3b041",
            "e989f7914f90cc2c7fff357876e506b5",
            "0d334ba77c225bc307ba537152f3f161",
            "0e4eafe595f6d9d90d11faa933a15ef1",
            "369546868a7f3a45a96768d40fd9d034",
            "12c091c6315cf4fde7cb68606937380d",
            "b2eaaa707b4c4185c32eddcdd306705e",
            "4dc1ffc872eeee475a64dfac86aba41c",
            "0618983f8741c5ef68d3a101e8a3b8ca",
            "c60c905c15fc910840b94c00a0b9d0",
        ];
        case(
            "f5e5767cf153319517630f226876b86c8160cc583bc013744c6bf255f5cc0ee5",
            "278117fc144c72340f67d0f2316e8386ceffbf2b2428c9c51fef7c597f1d426e",
            &msg_hex.concat(),
            "0aab4c900501b3e24d7cdf4663326a3a87df5e4843b2cbdb67cbf6e460fec350aa5371b1508f9f4528ecea23c436d94b5e8fcd4f681e30a6ac00a9704a188a03",
        );
    }

    #[test]
    fn cross_check_laws() {
        // 域算术基础
        assert_eq!(fe_add(&Fe::ONE, &Fe::ONE), fe_from_u64(2));
        assert_eq!(fe_mul(&fe_from_u64(2), &fe_from_u64(3)), fe_from_u64(6));
        let x = fe_from_u64(12345678901);
        assert_eq!(fe_pow(&fe_from_u64(2), &[5, 0, 0, 0]), fe_from_u64(32), "pow 2^5");
        assert_eq!(fe_pow(&fe_from_u64(2), &[10, 0, 0, 0]), fe_from_u64(1024), "pow 2^10");
        // 费马小定理：x^(p-1) == 1 → x^(p-2)·x == 1
        assert_eq!(fe_mul(&x, &fe_inv(&x)), Fe::ONE, "inv");
        // 基点编码已知值（y = 4/5 恢复后应编码回该字节串）
        let bref: [u8; 32] = hex(
            "5866666666666666666666666666666666666666666666666666666666666666",
        )
        .try_into()
        .unwrap();
        let b = base_point();
        assert_eq!(point_encode(&b), bref, "basepoint encoding");
        assert!(point_decode(&bref).is_some(), "decode basepoint");
        // 模约简基本性质
        assert_eq!(mod_l(&[0, 0, 0, 0, 0, 0, 0, 0]), [0, 0, 0, 0]);
        assert_eq!(mod_l(&[L[0], L[1], L[2], L[3], 0, 0, 0, 0]), [0, 0, 0, 0], "L mod L = 0");
        let mut l5 = [0u64; 8];
        l5[0] = 5;
        assert_eq!(mod_l(&l5), [5, 0, 0, 0], "5 mod L = 5");
        // [0]B == 0，[1]B == B
        assert_eq!(point_encode(&scalarmult(&b, &[0, 0, 0, 0])), point_encode(&point_identity()));
        assert_eq!(point_encode(&scalarmult(&b, &[1, 0, 0, 0])), bref);
    }

    #[test]
    fn group_laws() {
        let b = base_point();
        let a = point_decode(&pk32(
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        ))
        .unwrap();
        let k1 = [0x1234_5678_9abc_def0u64, 0xfedc_ba98_7654_3210, 0x1111_2222_3333_4444, 0x5555_6666_7777_8888];
        let k2 = [0x8888_7777_6666_5555, 0x4444_3333_2222_1111, 0x0fed_cba9_8765_4321, 0x0a0b_0c0d_0e0f_0a0b];
        let mut sumk = [0u64; 4];
        let mut carry = 0u64;
        for i in 0..4 {
            let (s1, c1) = k1[i].overflowing_add(k2[i]);
            let (s2, c2) = s1.overflowing_add(carry);
            sumk[i] = s2;
            carry = c1 as u64 | c2 as u64;
        }
        println!("B x={:02x?} y={:02x?} t={:02x?}", b.x.0, b.y.0, b.t.0);
        println!("A x={:02x?} y={:02x?} t={:02x?}", a.x.0, a.y.0, a.t.0);
        let dv = curve_d();
        let dhex = fe_to_bytes_le(&dv);
        println!("my d bytes = {:02x?}", dhex);
        // A 点解码中间量对照
        let ay2 = fe_sq(&a.y);
        let au = fe_sub(&ay2, &Fe::ONE);
        let av = fe_add(&fe_mul(&dv, &ay2), &Fe::ONE);
        // 对准 python 的 vx2 地面真值：v·x² 其中 x 取候选根（cand）
        {
            let xc = Fe([
                0x3548d54d0f57c740, 0xa0d40e41841113b9, 0xbc3c05add9e793cc, 0x0295_9926_24a6_076d,
            ]);
            let vx2r = fe_mul(&fe_mul(&xc, &xc), &av);
            let uexp = Fe([
                0x478cb5730ff574cb, 0x6762bf46475f341c, 0x9ab6c156784b5d0c, 0x3f0d543247db1463,
            ]);
            let nuexp = fe_neg(&uexp);
            println!("vx2 == u  : {}", vx2r == uexp);
            println!("vx2 == -u : {}", vx2r == nuexp);
            println!("vx2 limbs = {:02x?}", vx2r.0);
            // 手动重放 point_decode 的 recover_x 分支
            let mut yb = pk32(
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
            );
            yb[31] &= 0x7F;
            let yy = fe_from_bytes_le(&yb);
            let yy2 = fe_sq(&yy);
            let uu = fe_sub(&yy2, &Fe::ONE);
            let vv = fe_add(&fe_mul(&dv, &yy2), &Fe::ONE);
            let vvv3 = fe_mul(&fe_sq(&vv), &vv);
            let vvv7 = fe_mul(&fe_sq(&vvv3), &vv);
            let uvv7 = fe_mul(&uu, &vvv7);
            let e1 = [
                0xFFFF_FFFF_FFFF_FFFD,
                0xFFFF_FFFF_FFFF_FFFF,
                0xFFFF_FFFF_FFFF_FFFF,
                0x0FFF_FFFF_FFFF_FFFF,
            ];
            let cand2 = fe_mul(&fe_mul(&uu, &vvv3), &fe_pow(&uvv7, &e1));
            let vx2b = fe_mul(&vv, &fe_sq(&cand2));
            println!("cand2 == vx2r x? {}", cand2 == xc);
            println!("replay vx2 == u : {}", vx2b == uu);
            println!("replay vx2 == -u: {}", vx2b == fe_neg(&uu));
        }
        println!("my y2 = {:02x?}", ay2.0);
        println!("my u  = {:02x?}", au.0);
        println!("my v  = {:02x?}", av.0);
        // [a+b]P == [a]P + [b]P（对 B 与一般点 A 都成立）
        for p in [&b, &a] {
            assert_eq!(
                point_encode(&point_add(&scalarmult(p, &k1), &scalarmult(p, &k2))),
                point_encode(&scalarmult(p, &sumk)),
                "distribution"
            );
        }
        // [L]A == [L]B == 单位元（群阶）
        assert_eq!(point_encode(&scalarmult(&a, &L)), point_encode(&point_identity()), "[L]A == O");
        assert_eq!(point_encode(&scalarmult(&b, &L)), point_encode(&point_identity()), "[L]B == O");
    }

    #[test]
    fn fe_random_properties() {
        let mut state: u64 = 0x243f6a8885a308d3;
        let mut nxt = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            state ^= state >> 29;
            state
        };
        let mut fe = [Fe::ZERO; 32];
        for i in 0..32 {
            let mut f = Fe([nxt(), nxt(), nxt(), nxt()]);
            fe_canon(&mut f);
            fe[i] = f;
        }
        for i in 0..32 {
            let a = &fe[i];
            let b = &fe[(i + 1) % 32];
            let c = &fe[(i + 2) % 32];
            assert_eq!(fe_add(a, b), fe_add(b, a), "add comm");
            let ab = fe_add(a, b);
            let bc = fe_add(b, c);
            assert_eq!(fe_add(&ab, c), fe_add(a, &bc), "add assoc");
            assert_eq!(
                fe_mul(&fe_add(a, b), c),
                fe_add(&fe_mul(a, c), &fe_mul(b, c)),
                "mul distrib"
            );
            assert_eq!(fe_mul(a, b), fe_mul(b, a), "mul comm");
            let negb = fe_sub(&Fe::ZERO, b);
            assert_eq!(fe_sub(a, b), fe_add(a, &negb), "sub == add neg");
            assert_eq!(fe_sub(&fe_add(a, b), b), *a, "a+b-b==a");
            assert_eq!(fe_sub(a, a), Fe::ZERO, "a-a==0");
            let inv2 = fe_inv(b);
            assert_eq!(fe_mul(b, &inv2), Fe::ONE, "inv");
        }
    }
}
