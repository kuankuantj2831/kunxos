//! 对象存储引擎 v2：区段化布局 + 可变记录（append / write_range）。
//!
//! ## 磁盘布局（v2）
//!
//! 卷布局沿用 [`KosSuperblock::mkfs_layout`]。对象在池中**不再要求连续**：
//!
//! ```text
//! [header 块][区段表块链][数据区段（任意位置）][索引块区段（任意位置）]
//! ```
//!
//! - 区段表（[`crate::format::extent`]）记录全部数据/索引区段、Merkle
//!   根与 chunk 总数；头块 `indirect` 指向链首表块。
//! - 分配 = 空闲位图 first-fit 任意区段 → **无碎片化死区**。
//!
//! ## 不可变对象 vs 可变记录
//!
//! - 不可变对象（put/write）：CAS 语义不变，`content_hash` = SHA-256，
//!   全局去重。
//! - 可变记录（`write_mutable`，flags `MUTABLE`）：`content_hash` 存
//!   **Merkle 树根**，不做 CAS 去重。支持：
//!   - [`append`](KosStore::append)：新数据挂新区段，索引块重写，
//!     成本 O(索引块数)，与旧数据量无关（旧叶子哈希来自索引块，
//!     不重读数据、不重哈希内容）；
//!   - [`write_range`](KosStore::write_range)：原地改写受影响 chunk
//!     块，成本 O(改动 + 索引块数)。
//!
//! ## 崩溃安全
//!
//! append / write_range 的写序：新数据块 → 新索引块 → 新区段表 →
//! 头块指针切换（提交点）→ 回收旧区段（仅内存位图）。崩溃时旧对象
//! 完整可读（旧表/旧索引未被覆盖），半成品变孤儿等待 GC。
//!
//! ## 挂载
//!
//! 扫描全池找合法头块 → 读区段表 → 标记占用。区段表损坏的对象视同
//! 不存在（块回收）。命名空间双槽 manifest 择优不变。
//!
//! ## 权限模型
//!
//! 与 v1 一致：`KosObject.capability` 为 Unix 风格权限位（见 [`perm`]），
//! 路径操作按调用者 `uid` 检查。读取完整性由 [`VerifyLevel`] 控制。

use crate::error::KosError;
use crate::format::extent::{Extent, ExtentTable, KIND_DATA, KIND_INDEX};
use crate::format::manifest::{KosRootManifest, MANIFEST_MAGIC, MANIFEST_SIZE};
use crate::format::merkle::{MerkleIndexBlock, MERKLE_INDEX_BLOCK_SIZE};
use crate::format::object::{KosObject, ObjectType, OBJECT_HEADER_SIZE};
use crate::format::superblock::KosSuperblock;
use crate::hash::{Hasher, Sha256Hasher};
use crate::merkle::{level_block_counts, MerkleTree, DATA_CHUNK_SIZE};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

pub mod persist;
pub mod upload;

/// 卷块大小（与格式层一致）。
const BLK: usize = crate::format::KOS_BLOCK_SIZE;

// ==================== 权限位 ====================

/// 权限位定义（`KosObject.capability`，Unix rwx 同构）。
pub mod perm {
    /// 属主读。
    pub const OWNER_R: u32 = 0o400;
    /// 属主写。
    pub const OWNER_W: u32 = 0o200;
    /// 属主执行/穿越目录。
    pub const OWNER_X: u32 = 0o100;
    /// 其他读。
    pub const OTHER_R: u32 = 0o004;
    /// 其他写。
    pub const OTHER_W: u32 = 0o002;
    /// 其他执行/穿越目录。
    pub const OTHER_X: u32 = 0o001;
    /// 人人可读。
    pub const ALL_R: u32 = OWNER_R | OTHER_R;
    /// 属主读写、其他只读（系统缺省）。
    pub const DEFAULT: u32 = OWNER_R | OWNER_W | OTHER_R;
    /// 属主 rwx、其他 r-x（程序与脚本）。
    pub const EXEC: u32 = OWNER_R | OWNER_W | OWNER_X | OTHER_R | OTHER_X;
    /// 目录缺省：属主 rwx、其他 r-x。
    pub const DIR_DEFAULT: u32 = EXEC;
}

/// 能力检查：`uid` 为访问者，`want` 为要求的权限位（自动按属主/其他
/// 归位后判读）。
pub fn check_access(uid: u32, owner_uid: u32, capability: u32, want: u32) -> bool {
    let mask = if uid == owner_uid { 0o700 } else { 0o007 };
    capability & want & mask == want & mask
}

// ==================== 冲刷策略（写流畅度） ====================

/// Eager 每次提交 sync（最强持久性）；Batched 每 `n` 次提交或显式
/// `sync_now` 才落盘（FAT 驱动 write-back 等价物；崩溃一致性不变——
/// 写序与原子提交依旧，只是持久性窗口放宽到 n 次提交）。
#[derive(Debug, Clone, Copy)]
pub enum FlushPolicy {
    Eager,
    Batched(u32),
}

// ==================== 读校验强度 =====================

/// 读取路径的完整性校验强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyLevel {
    /// 每次读取重建 Merkle 树与磁盘索引块逐块比对（最严格）。
    Full,
    /// 只验证结构 CRC（头块 + 区段表）。不可变对象另做内容 SHA-256。
    HashOnly,
}

// ==================== 命名空间条目 ====================

/// 目录条目（内存形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// 名字（UTF-8，不含 `/`）。
    pub name: String,
    /// 条目类型。
    pub entry_type: ObjectType,
    /// 权限位（见 [`perm`]）。
    pub capability: u32,
    /// 属主 uid。
    pub owner_uid: u32,
    /// 指向对象的哈希（不可变 = SHA-256；可变记录 = Merkle 根）。
    pub content_hash: [u8; 32],
}

/// 名字最大长度（字节）。
pub const MAX_NAME: usize = 255;

/// 区段表内联标志（引擎 v3 小对象布局）：头块 96B 之后直接放区段表
/// （表序列化 3904B，头块剩余 4000B 装得下）→ 每对象省 1 块。
/// `KosObject.indirect == 0` 表示内联。
pub const OBJECT_FLAG_INLINE_TABLE: u16 = 0x0010;

impl DirEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.name.len() as u8);
        out.extend_from_slice(self.name.as_bytes());
        out.extend_from_slice(&self.entry_type.as_u16().to_le_bytes());
        out.extend_from_slice(&self.capability.to_le_bytes());
        out.extend_from_slice(&self.owner_uid.to_le_bytes());
        out.extend_from_slice(&self.content_hash);
    }

    fn encode_all(entries: &[DirEntry]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            e.encode(&mut out);
        }
        out
    }

    fn decode_all(data: &[u8]) -> Result<Vec<DirEntry>, KosError> {
        if data.len() < 4 {
            return Err(KosError::CorruptNamespace);
        }
        let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut out = Vec::new();
        let mut i = 4usize;
        while i < data.len() {
            let nlen = data[i] as usize;
            i += 1;
            if nlen == 0 || nlen > MAX_NAME || i + nlen + 42 > data.len() {
                return Err(KosError::CorruptNamespace);
            }
            let name = core::str::from_utf8(&data[i..i + nlen])
                .map_err(|_| KosError::CorruptNamespace)?
                .into();
            i += nlen;
            let entry_type = ObjectType::from_u16(u16::from_le_bytes([data[i], data[i + 1]]))?;
            i += 2;
            let capability = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            i += 4;
            let owner_uid = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            i += 4;
            let mut content_hash = [0u8; 32];
            content_hash.copy_from_slice(&data[i..i + 32]);
            i += 32;
            out.push(DirEntry {
                name,
                entry_type,
                capability,
                owner_uid,
                content_hash,
            });
        }
        if out.len() != count {
            return Err(KosError::CorruptNamespace);
        }
        Ok(out)
    }
}

// ==================== 内存索引 ====================

/// 挂载扫描得到的对象索引项。
#[derive(Debug, Clone)]
struct ObjIndex {
    header_block: u64,
    header: KosObject,
    /// 数据区段（按 chunk 顺序）。
    data_extents: Vec<Extent>,
    /// 索引块区段（第 0 层起逐层向上）。
    index_extents: Vec<Extent>,
    /// 区段表块（含链）。
    table_blocks: Vec<u64>,
    /// Merkle 树根（区段表链首权威）。
    merkle_root: [u8; 32],
}

/// 可变记录的命名空间键：SHA-256(root ‖ header_block)。
/// 纯 Merkle 根会撞车（两条同内容记录共享根），复合键让每条记录
/// 独立寻址；条目里存的就是这个键。
fn record_key(hasher: &Sha256Hasher, root: &[u8; 32], header_block: u64) -> [u8; 32] {
    let mut buf = [0u8; 40];
    buf[..32].copy_from_slice(root);
    buf[32..].copy_from_slice(&header_block.to_le_bytes());
    hasher.hash32(&buf)
}

/// 索引块总数（各层之和；0 chunk 也有 1 个 count=0 块）。
fn index_block_count(total_chunks: usize) -> usize {
    level_block_counts(total_chunks.max(1)).iter().sum()
}

/// Manifest 验签钩子（P5：Ed25519 接入点）。
///
/// 签名覆盖 manifest 映像 `[8..120)`（generation + timestamp +
/// namespace_root_hash + parent_manifest_hash），即 `signer_pk` 之前的
/// 全部受信字段。挂载时对带签名（`signer_pk` 非零）的槽校验，失败槽
/// 视为无效。默认 [`AcceptAllSignatures`]（不验签，兼容创世代）。
pub trait ManifestVerifier {
    /// 校验已签名槽（`signer_pk` 非零）；返回 false 该槽被拒。
    fn verify_manifest(&self, m: &KosRootManifest) -> bool;
    /// 严格模式：未签名槽也拒绝（部署方配置了信任锚时启用）。
    fn strict(&self) -> bool {
        false
    }
}

/// 接受一切签名（无信任锚的缺省）。
pub struct AcceptAllSignatures;
impl ManifestVerifier for AcceptAllSignatures {
    fn verify_manifest(&self, _m: &KosRootManifest) -> bool {
        true
    }
}

/// Manifest 签名回调：输入待签载荷（受信字段区），返回
/// (签名者公钥, 签名)。每次 manifest 提交时调用——部署方持有私钥
/// 时可保证所有代都有签名（严格验签模式即持续可用）。
pub type ManifestSignerFn = fn([u8; 80]) -> ([u8; 32], [u8; 64]);

/// 签名的规范消息：manifest[8..88)——generation + timestamp +
/// namespace_root_hash + parent_manifest_hash。**不含** signer_pk /
/// signature / CRC（签名不能覆盖自身所在的公钥字段）。
pub fn manifest_sign_payload(m: &KosRootManifest) -> [u8; 80] {
    let b = m.to_bytes();
    let mut out = [0u8; 80];
    out.copy_from_slice(&b[8..88]);
    out
}

// ==================== 存储引擎 ====================

