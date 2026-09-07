#!/usr/bin/env python3
"""KunxOS JPEG 编码器输出验证。

用法：
  python3 tests/scripts/verify_jpeg.py <jpg> [<宽> <高>] [--expect-errors <n>]

行为：
  - 有 Pillow：解码并做结构检查；对「彩条测试图」断言色带内部颜色误差；
    纯结构文件只做标记/尺寸检查。
  - 无 Pillow：退化为标记结构检查（SOI/APP0-JFIF/SOF0 尺寸/SOS/EOI/0xFF 填充）。
退出码 0 = 通过。
"""
import struct
import sys


def structural_check(data, w=None, h=None):
    assert data[:2] == b"\xff\xd8", "缺少 SOI"
    assert data[-2:] == b"\xff\xd9", "缺少 EOI"
    pos = 2
    saw_sof0 = saw_sos = False
    entropy_start = None
    while pos + 4 <= len(data):
        assert data[pos] == 0xFF, f"标记对齐错误 @{pos}"
        m = data[pos + 1]
        if m == 0xD8:
            pos += 2
            continue
        if m == 0xD9:
            break
        ln = struct.unpack(">H", data[pos + 2 : pos + 4])[0]
        assert ln >= 2, f"段长度异常 @{pos}"
        if m == 0xE0:
            assert data[pos + 4 : pos + 9] == b"JFIF\0", "APP0 缺 JFIF"
        if m == 0xC0:
            saw_sof0 = True
            hh, ww = struct.unpack(">HH", data[pos + 5 : pos + 9])
            if w is not None:
                assert hh == h and ww == w, f"SOF0 尺寸 {ww}x{hh} 不符 {w}x{h}"
        if m == 0xDA:
            saw_sos = True
            entropy_start = pos + 2 + ln
            break
        pos += 2 + ln
    assert saw_sof0 and saw_sos, "缺少 SOF0/SOS"
    # 熵数据 0xFF 填充检查
    i = entropy_start
    found_eoi = False
    while i + 1 < len(data):
        if data[i] == 0xFF:
            if data[i + 1] == 0xD9:
                found_eoi = True
                break
            assert data[i + 1] == 0x00, f"熵数据 0xFF 未填充 @{i}"
            i += 2
        else:
            i += 1
    assert found_eoi, "熵数据中未找到 EOI"


def verify(path, w=None, h=None):
    data = open(path, "rb").read()
    structural_check(data, w, h)
    try:
        from PIL import Image
    except ImportError:
        print(f"PASS (结构) {path} ({len(data)}B)")
        return 0
    im = Image.open(path).convert("RGB")
    iw, ih = im.size
    if w is not None:
        assert (iw, ih) == (w, h), f"解码尺寸 {iw}x{ih} 不符 {w}x{h}"
    # 彩条测试图验证：7 段色带内部颜色
    bands = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0),
             (0, 255, 255), (255, 0, 255), (128, 128, 128)]
    max_err = 0
    for i, (r, g, b) in enumerate(bands):
        # 带内采样（避开边界混合区）：x = 带起点 + 30px
        x = int(i * iw / 7) + 30
        y = ih // 2
        px = im.getpixel((x, y))
        err = max(abs(px[j] - (r, g, b)[j]) for j in range(3))
        max_err = max(max_err, err)
    limit = 48
    status = "PASS" if max_err <= limit else "FAIL"
    print(f"{status} (解码) {path} {iw}x{ih} {len(data)}B 色带最大误差={max_err} (限 {limit})")
    return 0 if max_err <= limit else 1


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    w = h = None
    if len(args) >= 3:
        w, h = int(args[1]), int(args[2])
    sys.exit(verify(args[0], w, h))
