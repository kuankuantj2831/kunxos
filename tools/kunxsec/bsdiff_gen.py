#!/usr/bin/env python3
"""KXDELTA1 增量补丁生成器（主机侧）。格式与 kunxsec/src/bspatch.rs 一致。

压缩层：KXRL1（字节式 LZSS）——
  - 0x40..=0x7F  字面量，长度 (op&0x3F)+1
  - 0x80..=0xBF  短拷贝，长度 ((op&0x3C)>>2)+4，距离 ((op&3)<<8|next)+1
  - 0xC0..=0xFF  长拷贝，长度 (op&0x3F)+4，距离 (next2_le)+1
匹配算法：简化贪心（与真实 bsdiff 的 suffix-array 相比不追求最优，可接受）。

用法：
    bsdiff_gen.py --old <old.bin> --new <new.bin> -o <patch.kdelta>
    bsdiff_gen.py --verify --old <old> --patch <patch> --expect-new <new>
"""
import argparse
import os
import sys

MAGIC = b"KXDELTA1"
HEADER = 44
TRIPLE = 24
LEN_MAX = 67
CAND_LIMIT = 16
MATCH_WINDOW = 1 << 20
SHORT_DIST = 1024
LONG_DIST = 65536
FLAG_DIFF = 1 << 0
FLAG_EXTRA = 1 << 1


def kxrl1_compress(data):
    out = bytearray()
    i = 0
    n = len(data)
    while i < n:
        best_len = 0
        best_dist = 1
        max_dist = min(i, LONG_DIST)
        for dist in range(1, max_dist + 1):
            l = 0
            while l < LEN_MAX and i + l < n and data[i + l] == data[i - dist + l]:
                l += 1
            if l > best_len:
                best_len = l
                best_dist = dist
        if best_len >= 4 and best_len <= 19 and best_dist <= SHORT_DIST:
            out.append(0x80 | ((best_len - 4) & 0x0F) << 2 | ((best_dist - 1) >> 8) & 0x03)
            out.append((best_dist - 1) & 0xFF)
            i += best_len
        elif best_len >= 4 and best_dist <= LONG_DIST:
            out.append(0xC0 | (best_len - 4))
            out.append((best_dist - 1) & 0xFF)
            out.append((best_dist - 1) >> 8)
            i += best_len
        else:
            out.append(0x40)
            out.append(data[i])
            i += 1
    return bytes(out)


def kxrl1_inflate(src, size):
    out = bytearray()
    si = 0
    while len(out) < size:
        if si >= len(src):
            return None
        op = src[si]
        si += 1
        if 0x40 <= op <= 0x7F:
            ln = (op & 0x3F) + 1
            if si + ln > len(src) or len(out) + ln > size:
                return None
            out += src[si:si + ln]
            si += ln
        elif 0x80 <= op <= 0xBF:
            ln = ((op & 0x3C) >> 2) + 4
            if si >= len(src):
                return None
            lo = src[si]
            si += 1
            dist = (((op & 0x03) << 8) | lo) + 1
            if dist > len(out) or len(out) + ln > size:
                return None
            for _ in range(ln):
                out.append(out[-dist])
        elif 0xC0 <= op <= 0xFF:
            ln = (op & 0x3F) + 4
            if si + 1 >= len(src):
                return None
            dist = (src[si] | (src[si + 1] << 8)) + 1
            si += 2
            if dist > len(out) or len(out) + ln > size:
                return None
            for _ in range(ln):
                out.append(out[-dist])
        else:
            return None
    return bytes(out)


def _match(old, new, pos, positions, hint=None):
    """在 old 中找 new[pos:] 的最长匹配；返回 (len, index_in_old)。

    为可运行性（不求最优）：candidates 上限与最长匹配窗口有界；
    真正的生产级 bsdiff 用 suffix-array，这里用于演示/测试（小镜像）。
    """
    if pos >= len(new):
        return (0, 0)
    key = bytes(new[pos:pos + 4])
    cand = positions.get(key, [])[:CAND_LIMIT]
    if hint is not None and hint < len(old):
        cand = [hint] + cand
    best = (0, 0)
    for c in cand:
        l = 0
        limit = min(len(old) - c, len(new) - pos, MATCH_WINDOW)
        while l < limit and old[c + l] == new[pos + l]:
            l += 1
        if l > best[0]:
            best = (l, c)
    return best


def make_delta(old, new):
    """返回 KXDELTA1 补丁字节（diff/extra 均用 KXRL1 压缩，压缩更小才启用）。"""
    positions = {}
    if len(old) >= 4:
        for i in range(len(old) - 3):
            key = bytes(old[i:i + 4])
            positions.setdefault(key, []).append(i)

    ctrl = bytearray()
    diff = bytearray()
    extra = bytearray()
    oldpos = 0
    newpos = 0
    while newpos < len(new):
        l, c = _match(old, new, newpos, positions)
        if l >= 4:
            seek = c - oldpos
            if seek > 0:
                # 未对齐：先把不匹配的新字节作为 extra 消费，并把 oldpos 前跳
                # （三元组 (0, e, s)：newpos += e；oldpos += s）
                extra += new[newpos:newpos + seek]
                ctrl += le64_enc(0) + le64_enc(seek) + le64_enc(seek)
                newpos += seek
                oldpos += seek
                continue
            if seek < 0:
                # 向后定位（回看匹配）→ 三元组 (0, 0, s) 只移动 oldpos
                ctrl += le64_enc(0) + le64_enc(0) + le64_enc(seek)
                oldpos += seek
            for k in range(l):
                diff.append((new[newpos + k] - old[oldpos + k]) & 0xFF)
            ctrl += le64_enc(l) + le64_enc(0) + le64_enc(0)
            newpos += l
            oldpos += l
        else:
            # 无匹配：消费一个 extra 字节（oldpos 不动）
            extra.append(new[newpos])
            ctrl += le64_enc(0) + le64_enc(1) + le64_enc(0)
            newpos += 1
    return build_patch_blob(len(new), ctrl, diff, extra)


