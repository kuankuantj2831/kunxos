//! 本地记录存储（与 SFS 集成）：固定尺寸索引 + Blob 文件。
//!
//! 约束（来自 SFS 审计）：整个 /data 分区仅 128 个 inode、单文件上限约 4.2MB，
//! 无 fsync / 原子写 / rename。因此设计为**每类数据稳态仅两个文件**，并用
//! "代际文件 + 校验和"获得掉电安全（见下文"掉电安全"）：
//!
//!   - `<name>.idx`：头（magic u32 + count u32 + next_id u32）+ N 条固定尺寸
//!     记录（REC_SIZE=256 字节，见 `Rec`）。记录含 UID / 集合 / ETag / 标志 /
//!     本地与远程修订号 / 时间戳 / Blob 偏移与长度。
//!   - `<name>.blob`：原始负载（iCalendar / vCard 文本）按记录顺序连续存放。
//!
//! 掉电安全：每次保存把完整状态写入另一代文件 `<name>.idx.N` / `<name>.blob.N`
//! （N = 代号 & 1），索引头带 Blob 长度/校验和与自身校验和，索引完整写入即提交，
//! 之后才删除上一代。加载时选择校验通过且最新的一代，最新一代损坏则回退上一代；
//! 旧版 `<name>.idx` / `<name>.blob` 单文件在首次成功保存后迁移并删除。
//! 变更即全量重写（数据量小：512 条 × ~1KB）。删除使用墓碑
//! （FLAG_DELETED）。显式 compact 保留待同步/冲突删除，回收已确认的墓碑；
//! 经 `Fs::write_file_at` 流式重建 blob，无需第二块大缓冲。
//!
//! 查询 API：按 UID 精确查找、按集合枚举、按时间范围查询（事件）、
//! 脏记录枚举（同步队列）。索引记录本身是 Copy 结构，可整体拷入
//! 调用方缓冲参与排序 / 过滤。

use crate::sync::fs::Fs;
use crate::sync::model::*;
use crate::sync::util;

pub const IDX_MAGIC: u32 = 0x4B_58_53_59; // "KXSY"
pub const BLOB_MAGIC: u32 = 0x4B_58_42_4C; // "KXBL"
/// 代际索引格式（带自身校验和与 Blob 校验和）。
pub const IDX_MAGIC2: u32 = 0x4B_58_53_32; // "KXS2"
/// 代际索引头：magic, count, next_id, generation, blob_len, blob_fnv, idx_fnv（各 u32）。
const GEN_HDR: usize = 28;
/// 代际文件路径缓冲：基路径（≤64）+ ".0"/".1"。
const PATH_BUF: usize = 72;

/// 索引记录（固定 256 字节，磁盘布局见 `Rec::to_bytes`）。
/// 记录键 = 服务器资源路径（href）；iCalendar/vCard 内的 UID 在 Blob 中，
/// 解析时进入 Event.uid / Contact.uid。
#[derive(Clone, Copy)]
pub struct Rec {
    pub key: [u8; KEY_LEN],
    pub key_len: usize,
    pub coll: u8,
    pub etag: [u8; ETAG_LEN],
    pub etag_len: usize,
    pub flags: u32,
    pub lrev: u32,
    pub rrev: u32,
    pub mtime: u64,
    pub boff: u32,
    pub blen: u32,
    pub seq: u32,
}

impl Rec {
    pub fn new() -> Self {
        Rec {
            key: [0; KEY_LEN],
            key_len: 0,
            coll: 0,
            etag: [0; ETAG_LEN],
            etag_len: 0,
            flags: 0,
            lrev: 0,
            rrev: 0,
            mtime: 0,
            boff: 0,
            blen: 0,
            seq: 0,
        }
    }

    pub fn key_slice(&self) -> &[u8] {
        &self.key[..self.key_len]
    }

    pub fn etag_slice(&self) -> &[u8] {
        &self.etag[..self.etag_len]
    }

    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
    pub fn is_dirty(&self) -> bool {
        self.flags & FLAG_DIRTY != 0
    }
    pub fn is_conflict(&self) -> bool {
        self.flags & FLAG_CONFLICT != 0
    }

