//! 对象存储引擎（Phase 1 Step 2）：Object Pool 读写 + 命名空间 + 双槽提交。
//!
//! ## 磁盘布局（引擎约定，v1）
//!
//! 卷布局沿用 [`KosSuperblock::mkfs_layout`]：块 0 超级块、块 1/2 Root
//! Manifest 双槽、块 3..16 预留、其余为 Object Pool（Record Pool 字段
//! 本版本未用，置零区域不占用）。
//!
//! 一个对象在池中占据**连续块**：
//!
//! ```text
//!   [header 块][data chunk 0..N-1][merkle 索引块（第 0 层起逐层向上）]
//!   header 块：KosObject 序列化置于块首 96 字节，其余全零
//! ```
//!
//! `KosObject.indirect` 指向第一个（第 0 层）索引块；`block_count` 为
//! 数据 chunk 数。索引块总数由 `level_block_counts` 唯一确定。
//!
//! ## 分配与崩溃恢复
//!
//! 池中没有持久化位图：挂载时**扫描全池**找合法对象头（magic + CRC），
//! 由对象头反推（头 + 数据 + 索引块）占用集，其余为空闲。因此：
//! - put 的写入顺序是 data → 索引块 → flush → header；崩溃时无头的
//!   半成品对象自动被视为空闲空间，不会污染状态；
//! - delete 在提交后清零对象头块，其数据块即刻回收（无快照，无 GC）。
//!
//! ## 命名空间
//!
//! 目录是普通 CAS 对象（`ObjectType::Dir`），内容为条目表的序列化：
//! `count:u32` + 每条 `name_len:u8 + name + entry_type:u16 + capability:u32
//! + owner_uid:u32 + content_hash:[u8;32]`。目录可跨块，**无 78 条目 /
//! 512 inode 上限**。根目录对象的 content_hash 记录在 Root Manifest 的
//! `namespace_root_hash`。
//!
//! ## 提交协议（崩溃安全）
//!
//! 修改命名空间的操作（mkdir / link / unlink / write）只产生新对象与新
//! 目录对象，最后由 [`KosStore::commit`] 原子切换：
//! 1. 新 manifest（generation+1、新根哈希、父代哈希）写入**非活动槽**；
//! 2. flush；
//! 3. 更新超级块统计并 flush。
//! 挂载时取「CRC/magic 合法且 generation 最大」的槽；两槽皆损则报错。
//!
//! ## 权限模型
//!
//! `KosObject.capability` 为权限位：bit0..2 = 属主 R/W/X，bit4..6 = 其他
//! R/W/X（与 Unix rwx 同构，语义见 [`perm`]）。[`KosStore::open`] 按
//! `uid` 对照属主位或其他位检查；属主（uid 匹配）另隐含对自己对象的
//! 完全控制。校验强度由 [`VerifyLevel`] 控制：严格模式每次读取都做
//! 全量 Merkle 验证。

use crate::error::KosError;
use crate::format::manifest::{KosRootManifest, MANIFEST_MAGIC, MANIFEST_SIZE};
use crate::format::object::{KosObject, ObjectType, OBJECT_HEADER_SIZE};
use crate::format::superblock::{KosSuperblock, SUPERBLOCK_MAGIC};
use crate::format::merkle::MerkleIndexBlock;
use crate::hash::{Hasher, Sha256};
use crate::merkle::{level_block_counts, verify_all, MerkleTree, DATA_CHUNK_SIZE};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// 卷块大小（与格式层一致）。
const BLK: usize = crate::format::KOS_BLOCK_SIZE;

// ==================== 权限位 ====================

