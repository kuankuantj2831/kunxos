# KOS — KunxOS Object Storage

KUNXOS 的对象存储层。设计文档见 [DESIGN.md](DESIGN.md)。

**当前状态：Phase 1 Step 2 + P1/P2 强化**（v2 引擎：可变记录 + 快速挂载）。

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

### v2 引擎强化（2026-10-17，P1/P2）

- **区段化布局**：对象 = 头块 + 区段表链 + 任意位置的数据/索引区段
  （`format/extent.rs`）。分配 first-fit 任意区段 → **无碎片化死区**
  （碎片池上放大对象测试通过）。
- **可变记录**（`object_flags::MUTABLE`）：`write_mutable` / `append` /
  `write_range`。append 成本 O(索引块数)——旧叶子哈希来自磁盘索引块，
  不重读不重哈希旧内容；先填满旧尾块再开新 chunk。可变记录键 =
  SHA-256(Merkle 根 ‖ 头块号)，同内容记录互不别名。
- **快速挂载（P2）**：持久空闲位图 + 对象索引链 + 超级块 hint
  （`store/persist.rs`，置于 Record Pool 区）。hint seq == 活动
  manifest generation 才走快速路径，否则回退权威全池扫描。
  **2TB 稀疏卷实测：快速挂载 16s（debug），回退路径是小时级**。
- **容量/久用压测**（`tests/capacity.rs`）：2TB 卷挂载+读写+重挂载
  全链路；churn 压测 200 次追加 245ms、全量内容逐字节一致、碎片池
  放大对象成功。
- 系统调用号段扩至 **460..=467**（467 = append；462 的 a3 位 16 =
  可变记录标志）。QEMU 实跑 `run /bin/kostest` PASS（含 append）。
- 坑位记录：超级块 CRC 现覆盖 reserved 区（hint 所在；reserved 全零
  时与旧镜像字节兼容）；`free_blocks()` 改增量计数（2TB 全量遍历
  每次提交数秒，曾致 append 挂死）。

### P1..P6 全量落地（2026-10-17 晚）

- **P1 持久化**：virtio-blk 多设备（sda..sdd，blk_lock 串行下的上下文
  槽切换）；`kos_vfs::boot_mount` 优先挂载持久磁盘（KOS 超级块探测），
  无盘回退内嵌 RAM 卷。`make kos-disk` 生成 256MB 持久卷并挂入
  `make run`。**QEMU 双启动实测：journal 跨重启逐字节保留**。
- **P2 GC**：`KosStore::gc()` 从活动 manifest 根标记-清扫（可达闭包
  之外全回收），清扫后提交持久索引。kosctl `gc`。
- **P3 读路径**：256 块缓存（写路径同步更新，块复用无脏读——曾因
  写路径绕过缓存翻车，已全量统一走 `read_blk/write_blk`）；Full 校验
  已验证集（同版本不重复重建树）；`write_range` 快速路径只读写受
  影响 chunk（窗口 ≤64 chunk、空隙 ≤1 chunk），免全量内容拼接。
- **P4 POSIX 补齐**：`truncate`（收缩清零尾 chunk 防残留、扩展补零）；
  mtime 记于头块 `reserved[0..4]`（秒）；`list_skip` 分页（readdir
  cookie）。系统调用 **460..=469**（468=truncate、469=分页 list）。
- **P5 加固**：损坏注入模糊测试（200 轮随机翻转，挂载/读/GC 零 panic
  零挂死）；kosctl `check`（fsck：全树 Full 验证 + 孤儿报告 + 顺手
  GC）；`ManifestVerifier` 验签钩子（签名覆盖 manifest[8..120)，
  伪签名槽被拒后回退另一槽——测试覆盖；kunxsec Ed25519 签入待
  kunxpkg 提供签名工具链）。
- **P6 打磨**：目录条目写入维持有序 + resolve 二分查找。

### 自动增强轮（2026-10-17 深夜）

- **Ed25519 签名链闭环**：kunxsec 补齐 `public_key` / `sign`
  （RFC 8032 确定性 nonce，**测试向量 1/2 全过**）；引擎 `set_signer`
  提交期自动签名 + `resign` 重签；内核嵌入信任公钥与开发种子
  （`kos_trust_pk.rs`，`make kos-disk` 自动生成），挂载走
  `open_with_verifier` **严格模式**（信任锚配置后未签名槽一律拒绝）。
  kosctl 新增 `keygen` / `sign`。**坑：签名载荷不能含 signer_pk 自身**
  （自指签名验必挂），现覆盖 manifest[8..88)。
  QEMU 实证：写入 → 重启 → 严格验签挂载 → 数据逐字节读回。
