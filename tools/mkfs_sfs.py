#!/usr/bin/env python3
"""把目录树打包成 KunxOS SFS v2 格式的 ramdisk 镜像。

磁盘布局与 kunxcore/src/fs_simple.rs（SFS v2）严格一致：
  块 0         超级块（主）
  块 1         超级块（备份）
  块 2         inode 位图（256 位）
  块 3..3+B-1  数据位图（B = ceil(数据块数/32768)）
  块 3+B..3+B+15 inode 表（256 × 256B = 16 块）
  块 3+B+8..+J-1 日志环（J = min(1024, max(64, total/8)) 块）
  块 DATA_START..  数据区

inode（256 字节）：type@0(u16) nlink@2(u16) size@4(u64) rdev@12(u32) gen@16(u32)
                  crc@20(u32) direct@24(24×u32) indirect@120(u32)
                  double@124(u32) triple@128(u32)
目录项（32 字节）：ino@0(u32) + name@4（28 字节，NUL 结尾，最长 27 字符）
日志记录（2 块）：头块 magic/seq/target/crc/flags + 镜像块。

CRC32：IEEE 802.3（反射多项式 0xEDB88320，init/xorout 0xFFFFFFFF），
与内核 crc32() 一致。
"""

import os
import struct
import sys

BLK_SIZE = 4096
# 512：与内核 MAX_INODES 同步扩容（2026-09-18，256 → 512）。
# rootfs 文件数增长到 196 后，sfsrace（48 文件）+ 守护进程信箱数秒内
# 耗尽剩余 inode，全系统 create 失败。旧 256-inode 镜像会被内核
# parse_sb 的 SB_NUM_INODES 护栏拒绝挂载，需 rm disk.img && make disk 重建。
MAX_INODES = 512
INODE_SIZE = 256
INODES_PER_BLOCK = BLK_SIZE // INODE_SIZE  # 16
INODE_BLOCKS = (MAX_INODES + INODES_PER_BLOCK - 1) // INODES_PER_BLOCK  # 32
NDIRECT = 24
NINDIRECT = BLK_SIZE // 4  # 1024
# 写入端只实现 直接块 + 一级间接（ind_buf 单块 1024 项）：容量按此封顶。
# 此前按二级/三级间接的数学容量放行 —— >24+1024 块（≈4.29MB）的文件会在
# struct.pack_into 处越界抛未处理异常。
MAX_BLOCKS = NDIRECT + NINDIRECT
MAX_NAME = 27
DIRENT_SIZE = 32
BITS_PER_BLOCK = BLK_SIZE * 8  # 32768

SB0 = 0
SB1 = 1
IMAP_BLOCK = 2
BMAP_START = 3

MAGIC = 0x5F534632  # "_SF2"
SFS_VERSION = 2
JLOG_MAX = 1024
JLOG_MIN = 64

T_REG = 1
T_DIR = 2

SB_MAGIC = 0
SB_VERSION = 4
SB_BLK_SIZE = 8
SB_TOTAL = 12
SB_INODE_BLOCKS = 16
SB_BMAP_START = 20
SB_BMAP_BLOCKS = 24
SB_INODE_START = 28
SB_JLOG_START = 32
SB_JLOG_BLOCKS = 36
SB_DATA_START = 40
SB_NUM_INODES = 44
SB_ROOT_INO = 48
SB_FREE_INODES = 52
SB_FREE_BLOCKS = 56
SB_FLAGS = 60
SB_JLOG_HEAD_SEQ = 64
SB_JLOG_HEAD_ABS = 72
SB_JLOG_NEXT_ABS = 80
SB_INO_HINT = 88
SB_BLK_HINT = 92
SB_CRC = 96

I_TYPE = 0
I_NLINK = 2
I_SIZE = 4
I_RDEV = 12
I_GEN = 16
I_CRC = 20
I_DIRECT = 24
I_INDIRECT = I_DIRECT + NDIRECT * 4  # 120
I_DOUBLE = 124
I_TRIPLE = 128

JLOG_MAGIC = 0x5346534A  # "SFSJ"
JH_MAGIC = 0
JH_SEQ_LO = 4
JH_SEQ_HI = 8
JH_TARGET = 12
JH_CRC = 16
JH_FLAGS = 20


