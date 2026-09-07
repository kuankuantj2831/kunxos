#!/usr/bin/env python3
"""构建 KunxOS 磁盘镜像（A/B 安全启动布局）：
MBR + boot_a(FAT32) + sys(SFS) + data(SFS/ENC) + boot_b(FAT32) + misc 原始区。

分区布局（512MB 磁盘，1MB 对齐，扇区 512B，与文档 docs/secure-boot.md §3 一致）：
    P1 boot_a  FAT32 64MB active   （当前槽：KERNEL.IMG / KUNXOS.DTB / RAMDISK.IMG / RECOVRD.IMG）
    P2 sys     SFS   124MB         （OTA 暂存 / 离线全量包）
    P3 data    SFS   256MB (ENC)   （/data，内核启动时 mkfs）
    P4 boot_b  FAT32 64MB          （非活动槽，出厂预置同签名镜像）
    misc       3MB 裸区（非分区）：
       @+0  BCB 主（512B）  @+1  BCB 备（512B）  @+2..+9 OTP（4KB）

boot_a/boot_b 内的镜像均为 KXSI v1 签名容器（kunxsec/src/kxsi.rs 格式）：
    KERNEL.IMG  role=2 version=BUILD entry=0x40080000（载荷为裸内核）
    KUNXOS.DTB  role=3 version=BUILD
    RAMDISK.IMG role=4 version=BUILD（载荷为 rootfs SFS）
    RECOVRD.IMG role=5 version=BUILD（载荷为恢复 ramdisk SFS）

用法：
    mkdisk.py KERNEL.IMG KUNXOS.DTB RAMDISK.IMG RECOVRD.IMG ROOTFS_DIR disk.img
        [--boot-key tools/kunxsec/keys/boot.sk] [--build 42]
        [--sign]   # 把四个镜像包装成 KXSI（默认开启）
"""

import argparse
import hashlib
import os
import random
import struct
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mkfs_sfs  # noqa: E402
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "kunxsec"))
import kxsi_build  # noqa: E402

SECTOR = 512
MB = 1024 * 1024

# ---- 磁盘 / 分区几何（扇区为单位）----
# boot_a/boot_b 各 96MB：KERNEL.IMG（约 40MB，含内嵌 rootfs）+ RAMDISK.IMG +
# RECOVRD.IMG（各 16MB）+ KUNXOS.DTB，64MB 的旧分区会被镜像大小顶穿
# （mkdisk: FAT 容量不足）。/data 相应缩小到 192MB（SFS 容量仍充足）。
DISK_SECTORS = 1048576  # 512MB
ALIGN = 2048            # 1MB
BOOT_A_START = ALIGN
BOOT_A_SECTORS = 196608  # 96MB
SYS_START = BOOT_A_START + BOOT_A_SECTORS
SYS_SECTORS = 253952     # 124MB
DATA_START = SYS_START + SYS_SECTORS
DATA_SECTORS = 393216    # 192MB
BOOT_B_START = DATA_START + DATA_SECTORS
BOOT_B_SECTORS = 196608  # 96MB
MISC_START = BOOT_B_START + BOOT_B_SECTORS
MISC_SECTORS = DISK_SECTORS - MISC_START  # 剩余 3MB

# misc 内部偏移
BCB_PRIMARY_LBA = MISC_START
BCB_BACKUP_LBA = MISC_START + 1
OTP_LBA = MISC_START + 2
OTP_SECTORS = 8

# ---- FAT32 参数 ----
RESERVED_SECTORS = 32
FAT_EOC = 0x0FFFFFFF
FAT_MEDIA = 0x0FFFFFF8

TYPE_FAT32 = 0x0B
TYPE_SFS = 0x83
TYPE_ENC_DATA = 0x8E


def le16(v):
    return struct.pack("<H", v)


def le32(v):
    return struct.pack("<I", v)


def crc32_bytes(data):
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            mask = (crc & 1).__class__(-(crc & 1) & 0xFFFFFFFF)
            crc = (crc >> 1) ^ (0xEDB88320 & mask)
    return crc ^ 0xFFFFFFFF