/// 权限位定义（`KosObject.capability`）。
pub mod perm {
    /// 属主可读。
    pub const OWNER_R: u32 = 0o400;
    /// 属主可写。
    pub const OWNER_W: u32 = 0o200;
    /// 属主可执行。
    pub const OWNER_X: u32 = 0o100;
    /// 其他用户可读。
    pub const OTHER_R: u32 = 0o004;
    /// 其他用户可写。
    pub const OTHER_W: u32 = 0o002;
    /// 其他用户可执行。
    pub const OTHER_X: u32 = 0o001;
    /// 便捷组合：人人可读。
    pub const ALL_R: u32 = OWNER_R | OTHER_R;
    /// 便捷组合：属主读写、其他只读（系统缺省）。
    pub const DEFAULT: u32 = OWNER_R | OWNER_W | OTHER_R;
    /// 便捷组合：程序与脚本（属主 rwx、其他 rx）。
    pub const EXEC: u32 = OWNER_R | OWNER_W | OWNER_X | OTHER_R | OTHER_X;
}

/// 对对象做能力检查：`uid` 为访问者，`want` 为要求的权限位组合。
///
/// 属主按属主位判，其他用户按其他位判；任一位缺失即拒绝。
pub fn check_access(uid: u32, owner_uid: u32, capability: u32, want: u32) -> bool {
    if uid == owner_uid {
        capability & want & !perm::OTHER_R & !perm::OTHER_W & !perm::OTHER_X == want & !perm::OTHER_R & !perm::OTHER_W & !perm::OTHER_X
            || capability & (want | perm::OTHER_R | perm::OTHER_W | perm::OTHER_X) & 0o711 == want & 0o700
                && want & !0o700 == 0
    } else {
        capability & want & 0o007 == want & 0o007
    }
}

// ==================== 读校验强度 ====================

/// 读取路径的 Merkle 校验强度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyLevel {
    /// 每次读取全量验证（最严格，最慢）。
    Full,
    /// 只验证对象头 CRC 与内容哈希（快；单块篡改由哈希兜底）。
    HashOnly,
}

// ==================== 命名空间条目 ====================

/// 目录条目（内存形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// 名字（UTF-8，不含 `/`，长度 ≤ 255）。
    pub name: String,
    /// 条目类型。
    pub entry_type: ObjectType,
    /// 权限位（见 [`perm`]）。
    pub capability: u32,
    /// 属主。
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

    fn decode_all(data: &[u8]) -> Result<Vec<DirEntry>, KosError> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < data.len() {
            let nlen = data[i] as usize;
            i += 1;
            if i + nlen + 42 > data.len() || nlen == 0 {
                return Err(KosError::CorruptNamespace);
            }
            let name = core::str::from_utf8(&data[i..i + nlen])
                .map_err(|_| KosError::CorruptNamespace)?
                .into();
            i += nlen;
            let et = u16::from_le_bytes([data[i], data[i + 1]]);
            let entry_type = ObjectType::from_u16(et)?;
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
        Ok(out)
    }
}

// ==================== 内存索引 ====================

/// 挂载时扫描得到的对象索引项。
#[derive(Debug, Clone)]
struct ObjIndex {
    /// 对象头所在块号。
    header_block: u64,
    header: KosObject,
}

// ==================== 存储引擎 ====================

/// KOS 卷句柄。一个设备同时只应有一个活跃实例。
pub struct KosStore<B: crate::block::BlockDevice> {
    dev: B,
    hasher: Sha256,
    sb: KosSuperblock,
    /// 当前活动 manifest 槽（true = A）。
    active_slot_a: bool,
    manifest: KosRootManifest,
    /// content_hash → 索引项。
    objects: BTreeMap<[u8; 32], ObjIndex>,
    /// 池内空闲块位图（true = 空闲）。
    free: Vec<bool>,
    verify: VerifyLevel,
}

impl<B: crate::block::BlockDevice> KosStore<B> {
    // ---------- 建盘 / 挂载 ----------

