//! KXDELTA v1：增量补丁应用（BSDIFF40 结构自研 + KXRL1 轻量压缩）。
//!
//! 结构保持 bsdiff 经典三元组语义：新文件 = 旧文件对应位加 diff + 直拷 extra +
//! seek 重定位。压缩层用自研 KXRL1（LZSS 变体）替代 bsdiff 的 bzip2，使
//! no_std/零依赖链可自包含实现。
//!
//! ```text
//! @0   magic[8]  = "KXDELTA1"
//! @8   format u32 = 1
//! @12  new_len u32      （产出镜像大小）
//! @16  ctrl_len u32     （控制块在包内占用字节数）
//! @20  diff_stored u32  （diff 块在包内占用字节数；压缩后=压缩流长度）
//! @24  extra_stored u32 （extra 块在包内占用字节数）
//! @28  diff_full u32    （diff 块解码后字节数；flag0 未压缩时 == diff_stored）
//! @32  extra_full u32   （extra 块解码后字节数）
//! @36  flags u32        （bit0 = diff 块 KXRL1 压缩, bit1 = extra 块 KXRL1 压缩）
//! @40  reserved u32 = 0
//! @44  ctrl[...]       每 24 字节一个三元组（LE i64）：diff_len / extra_len / seek
//!      diff[...stored]  extra[...stored]
//! ```
//!
//! KXRL1 字节编码（简单 LZSS）：
//! - `0x40..=0x7F`：字面量，长度 = (op&0x3F)+1 字节紧随其后；
//! - `0x80..=0xBF`：短拷贝，长度 = ((op&0x3C)>>2)+4（4..19），
//!   距离 = ((op&0x03)<<8 | 下一字节)+1（1..1024）；
//! - `0xC0..=0xFF`：长拷贝，长度 = (op&0x3F)+4（4..67），
//!   距离 = 下一 2 字节小端+1（1..65536）；
//! - `0x00..=0x3F`：结束/保留。

use crate::le_u32;
use crate::le_u64;

pub const MAGIC: &[u8; 8] = b"KXDELTA1";
pub const HEADER_SIZE: usize = 44;
pub const TRIPLE_SIZE: usize = 24;
/// KXRL1 距离/长度上限。
pub const KXRL1_MAX_SHORT_DIST: usize = 1024;
pub const KXRL1_MAX_LONG_DIST: usize = 65536;
pub const KXRL1_MAX_LEN: usize = 67;

const FLAG_DIFF_KXRL1: u32 = 1 << 0;
const FLAG_EXTRA_KXRL1: u32 = 1 << 1;

/// 解析补丁头；失败返回 None。任何越界/不对齐/不铺满都被拒绝。
pub fn parse(blob: &[u8]) -> Option<Patch> {
    if blob.len() < HEADER_SIZE || &blob[0..8] != MAGIC {
        return None;
    }
    if le_u32(&blob[8..12]) != 1 {
        return None;
    }
    let new_len = le_u32(&blob[12..16]) as usize;
    let ctrl_len = le_u32(&blob[16..20]) as usize;
    let diff_stored = le_u32(&blob[20..24]) as usize;
    let extra_stored = le_u32(&blob[24..28]) as usize;
    let diff_full = le_u32(&blob[28..32]) as usize;
    let extra_full = le_u32(&blob[32..36]) as usize;
    let flags = le_u32(&blob[36..40]);
    if ctrl_len % TRIPLE_SIZE != 0 {
        return None;
    }
    let ctrl_off = HEADER_SIZE;
    let Some(diff_off) = ctrl_off.checked_add(ctrl_len) else { return None };
    let Some(extra_off) = diff_off.checked_add(diff_stored) else { return None };
    let Some(end) = extra_off.checked_add(extra_stored) else { return None };
    if end != blob.len() {
        return None;
    }
    // 未压缩时 full 必须与 stored 一致
    if flags & FLAG_DIFF_KXRL1 == 0 && diff_full != diff_stored {
        return None;
    }
    if flags & FLAG_EXTRA_KXRL1 == 0 && extra_full != extra_stored {
        return None;
    }
    Some(Patch {
        new_len,
        ctrl_len,
        diff_stored,
        extra_stored,
        diff_full,
        extra_full,
        flags,
        ctrl_off,
        diff_off,
        extra_off,
    })
}