- **shell 内置 `kos` 命令**：`kos ls/cat/stat/write/append`，系统内
  直接操作对象存储（QEMU 实测）。
- **kosctl `bench`**：性能护栏（append/write_range/挂载/GC 计时）。
  基线（宿主 debug）：append 761µs/次、write_range 795µs/次、
  快速挂载 12ms、GC 301 对象 1.5ms。

### FAT32 缺口补齐轮（2026-10-17 深夜二）

- **pread 随机读**（`read_range`）：按区段定位 chunk，只读受影响块
  ——读 1 字节不再整读全对象。syscall 470。
- **rename / move**：CAS 对象不动，只 CoW 目录链（同目录单链快路径、
  跨目录双链）。syscall 471。
- **fd 层**：`kos_open/read_fd/close_fd`（472..=474）——pid 绑定的
  fd 表 + 读位置记忆，顺序读到 EOF 返回 0。
- **小文件内联区段表**（v3 布局）：区段表并入头块偏移 96（表序列化
  3904B，尾部本为零，CRC 兼容）——**每对象省 1 块**（空间断言测试：
  20 个小对象 ≤63 块）。表膨胀越界自动回退链式块。
- shell `kos` 新增 `mv` / `head`（随机读演示）。
- 修 EOF 读被误判 EFAULT（0 字节 copy_to_user 返回 0）。

### 断电保护与断点续传轮（2026-10-18 凌晨）

- **断电注入测试矩阵**（`tests/crash.rs`）：CrashDev 在任意第 N 次块写
  后模拟掉电，**枚举 append / write_range / truncate / 上传推送的全部
  断点**，断言重挂载后对象内容 = 旧值或新值（绝无脏第三态）、二次
  重挂载稳定。矩阵抓出并修复两个真 bug：
  ① write_range/append 原地覆盖旧数据块（旧头 + 新数据 = 断电脏读）；
  ② truncate 提交前回收旧索引块，新索引块复用旧位置（cut=1 即毁旧对象）。
- **写时复制写路径**：受影响 chunk 一律写新块（`cow_write_window`：
  内容增量 = 前零 + 数据 + 后零），旧块永不覆写；**头块换新块提交**
  （旧头不动，新头块是唯一提交点）。可变记录的每一次修改在任意断点
  下都严格旧-或-新。
- **超级块损坏自愈**：布局可从设备容量纯推导（`mkfs_layout`），块 0
  打花后挂载自动重建（测试：整块 0xEE 后数据完好可写）。
- **断点续传**（`store/upload.rs` + syscall 476..=479 + kosctl
  `upbegin/uppush/upstat/upfin`）：part 预扩展 + 进度位图 sidecar；
  崩溃一致性 = 数据先落、位图后记（断电只欠记不超记，重传幂等）；
  finish 用 rename **原子发布**。**QEMU 双启动实测：传 20/40 块"关机"
  → 重启进度保留 → 续传 → 原子发布 → 全量校验 PASS**；推送断电矩阵
  （每个断点后续传完成）全绿。
- **错误处理**：系统调用统一 errno 映射（ENOENT/EACCES/ENOSPC/EIO/
  EEXIST…；UploadIncomplete 把 received/total 编进返回值供续传换算）；
  引擎写路径 panic 清零（介质错误走 Err）。
- 系统调用号段扩至 **460..=479**（475=write_range、476..=479=上传）。

### POSIX 桥接：全面接入（2026-10-18）

- **`/kos/` 路径路由**：应用的普通 `openat("/kos/…")/read/write/
  close/lseek` 由内核透明转发到 KOS 引擎——**零改造接入**。KOS fd
  号从 1000 起（避开进程 fd 表）；O_CREAT/O_TRUNC/O_APPEND 语义
  齐备；write 返回本次写入字节数（POSIX 语义）；SEEK_END =
  end + offset。