def build_patch_blob(new_len, ctrl, diff, extra):
    def maybe_compress(block, flag_bit, full_len):
        enc = kxrl1_compress(block)
        if len(enc) < len(block):
            return enc, True, full_len
        return bytes(block), False, full_len

    diff_s, cf_d, diff_full = maybe_compress(diff, FLAG_DIFF, len(diff))
    extra_s, cf_e, extra_full = maybe_compress(extra, FLAG_EXTRA, len(extra))
    flags = (FLAG_DIFF if cf_d else 0) | (FLAG_EXTRA if cf_e else 0)
    hdr = bytearray(HEADER)
    hdr[0:8] = MAGIC
    struct_pack("<I", hdr, 8, 1)
    struct_pack("<I", hdr, 12, new_len)
    struct_pack("<I", hdr, 16, len(ctrl))
    struct_pack("<I", hdr, 20, len(diff_s))
    struct_pack("<I", hdr, 24, len(extra_s))
    struct_pack("<I", hdr, 28, diff_full)
    struct_pack("<I", hdr, 32, extra_full)
    struct_pack("<I", hdr, 36, flags)
    struct_pack("<I", hdr, 40, 0)
    return bytes(hdr) + bytes(ctrl) + diff_s + extra_s


def le64_enc(v):
    # 有符号补码编码（向后 seek 为负值）
    return (v & 0xFFFF_FFFF_FFFF_FFFF).to_bytes(8, "little")


def struct_pack(fmt, buf, off, v):
    buf[off:off + 4] = v.to_bytes(4, "little")


def apply_delta(old, patch):
    """Python 侧应用（与 Rust bspatch 同算法），用于自测。"""
    import struct as st
    if len(patch) < HEADER or patch[0:8] != MAGIC:
        return None
    new_len = st.unpack_from("<I", patch, 12)[0]
    ctrl_len = st.unpack_from("<I", patch, 16)[0]
    diff_stored = st.unpack_from("<I", patch, 20)[0]
    extra_stored = st.unpack_from("<I", patch, 24)[0]
    diff_full = st.unpack_from("<I", patch, 28)[0]
    extra_full = st.unpack_from("<I", patch, 32)[0]
    flags = st.unpack_from("<I", patch, 36)[0]
    off = 44
    ctrl = patch[off:off + ctrl_len]
    off += ctrl_len
    diff_raw = patch[off:off + diff_stored]
    off += diff_stored
    extra_raw = patch[off:off + extra_stored]
    diffb = kxrl1_inflate(diff_raw, diff_full) if (flags & FLAG_DIFF) else diff_raw
    if diffb is None or len(diffb) != diff_full:
        return None
    extrab = kxrl1_inflate(extra_raw, extra_full) if (flags & FLAG_EXTRA) else extra_raw
    if extrab is None or len(extrab) != extra_full:
        return None
    out = bytearray(new_len)
    oldpos = 0
    newpos = 0
    di = 0
    ei = 0
    for i in range(0, len(ctrl), TRIPLE):
        dlen, elen, seek = st.unpack_from("<qqq", ctrl, i)
        for j in range(dlen):
            if oldpos + j < 0 or oldpos + j >= len(old) or di + j >= len(diffb):
                return None
            out[newpos + j] = (old[oldpos + j] + diffb[di + j]) & 0xFF
        newpos += dlen
        oldpos += dlen
        di += dlen
        for j in range(elen):
            if ei + j >= len(extrab):
                return None
            out[newpos + j] = extrab[ei + j]
        newpos += elen
        ei += elen
        oldpos += seek
    if newpos != new_len or di != diff_full or ei != extra_full:
        return None
    return bytes(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--old")
    ap.add_argument("--new")
    ap.add_argument("-o", "--out")
    ap.add_argument("--patch")
    ap.add_argument("--expect-new")
    args = ap.parse_args()

    if args.patch:
        old = open(args.old, "rb").read()
        patch = open(args.patch, "rb").read()
        new = apply_delta(old, patch)
        want = open(args.expect_new, "rb").read() if args.expect_new else None
        ok = new == want if want is not None else new is not None
        print("bsdiff verify: %s (%d -> %d bytes)" % ("PASS" if ok else "FAIL",
                                                       len(old), len(want) if want else -1))
        sys.exit(0 if ok else 1)

    if not (args.old and args.new and args.out):
        ap.error("需 --old --new -o（或 --patch 验证模式）")
    old = open(args.old, "rb").read()
    new = open(args.new, "rb").read()
    patch = make_delta(old, new)
    # 自检
    applied = apply_delta(old, patch)
    if applied is None or applied != new:
        print("bsdiff_gen: FAIL（内部自检不一致）")
        sys.exit(1)
    open(args.out, "wb").write(patch)
    print("bsdiff_gen: wrote %s (%d bytes; old=%d new=%d, patch/old=%.1f%%)" %
          (args.out, len(patch), len(old), len(new), 100.0 * len(patch) / max(len(old), 1)))


if __name__ == "__main__":
    main()