/// KOS 卷句柄。一个设备同时只应有一个活跃实例。
pub struct KosStore<B: crate::block::BlockDevice> {
    dev: B,
    hasher: Sha256Hasher,
    sb: KosSuperblock,
    /// 当前活动 manifest 是否为槽 A。
    active_slot_a: bool,
    manifest: KosRootManifest,
    /// 哈希 → 索引项（不可变对象按 SHA-256；可变记录按 Merkle 根）。
    objects: BTreeMap<[u8; 32], ObjIndex>,
    /// 池内空闲块位图（true = 空闲；下标 = 块号 - obj_pool_start）。
    free: Vec<bool>,
    /// 分配搜索提示（上次分配结束位置，摊还 first-fit 扫描成本）。
    alloc_hint: usize,
    /// 脏位图块标记（下标 = 位图块号；挂载后按需持久化）。
    bitmap_dirty: Vec<bool>,
    /// 是否来自快速挂载路径（决定 commit 时是否重写全量位图）。
    fast_mounted: bool,
    /// 空闲块计数（增量维护；O(1) 读取，避免亿级位图遍历）。
    free_cnt: u64,
    /// 块缓存（LRU 粗粒度：满则整清）。键为绝对块号。
    cache: BTreeMap<u64, [u8; BLK]>,
    /// 块缓存容量上限（块）。
    cache_cap: usize,
    /// 已通过 Full 校验的对象键（重复读免重建树）。
    verified: BTreeSet<[u8; 32]>,
    /// Manifest 签名回调（None = 不签名）。
    signer: Option<ManifestSignerFn>,
    /// dentry 缓存：路径 → (generation, 条目)。命名空间提交后按代失效。
    dentry_cache: BTreeMap<alloc::string::String, (u64, DirEntry)>,
    /// 预读游标（F2 顺序读检测）：上次 read_range 的 (hash, offset)。
    ra_last: Option<([u8; 32], u64)>,
    /// 冲刷策略。
    flush_policy: FlushPolicy,
    /// Batched 模式自上次冲刷以来的提交数。
    dirty_commits: u32,
    verify: VerifyLevel,
    /// 提交时间戳来源。
    clock: u64,
}

impl<B: crate::block::BlockDevice> KosStore<B> {
    // ---------- 建盘 / 挂载 ----------

    /// mkfs：初始化超级块 + 空根目录 + 创世代 manifest（槽 A）。
    pub fn mkfs(dev: B) -> Result<Self, KosError> {
        check_blk(dev.block_size())?;
        let total = dev.block_count();
        let mut sb = KosSuperblock::mkfs_layout(total);
        sb.free_blocks = sb.obj_pool_blocks;
        let pool_blocks = sb.obj_pool_blocks as usize;

        let mut s = KosStore {
            hasher: Sha256Hasher,
            sb,
            active_slot_a: true,
            manifest: KosRootManifest::genesis([0u8; 32], 0),
            objects: BTreeMap::new(),
            free: alloc::vec![true; pool_blocks],
            alloc_hint: 0,
            bitmap_dirty: alloc::vec![true; 1],
            fast_mounted: false,
            free_cnt: pool_blocks as u64,
            cache: BTreeMap::new(),
            cache_cap: 256,
            verified: BTreeSet::new(),
            signer: None,
            dentry_cache: BTreeMap::new(),
            ra_last: None,
            flush_policy: FlushPolicy::Eager,
            dirty_commits: 0,
            verify: VerifyLevel::HashOnly,
            clock: 0,
            dev,
        };
        // 空根目录对象（内容为 count=0 的条目表）。
        let root_hash = s.put(&[0u8; 4], ObjectType::Dir, 0, perm::DIR_DEFAULT)?;
        let genesis = KosRootManifest::genesis(root_hash, 0);
        write_manifest_slot(&mut s.dev, s.sb.root_slot_a, &genesis)?;
        s.dev.flush()?;
        s.manifest = genesis;
        s.persist_superblock()?;
        Ok(s)
    }

    /// 挂载：超级块 → 双槽择优 → 扫描对象池建索引。
    pub fn open(dev: B, verify: VerifyLevel) -> Result<Self, KosError> {
        Self::open_with_verifier(dev, verify, &AcceptAllSignatures)
    }

    /// 带验签挂载：`signer_pk` 非零的槽必须通过 `verifier`，否则该槽
    /// 视为无效（双槽择优时跳过）。验签器由部署方注入（内核侧接
    /// kunxsec Ed25519；宿主缺省接受一切）。
    pub fn open_with_verifier(
        dev: B,
        verify: VerifyLevel,
        verifier: &dyn ManifestVerifier,
    ) -> Result<Self, KosError> {
        check_blk(dev.block_size())?;
        let mut buf = [0u8; BLK];
        dev_read(&dev, 0, &mut buf)?;
        // 超级块损坏自愈：布局全部可推导（mkfs_layout 对同一容量是纯
        // 函数；manifest 在固定块 1/2；对象状态由扫描得出）→ 直接重建。
        // hint 在 reserved 内，重建后为空 → 快速路径自动退回权威扫描。
        let sb = match KosSuperblock::from_bytes(&buf) {
            Ok(sb) => sb,
            Err(_) => KosSuperblock::mkfs_layout(dev.block_count()),
        };

        let (manifest, active_slot_a) = {
            let ok_slot = |m: Option<KosRootManifest>| m.filter(|m| {
                if m.signer_pk == [0u8; 32] {
                    !verifier.strict()
                } else {
                    verifier.verify_manifest(m)
                }
            });
            let ma = ok_slot(read_manifest_slot(&dev, sb.root_slot_a)?);
            let mb = ok_slot(read_manifest_slot(&dev, sb.root_slot_b)?);
            match (ma, mb) {
                (Some(a), Some(b)) => {
                    if a.generation >= b.generation {
                        (a, true)
                    } else {
                        (b, false)
                    }
                }
                (Some(a), None) => (a, true),
                (None, Some(b)) => (b, false),
                (None, None) => return Err(KosError::NoValidManifest),
            }
        };

        // 挂载：优先快速路径（持久位图 + 对象索引），失败回退全池扫描。
        let pool_start = sb.obj_pool_start;
        let pool_end = pool_start + sb.obj_pool_blocks;
        let pool_blocks = sb.obj_pool_blocks as usize;
        let per = crate::store::persist::BITS_PER_BMP_BLOCK;
        let n_bmp_blocks = (pool_blocks + per - 1) / per;

        if let Some((objects, free)) = fast_mount(&dev, &sb, &manifest, pool_end) {
            let free_cnt = free.iter().filter(|f| **f).count() as u64;
            return Ok(KosStore {
                dev,
                hasher: Sha256Hasher,
                sb,
                active_slot_a,
                manifest,
                objects,
                free,
                alloc_hint: 0,
                bitmap_dirty: alloc::vec![false; n_bmp_blocks],
                fast_mounted: true,
                free_cnt,
                cache: BTreeMap::new(),
                cache_cap: 256,
                verified: BTreeSet::new(),
                signer: None,
                dentry_cache: BTreeMap::new(),
                ra_last: None,
                flush_policy: FlushPolicy::Eager,
                dirty_commits: 0,
                verify,
                clock: 0,
            });
        }

        // 权威恢复路径：扫描全池。
        let mut objects = BTreeMap::new();
        let mut free = alloc::vec![true; pool_blocks];
        let mut scan = [0u8; BLK];
        let mut b = pool_start;
        while b < pool_end {
            dev_read(&dev, b, &mut scan)?;
            if scan[0..4] == *b"KO\0\0" {
                if let Ok(hdr) = KosObject::from_bytes(&scan[..OBJECT_HEADER_SIZE]) {
                    if let Some(idx) = load_object(&dev, b, hdr, pool_end) {
                        mark_obj(&mut free, pool_start, &idx, false);
                        objects.insert(idx.header.content_hash, idx);
                    }
                }
            }
            b += 1;
        }
        let free_cnt = free.iter().filter(|f| **f).count() as u64;

        Ok(KosStore {
            dev,
            hasher: Sha256Hasher,
            sb,
            active_slot_a,
            manifest,
            objects,
            free,
            alloc_hint: 0,
            bitmap_dirty: alloc::vec![false; n_bmp_blocks],
            fast_mounted: false,
            free_cnt,
            cache: BTreeMap::new(),
            cache_cap: 256,
            verified: BTreeSet::new(),
            signer: None,
            dentry_cache: BTreeMap::new(),
            ra_last: None,
            flush_policy: FlushPolicy::Eager,
            dirty_commits: 0,
            verify,
            clock: 0,
        })
    }

    /// 重签当前活动 manifest（把 `signer_pk`/`signature` 写入活动槽，
    /// CRC 重算）。签名内容由调用方（宿主 kunxsec）计算。
    pub fn resign(
        &mut self,
        signer_pk: [u8; 32],
        signature: [u8; 64],
    ) -> Result<(), KosError> {
        self.manifest.signer_pk = signer_pk;
        self.manifest.signature = signature;
        let slot = if self.active_slot_a {
            self.sb.root_slot_a
        } else {
            self.sb.root_slot_b
        };
        write_manifest_slot(&mut self.dev, slot, &self.manifest)?;
        self.dev.flush()
    }

    /// 当前活动 manifest 的签名载荷（受信字段区）。
    pub fn manifest_payload(&self) -> [u8; 80] {
        manifest_sign_payload(&self.manifest)
    }

    /// 设置 manifest 签名回调（每次提交自动签名）。
    pub fn set_signer(&mut self, f: ManifestSignerFn) {
        self.signer = Some(f);
    }

    /// 设置冲刷策略。
    pub fn set_flush_policy(&mut self, p: FlushPolicy) {
        self.flush_policy = p;
    }

    /// 提交路径冲刷闸门。
    fn maybe_flush(&mut self) -> Result<(), KosError> {
        match self.flush_policy {
            FlushPolicy::Eager => self.dev.flush(),
            FlushPolicy::Batched(n) => {
                self.dirty_commits += 1;
                if self.dirty_commits >= n { self.sync_now() } else { Ok(()) }
            }
        }
    }

    /// 立即落盘（显式 fsync / 定时器兜底）。
    pub fn sync_now(&mut self) -> Result<(), KosError> {
        self.dirty_commits = 0;
        self.dev.flush()
    }

    /// 注入提交时间戳（单调微秒；由调用环境提供）。
    pub fn set_clock(&mut self, us: u64) {
        self.clock = us;
    }

    /// 设置读取校验强度。
    pub fn set_verify_level(&mut self, v: VerifyLevel) {
        self.verify = v;
    }

    /// 当前 generation。
    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    /// 对象总数（含未链接的孤儿）。
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// 取回块设备（丢弃句柄；用于测试/迁移）。
    pub fn take_device(self) -> B {
        self.dev
    }

    /// 对象池总块数。
    pub fn pool_blocks(&self) -> u64 {
        self.sb.obj_pool_blocks
    }

    /// 空闲块数（O(1)，增量维护）。
    pub fn free_blocks(&self) -> u64 {
        self.free_cnt
    }

    /// 本次挂载是否走了快速路径（P2 位图+索引）。
    pub fn was_fast_mounted(&self) -> bool {
        self.fast_mounted
    }

