//! 语义化版本（SemVer 2.0.0 子集）解析、比较与约束匹配（no_std、无分配）。
//!
//! 支持：
//! - `MAJOR.MINOR.PATCH[-prerelease][+build]`（build 不参与比较）；
//! - 约束：`1.2.3`（精确）、`=1.2.3`、`>=1.2.3`、`>1.2.3`、`<=1.2.3`、`<1.2.3`、
//!   `^1.2.3`（caret）、`~1.2.3`（tilde）、`1.2.x`/`1.x`（通配），
//!   多个约束用逗号分隔（AND 语义）。

/// 解析后的版本号。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// 预发布标识（点分段原文，比较时逐段比较）。
    pub pre: [u8; 32],
    pub pre_len: usize,
}

impl Version {
    pub const ZERO: Version = Version { major: 0, minor: 0, patch: 0, pre: [0; 32], pre_len: 0 };

    fn has_pre(&self) -> bool {
        self.pre_len > 0
    }

    fn pre_segment(&self, idx: usize) -> &[u8] {
        let mut i = 0usize;
        let mut seg = 0usize;
        while i < self.pre_len {
            let s = i;
            while i < self.pre_len && self.pre[i] != b'.' {
                i += 1;
            }
            if seg == idx {
                return &self.pre[s..i];
            }
            seg += 1;
            if i < self.pre_len {
                i += 1; // 跳过 '.'
            }
        }
        &[]
    }

    fn pre_seg_count(&self) -> usize {
        let mut n = 1usize;
        for i in 0..self.pre_len {
            if self.pre[i] == b'.' {
                n += 1;
            }
        }
        if self.pre_len == 0 {
            0
        } else {
            n
        }
    }
}

fn digits_only(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| c.is_ascii_digit())
}

/// 解析 "x.y.z[-pre][+build]"。宽松：允许省略 patch（"1.2" → 1.2.0）、
/// 允许通配 "x"（"1.x" → major=1，其余 0，标记通配）。返回 None 表示非法。
pub fn parse(s: &[u8]) -> Option<Version> {
    // 去掉 +build
    let core = match s.iter().position(|&c| c == b'+') {
        Some(i) => &s[..i],
        None => s,
    };
    // 拆 pre-release
    let (num, pre) = match core.iter().position(|&c| c == b'-') {
        Some(i) => (&core[..i], &core[i + 1..]),
        None => (core, &[][..]),
    };
    // 无分配拆分数字段
    let mut segs = [&[][..]; 3];
    let mut segn = 0usize;
    let mut start = 0usize;
    for (i, &c) in num.iter().enumerate() {
        if c == b'.' {
            if segn >= 3 {
                return None;
            }
            segs[segn] = &num[start..i];
            segn += 1;
            start = i + 1;
        }
    }
    if segn >= 3 {
        return None;
    }
    segs[segn] = &num[start..];
    segn += 1;

    let mut nums = [0u32; 3];
    for (i, p) in segs[..segn].iter().enumerate() {
        if *p == b"x" || *p == b"X" || *p == b"*" {
            nums[i] = 0;
        } else {
            if !digits_only(p) {
                return None;
            }
            nums[i] = parse_u32(p)?;
        }
    }
    let mut v = Version {
        major: nums[0],
        minor: nums[1],
        patch: nums[2],
        pre: [0; 32],
        pre_len: 0,
    };
    if !pre.is_empty() {
        if pre.len() > v.pre.len() {
            return None;
        }
        v.pre[..pre.len()].copy_from_slice(pre);
        v.pre_len = pre.len();
    }
    Some(v)
}

fn parse_u32(b: &[u8]) -> Option<u32> {
    let mut v: u32 = 0;
    for &c in b {
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(v)
}

/// 逐点比较预发布标识（数字 < 字母数字；短前缀 < 长前缀；无预发布 > 有预发布）。
fn cmp_pre(a: &Version, b: &Version) -> i32 {
    let an = a.pre_seg_count();
    let bn = b.pre_seg_count();
    let n = an.max(bn);
    for i in 0..n {
        let sa = a.pre_segment(i);
        let sb = b.pre_segment(i);
        if sa.is_empty() && sb.is_empty() {
            continue;
        }
        if sa.is_empty() {
            return -1; // b 有标识而 a 没有 → a 更早
        }
        if sb.is_empty() {
            return 1;
        }
        let ad = digits_only(sa);
        let bd = digits_only(sb);
        if ad && bd {
            // 数字按数值比较
            let (mut va, mut vb) = (0u32, 0u32);
            for &c in sa {
                va = va.wrapping_mul(10).wrapping_add((c - b'0') as u32);
            }
            for &c in sb {
                vb = vb.wrapping_mul(10).wrapping_add((c - b'0') as u32);
            }
            if va != vb {
                return if va < vb { -1 } else { 1 };
            }
        } else if ad {
            return -1; // 数字 < 字母数字
        } else if bd {
            return 1;
        } else {
            // 都字母数字：字典序
            let (mut i, mut j) = (0usize, 0usize);
            while i < sa.len() && j < sb.len() {
                if sa[i] != sb[j] {
                    return if sa[i] < sb[j] { -1 } else { 1 };
                }
                i += 1;
                j += 1;
            }
            if i != sa.len() {
                return 1; // a 更长 → a 更大
            }
            if j != sb.len() {
                return -1;
            }
        }
    }
    0
}

/// 比较两个版本：-1 / 0 / 1。
pub fn cmp(a: &Version, b: &Version) -> i32 {
    for (x, y) in [(a.major, b.major), (a.minor, b.minor), (a.patch, b.patch)] {
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    // 预发布：无预发布 > 有预发布
    match (a.has_pre(), b.has_pre()) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => -1,
        (true, true) => cmp_pre(a, b),
    }
}

/// 约束运算符。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Exact,
    Ge,
    Gt,
    Le,
    Lt,
    Caret,
    Tilde,
    Wildcard,
}

