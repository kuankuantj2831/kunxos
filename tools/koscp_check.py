#!/usr/bin/env python3
"""KOS 无缝复制回归判定：从 QEMU 串口日志提取 4 个 sha1sum，
断言 SFS→KOS / KOS→SFS / KOS→KOS 三向复制内容一致。"""
import re
import sys

log = open(sys.argv[1], errors="replace").read()
hashes = dict((p, h) for h, p in re.findall(r"^([0-9a-f]{40})\s+(\S+)$", log, re.M))
need = ["/bin/hello", "/kos/data/from-sfs.txt", "/bin/back.txt", "/kos/data/copy2.txt"]
missing = [p for p in need if p not in hashes]
if missing:
    print(f"KOS COPY TEST: FAIL（缺 sha1：{missing}；日志里只找到 {list(hashes)}）")
    sys.exit(1)
uniq = {hashes[p] for p in need}
if len(uniq) != 1:
    print(f"KOS COPY TEST: FAIL（哈希不一致：{hashes}）")
    sys.exit(1)
print("KOS COPY TEST: PASS（SFS→KOS / KOS→SFS / KOS→KOS 三向 sha1 一致）")
