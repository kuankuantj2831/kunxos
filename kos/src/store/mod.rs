//! 对象存储引擎（Phase 1 Step 2）：Object Pool 读写 + 命名空间 + 双槽提交。
//!
//! ## 磁盘布局（引擎约定 v1）
//!
//! 卷布局沿用 [`KosSuperblock::mkfs_layout`]：块 0 超级块、块 1/2 Root
//! Manifest 双槽、块 3..16 预留、其余为 Object Pool。
//!
//! 一个对象在池中占据**连续块**：
//!
//! ```text
//!   [header 块][data chunk 0..N-1][索引块：第 0 层起逐层向上]
//!   header 块 = KosObject 序列化置于块首 96 字节，其余全零
//! ```
//!
//! `KosObject.indirect` 指向第一个（第 0 层）索引块；`block_count` 为
//! 数据 chunk 数；索引块总数由 `level_block_counts` 唯一确定。
//!
//! ## 分配与崩溃恢复
//!
//! 池中没有持久化位图：挂载时**扫描全池**找合法对象头（magic + CRC），
//! 由对象头反推（头 + 数据 + 索引块）占用集，其余为空闲。因此：
//! - put 写入顺序为 data → 索引块 → flush → header，崩溃时的半成品
//!   （无合法头）自动被视为空闲空间，不污染状态；
//! - unlink 在提交后清零对象头块，其块即刻回收。
//!
//! ## 命名空间
//!
//! 目录是普通 CAS 对象（`ObjectType::Dir`），内容为条目表序列化：
//! `count:u32` + 每条 `name_len:u8 + name + entry_type:u16 +
//! capability:u32 + owner_uid:u32 + content_hash:[u8;32]`。目录内容可
//! 跨块，**无 78 条目 / 512 inode 上限**。根目录对象的 content_hash
//! 记录在 Root Manifest 的 `namespace_root_hash`。
//!
//! ## 提交协议（崩溃安全）
//!
//! 修改命名空间的操作（mkdir/write/unlink）只追加新对象与新目录对象，
//! 最后切换 manifest：新 manifest（generation+1、新根哈希、父代哈希）
//! 写入**非活动槽**并 flush，再更新超级块统计。挂载时取「CRC/magic
//! 合法且 generation 最大」的槽。
//!
//! ## 权限模型
//!
//! `KosObject.capability` 为 Unix 风格权限位（属主/其他 × rwx，见
//! [`perm`]）。[`KosStore::open`] 等路径操作按调用者 `uid` 对照检查。
//! 读取完整性由 [`VerifyLevel`] 控制：`Full` 每次读取重建 Merkle 树
//! 逐块比对；`HashOnly` 只做内容哈希（CAS 兜底）。

use crate::error::KosError;
use crate::format::manifest::{KosRootManifest, MANIFEST_MAGIC, MANIFEST_SIZE};
use crate::format::merkle::MERKLE_INDEX_BLOCK_SIZE;
use crate::format::object::{KosObject, ObjectType, OBJECT_HEADER_SIZE};
use crate::format::superblock::KosSuperblock;
use crate::hash::{Hasher, Sha256Hasher};
use crate::merkle::{level_block_counts, MerkleTree, DATA_CHUNK_SIZE};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

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

// ==================== 读校验强度 ====================

/// 读取路径的完整性校验强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyLevel {
    /// 每次读取重建 Merkle 树与磁盘索引块逐块比对（最严格）。
    Full,
    /// 只验证对象头 CRC 与内容哈希。
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
    /// 指向对象的 content_hash。
    pub content_hash: [u8; 32],
}

/// 名字最大长度（字节）。
pub const MAX_NAME: usize = 255;

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
}

/// 对象在池中的总占块数（头 + 数据 + 索引）。
fn object_span(nchunks: usize) -> u64 {
    1 + nchunks as u64 + index_block_count(nchunks) as u64
}

