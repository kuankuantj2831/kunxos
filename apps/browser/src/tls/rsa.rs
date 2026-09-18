//! RSA 公钥验签（PKCS#1 v1.5 + SHA-256）—— TLS 证书链与
//! ServerKeyExchange 签名验证。
//!
//! u32 肢体定长数组（上限 4224 位，覆盖 2048/3072/4096 位模数），
//! Montgomery 乘法（CIOS）做模幂；R² mod n 用反复倍增构造（免实现
//! 大数除法）。指数实际是 65537 量级，一次验签约几十次 Mont 乘。
//! 仅验签（公开数据），不做常量时间。

use alloc::vec;
use alloc::vec::Vec;

/// 模数上限：4224 位。
const LIMBS: usize = 132;

/// 定长小端 u32 肢体大数。
#[derive(Clone, Copy)]
struct Ubig {
    n: usize,
    w: [u32; LIMBS],
}

impl Ubig {
    fn zero() -> Ubig {
        Ubig { n: 0, w: [0; LIMBS] }
    }

    /// 从大端字节（无前导 0 要求）导入。
    fn from_be(bytes: &[u8]) -> Option<Ubig> {
        if bytes.is_empty() || bytes.len() > LIMBS * 4 {
            return None;
        }
        let mut r = Ubig::zero();
        // 每字节自高位向低位：整体左移 8 位加新字节。
        // 注意：乘 256 是对全部肢体的移位，即使进位为 0 也必须继续
        // 处理更高肢体（此处不能因 carry==0 提前 break）。
        for &b in bytes {
            // r = r * 256 + b
            let mut carry = b as u32;
            for i in 0..r.n {
                let v = (r.w[i] as u64) * 256 + carry as u64;
                r.w[i] = v as u32;
                carry = (v >> 32) as u32;
            }
            if carry > 0 {
                if r.n >= LIMBS {
                    return None;
                }
                r.w[r.n] = carry;
                r.n += 1;
            }
            // 注意：左移 8 位后可能最高字变 0 吗？乘 256 只进位不缩位，n 不缩
        }
        while r.n > 0 && r.w[r.n - 1] == 0 {
            r.n -= 1;
        }
        Some(r)
    }

    fn bits(&self) -> usize {
        if self.n == 0 {
            return 0;
        }
        let top = self.w[self.n - 1];
        (self.n - 1) * 32 + (32 - top.leading_zeros() as usize)
    }

    fn norm(&mut self) {
        while self.n > 0 && self.w[self.n - 1] == 0 {
            self.n -= 1;
        }
    }
}

/// RSA 公钥。
pub struct RsaPub {
    n: Ubig,
    e: Ubig,
    /// n0' = −n^(−1) mod 2^32。
    n0inv: u32,
    /// R mod n（R = 2^(32·s)，s = n 肢体数；Montgomery 域的「1」）。
    r_mod_n: Ubig,
    /// R² mod n（入 Montgomery 域用）。
    r2_mod_n: Ubig,
}

/// 重算有效肢体数（r.n 之外的字节恒为 0：初始化清零、且只有
/// double_mod / sub 写入真实值）。norm 只会收缩、不会扩展，倍增把值
/// 推进新肢体时（如 0x80000000 翻转到 w[1]=1）必须重扫，否则值被
/// 修剪成 0。
fn renorm(r: &mut Ubig) {
    r.n = LIMBS;
    while r.n > 0 && r.w[r.n - 1] == 0 {
        r.n -= 1;
    }
}

/// r = 2r mod n。进位越过最高肢体时作为「虚拟第 LIMBS 位」吸收减 n 的
/// 顶位借位（2r < 2n，减一次必够）。
fn double_mod(r: &mut Ubig, n: &Ubig) {
    let mut carry: u32 = 0;
    for i in 0..LIMBS {
        let v = (r.w[i] as u64) * 2 + carry as u64;
        r.w[i] = v as u32;
        carry = (v >> 32) as u32;
    }
    renorm(r);
    let needs_sub = carry > 0 || ge(r, n);
    if needs_sub {
        let mut borrow: i64 = 0;
        for i in 0..LIMBS {
            let b = if i < n.n { n.w[i] } else { 0 };
            let s = r.w[i] as i64 - b as i64 - borrow;
            if s < 0 {
                r.w[i] = (s + (1 << 32)) as u32;
                borrow = 1;
            } else {
                r.w[i] = s as u32;
                borrow = 0;
            }
        }
        // 顶位借位必须被虚拟进位位吸收（值 ≥ n 保证）
        debug_assert!(carry as i64 >= borrow);
        let _ = borrow;
    }
    renorm(r);
}

impl RsaPub {
    /// 调试用：返回 (n 肢体数, n 位数, n 顶肢体)。
    pub fn debug_n(&self) -> (usize, usize, u32) {
        (self.n.n, self.n.bits(), self.n.w[self.n.n - 1])
    }

