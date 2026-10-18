#!/usr/bin/env python3
"""VNC(RFB3.x) 抓帧工具：连接 QEMU VNC（如 `-vnc :0` → 127.0.0.1:5900），
请求全帧原始像素并保存 PNG。无第三方依赖（PNG 用 zlib 手工打包）。

用法：python3 tools/vnc_shot.py [输出.png] [端口]
默认输出 /tmp/kunxos-vnc.png，端口 5900。用于 GUI 冒烟：串口无 PANIC ≠
画面正常，凡渲染/合成改动必须抓帧确认。
"""
import socket
import struct
import sys
import zlib


def recvn(s: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("VNC 连接中断")
        buf += chunk
    return buf


def png_write(path: str, w: int, h: int, rgb: bytes) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    raw = b"".join(b"\x00" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kunxos-vnc.png"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 5900
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    # 版本握手：回显服务器版本
    s.sendall(recvn(s, 12))
    # 安全协商（3.7+：数量+类型表；3.3：直接 4 字节安全码）
    ntypes = recvn(s, 1)[0]
    if ntypes == 0:
        recvn(s, 4)
    else:
        types = recvn(s, ntypes)
        if 1 not in types:  # 1 = None
            raise SystemExit(f"VNC 需要认证，不支持免密类型: {types!r}")
        s.sendall(b"\x01")
        if struct.unpack(">I", recvn(s, 4))[0] != 0:
            raise SystemExit("VNC 安全握手失败")
    # ClientInit（共享连接）→ ServerInit（帧尺寸 + 像素格式 + 桌面名）
    s.sendall(b"\x01")
    w, h = struct.unpack(">HH", recvn(s, 4))
    pf = recvn(s, 16)
    bpp, depth, be = pf[0], pf[1], pf[2]
    rm, gm, bm = struct.unpack(">HHH", pf[4:10])
    rs, gs, bs = pf[10], pf[11], pf[12]
    namelen = struct.unpack(">I", recvn(s, 4))[0]
    name = recvn(s, namelen).decode("latin1", "replace")
    if bpp not in (16, 32):
        raise SystemExit(f"不支持的像素格式 bpp={bpp} depth={depth}")
    # 只要 raw(0) 编码，非增量请求全帧
    s.sendall(struct.pack(">Bxh", 2, 1) + struct.pack(">i", 0))
    s.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))
    canvas = bytearray(w * h * 3)
    byteorder = "big" if be else "little"
    done = False
    while not done:
        msg = recvn(s, 1)[0]
        if msg == 0:  # FramebufferUpdate
            recvn(s, 1)
            nrects = struct.unpack(">H", recvn(s, 2))[0]
            for _ in range(nrects):
                rx, ry, rw, rh = struct.unpack(">HHHH", recvn(s, 8))
                enc = struct.unpack(">i", recvn(s, 4))[0]
                if enc != 0:
                    raise SystemExit(f"收到非 raw 编码 {enc}，无法解析")
                data = recvn(s, rw * rh * bpp // 8)
                pxw = bpp // 8
                for row in range(rh):
                    base = (ry + row) * w * 3
                    for col in range(rw):
                        off = (row * rw + col) * pxw
                        v = int.from_bytes(data[off:off + pxw], byteorder)
                        r = ((v >> rs) & rm) * 255 // max(rm, 1)
                        g = ((v >> gs) & gm) * 255 // max(gm, 1)
                        b = ((v >> bs) & bm) * 255 // max(bm, 1)
                        o = base + (rx + col) * 3
                        canvas[o:o + 3] = bytes((r, g, b))
            done = True
        elif msg == 1:  # SetColourMapEntries
            recvn(s, 3)
            recvn(s, struct.unpack(">H", recvn(s, 2))[0] * 6)
        elif msg == 2:  # Bell
            pass
        elif msg == 3:  # ServerCutText
            recvn(s, 3)
            recvn(s, struct.unpack(">I", recvn(s, 4))[0])
        else:
            raise SystemExit(f"未知 RFB 消息类型 {msg}")
    png_write(out, w, h, bytes(canvas))
    print(f"已保存 {out}（{w}x{h}，桌面名「{name}」）")


if __name__ == "__main__":
    main()
