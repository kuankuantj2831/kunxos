#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成 KunxOS 桌面应用图标贴图（字形来自 Phosphor 图标字体）。

为 apps/launcher/src/main.rs 的 APPS 表（21 个应用，顺序一致）逐个渲染
56x56 RGBA8888 图标，写入 shared/app_icons.rgba（裸像素，行主序）。launcher
启动时从 /share/app_icons.rgba 读取（不内嵌 ELF——263KB 会顶过内核 execve
的 EXEC_BUF_CAP）。

字形来源：fonts/Phosphor-Fill.ttf（Phosphor 图标字体，实心风格）。每个应用
的图标码点取自 shared/icons.rs 的 `app` 模块（source of truth，key 与
launcher APPS 表一致）——运行期 emoji/UI 图标也用同一套码点表。

渲染方式：Phosphor 矢量轮廓经 fontTools 展平（quadratic/cubic 采样），
56 单位坐标系按 SS=8 倍超采样（448px）画在独立 alpha 蒙版上（轮廓逐层
XOR = even-odd 填充，自带挖洞），背景为纵向渐变圆角矩形；RGB 与 alpha
分通道 LANCZOS 缩到 56px，外缘抗锯齿无黑边。重跑：
    python3 tools/make_icons.py
生成文件随源码提交，构建不需要 Python。