    /// 序列化到 256 字节记录。
    pub fn to_bytes(&self, out: &mut [u8; REC_SIZE]) {
        let n = self.key.len().min(KEY_LEN);
        out[..n].copy_from_slice(&self.key[..n]);
        out[128] = self.coll;
        let en = self.etag.len().min(ETAG_LEN);
        out[129..129 + en].copy_from_slice(&self.etag[..en]);
        out[177..181].copy_from_slice(&self.flags.to_le_bytes());
        out[181..185].copy_from_slice(&self.lrev.to_le_bytes());
        out[185..189].copy_from_slice(&self.rrev.to_le_bytes());
        out[189..197].copy_from_slice(&self.mtime.to_le_bytes());
        out[197..201].copy_from_slice(&self.boff.to_le_bytes());
        out[201..205].copy_from_slice(&self.blen.to_le_bytes());
        out[205..209].copy_from_slice(&self.seq.to_le_bytes());
        // 209..256 保留（零填充）
    }

    /// 从 256 字节记录反序列化。
    pub fn from_bytes(b: &[u8; REC_SIZE]) -> Self {
        let mut r = Rec::new();
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&b[..KEY_LEN]);
        r.key = key;
        r.key_len = KEY_LEN;
        if let Some(z) = r.key.iter().position(|&c| c == 0) {
            r.key_len = z;
        }
        r.coll = b[128];
        let mut etag = [0u8; ETAG_LEN];
        etag.copy_from_slice(&b[129..129 + ETAG_LEN]);
        r.etag = etag;
        r.etag_len = ETAG_LEN;
        if let Some(z) = r.etag.iter().position(|&c| c == 0) {
            r.etag_len = z;
        }
        r.flags = u32::from_le_bytes([b[177], b[178], b[179], b[180]]);
        r.lrev = u32::from_le_bytes([b[181], b[182], b[183], b[184]]);
        r.rrev = u32::from_le_bytes([b[185], b[186], b[187], b[188]]);
        r.mtime = u64::from_le_bytes([b[189], b[190], b[191], b[192], b[193], b[194], b[195], b[196]]);
        r.boff = u32::from_le_bytes([b[197], b[198], b[199], b[200]]);
        r.blen = u32::from_le_bytes([b[201], b[202], b[203], b[204]]);
        r.seq = u32::from_le_bytes([b[205], b[206], b[207], b[208]]);
        r
    }
}

/// 存储视图：索引数组 + 调用方提供的 Blob 缓冲。
pub struct Store<'a> {
    pub recs: [Rec; MAX_RECORDS],
    pub n: usize,
    pub next_id: u32,
    pub blob: &'a mut [u8],
    pub blob_len: usize,
    pub idx_path: [u8; 64],
    pub idx_path_len: usize,
    pub blob_path: [u8; 64],
    pub blob_path_len: usize,
    pub loaded: bool,
    /// 已提交的代号（每次成功保存 +1，回绕比较）；仅 `has_gen` 为 true 时有意义。
    pub generation: u32,
    /// 磁盘上是否已有代际格式文件（旧版单文件或空存储为 false）。
    pub has_gen: bool,
}

/// Shared index scratch stays out of the small EL0 stack. The RAII lock also
/// serializes host tests and future concurrent callers; Fs callbacks must not
/// recursively invoke Store::load/save while the scratch is held.
static mut IDX_SCRATCH: [u8; GEN_HDR + MAX_RECORDS * REC_SIZE] =
    [0; GEN_HDR + MAX_RECORDS * REC_SIZE];
static IDX_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

struct IndexScratch;
impl core::ops::Deref for IndexScratch {
    type Target = [u8];
    fn deref(&self) -> &[u8] { unsafe { &*core::ptr::addr_of!(IDX_SCRATCH) } }
}
impl core::ops::DerefMut for IndexScratch {
    fn deref_mut(&mut self) -> &mut [u8] { unsafe { &mut *core::ptr::addr_of_mut!(IDX_SCRATCH) } }
}
impl Drop for IndexScratch {
    fn drop(&mut self) { IDX_BUSY.store(false, core::sync::atomic::Ordering::Release); }
}

fn idx_scratch() -> IndexScratch {
    use core::sync::atomic::Ordering;
    while IDX_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    IndexScratch
}

/// 已通过校验的一代索引头。
#[derive(Clone, Copy)]
struct GenHdr {
    generation: u32,
    count: usize,
    next_id: u32,
    blob_len: u32,
    blob_fnv: u32,
}

/// 代号 a 是否比 b 新（按 u32 回绕比较）。
fn gen_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000_0000
}

/// 拼出某一代文件的路径：`<base>.0` / `<base>.1`，返回长度。
fn slot_path(base: &[u8], slot: u32, out: &mut [u8; PATH_BUF]) -> usize {
    let n = util::copy_into(&mut out[..PATH_BUF - 2], base);
    out[n] = b'.';
    out[n + 1] = if slot & 1 == 0 { b'0' } else { b'1' };
    n + 2
}

