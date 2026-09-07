#!/usr/bin/env python3
"""把 cn.ttf 子集化为 KunxOS 实际用到的字符，供系统平滑字体渲染使用。

字符集 = GB2312 一级字库（3755 常用汉字）+ ASCII 可打印 + 全仓库源码用到的
字符 + 常用 CJK 标点/符号。输出 tools/fonts/cn-subset.ttf（一般 1-3MB）。

用法：
  python3 tools/subset_font.py [输出路径]
"""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC_TTF = os.path.join(ROOT, "fonts", "cn.ttf")
OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "tools", "fonts", "cn-subset.ttf")

# ---- 1) GB2312 一级字库：区 16..=55，每区 94 位 ----
def gb2312_level1():
    chars = set()
    for qu in range(16, 56):
        for wei in range(1, 95):
            b0, b1 = qu + 0xA0, wei + 0xA0
            try:
                chars.add(bytes([b0, b1]).decode("gb2312"))
            except UnicodeDecodeError:
                pass
    return chars

# ---- 2) 源码用到的字符 ----
def source_chars():
    chars = set()
    skip_dirs = {".git", "target", "node_modules", ".agent_backup", "tools/fonts"}
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for fn in filenames:
            if not fn.endswith((".rs", ".md", ".py", ".sh", ".toml", ".txt", ".html", ".css", ".js")):
                continue
            p = os.path.join(dirpath, fn)
            try:
                with open(p, encoding="utf-8", errors="ignore") as f:
                    chars.update(f.read())
            except OSError:
                pass
    return chars

# ---- 3) 常用 CJK 标点/符号（GB2312 一级不含但界面常见） ----
EXTRA = set("，。！？；：、（）《》「」『』【】·—…～￥×÷±°％‰′″←↑→↓√∑∈≈≠≤≥∞①②③④⑤⑥⑦⑧⑨⑩★☆●○◆◇■□▲△¤§№℃℉")

def main():
    # 字符集 = 源码用字 + 常用标点 + ASCII（去掉 GB2312 一级全量：
    # 3755 字使子集 3.7MB、fontdue 解析数分钟；系统文本均为源码写死，
    # 源码用字即可全覆盖）
    chars = source_chars() | EXTRA
    # 仅保留 U+0020..U+FFFF（字体子集不支持代理对）
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"collecting {len(chars)} unique chars")

    # 用 fontTools 子集化
    from fontTools import subset
    opts = subset.Options()
    opts.desubroutinize = True
    opts.layout_features = ["*"]
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.drop_tables += ["FFTM", "fvar", "gvar", "cvar", "avar", "STAT", "HVAR", "VVAR", "MVAR", "GDEF"]
    opts.retain_gids = False
    opts.recommended_glyphs = True

    font = subset.load_font(SRC_TTF, opts)
    subsetter = subset.Subsetter(options=opts)
    subsetter.populate(text="".join(sorted(chars)))
    subsetter.subset(font)
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    subset.save_font(font, OUT, opts)
    size = os.path.getsize(OUT)
    print(f"wrote {OUT} ({size/1024:.1f} KiB, {size/1024/1024:.2f} MiB)")

if __name__ == "__main__":
    main()
