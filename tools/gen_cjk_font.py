#!/usr/bin/env python3
"""生成共享 CJK 16x16 点阵字库 Rust 模块。

输入：tools/cjk/hzk16.bin（标准 HZK16，GB2312 区位码索引，每字 32 字节）
用法：
  python3 tools/gen_cjk_font.py full shared/cjk_font_full.rs
      # 全量 GB2312 一级字库（3755 字），供内核使用
  python3 tools/gen_cjk_font.py subset <out.rs> <src1.rs> [src2.rs ...]
      # 从指定源文件中提取用到的 CJK 字符生成子集，供各应用使用（SFS 单文件 96KB 限制）

字形编码：每行 2 字节、高位在前（MSB = 左起第 1 列）。
索引：quwei（0 基）= (byte0 - 0xA1, byte1 - 0xA1)，offset = (qu*94 + wei)*32。
"""
import os
import sys

HZK16 = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cjk", "hzk16.bin")

def load_font(path):
    with open(path, "rb") as f:
        return f.read()

def quwei_to_char(qu, wei):
    """1 基区位 → 字符（GB2312）。"""
    b0, b1 = qu + 0xA0, wei + 0xA0
    try:
        return bytes([b0, b1]).decode("gb2312")
    except UnicodeDecodeError:
        return None

def glyph_offset(qu, wei):
    return ((qu - 1) * 94 + (wei - 1)) * 32  # qu/wei 1 基

def collect(font, chars):
    """chars: 字符集合（str）。返回 [(码点, 32 字节字形), ...]"""
    out = []
    seen = set()
    for ch in chars:
        cp = ord(ch)
        if cp < 0x80 or cp > 0xFFFF or cp in seen:
            continue
        try:
            gb = ch.encode("gb2312")
        except UnicodeEncodeError:
            continue  # 非 GB2312 字符（生僻字）跳过
        if len(gb) != 2:
            continue
        qu, wei = gb[0] - 0xA0, gb[1] - 0xA0
        if not (1 <= qu <= 94 and 1 <= wei <= 94):
            continue
        off = glyph_offset(qu, wei)
        g = font[off:off + 32]
        if len(g) != 32:
            continue
        seen.add(cp)
        out.append((cp, g))
    out.sort()
    return out

def collect_level1(font):
    """GB2312 一级字库：区 16..=55。"""
    chars = []
    for qu in range(16, 56):
        for wei in range(1, 95):
            ch = quwei_to_char(qu, wei)
            if ch is not None:
                chars.append(ch)
    return collect(font, chars)

def scan_sources(paths):
    """扫描源文件中的全部 CJK 字符。"""
    chars = set()
    for p in paths:
        with open(p, encoding="utf-8") as f:
            chars.update(c for c in f.read() if ord(c) >= 0x80 and ord(c) <= 0xFFFF)
    return chars

def emit(glyphs, note):
    n = len(glyphs)
    codes = ", ".join(f"0x{c:04X}" for c, _ in glyphs)
    rows = []
    for _, g in glyphs:
        for b in g:
            rows.append(f"0x{b:02X}")
    bm_lines = []
    for i in range(0, len(rows), 24):
        bm_lines.append("    " + ", ".join(rows[i:i + 24]) + ",")

    src = f"""//! {note}
//! 由 tools/gen_cjk_font.py 从 HZK16 生成，勿手改。字形每行 2 字节、MSB 在左。

/// 中文字形尺寸（像素）。
pub const CJK_W: i32 = 16;
pub const CJK_H: i32 = 16;
/// 字库字符数。
pub const CJK_COUNT: usize = {n};

/// 码点表（升序，配合二分查找）。
pub static CJK_CODES: [u16; {n}] = [
    {codes},
];

/// 字形位图（每字 32 字节：16 行 × 2 字节/行，MSB 在前）。
pub static CJK_BITMAPS: [u8; {n * 32}] = [
{chr(10).join(bm_lines)}
];

/// 二分查找码点，返回字形索引。
#[inline]
pub fn cjk_index(cp: u16) -> Option<usize> {{
    let mut lo = 0usize;
    let mut hi = CJK_CODES.len();
    while lo < hi {{
        let mid = (lo + hi) / 2;
        let v = CJK_CODES[mid];
        if v == cp {{
            return Some(mid);
        }} else if v < cp {{
            lo = mid + 1;
        }} else {{
            hi = mid;
        }}
    }}
    None
}}

/// 解码 `s[i..]` 处的下一个 UTF-8 码点，返回 `(码点, 字节数)`。
/// `i` 越界或非法字节时返回 `(0, 1)`（渲染为空位）。
#[inline]
pub fn utf8_next(s: &[u8], i: usize) -> (u32, usize) {{
    if i >= s.len() {{
        return (0, 1);
    }}
    let b0 = s[i];
    if b0 < 0x80 {{
        return (b0 as u32, 1);
    }}
    let need = if b0 >= 0xF0 {{
        4
    }} else if b0 >= 0xE0 {{
        3
    }} else if b0 >= 0xC0 {{
        2
    }} else {{
        return (0, 1);
    }};
    if i + need > s.len() {{
        return (0, 1);
    }}
    let mut cp = match need {{
        2 => (b0 & 0x1F) as u32,
        3 => (b0 & 0x0F) as u32,
        _ => (b0 & 0x07) as u32,
    }};
    for k in 1..need {{
        let b = s[i + k];
        if b & 0xC0 != 0x80 {{
            return (0, 1);
        }}
        cp = (cp << 6) | (b & 0x3F) as u32;
    }}
    (cp, need)
}}

/// 字符串的文本宽度（单位：像素，ASCII 8px、CJK 16px）。
#[inline]
pub fn text_width(s: &[u8]) -> i32 {{
    let mut w = 0i32;
    let mut i = 0;
    while i < s.len() {{
        let (cp, n) = utf8_next(s, i);
        if cp >= 0x80 {{
            w += CJK_W;
        }} else {{
            w += 8;
        }}
        i += n;
    }}
    w
}}
"""
    return src

def main():
    mode = sys.argv[1]
    if mode == "full":
        out = sys.argv[2]
        font = load_font(HZK16)
        glyphs = collect_level1(font)
        note = "共享 CJK 16x16 点阵字库（GB2312 一级字库，%d 个常用汉字）" % len(glyphs)
    elif mode == "subset":
        out = sys.argv[2]
        srcs = sys.argv[3:]
        font = load_font(HZK16)
        chars = scan_sources(srcs)
        glyphs = collect(font, chars)
        note = "共享 CJK 16x16 点阵字库（按应用源码提取 %d 字子集）" % len(glyphs)
    else:
        print("usage: gen_cjk_font.py full <out.rs> | subset <out.rs> <src...>")
        sys.exit(1)
    print(f"collected {len(glyphs)} glyphs")
    out = os.path.abspath(out)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w", encoding="utf-8") as f:
        f.write(emit(glyphs, note))
    print(f"wrote {out} ({os.path.getsize(out)} bytes)")

if __name__ == "__main__":
    main()