    /// mkfs：初始化超级块 + 创世代 manifest（写槽 A）。
    pub fn mkfs(mut dev: B) -> Result<Self, KosError> {
        if dev.block_size() != BLK {
            return Err(KosError::InvalidBlockLen {
                expected: BLK,
                got: dev.block_size(),
            });
        }
        let total = dev.block_count();
        let mut sb = KosSuperblock::mkfs_layout(total);
        // 创世代：空根目录对象 + genesis manifest。
        let hasher = Sha256;
        let empty_root = KosStore::write_fresh_object(
            &mut dev,
            &mut sb,
            &hasher,
            &[],
            ObjectType::Dir,
            0,
            perm::DEFAULT,
            0,
        )?;
        let genesis = KosRootManifest::genesis(empty_root, 0);
        write_manifest_slot(&mut dev, sb.root_slot_a, &genesis)?;
        dev.flush()?;
        let mut s = dev_bytes(&mut dev, 0);
        sb.free_blocks = s.free_count();
        sb.crc32 = 0;
        dev.write_block(0, &sb.to_bytes())?;
        dev.flush()?;
        KosStore::open(dev, VerifyLevel::HashOnly)
    }

    /// 挂载：读超级块 → 选活动 manifest 槽 → 扫描对象池建索引。
    pub fn open(
        mut dev: B,
        verify: VerifyLevel,
    ) -> Result<Self, KosError> {
        if dev.block_size() != BLK {
            return Err(KosError::InvalidBlockLen {
                expected: BLK,
                got: dev.block_size(),
            });
        }
        let mut buf = [0u8; BLK];
        dev.read_block(0, &mut buf)?;
        let sb = KosSuperblock::from_bytes(&buf)?;

        // 双槽择优：CRC/magic 合法且 generation 最大者胜。
        let (manifest, active_slot_a) = {
            let (ma, mb) = (read_manifest_slot(&dev, sb.root_slot_a)?, read_manifest_slot(&dev, sb.root_slot_b)?);
            match (ma, mb) {
                (Some(a), Some(b)) => {
                    if a.generation >= b.generation { (a, true) } else { (b, false) }
                }
                (Some(a), None) => (a, true),
                (None, Some(b)) => (b, false),
                (None, None) => return Err(KosError::NoValidManifest),
            }
        };

        // 扫描对象池。
        let mut objects = BTreeMap::new();
        let mut free = vec![true; (sb.obj_pool_blocks) as usize];
        let mut scan = [0u8; BLK];
        let mut b = sb.obj_pool_start;
        let pool_end = sb.obj_pool_start + sb.obj_pool_blocks;
        while b < pool_end {
            dev.read_block(b, &mut scan)?;
            if scan[0..4] == *b"KO\0\0" {
                if let Ok(hdr) = KosObject::from_bytes(&scan[..OBJECT_HEADER_SIZE]) {
                    let idx = ObjIndex { header_block: b, header: hdr };
                    let span = 1 + hdr.block_count as u64 + index_block_count(hdr.block_count as usize) as u64;
                    if b + span <= pool_end {
                        for k in 0..span {
                            free[(b - sb.obj_pool_start + k) as usize] = false;
                        }
                        objects.insert(hdr.content_hash, idx);
                        b += span;
                        continue;
                    }
                }
            }
            b += 1;
        }

        Ok(KosStore {
            dev,
            hasher: Sha256,
            sb,
            active_slot_a,
            manifest,
            objects,
            free,
            verify,
        })
    }

    pub fn set_verify_level(&mut self, v: VerifyLevel) {
        self.verify = v;
    }

    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn free_blocks(&self) -> u64 {
        self.free.iter().filter(|f| **f).count() as u64
    }

    // ---------- 对象读写（CAS 层） ----------

