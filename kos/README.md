# KOS — KunxOS Object Storage

KUNXOS 的对象存储层。设计文档见 [DESIGN.md](DESIGN.md)。

**当前状态：Phase 1 Step 2**（存储引擎 + 宿主工具 + 内核集成）。

- `src/store/`：**读写引擎**。Object Pool 连续布局（头块 + 数据 + Merkle
  索引块）、CAS 去重（SHA-256 内容寻址）、目录即对象的无上限命名空间
  （无 78 entry / 512 inode 限制）、Root Manifest 双槽崩溃安全提交、
  Unix 风格权限位（`perm` + `check_access`）、`VerifyLevel::Full`
  读取路径全量 Merkle 重建比对。
- `src/block/file.rs` + `src/bin/kosctl.rs`：宿主 CLI
  （mkfs / import / export / ls / put / get / unlink / verify / stat）。
  实测：11MB rootfs 树 0.9s 导入、全树严格验证 0.8s、导出与源树
  byte-identical（`diff -r` 为空）。
- 内核集成（`kunxcore/src/kos_vfs.rs`）：块设备适配器 + 私有系统调用
  号段 **460..=466**（mount/read/write/mkdir/unlink/list/stat），启动时
  自动挂载内嵌 KOS 卷（`make kosdisk` 生成 kosdisk.img，主设备号 254），
  与 SFS 双跑（Phase 1 计划）。用户态验收：`/bin/kostest`（QEMU 实跑
  KOSTEST PASS）。

不包含：GC / compaction、kunxpkg 对象模型迁移、Ed25519 验签集成。

磁盘格式与 CPU 架构解耦：多字节字段一律小端（RISC-V RV64 / ARM64 均为小端）。
KUNXOS 已决策切换 RISC-V 目标架构，本 crate 无需任何改动。

## 目录结构

```
kos/
├── Cargo.toml
├── DESIGN.md               # 设计文档（未随本步修改）
├── README.md
├── src/
│   ├── lib.rs              # no_std 门控、模块声明、文档
│   ├── error.rs            # KosError（无 String，no_std 友好）
│   ├── format/             # 磁盘格式：纯数据结构 + 定长序列化
│   │   ├── mod.rs          #   字节读写/CRC 辅助
│   │   ├── superblock.rs   #   KosSuperblock 4096B（Block 0）
│   │   ├── object.rs       #   KosObject 96B（对象头）
│   │   ├── manifest.rs     #   KosRootManifest 192B（版本链根）
│   │   ├── namespace.rs    #   NamespaceNode 4096B / NamespaceEntry 52B
│   │   └── merkle.rs       #   MerkleIndexBlock 4096B（宽树索引块）
│   ├── hash/               # Hasher trait + BLAKE3 / SHA-256
│   ├── merkle/             # MerkleTree::build / verify_chunk / verify_all
│   └── block/              # BlockDevice trait（trait.rs）+ MemBlockDevice
├── tests/
│   ├── format_roundtrip.rs #   往返 + 大小断言 + 块设备
│   ├── merkle.rs           #   全矩阵（含三层 16130-chunk 树）
│   └── tamper.rs           #   逐字节翻转必被检测
└── benches/
    ├── hash.rs             #   BLAKE3 vs SHA-256 吞吐（criterion）
    └── merkle.rs           #   构建 / 单块验证 / 全量验证
```

## 布局常量

| 结构 | 大小 | magic | CRC 偏移 | 说明 |
|------|------|-------|----------|------|
| `KosSuperblock` | 4096 | `KOSV0001` | 96 | 块 0；双 Root 槽在块 1/2 |
| `KosObject` | 96 | `KO\0\0` | 88 | content_hash + capability + indirect |
| `KosRootManifest` | 192 | `KOSROOT\0` | 184 | 块内包装（置于 4096 槽首）属后续步骤 |
| `NamespaceNode` | 4096 | `KNS\0` | 4064 | ≤78 entry；未用槽位必须全零 |
| `NamespaceEntry` | 52 | —（无 CRC） | — | 篡改检测由所在节点 CRC 承担 |
| `MerkleIndexBlock` | 4096 | `KMIX` | 4076 | 127×32B 哈希 + 12B 头 + 4B CRC + 16B 预留 |

- 多字节标量小端；`repr(C)` 仅声明内存布局，字节布局由各文件偏移常量决定；
  **全库 `#![forbid(unsafe_code)]`，序列化按字段逐一 `copy_from_slice`**。
