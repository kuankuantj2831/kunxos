#!/usr/bin/env python3
"""KunxOS OTP（eFUSE）区：纯函数封装 + 磁盘镜像查看。

与 kunxsec/src/otp.rs 格式一致：
  @0 magic "KXOTPv10"  @8 version  @12 flags(bit0=secure-boot)  @16 boot_pk_sha256[32]
  @48 device_key[32]  @112 floor_fuses[64]（1→0 熔断位数 = 楼层）

用法：
    otp_dump.py --disk disk.img            # 打印磁盘 misc 区 OTP + BCB 摘要
    otp_dump.py --raw otp.bin              # 打印 4KB OTP 原始区
"""
import argparse
import struct
import sys
import os

OTP_SIZE = 2048
MAGIC = b"KXOTPv10"
FLAG_SECURE_BOOT = 1 << 0
OFF_FLAGS = 12
OFF_BOOT_PK = 16
OFF_DEV = 48
OFF_FLOOR = 112
FLOOR_LEN = 64
FLOOR_MAX = FLOOR_LEN * 8

# 磁盘 misc 几何（与 tools/mkdisk.py 一致）
MISC_START = 1_042_432
BCB_LBA = MISC_START
OTP_LBA = MISC_START + 2


def crc32(data):
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            mask = -(crc & 1) & 0xFFFFFFFF
            crc = (crc >> 1) ^ (0xEDB88320 & mask)
    return crc ^ 0xFFFFFFFF


def fresh():
    raw = bytearray([0xFF] * OTP_SIZE)
    raw[0:8] = MAGIC
    raw[8:12] = (1).to_bytes(4, "little")
    return raw


def floor_of(raw):
    fuses = raw[OFF_FLOOR:OFF_FLOOR + FLOOR_LEN]
    return sum(8 - b.bit_count() for b in fuses)


def secure_enabled(raw):
    f = raw[OFF_FLAGS:OFF_FLAGS + 4]
    if f == b"\xFF" * 4:
        return False
    return (int.from_bytes(f, "little") & FLAG_SECURE_BOOT) != 0


def burn_flags(raw, flags):
    cur = int.from_bytes(raw[OFF_FLAGS:OFF_FLAGS + 4], "little")
    if raw[OFF_FLAGS:OFF_FLAGS + 4] == b"\xFF" * 4:
        cur = 0
    if cur & flags:
        return True
    raw[OFF_FLAGS:OFF_FLAGS + 4] = (cur | flags).to_bytes(4, "little")
    return True


def burn_boot_pk(raw, sha):
    if raw[OFF_BOOT_PK:OFF_BOOT_PK + 32] != b"\xFF" * 32:
        return False
    raw[OFF_BOOT_PK:OFF_BOOT_PK + 32] = sha
    return True


def burn_dev_key(raw, key):
    if raw[OFF_DEV:OFF_DEV + 32] != b"\xFF" * 32:
        return False
    raw[OFF_DEV:OFF_DEV + 32] = key
    return True


def bump_floor(raw, n):
    cur = floor_of(raw)
    if n <= cur or n > FLOOR_MAX:
        return False
    remaining = n - cur
    for i in range(FLOOR_LEN):
        if remaining == 0:
            break
        for bit in range(8):
            if remaining == 0:
                break
            off = OFF_FLOOR + i
            if raw[off] & (1 << bit):
                raw[off] &= ~(1 << bit)
                remaining -= 1
    return remaining == 0


def dump_otp(raw, label):
    print("== OTP %s ==" % label)
    if raw[0:8] != MAGIC:
        print("  magic: INVALID")
        return
    print("  magic: OK  version=%d  secure-boot=%s" %
          (int.from_bytes(raw[8:12], "little"),
           "ON" if secure_enabled(raw) else "OFF"))
    pk = raw[OFF_BOOT_PK:OFF_BOOT_PK + 32]
    fused = pk != b"\xFF" * 32
    print("  boot_pk_sha256: %s%s" % (pk.hex() if fused else "(not fused)",
                                      "  FUSED" if fused else ""))
    dk = raw[OFF_DEV:OFF_DEV + 32]
    print("  device_key: %s%s" % (dk.hex() if dk != b"\xFF" * 32 else "(none)",
                                  "  FUSED" if dk != b"\xFF" * 32 else ""))
    print("  anti-rollback floor: %d / %d" % (floor_of(raw), FLOOR_MAX))


def dump_bcb(sector, label):
    print("== BCB %s ==" % label)
    if sector[0:8] != b"KXBCBv10":
        print("  magic: INVALID")
        return
    if int.from_bytes(sector[8:12], "little") != 1:
        print("  version: MISMATCH")
        return
    if int.from_bytes(sector[504:508], "little") != crc32(sector[0:504]):
        print("  crc: INVALID")
        return
    print("  active=%c tries=%d/%d ok_A=%d ok_B=%d mode=%d build=%d gen=%d" % (
        "AB"[sector[12]], sector[13], sector[14], sector[15] & 1,
        (sector[15] >> 1) & 1, sector[17],
        int.from_bytes(sector[20:24], "little"),
        int.from_bytes(sector[28:32], "little")))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--disk")
    ap.add_argument("--raw")
    args = ap.parse_args()
    if args.raw:
        raw = bytearray(open(args.raw, "rb").read()[:OTP_SIZE])
        dump_otp(raw, args.raw)
        sys.exit(0)
    if args.disk:
        img = open(args.disk, "rb").read()
        dump_otp(bytearray(img[OTP_LBA * 512:OTP_LBA * 512 + OTP_SIZE]), "disk %s" % args.disk)
        dump_bcb(img[BCB_LBA * 512:BCB_LBA * 512 + 512], "primary (disk)")
        dump_bcb(img[(BCB_LBA + 1) * 512:(BCB_LBA + 1) * 512 + 512], "backup (disk)")
        sys.exit(0)
    ap.error("需 --disk 或 --raw")


if __name__ == "__main__":
    main()