    /// 调试用：返回 (R mod n, R² mod n) 的大端字节。
    pub fn debug_rr2(&self) -> (Vec<u8>, Vec<u8>) {
        let to_be = |u: &Ubig, nlen: usize| {
            let mut out = vec![0u8; nlen];
            for i in 0..u.n {
                let w = u.w[i].to_le_bytes();
                for j in 0..4 {
                    let pos = i * 4 + j;
                    if pos < nlen {
                        out[nlen - 1 - pos] = w[j];
                    }
                }
            }
            out
        };
        let nlen = (self.n.bits() + 7) / 8;
        (to_be(&self.r_mod_n, nlen), to_be(&self.r2_mod_n, nlen))
    }

    /// 调试用：返回 mul_mont(a, b) 的大端字节。
    pub fn debug_mul_mont(&self, a_be: &[u8], b_be: &[u8]) -> Option<Vec<u8>> {
        let a = Ubig::from_be(a_be)?;
        let b = Ubig::from_be(b_be)?;
        let r = self.mul_mont(&a, &b);
        let nlen = (self.n.bits() + 7) / 8;
        let mut out = vec![0u8; nlen];
        for i in 0..r.n {
            let w = r.w[i].to_le_bytes();
            for j in 0..4 {
                let pos = i * 4 + j;
                if pos < nlen {
                    out[nlen - 1 - pos] = w[j];
                }
            }
        }
        Some(out)
    }

