//! 商店索引解析（`Packages.json`，JSON/文本，复用 kunxpkg 的极简 JSON 解析器）。
//!
//! 索引格式（R2 桶根的 `Packages.json`）：
//! ```json
//! {
//!   "revision": 4,
//!   "updated": 1755300000,
//!   "packages": {
//!     "notes": {
//!       "version": "1.3.0",
//!       "description": "轻量笔记",
//!       "author": "KunxOS Team",
//!       "category": "工具",
//!       "filename": "packages/notes-1.3.0.kunx",
//!       "size": 24576,
//!       "min_api": 1,
//!       "permissions": ["STORAGE"]
//!     },
//!     "weather": { "version": "1.2.0", "category": "工具", "filename": "packages/weather-1.2.0.kunx", "size": 28672 }
//!   }
//! }
//! ```
//! 包名取对象键（`name` 字段不单独存在），版本/描述等取条目内字段。
//! `index.rs` 只做「解析 + 只读视图」，静态存储；缓存/过期/网络由 main 负责。

pub const MAX_PKGS: usize = 32;

/// 单个商店条目（静态缓冲，字段截断存储）。
#[derive(Clone, Copy)]
pub struct StorePkg {
    pub name: [u8; 32],
    pub name_len: usize,
    pub version: [u8; 16],
    pub version_len: usize,
    pub desc: [u8; 96],
    pub desc_len: usize,
    pub author: [u8; 48],
    pub author_len: usize,
    pub category: [u8; 16],
    pub category_len: usize,
    pub filename: [u8; 96],
    pub filename_len: usize,
    pub size: u32,
    pub min_api: u32,
    pub perms: [[u8; 16]; 6],
    pub perm_lens: [usize; 6],
    pub perm_count: usize,
}

impl StorePkg {
    pub const fn new() -> StorePkg {
        StorePkg {
            name: [0; 32],
            name_len: 0,
            version: [0; 16],
            version_len: 0,
            desc: [0; 96],
            desc_len: 0,
            author: [0; 48],
            author_len: 0,
            category: [0; 16],
            category_len: 0,
            filename: [0; 96],
            filename_len: 0,
            size: 0,
            min_api: 0,
            perms: [[0; 16]; 6],
            perm_lens: [0; 6],
            perm_count: 0,
        }
    }
}

static mut IDX_PKGS: [StorePkg; MAX_PKGS] = [StorePkg::new(); MAX_PKGS];
static mut IDX_COUNT: usize = 0;
static mut IDX_REVISION: u32 = 0;
static mut IDX_UPDATED: u32 = 0;

/// 清空索引。
pub fn reset() {
    unsafe {
        IDX_COUNT = 0;
        IDX_REVISION = 0;
        IDX_UPDATED = 0;
    }
}

pub fn count() -> usize {
    unsafe { IDX_COUNT }
}

pub fn revision() -> u32 {
    unsafe { IDX_REVISION }
}

pub fn updated() -> u32 {
    unsafe { IDX_UPDATED }
}

pub fn pkg(i: usize) -> &'static StorePkg {
    unsafe { &IDX_PKGS[i] }
}

