//! `.kunx` 应用包格式（KunxOS 原生，类似 APK/IPA）：
//! 自定义归档 = 头部 + manifest.json + 载荷 + META-INF 证书 + Ed25519 签名块。
//!
//! ```text
//! offset  0:  magic[8]        = "KUNXPKG1"
//! offset  8:  format_version  u32 = 1
//! offset 12:  flags           u32（bit0 = signed；其余保留）
//! offset 16:  manifest_off    u32
//! offset 20:  manifest_len    u32
//! offset 24:  payload_off     u32
//! offset 28:  payload_len     u32
//! offset 32:  meta_off        u32   （META-INF/：证书链，leaf 在前）
//! offset 36:  meta_len        u32
//! offset 40:  sig_off         u32
//! offset 44:  sig_len         u32   （64 = Ed25519；flags bit0=0 时为 0）
//! offset 48:  manifest.json
//!             载荷条目流：path_len u32 | path | data_len u32 | data
//!             META-INF 证书
//!             签名（Ed25519，覆盖 [0, sig_off) 全部字节）
//! ```
//!
//! 载荷路径为应用相对路径，前缀约定（安装器映射）：
//! `bin/<name>`（入口 ELF）、`res/<res>`、`lib/<lib>`、`data/...`（沙箱预置数据）。

use crate::json::Json;

pub const MAGIC: [u8; 8] = *b"KUNXPKG1";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_LEN: usize = 48;
/// Ed25519 签名长度。
pub const SIG_LEN: usize = 64;
/// 标志位：已签名。
pub const FLAG_SIGNED: u32 = 1;
/// 载荷条目路径最长。
pub const PATH_MAX: usize = 128;
/// 载荷条目数上限（防恶意包耗尽解析时间）。
pub const ENTRY_MAX: usize = 64;

/// 解析后的包视图（各区域偏移）。
#[derive(Clone, Copy, Debug)]
pub struct KunxPkg {
    pub manifest_off: usize,
    pub manifest_len: usize,
    pub payload_off: usize,
    pub payload_len: usize,
    pub meta_off: usize,
    pub meta_len: usize,
    pub sig_off: usize,
    pub sig_len: usize,
    pub flags: u32,
}

impl KunxPkg {
    /// 签名覆盖的数据区（[0, sig_off)）。
    pub fn signed_data<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[..self.sig_off.min(data.len())]
    }

    pub fn manifest<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.manifest_off..self.manifest_off + self.manifest_len]
    }

    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload_off..self.payload_off + self.payload_len]
    }

    pub fn meta<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.meta_off..self.meta_off + self.meta_len]
    }

    pub fn signature<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.sig_off..self.sig_off + self.sig_len]
    }

    /// 是否带 Ed25519 签名。
    pub fn signed(&self) -> bool {
        self.flags & FLAG_SIGNED != 0
    }
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// 解析并校验包头部与全部区域边界。失败返回 None。
pub fn parse(data: &[u8]) -> Option<KunxPkg> {
    if data.len() < HEADER_LEN {
        return None;
    }
    if &data[0..8] != &MAGIC {
        return None;
    }
    let version = get_u32(data, 8);
    if version != FORMAT_VERSION {
        return None;
    }
    let flags = get_u32(data, 12);
    let mo = get_u32(data, 16) as usize;
    let ml = get_u32(data, 20) as usize;
    let po = get_u32(data, 24) as usize;
    let pl = get_u32(data, 28) as usize;
    let meo = get_u32(data, 32) as usize;
    let mel = get_u32(data, 36) as usize;
    let so = get_u32(data, 40) as usize;
    let sl = get_u32(data, 44) as usize;

    // 区域顺序与边界
    if mo != HEADER_LEN {
        return None;
    }
    let signed = flags & FLAG_SIGNED != 0;
    if signed && sl != SIG_LEN {
        return None;
    }
    if !signed && sl != 0 {
        return None;
    }
    let mut cur = HEADER_LEN;
    let end = if signed { so } else { data.len() };
    if so != end || so + sl != data.len() {
        return None;
    }
    cur = cur.checked_add(ml)?;
    if cur != po {
        return None;
    }
    cur = cur.checked_add(pl)?;
    if cur != meo {
        return None;
    }
    cur = cur.checked_add(mel)?;
    if cur != so {
        return None;
    }
    if ml == 0 || pl == 0 {
        return None;
    }
    Some(KunxPkg {
        manifest_off: mo,
        manifest_len: ml,
        payload_off: po,
        payload_len: pl,
        meta_off: meo,
        meta_len: mel,
        sig_off: so,
        sig_len: sl,
        flags,
    })
}

