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
  emoji-subset.ttf Noto Color Emoji 子集（CBDT 位图，retain_gids 保持字形 id 稳定）：
                   常用 Emoji + ZWJ 家庭序列 + 肤色修饰 + 键盘字符

用法：python3 tools/make_fonts.py [--no-emoji]
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "tools", "fonts")
os.makedirs(OUT, exist_ok=True)


# ==================== 1) CJK/UI 子集（普通文字 + 标题） ====================

EXTRA = set("，。！？；：、（）《》「」『』【】·—…～￥×÷±°％‰′″←↑→↓√∑∈≈≠≤≥∞①②③④⑤⑥⑦⑧⑨⑩★☆●○◆◇■□▲△¤§№℃℉")

# 运行期不渲染该文件的字符：拼音 IME 尚未接线（无应用 include pinyin_dict），
# 其 GB2312 一级全字表（~3.5k 字）占全量子集的 2/3，是 fontdue 启动解析
# 缓慢的主因。将来接入 IME 时删除本排除并重建字体。
RUNTIME_TEXT_EXCLUDES = ("pinyin_dict.rs",)

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

def source_chars():
    chars = set()
    skip_dirs = {".git", "target", "node_modules", ".agent_backup", "tools/fonts", ".kilo", "kunxcore/ramdisk.img", "__pycache__"}
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in skip_dirs]
        for fn in filenames:
            p = os.path.join(dirpath, fn)
            # 运行期文本只来自应用/Rust 源码字符串与包仓库生成器（store 显示
            # 的 Packages 描述由 tools/kunxrepo 生成）；文档/日志等大文件
            # 里的字不参与子集，避免 CHANGELOG/BUG_REPORT 撑大字体。
            if fn.endswith(".rs"):
                pass
            elif p.startswith(os.path.join(ROOT, "tools", "kunxrepo")) and fn.endswith((".py", ".json")):
                pass
            else:
                continue
            if any(fn.endswith(e) or fn == e for e in RUNTIME_TEXT_EXCLUDES):
                continue
            try:
                with open(p, encoding="utf-8", errors="ignore") as f:
                    text = f.read()
            except OSError:
                pass
            else:
                chars.update(text)
                chars.update(decode_escapes(text))
    return chars

def subset_font(src, dst, tag):
    from fontTools import subset
    chars = source_chars() | EXTRA
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[{tag}] {len(chars)} unique chars")
    opts = subset.Options()
    opts.retain_gids = False
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.recommended_glyphs = False
    opts.drop_tables = ["FFTM", "FFTM"]
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
                chars.update(fh.read())
        except OSError:
            pass
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[oobe] {len(chars)} unique chars")
    from fontTools import subset
    opts = subset.Options()
    opts.retain_gids = False
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.recommended_glyphs = False
    opts.drop_tables = ["FFTM", "FFTM"]
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


# ==================== 3) Emoji 子集（CBDT 位图，retain_gids） ====================

