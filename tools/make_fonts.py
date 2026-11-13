#!/usr/bin/env python3
"""KunxOS 字体构建脚本：从 fonts/ 源字体生成 rootfs 实际使用的子集。

输出（tools/fonts/，随 rootfs 提供为 /fonts/*.ttf）：
  cn-subset.ttf    更纱黑体 UI SC 常规（fonts/uiputong.ttf）子集：
                   ASCII + 源码用字 + 常用 CJK 标点（普通文字）
  bold-subset.ttf  更纱黑体 UI SC SemiBold（fonts/bold.ttf）子集：
                   与 cn 相同字符集（标题）
  icon-fill-subset.ttf  Phosphor-Fill（fonts/Phosphor-Fill.ttf）实心图标子集，
                   码点取自 shared/icons.rs → /fonts/icon-fill.ttf
  icon-line-subset.ttf  Phosphor-Bold（fonts/Phosphor-Bold.ttf）粗线图标子集，
                   码点取自 shared/icons.rs → /fonts/icon-line.ttf（行内 emoji）

用法：python3 tools/make_fonts.py
"""
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tools", "fonts")
os.makedirs(OUT, exist_ok=True)


# ==================== 1) CJK/UI 子集（普通文字 + 标题） ====================

EXTRA = set("，。！？；：、（）《》「」『』【】·—…～￥×÷±°％‰′″←↑→↓√∑∈≈≠≤≥∞①②③④⑤⑥⑦⑧⑨⑩★☆●○◆◇■□▲△¤§№℃℉")

# 运行期不渲染该文件的字符：拼音 IME 尚未接线（无应用 include pinyin_dict），
# 其 GB2312 一级全字表（~3.5k 字）占全量子集的 2/3，是 fontdue 启动解析
# 缓慢的主因。将来接入 IME 时删除本排除并重建字体。
RUNTIME_TEXT_EXCLUDES = ("pinyin_dict.rs",)

# 扫描范围（运行期会往屏幕渲染文本的源码）：
#   apps/**    应用 UI 文案；shared/** 共享 UI 底座（l10n/app_kit/error_ui…）；
#   kunxcore/src/{main,ui_framework}.rs 内核启动 logo/窗口标题；
#   tools/kunxrepo 包仓库描述（store 界面渲染）。
# 内核/守护进程的串口日志、注释、测试代码从不上屏，不再进子集 ——
# 此前整文件扫描把全仓库中文注释的汉字都收进了 cn.ttf（~1500 字形
# 761KB），是 fontdue 每次应用启动都要秒级解析的主因。
SCAN_RS_DIRS = ("apps", "shared")
SCAN_RS_FILES = ("kunxcore/src/main.rs", "kunxcore/src/ui_framework.rs")
SCAN_DATA_DIRS = (("tools", "kunxrepo"),)

def decode_escapes(text):
    """把源码中的 \\xNN 字节转义序列按 UTF-8 解出（中文文案常以
    b"\\xE7\\xBD\\x91..." 形式书写，直接扫文本只会扫到转义字面量）。"""
    out = set()
    for m in re.finditer(r"(?:\\x[0-9A-Fa-f]{2}){2,}", text):
        try:
            raw = bytes(int(x, 16) for x in re.findall(r"\\x([0-9A-Fa-f]{2})", m.group(0)))
        except ValueError:
            continue
        out.update(raw.decode("utf-8", errors="ignore"))
    for m in re.finditer(r"\\u\{([0-9A-Fa-f]{1,6})\}", text):
        out.add(chr(int(m.group(1), 16)))
    return out

def rust_literal_chars(text):
    """从 Rust 源码提取字符串/字符字面量里的字符（状态机跳过注释与代码）。
    转义序列原文保留在输出里，由 decode_escapes 统一解码。"""
    out = set()
    i, n = 0, len(text)
    in_str = in_chr = in_lc = in_bc = False
    while i < n:
        c = text[i]
        if in_lc:
            if c == "\n":
                in_lc = False
            i += 1
        elif in_bc:
            if c == "*" and i + 1 < n and text[i + 1] == "/":
                in_bc = False
                i += 2
            else:
                i += 1
        elif in_str:
            if c == "\\":
                # 转义序列：保留原文（含引号转义），跳过下一字符
                out.add(c)
                if i + 1 < n:
                    out.add(text[i + 1])
                    i += 2
                else:
                    i += 1
            elif c == '"':
                in_str = False
                i += 1
            else:
                out.add(c)
                i += 1
        elif in_chr:
            if c == "\\":
                if i + 1 < n and text[i + 1] == "u":
                    j = text.find("}", i)
                    if j != -1:
                        out.update(decode_escapes(text[i:j + 1]))
                        i = j + 1
                        in_chr = False
                        continue
                out.update(decode_escapes(text[i:i + 4]))
                i += 2
            elif c == "'":
                in_chr = False
                i += 1
            else:
                out.add(c)
                i += 1
        elif c == "/" and i + 1 < n and text[i + 1] == "/":
            in_lc = True
            i += 2
        elif c == "/" and i + 1 < n and text[i + 1] == "*":
            in_bc = True
            i += 2
        elif c == '"':
            in_str = True
            i += 1
        elif c == "'":
            # 字符字面量 vs 生命周期标注（&'static / 'a）：只有「' 后 1~2 个
            # 字符即闭合」的形态才按字符字面量处理，避免生命周期吞掉代码。
            if i + 2 < n and text[i + 2] == "'" and text[i + 1] != " ":
                in_chr = True
                i += 1
            elif i + 3 < n and text[i + 1] == "\\" and text[i + 3] == "'":
                in_chr = True
                i += 1
            else:
                i += 1
        else:
            i += 1
    return out

