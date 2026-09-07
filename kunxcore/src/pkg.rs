//! `.pkg` 应用包格式（Phase 8）：静态 ELF + manifest + 资源 + 签名。
//!
//! 二进制布局（小端）：
//! ```text
//! offset 0:  magic[8]       = "NOVAPKG1"
//! offset 8:  version        u32 = 1
//! offset 12: name_len       u32
//! offset 16: elf_len        u32
//! offset 20: manifest_len   u32
//! offset 24: res_count      u32
//! offset 28: reserved       u32 = 0
//! offset 32: name[name_len]
//!            elf[elf_len]
//!            manifest[manifest_len]
//!            每个资源: res_name_len u32, res_data_len u32, name[..], data[..]
//!            签名 signature[32]（覆盖 offset 32 到末尾 32 字节前的全部内容）
//! ```
//!
//! 安装：校验签名 → 把 ELF 写入 `/bin/<name>`、manifest 写入 `/pkg/<name>.mf`、
//! 资源写入 `/res/<name>/<resname>`。卸载：删除上述文件与资源目录内容。

use crate::vfs;

/// 包魔数。
pub const PKG_MAGIC: [u8; 8] = *b"NOVAPKG1";
/// 签名长度。
const SIG_LEN: usize = 32;
/// 包名最大长度。
const NAME_MAX: usize = 28;
/// 资源数量上限。
const MAX_RES: usize = 8;
/// 资源名单串长度上限。
const RES_NAME_MAX: usize = 64;
/// 文件系统单条目录名上限（对应 `fs_simple::MAX_NAME = 27`）。
///
/// 包内可以携带更长名字，但写盘时 `fs_create` 会拒绝超长名字，
/// 若等到写资源阶段才发现，会让 /bin、/pkg 文件半安装残留，这里保持一致。
const FS_NAME_MAX: usize = 27;

/// 校验包名/资源名：仅允许 `[A-Za-z0-9_-]`，拒绝路径分隔符、`.`、`..` 与 NUL，
/// 防止 `/bin/../evil` 之类的路径穿越与任意文件写。
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= NAME_MAX
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 校验资源名（允许更长，规则同 [`valid_name`]）。
fn valid_res_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= RES_NAME_MAX
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 一个资源（构建用）。
pub struct Res<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

/// 解析后的包视图（保存数据内的偏移，不含引用以规避生命周期）。
pub struct Pkg {
    pub name: [u8; NAME_MAX],
    pub name_len: usize,
    pub elf_off: usize,
    pub elf_len: usize,
    pub man_off: usize,
    pub man_len: usize,
    pub res_off: usize,
    pub res_count: usize,
}

// ==================== 字节工具 ====================

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

// ==================== 签名 ====================

fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// 计算 32 字节签名（4 段不同种子的 FNV-1a 64 位哈希）。
pub fn sign(data: &[u8]) -> [u8; SIG_LEN] {
    let mut out = [0u8; SIG_LEN];
    for i in 0..4 {
        let seed = 0xCBF2_9CE4_8422_2325u64.wrapping_add(i as u64 * 0x9E37_79B9_7F4A_7C15);
        let h = fnv1a(seed, data);
        out[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes());
    }
    out
}

// ==================== 构建 ====================

/// 构造一个包到 `out`，返回包总长度；缓冲区不足返回 0。
pub fn build(name: &str, elf: &[u8], manifest: &str, res: &[Res], out: &mut [u8]) -> usize {
    let name_len = name.len().min(NAME_MAX);
    if name_len == 0 {
        return 0;
    }
    let res_count = res.len().min(MAX_RES);
    let mut total = 32 + name_len + elf.len() + manifest.len();
    for r in &res[..res_count] {
        total += 8 + r.name.len() + r.data.len();
    }
    total += SIG_LEN;
    if total > out.len() {
        return 0;
    }

    out[0..8].copy_from_slice(&PKG_MAGIC);
    put_u32(out, 8, 1);
    put_u32(out, 12, name_len as u32);
    put_u32(out, 16, elf.len() as u32);
    put_u32(out, 20, manifest.len() as u32);
    put_u32(out, 24, res_count as u32);
    put_u32(out, 28, 0);

    let mut off = 32usize;
    out[off..off + name_len].copy_from_slice(&name.as_bytes()[..name_len]);
    off += name_len;
    out[off..off + elf.len()].copy_from_slice(elf);
    off += elf.len();
    out[off..off + manifest.len()].copy_from_slice(manifest.as_bytes());
    off += manifest.len();
    for r in &res[..res_count] {
        put_u32(out, off, r.name.len() as u32);
        off += 4;
        put_u32(out, off, r.data.len() as u32);
        off += 4;
        out[off..off + r.name.len()].copy_from_slice(r.name.as_bytes());
        off += r.name.len();
        out[off..off + r.data.len()].copy_from_slice(r.data);
        off += r.data.len();
    }

    let sig = sign(&out[32..off]);
    out[off..off + SIG_LEN].copy_from_slice(&sig);
    off + SIG_LEN
}