/// 读取并校验一代索引文件（魔数、记录数、自身校验和）；内容留在 `buf` 中。
fn read_gen_idx(fs: &mut dyn Fs, path: &[u8], buf: &mut [u8]) -> Option<GenHdr> {
    let got = fs.read_file(path, buf);
    if got < GEN_HDR as isize {
        return None;
    }
    let got = got as usize;
    let rd = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    if rd(0) != IDX_MAGIC2 {
        return None;
    }
    let count = rd(4) as usize;
    if count > MAX_RECORDS || GEN_HDR + count * REC_SIZE > got {
        return None;
    }
    let len = GEN_HDR + count * REC_SIZE;
    let sum = util::fnv1a_update(
        util::fnv1a_update(util::FNV1A_BASIS, &buf[..24]),
        &buf[GEN_HDR..len],
    );
    if sum != rd(24) {
        return None;
    }
    Some(GenHdr { generation: rd(12), count, next_id: rd(8), blob_len: rd(16), blob_fnv: rd(20) })
}

impl<'a> Store<'a> {
    pub fn new(blob_buf: &'a mut [u8], idx_path: &[u8], blob_path: &[u8]) -> Self {
        let mut idx = [0u8; 64];
        let mut bl = [0u8; 64];
        let il = util::copy_into(&mut idx, idx_path);
        let bln = util::copy_into(&mut bl, blob_path);
        Store {
            recs: [Rec::new(); MAX_RECORDS],
            n: 0,
            next_id: 1,
            blob: blob_buf,
            blob_len: 0,
            idx_path: idx,
            idx_path_len: il,
            blob_path: bl,
            blob_path_len: bln,
            loaded: false,
            generation: 0,
            has_gen: false,
        }
    }

    /// 从磁盘加载。文件不存在视为空存储（返回 true）。
    ///
    /// 优先读取"代际文件"（`<idx>.0` / `<idx>.1` 与 `<blob>.0` / `<blob>.1`）：
    /// 选择校验通过且代号最新的一代；最新一代损坏（例如保存中途掉电、
    /// 写入被重排导致校验不符）时自动回退到上一代。两代都不可用而旧版
    /// 单文件仍在时（迁移窗口）读取旧版文件；只有损坏的代际文件而无旧版
    /// 文件时返回 false，不把损坏数据当作空存储。
    pub fn load(&mut self, fs: &mut dyn Fs) -> bool {
        // A failed load must never expose a partially decoded store as usable.
        self.loaded = false;
        self.n = 0;
        self.blob_len = 0;
        self.has_gen = false;
        self.generation = 0;
        let mut hdrs: [Option<GenHdr>; 2] = [None, None];
        let mut any_gen_file = false;
        {
            let mut idx_buf = idx_scratch();
            for s in 0..2u32 {
                let mut p = [0u8; PATH_BUF];
                let pl = slot_path(&self.idx_path[..self.idx_path_len], s, &mut p);
                if fs.file_exists(&p[..pl]) {
                    any_gen_file = true;
                }
                hdrs[s as usize] = read_gen_idx(fs, &p[..pl], &mut idx_buf);
            }
        }
        // 候选按代号从新到旧排列
        let mut cands = [0u32; 2];
        let mut nc = 0usize;
        match (hdrs[0], hdrs[1]) {
            (Some(a), Some(b)) => {
                cands = if gen_newer(b.generation, a.generation) { [1, 0] } else { [0, 1] };
                nc = 2;
            }
            (Some(_), None) => {
                cands[0] = 0;
                nc = 1;
            }
            (None, Some(_)) => {
                cands[0] = 1;
                nc = 1;
            }
            (None, None) => {}
        }
        for k in 0..nc {
            if self.load_gen(fs, cands[k]) {
                return true;
            }
        }
        // 没有可用的代际文件：仅当从未保存过（空存储）或旧版文件仍在（迁移窗口）时继续。
        let legacy_exists = fs.file_exists(&self.idx_path[..self.idx_path_len]);
        if !any_gen_file || legacy_exists {
            return self.load_legacy(fs);
        }
        false
    }