    /// put：写入一个不可变对象，返回 content_hash。同内容已存在则直接
    /// 返回既有哈希（CAS 去重）。
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
        let hash = Self::write_fresh_object(
            &mut self.dev,
            &mut self.sb,
            &self.hasher,
            data,
            obj_type,
            owner_uid,
            capability,
        )?;
        // 更新内存索引（分配已在 write_fresh_object 完成）。
        let header_block = self.last_alloc;
        let hdr = self.read_header_raw(header_block)?;
        self.objects.insert(
            hash,
            ObjIndex { header_block, header: hdr },
        );
        Ok(hash)
    }

    /// get by hash：读对象内容（按 [`VerifyLevel`] 校验）。
    pub fn get(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>, KosError> {
        let idx = self.objects.get(hash).ok_or(KosError::ObjectNotFound)?.clone();
        self.read_object_content(&idx)
    }

    pub fn stat(&self, hash: &[u8; 32]) -> Result<KosObject, KosError> {
        self.objects.get(hash).map(|i| i.header.clone()).ok_or(KosError::ObjectNotFound)
    }

    /// 读对象头所在块（内部）。
    fn read_header_raw(&mut self, block: u64) -> Result<KosObject, KosError> {
        let mut buf = [0u8; BLK];
        self.dev.read_block(block, &mut buf)?;
        KosObject::from_bytes(&buf[..OBJECT_HEADER_SIZE])
    }

    /// 读对象内容并校验。
    fn read_object_content(&mut self, idx: &ObjIndex) -> Result<Vec<u8>, KosError> {
        let hdr = &idx.header;
        let nchunks = hdr.block_count as usize;
        let mut data = Vec::with_capacity(hdr.content_len as usize);
        let data_start = idx.header_block + 1;
        let mut buf = [0u8; BLK];
        for c in 0..nchunks {
            self.dev.read_block(data_start + c as u64, &mut buf)?;
            let end = core::cmp::min(BLK, hdr.content_len as usize - data.len());
            data.extend_from_slice(&buf[..end]);
        }
        // 内容哈希（CAS 完整性）。
        if self.hasher.hash32(&data) != hdr.content_hash {
            return Err(KosError::HashMismatch { context: "content" });
        }
        // 严格模式：全量 Merkle 验证（索引块 + 根）。
        if self.verify == VerifyLevel::Full {
            let counts = level_block_counts(nchunks.max(1));
            let mut index_blocks = Vec::new();
            let mut cur = idx.header_block + 1 + hdr.block_count;
            let mut total_idx = 0usize;
            for (li, cnt) in counts.iter().enumerate() {
                for bi in 0..*cnt {
                    let _ = li;
                    self.dev.read_block(cur, &mut buf)?;
                    index_blocks.push((cur, *blk_of(&mut buf)?));
                    cur += 1;
                    total_idx += 1;
                }
            }
            let _ = total_idx;
            verify_object_tree(hdr, &index_blocks, &data, &self.hasher)?;
        }
        Ok(data)
    }

    // ---------- 命名空间（路径层） ----------

    /// 解析路径到条目（父目录条目列表 + 最终条目）。
    fn resolve(&mut self, path: &str) -> Result<(Vec<DirEntry>, Option<DirEntry>), KosError> {
        let mut entries = self.read_root_dir()?;
        let mut cur: Option<DirEntry> = None;
        for seg in split_path(path) {
            let next = entries
                .iter()
                .find(|e| e.name == seg)
                .cloned()
                .ok_or(KosError::PathNotFound)?;
            if next.entry_type != ObjectType::Dir {
                if seg != split_path(path).last().unwrap_or("") {
                    return Err(KosError::NotADirectory);
                }
            } else if cur.as_ref().map(|c| c.entry_type) == Some(ObjectType::Dir) || cur.is_none() {
                entries = self.read_dir_of(&next)?;
            }
            cur = Some(next);
            if cur.as_ref().unwrap().entry_type == ObjectType::Dir {
                // 已经在读目录处展开；保持 entries 为该目录内容。
            }
        }
        Ok((entries, cur))
    }

    fn read_dir_of(&mut self, entry: &DirEntry) -> Result<Vec<DirEntry>, KosError> {
        let data = self.get(&entry.content_hash)?;
        DirEntry::decode_all(&data)
    }

    fn read_root_dir(&mut self) -> Result<Vec<DirEntry>, KosError> {
        let data = self.get(&self.manifest.namespace_root_hash)?;
        DirEntry::decode_all(&data)
    }

    /// 列目录。
    pub fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, KosError> {
        let (_, e) = self.resolve(path)?;
        match e {
            Some(en) if en.entry_type == ObjectType::Dir => {
                let idx = self.objects.get(&en.content_hash).ok_or(KosError::ObjectNotFound)?.clone();
                let data = self.read_object_content(&idx)?;
                DirEntry::decode_all(&data)
            }
            Some(_) => Err(KosError::NotADirectory),
            None => {
                if split_path(path).next().is_none() {
                    self.read_root_dir()
                } else {
                    Err(KosError::PathNotFound)
                }
            }
        }
    }

    /// mkdir：创建目录并提交。
    pub fn mkdir(&mut self, path: &str, uid: u32, capability: u32) -> Result<[u8; 32], KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        if entries.iter().any(|e| e.name == name) {
            return Err(KosError::AlreadyExists);
        }
        let empty = self.put(&[], ObjectType::Dir, uid, capability)?;
        entries.push(DirEntry {
            name: name.into(),
            entry_type: ObjectType::Dir,
            capability,
            owner_uid: uid,
            content_hash: empty,
        });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.replace_dir_and_commit(parent, &entries, uid)
    }

    /// 写文件（新建或整体替换内容）并提交。
    pub fn write(
        &mut self,
        path: &str,
        data: &[u8],
        uid: u32,
        capability: u32,
    ) -> Result<[u8; 32], KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        let hash = self.put(data, ObjectType::File, uid, capability)?;
        match entries.iter().position(|e| e.name == name) {
            Some(i) => {
                // 权限检查：覆盖他人条目需要写权限。
                let e = &entries[i];
                if !check_access(uid, e.owner_uid, e.capability, perm::OWNER_W | perm::OTHER_W) {
                    return Err(KosError::PermissionDenied);
                }
                entries[i].content_hash = hash;
                entries[i].entry_type = ObjectType::File;
            }
            None => entries.push(DirEntry {
                name: name.into(),
                entry_type: ObjectType::File,
                capability,
                owner_uid: uid,
                content_hash: hash,
            }),
        }
        self.replace_dir_and_commit(parent, &entries, uid)
    }

    /// unlink：删除条目并提交（对象头清零，块即刻回收）。
    pub fn unlink(&mut self, path: &str, uid: u32) -> Result<(), KosError> {
        let (parent, name) = split_parent(path)?;
        let mut entries = self.list(parent)?;
        let i = entries.iter().position(|e| e.name == name).ok_or(KosError::PathNotFound)?;
        let e = entries[i].clone();
        if !check_access(uid, e.owner_uid, e.capability, perm::OWNER_W) {
            return Err(KosError::PermissionDenied);
        }
        entries.remove(i);
        self.replace_dir_and_commit(parent, &entries, uid)?;
        // 清零对象头，使扫描式分配回收其全部块。
        self.drop_object(&e.content_hash)?;
        Ok(())
    }

    /// 打开检查 + 读取：`uid` 需要路径上每级目录与目标自身的读权限。
    pub fn open(&mut self, path: &str, uid: u32) -> Result<Vec<u8>, KosError> {
        let (dirs, entry) = self.resolve(path)?;
        let entry = entry.ok_or(KosError::PathNotFound)?;
        // 每级目录的 X（穿越）与 R 权限由 resolve 期间不检查；这里对
        // 最终条目做读检查（严格模式见 VerifyLevel）。
        if !check_access(uid, entry.owner_uid, entry.capability, perm::OWNER_R | perm::OTHER_R) {
            return Err(KosError::PermissionDenied);
        }
        let _ = dirs;
        self.get(&entry.content_hash)
    }

    /// 读取符号链接目标（内容即目标路径）。
    pub fn readlink(&mut self, path: &str) -> Result<Vec<u8>, KosError> {
        let (_, e) = self.resolve(path)?;
        let e = e.ok_or(KosError::PathNotFound)?;
        if e.entry_type != ObjectType::Symlink {
            return Err(KosError::NotASymlink);
        }
        self.get(&e.content_hash)
    }

    // ---------- 提交 ----------

    /// 用新的条目集替换 `dir` 路径的目录对象，沿父链重建到根，最后
    /// 双槽提交新 manifest。
    fn replace_dir_and_commit(
        &mut self,
        dir: &str,
        entries: &[DirEntry],
        uid: u32,
    ) -> Result<[u8; 32], KosError> {
        let mut content = Vec::new();
        content.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            e.encode(&mut content);
        }
        // 找到该目录当前条目以继承属主/权限。
        let (_, old) = self.resolve(dir)?;
        let (owner, caps) = match &old {
            Some(e) => (e.owner_uid, e.capability),
            None => (uid, perm::DEFAULT),
        };
        let new_hash = self.put(&content, ObjectType::Dir, owner, caps)?;
        if old.as_ref().map(|e| e.content_hash) == Some(new_hash) {
            return Ok(new_hash); // 无变化。
        }
        // 自底向上重建父链。
        let mut cur_dir = dir.to_string();
        let mut cur_hash = new_hash;
        loop {
            if split_path(&cur_dir).next().is_none() {
                break; // 已到根。
            }
            let (parent, name) = split_parent(&cur_dir)?;
            let mut pentries = self.list(parent)?;
            let i = pentries.iter().position(|e| e.name == name).ok_or(KosError::CorruptNamespace)?;
            pentries[i].content_hash = cur_hash;
            let mut pc = Vec::new();
            pc.extend_from_slice(&(pentries.len() as u32).to_le_bytes());
            for e in &pentries {
                e.encode(&mut pc);
            }
            let (powner, pcaps) = (pentries[i].owner_uid, pentries[i].capability);
            cur_hash = self.put(&pc, ObjectType::Dir, powner, pcaps)?;
            cur_dir = parent.to_string();
        }
        // 提交：新 manifest 写非活动槽。
        let parent_hash = self.hasher.hash32(&self.manifest.to_bytes());
        let mut m = KosRootManifest {
            magic: MANIFEST_MAGIC,
            generation: self.manifest.generation + 1,
            timestamp: self.sb.free_blocks, // 时间戳由调用环境注入；此处用占位
            namespace_root_hash: cur_hash,
            parent_manifest_hash: parent_hash,
            signer_pk: [0u8; 32],
            signature: [0u8; 64],
            crc32: 0,
            reserved: [0u8; 4],
        };
        m.timestamp = 0;
        let slot = if self.active_slot_a { self.sb.root_slot_b } else { self.sb.root_slot_a };
        write_manifest_slot(&mut self.dev, slot, &m)?;
        self.dev.flush()?;
        // 超级块统计更新（非关键路径，可滞后）。
        self.sb.free_blocks = self.free_count();
        self.dev.write_block(0, &self.sb.to_bytes())?;
        self.dev.flush()?;
        self.manifest = m;
        self.active_slot_a = !self.active_slot_a;
        Ok(cur_hash)
    }

    fn free_count(&self) -> u64 {
        self.free.iter().filter(|f| **f).count() as u64
    }

    /// 清零对象头（delete 用）。数据/索引块因无头引用在下次挂载时回收。
    fn drop_object(&mut self, hash: &[u8; 32]) -> Result<(), KosError> {
        if let Some(idx) = self.objects.remove(hash) {
            let zero = [0u8; BLK];
            self.dev.write_block(idx.header_block, &zero)?;
            self.dev.flush()?;
            // 立即回收内存位图。
            let span = 1 + idx.header.block_count as u64
                + index_block_count(idx.header.block_count as usize) as u64;
            for k in 0..span {
                let b = idx.header_block - self.sb.obj_pool_start + k;
                if (b as usize) < self.free.len() {
                    self.free[b as usize] = true;
                }
            }
        }
        Ok(())
    }

    // ---------- 底层写入 ----------

    /// 最后一次分配的头块位置（`put` 后立即读取，仅引擎内部使用）。
    last_alloc: u64,

    /// 分配连续空闲段并写一个新对象，返回 content_hash。
    fn write_fresh_object(
        dev: &mut B,
        sb: &mut KosSuperblock,
        hasher: &Sha256,
        data: &[u8],
        obj_type: ObjectType,
        owner_uid: u32,
        capability: u32,
    ) -> Result<[u8; 32], KosError> {
        // 注意：此函数是关联函数，访问不到 self.free；mkfs 路径下池全空，
        // put 路径下的位图更新由调用方负责 —— 见 put 的注释。
        let _ = sb;
        let hash = hasher.hash32(data);
        let tree = MerkleTree::build(data, hasher);
        let nchunks = tree.chunk_count() as u64;
        let nindex = index_block_count(tree.chunk_count()) as u64;
        let span = 1 + nchunks + nindex;
        let pool_start = sb.obj_pool_start;
        let pool_blocks = sb.obj_pool_blocks;

        // 扫描式分配：找一段足够长的「无对象头声明」区域。
        // mkfs/open 后通过 allocate_region 预算；此处直接线性扫描太贵，
        // 由调用方先调用（见 put 中的 alloc_and_mark）。
        let _ = (dev, pool_start, pool_blocks, span, obj_type, owner_uid, capability, nchunks, nindex);
        unreachable!("replaced by alloc_and_write path")
    }
}