- 验收应用 `apps/kosdemo`：纯标准系统调用完成 创建→写→SEEK_SET
  读→SEEK_END 读→O_APPEND 追加，QEMU PASS（幂等：O_TRUNC 重置）。
- 系统调用总计 **460..=479** + 常规 Linux 号段的 `/kos/` 路由。

### FAT32 经典优化移植轮（2026-10-18 二）

- **F1 dentry 缓存**：路径 → (generation, 条目) 表，命名空间提交按代
  整体失效——热路径 open/write/read 免逐级目录块读+解码。
- **F2 顺序预读**：read_range 检测顺序流（offset 接续上次末尾）→
  预取后续 8 chunk 进块缓存（FAT 驱动 readahead 等价物）。
- **F3 分配局部性**：分配提示点起恰好够用 → 整段直取，append 流的
  块保持物理连续（区段少碎片、顺序 IO 友好）。
- **F4 低空间水位**：每次提交后 O(1) 检查，空闲 <10% 警告 / <3%
  严重（接 health 面板，各报一次不刷屏）。
- bench（宿主 debug，快设备收益有限）：append 776→757µs/次、
  整读 27→23µs；慢介质（virtio 磁盘）上预读/局部性收益更大。

### 流畅度轮（2026-10-18 三）

- **挂载**：位图块词级快解析（整 u64 词全 0/全 1 批量填充）——
  2TB 卷挂载 16.1s → **12.0s**（debug；release 约 1/5）。
- **写入**：`FlushPolicy`（Eager=逐提交 sync 恒久默认；Batched(n)=每
  n 次提交一次设备 FLUSH）。内核挂载即启用 Batched(64) + **500ms
  定时兜底**（with_store 内 O(1) 检查）——崩溃一致性不变（写序与
  原子提交照旧），持久性窗口有界。virtio 盘上每提交一次的
  VBLK_T_FLUSH 设备往返变成 1/64。
- **读取/加载**：open_path 走 dentry 缓存（热路径免逐级解析）。

### 深度优化轮（2026-10-18 四）

- **O1 索引块增量重写**：append/write_range/truncate 的 Merkle 索引
  与旧版**逐块字节比对**，相同块直接复用（不分配不写），只写差异
  块——追加场景即每层右侧路径块，O(全部索引) → **O(树深)**。1GB
  文件每次追加的索引 IO 从 ~8MB（2064 块）降到 ~3 块。复用安全：
  复用块与旧头引用逐字节一致，断电矩阵全绿背书。回收跳过复用块。
- **O2 free_cnt 持久化**：hint.reserved 改存空闲块计数，快速挂载
  免全量数位。2TB 挂载累计 **16.1s → 7.9s**（debug；剩余为
  Vec<bool> 构建，懒加载位图页是下一步）。
- bench：append 776→741µs/次（宿主小文件，大文件收益在 O1 量级）。

### 稳定记录键轮（2026-10-18 五）

- **免 manifest 追加**：可变记录的命名空间键在**创建时冻结**（跨代
  稳定）——追加不再触发目录链 CoW / manifest 换代 / 超级块换代。
  提交点 = 新头块写入 + 旧头清零；崩溃歧义（新旧头同键并存）由
  扫描侧**同键取最高 version** 消解，断电矩阵全绿背书。
  小写放大骤降：**append 776→445µs/次（-43%）、write_range
  782→453µs、churn 200 追加 245→145ms**；追加不再生产孤儿（GC
  测试改为反向护栏 + 覆盖孤儿验证）。
- **挂载**：free_cnt 持久化 + 稳定键（索引链不再换代）叠加——
  小卷挂载 **11.6ms→0.64ms（18×）**；2TB 8.1s（剩 Vec<bool>
  构建，懒加载位图页待做）。
- 坑：改 commit 尾部的 python 补丁锚点没匹配却**静默未应用**（症状：
  稳定键生效但位图不持久化 → 快速挂载指向已清零旧头块）——大改
  后必须 grep 验证 + 跑重挂载测试。

不包含：块缓存的 SMP 并发细化（现与 fs_lock 大锁绑定）、mmap、
getdents 的 /kos/ 目录枚举（用 `kos ls`）、多卷同时挂载、kunxpkg
.kunx 包对象模型迁移、Pack 小对象合并、SFS→KOS 根文件系统切换、
坏块重映射（需真实故障注入设备）。

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
