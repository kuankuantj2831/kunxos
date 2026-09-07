#!/usr/bin/env python3
"""KXUP v1 OTA 更新包打包/验签（主机侧）。格式与 kunxsec/src/kxup.rs 一致。

组件：全量（CF_FULL，data 区为完整 KXSI 镜像）或增量（CF_DELTA，data 区为
KXDELTA 补丁，应用后得到 KXSI 镜像）。整包用 ota 私钥签名（Ed25519）。

用法：
    kxup_build.py --manifest <manifest.txt> --target-version N --base-version M \\\\
        --full role=file  ...     # role: 2 内核 / 3 DTB / 4 ramdisk / 5 恢复ramdisk
        --delta role=patch.kdelta ...\n        --key ota.sk -o pkg.kxup\n        --pk ota.pk  (可选：验签)\n    kxup_build.py --verify pkg.kxup --pk ota.pk --boot-pk boot.pk\n"""
import argparse
import hashlib
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kunxrepo"))
from ed25519 import sign, verify  # noqa: E402

MAGIC = b"KXUPDv10"
HEADER = 40
COMPONENT_HEADER = 16
SIG = 64
CF_DELTA = 1 << 0
CF_FULL = 1 << 1
ROLE_SYSTEM_PKG = 6


def build(manifest, components, ota_sk):
    """components: list of (role, flags, data_bytes)."""
    b = bytearray()
    b += MAGIC
    b += (1).to_bytes(4, "little")
    b += ROLE_SYSTEM_PKG.to_bytes(4, "little")
    b += (0).to_bytes(4, "little")  # target_version 由 manifest 冗余校验
    b += (0).to_bytes(4, "little")  # base_version 同上
    b += (0).to_bytes(4, "little")  # flags
    b += (0).to_bytes(4, "little")  # manifest_len（占位）
    b += (0).to_bytes(4, "little")  # components_count（占位）
    b += (0).to_bytes(4, "little")  # reserved
    for (role, flags, data) in components:
        b += role.to_bytes(4, "little")
        b += len(data).to_bytes(4, "little")
        b += (0).to_bytes(4, "little")  # delta_len（占位：与 flags 互斥）
        b += flags.to_bytes(4, "little")
        b += data
    b += manifest
    # 回填头部
    b[20:24] = (0).to_bytes(4, "little")
    b[24:28] = (0).to_bytes(4, "little")
    b[28:32] = len(manifest).to_bytes(4, "little")
    b[32:36] = len(components).to_bytes(4, "little")
    sig = sign(ota_sk, bytes(b))
    return bytes(b) + sig


def parse_and_verify(blob, ota_pk, boot_pk=None):
    if len(blob) < HEADER + SIG or blob[0:8] != MAGIC:
        return None
    manifest_len = int.from_bytes(blob[28:32], "little")
    comp_count = int.from_bytes(blob[32:36], "little")
    p = HEADER
    comps = []
    for _ in range(comp_count):
        if p + COMPONENT_HEADER > len(blob) - SIG:
            return None
        role = int.from_bytes(blob[p:p + 4], "little")
        data_len = int.from_bytes(blob[p + 4:p + 8], "little")
        flags = int.from_bytes(blob[p + 12:p + 16], "little")
        d = p + COMPONENT_HEADER
        if d + data_len > len(blob) - SIG:
            return None
        comps.append((role, flags, d, data_len))
        p = d + data_len
    manifest = blob[p:p + manifest_len]
    sig_off = p + manifest_len
    if sig_off + SIG != len(blob):
        return None
    if not verify(ota_pk, blob[:sig_off], blob[sig_off:]):
        return None
    if boot_pk:
        for (role, flags, d, data_len) in comps:
            if flags & CF_FULL:
                data = blob[d:d + data_len]
                if data[0:8] != b"KXIMGKV1" or not verify(boot_pk, data[:-64], data[-64:]):
                    return None
    return {"manifest": manifest, "components": comps}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest")
    ap.add_argument("--target-version", type=int)
    ap.add_argument("--base-version", type=int, default=0)
    ap.add_argument("--full", action="append", default=[], metavar="role=file")
    ap.add_argument("--delta", action="append", default=[], metavar="role=patch")
    ap.add_argument("--key")
    ap.add_argument("--pk")
    ap.add_argument("--boot-pk")
    ap.add_argument("-o", "--out")
    ap.add_argument("--verify", metavar="pkg")
    args = ap.parse_args()

    if args.verify:
        blob = open(args.verify, "rb").read()
        ota_pk = open(args.pk, "rb").read()
        boot = open(args.boot_pk, "rb").read() if args.boot_pk else None
        info = parse_and_verify(blob, ota_pk, boot)
        if info is None:
            print("kxup verify: FAIL")
            sys.exit(1)
        print("kxup verify: PASS manifest=%d bytes, components=%d" %
              (len(info["manifest"]), len(info["components"])))
        sys.exit(0)

    if not (args.manifest and args.key and args.out):
        ap.error("需 --manifest --key -o（或 --verify）")
    manifest = open(args.manifest, "rb").read()
    ota_sk = open(args.key, "rb").read()
    if args.target_version is None:
        ap.error("需 --target-version（或 manifest 含 target_version）")
    # 用 manifest 内 target_version/base_version 回填头部
    comps = []
    for spec in args.full:
        role_str, _, path = spec.partition("=")
        role = int(role_str)
        data = open(path, "rb").read()
        comps.append((role, CF_FULL, data))
    for spec in args.delta:
        role_str, _, path = spec.partition("=")
        role = int(role_str)
        data = open(path, "rb").read()
        comps.append((role, CF_DELTA, data))
    blob = build(manifest, comps, ota_sk)
    open(args.out, "wb").write(blob)
    print("kxup_build: wrote %s (%d bytes, %d components)" % (args.out, len(blob), len(comps)))


if __name__ == "__main__":
    main()