#!/usr/bin/env python3
"""KXSI v1 签名镜像打包/签名/验签（主机侧）。

格式与 kunxsec/src/kxsi.rs 一致（单源），私钥经 tools/kunxrepo/ed25519.py
（Ed25519 签名由 Python 实现，与设备端验签算法同构）。

用法：
    kxsi_build.py --role <u32> --version <n> --entry <hex> \\
        --in <payload_file> --key <boot.sk> -o <out.IMG>
    kxsi_build.py --verify <img> --pk <boot.pk> [--role n]

角色：2=内核 3=DTB 4=ramdisk 5=恢复ramdisk 6=系统镜像包
"""
import argparse
import hashlib
import struct
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kunxrepo"))
from ed25519 import sign, verify  # noqa: E402

MAGIC = b"KXIMGKV1"
HEADER = 80
SIG = 64


def build(payload: bytes, role: int, version: int, sk: bytes, entry: int = 0,
          timestamp: int = 0):
    if timestamp == 0:
        timestamp = 1800000000
    hdr = bytearray(80)
    hdr[0:8] = MAGIC
    struct.pack_into("<I", hdr, 8, 0)
    struct.pack_into("<I", hdr, 12, role)
    struct.pack_into("<I", hdr, 16, version)
    struct.pack_into("<Q", hdr, 20, timestamp)
    struct.pack_into("<I", hdr, 28, 0)
    struct.pack_into("<I", hdr, 32, 80)
    struct.pack_into("<I", hdr, 36, len(payload))
    hdr[40:72] = hashlib.sha256(payload).digest()
    struct.pack_into("<Q", hdr, 72, entry)
    body = bytes(hdr) + payload
    sig = sign(sk, body)
    return body + sig


def parse(blob: bytes):
    if len(blob) < HEADER + SIG or blob[0:8] != MAGIC:
        return None
    if struct.unpack_from("<I", blob, 8)[0] != 0:
        return None
    poff = struct.unpack_from("<I", blob, 32)[0]
    plen = struct.unpack_from("<I", blob, 36)[0]
    if poff != 80 or poff + plen > len(blob) - SIG:
        return None
    return {
        "role": struct.unpack_from("<I", blob, 12)[0],
        "version": struct.unpack_from("<I", blob, 16)[0],
        "timestamp": struct.unpack_from("<Q", blob, 20)[0],
        "payload_off": poff,
        "payload_len": plen,
        "payload_sha256": bytes(blob[40:72]),
        "entry": struct.unpack_from("<Q", blob, 72)[0],
    }


def verify_blob(blob: bytes, pk: bytes, role=0):
    info = parse(blob)
    if info is None:
        return None
    if role and info["role"] != role:
        return None
    pay = blob[info["payload_off"]:info["payload_off"] + info["payload_len"]]
    if hashlib.sha256(pay).digest() != info["payload_sha256"]:
        return None
    sig = blob[-SIG:]
    if not verify(pk, blob[:-SIG], sig):
        return None
    return info


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--role", type=int)
    ap.add_argument("--version", type=int)
    ap.add_argument("--entry", type=lambda s: int(s, 0), default=0)
    ap.add_argument("--in", dest="inp")
    ap.add_argument("--key")
    ap.add_argument("--pk")
    ap.add_argument("-o", "--out")
    ap.add_argument("--verify", nargs="?", const="__self__")
    args = ap.parse_args()

    if args.verify:
        blob = open(args.key if args.verify == "__self__" else args.verify, "rb").read()
        if args.verify == "__self__":
            # 自验模式：--in 为镜像、--pk 为公钥
            blob = open(args.inp, "rb").read()
            pk = open(args.pk, "rb").read()
        else:
            blob = open(args.verify, "rb").read()
            pk = open(args.pk, "rb").read()
        info = verify_blob(blob, pk, args.role or 0)
        if info is None:
            print("verify: FAIL")
            sys.exit(1)
        print("verify: OK role=%d version=%d entry=0x%x payload=%d" %
              (info["role"], info["version"], info["entry"], info["payload_len"]))
        sys.exit(0)

    if not (args.inp and args.key and args.out):
        ap.error("需 --in --key -o（或 --verify）")
    payload = open(args.inp, "rb").read()
    sk = open(args.key, "rb").read()
    blob = build(payload, args.role or 0, args.version or 0, sk, args.entry)
    open(args.out, "wb").write(blob)
    print("kxsi_build: wrote %s (%d bytes, role=%s version=%s)" %
          (args.out, len(blob), args.role, args.version))


if __name__ == "__main__":
    main()