/// 已解析补丁视图。
#[derive(Clone, Copy, Debug)]
pub struct Patch {
    pub new_len: usize,
    pub ctrl_len: usize,
    /// diff 块在包内占用字节数。
    pub diff_stored: usize,
    pub extra_stored: usize,
    /// 解码后字节数。
    pub diff_full: usize,
    pub extra_full: usize,
    pub flags: u32,
    pub ctrl_off: usize,
    pub diff_off: usize,
    pub extra_off: usize,
}

/// KXRL1 解码：把一个压缩块解到 `out`，返回写入字节数；耗尽/越界返回 None。
pub fn inflate_kxrl1(src: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut si = 0usize;
    let mut oi = 0usize;
    loop {
        if oi >= out.len() {
            break;
        }
        let op = *src.get(si)?;
        si += 1;
        match op {
            0x40..=0x7F => {
                let n = (op & 0x3F) as usize + 1;
                if oi + n > out.len() {
                    return None;
                }
                if si + n > src.len() {
                    return None;
                }
                out[oi..oi + n].copy_from_slice(&src[si..si + n]);
                oi += n;
                si += n;
            }
            0x80..=0xBF => {
                let len = ((op & 0x3C) >> 2) as usize + 4;
                let hi = (op & 0x03) as usize;
                let lo = *src.get(si)? as usize;
                si += 1;
                let dist = ((hi << 8) | lo) + 1;
                if dist > oi || oi + len > out.len() {
                    return None;
                }
                for _ in 0..len {
                    let b = out[oi - dist];
                    out[oi] = b;
                    oi += 1;
                }
            }
            0xC0..=0xFF => {
                let len = (op & 0x3F) as usize + 4;
                let b0 = *src.get(si)? as usize;
                let b1 = *src.get(si + 1)? as usize;
                si += 2;
                let dist = (b0 | (b1 << 8)) + 1;
                if dist > oi || oi + len > out.len() {
                    return None;
                }
                for _ in 0..len {
                    let b = out[oi - dist];
                    out[oi] = b;
                    oi += 1;
                }
            }
            _ => return None, // 0x00..=0x3F：结束/保留/非法
        }
    }
    Some(oi)
}