    /// 加载指定槽位的一代（索引校验和、Blob 长度与校验和、记录范围全部通过才成功）。
    fn load_gen(&mut self, fs: &mut dyn Fs, slot: u32) -> bool {
        let mut ip = [0u8; PATH_BUF];
        let ipl = slot_path(&self.idx_path[..self.idx_path_len], slot, &mut ip);
        let mut bp = [0u8; PATH_BUF];
        let bpl = slot_path(&self.blob_path[..self.blob_path_len], slot, &mut bp);
        let mut idx_buf = idx_scratch();
        let Some(h) = read_gen_idx(fs, &ip[..ipl], &mut idx_buf) else { return false };
        let count = h.count;
        for i in 0..count {
            let off = GEN_HDR + i * REC_SIZE;
            let mut rb = [0u8; REC_SIZE];
            rb.copy_from_slice(&idx_buf[off..off + REC_SIZE]);
            self.recs[i] = Rec::from_bytes(&rb);
        }
        drop(idx_buf);
        let want = h.blob_len as usize;
        if want > self.blob.len() {
            return false;
        }
        if want > 0 {
            let got = fs.read_file(&bp[..bpl], self.blob);
            if got < 0 || (got as usize) < want {
                return false;
            }
            if util::fnv1a(&self.blob[..want]) != h.blob_fnv {
                return false;
            }
        }
        for r in &self.recs[..count] {
            if r.is_deleted() || r.blen == 0 {
                continue;
            }
            let Some(end) = r.boff.checked_add(r.blen) else { return false };
            if end as usize > want {
                return false;
            }
        }
        self.blob_len = want;
        self.n = count;
        self.next_id = h.next_id;
        self.generation = h.generation;
        self.has_gen = true;
        self.loaded = true;
        true
    }

    /// 读取旧版（无代际、无校验和）的 `<idx>` + `<blob>` 单文件布局。
    fn load_legacy(&mut self, fs: &mut dyn Fs) -> bool {
        self.loaded = false;
        self.n = 0;
        self.blob_len = 0;
        let mut idx_buf = idx_scratch();
        let got = fs.read_file(&self.idx_path[..self.idx_path_len], &mut idx_buf);
        if got < 0 {
            // Existing but unreadable index is an error, not an empty store.
            if fs.file_exists(&self.idx_path[..self.idx_path_len]) { return false; }
            // 不存在：空存储
            self.n = 0;
            self.next_id = 1;
            self.blob_len = 0;
            self.loaded = true;
            return true;
        }
        let got = got as usize;
        if got < 12 {
            return false;
        }
        let magic = u32::from_le_bytes([idx_buf[0], idx_buf[1], idx_buf[2], idx_buf[3]]);
        if magic != IDX_MAGIC {
            return false;
        }
        let count = u32::from_le_bytes([idx_buf[4], idx_buf[5], idx_buf[6], idx_buf[7]]) as usize;
        self.next_id = u32::from_le_bytes([idx_buf[8], idx_buf[9], idx_buf[10], idx_buf[11]]);
        if count > MAX_RECORDS || 12 + count * REC_SIZE > got {
            return false;
        }
        for i in 0..count {
            let mut rec_buf = [0u8; REC_SIZE];
            rec_buf.copy_from_slice(&idx_buf[12 + i * REC_SIZE..12 + (i + 1) * REC_SIZE]);
            self.recs[i] = Rec::from_bytes(&rec_buf);
        }
        drop(idx_buf);
        let bgot = fs.read_file(&self.blob_path[..self.blob_path_len], self.blob);
        if bgot < 0 {
            // A missing blob is allowed only when no live payload needs it.
            if fs.file_exists(&self.blob_path[..self.blob_path_len]) { return false; }
        } else {
            self.blob_len = bgot as usize;
        }
        for r in &self.recs[..count] {
            if r.is_deleted() || r.blen == 0 { continue; }
            let Some(end) = r.boff.checked_add(r.blen) else { return false };
            if end as usize > self.blob_len { return false; }
        }
        self.n = count;
        self.generation = 0;
        self.has_gen = false;
        self.loaded = true;
        // Loading is read-only. Explicit compact() may reclaim acknowledged
        // tombstones; it must never discard an unsent remote deletion.

        true
    }

    /// 当前已提交代际所用的槽位（0 或 1）；尚未以代际格式保存过时返回 None。
    pub fn active_slot(&self) -> Option<u32> {
        if self.has_gen { Some(self.generation & 1) } else { None }
    }