def _walk_rs_files():
    for d in SCAN_RS_DIRS:
        base = os.path.join(ROOT, d)
        for dirpath, dirnames, filenames in os.walk(base):
            dirnames[:] = [x for x in dirnames if x not in {"target", "__pycache__"}]
            for fn in filenames:
                if fn.endswith(".rs") and not any(fn.endswith(e) or fn == e for e in RUNTIME_TEXT_EXCLUDES):
                    yield os.path.join(dirpath, fn)
    for rel in SCAN_RS_FILES:
        p = os.path.join(ROOT, rel)
        if os.path.exists(p):
            yield p

def source_chars():
    chars = set()
    for p in _walk_rs_files():
        try:
            with open(p, encoding="utf-8", errors="ignore") as f:
                text = f.read()
        except OSError:
            continue
        chars.update(rust_literal_chars(text))
        chars.update(decode_escapes(text))
    for d0, d1 in SCAN_DATA_DIRS:
        base = os.path.join(ROOT, d0, d1)
        for dirpath, dirnames, filenames in os.walk(base):
            dirnames[:] = [x for x in dirnames if x not in {"__pycache__"}]
            for fn in filenames:
                if not fn.endswith((".py", ".json")):
                    continue
                try:
                    with open(os.path.join(dirpath, fn), encoding="utf-8", errors="ignore") as f:
                        chars.update(f.read())
                except OSError:
                    pass
    return chars

def subset_options():
    """子集化选项：fontdue 只读 cmap/glyf/loca/head/hhea/hmtx/maxp，其余表
    （name 记录、TrueType hinting 字节码、GSUB/GPOS 整形、post 字形名）全部
    丢弃 —— 同字形数下文件显著变小，应用启动的字体 I/O 与解析同步提速。"""
    from fontTools import subset
    opts = subset.Options()
    opts.retain_gids = False
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = []
    opts.name_legacy = False
    opts.recommended_glyphs = False
    opts.hinting = False
    opts.layout_features = []
    opts.drop_tables += ["GSUB", "GPOS", "GDEF", "BASE", "post"]
    return opts

def subset_font(src, dst, tag):
    from fontTools import subset
    chars = source_chars() | EXTRA
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[{tag}] {len(chars)} unique chars")
    opts = subset_options()
    subsetter = subset.Subsetter(options=opts)
    subsetter.populate(text="".join(sorted(chars)))
    from fontTools.ttLib import TTFont
    font = TTFont(src)
    subsetter.subset(font)
    font.save(dst)
    print(f"[{tag}] -> {dst} ({os.path.getsize(dst)//1024} KB)")

def build_cn():
    """普通文字子集：fonts/uiputong.ttf（更纱黑体 UI SC 常规）。"""
    subset_font(
        os.path.join(ROOT, "fonts", "uiputong.ttf"),
        os.path.join(OUT, "cn-subset.ttf"),
        "cn",
    )

def build_bold():
    """标题子集：fonts/bold.ttf（更纱黑体 UI SC SemiBold），与 cn 相同字符集。"""
    subset_font(
        os.path.join(ROOT, "fonts", "bold.ttf"),
        os.path.join(OUT, "bold-subset.ttf"),
        "bold",
    )


