#!/usr/bin/env python3
"""把目录树打包成 KunxOS 简单文件系统（SFS）格式的 ramdisk 镜像。

磁盘布局与 kunxcore/src/fs_simple.rs 严格一致：
  块 0         超级块
  块 1         inode 位图（64 inode）
  块 2         数据块位图
  块 3..4       inode 表（64 x 128 字节）
  块 5..N-1    数据块

inode（128 字节）：type@0(u16) nlink@2(u16) size@4(u32) rdev@8(u32)
                  direct@12（24 个 u32 块号）
目录项（32 字节）：ino@0(u32) + name@4（28 字节，NUL 结尾，最长 27 字符）
"""

import os
import struct
import sys

BLK_SIZE = 4096
MAX_INODES = 128  # 与内核 fs_simple.rs 保持一致（64 会被 OOBE 默认配置复制耗尽）
INODE_SIZE = 128
INODES_PER_BLOCK = BLK_SIZE // INODE_SIZE  # 32
INODE_BLOCKS = (MAX_INODES + INODES_PER_BLOCK - 1) // INODES_PER_BLOCK  # 2
NDIRECT = 24
MAX_NAME = 27
DIRENT_SIZE = 32

SUPER_BLOCK = 0
IMAP_BLOCK = 1
BMAP_BLOCK = 2
INODE_START = 3
DATA_START = INODE_START + INODE_BLOCKS  # 5

MAGIC = 0x5F534653

T_REG = 1
T_DIR = 2

SB_MAGIC = 0
SB_BLK_SIZE = 4
SB_TOTAL = 8
SB_INODE_BLOCKS = 12
SB_DATA_START = 16
SB_NUM_INODES = 20
SB_ROOT_INO = 24
SB_FREE_BLOCKS = 28
SB_FREE_INODES = 32