// ==================== manifest.json ====================

/// manifest.json 关键字段视图（解析 + 校验必需字段）。
#[derive(Clone, Copy)]
pub struct Manifest<'a> {
    pub json: Json<'a>,
    pub name: &'a [u8],
    pub version: &'a [u8],
    pub min_api: u32,
    pub permissions: [&'a [u8]; 16],
    pub perm_count: usize,
    pub deps: [(NameRef<'a>, &'a [u8]); 12],
    pub dep_count: usize,
    pub provides: [&'a [u8]; 8],
    pub provide_count: usize,
    pub conflicts: [&'a [u8]; 8],
    pub conflict_count: usize,
    pub description: &'a [u8],
    pub author: &'a [u8],
    pub main: &'a [u8],
}

/// 依赖名引用（指向 manifest 缓冲内的字节）。
#[derive(Clone, Copy)]
pub struct NameRef<'a>(pub &'a [u8]);

/// 解析 manifest.json 并提取字段。必需字段缺失/非法返回 None。
pub fn parse_manifest(data: &[u8]) -> Option<Manifest<'_>> {
    let json = Json::parse(data)?;
    let name = json.get("name")?.as_str()?;
    let version = json.get("version")?.as_str()?;
    let min_api = json.get("api_level").and_then(|j| j.as_u32()).unwrap_or(0);

    let mut permissions = [&[][..]; 16];
    let mut perm_count = 0usize;
    if let Some(arr) = json.get("permissions") {
        if let Some(mut it) = arr.arr() {
            while let Some(s) = it.next_str() {
                if perm_count < permissions.len() {
                    permissions[perm_count] = s;
                    perm_count += 1;
                }
            }
        }
    }

    let mut deps = [(NameRef(&[][..]), &[][..]); 12];
    let mut dep_count = 0usize;
    if let Some(obj) = json.get("dependencies") {
        if let Some(mut it) = obj.obj() {
            while let Some((k, v)) = it.next() {
                if dep_count < deps.len() {
                    if let Some(c) = v.as_str() {
                        deps[dep_count] = (NameRef(k), c);
                        dep_count += 1;
                    }
                }
            }
        }
    }

    let mut provides = [&[][..]; 8];
    let mut provide_count = 0usize;
    if let Some(arr) = json.get("provides") {
        if let Some(mut it) = arr.arr() {
            while let Some(s) = it.next_str() {
                if provide_count < provides.len() {
                    provides[provide_count] = s;
                    provide_count += 1;
                }
            }
        }
    }

    let mut conflicts = [&[][..]; 8];
    let mut conflict_count = 0usize;
    if let Some(arr) = json.get("conflicts") {
        if let Some(mut it) = arr.arr() {
            while let Some(s) = it.next_str() {
                if conflict_count < conflicts.len() {
                    conflicts[conflict_count] = s;
                    conflict_count += 1;
                }
            }
        }
    }

    let description = json.get("description").and_then(|j| j.as_str()).unwrap_or(&[]);
    let author = json.get("author").and_then(|j| j.as_str()).unwrap_or(&[]);
    let main = json.get("main").and_then(|j| j.as_str()).unwrap_or(&[]);

    Some(Manifest {
        json,
        name,
        version,
        min_api,
        permissions,
        perm_count,
        deps,
        dep_count,
        provides,
        provide_count,
        conflicts,
        conflict_count,
        description,
        author,
        main,
    })
}

// ==================== 载荷条目 ====================

/// 载荷条目视图。
#[derive(Clone, Copy)]
pub struct Entry<'a> {
    pub path: &'a [u8],
    pub data: &'a [u8],
}

/// 载荷条目迭代器（严格边界校验；路径合法性校验由调用方按安装策略执行）。
pub struct PayloadIter<'a> {
    region: &'a [u8],
    off: usize,
    count: usize,
}