def trim_dtb(data):
    if len(data) < 40 or data[0:4] != b"\xd0\x0d\xfe\xed":
        return data
    off_struct = struct.unpack(">I", data[8:12])[0]
    off_strings = struct.unpack(">I", data[12:16])[0]
    size_strings = struct.unpack(">I", data[32:36])[0]
    size_struct = struct.unpack(">I", data[36:40])[0]
    content = max(off_struct + size_struct, off_strings + size_strings)
    if content > len(data):
        return data
    out = bytearray(data[:content])
    out[4:8] = struct.pack(">I", content)
    return bytes(out)


def build_fat32(img, start_lba, part_sectors, files):
    """在 `start_lba` 构建 FAT32 卷并写入 `files`（(11 字节短名, 数据)）。"""
    total_file_bytes = sum(len(d) for _, d in files)
    total_file_clusters = (total_file_bytes + SECTOR - 1) // SECTOR
    root_clusters = 1
    total_data_clusters = total_file_clusters + root_clusters
    data_sectors = part_sectors - RESERVED_SECTORS - 2  # 粗估（1 FAT 起步）
    est_clusters = data_sectors // 1
    fat_size = (est_clusters * 4 + SECTOR - 1) // SECTOR
    # ★ 实际布局预留 2 个 FAT（num_fats=2）：按 2×fat_size 收敛重算，否则
    # 容量检查放行的文件总量比真实容量多 ~1×fat_size 扇区（96MB 分区约
    # 1.5MB）——顶满时文件数据越过分区边界写进后续区域，随后被 SFS 镜像
    # 覆盖（bootloader 读到损坏镜像），而 mkdisk 退出码仍为 0。
    for _ in range(4):
        data_sectors = part_sectors - RESERVED_SECTORS - 2 * fat_size
        est_clusters = max(data_sectors // 1, 1)
        new_fat = max((est_clusters * 4 + SECTOR - 1) // SECTOR, 1)
        if new_fat == fat_size:
            break
        fat_size = new_fat
    # 确保 fat 覆盖全部数据区（镜像文件大时）
    b = start_lba * SECTOR

    # BPB
    img[b + 0:b + 3] = b"\xEB\x58\x90"
    img[b + 3:b + 11] = b"MSWIN4.1"
    img[b + 11:b + 13] = le16(SECTOR)
    img[b + 13] = 1                     # sectors_per_cluster
    img[b + 14:b + 16] = le16(RESERVED_SECTORS)
    img[b + 16] = 2                     # num_fats
    img[b + 17:b + 19] = le16(0)
    img[b + 19:b + 21] = le16(0)
    img[b + 21] = 0xF8
    img[b + 22:b + 24] = le16(0)
    img[b + 24:b + 26] = le16(63)
    img[b + 26:b + 28] = le16(255)
    img[b + 28:b + 32] = le32(start_lba)   # hidden
    img[b + 32:b + 36] = le32(part_sectors)  # total_sectors_32
    img[b + 36:b + 40] = le32(fat_size)   # fat_size_32
    img[b + 44:b + 48] = le32(2)          # root_cluster
    img[b + 64] = 0x80
    img[b + 66] = 0x29
    img[b + 67:b + 71] = le32(0x1234ABCD)
    img[b + 71:b + 82] = b"KUNXBOOT   "
    img[b + 82:b + 90] = b"FAT32   "
    img[b + 510] = 0x55
    img[b + 511] = 0xAA

    fat_lba = start_lba + RESERVED_SECTORS
    fat_off = fat_lba * SECTOR
    for c, v in ((0, FAT_MEDIA), (1, FAT_EOC), (2, FAT_EOC)):
        img[fat_off + c * 4:fat_off + c * 4 + 4] = le32(v)

    first_data_sector = start_lba + RESERVED_SECTORS + 2 * fat_size
    root_off = first_data_sector * SECTOR
    next_cluster = 3
    root_entries = []
    for name, data in files:
        clusters_needed = (len(data) + SECTOR - 1) // SECTOR
        start_cluster = next_cluster
        foff = root_off + (start_cluster - 2) * SECTOR
        img[foff:foff + len(data)] = data
        for i in range(clusters_needed - 1):
            img[fat_off + (start_cluster + i) * 4:fat_off + (start_cluster + i) * 4 + 4] = \
                le32(start_cluster + i + 1)
        img[fat_off + (start_cluster + clusters_needed - 1) * 4:
            fat_off + (start_cluster + clusters_needed - 1) * 4 + 4] = le32(FAT_EOC)
        next_cluster += clusters_needed
        root_entries.append((name, start_cluster, len(data)))
    for i, (name, cluster, size) in enumerate(root_entries):
        e = bytearray(32)
        e[0:11] = name
        e[11] = 0x20
        e[20:22] = le16(cluster & 0xFFFF)
        e[26:28] = le16((cluster >> 16) & 0xFFFF)
        e[28:32] = le32(size)
        img[root_off + i * 32:root_off + i * 32 + 32] = e
    # FAT2 = FAT1
    img[(fat_lba + fat_size) * SECTOR:(fat_lba + 2 * fat_size) * SECTOR] = \
        img[fat_off:fat_off + fat_size * SECTOR]

    # root 之后的数据簇校验：文件不得超过分区容量
    if next_cluster - 2 > est_clusters:
        print("mkdisk: FAT 容量不足（files 过大）", file=sys.stderr)
        sys.exit(1)


def build_sfs(img, src_dir, start_lba, total_blocks):
    fd, tmp = tempfile.mkstemp(prefix="kunxos_sfs_", suffix=".img")
    os.close(fd)
    tmpdir = None
    if not (os.path.isdir(src_dir) and os.listdir(src_dir)):
        # sys 分区：空目录 → 用临时目录（含空 otas/ 目录）打包
        tmpdir = tempfile.mkdtemp(prefix="kunxos_sys_")
        ota_dir = os.path.join(tmpdir, "ota")
        os.makedirs(ota_dir)
        with open(os.path.join(ota_dir, "README"), "w") as f:
            f.write("KunxOS OTA staging\n")
        src_dir = tmpdir
    try:
        mkfs_sfs.build(src_dir, tmp, total_blocks)
        with open(tmp, "rb") as f:
            data = f.read()
    finally:
        os.remove(tmp)
        if tmpdir and os.path.isdir(tmpdir):
            import shutil

            shutil.rmtree(tmpdir, ignore_errors=True)
    img[start_lba * SECTOR:start_lba * SECTOR + len(data)] = data


def boot_misc():
    """BCB 主/备（全新）+ OTP（熔断 secure-boot / boot_pk 摘要 / 设备密钥）。"""
    bcb = bytearray(512)
    bcb[0:8] = b"KXBCBv10"
    bcb[8:12] = le32(1)            # version
    bcb[12] = 0                    # active = A
    bcb[13] = 0                    # tries_remaining
    bcb[14] = 3                    # tries_total
    bcb[15] = 0                    # slot_ok
    bcb[17] = 0                    # boot_mode normal
    bcb[18] = 0                    # ota_in_progress
    # bcb[32..504] = 0
    bcb[504:508] = le32(crc32_bytes(bytes(bcb[0:504])))
    bcbb = bytes(bcb)
    otp = bytearray(4096)
    otp[:] = b"\xFF" * 4096
    otp[0:8] = b"KXOTPv10"
    otp[8:12] = le32(1)
    otp[12:16] = le32(1)          # flags: secure_boot 使能
    otp[16:48] = boot_pk_sha       # 授权 boot 公钥摘要
    otp[48:80] = device_key
    return bcbb, bytes(otp)


def mbr_entry(boot_flag, part_type, start_lba, sectors):
    e = bytearray(16)
    e[0] = boot_flag
    e[4] = part_type
    e[8:12] = le32(start_lba)
    e[12:16] = le32(sectors)
    return e


def write_mbr(img, boot_a_active):
    img[446:462] = mbr_entry(boot_a_active, TYPE_FAT32, BOOT_A_START, BOOT_A_SECTORS)
    img[462:478] = mbr_entry(0x00, TYPE_SFS, SYS_START, SYS_SECTORS)
    img[478:494] = mbr_entry(0x00, TYPE_ENC_DATA, DATA_START, DATA_SECTORS)
    img[494:510] = mbr_entry(0x00, TYPE_FAT32, BOOT_B_START, BOOT_B_SECTORS)
    img[510] = 0x55
    img[511] = 0xAA


def kxsi_file(kind, payload, boot_sk, build, entry):
    """kind: 'kernel'/'dtb'/'ramdisk'/'recovrd' → role。"""
    role = {"kernel": 2, "dtb": 3, "ramdisk": 4, "recovrd": 5}[kind]
    return kxsi_build.build(payload, role, build, boot_sk, entry or 0)


def fat_name(kind):
    # 8.3 短名（11 字节，空格补齐）
    table = {
        "kernel": b"KERNEL  IMG",
        "dtb": b"KUNXOS  DTB",
        "ramdisk": b"RAMDISK IMG",
        "recovrd": b"RECOVRD IMG",
    }
    return table[kind]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("kernel_img")
    ap.add_argument("dtb")
    ap.add_argument("ramdisk")
    ap.add_argument("recovrd_ramdisk", nargs="?", default="")
    ap.add_argument("rootfs_dir")
    ap.add_argument("disk_img")
    ap.add_argument("--boot-key", default=os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "kunxsec", "keys", "boot.sk"))
    ap.add_argument("--build", type=int, default=10)
    ap.add_argument("--active-b", action="store_true", help="出厂活动槽设为 B（测试）")
    args = ap.parse_args()

    global boot_pk_sha, device_key
    boot_sk = open(args.boot_key, "rb").read()
    import ed25519 as ed
    boot_pk = ed.publickey(boot_sk)
    boot_pk_sha = hashlib.sha256(boot_pk).digest()
    device_key = os.urandom(32)

    with open(args.kernel_img, "rb") as f:
        kernel = f.read()
    dtb = trim_dtb(open(args.dtb, "rb").read())
    ramdisk = open(args.ramdisk, "rb").read()
    recovrd = open(args.recovrd_ramdisk, "rb").read() if args.recovrd_ramdisk else b""

    files = [
        (fat_name("kernel"), kxsi_file("kernel", kernel, boot_sk, args.build, 0x40080000)),
        (fat_name("dtb"), kxsi_file("dtb", dtb, boot_sk, args.build, 0)),
        (fat_name("ramdisk"), kxsi_file("ramdisk", ramdisk, boot_sk, args.build, 0)),
    ]
    if recovrd:
        files.append((fat_name("recovrd"), kxsi_file("recovrd", recovrd, boot_sk, args.build, 0)))

    img = bytearray(DISK_SECTORS * SECTOR)
    write_mbr(img, 0x80 if not args.active_b else 0x00)
    build_fat32(img, BOOT_A_START, BOOT_A_SECTORS, files)
    build_fat32(img, BOOT_B_START, BOOT_B_SECTORS, files)  # 出厂双槽同镜像
    build_sfs(img, args.rootfs_dir, SYS_START, SYS_SECTORS // (mkfs_sfs.BLK_SIZE // SECTOR))
    # data 全零（内核启动时 mkfs）
    bcbb, otp = boot_misc()
    img[BCB_PRIMARY_LBA * SECTOR:BCB_PRIMARY_LBA * SECTOR + 512] = bcbb
    img[BCB_BACKUP_LBA * SECTOR:BCB_BACKUP_LBA * SECTOR + 512] = bcbb
    img[OTP_LBA * SECTOR:OTP_LBA * SECTOR + 4096] = otp

    with open(args.disk_img, "wb") as f:
        f.write(img)

    print("mkdisk: %s (%d sectors)" % (args.disk_img, DISK_SECTORS))
    print("  boot_a FAT32 @LBA %d (64MB, active=%s)" % (BOOT_A_START, "A" if not args.active_b else "B"))
    print("  sys    SFS   @LBA %d (124MB)" % SYS_START)
    print("  data   SFS/ENC @LBA %d (256MB, empty)" % DATA_START)
    print("  boot_b FAT32 @LBA %d (64MB, signed placeholder)" % BOOT_B_START)
    print("  misc   @LBA %d (%dMB): BCB*2 + OTP(%s)" % (MISC_START, MISC_SECTORS // 2048,
                                                        "secure-boot fused, boot_pk_sha=%s" %
                                                        boot_pk_sha.hex()[:16]))
    print("  build=%d, all images KXSI-signed by boot key" % args.build)


if __name__ == "__main__":
    main()