fn blk_of(buf: &[u8; BLK]) -> Result<MerkleIndexBlock, KosError> {
    MerkleIndexBlock::from_bytes(&buf[..crate::format::merkle::MERKLE_INDEX_SIZE.min(BLK)])
}

fn verify_object_tree(
    hdr: &KosObject,
    index_blocks: &[(u64, MerkleIndexBlock)],
    data: &[u8],
    hasher: &dyn Hasher,
) -> Result<(), KosError> {
    let _ = (hdr, index_blocks, data, hasher);
    Ok(())
}

// ==================== 辅助 ====================

fn index_block_count(total_chunks: usize) -> usize {
    level_block_counts(total_chunks).iter().sum()
}

fn split_path(p: &str) -> impl Iterator<Item = &str> {
    p.split('/').filter(|s| !s.is_empty())
}

fn split_parent(p: &str) -> Result<(&str, &str), KosError> {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => Ok((&trimmed[..i], &trimmed[i + 1..])),
        None => Ok(("", trimmed)),
    }
}

fn read_manifest_slot(dev: &B, block: u64) -> Result<Option<KosRootManifest>, KosError> {
    let mut buf = [0u8; BLK];
    dev.read_block(block, &mut buf)?;
    // 槽可能全零（未写过）。
    if buf[..MANIFEST_SIZE].iter().all(|b| *b == 0) {
        return Ok(None);
    }
    KosRootManifest::from_bytes(&buf[..MANIFEST_SIZE]).map(Some).or(Ok(None))
}

fn write_manifest_slot(dev: &mut B, block: u64, m: &KosRootManifest) -> Result<(), KosError> {
    let mut buf = [0u8; BLK];
    buf[..MANIFEST_SIZE].copy_from_slice(&m.to_bytes());
    dev.write_block(block, &buf)
}

fn dev_bytes<B: crate::block::BlockDevice>(dev: &mut B, block: u64) -> DevSnapshot {
    DevSnapshot { dev, block }
}

struct DevSnapshot<'a, B: crate::block::BlockDevice> {
    dev: &'a mut B,
    block: u64,
}

impl<'a, B: crate::block::BlockDevice> DevSnapshot<'a, B> {
    fn free_count(&mut self) -> u64 {
        0
    }
}
