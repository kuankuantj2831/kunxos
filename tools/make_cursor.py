#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 KunxOS 系统鼠标指针贴图。

把 tubiao/shubiao.png（256x256 RGBA）面积平均缩放为 24x24 RGBA8888，
写入 shared/cursor.rgba（裸像素，R,G,B,A 各 1 字节/通道，行主序），
由 shared/cursor_sprite.rs `include_bytes!` 编译期内嵌进各 GUI 应用。

仅用 Python 标准库（zlib 解 PNG IDAT），无第三方依赖。
热点取源图指针尖端（不透明像素包围盒左上角）的等比映射；若与
shared/cursor_sprite.rs 中的 CUR_HOT_X/CUR_HOT_Y 不一致会报错提示同步。

用法：python3 tools/make_cursor.py
"""

import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "tubiao" / "shubiao.png"
DST = ROOT / "shared" / "cursor.rgba"

DST_SIZE = 24  # 必须与 shared/cursor_sprite.rs 的 CUR_W/CUR_H 一致
# 必须与 shared/cursor_sprite.rs 的 CUR_HOT_X/CUR_HOT_Y 一致
EXPECT_HOT = (5, 2)

THRESHOLD = 50  # 热点检测的不透明阈值（alpha > 该值视为实心）


def parse_png(data):
    """解析 PNG（bit depth 8、非隔行、颜色类型 2/3/6）→ (w, h, RGBA bytes)。"""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"不是 PNG 文件：{SRC}")
    pos = 8
    w = h = 0
    color_type = 0
    palette = bytearray()
    trns = bytearray()
    idat = bytearray()
    while pos + 8 <= len(data):
        (length,) = struct.unpack_from(">I", data, pos)
        ctype = data[pos + 4 : pos + 8]
        body = pos + 8
        if body + length > len(data):
            raise SystemExit("PNG chunk 越界")
        chunk = data[body : body + length]
        if ctype == b"IHDR":
            w, h, bit_depth, color_type, _, _, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
            if bit_depth != 8 or interlace != 0:
                raise SystemExit("仅支持 8bit 非隔行 PNG")
            if color_type not in (2, 3, 6):
                raise SystemExit(f"不支持的颜色类型 {color_type}")
        elif ctype == b"PLTE":
            palette += chunk
        elif ctype == b"tRNS":
            trns += chunk
        elif ctype == b"IDAT":
            idat += chunk
        elif ctype == b"IEND":
            break
        pos = body + length + 4
    if not w or not h:
        raise SystemExit("PNG 缺少 IHDR")

    raw = zlib.decompress(bytes(idat))
    bpp = {2: 3, 3: 1, 6: 4}[color_type]
    stride = w * bpp
    if len(raw) < h * (stride + 1):
        raise SystemExit("PNG 像素数据不完整")

    # 反滤波（与 shared/emoji.rs decode_png 相同的 Paeth 算法）
    out = bytearray(h * stride)
    for y in range(h):
        ft = raw[y * (stride + 1)]
        row_in = raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)]
        base = y * stride
        for x in range(stride):
            rv = row_in[x]
            left = out[base + x - bpp] if x >= bpp else 0
            up = out[base - stride + x] if y else 0
            ul = out[base - stride + x - bpp] if (y and x >= bpp) else 0
            if ft == 0:
                v = rv
            elif ft == 1:
                v = (rv + left) & 0xFF
            elif ft == 2:
                v = (rv + up) & 0xFF
            elif ft == 3:
                v = (rv + ((left + up) >> 1)) & 0xFF
            elif ft == 4:
                p = left + up - ul
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - ul)
                pred = left if (pa <= pb and pa <= pc) else (up if pb <= pc else ul)
                v = (rv + pred) & 0xFF
            else:
                raise SystemExit(f"未知滤波类型 {ft}")
            out[base + x] = v

    # → RGBA8888
    rgba = bytearray(w * h * 4)
    if color_type == 6:
        rgba[:] = out
    elif color_type == 2:
        for i in range(w * h):
            rgba[i * 4 : i * 4 + 3] = out[i * 3 : i * 3 + 3]
            rgba[i * 4 + 3] = 255
    else:  # 调色板
        for i in range(w * h):
            idx = out[i]
            rgba[i * 4 : i * 4 + 3] = palette[idx * 3 : idx * 3 + 3]
            rgba[i * 4 + 3] = trns[idx] if idx < len(trns) else 255
    return w, h, bytes(rgba)


def find_tip(w, h, rgba):
    """指针尖端：不透明像素包围盒的左上角（箭头尖）。"""
    minx, miny = w, h
    for y in range(h):
        for x in range(w):
            if rgba[(y * w + x) * 4 + 3] > THRESHOLD:
                minx = min(minx, x)
                miny = min(miny, y)
    if minx == w:
        raise SystemExit("整张贴图全透明，找不到指针尖端")
    return minx, miny


def downscale(w, h, rgba, size):
    """面积平均缩放（预乘 alpha，避免透明区暗色边缘）。"""
    out = bytearray(size * size * 4)
    for dy in range(size):
        sy0, sy1 = dy * h / size, (dy + 1) * h / size
        for dx in range(size):
            sx0, sx1 = dx * w / size, (dx + 1) * w / size
            sr = sg = sb = sa = wsum = 0.0
            for y in range(max(0, int(sy0)), min(h, int(sy1) + 1)):
                ky = min(y + 1, sy1) - max(y, sy0)
                if ky <= 0:
                    continue
                for x in range(max(0, int(sx0)), min(w, int(sx1) + 1)):
                    kx = min(x + 1, sx1) - max(x, sx0)
                    if kx <= 0:
                        continue
                    k = kx * ky
                    o = (y * w + x) * 4
                    a = rgba[o + 3]
                    sr += rgba[o] * a * k  # 预乘后累加
                    sg += rgba[o + 1] * a * k
                    sb += rgba[o + 2] * a * k
                    sa += a * k
                    wsum += k
            o = (dy * size + dx) * 4
            if wsum > 0 and sa > 0:
                a = sa / wsum
                out[o] = min(255, round(sr / wsum * 255 / a))
                out[o + 1] = min(255, round(sg / wsum * 255 / a))
                out[o + 2] = min(255, round(sb / wsum * 255 / a))
                out[o + 3] = min(255, round(a))
    return bytes(out)


def main():
    data = SRC.read_bytes()
    w, h, rgba = parse_png(data)
    print(f"源图：{SRC.name} {w}x{h}")

    tip = find_tip(w, h, rgba)
    hot = (round(tip[0] * DST_SIZE / w), round(tip[1] * DST_SIZE / h))
    print(f"指针尖端：源图 {tip} → {DST_SIZE}x{DST_SIZE} 热点 {hot}")
    if hot != EXPECT_HOT:
        raise SystemExit(
            f"热点 {hot} 与 shared/cursor_sprite.rs 的 "
            f"CUR_HOT_X/CUR_HOT_Y={EXPECT_HOT} 不一致，请同步两处常量"
        )

    out = downscale(w, h, rgba, DST_SIZE)
    DST.write_bytes(out)
    print(f"已写入 {DST}（{DST_SIZE}x{DST_SIZE} RGBA8888，{len(out)} 字节）")


if __name__ == "__main__":
    sys.exit(main())
