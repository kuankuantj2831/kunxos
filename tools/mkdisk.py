#!/usr/bin/env python3
"""构建 KunxOS 磁盘镜像：MBR + boot(FAT32) + rootfs(SFS) + data(SFS)。

用法：
    python3 tools/mkdisk.py KERNEL.IMG KUNXOS.DTB RAMDISK.IMG ROOTFS_DIR disk.img

分区布局（512MB 磁盘，1MB 对齐，扇区 512B）：
    分区 1（boot）：FAT32，64MB，active（0x80），type 0x0B
    分区 2（rootfs）：SFS，128MB，type 0x83
    分区 3（data）：SFS，256MB，type 0x83（留空，内核启动时 mkfs）

MBR 位于扇区 0；分区条目位于 0x1BE / 0x1CE / 0x1DE；签名 0x55AA 位于 0x1FE。
boot 分区放置 KERNEL.IMG、KUNXOS.DTB、RAMDISK.IMG 三个短文件名（8.3）。
rootfs 分区由 mkfs_sfs 打包 rootfs 目录内容；data 分区保持全零。
"""

import struct
import sys
import os
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mkfs_sfs  # noqa: E402

SECTOR = 512
MB = 1024 * 1024

# ---- 磁盘 / 分区几何（扇区为单位） ----
DISK_MB = 512
BOOT_MB = 64
ROOTFS_MB = 128
DATA_MB = 256

ALIGN_SECTORS = 2048  # 1MB 对齐
BOOT_START = ALIGN_SECTORS
BOOT_SECTORS = BOOT_MB * MB // SECTOR
ROOTFS_START = BOOT_START + BOOT_SECTORS
ROOTFS_SECTORS = ROOTFS_MB * MB // SECTOR
DATA_START = ROOTFS_START + ROOTFS_SECTORS
DATA_SECTORS = DATA_MB * MB // SECTOR
TOTAL_SECTORS = DISK_MB * MB // SECTOR

# ---- FAT32 参数 ----
RESERVED_SECTORS = 32
SECTORS_PER_CLUSTER = 1
NUM_FATS = 2
FAT_EOC = 0x0FFFFFFF
FAT_MEDIA = 0x0FFFFFF8

# ---- 分区类型 ----
TYPE_FAT32 = 0x0B
TYPE_SFS = 0x83
TYPE_ENC_DATA = 0x8E  # 加密 data 分区（内核 blkdev 透明 XOR 加密，宿主查看为密文）


def be32(b):
    return struct.unpack(">I", b)[0]


def le16(v):
    return struct.pack("<H", v)


def le32(v):
    return struct.pack("<I", v)


def trim_dtb(data):
    """去掉 QEMU dumpdtb 的尾部填充，只保留 FDT 头声明的实际内容。"""
    if len(data) < 40 or data[0:4] != b"\xd0\x0d\xfe\xed":
        return data
    off_struct = be32(data[8:12])
    off_strings = be32(data[12:16])
    size_strings = be32(data[32:36])
    size_struct = be32(data[36:40])
    content = max(off_struct + size_struct, off_strings + size_strings)
    if content > len(data):
        return data
    out = bytearray(data[:content])
    out[4:8] = struct.pack(">I", content)  # totalsize
    return bytes(out)


def set_fat(img, fat_off, cluster, value):
    off = fat_off + cluster * 4
    img[off:off + 4] = le32(value)


def mbr_entry(boot_flag, part_type, start_lba, sectors):
    """构造一个 16 字节 MBR 分区条目。"""
    e = bytearray(16)
    e[0] = boot_flag          # 0x80 = active
    e[1] = 0x00
    e[2] = 0x02
    e[3] = 0x00               # CHS start 占位
    e[4] = part_type          # 分区类型
    e[5] = 0xFE
    e[6] = 0xFF
    e[7] = 0xFF               # CHS end 占位
    e[8:12] = le32(start_lba)
    e[12:16] = le32(sectors)
    return e