/// 应用 KXDELTA 补丁到 `old`，输出写入 `out`（容量须 ≥ 补丁声明的 new_len）。
///
/// `no_std`：不分配内存；`diff_scratch` / `extra_scratch` 由调用方提供，
/// 用于 KXRL1 解压（容量须 ≥ 头部声明的 diff_full / extra_full）。
/// 返回产出字节数（== new_len）；任何越界/结构错误返回 None。
pub fn apply_patch(
    old: &[u8],
    patch_blob: &[u8],
    diff_scratch: &mut [u8],
    extra_scratch: &mut [u8],
    out: &mut [u8],
) -> Option<usize> {
    let p = parse(patch_blob)?;
    if p.new_len > out.len() {
        return None;
    }
    // 控制块：三元组总数必须正好支撑产出（由遍历时消费校验）
    let ctrl = &patch_blob[p.ctrl_off..p.ctrl_off + p.ctrl_len];

    // diff 块解码（解压后必须 == diff_full）
    let db: &[u8] = if p.flags & FLAG_DIFF_KXRL1 != 0 {
        if diff_scratch.len() < p.diff_full {
            return None;
        }
        let n = inflate_kxrl1(&patch_blob[p.diff_off..p.diff_off + p.diff_stored], diff_scratch)?;
        if n != p.diff_full {
            return None;
        }
        &diff_scratch[..n]
    } else {
        &patch_blob[p.diff_off..p.diff_off + p.diff_stored]
    };
    // extra 块解码
    let extra: &[u8] = if p.flags & FLAG_EXTRA_KXRL1 != 0 {
        if extra_scratch.len() < p.extra_full {
            return None;
        }
        let n = inflate_kxrl1(
            &patch_blob[p.extra_off..p.extra_off + p.extra_stored],
            extra_scratch,
        )?;
        if n != p.extra_full {
            return None;
        }
        &extra_scratch[..n]
    } else {
        &patch_blob[p.extra_off..p.extra_off + p.extra_stored]
    };

    let mut oldpos: i64 = 0;
    let mut newpos: i64 = 0;
    let mut di = 0usize; // diff 消费位置（按三元组累计）
    let mut ei = 0usize;
    let mut ctrl_left = p.ctrl_len;
    while (newpos as usize) < p.new_len {
        if ctrl_left == 0 {
            return None;
        }
        let idx = (ctrl.len() - ctrl_left) / TRIPLE_SIZE;
        let c = &ctrl[idx * TRIPLE_SIZE..(idx + 1) * TRIPLE_SIZE];
        ctrl_left -= TRIPLE_SIZE;
        let dlen = le_u64(&c[0..8]);
        let elen = le_u64(&c[8..16]);
        let seek = le_u64(&c[16..24]);
        if dlen > i64::MAX as u64 || elen > i64::MAX as u64 {
            return None;
        }
        // diff 应用：out = old[oldpos+i] + diff[di+i]
        for i in 0..(dlen as usize) {
            let oi = (oldpos as i128) + i as i128;
            if oi < 0 || oi >= old.len() as i128 {
                return None;
            }
            let ni = (newpos as i128) + i as i128;
            if ni < 0 || ni >= p.new_len as i128 {
                return None;
            }
            if di + i >= db.len() {
                return None;
            }
            let v = old[oi as usize] as i32 + db[di + i] as i32;
            out[ni as usize] = v as u8;
        }
        newpos = newpos.checked_add(dlen as i64)?;
        oldpos = oldpos.checked_add(dlen as i64)?;
        di += dlen as usize;
        // extra 直拷
        for i in 0..(elen as usize) {
            let ni = (newpos as i128) + i as i128;
            if ni < 0 || ni >= p.new_len as i128 {
                return None;
            }
            if ei + i >= extra.len() {
                return None;
            }
            out[ni as usize] = extra[ei + i];
        }
        newpos = newpos.checked_add(elen as i64)?;
        ei += elen as usize;
        oldpos = oldpos.checked_add(seek as i64)?;
    }
    if (newpos as usize) != p.new_len || di != p.diff_full || ei != p.extra_full {
        return None;
    }
    Some(p.new_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// 测试用 KXRL1 编码器（与主机 tools/kunxsec/bsdiff_gen.py 同构，仅测试）。
    fn encode_kxrl1(data: &[u8], out: &mut Vec<u8>) {
        let mut i = 0usize;
        while i < data.len() {
            // 贪心：寻找最长回看匹配
            let mut best_len = 0usize;
            let mut best_dist = 1usize;
            let max_dist = core::cmp::min(i, KXRL1_MAX_LONG_DIST);
            for dist in 1..=max_dist {
                let mut l = 0usize;
                while l < KXRL1_MAX_LEN && i + l < data.len() && data[i + l] == data[i - dist + l] {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_dist = dist;
                }
            }
            if best_len >= 4 && best_len <= 19 && best_dist <= KXRL1_MAX_SHORT_DIST {
                // 短拷贝：长度域 4 位（len-4 ∈ 0..15）
                let len = (best_len - 4) as u8;
                let dhi = ((best_dist - 1) >> 8) as u8;
                out.push(0x80 | ((len & 0x0F) << 2) | (dhi & 0x03));
                out.push(((best_dist - 1) & 0xFF) as u8);
                i += best_len;
            } else if best_len >= 4 && best_dist <= KXRL1_MAX_LONG_DIST {
                out.push(0xC0 | (best_len - 4) as u8);
                out.push(((best_dist - 1) & 0xFF) as u8);
                out.push(((best_dist - 1) >> 8) as u8);
                i += best_len;
            } else {
                // 字面量（单字节）
                out.push(0x40);
                out.push(data[i]);
                i += 1;
            }
        }
    }

    /// 构造一个 KXDELTA 补丁（diff 块可压缩/不压缩），用于逻辑测试。
    fn build_patch(old: &[u8], new: &[u8], compress_diff: bool) -> Vec<u8> {
        let dlen = core::cmp::min(old.len(), new.len());
        let elen = if new.len() > old.len() { new.len() - old.len() } else { 0 };
        let diff: Vec<u8> = new[..dlen]
            .iter()
            .enumerate()
            .map(|(i, &b)| b.wrapping_sub(old[i]))
            .collect();
        let extra = &new[dlen..];
        let mut ctrl = Vec::new();
        ctrl.extend_from_slice(&(dlen as u64).to_le_bytes());
        ctrl.extend_from_slice(&(elen as u64).to_le_bytes());
        ctrl.extend_from_slice(&0u64.to_le_bytes());

        let mut stored = diff.clone();
        let mut flags = 0u32;
        if compress_diff {
            stored.clear();
            encode_kxrl1(&diff, &mut stored);
            flags |= FLAG_DIFF_KXRL1;
        }

        let mut p = Vec::new();
        p.extend_from_slice(MAGIC);
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&(new.len() as u32).to_le_bytes());
        p.extend_from_slice(&(ctrl.len() as u32).to_le_bytes());
        p.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        p.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        p.extend_from_slice(&(diff.len() as u32).to_le_bytes());
        p.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        p.extend_from_slice(&flags.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&ctrl);
        p.extend_from_slice(&stored);
        p.extend_from_slice(extra);
        p
    }

    #[test]
    fn apply_simple_patch() {
        let old = b"hello, kunxos build 10 kernel!";
        let new = b"hello, kunxos build 42 kernel with more text!!";
        let patch = build_patch(old, new, false);
        let mut out = [0u8; 256];
        let mut ds = [0u8; 256];
        let mut es = [0u8; 256];
        let n = apply_patch(old, &patch, &mut ds, &mut es, &mut out).expect("apply");
        assert_eq!(&out[..n], new, "reconstructed new image must match");
        // 越界校验
        let mut small = [0u8; 8];
        assert!(apply_patch(old, &patch, &mut ds, &mut es, &mut small).is_none());
        // 篡改补丁
        let mut bad = patch.clone();
        bad[13] ^= 1; // new_len 字段
        assert!(apply_patch(old, &bad, &mut ds, &mut es, &mut out).is_none());
    }

    #[test]
    fn kxrl1_roundtrip() {
        let data: Vec<u8> = (0..300u32).map(|i| ((i * 7 + i / 3) % 251) as u8).collect();
        let mut enc = Vec::new();
        encode_kxrl1(&data, &mut enc);
        let mut dec = Vec::new();
        dec.resize(data.len(), 0);
        let n = inflate_kxrl1(&enc, &mut dec).expect("inflate");
        assert_eq!(n, data.len());
        assert_eq!(&dec[..n], &data[..]);
    }

    #[test]
    fn kxrl1_compressed_patch() {
        // 构造"高度相似"的两镜像（周期 12 的重复块 + 每 10 位翻转），用 KXRL1 压缩 diff
        let old: Vec<u8> = (0..100).flat_map(|r| vec![0x41 + (r % 3) as u8; 4]).collect();
        let mut new = old.clone();
        for i in (0..new.len()).step_by(10) {
            new[i] ^= 0x80;
        }
        let patch = build_patch(&old, &new, true);
        let mut out = vec![0u8; new.len()];
        let mut ds = vec![0u8; new.len()];
        let mut es = [0u8; 0];
        let n = apply_patch(&old, &patch, &mut ds, &mut es, &mut out).expect("apply compressed");
        assert_eq!(&out[..n], &new[..], "apply compressed diff");
        // 压缩确实缩小
        let p = parse(&patch).expect("parse");
        assert!(p.diff_stored < p.diff_full);
        // 等长输出：非压缩路径也成立
        let patch2 = build_patch(&old, &new, false);
        let mut out2 = vec![0u8; new.len()];
        let n2 = apply_patch(&old, &patch2, &mut ds, &mut es, &mut out2).expect("apply plain");
        assert_eq!(&out2[..n2], &new[..]);
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse(b"KXDELTA1!").is_none());
        assert!(parse(&[0u8; 44]).is_none());
        let mut p = build_patch(b"abc", b"abcd", false);
        p[16..20].copy_from_slice(&7u32.to_le_bytes()); // ctrl_len 非 24 倍数
        assert!(parse(&p).is_none());
        // 三块不铺满
        let mut p = build_patch(b"abc", b"abcd", false);
        p.push(0);
        assert!(parse(&p).is_none());
        // 未压缩标志但 full != stored → 拒绝
        let mut p = build_patch(b"abc", b"abcd", false);
        p[28..32].copy_from_slice(&999u32.to_le_bytes());
        assert!(parse(&p).is_none());
    }
}