    /// `modulus_be` / `exp_be`：DER INTEGER 内容（大端，无符号）。
    pub fn parse(modulus_be: &[u8], exp_be: &[u8]) -> Option<RsaPub> {
        let n = Ubig::from_be(modulus_be)?;
        let e = Ubig::from_be(exp_be)?;
        if n.n == 0 || e.n == 0 || n.w[0] & 1 == 0 {
            return None; // Montgomery 要求奇模数
        }
        // n0' = −n^{-1} mod 2^32（Newton 迭代：x ← x(2 − n·x) mod 2^32）
        let mut inv: u32 = 1; // 初值 1 位正确，5 轮到 32 位
        for _ in 0..5 {
            inv = inv.wrapping_mul(2u32.wrapping_sub(n.w[0].wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();

        // R mod n 与 R² mod n：反复倍增（R = 2^(32·s)，s = n 肢体数，
        // 与 mul_mont 内的 R 定义一致）
        let mut r = Ubig::zero();
        r.w[0] = 1;
        r.n = 1;
        for _ in 0..(32 * n.n) {
            double_mod(&mut r, &n);
        }
        r.norm();
        let mut r2 = r;
        for _ in 0..(32 * n.n) {
            double_mod(&mut r2, &n);
        }
        r2.norm();
        Some(RsaPub { n, e, n0inv, r_mod_n: r, r2_mod_n: r2 })
    }

    /// Montgomery 乘：返回 (a·b / R) mod n。要求 a、b < n。
    ///
    /// 采用「全加 + 整体右移」两步式（不用 CIOS 的字内截断技巧）：
    /// 每轮 t += a·b_i；m = t[0]·n0'（使 t[0] + m·n[0] ≡ 0 mod 2^32）；
    /// t += m·n；t[0] 必为 0 → 整体右移一个肢体。中间值 < 3R^s，
    /// 预留 3 个多余肢体不丢进位。
    fn mul_mont(&self, a: &Ubig, b: &Ubig) -> Ubig {
        let s = self.n.n;
        let mut t = [0u32; LIMBS + 3];
        for i in 0..s {
            // t += a·b[i]
            let mut carry: u64 = 0;
            for j in 0..s {
                let v = t[j] as u64 + a.w[j] as u64 * b.w[i] as u64 + carry;
                t[j] = v as u32;
                carry = v >> 32;
            }
            let mut k = s;
            while carry > 0 {
                let v = t[k] as u64 + carry;
                t[k] = v as u32;
                carry = v >> 32;
                k += 1;
            }
            // t += m·n（m 使和的最低字为 0）
            let m = t[0].wrapping_mul(self.n0inv);
            let mut carry: u64 = t[0] as u64 + m as u64 * self.n.w[0] as u64;
            t[0] = carry as u32; // 按构造 == 0
            carry >>= 32;
            for j in 1..s {
                let v = t[j] as u64 + m as u64 * self.n.w[j] as u64 + carry;
                t[j] = v as u32;
                carry = v >> 32;
            }
            let mut k = s;
            while carry > 0 {
                let v = t[k] as u64 + carry;
                t[k] = v as u32;
                carry = v >> 32;
                k += 1;
            }
            // t[0] == 0 → 右移一个肢体
            for j in 1..t.len() {
                t[j - 1] = t[j];
            }
            t[t.len() - 1] = 0;
        }
        // 收尾：中间值 < 3R^s，可能超出 s 个肢体（t[s..] 非零）——
        // 全宽拷入后循环减 n（值 ≡ ab/R^s (mod n)，减至 < n 为止）。
        let mut r = Ubig::zero();
        let len = t.len().min(LIMBS);
        r.w[..len].copy_from_slice(&t[..len]);
        renorm(&mut r);
        let mut guard = 0;
        while ge(&r, &self.n) {
            r = sub(&r, &self.n);
            guard += 1;
            if guard > 8 {
                return Ubig::zero(); // 不可达；防御死循环
            }
        }
        r
    }

    /// a^e mod n（Montgomery 域内平方-乘，MSB→LSB）。
    fn modexp(&self, a: &Ubig) -> Ubig {
        // 入域：a·R mod n = mul_mont(a, R²)
        let mut m = self.mul_mont(a, &self.r2_mod_n);
        // 结果寄存器：1 的 Montgomery 表示 = R mod n
        let mut r = self.r_mod_n;
        let ebits = self.e.bits();
        for i in (0..ebits).rev() {
            r = self.mul_mont(&r, &r);
            if (self.e.w[i / 32] >> (i % 32)) & 1 == 1 {
                r = self.mul_mont(&r, &m);
            }
        }
        // 出域：·R^{-1} = mul_mont(r, 1)
        let one = Ubig { n: 1, w: { let mut w = [0u32; LIMBS]; w[0] = 1; w } };
        m = self.mul_mont(&r, &one);
        m
    }

    /// 计算签名的 s^e mod n 并按大端返回定长 em 字节（调试/验签共用）。
    pub fn modexp_em(&self, sig_be: &[u8]) -> Option<Vec<u8>> {
        let s = match Ubig::from_be(sig_be) {
            Some(s) if !ge(&s, &self.n) => s,
            _ => return None,
        };
        let em = self.modexp(&s);
        let nlen = (self.n.bits() + 7) / 8;
        let mut em_bytes = vec![0u8; nlen];
        for i in 0..em.n {
            let w = em.w[i].to_le_bytes();
            for j in 0..4 {
                let pos = i * 4 + j;
                if pos < nlen {
                    em_bytes[nlen - 1 - pos] = w[j];
                }
            }
        }
        Some(em_bytes)
    }

    /// PKCS#1 v1.5 + SHA-256 验签。`sig_be` 为签名（大端），`hash` 为消息摘要。
    pub fn verify_pkcs1_sha256(&self, hash: &[u8; 32], sig_be: &[u8]) -> bool {
        let em_bytes = match self.modexp_em(sig_be) {
            Some(e) => e,
            None => return false,
        };
        if self.e.w[0] < 3 {
            return false;
        }
        let nlen = em_bytes.len();
        // EMSA-PKCS1-v1_5：0x00 0x01 FF..FF 00 ‖ DigestInfo(SHA-256) ‖ hash
        const DIGEST_INFO: &[u8] = &[
            0x30, 0x31, 0x30, 0x0D, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ];
        let ps_len = nlen as isize - 3 - (DIGEST_INFO.len() + 32) as isize;
        if ps_len < 8 {
            return false;
        }
        let mut idx = 0usize;
        if em_bytes[idx] != 0x00 {
            return false;
        }
        idx += 1;
        if em_bytes[idx] != 0x01 {
            return false;
        }
        idx += 1;
        for _ in 0..ps_len {
            if em_bytes[idx] != 0xFF {
                return false;
            }
            idx += 1;
        }
        if em_bytes[idx] != 0x00 {
            return false;
        }
        idx += 1;
        if em_bytes[idx..].len() < DIGEST_INFO.len() + 32 {
            return false;
        }
        if &em_bytes[idx..idx + DIGEST_INFO.len()] != DIGEST_INFO {
            return false;
        }
        idx += DIGEST_INFO.len();
        let mut d = 0u8;
        for i in 0..32 {
            d |= em_bytes[idx + i] ^ hash[i];
        }
        d == 0
    }
}

fn ge(a: &Ubig, b: &Ubig) -> bool {
    if a.n != b.n {
        return a.n > b.n;
    }
    for i in (0..a.n).rev() {
        if a.w[i] != b.w[i] {
            return a.w[i] > b.w[i];
        }
    }
    true
}

/// a − b（要求 a ≥ b）。
fn sub(a: &Ubig, b: &Ubig) -> Ubig {
    let mut r = *a;
    let mut borrow: i64 = 0;
    for i in 0..a.n {
        let s = a.w[i] as i64 - b.w.get(i).copied().unwrap_or(0) as i64 - borrow;
        if s < 0 {
            r.w[i] = (s + (1 << 32)) as u32;
            borrow = 1;
        } else {
            r.w[i] = s as u32;
            borrow = 0;
        }
    }
    r.norm();
    r
}