EMOJI_BASE = (
    "😀😁😂🤣😊😍😘😗😜🤪🤨🧐🤓😎🤩🥳😏😒🙄😮💨😴😷🤒🤕🤢🤮🥵🥶😭😱😳🤔🤫🤭"
    "😡😠😤😇🥰😉🙃😋🤤😴😈👿💀👻👽🤖💩😺😸😹😻😼😽🙀😿😾"
    "👋🤚🖐✋🖖👌🤌🤏✌🤞🤟🤘🤙👈👉👆👇☝👍👎✊👊🤛🤜👏🙌👐🤲🤝🙏"
    "💪🦾🦿🦵🦶👂🦻👃🧠🦷🦴👀👁👅👄💋"
    "❤🧡💛💚💙💜🖤🤍🤎💔❣💕💞💓💗💖💘💝💟"
    "🌹🥀🌷🌸💐🌻🌼🌺🌾🍀🍁🍂🍃🌵🌴🌳🌲🌿☘🍄🌊🌈☀🌤⛅🌥☁🌦🌧⛈🌩🌨❄☃⛄🌬💨💧💦☔"
    "🍏🍎🍐🍊🍋🍌🍉🍇🍓🍈🍒🍑🥭🍍🥥🥝🍅🥑🍆🥔🥕🌽🌶🥒🥬🥦🧄🧅🍄🥜🌰"
    "🍞🥐🥖🥨🥯🥞🧇🧀🍖🍗🥩🥓🍔🍟🍕🌭🥪🌮🌯🥙🧆🥚🍳🥘🍲🥣🥗🍿🧈🍱🍘🍙🍚🍛🍜🍝🍠🍢🍣🍤🍥🥮🍡🥟🥠🥡🦀🦞🦐🦑🦪🍦🍧🍨🍩🍪🎂🍰🧁🥧🍫🍬🍭🍮🍯🍼🥛☕🍵🍶🍾🍷🍸🍹🍺🍻🥂🥃"
    "⚽⚾🏀🏐🏈🏉🎾🎱🎳🏏🏑🏒🏓🏸🥊🥋🥅🎣🤿🎽🎿🛷🥌🎯🪀🪁🎱🔮🎮🕹🎲♟🎭🎨🎪🎤🎧🎼🎹🥁🎷🎺🎸🪕🎻🎬🎽🎾"
    "🚗🚕🚙🚌🚎🏎🚓🚑🚒🚐🛻🚚🚛🚜🛴🚲🛵🏍🛺🚨🚔🚍🚘🚖🚡🚠🚟🚃🚋🚞🚝🚄🚅🚈🚂🚆🚇🚊🚉✈🛫🛬🛩💺🛰🚀🛸🚁🛶⛵🚤🛥🛳⛴🚢⚓"
    "⌚📱📲💻⌨🖥🖨🖱🖲🕹🗜💽💾💿📀📼📷📸📹🎥📽🎞📞☎📟📠📺📻🎙🎚🎛⏱⏲⏰🕰⌛⏳📡🔋🔌💡🔦🕯🪔🧯🛢💸💵💴💶💷💰💳🧾💎⚖🪜🧰🪛🔧🔨⚒🛠⛏🪚🔩⚙🪤🧱⛓🧲🔫💣🧨🪓🔪🗡⚔🛡🚬⚰🪦⚱🏺🔮📿🧿"
    "📝📁📂🗂📅📆🗒🗓📇📈📉📊📋📌📍📎🖇📏📐✂🗃🗄🗑🔒🔓🔏🔐🔑🗝🔨"
    "❤️✨⭐🌟💫💥💢💣💬💭💤🗨🗯💦💨🕳💉💊🚬"
    "🎉🎊🎁🎈🎏🎀🪄🎊🪅🎪🎭🎨🎬🎤🎧🎼🎹🥁🎷🎺🎸🪕🎻🎲♟🎯🎳🎮🎰🧩"
    "🏠🏡🏘🏢🏣🏤🏥🏦🏨🏩🏪🏫🏬🏭🏯🏰💒🗼🗽🗻🏔⛰🌋🗾🏕🏖🏜🏝🏟🏛🏗🧱🏘"
    "🇨🇳🇺🇸🇬🇧🇯🇵🇰🇷🇫🇷🇩🇪🇮🇹🇪🇸🇷🇺🇧🇷🇮🇳🇦🇪🇸🇦"
    "1️⃣2️⃣3️⃣4️⃣5️⃣6️⃣7️⃣8️⃣9️⃣0️⃣🔟"
    "↔️↕️↖️↗️↘️↙️↩️↪️⤴️⤵️🔃🔄🔙🔚🔛🔜🔝🛐"
    "⚛️🕉️✡️☸️☯️✝️☦️☪️☮️🕎🔯♈️♉️♊️♋️♌️♍️♎️♏️♐️♑️♒️♓️⛎️"
    "▶️⏸️⏹️⏺️⏭️⏮️⏩️⏪️⏫️⏬️◀️🔼🔽➡️⬅️⬆️⬇️↗️↘️↙️↖️↕️↔️"
    "🔴🟠🟡🟢🔵🟣🟤⚫️⚪️🟥🟧🟨🟩🟦🟪🟫⬛️⬜️"
    "🆕🆙🆒🆓🆖🆗🆘🆚🈁🈂️🈷️🈶️🈯️🉐️🈹️🈚️🈲️🉑️🈸️🈴️🈳️㊗️㊙️🈺️🈵️🔞"
    "🟰©️®️™️#️⃣*️⃣ℹ️❕❗❓❔❕❗⭕️✅️☑️✔️❌️❎️➕️➖️➗️➰️➿️〽️✳️✴️❇️‼️⁉️❓❔"
    "0️⃣1️⃣2️⃣3️⃣4️⃣5️⃣6️⃣7️⃣8️⃣9️⃣🔟"
)