fn fill(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

/// 解析完整 Packages.json 到静态索引；成功（至少解析出顶层包表且含 1 个条目）
/// 返回 true。数据非法时部分解析仍可展示（宽容策略，便于离线查看）。
pub fn parse(data: &[u8]) -> bool {
    let Some(root) = crate::json::Json::parse(data) else {
        return false;
    };
    unsafe {
        IDX_COUNT = 0;
        IDX_REVISION = 0;
        IDX_UPDATED = 0;
        if let Some(r) = root.get("revision").and_then(|v| v.as_u32()) {
            IDX_REVISION = r;
        }
        if let Some(u) = root.get("updated").and_then(|v| v.as_u32()) {
            IDX_UPDATED = u;
        }
        let Some(mut it) = root.get("packages").and_then(|v| v.obj()) else {
            return false;
        };
        while IDX_COUNT < MAX_PKGS {
            match it.next() {
                Some((name, val)) => {
                    if parse_pkg(&name, &val, &mut IDX_PKGS[IDX_COUNT]) {
                        IDX_COUNT += 1;
                    }
                }
                None => break,
            }
        }
    }
    unsafe { IDX_COUNT > 0 }
}

fn parse_pkg(name: &[u8], v: &crate::json::Json<'_>, p: &mut StorePkg) -> bool {
    if name.is_empty() || name.len() > p.name.len() {
        return false;
    }
    let Some(version) = v.get("version").and_then(|x| x.as_str()) else {
        return false;
    };
    if version.is_empty() || version.len() > p.version.len() {
        return false;
    }
    p.name_len = fill(&mut p.name, name);
    p.version_len = fill(&mut p.version, version);
    p.desc_len = fill(&mut p.desc, v.get("description").and_then(|x| x.as_str()).unwrap_or(&[]));
    p.author_len = fill(&mut p.author, v.get("author").and_then(|x| x.as_str()).unwrap_or(&[]));
    p.category_len = fill(&mut p.category, v.get("category").and_then(|x| x.as_str()).unwrap_or(&[]));
    p.filename_len = fill(&mut p.filename, v.get("filename").and_then(|x| x.as_str()).unwrap_or(&[]));
    p.size = v.get("size").and_then(|x| x.as_u32()).unwrap_or(0);
    p.min_api = v.get("min_api").and_then(|x| x.as_u32()).unwrap_or(0);
    p.perm_count = 0;
    if let Some(perms) = v.get("permissions") {
        if let Some(mut pit) = perms.arr() {
            while p.perm_count < p.perms.len() {
                match pit.next_str() {
                    Some(s) => {
                        if p.perm_count < 6 {
                            p.perm_lens[p.perm_count] = fill(&mut p.perms[p.perm_count], s);
                            p.perm_count += 1;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    true
}

/// 索引中的全部分类（保持出现顺序，去重），写入 `out`/`lens`，返回数量。
pub fn categories(out: &mut [[u8; 16]; 8], lens: &mut [usize; 8]) -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..IDX_COUNT {
            let p = &IDX_PKGS[i];
            if p.category_len == 0 {
                continue;
            }
            let mut dup = false;
            for j in 0..n {
                if lens[j] == p.category_len
                    && &out[j][..lens[j]] == &p.category[..p.category_len]
                {
                    dup = true;
                    break;
                }
            }
            if !dup && n < 8 {
                out[n] = p.category;
                lens[n] = p.category_len;
                n += 1;
            }
        }
    }
    n
}

/// 按分类过滤，写入索引数组；返回数量（0 = 全部）。
pub fn filter_category(cat: &[u8], out: &mut [usize], max: usize) -> usize {
    let mut n = 0usize;
    if cat.is_empty() {
        return filter_all(out, max);
    }
    unsafe {
        for i in 0..IDX_COUNT {
            let p = &IDX_PKGS[i];
            if p.category_len == cat.len() && &p.category[..cat.len()] == cat {
                if n < max {
                    out[n] = i;
                    n += 1;
                }
            }
        }
    }
    n
}

pub fn filter_all(out: &mut [usize], max: usize) -> usize {
    let mut n = 0usize;
    unsafe {
        for i in 0..IDX_COUNT {
            if n < max {
                out[n] = i;
                n += 1;
            }
        }
    }
    n
}

/// ASCII 不区分大小写的子串匹配（name / description / author / category）。
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.len() < needle.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        let mut ok = true;
        for j in 0..needle.len() {
            if hay[i + j].to_ascii_lowercase() != needle[j].to_ascii_lowercase() {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
        i += 1;
    }
    false
}

/// 追加一个已填好的条目（本地目录回退用）。
pub fn push_local(p: StorePkg) {
    unsafe {
        if IDX_COUNT < MAX_PKGS {
            IDX_PKGS[IDX_COUNT] = p;
            IDX_COUNT += 1;
        }
    }
}

/// 搜索：query 为空 = 全部；否则匹配 name/desc/author/category。返回命中数。
pub fn search(query: &[u8], out: &mut [usize], max: usize) -> usize {
    let mut n = 0usize;
    if query.is_empty() {
        return filter_all(out, max);
    }
    unsafe {
        for i in 0..IDX_COUNT {
            let p = &IDX_PKGS[i];
            if contains_ci(&p.name[..p.name_len], query)
                || contains_ci(&p.desc[..p.desc_len], query)
                || contains_ci(&p.author[..p.author_len], query)
                || contains_ci(&p.category[..p.category_len], query)
            {
                if n < max {
                    out[n] = i;
                    n += 1;
                }
            }
        }
    }
    n
}