    /// 持久化（掉电安全）：把全部状态写入"另一代"的 idx/blob 新文件，索引文件
    /// （带校验和）完整写入即为提交点；之后才在内存里压实布局并删除上一代。
    ///
    /// 保存过程中任一时刻掉电或写入被重排，磁盘上都至少有一代校验通过的完整
    /// 数据：旧代在提交前不会被改动，新代未写完时校验和不符，加载时被忽略。
    /// 保存失败（返回 false）不修改内存中的记录或 Blob 布局。
    pub fn save(&mut self, fs: &mut dyn Fs) -> bool {
        // 1) 先校验全部范围并按旧偏移排序：压实后目标偏移恒 ≤ 源偏移，可在内存中原地搬移，
        //    不需要第二块大缓冲，也不必在提交前改动内存布局。
        let mut order = [0u16; MAX_RECORDS];
        let mut m = 0usize;
        let mut final_len = 0u32;
        for i in 0..self.n {
            let r = &self.recs[i];
            if r.is_deleted() || r.blen == 0 {
                continue;
            }
            let Some(end) = r.boff.checked_add(r.blen) else { return false };
            if end as usize > self.blob_len {
                return false;
            }
            let Some(total) = final_len.checked_add(r.blen) else { return false };
            final_len = total;
            let mut j = m;
            while j > 0 && self.recs[order[j - 1] as usize].boff > r.boff {
                order[j] = order[j - 1];
                j -= 1;
            }
            order[j] = i as u16;
            m += 1;
        }
        // Blob 范围重叠说明索引已损坏：拒绝保存，避免原地压实时互相覆盖。
        let mut prev_end = 0u32;
        for k in 0..m {
            let r = &self.recs[order[k] as usize];
            if r.boff < prev_end {
                return false;
            }
            prev_end = r.boff + r.blen;
        }
        let mut new_off = [0u32; MAX_RECORDS];
        let mut acc = 0u32;
        for k in 0..m {
            let i = order[k] as usize;
            new_off[i] = acc;
            acc += self.recs[i].blen;
        }

        let new_gen = self.generation.wrapping_add(1);
        let slot = new_gen & 1;
        let mut ip = [0u8; PATH_BUF];
        let ipl = slot_path(&self.idx_path[..self.idx_path_len], slot, &mut ip);
        let mut bp = [0u8; PATH_BUF];
        let bpl = slot_path(&self.blob_path[..self.blob_path_len], slot, &mut bp);

        // 2) 新一代 Blob：截断一次，再按新偏移流式写入，同时累计校验和。
        if !fs.write_file(&bp[..bpl], &[]) {
            return false;
        }
        let mut sum = util::FNV1A_BASIS;
        for k in 0..m {
            let i = order[k] as usize;
            let r = &self.recs[i];
            let (s, e) = (r.boff as usize, (r.boff + r.blen) as usize);
            if !fs.write_file_at(&bp[..bpl], new_off[i], &self.blob[s..e]) {
                return false;
            }
            sum = util::fnv1a_update(sum, &self.blob[s..e]);
        }

        // 3) 新一代索引：完整写入（含自身校验和）即提交。
        {
            let mut idx_buf = idx_scratch();
            idx_buf[0..4].copy_from_slice(&IDX_MAGIC2.to_le_bytes());
            idx_buf[4..8].copy_from_slice(&(self.n as u32).to_le_bytes());
            idx_buf[8..12].copy_from_slice(&self.next_id.to_le_bytes());
            idx_buf[12..16].copy_from_slice(&new_gen.to_le_bytes());
            idx_buf[16..20].copy_from_slice(&final_len.to_le_bytes());
            idx_buf[20..24].copy_from_slice(&sum.to_le_bytes());
            for i in 0..self.n {
                let mut rec = self.recs[i];
                rec.boff = if rec.is_deleted() || rec.blen == 0 { 0 } else { new_off[i] };
                let mut rb = [0u8; REC_SIZE];
                rec.to_bytes(&mut rb);
                idx_buf[GEN_HDR + i * REC_SIZE..GEN_HDR + (i + 1) * REC_SIZE].copy_from_slice(&rb);
            }
            let len = GEN_HDR + self.n * REC_SIZE;
            let chk = util::fnv1a_update(
                util::fnv1a_update(util::FNV1A_BASIS, &idx_buf[..24]),
                &idx_buf[GEN_HDR..len],
            );
            idx_buf[24..28].copy_from_slice(&chk.to_le_bytes());
            if !fs.write_file(&ip[..ipl], &idx_buf[..len]) {
                return false;
            }
        }

        // 4) 已提交：把内存布局压实到新偏移（先搬数据，再改偏移）。
        for k in 0..m {
            let i = order[k] as usize;
            let (s, l) = (self.recs[i].boff as usize, self.recs[i].blen as usize);
            let d = new_off[i] as usize;
            if d != s {
                self.blob.copy_within(s..s + l, d);
            }
        }
        for i in 0..self.n {
            let live = !self.recs[i].is_deleted() && self.recs[i].blen != 0;
            self.recs[i].boff = if live { new_off[i] } else { 0 };
        }
        self.blob_len = final_len as usize;
        self.generation = new_gen;
        self.has_gen = true;

        // 5) 尽力清理：上一代，以及迁移前的旧版单文件。失败无害（加载时代号较小/被忽略）。
        let mut op = [0u8; PATH_BUF];
        let opl = slot_path(&self.idx_path[..self.idx_path_len], slot ^ 1, &mut op);
        let _ = fs.delete_file(&op[..opl]);
        let opl = slot_path(&self.blob_path[..self.blob_path_len], slot ^ 1, &mut op);
        let _ = fs.delete_file(&op[..opl]);
        if fs.file_exists(&self.idx_path[..self.idx_path_len]) {
            let _ = fs.delete_file(&self.idx_path[..self.idx_path_len]);
            let _ = fs.delete_file(&self.blob_path[..self.blob_path_len]);
        }
        true
    }