    /// 根目录 content_hash。
    pub fn root_hash(&self) -> [u8; 32] {
        self.manifest.namespace_root_hash
    }

    // ---------- 不可变对象层（CAS） ----------

    /// put：写入不可变对象，返回内容 SHA-256；同内容去重。
    pub fn put(
        &mut self,
        data: &[u8],
        obj_type: ObjectType,
        owner_uid: u32,
        capability: u32,
    ) -> Result<[u8; 32], KosError> {
        let hash = self.hasher.hash32(data);
        if self.objects.contains_key(&hash) {
            return Ok(hash);
        }
        let tree = MerkleTree::build(data, &self.hasher);
        let nchunks = tree.chunk_count();

        // 1) 数据块 → 2) 索引块 → 3) 区段表（可内联）→ 4) 头块（提交点）。
        let data_extents = self.write_chunk_extents(data)?;
        let index_extents = self.write_index_blocks(&tree)?;
        let loc = self.build_table(&data_extents, &index_extents, tree.root_hash(), nchunks as u32)?;
        self.dev.flush()?;
        let mut flags = 0u16;
        let (indirect, inline) = match &loc {
            TableLoc::Inline(b) => {
                flags |= OBJECT_FLAG_INLINE_TABLE;
                (0u64, Some(*b))
            }
            TableLoc::Chain(f) => (*f, None),
        };
        let hdr = self.make_header(obj_type, flags, &hash, data.len() as u64, nchunks as u32, indirect, owner_uid, capability);
        let header_block = self.alloc_blocks(1)?[0];
        self.write_header_block_inline(header_block, &hdr, inline.as_ref())?;
        let idx = load_object(&self.dev, header_block, hdr, self.pool_end())
            .ok_or(KosError::ExtentTableCorrupt)?;
        self.objects.insert(hash, idx);
        Ok(hash)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_header(
        &self,
        obj_type: ObjectType,
        flags: u16,
        content_hash: &[u8; 32],
        content_len: u64,
        block_count: u32,
        indirect: u64,
        owner_uid: u32,
        capability: u32,
    ) -> KosObject {
        KosObject {
            magic: crate::format::object::OBJECT_MAGIC,
            obj_type: obj_type.as_u16(),
            flags,
            content_len,
            content_hash: *content_hash,
            owner_uid,
            capability,
            created_at: self.clock,
            version: 1,
            block_count,
            _pad: 0,
            indirect,
            header_crc: 0,
            reserved: [0u8; 4],
        }
    }

    fn write_header_block(&mut self, block: u64, hdr: &KosObject) -> Result<(), KosError> {
        self.write_header_block_inline(block, hdr, None)
    }

    /// 写头块；`inline` 为内联区段表字节（置于偏移 96）。
    fn write_header_block_inline(
        &mut self,
        block: u64,
        hdr: &KosObject,
        inline: Option<&[u8; 3904]>,
    ) -> Result<(), KosError> {
        let mut hblock = [0u8; BLK];
        hblock[..OBJECT_HEADER_SIZE].copy_from_slice(&hdr.to_bytes());
        if let Some(t) = inline {
            hblock[96..96 + 3904].copy_from_slice(t);
        }
        self.write_blk(block, &hblock)?;
        self.dev.flush()
    }

    /// get by hash：读对象内容（按校验强度验证）。
    pub fn get(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>, KosError> {
        let idx = self
            .objects
            .get(hash)
            .ok_or(KosError::ObjectNotFound)?
            .clone();
        self.read_object_content(&idx)
    }

    /// 对象元数据。
    pub fn stat(&self, hash: &[u8; 32]) -> Result<KosObject, KosError> {
        self.objects
            .get(hash)
            .map(|i| i.header.clone())
            .ok_or(KosError::ObjectNotFound)
    }

    /// 对象是否为可变记录。
    pub fn is_mutable(&self, hash: &[u8; 32]) -> bool {
        self.objects
            .get(hash)
            .map(|i| i.header.flags & crate::format::object::object_flags::MUTABLE != 0)
            .unwrap_or(false)
    }

    fn read_object_content(&mut self, idx: &ObjIndex) -> Result<Vec<u8>, KosError> {
        let data = self.read_all_chunks(idx, idx.header.content_len as usize)?;
        let mutable =
            idx.header.flags & crate::format::object::object_flags::MUTABLE != 0;
        if !mutable {
            // CAS 完整性：内容 SHA-256。
            if self.hasher.hash32(&data) != idx.header.content_hash {
                return Err(KosError::HashMismatch {
                    context: "content",
                });
            }
        }
        // 已验证集：同版本对象只做一次全量校验（重复读免重建树）。
        if self.verify == VerifyLevel::Full && !self.verified.contains(&idx.header.content_hash) {
            self.verify_full(idx, &data)?;
            self.verified.insert(idx.header.content_hash);
        }
        Ok(data)
    }

    /// Full 校验：重建树 → 根比对 + 磁盘索引块逐块比对。
    fn verify_full(&mut self, idx: &ObjIndex, data: &[u8]) -> Result<(), KosError> {
        let tree = MerkleTree::build(data, &self.hasher);
        if tree.root_hash() != idx.merkle_root {
            return Err(KosError::HashMismatch {
                context: "merkle-root",
            });
        }
        let mut buf = [0u8; BLK];
        let mut cur = 0usize;
        for ext in &idx.index_extents {
            for k in 0..ext.count as u64 {
                self.read_blk(ext.start + k, &mut buf)?;
                let blk = tree
                    .index_blocks()
                    .nth(cur)
                    .ok_or(KosError::HashMismatch {
                        context: "index-geometry",
                    })?;
                if buf[..MERKLE_INDEX_BLOCK_SIZE] != blk.to_bytes()[..] {
                    return Err(KosError::HashMismatch {
                        context: "index-block",
                    });
                }
                cur += 1;
            }
        }
        if cur != index_block_count(idx.header.block_count as usize) {
            return Err(KosError::HashMismatch {
                context: "index-geometry",
            });
        }
        Ok(())
    }

    /// 顺序读全部数据 chunk（按区段拼接）。
    fn read_all_chunks(&mut self, idx: &ObjIndex, len: usize) -> Result<Vec<u8>, KosError> {
        let mut data = Vec::with_capacity(len);
        let mut buf = [0u8; BLK];
        for ext in &idx.data_extents {
            for k in 0..ext.count as u64 {
                self.read_blk(ext.start + k, &mut buf)?;
                let remain = len.saturating_sub(data.len());
                let end = core::cmp::min(BLK, remain);
                data.extend_from_slice(&buf[..end]);
            }
        }
        Ok(data)
    }

    /// 第 `chunk` 个数据块的绝对块号。
    fn chunk_block_no(idx: &ObjIndex, chunk: u32) -> Result<u64, KosError> {
        let mut c = chunk as u64;
        for ext in &idx.data_extents {
            if c < ext.count as u64 {
                return Ok(ext.start + c);
            }
            c -= ext.count as u64;
        }
        Err(KosError::ChunkIndexOutOfRange {
            index: chunk as usize,
            total_chunks: idx.header.block_count as usize,
        })
    }

    // ---------- 可变记录层 ----------

    /// 写可变记录（新建或整体替换）。返回当前 Merkle 根（即条目哈希）。
    pub fn write_mutable(
        &mut self,
        path: &str,
        data: &[u8],
        uid: u32,
        capability: u32,
    ) -> Result<[u8; 32], KosError> {
        match self.lookup_entry(path)? {
            Some(entry) => {
                if !check_access(
                    uid,
                    entry.owner_uid,
                    entry.capability,
                    perm::OWNER_W | perm::OTHER_W,
                ) {
                    return Err(KosError::PermissionDenied);
                }
                let root = self.put_mutable(data, uid, capability)?;
                self.update_entry_hash(path, root, ObjectType::File)?;
                Ok(root)
            }
            None => {
                let root = self.put_mutable(data, uid, capability)?;
                let (parent, name) = split_parent(path)?;
                let mut entries = self.list(parent)?;
                entries.push(DirEntry {
                    name: name.into(),
                    entry_type: ObjectType::File,
                    capability,
                    owner_uid: uid,
                    content_hash: root,
                });
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                self.replace_dir_and_commit(parent, &entries)?;
                Ok(root)
            }
        }
    }

    fn put_mutable(
        &mut self,
        data: &[u8],
        uid: u32,
        capability: u32,
    ) -> Result<[u8; 32], KosError> {
        let tree = MerkleTree::build(data, &self.hasher);
        let root = tree.root_hash();
        let nchunks = tree.chunk_count();
        let data_extents = self.write_chunk_extents(data)?;
        let index_extents = self.write_index_blocks(&tree)?;
        let loc = self.build_table(&data_extents, &index_extents, root, nchunks as u32)?;
        self.dev.flush()?;
        let header_block = self.alloc_blocks(1)?[0];
        let key = record_key(&self.hasher, &root, header_block);
        let (indirect, inline, flags) = match &loc {
            TableLoc::Inline(b) => (0u64, Some(*b), crate::format::object::object_flags::MUTABLE | OBJECT_FLAG_INLINE_TABLE),
            TableLoc::Chain(f) => (*f, None, crate::format::object::object_flags::MUTABLE),
        };
        let hdr = self.make_header(
            ObjectType::File,
            flags,
            &key,
            data.len() as u64,
            nchunks as u32,
            indirect,
            uid,
            capability,
        );
        self.write_header_block_inline(header_block, &hdr, inline.as_ref())?;
        let idx = load_object(&self.dev, header_block, hdr, self.pool_end())
            .ok_or(KosError::ExtentTableCorrupt)?;
        self.objects.insert(key, idx);
        Ok(key)
    }

    /// 追加写（仅可变记录）：新数据挂新区段，索引块重写。成本
    /// O(索引块数)：旧叶子哈希来自磁盘索引块，不重读、不重哈希旧内容。
    pub fn append(&mut self, path: &str, data: &[u8], uid: u32) -> Result<u64, KosError> {
        let entry = self.lookup_entry(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(
            uid,
            entry.owner_uid,
            entry.capability,
            perm::OWNER_W | perm::OTHER_W,
        ) {
            return Err(KosError::PermissionDenied);
        }
        let old_idx = self
            .objects
            .get(&entry.content_hash)
            .ok_or(KosError::ObjectNotFound)?
            .clone();
        if old_idx.header.flags & crate::format::object::object_flags::MUTABLE == 0 {
            return Err(KosError::NotMutable);
        }
        let new_len = old_idx
            .header
            .content_len
            .checked_add(data.len() as u64)
            .ok_or(KosError::OffsetInvalid)?;
        let old_chunks = old_idx.header.block_count as usize;

        // 1) 旧叶子哈希（读第 0 层索引块，不读数据）。
        let mut leaf_hashes = self.read_leaf_hashes(&old_idx, old_chunks)?;
        // 2) 先填满最后一个未写满的 chunk（顺序布局：新数据的前段属于
        //    旧尾块，不能直接开新 chunk）。
        let old_len_us = old_idx.header.content_len as usize;
        let tail = old_len_us % DATA_CHUNK_SIZE;
        let fill = if old_chunks > 0 && tail != 0 && !data.is_empty() {
            core::cmp::min(DATA_CHUNK_SIZE - tail, data.len())
        } else {
            0
        };
        let mut data_extents = old_idx.data_extents.clone();
        if fill > 0 {
            // 尾块填充走 CoW（旧尾块不覆写，断电安全）。
            let (ext, patched) = self.cow_write_window(
                &old_idx,
                old_chunks,
                old_chunks,
                old_len_us,
                old_len_us,
                &data[..fill],
                old_len_us + fill,
                new_len as usize,
            )?;
            data_extents = ext;
            for (c, h) in patched {
                leaf_hashes[c] = h;
            }
        }
        // 3) 剩余数据开新 chunk（全新块，天然 CoW）。
        let rest = &data[fill..];
        let new_chunks = if rest.is_empty() {
            0
        } else {
            (rest.len() + DATA_CHUNK_SIZE - 1) / DATA_CHUNK_SIZE
        };
        let new_extents = self.write_chunk_extents(rest)?;
        for c in 0..new_chunks {
            let start = c * DATA_CHUNK_SIZE;
            let end = core::cmp::min(start + DATA_CHUNK_SIZE, rest.len());
            leaf_hashes.push(self.hasher.hash32(&rest[start..end]));
        }
        data_extents.extend_from_slice(&new_extents);
        // 4) 重建树 + 新索引块 + 新区段表。
        let tree = MerkleTree::build_from_chunk_hashes(&leaf_hashes, &self.hasher);
        let new_index_extents = self.write_index_blocks(&tree)?;
        let total_chunks = (old_chunks + new_chunks) as u32;
        let loc = self.build_table(
            &data_extents,
            &new_index_extents,
            tree.root_hash(),
            total_chunks,
        )?;
        self.dev.flush()?;
        // 5) 头块换新块切换（提交点）。
        self.commit_record_switch(
            &old_idx,
            tree.root_hash(),
            new_len,
            total_chunks,
            loc,
            &entry.content_hash,
            path,
        )?;
        Ok(new_len)
    }

    /// 截断/扩展（仅可变记录）：shrink 回收尾部 chunk；扩展补零 chunk。
    /// 语义对齐 ftruncate。返回新长度。
    pub fn truncate(&mut self, path: &str, len: u64, uid: u32) -> Result<u64, KosError> {
        let entry = self.lookup_entry(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(uid, entry.owner_uid, entry.capability, perm::OWNER_W | perm::OTHER_W) {
            return Err(KosError::PermissionDenied);
        }
        let idx = self.objects.get(&entry.content_hash).ok_or(KosError::ObjectNotFound)?.clone();
        if idx.header.flags & crate::format::object::object_flags::MUTABLE == 0 {
            return Err(KosError::NotMutable);
        }
        let old_chunks = idx.header.block_count as usize;
        let old_len_i = idx.header.content_len as usize;
        let new_chunks = ((len as usize) + DATA_CHUNK_SIZE - 1) / DATA_CHUNK_SIZE;

        type PatchMap = BTreeMap<usize, [u8; 32]>;
        let mut pending_drop: Option<Vec<Extent>> = None;
        let mut leaves = self.read_leaf_hashes(&idx, old_chunks)?;
        let (extents, patched, tree) = if new_chunks == old_chunks {
            let t = MerkleTree::build_from_chunk_hashes(&leaves, &self.hasher);
            (idx.data_extents.clone(), PatchMap::new(), t)
        } else if new_chunks < old_chunks {
            // 收缩：清零新末尾 chunk 的尾部（CoW，防扩容后读回残留）。
            let tail_in = (len as usize) % DATA_CHUNK_SIZE;
            let zero_to = if tail_in == 0 { 0 } else { (new_chunks) * DATA_CHUNK_SIZE };
            let zfrom = if zero_to == 0 { 0 } else { len as usize };
            let (ext, patched) = if zero_to > 0 {
                self.cow_write_window(
                    &idx, old_chunks, new_chunks,
                    zfrom, zfrom, &[], zero_to, len as usize,
                )?
            } else {
                (idx.data_extents.clone(), PatchMap::new())
            };
            // 回收被截掉的数据区段——必须推迟到提交之后（提前回收会让
            // 新索引块复用旧索引块位置，断电即毁旧对象）。
            let (keep, drop_tail) = split_extents_at(&ext, new_chunks);
            pending_drop = Some(drop_tail);
            let mut ls = leaves;
            ls.truncate(new_chunks);
            for (c, h) in patched {
                if c < ls.len() { ls[c] = h; }
            }
            let t = MerkleTree::build_from_chunk_hashes(&ls, &self.hasher);
            (keep, PatchMap::new(), t)
        } else {
            // 扩展：补零 chunk 全走 CoW 新块。
            let (ext, patched) = self.cow_write_window(
                &idx, old_chunks, new_chunks,
                old_len_i, old_len_i, &[], len as usize, len as usize,
            )?;
            let mut ls = leaves;
            for (c, h) in patched {
                if c < ls.len() { ls[c] = h; } else { ls.resize(c + 1, [0u8; 32]); ls[c] = h; }
            }
            ls.resize(new_chunks.max(1), [0u8; 32]);
            let t = MerkleTree::build_from_chunk_hashes(&ls, &self.hasher);
            (ext, PatchMap::new(), t)
        };
        let _ = patched;
        let new_index_extents = self.write_index_blocks(&tree)?;
        let loc = self.build_table(&extents, &new_index_extents, tree.root_hash(), new_chunks as u32)?;
        self.dev.flush()?;
        self.commit_record_switch(
            &idx, tree.root_hash(), len, new_chunks as u32, loc,
            &entry.content_hash, path,
        )?;
        if let Some(drop_tail) = pending_drop {
            self.reclaim(&drop_tail, &[]);
        }
        Ok(len)
    }

    /// 原地范围写（仅可变记录）：`offset..offset+data.len()`。
    /// 超出当前末尾即隐式扩展（空隙补零）。
    pub fn write_range(
        &mut self,
        path: &str,
        offset: u64,
        data: &[u8],
        uid: u32,
    ) -> Result<u64, KosError> {
        let entry = self.resolve_cached(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(
            uid,
            entry.owner_uid,
            entry.capability,
            perm::OWNER_W | perm::OTHER_W,
        ) {
            return Err(KosError::PermissionDenied);
        }
        let idx = self
            .objects
            .get(&entry.content_hash)
            .ok_or(KosError::ObjectNotFound)?
            .clone();
        if idx.header.flags & crate::format::object::object_flags::MUTABLE == 0 {
            return Err(KosError::NotMutable);
        }
        let old_len = idx.header.content_len as usize;
        let new_len = core::cmp::max(old_len, offset as usize + data.len());
        let old_chunks = idx.header.block_count as usize;
        let new_chunks = new_len.div_ceil(DATA_CHUNK_SIZE).max(old_chunks);
        if data.is_empty() && new_chunks == old_chunks {
            return Ok(old_len as u64);
        }

        // CoW：受影响 chunk（含空隙/扩容补零）全部写新块，旧块不动。
        let zero_from = core::cmp::min(offset as usize, old_len);
        let mut leaves = self.read_leaf_hashes(&idx, old_chunks)?;
        let (extents, patched) = self.cow_write_window(
            &idx,
            old_chunks,
            new_chunks,
            zero_from,
            offset as usize,
            data,
            offset as usize + data.len(),
            new_len,
        )?;
        for (c, h) in patched {
            if c < leaves.len() {
                leaves[c] = h;
            } else {
                leaves.resize(c + 1, [0u8; 32]);
                leaves[c] = h;
            }
        }
        leaves.resize(new_chunks.max(1), [0u8; 32]);
        let tree = MerkleTree::build_from_chunk_hashes(&leaves, &self.hasher);

        let new_index_extents = self.write_index_blocks(&tree)?;
        let loc =
            self.build_table(&extents, &new_index_extents, tree.root_hash(), new_chunks as u32)?;
        self.dev.flush()?;
        self.commit_record_switch(
            &idx,
            tree.root_hash(),
            new_len as u64,
            new_chunks as u32,
            loc,
            &entry.content_hash,
            path,
        )?;
        Ok(new_len as u64)
    }

    /// 可变记录提交尾部（头块换新块 + 换键 + 条目更新）。断电安全：
    /// 新头块是唯一提交点，旧头/旧数据/旧索引永不覆写。
    #[allow(clippy::too_many_arguments)]
    fn commit_record_switch(
        &mut self,
        old: &ObjIndex,
        root: [u8; 32],
        new_len: u64,
        total_chunks: u32,
        loc: TableLoc,
        old_key: &[u8; 32],
        path: &str,
    ) -> Result<(), KosError> {
        let mut hdr = old.header.clone();
        let secs = (self.clock / 1_000_000) as u32;
        hdr.reserved[0..4].copy_from_slice(&secs.to_le_bytes());
        hdr.content_len = new_len;
        hdr.content_hash = [0u8; 32];
        hdr.block_count = total_chunks;
        let inline;
        match &loc {
            TableLoc::Inline(b) => {
                hdr.flags |= OBJECT_FLAG_INLINE_TABLE;
                hdr.indirect = 0;
                inline = Some(*b);
            }
            TableLoc::Chain(f) => {
                hdr.flags &= !OBJECT_FLAG_INLINE_TABLE;
                hdr.indirect = *f;
                inline = None;
            }
        }
        hdr.version += 1;
        let new_block = self.alloc_blocks(1)?[0];
        let new_key = record_key(&self.hasher, &root, new_block);
        hdr.content_hash = new_key;
        self.write_header_block_inline(new_block, &hdr, inline.as_ref())?;
        let new_idx = load_object(&self.dev, new_block, hdr, self.pool_end())
            .ok_or(KosError::ExtentTableCorrupt)?;
        self.objects.remove(old_key);
        self.objects.insert(new_key, new_idx);
        self.reclaim(&old.index_extents, &old.table_blocks);
        self.update_entry_hash(path, new_key, ObjectType::File)?;
        Ok(())
    }

    /// 受影响 chunk 写时复制（断电安全核心）：
    /// 内容增量 = 零[zero_from..data_at) + data + 零[data_at+len..zero_to)。
    /// 受影响 chunk 读旧块→应用增量→写**新块**；区段表 = 前缀 + 新块
    /// 区段 + 后缀。旧块不动 → 断电后旧头引用的数据永远完整。
    #[allow(clippy::too_many_arguments)]
    fn cow_write_window(
        &mut self,
        idx: &ObjIndex,
        old_chunks: usize,
        new_chunks: usize,
        zero_from: usize,
        data_at: usize,
        data: &[u8],
        zero_to: usize,
        new_len: usize,
    ) -> Result<(Vec<Extent>, BTreeMap<usize, [u8; 32]>), KosError> {
        let c0 = zero_from / DATA_CHUNK_SIZE;
        let raw_c1 = core::cmp::max(zero_to.div_ceil(DATA_CHUNK_SIZE), c0 + 1);
        let c1 = core::cmp::min(raw_c1, core::cmp::max(new_chunks, c0 + 1));
        let count = (c1 - c0) as u64;
        let new_blocks = self.alloc_blocks(count)?;
        let mut hashes = BTreeMap::new();
        let mut buf;
        for (i, &nb) in new_blocks.iter().enumerate() {
            let c = c0 + i;
            let chunk_off = c * DATA_CHUNK_SIZE;
            if c < old_chunks {
                let ob = Self::chunk_block_no(idx, c as u32)?;
                let mut b = [0u8; BLK];
                self.read_blk(ob, &mut b)?;
                buf = b;
            } else {
                buf = [0u8; BLK];
            }
            let z_lo = core::cmp::max(zero_from, chunk_off);
            let z_hi = core::cmp::min(data_at, chunk_off + DATA_CHUNK_SIZE);
            if z_lo < z_hi {
                for b in buf[z_lo - chunk_off..z_hi - chunk_off].iter_mut() {
                    *b = 0;
                }
            }
            let d_lo = core::cmp::max(data_at, chunk_off);
            let d_hi = core::cmp::min(data_at + data.len(), chunk_off + DATA_CHUNK_SIZE);
            if d_lo < d_hi {
                buf[d_lo - chunk_off..d_hi - chunk_off]
                    .copy_from_slice(&data[d_lo - data_at..d_hi - data_at]);
            }
            let z2_lo = core::cmp::max(data_at + data.len(), chunk_off);
            let z2_hi = core::cmp::min(zero_to, chunk_off + DATA_CHUNK_SIZE);
            if z2_lo < z2_hi {
                for b in buf[z2_lo - chunk_off..z2_hi - chunk_off].iter_mut() {
                    *b = 0;
                }
            }
            self.write_blk(nb, &buf)?;
            let clen = core::cmp::min(DATA_CHUNK_SIZE, new_len.saturating_sub(chunk_off)).max(1);
            hashes.insert(c, self.hasher.hash32(&buf[..clen]));
        }
        let (head, rest) = split_extents_at(&idx.data_extents, c0);
        let (_, tail) = split_extents_at(&rest, c1 - c0);
        let mut extents = head;
        extents.extend(runs_of(&new_blocks, KIND_DATA));
        extents.extend(tail);
        Ok((extents, hashes))
    }

    /// 把 `content` 的 chunk 集写回：旧 chunk 原地覆盖（复用旧区段块），
    /// 扩容的新 chunk 走新区段。返回合并后的数据区段列表。
    fn rewrite_chunks(
        &mut self,
        idx: &ObjIndex,
        content: &[u8],
        old_chunks: usize,
        new_chunks: usize,
    ) -> Result<Vec<Extent>, KosError> {
        let mut buf = [0u8; BLK];
        for c in 0..old_chunks.min(new_chunks) {
            let block = Self::chunk_block_no(idx, c as u32)?;
            let start = c * DATA_CHUNK_SIZE;
            let end = core::cmp::min(start + DATA_CHUNK_SIZE, content.len());
            buf = [0u8; BLK];
            buf[..end - start].copy_from_slice(&content[start..end]);
            self.write_blk(block, &buf)?;
        }
        if new_chunks <= old_chunks {
            return Ok(idx.data_extents.clone());
        }
        // 扩容：新增 chunk 挂新区段。
        let tail = &content[old_chunks * DATA_CHUNK_SIZE..];
        let mut extents = idx.data_extents.clone();
        let new_extents = self.write_chunk_extents(tail)?;
        extents.extend_from_slice(&new_extents);
        Ok(extents)
    }

    /// 读第 0 层索引块提取全部 chunk 哈希（append/重建用，不读数据块）。
    fn read_leaf_hashes(
        &mut self,
        idx: &ObjIndex,
        total_chunks: usize,
    ) -> Result<Vec<[u8; 32]>, KosError> {
        let mut out = Vec::with_capacity(total_chunks);
        let mut buf = [0u8; BLK];
        let counts = level_block_counts(total_chunks.max(1));
        let level0 = counts[0];
        let mut block_i = 0u64;
        for ext in &idx.index_extents {
            for k in 0..ext.count as u64 {
                if block_i < level0 as u64 {
                    self.read_blk(ext.start + k, &mut buf)?;
                    let t = MerkleIndexBlock::from_bytes(&buf[..MERKLE_INDEX_BLOCK_SIZE])?;
                    for h in t.hashes.iter().take(t.count as usize) {
                        out.push(*h);
                    }
                }
                block_i += 1;
            }
        }
        if out.len() != total_chunks {
            return Err(KosError::HashMismatch {
                context: "leaf-hashes",
            });
        }
        Ok(out)
    }

    /// 命名空间条目哈希更新（沿父链 CoW 到根并提交）。
    fn update_entry_hash(
        &mut self,
        path: &str,
        new_hash: [u8; 32],
        entry_type: ObjectType,
    ) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        let i = entries
            .iter()
            .position(|e| e.name == name)
            .ok_or(KosError::PathNotFound)?;
        entries[i].content_hash = new_hash;
        entries[i].entry_type = entry_type;
        self.replace_dir_and_commit(parent, &entries)
    }

    // ---------- 命名空间（路径层） ----------

    fn read_dir_hash(&mut self, hash: &[u8; 32]) -> Result<Vec<DirEntry>, KosError> {
        let data = self.get(hash)?;
        DirEntry::decode_all(&data)
    }

    /// 解析路径 → 条目（无则 None）。路径为空/`/` 返回 None（根自身）。
    fn resolve(&mut self, path: &str) -> Result<Option<DirEntry>, KosError> {
        let segs: Vec<&str> = split_path(path).collect();
        let mut cur_hash = self.manifest.namespace_root_hash;
        let mut entry = None;
        let n = segs.len();
        for (i, seg) in segs.into_iter().enumerate() {
            if seg.len() > MAX_NAME {
                return Err(KosError::NameTooLong);
            }
            let entries = self.read_dir_hash(&cur_hash)?;
            // 目录条目按名字有序（写入路径维持不变量）：二分定位。
            let e = entries
                .get(entries.partition_point(|e| e.name.as_str() < seg))
                .filter(|e| e.name == seg)
                .cloned()
                .ok_or(KosError::PathNotFound)?;
            if e.entry_type != ObjectType::Dir && i + 1 < n {
                return Err(KosError::NotADirectory);
            }
            if e.entry_type == ObjectType::Dir && i + 1 < n {
                cur_hash = e.content_hash;
            }
            entry = Some(e);
        }
        Ok(entry)
    }

    /// 解析路径 → 条目（公开；内核 fd 层 / 上传会话用）。
    pub fn resolve_pub(&mut self, path: &str) -> Result<Option<DirEntry>, KosError> {
        self.resolve(path)
    }

    /// 缓存感知解析：命中且代一致直接返回（免逐级块读+解码）。
    fn resolve_cached(&mut self, path: &str) -> Result<Option<DirEntry>, KosError> {
        if let Some((gen, e)) = self.dentry_cache.get(path) {
            if *gen == self.manifest.generation {
                return Ok(Some(e.clone()));
            }
        }
        let r = match self.resolve(path) {
            Ok(e) => e,
            Err(KosError::PathNotFound) => None,
            Err(e) => return Err(e),
        };
        if let Some(e) = &r {
            self.dentry_cache
                .insert(path.into(), (self.manifest.generation, e.clone()));
        }
        Ok(r)
    }

    /// lookup_entry 公开包装（上传会话用）。
    pub fn lookup_entry_pub(&mut self, path: &str) -> Result<Option<DirEntry>, KosError> {
        self.lookup_entry(path)
    }

    fn lookup_entry(&mut self, path: &str) -> Result<Option<DirEntry>, KosError> {
        // 路径不存在 = 无条目（新建路径）；中间目录缺失由后续 list 报错。
        match self.resolve(path) {
            Ok(e) => Ok(e),
            Err(KosError::PathNotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 分页列目录：跳过前 `skip` 条（readdir cookie 语义）。
    pub fn list_skip(&mut self, path: &str, skip: usize) -> Result<Vec<DirEntry>, KosError> {
        let mut all = self.list(path)?;
        if skip >= all.len() {
            all.clear();
        } else {
            all.drain(0..skip);
        }
        Ok(all)
    }

    /// 列目录。
    pub fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, KosError> {
        match self.resolve(path)? {
            Some(e) if e.entry_type == ObjectType::Dir => self.read_dir_hash(&e.content_hash),
            Some(_) => Err(KosError::NotADirectory),
            None => {
                let h = self.manifest.namespace_root_hash;
                self.read_dir_hash(&h)
            }
        }
    }

    /// mkdir：创建目录并提交。
    pub fn mkdir(&mut self, path: &str, uid: u32, capability: u32) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        if entries.iter().any(|e| e.name == name) {
            return Err(KosError::AlreadyExists);
        }
        let empty = self.put(&[0u8; 4], ObjectType::Dir, uid, capability)?;
        entries.push(DirEntry {
            name: name.into(),
            entry_type: ObjectType::Dir,
            capability,
            owner_uid: uid,
            content_hash: empty,
        });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.replace_dir_and_commit(parent, &entries)
    }

    /// 写不可变文件（新建或整体替换）并提交。
    pub fn write(
        &mut self,
        path: &str,
        data: &[u8],
        uid: u32,
        capability: u32,
    ) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        let hash = self.put(data, ObjectType::File, uid, capability)?;
        match entries.iter().position(|e| e.name == name) {
            Some(i) => {
                let e = &entries[i];
                if !check_access(
                    uid,
                    e.owner_uid,
                    e.capability,
                    perm::OWNER_W | perm::OTHER_W,
                ) {
                    return Err(KosError::PermissionDenied);
                }
                entries[i].content_hash = hash;
                entries[i].capability = capability;
                entries[i].owner_uid = uid;
            }
            None => {
                entries.push(DirEntry {
                    name: name.into(),
                    entry_type: ObjectType::File,
                    capability,
                    owner_uid: uid,
                    content_hash: hash,
                });
                entries.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        self.replace_dir_and_commit(parent, &entries)
    }

    /// 写符号链接并提交。
    pub fn symlink(&mut self, path: &str, target: &[u8], uid: u32) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        if entries.iter().any(|e| e.name == name) {
            return Err(KosError::AlreadyExists);
        }
        let hash = self.put(target, ObjectType::Symlink, uid, perm::ALL_R)?;
        entries.push(DirEntry {
            name: name.into(),
            entry_type: ObjectType::Symlink,
            capability: perm::ALL_R,
            owner_uid: uid,
            content_hash: hash,
        });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.replace_dir_and_commit(parent, &entries)
    }

    /// unlink：删除条目并提交；随后回收对象全部块。
    pub fn unlink(&mut self, path: &str, uid: u32) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        let i = entries
            .iter()
            .position(|e| e.name == name)
            .ok_or(KosError::PathNotFound)?;
        let e = entries[i].clone();
        if !check_access(uid, e.owner_uid, e.capability, perm::OWNER_W) {
            return Err(KosError::PermissionDenied);
        }
        entries.remove(i);
        self.replace_dir_and_commit(parent, &entries)?;
        if let Some(idx) = self.objects.remove(&e.content_hash) {
            let zero = [0u8; BLK];
            self.write_blk(idx.header_block, &zero)?;
            self.dev.flush()?;
            self.reclaim(&idx.data_extents, &[]);
            self.reclaim(&idx.index_extents, &idx.table_blocks);
        }
        Ok(())
    }

    /// 随机读（pread 语义）：`offset..offset+buf.len()`，只读受影响
    /// chunk 块（读 1 字节不再整读全对象）。返回实际读到的字节数
    /// （末尾截断）。权限与 Full 校验同 open_path。
    pub fn read_range(
        &mut self,
        path: &str,
        offset: u64,
        buf: &mut [u8],
        uid: u32,
    ) -> Result<usize, KosError> {
        let entry = self.resolve_cached(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(uid, entry.owner_uid, entry.capability, perm::OWNER_R | perm::OTHER_R) {
            return Err(KosError::PermissionDenied);
        }
        self.read_range_hash(&entry.content_hash, offset, buf)
    }

    /// 按哈希随机读（fd 层用：条目已解析、权限已查）。
    /// 顺序模式（offset == 上次末尾）时预读后续 chunk 进缓存（F2，
    /// FAT32 驱动 readahead 的等价物）。
    pub fn read_range_hash(
        &mut self,
        hash: &[u8; 32],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, KosError> {
        let n = self.read_range_inner(hash, offset, buf)?;
        // 顺序检测 + 预读（8 chunk；只对可变记录/文件的热顺序流生效）。
        let seq = matches!(self.ra_last, Some((h, off)) if h == *hash && off + 4096 >= offset);
        if seq && n > 0 {
            if let Some(idx) = self.objects.get(hash).cloned() {
                let mut blk = [0u8; BLK];
                let start_c = (offset as usize + n) / DATA_CHUNK_SIZE;
                for c in start_c..start_c + 8 {
                    if c >= idx.header.block_count as usize {
                        break;
                    }
                    if let Ok(b) = Self::chunk_block_no(&idx, c as u32) {
                        if !self.cache.contains_key(&b) {
                            if self.read_blk(b, &mut blk).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        self.ra_last = Some((*hash, offset + n as u64));
        Ok(n)
    }

    fn read_range_inner(
        &mut self,
        hash: &[u8; 32],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, KosError> {
        let idx = self
            .objects
            .get(hash)
            .ok_or(KosError::ObjectNotFound)?
            .clone();
        let len = idx.header.content_len;
        if offset >= len {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, len - offset) as usize;
        let c0 = offset as usize / DATA_CHUNK_SIZE;
        let c1 = ((offset as usize) + want + DATA_CHUNK_SIZE - 1) / DATA_CHUNK_SIZE;
        let mut out = 0usize;
        let mut blk = [0u8; BLK];
        for c in c0..c1.max(c0 + 1) {
            if c >= idx.header.block_count as usize {
                break;
            }
            let block = Self::chunk_block_no(&idx, c as u32)?;
            self.read_blk(block, &mut blk)?;
            let chunk_off = c * DATA_CHUNK_SIZE;
            let start = if offset as usize > chunk_off {
                offset as usize - chunk_off
            } else {
                0
            };
            let end = core::cmp::min(DATA_CHUNK_SIZE, offset as usize + want - chunk_off);
            if start < end {
                let take = core::cmp::min(end - start, want - out);
                buf[out..out + take].copy_from_slice(&blk[start..start + take]);
                out += take;
            }
        }
        Ok(out)
    }

    /// rename / move：`from` → `to`（`to` 已存在则替换）。对象本身不动
    /// （CAS），只 CoW 目录链：删旧条目 + 加新条目 + 提交。跨目录移动
    /// 需要两条链都重建。
    pub fn rename(&mut self, from: &str, to: &str, uid: u32) -> Result<(), KosError> {
        let entry = self.resolve(from)?.ok_or(KosError::PathNotFound)?;
        if !check_access(
            uid,
            entry.owner_uid,
            entry.capability,
            perm::OWNER_W | perm::OTHER_W,
        ) {
            return Err(KosError::PermissionDenied);
        }
        let (old_parent, old_name) = split_parent(from)?;
        let (new_parent, new_name) = split_parent(to)?;
        if new_name.len() > MAX_NAME {
            return Err(KosError::NameTooLong);
        }
        // 目标父目录必须存在（list 报错即不存在）。
        let mut np_entries = self.list(new_parent)?;
        // 同目录快路径：一条链。
        if old_parent == new_parent {
            let mut entries = np_entries;
            let i = entries
                .iter()
                .position(|e| e.name == old_name)
                .ok_or(KosError::PathNotFound)?;
            let mut e = entries.remove(i);
            e.name = new_name.into();
            // 目标已存在则替换（删除其条目）。
            if let Some(j) = entries.iter().position(|x| x.name == new_name) {
                entries.remove(j);
            }
            entries.push(e);
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            return self.replace_dir_and_commit(new_parent, &entries);
        }
        // 跨目录：先在新父目录加条目，再在旧父目录删条目。
        if let Some(j) = np_entries.iter().position(|x| x.name == new_name) {
            np_entries.remove(j);
        }
        np_entries.push(DirEntry {
            name: new_name.into(),
            entry_type: entry.entry_type,
            capability: entry.capability,
            owner_uid: entry.owner_uid,
            content_hash: entry.content_hash,
        });
        np_entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.replace_dir_and_commit(new_parent, &np_entries)?;
        let mut op_entries = self.list(old_parent)?;
        let i = op_entries
            .iter()
            .position(|e| e.name == old_name)
            .ok_or(KosError::PathNotFound)?;
        op_entries.remove(i);
        self.replace_dir_and_commit(old_parent, &op_entries)
    }

    /// 打开并读取：对目标做读权限检查。
    pub fn open_path(&mut self, path: &str, uid: u32) -> Result<Vec<u8>, KosError> {
        let entry = self.resolve_cached(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(
            uid,
            entry.owner_uid,
            entry.capability,
            perm::OWNER_R | perm::OTHER_R,
        ) {
            return Err(KosError::PermissionDenied);
        }
        self.get(&entry.content_hash)
    }

    /// 读取符号链接目标。
    pub fn readlink(&mut self, path: &str) -> Result<Vec<u8>, KosError> {
        let entry = self.resolve(path)?.ok_or(KosError::PathNotFound)?;
        if entry.entry_type != ObjectType::Symlink {
            return Err(KosError::NotASymlink);
        }
        self.get(&entry.content_hash)
    }

    // ---------- 提交 ----------

    /// 用新条目集替换 `dir` 的目录对象，沿父链重建到根，双槽提交。
    fn replace_dir_and_commit(
        &mut self,
        dir: &str,
        entries: &[DirEntry],
    ) -> Result<(), KosError> {
        let mut content = DirEntry::encode_all(entries);

        let chain: Vec<&str> = split_path(dir).collect();
        let mut dir_hashes = Vec::with_capacity(chain.len());
        let mut cur = self.manifest.namespace_root_hash;
        for seg in &chain {
            let ents = self.read_dir_hash(&cur)?;
            let e = ents
                .into_iter()
                .find(|e| &e.name == seg)
                .ok_or(KosError::CorruptNamespace)?;
            cur = e.content_hash;
            dir_hashes.push(cur);
        }

        let mut owner = 0u32;
        let mut caps = perm::DIR_DEFAULT;
        if let Some(last) = dir_hashes.last() {
            if let Some(old) = self.objects.get(last) {
                owner = old.header.owner_uid;
                caps = old.header.capability;
            }
        }
        let mut new_hash = self.put(&content, ObjectType::Dir, owner, caps)?;

        for depth in (0..chain.len()).rev() {
            let parent_hash = if depth == 0 {
                self.manifest.namespace_root_hash
            } else {
                dir_hashes[depth - 1]
            };
            let mut pents = self.read_dir_hash(&parent_hash)?;
            let i = pents
                .iter()
                .position(|e| e.name == chain[depth])
                .ok_or(KosError::CorruptNamespace)?;
            pents[i].content_hash = new_hash;
            let (powner, pcaps) = (pents[i].owner_uid, pents[i].capability);
            content = DirEntry::encode_all(&pents);
            new_hash = self.put(&content, ObjectType::Dir, powner, pcaps)?;
        }

        if new_hash == self.manifest.namespace_root_hash {
            return Ok(());
        }

        let parent_manifest = self.hasher.hash32(&self.manifest.to_bytes());
        let mut m = KosRootManifest {
            magic: MANIFEST_MAGIC,
            generation: self.manifest.generation + 1,
            timestamp: self.clock,
            namespace_root_hash: new_hash,
            parent_manifest_hash: parent_manifest,
            signer_pk: [0u8; 32],
            signature: [0u8; 64],
            crc32: 0,
            reserved: [0u8; 4],
        };
        if let Some(sign) = self.signer {
            let payload = manifest_sign_payload(&m);
            let (pk, sig) = sign(payload);
            m.signer_pk = pk;
            m.signature = sig;
        }
        let slot = if self.active_slot_a {
            self.sb.root_slot_b
        } else {
            self.sb.root_slot_a
        };
        write_manifest_slot(&mut self.dev, slot, &m)?;
        self.maybe_flush()?;
        self.manifest = m;
        self.active_slot_a = !self.active_slot_a;
        self.dentry_cache.clear();
        self.persist_superblock()
    }

    fn persist_superblock(&mut self) -> Result<(), KosError> {
        self.sb.free_blocks = self.free_blocks();
        self.persist_alloc_state()?;
        self.write_blk(0, &self.sb.to_bytes())?;
        self.maybe_flush()
    }

    /// 持久化分配状态（P2 快速挂载）：脏位图块 → 对象索引链 → hint。
    /// 位图/索引放在 Record Pool 区（分配器不可见）。
    fn persist_alloc_state(&mut self) -> Result<(), KosError> {
        use crate::store::persist as p;
        let pool_blocks = self.free.len();
        let per = p::BITS_PER_BMP_BLOCK;
        let bmp_blocks = p::bitmap_blocks_for(pool_blocks);
        let idx_blocks = (self.objects.len() + p::IDS_PER_IDX_BLOCK - 1)
            / p::IDS_PER_IDX_BLOCK;
        let need = bmp_blocks + idx_blocks.max(1) as u64;
        if self.sb.rec_pool_blocks < need || self.sb.rec_pool_start == 0 {
            // Record Pool 装不下：放弃快速挂载（扫描路径仍正确）。
            return Ok(());
        }
        let bmp_base = self.sb.rec_pool_start;
        let idx_base = bmp_base + bmp_blocks;

        // 1) 脏位图块（快速挂载后只写改过的；扫描挂载后全写）。
        for i in 0..bmp_blocks as usize {
            if self.fast_mounted && !self.bitmap_dirty.get(i).copied().unwrap_or(false) {
                continue;
            }
            let first = i * per;
            let last = core::cmp::min(first + per, pool_blocks);
            if first >= last {
                break;
            }
            let bits = &self.free[first..last];
            let bytes = p::bitmap_block_bytes(first as u32, bits);
            self.write_blk(bmp_base + i as u64, &bytes)?;
            if i < self.bitmap_dirty.len() {
                self.bitmap_dirty[i] = false;
            }
        }
        // 2) 对象索引链（全量重写，块数很小）。
        let ids: Vec<u64> = self.objects.values().map(|i| i.header_block).collect();
        let mut written = 0usize;
        let mut block = idx_base;
        while written < ids.len() || written == 0 {
            let chunk = &ids[written..core::cmp::min(written + p::IDS_PER_IDX_BLOCK, ids.len())];
            let next = if written + p::IDS_PER_IDX_BLOCK < ids.len() {
                block + 1
            } else {
                0
            };
            let bytes = p::index_block_bytes(next, chunk);
            self.write_blk(block, &bytes)?;
            written += chunk.len();
            block += 1;
            if chunk.is_empty() {
                break;
            }
        }
        // 3) hint 写进超级块 reserved（随 superblock CRC 一起保护）。
        let hint = p::MountHint {
            magic: p::HINT_MAGIC,
            seq: self.manifest.generation,
            bitmap_blocks: bmp_blocks as u32,
            index_head: idx_base,
            index_blocks: idx_blocks.max(1) as u32,
            reserved: 0,
        };
        hint.write_to_sb(&mut self.sb);
        // 持久化状态已是最新：后续 commit 只需写脏块。
        self.bitmap_dirty.resize(bmp_blocks as usize, false);
        self.fast_mounted = true;
        Ok(())
    }

    // ---------- GC（P2：标记-清扫） ----------

    /// 从活动 manifest 根做可达性标记，清扫全部不可达对象（CoW 孤儿、
    /// 被覆盖的旧版本）。返回 (清扫对象数, 回收块数)。
    ///
    /// 语义：只回收「命名空间不可达」的对象——活动目录树闭包之外的
    /// 一切都是垃圾（旧目录版本、append 换代的旧索引/表、unlink 残留）。
    /// 清扫后提交持久索引，使快速挂载立即可用回收空间。
    pub fn gc(&mut self) -> Result<(usize, u64), KosError> {
        // 1) 标记：根目录闭包。
        let mut reachable: Vec<[u8; 32]> = Vec::new();
        let mut queue: Vec<[u8; 32]> = Vec::new();
        queue.push(self.manifest.namespace_root_hash);
        while let Some(h) = queue.pop() {
            if reachable.contains(&h) || !self.objects.contains_key(&h) {
                continue;
            }
            reachable.push(h);
            let idx = self.objects.get(&h).unwrap().clone();
            if idx.header.obj_type == ObjectType::Dir.as_u16() {
                let data = self.read_object_content(&idx)?;
                for e in DirEntry::decode_all(&data)? {
                    queue.push(e.content_hash);
                }
            }
        }
        // 2) 清扫：不可达对象清零头块并回收全部块。
        let dead: Vec<[u8; 32]> = self
            .objects
            .keys()
            .filter(|k| !reachable.contains(k))
            .cloned()
            .collect();
        let mut freed_blocks = 0u64;
        let before_free = self.free_cnt;
        let zero = [0u8; BLK];
        for k in &dead {
            if let Some(idx) = self.objects.remove(k) {
                self.write_blk(idx.header_block, &zero)?;
                self.reclaim(&idx.data_extents, &[]);
                self.reclaim(&idx.index_extents, &idx.table_blocks);
            }
        }
        self.dev.flush()?;
        freed_blocks = self.free_cnt - before_free;
        // 3) 提交持久索引（位图脏块 + 对象索引链 + hint）。
        self.persist_alloc_state()?;
        self.write_blk(0, &self.sb.to_bytes())?;
        self.dev.flush()?;
        Ok((dead.len(), freed_blocks))
    }

    // ---------- 分配与底层写入 ----------

    /// 带缓存的块读（读路径）。缓存满时整清（粗粒度 LRU 替代）。
    fn read_blk(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        if let Some(c) = self.cache.get(&lba) {
            buf.copy_from_slice(c);
            return Ok(());
        }
        dev_read(&self.dev, lba, buf)?;
        if self.cache.len() >= self.cache_cap {
            self.cache.clear();
        }
        let mut snap = [0u8; BLK];
        snap.copy_from_slice(buf);
        self.cache.insert(lba, snap);
        Ok(())
    }

    /// 带缓存失效的块写（写路径同时更新缓存）。
    fn write_blk(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        dev_write(&mut self.dev, lba, data)?;
        if self.cache.len() >= self.cache_cap {
            self.cache.clear();
        }
        self.cache.insert(lba, data.try_into().map_err(|_| KosError::InvalidBlockLen {
            expected: BLK,
            got: data.len(),
        })?);
        Ok(())
    }

    fn pool_end(&self) -> u64 {
        self.sb.obj_pool_start + self.sb.obj_pool_blocks
    }

    /// 分配 `n` 个空闲块（返回绝对块号列表，可能不连续）。
    fn alloc_blocks(&mut self, n: u64) -> Result<Vec<u64>, KosError> {
        let runs = self.alloc_runs(n)?;
        Ok(runs
            .iter()
            .flat_map(|(s, c)| (*s)..(*s + *c as u64))
            .collect())
    }

    /// first-fit 分配 `need` 块（可跨多个区段），带旋转搜索提示。
    /// F3 局部性：优先尝试上次分配末尾的紧邻块（append 流保持物理
    /// 连续 → 索引/区段少碎片、顺序 IO）。
    fn alloc_runs(&mut self, need: u64) -> Result<Vec<(u64, u32)>, KosError> {
        if need == 0 {
            return Ok(Vec::new());
        }
        let total = self.free.len();
        let per = crate::store::persist::BITS_PER_BMP_BLOCK;
        let mut runs: Vec<(u64, u32)> = Vec::new();
        let mut got = 0u64;
        // F3 局部性：分配提示点起恰好有连续 need 空闲块 → 直接取用
        // （append 流的块物理连续：区段少、顺序 IO 友好）。
        let h = self.alloc_hint;
        if h + (need as usize) <= total && (h..h + need as usize).all(|k| self.free[k]) {
            for k in h..h + need as usize {
                self.free[k] = false;
                self.bitmap_dirty[k / per] = true;
            }
            self.free_cnt -= need;
            self.alloc_hint = (h + need as usize) % total;
            let mut v = Vec::new();
            v.push((self.sb.obj_pool_start + h as u64, need.min(u32::MAX as u64) as u32));
            return Ok(v);
        }
        let mut i = self.alloc_hint;
        // 最多绕池两圈：一圈找空隙，一圈兜底合并。
        let mut steps = total * 2;
        while steps > 0 && got < need {
            steps -= 1;
            if !self.free[i] {
                i = (i + 1) % total;
                continue;
            }
            // 从 i 起收集连续空闲段（截到 need）。
            let start = i;
            let mut end = i;
            while end < total
                && self.free[end]
                && got + (end - start + 1) as u64 <= need
            {
                end += 1;
            }
            let cnt = (end - start) as u64;
            if cnt == 0 {
                // need 已被前一段满足的边界情形由循环条件处理。
                i = (i + 1) % total;
                continue;
            }
            for k in start..end {
                self.free[k] = false;
                self.bitmap_dirty[k / per] = true;
            }
            runs.push((
                self.sb.obj_pool_start + start as u64,
                cnt.min(u32::MAX as u64) as u32,
            ));
            got += cnt;
            self.free_cnt -= cnt;
            i = end % total;
        }
        if got < need {
            return Err(KosError::NoSpace);
        }
        self.alloc_hint = i;
        Ok(runs)
    }

    /// 写数据 chunk 并组织为区段。
    fn write_chunk_extents(&mut self, data: &[u8]) -> Result<Vec<Extent>, KosError> {
        let nchunks = (data.len() + DATA_CHUNK_SIZE - 1) / DATA_CHUNK_SIZE;
        if nchunks == 0 {
            return Ok(Vec::new());
        }
        let blocks = self.alloc_blocks(nchunks as u64)?;
        let extents = runs_of(&blocks, KIND_DATA);
        let mut buf;
        for (c, &block) in blocks.iter().enumerate() {
            let start = c * DATA_CHUNK_SIZE;
            let end = core::cmp::min(start + DATA_CHUNK_SIZE, data.len());
            buf = [0u8; BLK];
            buf[..end - start].copy_from_slice(&data[start..end]);
            self.write_blk(block, &buf)?;
        }
        Ok(extents)
    }

    /// 写全部 Merkle 索引块（level 0 起逐层向上）并组织为区段。
    fn write_index_blocks(&mut self, tree: &MerkleTree) -> Result<Vec<Extent>, KosError> {
        let n = tree.index_block_count();
        if n == 0 {
            return Ok(Vec::new());
        }
        let blocks = self.alloc_blocks(n as u64)?;
        for (blk, &block) in tree.index_blocks().zip(blocks.iter()) {
            self.write_blk(block, &blk.to_bytes())?;
        }
        Ok(runs_of(&blocks, KIND_INDEX))
    }

    fn build_table(
        &mut self,
        data_extents: &[Extent],
        index_extents: &[Extent],
        merkle_root: [u8; 32],
        total_chunks: u32,
    ) -> Result<TableLoc, KosError> {
        let all: Vec<Extent> = data_extents
            .iter()
            .chain(index_extents.iter())
            .cloned()
            .collect();
        if all.len() <= crate::format::extent::EXTENTS_PER_TABLE {
            let mut table = ExtentTable::empty(merkle_root, total_chunks);
            table.entry_count = all.len() as u32;
            table.entries[..all.len()].copy_from_slice(&all);
            let full = table.to_bytes();
            let mut inline = [0u8; 3904];
            inline.copy_from_slice(&full[..3904]);
            return Ok(TableLoc::Inline(inline));
        }
        Ok(TableLoc::Chain(self.write_extent_tables(
            data_extents,
            index_extents,
            merkle_root,
            total_chunks,
        )?))
    }

    /// 写区段表链（可能多块），返回链首块号。
    fn write_extent_tables(
        &mut self,
        data_extents: &[Extent],
        index_extents: &[Extent],
        merkle_root: [u8; 32],
        total_chunks: u32,
    ) -> Result<u64, KosError> {
        let all: Vec<Extent> = data_extents
            .iter()
            .chain(index_extents.iter())
            .cloned()
            .collect();
        const PER: usize = crate::format::extent::EXTENTS_PER_TABLE;
        let tables = all.len().div_ceil(PER).max(1);
        let blocks = self.alloc_blocks(tables as u64)?;
        let mut first = 0u64;
        for (t, &head) in blocks.iter().enumerate() {
            if t == 0 {
                first = head;
            }
            let lo = t * PER;
            let hi = core::cmp::min((t + 1) * PER, all.len());
            let slice = if lo < all.len() { &all[lo..hi.max(lo)] } else { &[] };
            let mut table = ExtentTable::empty(merkle_root, total_chunks);
            table.entry_count = slice.len() as u32;
            if !slice.is_empty() {
                table.entries[..slice.len()].copy_from_slice(slice);
            }
            table.next_table = if t + 1 < blocks.len() { blocks[t + 1] } else { 0 };
            self.write_blk(head, &table.to_bytes())?;
        }
        Ok(first)
    }

    /// 回收区段与表块（仅内存位图；磁盘块下次挂载由扫描回收）。
    fn reclaim(&mut self, extents: &[Extent], tables: &[u64]) {
        let base = self.sb.obj_pool_start as usize;
        let per = crate::store::persist::BITS_PER_BMP_BLOCK;
        for ext in extents {
            for k in 0..ext.count as usize {
                let b = (ext.start as usize).wrapping_sub(base) + k;
                if b < self.free.len() {
                    if !self.free[b] {
                        self.free_cnt += 1;
                    }
                    self.free[b] = true;
                    self.bitmap_dirty[b / per] = true;
                }
            }
        }
        for &t in tables {
            let b = (t as usize).wrapping_sub(base);
            if b < self.free.len() {
                if !self.free[b] {
                    self.free_cnt += 1;
                }
                self.free[b] = true;
                self.bitmap_dirty[b / per] = true;
            }
        }
    }
}

// ==================== 辅助（自由函数） ====================

/// 把数据区段在第 `chunk` 个 chunk 处切成两段（前者恰好覆盖前
/// `chunk` 个 chunk）。
fn split_extents_at(extents: &[Extent], chunk: usize) -> (Vec<Extent>, Vec<Extent>) {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut c = chunk as u64;
    for e in extents {
        if c == 0 {
            tail.push(*e);
        } else if c >= e.count as u64 {
            head.push(*e);
            c -= e.count as u64;
        } else {
            head.push(Extent { start: e.start, count: c as u32, kind: e.kind });
            tail.push(Extent { start: e.start + c, count: e.count - c as u32, kind: e.kind });
            c = 0;
        }
    }
    (head, tail)
}

/// 按区段列表定位第 `chunk` 个数据块的绝对块号。
fn extents_block_no(extents: &[Extent], chunk: u32) -> Result<u64, KosError> {
    let mut c = chunk as u64;
    for ext in extents {
        if c < ext.count as u64 {
            return Ok(ext.start + c);
        }
        c -= ext.count as u64;
    }
    Err(KosError::ChunkIndexOutOfRange {
        index: chunk as usize,
        total_chunks: 0,
    })
}

/// 区段表落点：单表可容纳时内联进头块（省 1 块），否则链式块。
enum TableLoc {
    /// 表字节（3904B，写入头块偏移 96）。
    Inline([u8; 3904]),
    /// 链首块号（写入 hdr.indirect）。
    Chain(u64),
}

/// 把（升序）块号列表组织为 `kind` 类型的连续区段。
fn runs_of(blocks: &[u64], kind: u32) -> Vec<Extent> {
    let mut extents = Vec::new();
    let mut run_start = 0usize;
    for i in 1..=blocks.len() {
        if i == blocks.len() || blocks[i] != blocks[i - 1] + 1 {
            extents.push(Extent {
                start: blocks[run_start],
                count: (i - run_start) as u32,
                kind,
            });
            run_start = i;
        }
    }
    extents
}

fn check_blk(bs: usize) -> Result<(), KosError> {
    if bs == BLK {
        Ok(())
    } else {
        Err(KosError::InvalidBlockLen {
            expected: BLK,
            got: bs,
        })
    }
}

fn dev_read<B: crate::block::BlockDevice>(
    dev: &B,
    lba: u64,
    buf: &mut [u8],
) -> Result<(), KosError> {
    dev.read_block(lba, buf)
}

fn dev_write<B: crate::block::BlockDevice>(
    dev: &mut B,
    lba: u64,
    data: &[u8],
) -> Result<(), KosError> {
    dev.write_block(lba, data)
}

/// 从头块加载对象索引（读区段表链；任一环节损坏返回 None）。
fn load_object<B: crate::block::BlockDevice>(
    dev: &B,
    header_block: u64,
    hdr: KosObject,
    pool_end: u64,
) -> Option<ObjIndex> {
    let mut data_extents = Vec::new();
    let mut index_extents = Vec::new();
    let mut table_blocks = Vec::new();
    let mut merkle_root = [0u8; 32];
    // 内联表（v3 小对象）：区段表就在头块偏移 96 处。
    if hdr.flags & OBJECT_FLAG_INLINE_TABLE != 0 {
        let mut buf = [0u8; BLK];
        dev.read_block(header_block, &mut buf).ok()?;
        let mut tbl = [0u8; BLK];
        tbl[..3904].copy_from_slice(&buf[96..96 + 3904]);
        let t = ExtentTable::from_bytes(&tbl).ok()?;
        merkle_root = t.merkle_root;
        for i in 0..t.entry_count as usize {
            let e = t.entries[i];
            if e.start >= pool_end || e.start + e.count as u64 > pool_end {
                return None;
            }
            match e.kind {
                KIND_DATA => data_extents.push(e),
                KIND_INDEX => index_extents.push(e),
                _ => return None,
            }
        }
        let data_chunks: u64 = data_extents.iter().map(|e| e.count as u64).sum();
        if data_chunks != hdr.block_count as u64 {
            return None;
        }
        return Some(ObjIndex {
            header_block,
            header: hdr,
            data_extents,
            index_extents,
            table_blocks: Vec::new(),
            merkle_root,
        });
    }
    let mut next = hdr.indirect;
    let mut hops = 0;
    while next != 0 {
        if next >= pool_end || hops > 1 << 20 {
            return None;
        }
        let mut buf = [0u8; BLK];
        dev.read_block(next, &mut buf).ok()?;
        let t = ExtentTable::from_bytes(&buf).ok()?;
        if hops == 0 {
            merkle_root = t.merkle_root;
        }
        for i in 0..t.entry_count as usize {
            let e = t.entries[i];
            if e.start >= pool_end || e.start + e.count as u64 > pool_end {
                return None;
            }
            match e.kind {
                KIND_DATA => data_extents.push(e),
                KIND_INDEX => index_extents.push(e),
                _ => return None,
            }
        }
        table_blocks.push(next);
        next = t.next_table;
        hops += 1;
    }
    let data_chunks: u64 = data_extents.iter().map(|e| e.count as u64).sum();
    if data_chunks != hdr.block_count as u64 {
        return None;
    }
    Some(ObjIndex {
        header_block,
        header: hdr,
        data_extents,
        index_extents,
        table_blocks,
        merkle_root,
    })
}

/// 把对象占用（头块 + 表块 + 区段）标进/清出位图。
fn mark_obj(free: &mut [bool], pool_start: u64, idx: &ObjIndex, is_free: bool) {
    let mut mark = |b: u64| {
        let i = (b - pool_start) as usize;
        if i < free.len() {
            free[i] = is_free;
        }
    };
    mark(idx.header_block);
    for &t in &idx.table_blocks {
        mark(t);
    }
    for ext in idx.data_extents.iter().chain(idx.index_extents.iter()) {
        for k in 0..ext.count as u64 {
            mark(ext.start + k);
        }
    }
}

/// 快速挂载：hint 合法且 seq 与活动 manifest 一致 → 读位图 + 索引链 +
/// 逐对象头/区段表。任一环节失败返回 None（调用方回退全池扫描）。
fn fast_mount<B: crate::block::BlockDevice>(
    dev: &B,
    sb: &KosSuperblock,
    manifest: &KosRootManifest,
    pool_end: u64,
) -> Option<(BTreeMap<[u8; 32], ObjIndex>, Vec<bool>)> {
    use crate::store::persist as p;
    let hint = p::MountHint::from_sb(sb)?;
    if hint.seq != manifest.generation {
        return None;
    }
    let pool_blocks = sb.obj_pool_blocks as usize;
    let per = p::BITS_PER_BMP_BLOCK;
    let bmp_blocks = p::bitmap_blocks_for(pool_blocks) as usize;
    if bmp_blocks == 0 || hint.bitmap_blocks as usize != bmp_blocks {
        return None;
    }
    if sb.rec_pool_blocks < bmp_blocks as u64 + hint.index_blocks as u64 {
        return None;
    }

    // 1) 位图。
    let mut free = alloc::vec![false; pool_blocks];
    let mut buf = [0u8; BLK];
    for i in 0..bmp_blocks {
        if dev.read_block(sb.rec_pool_start + i as u64, &mut buf).is_err() {
            return None;
        }
        let (first, bits) = p::bitmap_block_parse(&buf)?;
        if first as usize != i * per {
            return None;
        }
        for (k, &f) in bits.iter().enumerate() {
            let b = i * per + k;
            if b < pool_blocks {
                free[b] = f;
            }
        }
    }

    // 2) 对象索引链 → 头块列表。
    let mut ids = Vec::new();
    let mut next = hint.index_head;
    let mut hops = 0;
    while next != 0 {
        // 索引链在 Record Pool 区（对象池之外），边界是设备总块数。
        if hops > hint.index_blocks || next >= sb.total_blocks {
            return None;
        }
        if dev.read_block(next, &mut buf).is_err() {
            return None;
        }
        let (n, chunk) = p::index_block_parse(&buf)?;
        ids.extend_from_slice(&chunk);
        next = n;
        hops += 1;
    }

    // 3) 逐对象加载；对象占用必须与位图一致（防陈旧位图误判）。
    let mut objects = BTreeMap::new();
    for &id in &ids {
        // 位图说空闲但索引说有对象 → 陈旧，回退扫描。
        if id < sb.obj_pool_start || id >= pool_end || free[(id - sb.obj_pool_start) as usize] {
            return None;
        }
        if dev.read_block(id, &mut buf).is_err() {
            return None;
        }
        let hdr = KosObject::from_bytes(&buf[..OBJECT_HEADER_SIZE]).ok()?;
        let idx = load_object(dev, id, hdr, pool_end)?;
        objects.insert(idx.header.content_hash, idx);
    }
    Some((objects, free))
}

fn split_path(p: &str) -> impl Iterator<Item = &str> {
    p.split('/').filter(|s| !s.is_empty())
}

fn split_parent(p: &str) -> Result<(&str, &str), KosError> {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(KosError::PathNotFound);
    }
    match trimmed.rfind('/') {
        Some(i) => Ok((&trimmed[..i], &trimmed[i + 1..])),
        None => Ok(("", trimmed)),
    }
}

fn read_manifest_slot<B: crate::block::BlockDevice>(
    dev: &B,
    block: u64,
) -> Result<Option<KosRootManifest>, KosError> {
    let mut buf = [0u8; BLK];
    dev.read_block(block, &mut buf)?;
    if buf[..MANIFEST_SIZE].iter().all(|b| *b == 0) {
        return Ok(None);
    }
    Ok(KosRootManifest::from_bytes(&buf[..MANIFEST_SIZE]).ok())
}

fn write_manifest_slot<B: crate::block::BlockDevice>(
    dev: &mut B,
    block: u64,
    m: &KosRootManifest,
) -> Result<(), KosError> {
    let mut buf = [0u8; BLK];
    buf[..MANIFEST_SIZE].copy_from_slice(&m.to_bytes());
    dev.write_block(block, &buf)
}


