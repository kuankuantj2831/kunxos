#!/usr/bin/env python3
"""构建 .kunx 应用包（KunxOS 原生格式）。

用法：
  python3 tools/kunxrepo/mkkunx.py \
      --manifest manifest.json \
      --elf build/calculator \
      --res icon.bin=res/icon.bin \
      --data prefs.json=data/prefs.json \
      --key keys/kunx-dev.key --cert keys/kunx-dev.cert \
      --out calculator-1.0.0.kunx

--res / --data 可重复：格式为 <安装名>=<源文件>。
manifest.json 字段：
  name, version, api_level, main("bin/<name>"), description, author,
  permissions[], dependencies{name: constraint}, provides[], conflicts[]
"""
import argparse
import json
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ed25519

MAGIC = b"KUNXPKG1"
FORMAT_VERSION = 1
FLAG_SIGNED = 1
HEADER_LEN = 48
SIG_LEN = 64
PATH_MAX = 128


def put_u32(b, off, v):
    b[off:off + 4] = struct.pack("<I", v)


def build(manifest_bytes, entries, certs, sign_sk):
    """entries: [(path:str, data:bytes)]；certs: [bytes]（leaf 在前）。
    sign_sk：开发者私钥，用于对 [0, sig_off) 签名。"""
    payload = b"".join(
        struct.pack("<I", len(p)) + p.encode() + struct.pack("<I", len(d)) + d
        for p, d in entries
    )
    meta = b"".join(certs)
    manifest_off = HEADER_LEN
    payload_off = manifest_off + len(manifest_bytes)
    meta_off = payload_off + len(payload)
    sig_off = meta_off + len(meta)
    total = sig_off + SIG_LEN

    out = bytearray(total)
    out[0:8] = MAGIC
    put_u32(out, 8, FORMAT_VERSION)
    put_u32(out, 12, FLAG_SIGNED)
    put_u32(out, 16, manifest_off)
    put_u32(out, 20, len(manifest_bytes))
    put_u32(out, 24, payload_off)
    put_u32(out, 28, len(payload))
    put_u32(out, 32, meta_off)
    put_u32(out, 36, len(meta))
    put_u32(out, 40, sig_off)
    put_u32(out, 44, SIG_LEN)
    out[manifest_off:manifest_off + len(manifest_bytes)] = manifest_bytes
    out[payload_off:payload_off + len(payload)] = payload
    out[meta_off:meta_off + len(meta)] = meta
    # 签名覆盖 [0, sig_off)
    out[sig_off:] = ed25519.sign(sign_sk, bytes(out[:sig_off]))
    return bytes(out)


def load_key(path):
    with open(path, "rb") as f:
        return f.read()


def main():
    global args
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--elf", required=True, help="入口 ELF（将安装为 bin/<name>）")
    ap.add_argument("--res", action="append", default=[], help="资源：安装名=源文件")
    ap.add_argument("--data", action="append", default=[], help="预置数据：安装名=源文件")
    ap.add_argument("--key", required=True)
    ap.add_argument("--cert", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    with open(args.manifest, "rb") as f:
        manifest_bytes = f.read()
    manifest = json.loads(manifest_bytes)
    name = manifest.get("name")
    if not name or "/" in name:
        sys.exit("manifest 缺少合法 name")

    entries = []
    with open(args.elf, "rb") as f:
        entries.append((f"bin/{name}", f.read()))
    for r in args.res:
        inst, src = r.split("=", 1)
        with open(src, "rb") as f:
            entries.append((f"res/{inst}", f.read()))
    for d in args.data:
        inst, src = d.split("=", 1)
        with open(src, "rb") as f:
            entries.append((f"data/{inst}", f.read()))

    with open(args.cert, "rb") as f:
        cert = f.read()

    pkg = build(manifest_bytes, entries, [cert], load_key(args.key))

    with open(args.out, "wb") as f:
        f.write(pkg)
    print(f"[mkkunx] {args.out}: {len(pkg)} bytes, name={name}, version={manifest.get('version')}")
    print(f"[mkkunx] entries: {[p for p, _ in entries]}")


if __name__ == "__main__":
    main()