def build_oobe():
    """OOBE 超小 uiputong 子集（正文 + 标题同词汇，仅向导文案）。"""
    import glob
    chars = EXTRA.copy()
    for f in glob.glob(os.path.join(ROOT, "apps", "oobe", "src", "*.rs")) + \
             glob.glob(os.path.join(ROOT, "apps", "oobe", "src", "widgets", "*.rs")):
        try:
            with open(f, encoding="utf-8", errors="ignore") as fh:
                text = fh.read()
        except OSError:
            continue
        chars.update(rust_literal_chars(text))
        chars.update(decode_escapes(text))
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[oobe] {len(chars)} unique chars")
    from fontTools import subset
    opts = subset_options()
    from fontTools.ttLib import TTFont
    for (src, dst, tag) in (
        (os.path.join(ROOT, "fonts", "uiputong.ttf"), os.path.join(OUT, "oobe-cn-subset.ttf"), "oobe-cn"),
        (os.path.join(ROOT, "fonts", "bold.ttf"), os.path.join(OUT, "oobe-bold-subset.ttf"), "oobe-bold"),
    ):
        s = subset.Subsetter(options=opts)
        s.populate(text="".join(sorted(chars)))
        f = TTFont(src)
        s.subset(f)
        f.save(dst)
        print(f"[{tag}] -> {dst} ({os.path.getsize(dst)//1024} KB)")


def build_launcher():
    """桌面启动器超小子集：仅扫描 apps/launcher 文案（应用名/dock/小组件/状态栏/
    控制中心/弹窗），字体小一个量级 → 桌面首帧即平滑，慢 TCG 宿主不再卡在
    「正在进入系统」。"""
    import glob
    chars = EXTRA.copy()
    for f in glob.glob(os.path.join(ROOT, "apps", "launcher", "src", "**", "*.rs"), recursive=True):
        try:
            with open(f, encoding="utf-8", errors="ignore") as fh:
                text = fh.read()
        except OSError:
            continue
        chars.update(rust_literal_chars(text))
        chars.update(decode_escapes(text))
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[launcher] {len(chars)} unique chars")
    from fontTools import subset
    opts = subset_options()
    from fontTools.ttLib import TTFont
    for (src, dst, tag) in (
        (os.path.join(ROOT, "fonts", "uiputong.ttf"), os.path.join(OUT, "launcher-cn-subset.ttf"), "launcher-cn"),
        (os.path.join(ROOT, "fonts", "bold.ttf"), os.path.join(OUT, "launcher-bold-subset.ttf"), "launcher-bold"),
    ):
        s = subset.Subsetter(options=opts)
        s.populate(text="".join(sorted(chars)))
        f = TTFont(src)
        s.subset(f)
        f.save(dst)
        print(f"[{tag}] -> {dst} ({os.path.getsize(dst)//1024} KB)")


# ==================== 4) Phosphor 图标子集（应用图标贴图 + 行内 emoji） ====================

def icon_codepoints():
    """shared/icons.rs 是图标码点唯一事实来源：提取其中全部 PUA 区字面量
    （0xE000..=0xF8FF 的 4 位十六进制），应用图标/天气图标/emoji 映射值
    全部包含在内。"""
    import re
    src = os.path.join(ROOT, "shared", "icons.rs")
    with open(src, encoding="utf-8") as f:
        text = f.read()
    cps = {int(m, 16) for m in re.findall(r"0x([EeFf][0-9A-Fa-f]{3})\b", text)}
    cps = {c for c in cps if 0xE000 <= c <= 0xF8FF}
    if not cps:
        raise SystemExit("[icons] shared/icons.rs 中未找到任何 PUA 码点")
    return sorted(cps)


def build_icons():
    """Phosphor 图标子集：fonts/Phosphor-Fill.ttf → icon-fill-subset.ttf（实心，
    随 rootfs 以 /fonts/icon-fill.ttf 提供应用图标/天气图标）；fonts/Phosphor-
    Bold.ttf → icon-line-subset.ttf（粗线，/fonts/icon-line.ttf 行内 emoji）。
    码点来自 shared/icons.rs（source of truth）。"""
    from fontTools import subset
    from fontTools.ttLib import TTFont
    cps = icon_codepoints()
    print(f"[icons] {len(cps)} icon codepoints")
    opts = subset.Options()
    opts.retain_gids = False
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.recommended_glyphs = False
    opts.drop_tables = ["FFTM", "FFTM"]
    for (src, dst, tag) in (
        (os.path.join(ROOT, "fonts", "Phosphor-Fill.ttf"), os.path.join(OUT, "icon-fill-subset.ttf"), "icon-fill"),
        (os.path.join(ROOT, "fonts", "Phosphor-Bold.ttf"), os.path.join(OUT, "icon-line-subset.ttf"), "icon-line"),
    ):
        s = subset.Subsetter(options=opts)
        s.populate(unicodes=cps)
        f = TTFont(src)
        s.subset(f)
        f.save(dst)
        print(f"[{tag}] -> {dst} ({os.path.getsize(dst)//1024} KB)")


if __name__ == "__main__":
    build_cn()
    build_bold()
    build_oobe()
    build_launcher()
    build_icons()
    print("done")
