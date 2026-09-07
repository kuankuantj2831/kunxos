#!/usr/bin/env python3
"""KunxOS 安全链主机自检（镜像级）：KXSI / KXUP / KXDELTA / OTP 一致性与篡改拒绝。

用法：
    selftest.py                # 全量自检（demo 密钥）
    selftest.py --img F        # 校验单个 KXSI 镜像 F（--pk）
    selftest.py --pkg F        # 校验 KXUP 包 F（--ota-pk --boot-pk）
"""
import argparse
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

TOOLS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kunxrepo")
sys.path.insert(0, TOOLS)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ed25519 import sign, verify  # noqa: E402
import bsdiff_gen  # noqa: E402
import otp_dump  # noqa: E402


def demo_keys():
    kd = os.path.join(os.path.dirname(os.path.abspath(__file__)), "keys")
    boot_sk = open(os.path.join(kd, "boot.sk"), "rb").read()
    boot_pk = open(os.path.join(kd, "boot.pk"), "rb").read()
    ota_pk = open(os.path.join(kd, "ota.pk"), "rb").read()
    return boot_sk, boot_pk, ota_pk


def kxsi_blob(payload, role, version, sk, entry=0):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import kxsi_build  # noqa: E402

    return kxsi_build.build(payload, role, version, sk, entry)


def self_check_kxsi():
    boot_sk, boot_pk, _ = demo_keys()
    blob = kxsi_blob(b"kernel-v42-payload", 2, 42, boot_sk, 0x40080000)
    # 结构 + 签名
    assert blob[0:8] == b"KXIMGKV1"
    assert struct.unpack_from("<Q", blob, 72)[0] == 0x40080000
    assert verify(boot_pk, blob[:-64], blob[-64:])
    # 篡改载荷 / 签名 / role
    bad = bytearray(blob); bad[90] ^= 1
    assert not verify(boot_pk, bytes(bad)[:-64], bytes(bad)[-64:])
    bad = bytearray(blob); bad[-1] ^= 1
    assert not verify(boot_pk, bytes(bad)[:-64], bytes(bad)[-64:])
    bad = bytearray(blob); bad[12:16] = (3).to_bytes(4, "little")
    assert not verify(boot_pk, bytes(bad)[:-64], bytes(bad)[-64:])
    print("selftest: KXSI PASS")


def self_check_delta():
    old = bytes([0x41 + i % 3 for i in range(500)])
    new = bytearray(old)
    for i in range(0, len(new), 10):
        new[i] ^= 0x80
    new = bytes(new)
    pt = bsdiff_gen.make_delta(old, new)
    assert bsdiff_gen.apply_delta(old, pt) == new
    print("selftest: KXDELTA PASS (patch %d bytes for %d)" % (len(pt), len(new)))
    # 错误 patch 拒绝
    assert bsdiff_gen.apply_delta(b"abc", b"KXDELTA1!!") is None
    print("selftest: KXDELTA tamper-reject PASS")


def self_check_otp():
    # 复用 otp_dump 的纯函数：fresh + burn + floor 语义
    raw = otp_dump.fresh()
    assert otp_dump.floor_of(raw) == 0
    assert otp_dump.burn_flags(raw, 1)
    assert otp_dump.secure_enabled(raw)
    assert otp_dump.burn_boot_pk(raw, b"\xAB" * 32)
    assert otp_dump.burn_dev_key(raw, b"\x11" * 32)
    assert otp_dump.bump_floor(raw, 9)
    assert otp_dump.floor_of(raw) == 9
    assert not otp_dump.bump_floor(raw, 5)
    print("selftest: OTP PASS")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--img")
    ap.add_argument("--pkg")
    ap.add_argument("--pk")
    ap.add_argument("--ota-pk")
    ap.add_argument("--boot-pk")
    args = ap.parse_args()
    if args.img:
        import kxsi_build
        blob = open(args.img, "rb").read()
        pk = open(args.pk, "rb").read()
        ok = kxsi_build.verify_blob(blob, pk) is not None
        print("verify %s: %s" % (args.img, "PASS" if ok else "FAIL"))
        sys.exit(0 if ok else 1)
    if args.pkg:
        import kxup_build
        ota_pk = open(args.ota_pk, "rb").read()
        boot_pk = open(args.boot_pk, "rb").read()
        blob = open(args.pkg, "rb").read()
        ok = kxup_build.parse_and_verify(blob, ota_pk, boot_pk) is not None
        print("verify %s: %s" % (args.pkg, "PASS" if ok else "FAIL"))
        sys.exit(0 if ok else 1)
    self_check_kxsi()
    self_check_otp()
    self_check_delta()
    print("selftest: ALL PASSED")


if __name__ == "__main__":
    main()