def write_mbr(img):
    """写入 MBR：3 个分区条目 + 签名。"""
    img[446:462] = mbr_entry(0x80, TYPE_FAT32, BOOT_START, BOOT_SECTORS)
    img[462:478] = mbr_entry(0x00, TYPE_SFS, ROOTFS_START, ROOTFS_SECTORS)
    img[478:494] = mbr_entry(0x00, TYPE_ENC_DATA, DATA_START, DATA_SECTORS)
    img[494:510] = bytes(16)  # 第 4 分区条目为空
    img[510] = 0x55
    img[511] = 0xAA


def build_fat32(img, files):
    """在 boot 分区起始处构建 FAT32 卷，放置 `files`（11 字节短名, 数据）。"""
    total_file_bytes = sum(len(d) for _, d in files)
    total_file_clusters = (total_file_bytes + SECTOR - 1) // SECTOR
    root_clusters = 1
    total_data_clusters = total_file_clusters + root_clusters
    # ★ FAT 必须覆盖整个分区的数据区（否则大簇号没有 FAT 项，内核写大文件时
    # 扫描不到空闲簇而失败）。此前按文件内容计算 fat_size（34 扇区 ≈ 4352 簇），
    # 而分区有 13 万簇：簇号 >4351 的 FAT 项落在 FAT 区外，读回 0/越界 → 分配失败。
    data_sectors = BOOT_SECTORS - RESERVED_SECTORS - NUM_FATS  # 先粗估（1 个 FAT 扇区/簇的极限）
    est_clusters = data_sectors // SECTORS_PER_CLUSTER
    fat_size = (est_clusters * 4 + SECTOR - 1) // SECTOR
    # 用确定的 fat_size 反推真实数据区大小（FAT 覆盖全部簇后留有余量即可）
    num_clusters = total_data_clusters + 2

    part_sectors = RESERVED_SECTORS + NUM_FATS * fat_size + total_data_clusters * SECTORS_PER_CLUSTER
    if part_sectors > BOOT_SECTORS:
        print("mkdisk: boot files do not fit in %dMB FAT32 partition" % BOOT_MB,
              file=sys.stderr)
        sys.exit(1)

    b = BOOT_START * SECTOR

    # ---- FAT32 BPB ----
    img[b + 0:b + 3] = b"\xEB\x58\x90"
    img[b + 3:b + 11] = b"MSWIN4.1"
    img[b + 11:b + 13] = le16(SECTOR)
    img[b + 13] = SECTORS_PER_CLUSTER
    img[b + 14:b + 16] = le16(RESERVED_SECTORS)
    img[b + 16] = NUM_FATS
    img[b + 17:b + 19] = le16(0)           # FAT32 根目录项数 = 0
    img[b + 19:b + 21] = le16(0)           # total_sectors_16 = 0
    img[b + 21] = 0xF8                     # 介质
    img[b + 22:b + 24] = le16(0)           # fat_size_16 = 0
    img[b + 24:b + 26] = le16(63)          # 每磁道扇区
    img[b + 26:b + 28] = le16(255)         # 磁头数
    img[b + 28:b + 32] = le32(BOOT_START)  # hidden_sectors
    img[b + 32:b + 36] = le32(BOOT_SECTORS)  # total_sectors_32
    img[b + 36:b + 40] = le32(fat_size)    # fat_size_32
    img[b + 40:b + 42] = le16(0)           # ext_flags
    img[b + 42:b + 44] = le16(0)           # fs_version
    img[b + 44:b + 48] = le32(2)           # root_cluster
    img[b + 48:b + 50] = le16(1)           # fs_info
    img[b + 50:b + 52] = le16(6)           # backup_boot_sector
    img[b + 64] = 0x80                     # drive_number
    img[b + 66] = 0x29                     # boot_signature
    img[b + 67:b + 71] = le32(0x1234ABCD)  # volume_id
    img[b + 71:b + 82] = b"KUNXBOOT   "    # volume_label
    img[b + 82:b + 90] = b"FAT32   "       # fs_type
    img[b + 510] = 0x55
    img[b + 511] = 0xAA

    # ---- FAT ----
    fat_lba = BOOT_START + RESERVED_SECTORS
    fat_off = fat_lba * SECTOR
    set_fat(img, fat_off, 0, FAT_MEDIA)
    set_fat(img, fat_off, 1, FAT_EOC)
    set_fat(img, fat_off, 2, FAT_EOC)  # 根目录簇 2：FAT 项须置链结束（否则被当空闲簇分配）

    # ---- 数据区 ----
    first_data_sector = BOOT_START + RESERVED_SECTORS + NUM_FATS * fat_size
    data_off = first_data_sector * SECTOR
    root_off = data_off  # 根目录簇 2

    next_cluster = 3  # 簇 2 为根目录，文件从簇 3 开始
    root_entries = []
    for name, data in files:
        clusters_needed = (len(data) + SECTOR - 1) // SECTOR
        start_cluster = next_cluster
        foff = data_off + (start_cluster - 2) * SECTORS_PER_CLUSTER * SECTOR
        img[foff:foff + len(data)] = data
        for i in range(clusters_needed - 1):
            set_fat(img, fat_off, start_cluster + i, start_cluster + i + 1)
        set_fat(img, fat_off, start_cluster + clusters_needed - 1, FAT_EOC)
        next_cluster += clusters_needed
        root_entries.append((name, start_cluster, len(data)))

    # ---- 根目录项 ----
    for i, (name, cluster, size) in enumerate(root_entries):
        e = bytearray(32)
        e[0:11] = name
        e[11] = 0x20                        # archive 属性
        e[20:22] = le16(cluster & 0xFFFF)   # 首簇低 16 位
        e[26:28] = le16((cluster >> 16) & 0xFFFF)  # 首簇高 16 位
        e[28:32] = le32(size)
        img[root_off + i * 32:root_off + i * 32 + 32] = e

    # ---- FAT2 = FAT1 副本 ----
    fat2_off = (fat_lba + fat_size) * SECTOR
    img[fat2_off:fat2_off + fat_size * SECTOR] = img[fat_off:fat_off + fat_size * SECTOR]

    return fat_size