- CRC 语义：CRC 字段自身**不参与字节流**（生成与校验同一函数
  `crc32_skip_field`）。`to_bytes` 总是重算 CRC。
- Merkle：数据按 4KB 分块 → 每块哈希 → 每 127 个哈希一个索引块 →
  逐层向上；root = 顶层索引块**序列化字节**的哈希；空数据也有定义
  （一层 `count=0` 块）。树几何唯一由 `level_block_counts` 定义，
  构建与验证共用。

## 命令

```sh
cargo build                              # 宿主构建（std）
cargo build --no-default-features        # no_std 构建
cargo test                               # 61 个测试（单元+集成+文档）
cargo bench                              # criterion 基准
```

## 基准快照（2026-09-18，rustc 1.98.1，x86_64 宿主，仅供相对比较）

哈希吞吐（`hash32`）：

| 输入 | BLAKE3 | SHA-256 |
|------|--------|---------|
| 4 KiB | ~2.35 GiB/s | ~1.79 GiB/s |
| 64 KiB | ~4.29 GiB/s | ~1.87 GiB/s |
| 1 MiB | ~4.08 GiB/s | ~1.89 GiB/s |
| 8 MiB | ~3.56 GiB/s | ~1.91 GiB/s |

Merkle：

| 操作 | BLAKE3 | SHA-256 |
|------|--------|---------|
| 构建 1 MiB | 417 µs（2.34 GiB/s） | 550 µs（1.78 GiB/s） |
| 构建 8 MiB | 3.38 ms（2.31 GiB/s） | 4.42 ms（1.77 GiB/s） |
| 单块路径验证（8 MiB 树） | 9.0 µs | 4.4 µs |
| 全量验证 1 MiB | 386 µs | — |

观察：本机 SHA-256 有 SHA-NI 硬件加速，BLAKE3 优势约 1.9–2.3×
（设计文档预期的 5–10× 是无硬件加速场景）；单块验证成本由
「3 次 4KB 哈希 + 2 次索引块序列化（含 CRC32）」构成，两种算法都在
10 µs 内，不构成读取路径瓶颈。设计文档未决问题 №1（BLAKE3 vs
SHA-256）的数据基础已具备；RISC-V 目标机（无 SHA-NI）上需重测。

## 与设计文档的偏差

1. **宽树分支因子 127，不是 512。** 设计文档 3.3 的「每块 512 个
   hash」与 4KB 块自相矛盾（512×32B=16KB）。取 4KB 索引块容纳
   127 哈希 + 12B 头 + 4B CRC + 16B 预留。
2. **`MerkleIndexBlock` 自描述头**（magic/level/count/index）与 CRC：
   设计只画了树形未定索引块内格式；这让索引块也能通过
   `from_bytes` 的完整性校验，且验证时防御几何伪造。
3. **显式补齐字段**（`reserved`/`_pad`）：消除 `repr(C)` 隐式填充，
   大小断言精确成立。
4. **空数据语义**：0 chunk 也建一层 `count=0` 索引块，根哈希无歧义。
5. **Ed25519 不在本步**：依赖白名单（blake3/sha2/crc32fast/criterion）
   不含签名库，`signature` 为不透明 64 字节，`from_bytes` 只验
   CRC+magic。
6. **`from_bytes` 严格要求长度恰为 SIZE**；manifest 装入 4096 块槽的
   包装约定（置于槽首、尾部置零）由测试演示，正式化属后续步骤。
7. **超级块语义校验**：version 必须 1、block_size 必须 4096。

## 未决问题（留给后续步骤）

1. `NamespaceNode` 内部节点如何携带子指针/子哈希？设计字段缺失，
   若为 children 预留空间则 78 entry 上限需下调。
2. Root 双槽的崩溃切换状态机与 manifest 的块内包装正式化。
3. Ed25519 验签集成：复用 kunxsec，还是扩白名单引入 ed25519 库？
4. 对象头、数据块、索引块在 Object Pool 中的具体摆放（头在首块还是
   独立元数据区）；空闲链表（`obj_free_head`）的盘上格式。
5. `content_hash`（整内容哈希，CAS 去重键）与 Merkle root 的分工
   确认：对象头只存前者，后者经 `indirect` 链到达——签名覆盖哪个？
6. RISC-V 目标机（预期无 SHA-NI）上重跑本基准，为算法选型定论。