def build(src_dir, out_path, total_blocks=192):
    disk = bytearray(total_blocks * BLK_SIZE)
    inodes = [None]  # ino 0 保留（哨兵）

    def walk(path, parent_ino, is_root):
        ino = len(inodes)
        name = "" if is_root else os.path.basename(path)
        if os.path.isdir(path):
            node = {"type": T_DIR, "parent": parent_ino,
                    "name": name, "children": [], "data": None}
            inodes.append(node)
            for child in sorted(os.listdir(path)):
                cpath = os.path.join(path, child)
                node["children"].append((child, walk(cpath, ino, False)))
            return ino
        with open(path, "rb") as f:
            data = f.read()
        inodes.append({"type": T_REG, "parent": parent_ino,
                       "name": name, "children": None, "data": data})
        return ino

    root_ino = walk(src_dir, 0, True)
    assert root_ino == 1
    n_used = len(inodes) - 1
    if n_used > MAX_INODES:
        print("mkfs_sfs: too many inodes (%d > %d)" % (n_used, MAX_INODES), file=sys.stderr)
        sys.exit(1)

    next_block = DATA_START
    inode_blobs = [bytearray(INODE_SIZE)]  # ino 0 空

    for ino in range(1, len(inodes)):
        node = inodes[ino]
        rec = bytearray(INODE_SIZE)
        if node["type"] == T_DIR:
            parent_ref = ino if node["parent"] == 0 else node["parent"]
            entries = [(ino, b"."), (parent_ref, b"..")]
            for (cname, cino) in node["children"]:
                entries.append((cino, cname.encode("utf-8")))
            per_block = BLK_SIZE // DIRENT_SIZE  # 128
            n_blocks = (len(entries) + per_block - 1) // per_block
            blocks = []
            for b in range(n_blocks):
                blkno = next_block
                next_block += 1
                blocks.append(blkno)
                buf = bytearray(BLK_SIZE)
                for e in range(per_block):
                    idx = b * per_block + e
                    if idx >= len(entries):
                        break
                    eino, ename = entries[idx]
                    if len(ename) > MAX_NAME:
                        print("mkfs_sfs: name too long: %r" % ename, file=sys.stderr)
                        sys.exit(1)
                    off = e * DIRENT_SIZE
                    struct.pack_into("<I", buf, off, eino)
                    buf[off + 4:off + 4 + len(ename)] = ename
                disk[blkno * BLK_SIZE:(blkno + 1) * BLK_SIZE] = buf
            struct.pack_into("<H", rec, 0, T_DIR)
            struct.pack_into("<H", rec, 2, 1)
            struct.pack_into("<I", rec, 4, n_blocks * BLK_SIZE)
            for i, blk in enumerate(blocks):
                struct.pack_into("<I", rec, 12 + i * 4, blk)
        else:
            data = node["data"]
            size = len(data)
            n_blocks = (size + BLK_SIZE - 1) // BLK_SIZE
            if n_blocks > NDIRECT:
                print("mkfs_sfs: file too large: %s (%d bytes)" % (node["name"], size), file=sys.stderr)
                sys.exit(1)
            blocks = []
            for b in range(n_blocks):
                blkno = next_block
                next_block += 1
                blocks.append(blkno)
                chunk = data[b * BLK_SIZE:(b + 1) * BLK_SIZE]
                disk[blkno * BLK_SIZE:blkno * BLK_SIZE + len(chunk)] = chunk
            struct.pack_into("<H", rec, 0, T_REG)
            struct.pack_into("<H", rec, 2, 1)
            struct.pack_into("<I", rec, 4, size)
            for i, blk in enumerate(blocks):
                struct.pack_into("<I", rec, 12 + i * 4, blk)
        inode_blobs.append(rec)

    if next_block > total_blocks:
        print("mkfs_sfs: out of disk blocks (%d > %d)" % (next_block, total_blocks), file=sys.stderr)
        sys.exit(1)

    sb = bytearray(BLK_SIZE)
    struct.pack_into("<I", sb, SB_MAGIC, MAGIC)
    struct.pack_into("<I", sb, SB_BLK_SIZE, BLK_SIZE)
    struct.pack_into("<I", sb, SB_TOTAL, total_blocks)
    struct.pack_into("<I", sb, SB_INODE_BLOCKS, INODE_BLOCKS)
    struct.pack_into("<I", sb, SB_DATA_START, DATA_START)
    struct.pack_into("<I", sb, SB_NUM_INODES, MAX_INODES)
    struct.pack_into("<I", sb, SB_ROOT_INO, 1)
    struct.pack_into("<I", sb, SB_FREE_BLOCKS, total_blocks - next_block)
    struct.pack_into("<I", sb, SB_FREE_INODES, MAX_INODES - n_used)
    disk[0:BLK_SIZE] = sb

    imap = bytearray(BLK_SIZE)
    for ino in range(0, len(inodes)):
        imap[ino // 8] |= 1 << (ino % 8)
    disk[IMAP_BLOCK * BLK_SIZE:(IMAP_BLOCK + 1) * BLK_SIZE] = imap

    bmap = bytearray(BLK_SIZE)
    for blk in range(DATA_START, next_block):
        idx = blk - DATA_START
        bmap[idx // 8] |= 1 << (idx % 8)
    disk[BMAP_BLOCK * BLK_SIZE:(BMAP_BLOCK + 1) * BLK_SIZE] = bmap

    for ino in range(1, len(inodes)):
        off = INODE_START * BLK_SIZE + ino * INODE_SIZE
        disk[off:off + INODE_SIZE] = inode_blobs[ino]

    with open(out_path, "wb") as f:
        f.write(disk)
    print("mkfs_sfs: wrote %s (%d blocks, %d inodes, %d data blocks)" %
          (out_path, total_blocks, n_used, next_block - DATA_START))


def main():
    if len(sys.argv) != 3:
        print("usage: mkfs_sfs.py <src_dir> <out_img>", file=sys.stderr)
        sys.exit(2)
    build(sys.argv[1], sys.argv[2])


if __name__ == "__main__":
    main()