依赖：Pillow（pip install pillow）、fontTools（pip install fonttools）。
预览拼图输出到 tubiao/icons_preview.png。
"""

import re
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw
from fontTools.pens.basePen import BasePen
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent
DST = ROOT / "shared" / "app_icons.rgba"
PREVIEW = ROOT / "tubiao" / "icons_preview.png"
ICONS_RS = ROOT / "shared" / "icons.rs"
PHOSPHOR_FILL = ROOT / "fonts" / "Phosphor-Fill.ttf"

ICON = 56          # 必须与 shared/app_icons_sprite.rs 的 ICON_PX 一致
SS = 8             # 超采样倍率
CANVAS = ICON * SS
RADIUS = 17        # 圆角 30%（与 launcher icon_radius(56) 一致）
TARGET_INK = 38    # 字形最大边（56 单位空间）——墨迹缩放到该尺寸后居中

# 与 apps/launcher/src/main.rs 的 APPS 表严格同序（key 仅作校验）。
# (key, 渐变顶色, 渐变底色)
APPS = [
    ("phone",    0x4F, 0xB2, 0xFF, 0x0A, 0x6C, 0xFF),
    ("sms",      0x3E, 0xDC, 0x7C, 0x0F, 0xAF, 0x52),
    ("contacts", 0xB4, 0x70, 0xE8, 0x7E, 0x3C, 0xB8),
    ("settings", 0xA8, 0xB4, 0xC0, 0x5C, 0x6B, 0x7A),
    ("files",    0xFF, 0xD7, 0x5E, 0xF0, 0xA8, 0x18),
    ("browser",  0xFF, 0xA2, 0x4D, 0xF2, 0x6E, 0x0C),
    ("calc",     0xFF, 0xD3, 0x4E, 0xE8, 0xA8, 0x0A),
    ("calendar", 0xFF, 0x7A, 0x6E, 0xE0, 0x3E, 0x31),
    ("camera",   0x6B, 0x7F, 0x92, 0x33, 0x47, 0x5A),
    ("clock",    0x7E, 0x93, 0xA8, 0x4A, 0x5B, 0x6A),
    ("gallery",  0xD0, 0x8C, 0xF0, 0x94, 0x40, 0xC8),
    ("music",    0xFF, 0xA9, 0x4F, 0xED, 0x71, 0x17),
    ("mail",     0x55, 0xAA, 0xEF, 0x22, 0x73, 0xC8),
    ("map",      0x3B, 0xDA, 0x82, 0x1F, 0xA9, 0x58),
    ("weather",  0x9F, 0xB4, 0xC7, 0x64, 0x7C, 0x8E),
    ("store",    0xB0, 0x6B, 0xD8, 0x7A, 0x2F, 0xA8),
    ("terminal", 0x4A, 0x4A, 0x4A, 0x10, 0x10, 0x10),
    ("cloud",    0x6B, 0xC8, 0xF5, 0x2E, 0x93, 0xD6),
    ("video",    0xFF, 0x6D, 0x62, 0xD6, 0x33, 0x2C),
    ("notes",    0x4D, 0xC8, 0xC0, 0x1E, 0x9E, 0x96),
    ("lcapp",    0xFF, 0x7E, 0xB3, 0xE8, 0x44, 0x7F),
]


# ---------- shared/icons.rs 解析（图标码点 source of truth） ----------

def read_app_icons():
    """解析 icons.rs：返回 {launcher key: PUA 码点}。"""
    text = ICONS_RS.read_text(encoding="utf-8")
    consts = {name: int(cp, 16) for name, cp in re.findall(
        r"pub const ([A-Z0-9_]+): u32 = 0x([0-9A-Fa-f]{4});", text)}
    icons = {}
    for key, const in re.findall(r'b"([a-z0-9_]+)"\s*=>\s*app::([A-Z0-9_]+)', text):
        if const not in consts:
            raise SystemExit(f"icons.rs: app::{const} 常量缺失")
        icons[key] = consts[const]
    return icons


# ---------- Phosphor 字形 → 蒙版（轮廓展平 + even-odd 填充） ----------

class FlattenPen(BasePen):
    """把 TrueType 轮廓展平为多边形点列（字体单位）。quadratic/cubic 按弦长
    自适应采样，SS=8（448px 画布）下重采样误差不可见。"""

    STEPS_MAX = 48

    def __init__(self, glyphSet):
        super().__init__(glyphSet)
        self.contours = []
        self._cur = None

    def _moveTo(self, p):
        self._cur = [p]
        self.contours.append(self._cur)

    def _lineTo(self, p):
        self._cur.append(p)

    def _curveToOne(self, p1, p2, p3):
        # 三次 Bezier
        x0, y0 = self._cur[-1]
        n = self._steps(x0, y0, p3[0], p3[1])
        for i in range(1, n):
            t = i / n
            mt = 1 - t
            x = (mt ** 3 * x0 + 3 * mt * mt * t * p1[0]
                 + 3 * mt * t * t * p2[0] + t ** 3 * p3[0])
            y = (mt ** 3 * y0 + 3 * mt * mt * t * p1[1]
                 + 3 * mt * t * t * p2[1] + t ** 3 * p3[1])
            self._cur.append((x, y))
        self._cur.append(p3)

    def _qCurveToOne(self, p1, p2):
        # 二次 Bezier（BasePen 已拆解多段 off-curve 的隐式控制点）
        x0, y0 = self._cur[-1]
        n = self._steps(x0, y0, p2[0], p2[1])
        for i in range(1, n):
            t = i / n
            mt = 1 - t
            x = mt * mt * x0 + 2 * mt * t * p1[0] + t * t * p2[0]
            y = mt * mt * y0 + 2 * mt * t * p1[1] + t * t * p2[1]
            self._cur.append((x, y))
        self._cur.append(p2)

    def _steps(self, x0, y0, x1, y1):
        chord = ((x1 - x0) ** 2 + (y1 - y0) ** 2) ** 0.5
        return max(2, min(self.STEPS_MAX, int(chord / 16) + 2))

    def _closePath(self):
        pass


_font = None
_glyphset = None
_upem = None


def _load_font():
    global _font, _glyphset, _upem
    if _font is None:
        _font = TTFont(PHOSPHOR_FILL)
        _glyphset = _font.getGlyphSet()
        _upem = _font["head"].unitsPerEm
    return _glyphset, _upem


def glyph_mask(cp, canvas, k, cx, cy):
    """把 Phosphor 字形 cp 渲染到 canvas×canvas 的 L 蒙版（白色=覆盖）。

    k：像素/字体单位；字形墨迹包围盒中心对齐到 (cx, cy)。
    轮廓逐层 XOR（even-odd 填充）：嵌套轮廓自然成为挖洞。
    """
    glyphset, upem = _load_font()
    name = _font.getBestCmap().get(cp)
    if name is None:
        raise SystemExit(f"Phosphor-Fill.ttf 缺少码点 0x{cp:04X}（检查 shared/icons.rs）")

    # 包围盒（字体单位）→ 缩放与居中
    from fontTools.pens.boundsPen import BoundsPen
    bp = BoundsPen(glyphset)
    glyphset[name].draw(bp)
    if bp.bounds is None:
        raise SystemExit(f"码点 0x{cp:04X} 无轮廓")
    bx0, by0, bx1, by1 = bp.bounds

    pen = FlattenPen(glyphset)
    glyphset[name].draw(pen)

    ox = cx - (bx0 + bx1) / 2 * k
    oy = cy + (by0 + by1) / 2 * k

    mask = Image.new("1", (canvas, canvas), 0)
    for contour in pen.contours:
        if len(contour) < 3:
            continue
        pts = [(ox + x * k, oy - y * k) for x, y in contour]
        layer = Image.new("1", (canvas, canvas), 0)
        ImageDraw.Draw(layer).polygon(pts, fill=1)
        mask = ImageChops.logical_xor(mask, layer)
    return mask.convert("L")


# ---------- 贴图合成 ----------

def draw_icon(entry, cp):
    """单个图标 → 56x56 RGBA（Image 对象）。"""
    _, tr, tg, tb, br, bg, bb = entry
    # 背景渐变画满整个画布（不留透明区），缩放后外缘颜色外扩无黑边；
    # 圆角形状由单独的 alpha 蒙版决定。
    rgb = Image.new("RGB", (CANVAS, CANVAS))
    px = rgb.load()
    for y in range(CANVAS):
        t = y / (CANVAS - 1)
        r = round(tr + (br - tr) * t)
        g = round(tg + (bg - tg) * t)
        b = round(tb + (bb - tb) * t)
        for x in range(CANVAS):
            px[x, y] = (r, g, b)

    # 字形墨迹尺寸：最大边 → TARGET_INK 单位（×SS 像素），等比缩放
    glyphset, upem = _load_font()
    name = _font.getBestCmap().get(cp)
    from fontTools.pens.boundsPen import BoundsPen
    bp = BoundsPen(glyphset)
    glyphset[name].draw(bp)
    ink = max(bp.bounds[2] - bp.bounds[0], bp.bounds[3] - bp.bounds[1])
    k = TARGET_INK * SS / ink

    # 白色字形蒙版（挖洞由 even-odd 自带）
    mask = glyph_mask(cp, CANVAS, k, CANVAS / 2, CANVAS / 2)
    rgb.paste((255, 255, 255), mask=mask)

    # 圆角矩形 alpha 蒙版
    alpha = Image.new("L", (CANVAS, CANVAS), 0)
    ImageDraw.Draw(alpha).rounded_rectangle(
        [0, 0, CANVAS - 1, CANVAS - 1], radius=RADIUS * SS, fill=255)

    rgb = rgb.resize((ICON, ICON), Image.LANCZOS)
    alpha = alpha.resize((ICON, ICON), Image.LANCZOS)
    out = rgb.convert("RGBA")
    out.putalpha(alpha)
    return out


def main():
    app_icons = read_app_icons()
    for entry in APPS:
        if entry[0] not in app_icons:
            raise SystemExit(f"icons.rs 缺少应用图标 key: {entry[0]}")
    icons = [draw_icon(e, app_icons[e[0]]) for e in APPS]

    # 写 shared/app_icons.rgba（RGBA8888 裸像素，行主序，应用间紧邻）
    DST.parent.mkdir(parents=True, exist_ok=True)
    with open(DST, "wb") as f:
        for im in icons:
            f.write(im.tobytes())
    total = ICON * ICON * 4 * len(icons)
    assert DST.stat().st_size == total, "贴图字节数不符"
    print(f"[make_icons] 写出 {DST}（{len(icons)} 个 {ICON}x{ICON} RGBA，{total} 字节）")

    # 预览拼图：深色渐变底 + 2x 放大网格（7 列），标注顺序即 APPS 表下标
    cols, pad, cell = 7, 18, ICON * 2
    rows = (len(icons) + cols - 1) // cols
    W = cols * cell + (cols + 1) * pad
    H = rows * cell + (rows + 1) * pad
    board = Image.new("RGB", (W, H), (24, 28, 36))
    d = ImageDraw.Draw(board)
    for i, im in enumerate(icons):
        r, c = divmod(i, cols)
        x = pad + c * (cell + pad)
        y = pad + r * (cell + pad)
        big = im.resize((cell, cell), Image.BILINEAR)
        board.paste(big, (x, y), big)
        d.text((x + cell // 2 - 8, y + cell + 3), APPS[i][0], fill=(180, 190, 200))
    PREVIEW.parent.mkdir(parents=True, exist_ok=True)
    board.save(PREVIEW)
    print(f"[make_icons] 预览 {PREVIEW}")


if __name__ == "__main__":
    main()
