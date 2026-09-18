KOS: KunxOS Object Storage — 设计文档 v0.1
一、设计目标

KUNXOS 需要一个「最适配」的存储，不是「最先进」的存储。适配意味着：

    吃透已有约束：no_std、无 POSIX、per-app UID 沙箱、Ed25519 签名链、A/B OTA、512MB RAM、4KB 页、SD 擦除块

    解决真实问题：根卷 inode 耗尽、无去重、数据块无端到端校验、签名只在读取时刻、无版本链

    不推倒重来：SFS 的块管理、WAL、kunxsec 的密码学原件全部复用

    能用数据说话：设计里的每个决策对应一份 benchmark，能证明比现状好

不做：分布式、多设备同步、跨 OS 兼容、网络存储。
二、核心模型：双池

KUNXOS 的数据分两类，用两个池承载：
池	模型	承载	语义
Object Pool	不可变对象	系统镜像、应用包、照片、短信、联系人历史、文档	内容寻址、Merkle 校验、签名、版本链
Record Pool	可变记录	配置、状态、临时数据、当前位置、锁	inode + WAL、原地更新、无版本

判断标准：数据是否需要「历史」和「来源证明」？

    照片编辑 → 需要历史 → Object

    配置当前值 → 只有当前有意义 → Record

    短信时间线 → 需要历史 → Object

    电池电量 → 只有当前值 → Record

这条边界是设计的核心。OSTree、AVB、Git 全都把可变数据推给传统 FS，我们不推——我们内建 Record Pool。
三、磁盘格式
3.1 超级块（Block 0）
text

struct KosSuperblock {
    magic: u64,                 // "KOSV0001"
    version: u32,               // 1
    block_size: u32,            // 4096
    total_blocks: u64,
    free_blocks: u64,

    // Object Pool
    obj_pool_start: u64,
    obj_pool_blocks: u64,
    obj_free_head: u64,         // 空闲链表头

    // Record Pool
    rec_pool_start: u64,
    rec_pool_blocks: u64,
    rec_inode_count: u32,       // 动态扩容，无 256 硬上限
    rec_inode_free: u32,

    // Root Manifest 双槽
    root_slot_a: u64,           // 块号
    root_slot_b: u64,           // 块号

    // 校验
    crc32: u32,
}

关键改动：inode 不再硬编码，Record Pool 的 inode 数动态分配，位图可扩展。Object Pool 无 inode 概念——对象按内容哈希寻址，不需要 inode。
3.2 Object Header
text

struct KosObject {
    magic: u32,                 // "KO\0\0"
    obj_type: u16,              // FILE / DIR / SYMLINK / PACK / BLOB
    flags: u16,                 // COMPRESSED / PACKED / ENCRYPTED
    content_len: u64,
    content_hash: [u8; 32],     // BLAKE3 或 SHA-256

    owner_uid: u32,
    capability: u32,            // 权限位（与 per-app UID 对齐）
    created_at: u64,
    version: u64,

    // 存储：数据块列表（间接寻址见 3.3）
    block_count: u32,
    _pad: u32,
    indirect: u64,              // 指向索引块

    header_crc: u32,
}

3.3 Merkle 树（完整性核心）

数据按 4KB 分块，每块单独哈希，组织成多级树：
text

Root Hash (签名覆盖)
  ├── Level 2 Index Block (每块 512 个 hash → 512 个叶子)
  │     ├── Level 1 Index Block
  │     │     ├── Data Block 0  (hash)
  │     │     ├── Data Block 1  (hash)
  │     │     └── ...
  │     └── ...
  └── ...

读取验证：只需验证 root 签名 + 沿路径 O(log n) 个 hash。不需要每块验签。

写入：新数据产生新 hash 链，新 root。旧的保持不变（不可变）。
3.4 Root Manifest（版本链）
text

struct KosRootManifest {
    magic: u64,                 // "KOSROOT"
    generation: u64,            // 单调递增
    timestamp: u64,

    namespace_root_hash: [u8; 32],   // 命名空间 B-tree 根
    parent_manifest_hash: [u8; 32],  // 父代（版本链）

    signer_pk: [u8; 32],
    signature: [u8; 64],        // Ed25519，覆盖 generation + timestamp + 两个 hash