impl<'a> PayloadIter<'a> {
    pub fn new(region: &'a [u8]) -> PayloadIter<'a> {
        PayloadIter { region, off: 0, count: 0 }
    }

    pub fn next(&mut self) -> Option<Entry<'a>> {
        if self.count >= ENTRY_MAX || self.off >= self.region.len() {
            return None;
        }
        let r = self.region;
        if self.off + 4 > r.len() {
            return None;
        }
        let plen = get_u32(r, self.off) as usize;
        self.off += 4;
        if plen == 0 || plen > PATH_MAX || self.off + plen > r.len() {
            return None;
        }
        let path = &r[self.off..self.off + plen];
        self.off += plen;
        if self.off + 4 > r.len() {
            return None;
        }
        let dlen = get_u32(r, self.off) as usize;
        self.off += 4;
        if self.off + dlen > r.len() {
            return None;
        }
        let data = &r[self.off..self.off + dlen];
        self.off += dlen;
        self.count += 1;
        Some(Entry { path, data })
    }
}

/// 路径白名单：相对路径，允许 `[A-Za-z0-9._/-]`，拒绝空、绝对路径、
/// `..` 段、`.` 段、双斜杠与 NUL。返回是否合法。
pub fn valid_path(p: &[u8]) -> bool {
    if p.is_empty() || p.len() > PATH_MAX || p[0] == b'/' {
        return false;
    }
    let mut seg_start = 0usize;
    for (i, &c) in p.iter().enumerate() {
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' || c == b'/';
        if !ok {
            return false;
        }
        if c == b'/' {
            let seg = &p[seg_start..i];
            if seg.is_empty() || seg == b"." || seg == b".." {
                return false;
            }
            seg_start = i + 1;
        }
    }
    let last = &p[seg_start..];
    !(last.is_empty() || last == b"." || last == b"..")
}

// ==================== 构建（主机侧工具用） ====================

/// 构造 .kunx 包写入 `out`，返回总长度；缓冲不足返回 0。
/// `entries` 为 (path, data)；`certs` 为 META-INF 证书链（leaf 在前）。
/// `sig` 为对 [0, sig_off) 的 Ed25519 签名（由调用方计算）。
pub fn build(
    manifest: &[u8],
    entries: &[(&str, &[u8])],
    certs: &[&[u8]],
    sig: &[u8; SIG_LEN],
    out: &mut [u8],
) -> usize {
    let mut payload_len = 0usize;
    for (p, d) in entries {
        if !valid_path(p.as_bytes()) {
            return 0;
        }
        payload_len = payload_len
            .checked_add(4 + p.len() + 4 + d.len())
            .unwrap_or(usize::MAX);
    }
    let mut meta_len = 0usize;
    for c in certs {
        meta_len = meta_len.checked_add(c.len()).unwrap_or(usize::MAX);
    }
    let manifest_off = HEADER_LEN;
    let payload_off = match manifest_off.checked_add(manifest.len()) {
        Some(v) => v,
        None => return 0,
    };
    let meta_off = match payload_off.checked_add(payload_len) {
        Some(v) => v,
        None => return 0,
    };
    let sig_off = match meta_off.checked_add(meta_len) {
        Some(v) => v,
        None => return 0,
    };
    let total = match sig_off.checked_add(SIG_LEN) {
        Some(v) => v,
        None => return 0,
    };
    if total > out.len() {
        return 0;
    }
    out[..8].copy_from_slice(&MAGIC);
    put_u32(out, 8, FORMAT_VERSION);
    put_u32(out, 12, FLAG_SIGNED);
    put_u32(out, 16, manifest_off as u32);
    put_u32(out, 20, manifest.len() as u32);
    put_u32(out, 24, payload_off as u32);
    put_u32(out, 28, payload_len as u32);
    put_u32(out, 32, meta_off as u32);
    put_u32(out, 36, meta_len as u32);
    put_u32(out, 40, sig_off as u32);
    put_u32(out, 44, SIG_LEN as u32);

    let mut off = manifest_off;
    out[off..off + manifest.len()].copy_from_slice(manifest);
    off += manifest.len();
    for (p, d) in entries {
        put_u32(out, off, p.len() as u32);
        off += 4;
        out[off..off + p.len()].copy_from_slice(p.as_bytes());
        off += p.len();
        put_u32(out, off, d.len() as u32);
        off += 4;
        out[off..off + d.len()].copy_from_slice(d);
        off += d.len();
    }
    for c in certs {
        out[off..off + c.len()].copy_from_slice(c);
        off += c.len();
    }
    out[off..off + SIG_LEN].copy_from_slice(sig);
    off + SIG_LEN
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