// ==================== 解析 / 校验 ====================

/// 解析包头部，返回 [`Pkg`]；非法返回 -1。
pub fn parse(data: &[u8]) -> Result<Pkg, i32> {
    if data.len() < 32 + SIG_LEN {
        return Err(-1);
    }
    if &data[0..8] != &PKG_MAGIC {
        return Err(-1);
    }
    let name_len = get_u32(data, 12) as usize;
    let elf_len = get_u32(data, 16) as usize;
    let man_len = get_u32(data, 20) as usize;
    let res_count = get_u32(data, 24) as usize;
    if name_len == 0 || name_len > NAME_MAX || res_count > MAX_RES {
        return Err(-1);
    }
    let mut off = 32usize;
    if off + name_len + elf_len + man_len + SIG_LEN > data.len() {
        return Err(-1);
    }
    let mut name = [0u8; NAME_MAX];
    name[..name_len].copy_from_slice(&data[off..off + name_len]);
    off += name_len;
    let elf_off = off;
    off += elf_len;
    let man_off = off;
    off += man_len;
    let res_off = off;

    // 校验包名（拒绝路径穿越字符）
    let name_str = core::str::from_utf8(&name[..name_len]).unwrap_or("");
    if !valid_name(name_str) {
        return Err(-1);
    }

    // 校验资源区边界：逐条遍历，确保 rnl/rdl 均落在签名前的数据范围内
    let sig_off = data.len() - SIG_LEN;
    let mut r = res_off;
    for _ in 0..res_count {
        if r + 8 > sig_off {
            return Err(-1);
        }
        let rnl = get_u32(data, r) as usize;
        let rdl = get_u32(data, r + 4) as usize;
        if rnl == 0 || rnl > RES_NAME_MAX {
            return Err(-1);
        }
        r += 8;
        if r + rnl > sig_off {
            return Err(-1);
        }
        let rn = core::str::from_utf8(&data[r..r + rnl]).unwrap_or("");
        if !valid_res_name(rn) {
            return Err(-1);
        }
        r += rnl;
        if r + rdl > sig_off {
            return Err(-1);
        }
        r += rdl;
    }

    Ok(Pkg { name, name_len, elf_off, elf_len, man_off, man_len, res_off, res_count })
}

/// 校验包签名（签名位于末尾 32 字节，覆盖其前全部数据）。
pub fn verify(data: &[u8], pkg: &Pkg) -> bool {
    if data.len() < 32 + SIG_LEN {
        return false;
    }
    let sig_off = data.len() - SIG_LEN;
    let expected = &data[sig_off..sig_off + SIG_LEN];
    let computed = sign(&data[32..sig_off]);
    let _ = pkg;
    expected == &computed[..]
}

// ==================== 文件写入辅助 ====================

fn write_file(path: &str, data: &[u8]) -> i32 {
    let fd = vfs::open(path, vfs::O_WRONLY | vfs::O_CREAT | vfs::O_TRUNC);
    if fd < 0 {
        return -1;
    }
    let r = vfs::write(fd, data);
    vfs::close(fd);
    if r < 0 || (r as usize) < data.len() {
        -1
    } else {
        0
    }
}