def crc32(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            if crc & 1:
                crc = (crc >> 1) ^ 0xEDB88320
            else:
                crc >>= 1
    return crc ^ 0xFFFFFFFF


def layout(total: int):
    jlog = max(JLOG_MIN, min(JLOG_MAX, total // 8))

    def need_of(bmap: int) -> int:
        # bmap 增大 → data_start 后移 → ndata/need 单调减小
        inode_start = BMAP_START + bmap
        jlog_start = inode_start + INODE_BLOCKS
        data_start = jlog_start + jlog
        ndata = max(0, total - data_start)
        return max(1, (ndata + BITS_PER_BLOCK - 1) // BITS_PER_BLOCK)

    # need(bmap) 单调递减 → need(bmap) - bmap 严格递减，交点唯一。
    # 此前的不动点迭代在 need 与 bmap 之间可能陷入 2-循环（如
    # total=33813 永不返回，构建挂死）—— 改二分求最小可行 bmap。
    lo, hi = 1, max(1, total)
    if need_of(hi) > hi:
        raise SystemExit(f"mkfs_sfs: total={total} 过小，放不下位图")
    while lo < hi:
        mid = (lo + hi) // 2
        if need_of(mid) <= mid:
            hi = mid
        else:
            lo = mid + 1
    bmap = lo
    inode_start = BMAP_START + bmap
    jlog_start = inode_start + INODE_BLOCKS
    data_start = jlog_start + jlog
    return bmap, inode_start, jlog_start, jlog, data_start


def inode_crc(rec: bytearray) -> int:
    tmp = bytearray(rec)
    tmp[I_CRC:I_CRC + 4] = b"\0\0\0\0"
    return crc32(bytes(tmp))


def build(src_dir, out_path, total_blocks=1536):
    bmap_blocks, inode_start, jlog_start, jlog_blocks, data_start = layout(total_blocks)
    if data_start >= total_blocks:
        print("mkfs_sfs: partition too small", file=sys.stderr)
        sys.exit(1)
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

    next_block = data_start
    inode_blobs = [bytearray(INODE_SIZE)]  # ino 0 空

    for ino in range(1, len(inodes)):
        node = inodes[ino]
        rec = bytearray(INODE_SIZE)
        struct.pack_into("<H", rec, I_TYPE, node["type"])
        struct.pack_into("<H", rec, I_NLINK, 1)
        struct.pack_into("<I", rec, I_GEN, 1)
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
            struct.pack_into("<Q", rec, I_SIZE, n_blocks * BLK_SIZE)
            for i, blk in enumerate(blocks):
                struct.pack_into("<I", rec, I_DIRECT + i * 4, blk)
        else:
            data = node["data"]
            size = len(data)
            n_blocks = (size + BLK_SIZE - 1) // BLK_SIZE
            if n_blocks > MAX_BLOCKS:
                print("mkfs_sfs: file too large: %s (%d bytes)" % (node["name"], size), file=sys.stderr)
                sys.exit(1)
            blocks = []
            for b in range(n_blocks):
                blkno = next_block
                next_block += 1
                blocks.append(blkno)
                chunk = data[b * BLK_SIZE:(b + 1) * BLK_SIZE]
                disk[blkno * BLK_SIZE:blkno * BLK_SIZE + len(chunk)] = chunk
            struct.pack_into("<Q", rec, I_SIZE, size)
            if n_blocks > NDIRECT:
                for i, blk in enumerate(blocks[:NDIRECT]):
                    struct.pack_into("<I", rec, I_DIRECT + i * 4, blk)
                ind_blk = next_block
                next_block += 1
                ind_buf = bytearray(BLK_SIZE)
                for i, blk in enumerate(blocks[NDIRECT:]):
                    struct.pack_into("<I", ind_buf, i * 4, blk)
                disk[ind_blk * BLK_SIZE:(ind_blk + 1) * BLK_SIZE] = ind_buf
                struct.pack_into("<I", rec, I_INDIRECT, ind_blk)
            else:
                for i, blk in enumerate(blocks):
                    struct.pack_into("<I", rec, I_DIRECT + i * 4, blk)
        struct.pack_into("<I", rec, I_CRC, inode_crc(rec))
        inode_blobs.append(rec)

    if next_block > total_blocks:
        print("mkfs_sfs: out of disk blocks (%d > %d)" % (next_block, total_blocks), file=sys.stderr)
        sys.exit(1)

    # ---- 超级块（主 + 备份，CRC 一致） ----
    sb = bytearray(BLK_SIZE)
    struct.pack_into("<I", sb, SB_MAGIC, MAGIC)
    struct.pack_into("<I", sb, SB_VERSION, SFS_VERSION)
    struct.pack_into("<I", sb, SB_BLK_SIZE, BLK_SIZE)
    struct.pack_into("<I", sb, SB_TOTAL, total_blocks)
    struct.pack_into("<I", sb, SB_INODE_BLOCKS, INODE_BLOCKS)
    struct.pack_into("<I", sb, SB_BMAP_START, BMAP_START)
    struct.pack_into("<I", sb, SB_BMAP_BLOCKS, bmap_blocks)
    struct.pack_into("<I", sb, SB_INODE_START, inode_start)
    struct.pack_into("<I", sb, SB_JLOG_START, jlog_start)
    struct.pack_into("<I", sb, SB_JLOG_BLOCKS, jlog_blocks)
    struct.pack_into("<I", sb, SB_DATA_START, data_start)
    struct.pack_into("<I", sb, SB_NUM_INODES, MAX_INODES)
    struct.pack_into("<I", sb, SB_ROOT_INO, 1)
    struct.pack_into("<I", sb, SB_FREE_INODES, MAX_INODES - n_used - 1)
    struct.pack_into("<I", sb, SB_FREE_BLOCKS, total_blocks - next_block)
    struct.pack_into("<Q", sb, SB_JLOG_HEAD_SEQ, 0)
    struct.pack_into("<Q", sb, SB_JLOG_HEAD_ABS, 0)
    struct.pack_into("<Q", sb, SB_JLOG_NEXT_ABS, 0)
    struct.pack_into("<I", sb, SB_INO_HINT, 1)
    struct.pack_into("<I", sb, SB_BLK_HINT, 0)
    struct.pack_into("<I", sb, SB_CRC, crc32(bytes(sb[0:SB_CRC])))
    disk[SB0 * BLK_SIZE:(SB0 + 1) * BLK_SIZE] = sb
    disk[SB1 * BLK_SIZE:(SB1 + 1) * BLK_SIZE] = sb

    # ---- inode 位图（保留 ino 0） ----
    imap = bytearray(BLK_SIZE)
    for ino in range(0, len(inodes)):
        imap[ino // 8] |= 1 << (ino % 8)
    disk[IMAP_BLOCK * BLK_SIZE:(IMAP_BLOCK + 1) * BLK_SIZE] = imap

    # ---- 数据位图 ----
    for b in range(bmap_blocks):
        disk[(BMAP_START + b) * BLK_SIZE:(BMAP_START + b + 1) * BLK_SIZE] = bytearray(BLK_SIZE)
    for blk in range(data_start, next_block):
        idx = blk - data_start
        b = BMAP_START + idx // BITS_PER_BLOCK
        disk[b * BLK_SIZE + (idx % BITS_PER_BLOCK) // 8] |= 1 << (idx % 8)

    # ---- inode 表 ----
    for ino in range(1, len(inodes)):
        off = inode_start * BLK_SIZE + ino * INODE_SIZE
        disk[off:off + INODE_SIZE] = inode_blobs[ino]

    # ---- 日志区清零（恢复扫描可提前停止） ----
    for b in range(jlog_start, jlog_start + jlog_blocks):
        disk[b * BLK_SIZE:(b + 1) * BLK_SIZE] = bytearray(BLK_SIZE)

    with open(out_path, "wb") as f:
        f.write(disk)
    print("mkfs_sfs: wrote %s (%d blocks, %d inodes, %d data blocks, jlog=%d, data_start=%d)" %
          (out_path, total_blocks, n_used, next_block - data_start, jlog_blocks, data_start))


def main():
    if len(sys.argv) not in (3, 4):
        print("usage: mkfs_sfs.py <src_dir> <out_img> [total_blocks]", file=sys.stderr)
        sys.exit(2)
    total = int(sys.argv[3]) if len(sys.argv) == 4 else 1536
    build(sys.argv[1], sys.argv[2], total)


if __name__ == "__main__":
    main()