# ZWJ 家庭/职业序列（GSUB 连字，必须整序列传入）
EMOJI_ZWJ = (
    "👨‍👩‍👧‍👦👩‍👩‍👧‍👦👨‍👨‍👧‍👦👨‍👩‍👦‍👦👨‍👩‍👧‍👧👩‍👩‍👦‍👦👩‍👩‍👧‍👧👨‍👨‍👦‍👦👨‍👨‍👧‍👧"
    "👨‍👩‍👦👨‍👩‍👧👩‍👩‍👦👩‍👩‍👧👨‍👨‍👦👨‍👨‍👧"
    "👩‍❤️‍👨👨‍❤️‍👨👩‍❤️‍👩👩‍❤️‍💋‍👨👨‍❤️‍💋‍👨👩‍❤️‍💋‍👩"
    "👨‍💻👩‍💻👨‍💼👩‍💼👨‍🏫👩‍🏫👨‍🚀👩‍🚀👨‍🔧👩‍🔧👨‍🍳👩‍🍳👨‍⚕️👩‍⚕️👨‍🎨👩‍🎨👨‍🌾👩‍🌾👨‍🎤👩‍🎤👨‍🎓👩‍🎓👨‍🏭👩‍🏭👨‍🔬👩‍🔬👨‍✈️👩‍✈️👨‍🚒👩‍🚒👮‍♂️👮‍♀️👷‍♂️👷‍♀️💂‍♂️💂‍♀️🕵️‍♂️🕵️‍♀️👩‍⚖️👨‍⚖️👩‍🌾👨‍🌾👩‍🍳👨‍🍳👩‍🎓👨‍🎓"
    "👋🏻👋🏼👋🏽👋🏾👋🏿👍🏻👍🏼👍🏽👍🏾👍🏿👎🏻👎🏼👎🏽👎🏾👎🏿✌🏻✌🏼✌🏽✌🏾✌🏿👌🏻👌🏼👌🏽👌🏾👌🏿🤙🏻🤙🏼🤙🏽🤙🏾🤙🏿🤝🏻🤝🏼🤝🏽🤝🏾🤝🏿🙏🏻🙏🏼🙏🏽🙏🏾🙏🏿👏🏻👏🏼👏🏽👏🏾👏🏿💪🏻💪🏼💪🏽💪🏾💪🏿"
    "🏃🏻🏃🏼🏃🏽🏃🏾🏃🏿🧑‍🦱🧑‍🦰🧑‍🦳🧑‍🦲🧑‍🎄"
)

# 肤色修饰符本身（供直接引用）
EMOJI_MODIFIERS = "🏻🏼🏽🏾🏿"


def build_emoji():
    from fontTools import subset
    src = os.path.join(ROOT, "fonts", "emoji-full.ttf")
    dst = os.path.join(OUT, "emoji-subset.ttf")

    text = EMOJI_BASE + EMOJI_ZWJ + EMOJI_MODIFIERS
    # 去重（保留顺序）
    seen = set()
    chars = []
    for ch in text:
        if ch not in seen:
            seen.add(ch)
            chars.append(ch)
    # 排除代理对之外的（全为 BMP 平面内 emoji）
    chars = [c for c in chars if 0x20 <= ord(c) <= 0x10FFFF]
    print(f"[emoji] {len(chars)} unique codepoints")
    opts = subset.Options()
    opts.retain_gids = True        # 保持字形 id 稳定 -> CBDT 位图索引不变
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = ["*"]
    opts.recommended_glyphs = False
    opts.layout_features = ["*"]
    opts.layout_closure = True
    subsetter = subset.Subsetter(options=opts)
    subsetter.populate(text="".join(chars))
    from fontTools.ttLib import TTFont
    font = TTFont(src)
    subsetter.subset(font)
    font.save(dst)
    size = os.path.getsize(dst)
    print(f"[emoji] -> {dst} ({size//1024} KB, {size/1048576:.1f} MB)")


def build_launcher():
    """桌面启动器超小子集：仅扫描 apps/launcher 文案（应用名/dock/小组件/状态栏/
    控制中心/弹窗），字体小一个量级 → 桌面首帧即平滑，慢 TCG 宿主不再卡在
    「正在进入系统」。"""
    import glob
    chars = EXTRA.copy()
    for f in glob.glob(os.path.join(ROOT, "apps", "launcher", "src", "**", "*.rs"), recursive=True):
        try:
            with open(f, encoding="utf-8", errors="ignore") as fh:
                chars.update(fh.read())
        except OSError:
            pass
    chars = {c for c in chars if 0x20 <= ord(c) <= 0xFFFF}
    print(f"[launcher] {len(chars)} unique chars")
    from fontTools import subset
    opts = subset.Options()
    opts.retain_gids = False
    opts.notdef_outline = True
    opts.recalc_bounds = True
    opts.name_IDs = ["*"]
    opts.name_legacy = True
    opts.recommended_glyphs = False
    opts.drop_tables = ["FFTM", "FFTM"]
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
    if "--no-emoji" not in sys.argv:
        build_emoji()
    print("done")