/// 把 `/bin/name`、`/pkg/name.mf` 等路径拼入 `out`，返回末尾下标。
fn join(out: &mut [u8], prefix: &str, name: &str, suffix: &str) -> usize {
    let mut off = 0usize;
    for &c in prefix.as_bytes() {
        if off < out.len() {
            out[off] = c;
            off += 1;
        }
    }
    for &c in name.as_bytes() {
        if off < out.len() {
            out[off] = c;
            off += 1;
        }
    }
    for &c in suffix.as_bytes() {
        if off < out.len() {
            out[off] = c;
            off += 1;
        }
    }
    if off < out.len() {
        out[off] = 0;
    }
    off
}

fn path_str(out: &[u8]) -> &str {
    let len = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    core::str::from_utf8(&out[..len]).unwrap_or("")
}

// ==================== 安装 / 卸载 / 列表 ====================

/// 安装包：校验签名后写入 ELF、manifest 与资源。
pub fn install(pkg: &Pkg, data: &[u8]) -> i32 {
    if !verify(data, pkg) {
        return -1; // 签名校验失败
    }
    let name = core::str::from_utf8(&pkg.name[..pkg.name_len]).unwrap_or("");
    if name.is_empty() {
        return -1;
    }

    // 先整体校验资源区：名字超文件系统上限（MAX_NAME=27）或非法时直接拒绝，
    // 避免「先写 /bin、/pkg 再到写资源阶段才失败」造成半安装包残留。
    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        if off + 8 > data.len() {
            return -1;
        }
        let rnl = get_u32(data, off) as usize;
        off += 4;
        let rdl = get_u32(data, off) as usize;
        off += 4;
        if off + rnl + rdl > data.len() || rnl == 0 {
            return -1;
        }
        let rname = &data[off..off + rnl];
        if rnl > FS_NAME_MAX {
            return -1; // 资源名超 FS 上限：文件系统会拒绝创建目录项
        }
        let rname_str = core::str::from_utf8(rname).unwrap_or("");
        if !valid_res_name(rname_str) {
            return -1;
        }
        off += rnl + rdl;
    }

    vfs::mkdir("/bin");
    vfs::mkdir("/pkg");
    vfs::mkdir("/res");

    let mut path = [0u8; 96];

    // ELF → /bin/<name>
    join(&mut path, "/bin/", name, "");
    if write_file(path_str(&path), &data[pkg.elf_off..pkg.elf_off + pkg.elf_len]) != 0 {
        return -1;
    }

    // manifest → /pkg/<name>.mf
    join(&mut path, "/pkg/", name, ".mf");
    if write_file(path_str(&path), &data[pkg.man_off..pkg.man_off + pkg.man_len]) != 0 {
        // 回滚已写的 /bin/<name>
        join(&mut path, "/bin/", name, "");
        vfs::unlink(path_str(&path));
        return -1;
    }

    // 资源 → /res/<name>/<resname>
    // 路径最长："/res/" + 28（包名）+ "/" + 27（资源名）= 62 字节（含 NUL）
    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        if off + 8 > data.len() {
            break;
        }
        let rnl = get_u32(data, off) as usize;
        off += 4;
        let rdl = get_u32(data, off) as usize;
        off += 4;
        if off + rnl + rdl > data.len() {
            break;
        }
        let rname = &data[off..off + rnl];
        off += rnl;
        let rdata = &data[off..off + rdl];
        off += rdl;

        let rname_str = core::str::from_utf8(rname).unwrap_or("");
        if rnl == 0 || rnl > FS_NAME_MAX || !valid_res_name(rname_str) {
            break; // 防御：预检已保证合法，此处不写
        }
        // /res/<name>
        join(&mut path, "/res/", name, "");
        vfs::mkdir(path_str(&path));
        // /res/<name>/<resname>
        join(&mut path, "/res/", name, "/");
        let base = path_str(&path);
        let mut full = [0u8; 128];
        join(&mut full, base, rname_str, "");
        if write_file(path_str(&full), rdata) != 0 {
            // 回滚：删除已写的 /bin、/pkg 文件与本包已写入的 /res/<name>/
            // 资源（复用 uninstall 的枚举删除；此前残留的半包资源会在
            // 重装同名包时与新文件混装）
            join(&mut path, "/bin/", name, "");
            vfs::unlink(path_str(&path));
            join(&mut path, "/pkg/", name, ".mf");
            vfs::unlink(path_str(&path));
            let mut resdir = [0u8; 96];
            join(&mut resdir, "/res/", name, "");
            let dir = path_str(&resdir);
            let mut entry = [0u8; 32];
            let mut idx = 0usize;
            let mut fails = 0usize;
            loop {
                let ino = vfs::readdir(dir, idx, &mut entry);
                if ino < 0 {
                    break;
                }
                let elen = entry.iter().position(|&c| c == 0).unwrap_or(entry.len());
                let ename = core::str::from_utf8(&entry[..elen]).unwrap_or("");
                if ename == "." || ename == ".." {
                    idx += 1;
                    continue;
                }
                let mut full2 = [0u8; 128];
                join(&mut full2, dir, "/", "");
                let base2 = path_str(&full2);
                let mut full3 = [0u8; 128];
                join(&mut full3, base2, ename, "");
                let del = vfs::unlink(path_str(&full3));
                // 删除成功不递增 idx（条目前移）；失败递增跳过，防死循环
                if del != 0 {
                    idx += 1;
                    fails += 1;
                    if fails > 64 {
                        break;
                    }
                }
            }
            return -1;
        }
    }

    0
}

