# Hash replacement fixtures

Captured before replacing the KOS hash dependencies on 2026-11-15.
The independent host oracle used blake3 1.8.7, sha2 0.10.9 and crc32fast 1.5.2.
These libraries are not dependencies of the replacement tests or KOS runtime.

`hash-vectors.txt` contains length, BLAKE3-256, SHA-256 and IEEE CRC-32.
Input byte `i` is `i % 251`. Lengths cover padding, compression block, chunk,
balanced/unbalanced tree boundaries and a 1 MiB input.

`legacy-blocks.bin` records the nonzero blocks of an old 256 x 4096-byte KOS
image: each record is an 8-byte little-endian LBA followed by 4096 data bytes.
Missing blocks are zero. Generation: mkfs, mkdir `/compat` (uid 0, mode 0755),
write `/compat/data` (uid 0, mode 0644) with 8193 bytes of the same input pattern.
Tests mount this old image, update/remount it, and compare newly generated disk
bytes against the old image without invoking any third-party implementation.