    crc32: u32,
}

双槽交替写：Slot A / Slot B。更新时写另一个槽，成功后切换指针。崩溃时读两个槽，选 generation 更大且验签通过的那个。

与 A/B OTA 对齐：两个槽天然就是两个 OTA 槽。
3.5 命名空间（路径 → 对象）

命名空间本身是一棵 Merkle B-tree：
text

struct NamespaceNode {
    magic: u32,
    node_type: u8,              // LEAF / INTERNAL
    entry_count: u16,
    entries: [NamespaceEntry; N],
    crc32: u32,
}

struct NamespaceEntry {
    name_hash: [u8; 16],        // BLAKE3-128(path_segment)，用于排序和查找
    obj_hash: [u8; 32],         // 指向 Object
    entry_type: u8,             // FILE / DIR / SYMLINK
    mode: u16,
}

路径解析：/data/photos/a.jpg → 依次在 B-tree 里查 data → photos → a.jpg，每步验证 Merkle 路径。
四、API
4.1 内核态 Object API
rust

pub trait ObjectStore {
    // 对象生命周期
    fn put(&self, content: &[u8], meta: &ObjectMeta) -> Result<ObjectId>;
    fn get(&self, id: &ObjectId) -> Result<Vec<u8>>;
    fn ref_inc(&self, id: &ObjectId) -> Result<()>;
    fn ref_dec(&self, id: &ObjectId) -> Result<()>;

    // 完整性
    fn verify(&self, id: &ObjectId) -> Result<bool>;
    fn verify_path(&self, root: &ManifestHash, id: &ObjectId) -> Result<bool>;

    // 命名空间
    fn lookup(&self, path: &Path) -> Result<ObjectId>;
    fn link(&self, path: &Path, id: &ObjectId) -> Result<()>;
    fn unlink(&self, path: &Path) -> Result<()>;

    // 版本
    fn commit(&self, signer: &Keypair) -> Result<Generation>;
    fn checkout(&self, gen: Generation) -> Result<()>;
    fn history(&self) -> Result<Vec<Generation>>;
    fn rollback(&self, steps: u32) -> Result<()>;
}

pub trait RecordStore {
    fn create(&self, path: &Path, mode: u16) -> Result<InodeId>;
    fn read(&self, ino: InodeId, off: u64, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, ino: InodeId, off: u64, buf: &[u8]) -> Result<usize>;
    fn fsync(&self, ino: InodeId) -> Result<()>;
    fn unlink(&self, ino: InodeId) -> Result<()>;
}

4.2 VFS 兼容层

VFS 保持 POSIX 语义，但内部路由到两个池：
rust

// 打开时根据路径前缀决定池
fn route(path: &Path) -> Pool {
    match path.as_str() {
        p if p.starts_with("/data/objects/") => Pool::Object,
        p if p.starts_with("/data/records/") => Pool::Record,
        p if p.starts_with("/system/") => Pool::Object,   // 系统只读
        p if p.starts_with("/tmp/") => Pool::Record,
        _ => Pool::Record,  // 默认 Record（兼容现有应用）
    }
}

现有应用不改代码：写 /data/oobe/theme 还是 open/write，落在 Record Pool。新应用可以用 obj:// 前缀显式走 Object Pool。
五、关键算法
5.1 去重

put(content) 时：

    算 BLAKE3(content)

    查哈希表（Object Pool 的索引）是否已有

    有 → 只增加 refcount

    无 → 分配块、写、Merkle 建树、入索引

包缓存收益最大：多个 .kunx 包共享的库文件只存一份。照片编辑后的新版本，未修改的块共享。
5.2 GC（三阶段）

Online GC（引用归零）：

    每个 Object 有 refcount

    namespace 引用、inode 引用都计入

    归零 → 加入待回收队列

Background GC（标记清扫）：

    空闲时从 Root Manifest 出发，标记所有可达对象

    未标记且 refcount=0 → 清扫

    分批进行，每批有预算，不影响前台

Compaction（合并）：

    小对象打包成 Pack（类似 Git packfile）

    Pack 是一个大 Object，内部有索引

    写放大超阈值时触发

5.3 崩溃恢复