def build_sfs(img, src_dir, start_lba, total_blocks):
    """用 mkfs_sfs 打包 `src_dir`，写入磁盘 `start_lba` 处。"""
    fd, tmp = tempfile.mkstemp(prefix="kunxos_rootfs_", suffix=".img")
    os.close(fd)
    try:
        mkfs_sfs.build(src_dir, tmp, total_blocks)
        with open(tmp, "rb") as f:
            data = f.read()
    finally:
        os.remove(tmp)
    img[start_lba * SECTOR:start_lba * SECTOR + len(data)] = data


def main():
    if len(sys.argv) != 6:
        print("usage: mkdisk.py KERNEL.IMG KUNXOS.DTB RAMDISK.IMG ROOTFS_DIR disk.img")
        sys.exit(1)

    kernel_path, dtb_path, ramdisk_path, rootfs_dir, disk_path = sys.argv[1:6]

    with open(kernel_path, "rb") as f:
        kernel = f.read()
    with open(dtb_path, "rb") as f:
        dtb = trim_dtb(f.read())
    with open(ramdisk_path, "rb") as f:
        ramdisk = f.read()

    files = [
        (b"KERNEL  IMG", kernel),
        (b"KUNXOS  DTB", dtb),
        (b"RAMDISK IMG", ramdisk),
    ]

    img = bytearray(TOTAL_SECTORS * SECTOR)

    write_mbr(img)
    fat_size = build_fat32(img, files)
    build_sfs(img, rootfs_dir, ROOTFS_START, ROOTFS_SECTORS // (mkfs_sfs.BLK_SIZE // SECTOR))

    with open(disk_path, "wb") as f:
        f.write(img)

    print("mkdisk: %s (%dMB, %d sectors)" % (disk_path, DISK_MB, TOTAL_SECTORS))
    print("  boot   FAT32 @ LBA %d  (%dMB, %d sectors/FAT)" %
          (BOOT_START, BOOT_MB, fat_size))
    print("  rootfs SFS  @ LBA %d  (%dMB)" % (ROOTFS_START, ROOTFS_MB))
    print("  data   ENC  @ LBA %d  (%dMB, XOR-encrypted, empty)" % (DATA_START, DATA_MB))


if __name__ == "__main__":
    main()
