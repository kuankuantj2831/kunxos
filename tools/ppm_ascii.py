#!/usr/bin/env python3
"""Downsample a PPM screendump to coarse ASCII art + basic stats."""
import sys, os

def load_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    # P6 文件头
    assert data[:2] == b"P6", data[:2]
    parts = data[2:].split(b"\n", 4)
    # 格式：width height maxval，随后为原始像素
    toks = []
    i = 2
    while len(toks) < 3:
        j = data.index(b"\n", i)
        line = data[i:j].strip()
        if line and not line.startswith(b"#"):
            toks += line.split()
        i = j + 1
    w, h, mx = int(toks[0]), int(toks[1]), int(toks[2])
    raw = data[i:]
    return w, h, raw

def ascii_art(path, cols=90):
    w, h, raw = load_ppm(path)
    rows = int(cols * h / w * 0.5)
    # 对分块取平均
    out = []
    chars = " .:-=+*#%@"
    for by in range(rows):
        line = ""
        for bx in range(cols):
            x0 = bx * w // cols
            x1 = (bx + 1) * w // cols
            y0 = by * h // rows
            y1 = (by + 1) * h // rows
            r = g = b = 0
            n = 0
            for y in range(y0, y1):
                for x in range(x0, x1):
                    off = (y * w + x) * 3
                    r += raw[off]; g += raw[off+1]; b += raw[off+2]
                    n += 1
            r, g, b = r // n, g // n, b // n
            lum = (r * 3 + g * 6 + b) // 10
            line += chars[min(lum * len(chars) // 256, len(chars) - 1)]
        out.append(line)
    print("\n".join(out))

if __name__ == "__main__":
    for p in sys.argv[1:]:
        print("=====", p)
        w, h, raw = load_ppm(p)
        print("size", w, "x", h)
        ascii_art(p)
