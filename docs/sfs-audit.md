# SFS v1 架构审计报告（加固前基线）

审计对象：`kunxcore/src/fs_simple.rs`（v1，927 行）+ `blkdev.rs` + `virtio_blk.rs`。
审计日期：本次加固前。配套设计见 `docs/sfs-v2.md`。

## 1. 现状总结

| 特性 | 状态 | 位置/证据 |
|------|------|-----------|
| 写前日志（WAL） | ❌ 无 | 所有元数据写直接 `write_block` 落盘（`fs_simple.rs` 全文无日志概念） |
| CoW | ❌ 无 | 原地更新：位图/inode/目录块直接覆写 |
| 元数据校验和 | ❌ 无 | inode/超级块无 CRC；inode 表不清零时残留 RAM 垃圾可被误读为有效 inode（已有注释警告） |
| 双超级块 | ❌ 无 | 单超级块，无备份、无 CRC |
| fsck | ❌ 无 | 无一致性检查工具 |
| 掉电保护 | ❌ 无 | truncate 先释放位图后写 inode（崩溃 → inode 引用已释放块）；rename 先摘旧项后加新项（崩溃 → 文件丢失）；无 fsync |
| fsync | ❌ no-op | `linux_compat.rs:498` `SYS_SYNC | SYS_FSYNC | SYS_FDATASYNC => 0` |
| 磨损均衡 | ❌ 无 | `bitmap_alloc` 从位 0 first-fit 扫描；位图块（块 2）每次分配都写；inode 表块集中 |
| 大文件 | ⚠️ 4.2MB 上限 | size u32；24 直接 + 单间接 1024 块；`fs_write` 超限返回 -1 |
| 稀疏文件 | ✅ 天然支持 | 空洞按零填充（`bmap` 返回 0） |
| 锁 | ❌ 无 | 位图 RMW 无原子保护，SMP 下并发会损坏元数据（既有问题） |

## 2. 关键问题回答

### 2.1 SFS 当前块大小？是否对齐 SD 擦除块？

块大小 **4KB**（`blkdev::BLK_SIZE`），扇区 512B，1 块 = 8 扇区。
SD/eMMC 典型擦除块 512KB —— **不对齐**。v2 设计保持 4KB（改动块大小将推翻
全部布局且不兼容既有镜像），改以「日志环分散写 + 位图检查点批量刷盘」缓解
磨损，真机对齐与 discard 列为后续工作（见 `docs/sfs-v2.md` §10）。

### 2.2 日志区大小？如何防止日志写爆？

v1 无日志。v2 日志区 `J = min(1024, max(64, total/8))` 块：
/data 4MB（1024 块，可容纳 512 条待检查点记录），ramdisk 768KB（192 块）。
写爆防护：写前检查环余量，不足先检查点（§5.4）；事务记录数上限 24；
记录禁止跨环尾；元数据日志（非数据正文）写量远小于数据写量，且分散在
J 个块上，不构成擦写瓶颈。

### 2.3 是否需要 TRIM/discard？

**需要（后续工作）**。v2 未实现：virtio-blk 驱动未协商 discard 特性
（`virtio_blk.rs` 特性协商写 0）。真机 SD/eMMC 场景建议：
`VIRTIO_BLK_T_DISCARD`（QEMU 需 `discard=unmap` 或 `discard=on`）或
等价命令通知闪存块释放，降低写放大、延长寿命。

### 2.4 当前分区现状（确认）

MBR 三主分区：boot FAT32（64MB，仅内核/DTB/ramdisk，0x0B）、rootfs SFS
（128MB，0x83）、data SFS（256MB，**0x8E 加密**，`blkdev::wrap_encryption`
XOR 包装，内核首次启动 mkfs）。**/data 已是 SFS 而非 FAT32** —— 「替代
FAT32 作为 /data」实际是「加固现有 SFS」；FAT32 仅用于 boot 分区，符合
目标 6 的「保留 FAT32 仅用于 Boot/EFI」。

### 2.5 其他限制

- 数据位图单块覆盖 32768 块 = 128MB；256MB 分区被 `total_blocks()`
  钳制到 128MB（v2 改多块位图，用满全分区）。
- 目录项 32B、文件名 ≤27 字符、目录线性扫描；无 rmdir；目录上限 24 块
  （v1 用 `NDIRECT` 遍历目录 —— v2 目录仍限直接块以内，保持简单）。
- inode 128 个，v1 每 inode 128B；OOBE 默认配置复制后余量 ~90 个。
- 数据块分配即清零（防跨进程信息泄露）✅ 保留。

## 3. 加固映射（v1 缺口 → v2 措施）

| v1 缺口 | v2 措施（详见 docs/sfs-v2.md） |
|---------|-------------------------------|
| 无 WAL / 崩溃撕裂 | §5 环形日志 + 事务协议 + 恢复重放 |
| 无 fsync | §9 fsync/sync 真实化 + virtio FLUSH |
| 无校验和/双超级块 | §3 双超级块 + CRC32；§4 inode CRC |
| 无磨损均衡 | §7 位图 RAM 权威 + 检查点批量刷盘 + 轮转分配 + 磨损偏置 |
| 4.2MB 上限 | §8 u64 size + 三间接（≈4.4TB） |
| 无 fsck | §11 sfs_check |
| 128MB 位图钳制 | §2 多块位图 |