    // ==================== 查询 API ====================

    pub fn find(&self, key: &[u8]) -> Option<usize> {
        self.recs[..self.n].iter().position(|r| r.key_slice() == key && !r.is_deleted())
    }

    pub fn find_any(&self, key: &[u8]) -> Option<usize> {
        self.recs[..self.n].iter().position(|r| r.key_slice() == key)
    }

    /// 枚举集合内所有记录下标。
    pub fn query_coll(&self, coll: u8, out: &mut [usize]) -> usize {
        let mut k = 0usize;
        for i in 0..self.n {
            let r = &self.recs[i];
            if r.coll == coll && !r.is_deleted() {
                if k < out.len() {
                    out[k] = i;
                    k += 1;
                } else {
                    break;
                }
            }
        }
        k
    }

    /// 枚举所有脏记录（同步队列视图：本地修改 / 墓碑）。
    pub fn query_dirty(&self, coll: u8, out: &mut [usize]) -> usize {
        let mut k = 0usize;
        for i in 0..self.n {
            let r = &self.recs[i];
            if r.coll == coll && r.is_dirty() {
                if k < out.len() {
                    out[k] = i;
                    k += 1;
                } else {
                    break;
                }
            }
        }
        k
    }

    /// 枚举冲突记录。
    pub fn query_conflicts(&self, coll: u8, out: &mut [usize]) -> usize {
        let mut k = 0usize;
        for i in 0..self.n {
            let r = &self.recs[i];
            if r.coll == coll && r.is_conflict() {
                if k < out.len() {
                    out[k] = i;
                    k += 1;
                } else {
                    break;
                }
            }
        }
        k
    }

    /// 取记录 i 的 Blob 切片（不含墓碑）。
    pub fn blob_of(&self, i: usize) -> Option<&[u8]> {
        let r = &self.recs[i];
        if r.is_deleted() || r.blen == 0 {
            return None;
        }
        let s = r.boff as usize;
        let e = (r.boff + r.blen) as usize;
        if s > self.blob_len || e > self.blob_len {
            return None;
        }
        Some(&self.blob[s..e])
    }

    // ==================== 变更 ====================

    /// 追加（或覆盖同 UID 记录）一条新记录及其 Blob。
    /// 若 UID 已存在则更新 Blob 与标志（本地更新语义）。
    /// `mark_dirty`：本地修改置 FLAG_DIRTY。
    pub fn upsert(&mut self, rec: Rec, blob: &[u8], mark_dirty: bool) -> bool {
        // Validate capacity before modifying either metadata or record count.
        if rec.key_len == 0 || rec.key_len > KEY_LEN || rec.etag_len > ETAG_LEN
            || self.blob_len > self.blob.len()
            || blob.len() > self.blob.len() - self.blob_len { return false; }
        if let Some(i) = self.find_any(&rec.key[..rec.key_len]) {
            // 更新既有记录：替换 blob（追加到末尾），保留/合并标志
            let old = &mut self.recs[i];
            let mut flags = old.flags & !FLAG_DELETED;
            if mark_dirty {
                flags |= FLAG_DIRTY;
                flags &= !FLAG_CONFLICT;
                old.lrev = old.lrev.wrapping_add(1);
            }
            old.key = rec.key;
            old.key_len = rec.key_len;
            old.coll = rec.coll;
            old.flags = flags;
            old.seq = rec.seq;
            old.mtime = rec.mtime;
            // 远端元数据（ETag / 远端修订）随新记录更新
            if rec.etag_len > 0 {
                old.etag = rec.etag;
                old.etag_len = rec.etag_len;
            }
            if rec.rrev > 0 {
                old.rrev = rec.rrev;
            }
            // Upsert replaces an entity, including an explicitly empty payload.
            self.append_blob(i, blob);
            return true;
        }
        if self.n >= MAX_RECORDS {
            return false;
        }
        let mut r = rec;
        if mark_dirty {
            r.flags |= FLAG_DIRTY;
            r.lrev = 1;
        }
        let i = self.n;
        self.recs[i] = r;
        self.n += 1;
        self.append_blob(i, blob);
        true
    }