启动时：

    读 Slot A / Slot B 的两个 Root Manifest

    校验 CRC + Ed25519 签名

    选 generation 更大且验证通过的

    用它的 namespace_root 恢复命名空间

    Record Pool 用 WAL 重放（SFS 已有）

如果两个槽都验证失败：进恢复模式，提示用户。绝不静默降级。
六、与现有系统集成
系统	集成方式
VFS	路由层按路径前缀分发；Record Pool 保持 POSIX 语义，Object Pool 提供扩展 API
SFS v2	降级为 Block Layer 的实现；WAL、位图、块分配全部复用
kunxsec	Ed25519 验签直接复用；SHA-256 保留，BLAKE3 作为可选优化
kunxpkg	.kunx 包 = 一个 Root Object + 子对象树；安装 = 解包 + 链接 namespace；验证 = root 签名 + Merkle
OTA	每次 OTA = 新 Root Manifest；A/B 槽 = Manifest 双槽；回滚 = checkout 旧 generation
per-app UID	Object header 的 capability 字段；内核 open 时检查
IPC	从文件移到内核 channel / port，不再消耗 inode
七、迁移路径

Phase 1（3-6 月）：只读对象存储

    改造 Block Layer（数据块 CRC、擦除块对齐、TRIM 预留）

    定义磁盘格式（Superblock / Object / Manifest / Namespace）

    实现 put / get / verify

    宿主单测 + 无头 QEMU 测试

    不动现有 VFS，双写验证

Phase 2（3-6 月）：命名空间 + VFS 兼容

    Merkle B-tree 命名空间

    VFS 路由层

    Record Pool 独立 inode 分配（去掉 512 硬上限）

    与现有 SFS 并存，逐应用迁移

Phase 3（6-12 月）：GC + 系统迁移

    GC / compaction

    kunxpkg 迁移到对象模型

    OTA 迁移到 Root Manifest

    系统镜像迁移（rootfs 变成对象树）

    IPC 从文件移到内核 channel

Phase 4（持续）：优化

    压缩、加密

    去重效果评估

    性能调优

    移除旧 SFS 代码路径

八、工程量估算
模块	行数（Rust）
Block Layer 改造	~2,000
Object Layer	~5,000
Merkle 树	~1,000
Namespace B-tree	~3,000
VFS 兼容层	~2,000
GC / Compaction	~2,000
用户态工具	~2,000
测试	~5,000
合计	~22,000

加上文档、工具、迁移脚本，约 25,000 行。

对比：现有 kunxcore 是 46,000 行。这是半个内核的工程量。

周期：一个人全职做，18-24 个月。多个 AI 会话并行，可能压缩到 6-12 个月（如果并行策略做对）。
九、未决问题（需要你决策）

    BLAKE3 还是 SHA-256？ BLAKE3 快 5-10 倍，嵌入式友好，但要自己实现或引入 no_std 版。SHA-256 已有。我建议 BLAKE3，性能收益值这个工程量。

    Merkle 树分支因子？ 二进制树 vs 宽树（每索引块 512 个 hash）。我建议宽树，减少层数，AVB 也这么做。

    加密？ 可选，per-object 或 per-volume。我建议暂不做，等基础稳定。

    Record Pool 和 Object Pool 是否分开设备？ 分开可以独立 GC、独立性能调优，但增加复杂度。我建议同一设备，逻辑分区。

    命名：KOS（KunxOS Object Storage）是我暂定的名字。你可以换。

十、这份设计的代价

诚实说清楚：

    GC 是长期负担。Git、OSTree、LSM 全都为此付出大量代码。你要做好心理准备：GC 的 bug 会比 FS 本身还多。

    每对象开销。header + hash + Merkle 路径，小对象空间放大严重。Pack 机制是必须的，不是可选的。

    迁移痛苦。Phase 2-3 是双写并存期，两套存储同时跑，内存和复杂度都翻倍。

    性能未验证。virtio-blk 上算 BLAKE3 + 走 Merkle 验证的成本没有实测。Phase 1 必须先做 benchmark，如果太慢，整个设计要重新考虑。

    SMP 未解决。现有 SFS 的粗粒度锁问题，KOS 一样要面对。Object Pool 的并发写入需要新设计。