/// 单条约束：op + 版本（Wildcard 时版本为通配下界）。
#[derive(Clone, Copy, Debug)]
pub struct Constraint {
    pub op: Op,
    pub ver: Version,
}

/// 解析约束字符串，返回 (约束列表, 数量)（AND）。非法返回 None。
pub fn parse_constraints(s: &[u8]) -> Option<([Constraint; 4], usize)> {
    let mut out = [Constraint { op: Op::Exact, ver: Version::ZERO }; 4];
    let mut n = 0usize;
    for part in s.split(|&c| c == b',') {
        let p = trim(part);
        if p.is_empty() {
            continue;
        }
        if n >= 4 {
            return None;
        }
        let (op, rest) = if let Some(r) = p.strip_prefix(b">=") {
            (Op::Ge, r)
        } else if let Some(r) = p.strip_prefix(b"<=") {
            (Op::Le, r)
        } else if let Some(r) = p.strip_prefix(b">") {
            (Op::Gt, r)
        } else if let Some(r) = p.strip_prefix(b"<") {
            (Op::Lt, r)
        } else if let Some(r) = p.strip_prefix(b"^") {
            (Op::Caret, r)
        } else if let Some(r) = p.strip_prefix(b"~") {
            (Op::Tilde, r)
        } else if let Some(r) = p.strip_prefix(b"=") {
            (Op::Exact, r)
        } else {
            (Op::Exact, p)
        };
        let ver = parse(trim(rest))?;
        // 通配检测：仅末段（最后一个 '.' 之后，无 '.' 则整段）恰为
        // "x"/"X"/"*" 才算通配 —— 此前对整段做 any 检测，预发布版
        // "1.2.3-next"（pre 段含 x）会被误判成通配符，退化为 1.x 全匹配。
        let t = trim(rest);
        let last_seg = match t.iter().rposition(|&c| c == b'.') {
            Some(i) => &t[i + 1..],
            None => t,
        };
        let is_wild = last_seg == b"x" || last_seg == b"X" || last_seg == b"*";
        out[n] = Constraint { op: if is_wild { Op::Wildcard } else { op }, ver };
        n += 1;
    }
    if n == 0 {
        None
    } else {
        Some((out, n))
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t') {
        e -= 1;
    }
    &b[s..e]
}

/// 版本是否满足单条约束。
pub fn match_constraint(v: &Version, c: &Constraint) -> bool {
    match c.op {
        Op::Exact => cmp(v, &c.ver) == 0,
        Op::Ge => cmp(v, &c.ver) >= 0,
        Op::Gt => cmp(v, &c.ver) > 0,
        Op::Le => cmp(v, &c.ver) <= 0,
        Op::Lt => cmp(v, &c.ver) < 0,
        Op::Caret => {
            // ^x.y.z：>= x.y.z 且 < (x+1).0.0（x>0）；x=0 时 < 0.(y+1).0（y>0）
            if c.ver.major > 0 {
                cmp(v, &c.ver) >= 0 && v.major == c.ver.major
            } else if c.ver.minor > 0 {
                cmp(v, &c.ver) >= 0 && v.major == 0 && v.minor == c.ver.minor
            } else {
                cmp(v, &c.ver) >= 0 && v.major == 0 && v.minor == 0
            }
        }
        Op::Tilde => {
            // ~x.y.z：>= x.y.z 且 < x.(y+1).0
            cmp(v, &c.ver) >= 0 && v.major == c.ver.major && v.minor == c.ver.minor
        }
        Op::Wildcard => {
            // "1.x" 或 "1.2.x"：匹配前缀
            if v.major != c.ver.major {
                return false;
            }
            // 解析约束里给出的位数：从原文判断——这里用简化规则：
            // 约束 "1.x"（minor 通配）→ 只要求 major；"1.2.x" → 要求 major+minor。
            // 调用 parse 时通配部分记为 0，无法区分 "1.x" 与 "1.0.x"，因此
            // Wildcard 的语义按「约束版本非零位即要求匹配」处理：
            // "1.x" → major=1, minor=0 → 只查 major；"1.2.x" → minor=2 → 查 major+minor。
            if c.ver.minor != 0 && v.minor != c.ver.minor {
                return false;
            }
            true
        }
    }
}

/// 版本是否满足约束列表（全部 AND）。
pub fn match_all(v: &Version, cs: &[Constraint; 4], n: usize) -> bool {
    for i in 0..n {
        if !match_constraint(v, &cs[i]) {
            return false;
        }
    }
    true
}

/// 把版本格式化为 "x.y.z[-pre]" 写入 out，返回长度。
pub fn fmt(v: &Version, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    n += fmt_u32(v.major, &mut out[n..]);
    out[n] = b'.';
    n += 1;
    n += fmt_u32(v.minor, &mut out[n..]);
    out[n] = b'.';
    n += 1;
    n += fmt_u32(v.patch, &mut out[n..]);
    if v.pre_len > 0 {
        out[n] = b'-';
        n += 1;
        let c = v.pre_len.min(out.len() - n);
        out[n..n + c].copy_from_slice(&v.pre[..c]);
        n += c;
    }
    n
}

fn fmt_u32(mut v: u32, out: &mut [u8]) -> usize {
    if v == 0 {
        if out.is_empty() {
            return 0;
        }
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    let c = n.min(out.len());
    out[..c].copy_from_slice(&tmp[i..i + c]);
    c
}