    fn append_blob(&mut self, i: usize, blob: &[u8]) {
        let r = &mut self.recs[i];
        r.boff = self.blob_len as u32;
        r.blen = blob.len() as u32;
        self.blob[self.blob_len..self.blob_len + blob.len()].copy_from_slice(blob);
        self.blob_len += blob.len();
    }

    /// 标记删除（墓碑）。纯本地记录直接移除。
    pub fn remove(&mut self, i: usize) {
        let local_only = self.recs[i].flags & FLAG_ON_SERVER == 0;
        if local_only {
            // 本地未推送：直接移除（含 blob 空间，留待压实）
            self.recs[i].flags = (self.recs[i].flags | FLAG_DELETED) & !(FLAG_DIRTY | FLAG_CONFLICT);
            self.recs[i].blen = 0;
        } else {
            self.recs[i].flags |= FLAG_DELETED | FLAG_DIRTY;
            self.recs[i].lrev = self.recs[i].lrev.wrapping_add(1);
        }
    }

    /// 压实：清除墓碑与孤儿 blob（内存 + 落盘）。
    pub fn compact(&mut self, fs: &mut dyn Fs) -> bool {
        let mut kept: [Rec; MAX_RECORDS] = [Rec::new(); MAX_RECORDS];
        let mut kn = 0usize;
        for i in 0..self.n {
            let r = &self.recs[i];
            if r.is_deleted() && (r.flags & FLAG_ON_SERVER == 0
                || (!r.is_dirty() && !r.is_conflict())) { continue; }
            kept[kn] = *r;
            kn += 1;
        }
        self.recs = kept;
        self.n = kn;
        self.save(fs)
    }

    /// 修改记录标志并落盘（同步引擎用）。
    pub fn set_flags(&mut self, i: usize, flags: u32, etag: &[u8], rrev: u32) -> bool {
        self.recs[i].flags = flags;
        self.recs[i].etag_len = util::copy_into(&mut self.recs[i].etag, etag);
        self.recs[i].rrev = rrev;
        true
    }