/// 索引块总数（各层之和）。
fn index_block_count(total_chunks: usize) -> usize {
    level_block_counts(total_chunks.max(1)).iter().sum()
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
    /// content_hash → 索引项。
    objects: BTreeMap<[u8; 32], ObjIndex>,
    /// 池内空闲块位图（true = 空闲；下标 = 块号 - obj_pool_start）。
    free: Vec<bool>,
    verify: VerifyLevel,
    /// 提交时间戳来源（`set_clock` 注入；无注入则保持 0）。
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
            verify: VerifyLevel::HashOnly,
            clock: 0,
            dev,
        };
        // 空根目录对象（内容为 count=0 的条目表）。
        let root_hash = s.put(&[0u8; 4], ObjectType::Dir, 0, perm::DIR_DEFAULT)?;
        let genesis = KosRootManifest::genesis(root_hash, 0);
        write_manifest_slot(&mut s.dev, s.sb.root_slot_a, &genesis)?;
        s.dev.flush()?;
        s.persist_superblock()?;
        s.manifest = genesis;
        Ok(s)
    }

    /// 挂载：超级块 → 双槽择优 → 扫描对象池建索引。
    pub fn open(dev: B, verify: VerifyLevel) -> Result<Self, KosError> {
        check_blk(dev.block_size())?;
        let mut buf = [0u8; BLK];
        dev.read_block(0, &mut buf)?;
        let sb = KosSuperblock::from_bytes(&buf)?;

        let (manifest, active_slot_a) = {
            let ma = read_manifest_slot(&dev, sb.root_slot_a)?;
            let mb = read_manifest_slot(&dev, sb.root_slot_b)?;
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

        // 扫描对象池：合法头 → 反推占用；跳过其跨度后继续。
        let mut objects = BTreeMap::new();
        let mut free = alloc::vec![true; sb.obj_pool_blocks as usize];
        let mut scan = [0u8; BLK];
        let pool_start = sb.obj_pool_start;
        let pool_end = pool_start + sb.obj_pool_blocks;
        let mut b = pool_start;
        while b < pool_end {
            dev.read_block(b, &mut scan)?;
            if scan[0..4] == *b"KO\0\0" {
                if let Ok(hdr) = KosObject::from_bytes(&scan[..OBJECT_HEADER_SIZE]) {
                    let span = object_span(hdr.block_count as usize);
                    if b + span <= pool_end {
                        for k in 0..span {
                            free[(b - pool_start + k) as usize] = false;
                        }
                        objects.insert(
                            hdr.content_hash,
                            ObjIndex {
                                header_block: b,
                                header: hdr,
                            },
                        );
                        b += span;
                        continue;
                    }
                }
            }
            b += 1;
        }

        Ok(KosStore {
            dev,
            hasher: Sha256Hasher,
            sb,
            active_slot_a,
            manifest,
            objects,
            free,
            verify,
            clock: 0,
        })
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

    /// 空闲块数。
    pub fn free_blocks(&self) -> u64 {
        self.free.iter().filter(|f| **f).count() as u64
    }

    /// 根目录 content_hash。
    pub fn root_hash(&self) -> [u8; 32] {
        self.manifest.namespace_root_hash
    }

    // ---------- 对象层（CAS） ----------

    /// put：写入不可变对象，返回 content_hash；同内容已存在则去重返回。
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
        let span = object_span(nchunks);
        let base = self.alloc_contiguous(span)?;

        // 1) 数据块。
        let zero = [0u8; BLK];
        for c in 0..nchunks {
            let start = c * DATA_CHUNK_SIZE;
            let end = core::cmp::min(start + DATA_CHUNK_SIZE, data.len());
            let mut block = zero;
            block[..end - start].copy_from_slice(&data[start..end]);
            self.dev.write_block(base + 1 + c as u64, &block)?;
        }
        // 2) 索引块（第 0 层起逐层向上）。
        let mut next = base + 1 + nchunks as u64;
        for blk in tree.index_blocks() {
            self.dev.write_block(next, &blk.to_bytes())?;
            next += 1;
        }
        // 3) flush 后写头：崩溃时无头的半成品 = 空闲空间。
        self.dev.flush()?;
        let hdr = KosObject {
            magic: crate::format::object::OBJECT_MAGIC,
            obj_type: obj_type.as_u16(),
            flags: 0,
            content_len: data.len() as u64,
            content_hash: hash,
            owner_uid,
            capability,
            created_at: self.clock,
            version: 1,
            block_count: nchunks as u32,
            _pad: 0,
            indirect: base + 1 + nchunks as u64,
            header_crc: 0,
            reserved: [0u8; 4],
        };
        let mut hblock = zero;
        hblock[..OBJECT_HEADER_SIZE].copy_from_slice(&hdr.to_bytes());
        self.dev.write_block(base, &hblock)?;
        self.dev.flush()?;

        self.objects.insert(
            hash,
            ObjIndex {
                header_block: base,
                header: hdr,
            },
        );
        Ok(hash)
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

    fn read_object_content(&mut self, idx: &ObjIndex) -> Result<Vec<u8>, KosError> {
        let hdr = &idx.header;
        let nchunks = hdr.block_count as usize;
        let data_start = idx.header_block + 1;
        let mut data = Vec::with_capacity(hdr.content_len as usize);
        let mut buf = [0u8; BLK];
        for c in 0..nchunks {
            self.dev.read_block(data_start + c as u64, &mut buf)?;
            let remain = hdr.content_len as usize - data.len();
            let end = core::cmp::min(BLK, remain);
            data.extend_from_slice(&buf[..end]);
        }
        // CAS 完整性：内容哈希。
        if self.hasher.hash32(&data) != hdr.content_hash {
            return Err(KosError::HashMismatch {
                context: "content",
            });
        }
        // 严格模式：重建树并与磁盘索引块逐块比对。
        if self.verify == VerifyLevel::Full {
            let tree = MerkleTree::build(&data, &self.hasher);
            let mut cur = hdr.indirect;
            for blk in tree.index_blocks() {
                self.dev.read_block(cur, &mut buf)?;
                if buf[..MERKLE_INDEX_BLOCK_SIZE] != blk.to_bytes()[..] {
                    return Err(KosError::HashMismatch {
                        context: "index-block",
                    });
                }
                cur += 1;
            }
            if cur != hdr.indirect + index_block_count(nchunks) as u64 {
                return Err(KosError::HashMismatch {
                    context: "index-geometry",
                });
            }
        }
        Ok(data)
    }

    // ---------- 命名空间（路径层） ----------

    fn read_dir_hash(&mut self, hash: &[u8; 32]) -> Result<Vec<DirEntry>, KosError> {
        let data = self.get(hash)?;
        DirEntry::decode_all(&data)
    }

    /// 解析路径 → 最终条目（空路径/`/` 返回 None 表示根自身）。
    /// 中途遇到非目录段返回 [`KosError::NotADirectory`]。
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
            let e = entries
                .into_iter()
                .find(|e| e.name == seg)
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

    /// mkdir：创建目录并提交。父目录必须存在。
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

    /// 写文件（新建或整体替换内容）并提交。
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
                if !check_access(uid, e.owner_uid, e.capability, perm::OWNER_W | perm::OTHER_W) {
                    return Err(KosError::PermissionDenied);
                }
                entries[i].content_hash = hash;
                entries[i].capability = capability;
                entries[i].owner_uid = uid;
            }
            None => entries.push(DirEntry {
                name: name.into(),
                entry_type: ObjectType::File,
                capability,
                owner_uid: uid,
                content_hash: hash,
            }),
        }
        self.replace_dir_and_commit(parent, &entries)
    }

    /// 写符号链接（内容即目标路径）并提交。
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

    /// unlink：删除条目并提交；随后清零对象头使块即刻回收。
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
        self.drop_object(&e.content_hash)
    }

    /// 打开并读取：对目标做读权限检查（严格模式另做全量 Merkle）。
    pub fn open_path(&mut self, path: &str, uid: u32) -> Result<Vec<u8>, KosError> {
        let entry = self.resolve(path)?.ok_or(KosError::PathNotFound)?;
        if !check_access(uid, entry.owner_uid, entry.capability, perm::OWNER_R | perm::OTHER_R) {
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
        let mut new_hash;

        // 自底向上重建父链：先取沿路各目录的当前条目集。
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

        // 最深层：新内容直接作为该目录的新对象。
        let mut content = content;
        let mut owner = 0u32;
        let mut caps = perm::DIR_DEFAULT;
        if let Some(last) = dir_hashes.last() {
            if let Some(old) = self.objects.get(last) {
                owner = old.header.owner_uid;
                caps = old.header.capability;
            }
        }
        new_hash = self.put(&content, ObjectType::Dir, owner, caps)?;

        // 逐级向上：把子目录哈希替换进父条目集。
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

        // 若根内容没变则无需提交。
        if new_hash == self.manifest.namespace_root_hash {
            return Ok(());
        }

        // 提交：新 manifest 写非活动槽 → flush → 更新超级块。
        let parent_manifest = self.hasher.hash32(&self.manifest.to_bytes());
        let m = KosRootManifest {
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
        let slot = if self.active_slot_a {
            self.sb.root_slot_b
        } else {
            self.sb.root_slot_a
        };
        write_manifest_slot(&mut self.dev, slot, &m)?;
        self.dev.flush()?;
        self.manifest = m;
        self.active_slot_a = !self.active_slot_a;
        self.persist_superblock()
    }

    fn persist_superblock(&mut self) -> Result<(), KosError> {
        self.sb.free_blocks = self.free_blocks();
        self.dev.write_block(0, &self.sb.to_bytes())?;
        self.dev.flush()
    }

    /// 清零对象头使扫描式分配回收其全部块。
    fn drop_object(&mut self, hash: &[u8; 32]) -> Result<(), KosError> {
        if let Some(idx) = self.objects.remove(hash) {
            let zero = [0u8; BLK];
            self.dev.write_block(idx.header_block, &zero)?;
            self.dev.flush()?;
            let span = object_span(idx.header.block_count as usize);
            for k in 0..span {
                let b = (idx.header_block - self.sb.obj_pool_start + k) as usize;
                if b < self.free.len() {
                    self.free[b] = true;
                }
            }
        }
        Ok(())
    }

    // ---------- 分配 ----------

    /// 首次适配分配连续 `span` 个空闲块，返回起始块号（绝对）。
    fn alloc_contiguous(&mut self, span: u64) -> Result<u64, KosError> {
        let mut run = 0u64;
        for (i, f) in self.free.iter().enumerate() {
            if *f {
                run += 1;
                if run == span {
                    let start = i as u64 + 1 - span;
                    for k in 0..span as usize {
                        self.free[start as usize + k] = false;
                    }
                    return Ok(self.sb.obj_pool_start + start);
                }
            } else {
                run = 0;
            }
        }
        Err(KosError::NoSpace)
    }
}

// ==================== 辅助 ====================

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

fn read_manifest_slot<B: crate::block::BlockDevice>(dev: &B, block: u64) -> Result<Option<KosRootManifest>, KosError> {
    let mut buf = [0u8; BLK];
    dev.read_block(block, &mut buf)?;
    if buf[..MANIFEST_SIZE].iter().all(|b| *b == 0) {
        return Ok(None);
    }
    // 槽损坏（CRC 不过）视为不存在：双槽择优的另一槽兜底。
    Ok(KosRootManifest::from_bytes(&buf[..MANIFEST_SIZE]).ok())
}

fn write_manifest_slot<B: crate::block::BlockDevice>(dev: &mut B, block: u64, m: &KosRootManifest) -> Result<(), KosError> {
    let mut buf = [0u8; BLK];
    buf[..MANIFEST_SIZE].copy_from_slice(&m.to_bytes());
    dev.write_block(block, &buf)
}