/// 卸载包：删除 `/bin/<name>`、`/pkg/<name>.mf` 及 `/res/<name>/` 下内容。
pub fn uninstall(name: &str) -> i32 {
    // 包名直接拼进绝对路径，不做白名单校验时 `pkg uninstall ../../etc/hosts`
    // 会经真实的 `..` 目录项解析删除任意文件 —— 与 install 同等对待。
    if !valid_name(name) {
        return -1;
    }
    let mut path = [0u8; 96];

    join(&mut path, "/bin/", name, "");
    vfs::unlink(path_str(&path));

    join(&mut path, "/pkg/", name, ".mf");
    vfs::unlink(path_str(&path));

    // 枚举 /res/<name>/ 并删除每个条目
    join(&mut path, "/res/", name, "");
    let dir = path_str(&path);
    let mut entry = [0u8; 32];
    let mut idx = 0usize;
    let mut fails = 0usize;
    loop {
        let ino = vfs::readdir(dir, idx, &mut entry);
        if ino < 0 {
            break;
        }
        let elen = entry.iter().position(|&c| c == 0).unwrap_or(entry.len());
        let ename = core::str::from_utf8(&entry[..elen]).unwrap_or("");
        if ename == "." || ename == ".." {
            idx += 1;
            continue;
        }
        let mut full = [0u8; 96];
        join(&mut full, dir, "/", "");
        let base = path_str(&full);
        let mut full2 = [0u8; 96];
        join(&mut full2, base, ename, "");
        let del = vfs::unlink(path_str(&full2));
        // 删除成功后条目序号整体前移：不递增 idx，同一位置重读，避免跳过一半
        // 条目。★ 删除失败（文件仍打开/子目录/介质错误）必须递增 idx 跳过并
        // 计数，否则同一失败条目无限重读 → 内核任务死循环挂起。
        if del != 0 {
            idx += 1;
            fails += 1;
            if fails > 64 {
                break;
            }
        }
    }

    0
}

/// 列出已安装的包（读取 `/pkg` 目录）。
pub fn list() {
    // 直接输出到 UART（无 sink 的调用方）。
    list_to(&mut |s| crate::uart_send_str(s));
}

/// 列出已安装包，逐行交给 `emit` 回调（供 shell 走管道 Sink，保证
/// `pkg list | <下游>` 能收到内容；此前直接 uart 输出绕过管道）。
pub fn list_to(emit: &mut dyn FnMut(&str)) {
    emit("[pkg] installed packages:\n");
    let mut entry = [0u8; 32];
    let mut idx = 0usize;
    loop {
        let ino = vfs::readdir("/pkg", idx, &mut entry);
        if ino < 0 {
            break;
        }
        let elen = entry.iter().position(|&c| c == 0).unwrap_or(entry.len());
        let ename = core::str::from_utf8(&entry[..elen]).unwrap_or("");
        if ename != "." && ename != ".." {
            emit("  - ");
            emit(ename);
            emit("\n");
        }
        idx += 1;
    }
}