    /// 冲突解决：保留一侧。keep_local=true 保留本地（客户端优先），
    /// false 丢弃本地、以服务器 blob 为准（服务器优先）。
    pub fn resolve_conflict(&mut self, fs: &mut dyn Fs, i: usize, keep_local: bool) -> bool {
        if !keep_local {
            // 服务器优先：清 DIRTY/CONFLICT，etag 已是服务器值
            self.recs[i].flags &= !(FLAG_DIRTY | FLAG_CONFLICT);
        } else {
            // 客户端优先：保持 DIRTY，清 CONFLICT；推送时以本地覆盖服务器
            self.recs[i].flags &= !FLAG_CONFLICT;
            self.recs[i].flags |= FLAG_DIRTY;
        }
        self.save(fs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 宿主测试用内存文件系统。
    struct MemFs {
        files: HashMap<Vec<u8>, Vec<u8>>,
    }
    impl MemFs {
        fn new() -> Self {
            MemFs { files: HashMap::new() }
        }
    }
    impl Fs for MemFs {
        fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> isize {
            match self.files.get(path) {
                Some(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    n as isize
                }
                None => -1,
            }
        }
        fn write_file(&mut self, path: &[u8], data: &[u8]) -> bool {
            self.files.insert(path.to_vec(), data.to_vec());
            true
        }
        fn write_file_at(&mut self, path: &[u8], off: u32, data: &[u8]) -> bool {
            let entry = self.files.entry(path.to_vec()).or_default();
            let end = off as usize + data.len();
            if entry.len() < end {
                entry.resize(end, 0);
            }
            entry[off as usize..end].copy_from_slice(data);
            true
        }
        fn delete_file(&mut self, path: &[u8]) -> bool {
            self.files.remove(path);
            true
        }
        fn file_exists(&mut self, path: &[u8]) -> bool {
            self.files.contains_key(path)
        }
    }

    fn mk_rec(key: &[u8], coll: u8) -> Rec {
        let mut r = Rec::new();
        r.key_len = util::copy_into(&mut r.key, key);
        r.coll = coll;
        r
    }

    #[test]
    fn store_roundtrip() {
        let mut fs = MemFs::new();
        let mut blob = [0u8; 8192];
        {
            let mut s = Store::new(&mut blob, b"/data/sync/events.idx", b"/data/sync/events.blob");
            assert!(s.load(&mut fs));
            let mut r1 = mk_rec(b"ev-1", 1);
            r1.mtime = 123;
            assert!(s.upsert(r1, b"BEGIN:VCALENDAR...1", true));
            let mut r2 = mk_rec(b"ev-2", 1);
            assert!(s.upsert(r2, b"BEGIN:VCALENDAR...2", true));
            assert!(s.save(&mut fs));
            assert_eq!(s.n, 2);
            assert!(s.find(b"ev-1").is_some());
            assert!(s.find(b"nope").is_none());
        }
        // 重新加载（新缓冲模拟重启）
        let mut blob2 = [0u8; 8192];
        let mut s2 = Store::new(&mut blob2, b"/data/sync/events.idx", b"/data/sync/events.blob");
        assert!(s2.load(&mut fs));
        assert_eq!(s2.n, 2);
        let i = s2.find(b"ev-1").unwrap();
        assert_eq!(s2.blob_of(i), Some(&b"BEGIN:VCALENDAR...1"[..]));
        assert_eq!(s2.recs[i].mtime, 123);
    }

    #[test]
    fn store_upsert_and_delete() {
        let mut fs = MemFs::new();
        let mut blob = [0u8; 8192];
        let mut s = Store::new(&mut blob, b"i", b"b");
        s.load(&mut fs);
        let mut r = mk_rec(b"c1", 0);
        r.flags |= FLAG_ON_SERVER;
        s.upsert(r, b"v1", false);
        s.save(&mut fs);
        // 覆盖更新
        let mut r2 = mk_rec(b"c1", 0);
        r2.flags |= FLAG_ON_SERVER;
        s.upsert(r2, b"v2", true);
        s.save(&mut fs);
        let i = s.find(b"c1").unwrap();
        assert_eq!(s.blob_of(i), Some(&b"v2"[..]));
        assert!(s.recs[i].is_dirty());
        // 删除（墓碑）
        s.remove(i);
        assert!(s.recs[i].is_deleted());
        assert!(s.find(b"c1").is_none());
        let mut dirty = [0usize; 8];
        let n = s.query_dirty(0, &mut dirty);
        assert_eq!(n, 1);
        // Pending remote DELETE must survive explicit compaction.
        assert!(s.compact(&mut fs));
        assert_eq!(s.n, 1);
        assert!(s.recs[0].is_deleted() && s.recs[0].is_dirty());
        // Once acknowledged, the tombstone may be reclaimed.
        s.recs[0].flags &= !(FLAG_DIRTY | FLAG_CONFLICT);
        assert!(s.compact(&mut fs));
        assert_eq!(s.n, 0);
        assert!(s.find(b"c1").is_none());
    }

    #[test]
    fn store_queries() {
        let mut fs = MemFs::new();
        let mut blob = [0u8; 8192];
        let mut s = Store::new(&mut blob, b"i", b"b");
        s.load(&mut fs);
        for (uid, coll) in [(b"a".as_slice(), 0u8), (b"b", 1), (b"c", 0)] {
            let mut r = mk_rec(uid, coll);
            r.flags |= FLAG_ON_SERVER;
            s.upsert(r, b"x", false);
        }
        let mut out = [0usize; 8];
        let n = s.query_coll(0, &mut out);
        assert_eq!(n, 2);
        let n = s.query_coll(1, &mut out);
        assert_eq!(n, 1);
    }

    #[test]
    fn rec_layout_stable() {
        let mut r = mk_rec(b"uid-1234567890", 3);
        r.etag_len = util::copy_into(&mut r.etag, b"\"abc123\"");
        r.flags = FLAG_DIRTY | FLAG_CONFLICT;
        r.lrev = 7;
        r.rrev = 2;
        r.mtime = 1_700_000_000_000_000;
        r.boff = 1234;
        r.blen = 5678;
        r.seq = 9;
        let mut buf = [0u8; REC_SIZE];
        r.to_bytes(&mut buf);
        let back = Rec::from_bytes(&buf);
        assert_eq!(back.key_slice(), r.key_slice());
        assert_eq!(back.coll, 3);
        assert_eq!(back.etag_slice(), b"\"abc123\"");
        assert_eq!(back.flags, r.flags);
        assert_eq!(back.lrev, 7);
        assert_eq!(back.rrev, 2);
        assert_eq!(back.mtime, r.mtime);
        assert_eq!(back.boff, 1234);
        assert_eq!(back.blen, 5678);
        assert_eq!(back.seq, 9);
    }
}
