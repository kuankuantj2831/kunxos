// KunxOS 包管理器 CLI（kunxpkg）：no_std 裸 syscall 用户态程序，静态分配、零依赖。
//
// 复刻 kunxcore/src/pkg.rs 的 `.pkg` 格式（NOVAPKG1 魔数 + FNV-1a 签名），
// 但所有文件操作改为经 Linux 兼容层系统调用完成：
//   openat / read / write / close / lseek / mkdirat / unlinkat / getdents64
//
// 数据目录（/var/lib/pkg/）：
//   sources.list  软件源（file:// 本地源）
//   available     合并后的可用包索引（Packages 格式）
//   cache/        已下载的 .pkg
//   installed/    已安装记录（name= / version= / files=）
//   repo/main/    本地软件源（Packages 索引 + .pkg 文件）
//
// 命令：pkg update | install <name> | remove <name> | list | search <kw> | info <name>

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_GETDENTS64: usize = 61;
const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;

// ==================== Linux 常量 ====================

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C; // -100
const O_RDONLY: usize = 0;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;

// ==================== 路径常量 ====================

const SOURCES: &[u8] = b"/var/lib/pkg/sources.list";
const AVAILABLE: &[u8] = b"/var/lib/pkg/available";
const INSTALLED_DIR: &[u8] = b"/var/lib/pkg/installed";
const CACHE_DIR: &[u8] = b"/var/lib/pkg/cache";
const PKG_DIR: &[u8] = b"/var/lib/pkg";

// ==================== .pkg 格式（与 kunxcore/src/pkg.rs 一致） ====================

const PKG_MAGIC: [u8; 8] = *b"NOVAPKG1";
const SIG_LEN: usize = 32;
const NAME_MAX: usize = 28;

/// 解析后的包视图（保存数据内的偏移，不含引用以规避生命周期）。
struct Pkg {
    name: [u8; NAME_MAX],
    name_len: usize,
    elf_off: usize,
    elf_len: usize,
    man_off: usize,
    man_len: usize,
    res_off: usize,
    res_count: usize,
}

// ==================== 全局缓冲（静态分配） ====================

/// .pkg 文件缓冲（文件系统单文件上限 96KB）。
static mut PKG_BUF: [u8; 96 * 1024] = [0; 96 * 1024];
/// 合并后的可用包索引。
static mut AVAIL_BUF: [u8; 16384] = [0; 16384];
/// 单个 Packages 索引临时缓冲。
static mut IDX_BUF: [u8; 8192] = [0; 8192];
/// sources.list 内容。
static mut SRC_BUF: [u8; 512] = [0; 512];
/// 路径临时缓冲。
static mut PATH_BUF: [u8; 128] = [0; 128];
/// getdents64 缓冲。
static mut DIR_BUF: [u8; 4096] = [0; 4096];

// ==================== 系统调用封装 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "svc #0",
        in("x8") n,
        inlateout("x0") a1 => ret,
        in("x1") a2,
        in("x2") a3,
        options(nostack)
    );
    ret
}

// ==================== 基础 I/O ====================

unsafe fn write_fd(fd: usize, buf: &[u8]) -> usize {
    syscall3(SYS_WRITE, fd, buf.as_ptr() as usize, buf.len())
}

fn print(s: &str) {
    unsafe {
        write_fd(1, s.as_bytes());
    }
}

fn print_bytes(s: &[u8]) {
    unsafe {
        write_fd(1, s);
    }
}

fn print_dec(mut v: usize) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    unsafe {
        write_fd(1, &buf[i..]);
    }
}

// ==================== 路径工具 ====================

/// 把 `a` 与 `b` 拼接进 `out` 并以 NUL 结尾，返回长度（不含 NUL）。
fn path_join(out: &mut [u8], a: &[u8], b: &[u8]) -> usize {
    let mut n = 0usize;
    for &c in a {
        if n < out.len() - 1 {
            out[n] = c;
            n += 1;
        }
    }
    for &c in b {
        if n < out.len() - 1 {
            out[n] = c;
            n += 1;
        }
    }
    out[n] = 0;
    n
}

/// 把三个片段 `a`+`b`+`c` 拼入 `out`，NUL 结尾，返回长度（不含 NUL）。
fn path_join3(out: &mut [u8], a: &[u8], b: &[u8], c: &[u8]) -> usize {
    let n = path_join(out, a, b);
    let mut m = n;
    for &ch in c {
        if m < out.len() - 1 {
            out[m] = ch;
            m += 1;
        }
    }
    out[m] = 0;
    m
}

// ==================== 文件操作 ====================

fn open_path(path: &[u8], flags: usize) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
    }
}

fn read_path(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0usize;
    loop {
        let space = buf.len() - total;
        if space == 0 {
            break;
        }
        let chunk = space.min(4096);
        let n = unsafe {
            syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize + total, chunk) as isize
        };
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    total
}

fn write_path(path: &[u8], data: &[u8]) -> isize {
    let fd = open_path(path, O_WRONLY | O_CREAT | O_TRUNC);
    if fd < 0 {
        return -1;
    }
    let mut off = 0usize;
    let mut ok = true;
    while off < data.len() {
        let chunk = (data.len() - off).min(4096);
        let n = unsafe {
            syscall3(SYS_WRITE, fd as usize, data.as_ptr() as usize + off, chunk) as isize
        };
        if n <= 0 {
            ok = false;
            break;
        }
        off += n as usize;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    if ok {
        0
    } else {
        -1
    }
}

fn mkdir_path(path: &[u8]) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
    }
}

fn unlink_path(path: &[u8]) -> isize {
    unsafe {
        let n = path.len().min(PATH_BUF.len() - 1);
        PATH_BUF[..n].copy_from_slice(&path[..n]);
        PATH_BUF[n] = 0;
        syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, 0) as isize
    }
}

fn exists_path(path: &[u8]) -> bool {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return false;
    }
    unsafe {
        syscall1(SYS_CLOSE, fd as usize);
    }
    true
}

/// 列出目录条目：把名字（NUL 分隔）写入 `out`，返回写入字节数；失败 0。
fn list_dir(dir: &[u8], out: &mut [u8]) -> usize {
    let fd = open_path(dir, O_RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut total = 0usize;
    unsafe {
        let n = syscall3(SYS_GETDENTS64, fd as usize, DIR_BUF.as_ptr() as usize, DIR_BUF.len())
            as isize;
        if n > 0 {
            let mut off = 0usize;
            while off + 19 <= n as usize {
                let reclen =
                    u16::from_le_bytes([DIR_BUF[off + 16], DIR_BUF[off + 17]]) as usize;
                if reclen == 0 || off + reclen > n as usize {
                    break;
                }
                let name_start = off + 19;
                // 名字扫描上界 = 条目末尾（reclen 可能 < 19+名字长，防越界读）
                let name_end = (off + reclen).min(n as usize);
                let mut len = 0usize;
                while len < 31 && name_start + len < name_end && DIR_BUF[name_start + len] != 0 {
                    len += 1;
                }
                let dot = len == 1 && DIR_BUF[name_start] == b'.';
                let dotdot = len == 2
                    && DIR_BUF[name_start] == b'.'
                    && DIR_BUF[name_start + 1] == b'.';
                if !dot && !dotdot {
                    let mut i = 0usize;
                    while i < len && total < out.len() {
                        out[total] = DIR_BUF[name_start + i];
                        total += 1;
                        i += 1;
                    }
                    if total < out.len() {
                        out[total] = 0;
                        total += 1;
                    }
                }
                off += reclen;
            }
        }
        syscall1(SYS_CLOSE, fd as usize);
    }
    total
}

// ==================== argv ====================

fn arg(argv: *const usize, i: usize) -> &'static str {
    unsafe {
        let p = *argv.add(i);
        if p == 0 {
            return "";
        }
        let mut len = 0usize;
        // 先判边界再解引用，避免 len==512 时读取 p[512] 越界
        while len < 512 && *((p as *const u8).add(len)) != 0 {
            len += 1;
        }
        // 安全 UTF-8 校验（此前 from_utf8_unchecked：任意字节输入为 UB）
        core::str::from_utf8(core::slice::from_raw_parts(p as *const u8, len)).unwrap_or("")
    }
}

// ==================== .pkg 字节工具 / 签名 ====================

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn sign(data: &[u8]) -> [u8; SIG_LEN] {
    let mut out = [0u8; SIG_LEN];
    for i in 0..4 {
        let seed = 0xCBF2_9CE4_8422_2325u64.wrapping_add(i as u64 * 0x9E37_79B9_7F4A_7C15);
        let h = fnv1a(seed, data);
        out[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes());
    }
    out
}

fn parse(data: &[u8]) -> Result<Pkg, i32> {
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
    if name_len == 0 || name_len > NAME_MAX || res_count > 8 {
        return Err(-1);
    }
    // 数据区（不含末尾签名）
    let payload_end = data.len() - SIG_LEN;
    let mut off = 32usize;
    if off + name_len + elf_len + man_len > payload_end {
        return Err(-1);
    }
    let mut name = [0u8; NAME_MAX];
    name[..name_len].copy_from_slice(&data[off..off + name_len]);
    // 包名白名单：禁止 '/' 与 ".."（路径穿越），与内核 pkg.rs 的 valid_name 一致
    if !valid_name(&name[..name_len]) {
        return Err(-1);
    }
    off += name_len;
    let elf_off = off;
    off += elf_len;
    let man_off = off;
    off += man_len;
    let res_off = off;

    // 资源区逐条边界校验：每条 = rnl(u32) + rdl(u32) + rname[rnl] + rdata[rdl]，
    // 必须完整落在签名之前（防伪造包切片越界 panic）
    let mut ro = res_off;
    for _ in 0..res_count {
        if ro + 8 > payload_end {
            return Err(-1);
        }
        let rnl = get_u32(data, ro) as usize;
        let rdl = get_u32(data, ro + 4) as usize;
        ro += 8;
        if ro + rnl + rdl > payload_end {
            return Err(-1);
        }
        if rnl == 0 || rnl > 64 {
            return Err(-1);
        }
        // 资源名白名单（禁止 '/' 与 ".."）
        if !valid_name(&data[ro..ro + rnl]) {
            return Err(-1);
        }
        ro += rnl + rdl;
    }

    Ok(Pkg { name, name_len, elf_off, elf_len, man_off, man_len, res_off, res_count })
}

/// 包名/资源名白名单：仅允许字母、数字、`_`、`-`、`.`，长度 1..=28；
/// 禁止 `/` 与 `..`/`.`（防止安装路径穿越覆盖 /bin、/init 等）。
fn valid_name(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > NAME_MAX {
        return false;
    }
    for &c in b {
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.';
        if !ok {
            return false;
        }
    }
    if b == b"..".as_slice() || b == b".".as_slice() {
        return false;
    }
    true
}

fn verify(data: &[u8], pkg: &Pkg) -> bool {
    if data.len() < 32 + SIG_LEN {
        return false;
    }
    let sig_off = data.len() - SIG_LEN;
    let expected = &data[sig_off..sig_off + SIG_LEN];
    let computed = sign(&data[32..sig_off]);
    let _ = pkg;
    expected == &computed[..]
}

// ==================== 安装 / 卸载（经系统调用） ====================

/// 安装包：写入 ELF 到 /bin/<name>、manifest 到 /pkg/<name>.mf、资源到 /res/<name>/。
fn install(pkg: &Pkg, data: &[u8]) -> isize {
    let name = core::str::from_utf8(&pkg.name[..pkg.name_len]).unwrap_or("");
    // 防御性校验：parse 已保证，这里再查一次防旁路调用
    if !valid_name(name.as_bytes()) {
        return -1;
    }
    // 防御性边界：ELF/manifest 切片必须在数据区内（parse 已校验，双保险）
    if pkg.elf_off + pkg.elf_len > data.len()
        || pkg.man_off + pkg.man_len > data.len()
        || pkg.res_off > data.len()
    {
        return -1;
    }

    mkdir_path(b"/bin");
    mkdir_path(b"/pkg");
    mkdir_path(b"/res");

    // ELF -> /bin/<name>
    let mut path = [0u8; 128];
    let n = path_join(&mut path, b"/bin/", name.as_bytes());
    if write_path(&path[..n], &data[pkg.elf_off..pkg.elf_off + pkg.elf_len]) != 0 {
        return -1;
    }

    // manifest -> /pkg/<name>.mf
    let n = path_join3(&mut path, b"/pkg/", name.as_bytes(), b".mf");
    if write_path(&path[..n], &data[pkg.man_off..pkg.man_off + pkg.man_len]) != 0 {
        return -1;
    }

    // 资源 -> /res/<name>/<resname>
    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        let rnl = get_u32(data, off) as usize;
        off += 4;
        let rdl = get_u32(data, off) as usize;
        off += 4;
        let rname = &data[off..off + rnl];
        off += rnl;
        let rdata = &data[off..off + rdl];
        off += rdl;

        // /res/<name>
        let dn = path_join(&mut path, b"/res/", name.as_bytes());
        mkdir_path(&path[..dn]);

        // /res/<name>/<resname>
        let fn0 = path_join3(&mut path, b"/res/", name.as_bytes(), b"/");
        let mut full = [0u8; 128];
        let flen = path_join(&mut full, &path[..fn0], rname);
        write_path(&full[..flen], rdata);
    }

    0
}

/// 卸载包：删除 /bin/<name>、/pkg/<name>.mf 及 /res/<name>/ 下内容。
fn uninstall(name: &str) {
    if name.is_empty() {
        return;
    }
    let nb = name.as_bytes();
    let mut path = [0u8; 128];

    let n = path_join(&mut path, b"/bin/", nb);
    unlink_path(&path[..n]);

    let n = path_join3(&mut path, b"/pkg/", nb, b".mf");
    unlink_path(&path[..n]);

    // 枚举 /res/<name>/ 并删除每个条目
    let dn = path_join(&mut path, b"/res/", nb);
    let mut names = [0u8; 512];
    let total = list_dir(&path[..dn], &mut names);
    let mut i = 0usize;
    while i < total {
        let mut j = i;
        while j < total && names[j] != 0 {
            j += 1;
        }
        if j > i {
            let mut full = [0u8; 128];
            let flen = path_join3(&mut full, &path[..dn], b"/", &names[i..j]);
            unlink_path(&full[..flen]);
        }
        i = j + 1;
    }
}

// ==================== 索引 / 记录解析 ====================

/// 返回 `idx` 中从 `from` 起下一行结束位置（越过 '\n'；无 '\n' 时为 len）。
fn next_line(idx: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < idx.len() && idx[i] != b'\n' {
        i += 1;
    }
    if i < idx.len() {
        i + 1
    } else {
        i
    }
}

/// 取一行内容（去掉行尾 '\n' 与 '\r'）。
fn line_str(idx: &[u8], start: usize, end: usize) -> &[u8] {
    let mut e = end;
    if e > start && idx[e - 1] == b'\n' {
        e -= 1;
    }
    if e > start && idx[e - 1] == b'\r' {
        e -= 1;
    }
    &idx[start..e]
}

/// 去掉首尾空白。
fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r') {
        e -= 1;
    }
    &b[s..e]
}

/// 在索引中定位名为 `name` 的包记录，返回 (start, end)。
fn find_record(idx: &[u8], name: &str) -> Option<(usize, usize)> {
    let nb = name.as_bytes();
    let mut ls = 0usize;
    while ls < idx.len() {
        let le = next_line(idx, ls);
        let line = line_str(idx, ls, le);
        if line.len() >= 8 && &line[..8] == b"Package:" {
            if trim(&line[8..]) == nb {
                // 记录结束：下一个空行 / 下一个 Package: / EOF
                let mut rs = le;
                let mut end = le;
                while rs < idx.len() {
                    let re = next_line(idx, rs);
                    let rl = line_str(idx, rs, re);
                    if rl.is_empty() || (rl.len() >= 8 && &rl[..8] == b"Package:") {
                        end = rs;
                        break;
                    }
                    end = re;
                    rs = re;
                }
                return Some((ls, end));
            }
        }
        ls = le;
    }
    None
}

/// 拷贝记录 [start,end) 内 `key` 字段的值到 `out`，返回长度；无此字段返回 0。
fn field_copy(idx: &[u8], start: usize, end: usize, key: &str, out: &mut [u8]) -> usize {
    let kb = key.as_bytes();
    let mut ls = start;
    while ls < end {
        let mut le = next_line(idx, ls);
        if le > end {
            le = end;
        }
        let line = line_str(idx, ls, le);
        if line.len() > kb.len() && &line[..kb.len()] == kb && line[kb.len()] == b':' {
            let val = trim(&line[kb.len() + 1..]);
            let c = val.len().min(out.len());
            out[..c].copy_from_slice(&val[..c]);
            return c;
        }
        ls = le;
    }
    0
}

/// 统计索引中的包记录数（按 Package: 行计数）。
fn count_packages(buf: &[u8]) -> usize {
    let mut n = 0usize;
    let mut ls = 0usize;
    while ls < buf.len() {
        let le = next_line(buf, ls);
        let line = line_str(buf, ls, le);
        if line.len() >= 8 && &line[..8] == b"Package:" {
            n += 1;
        }
        ls = le;
    }
    n
}

// ==================== 已安装数据库 ====================

fn installed_path(out: &mut [u8], name: &str) -> usize {
    path_join(out, INSTALLED_DIR, name.as_bytes())
}

fn is_installed(name: &str) -> bool {
    let mut p = [0u8; 128];
    let n = installed_path(&mut p, name);
    exists_path(&p[..n])
}

/// 读取已安装记录的 version 字段到 `out`，返回长度。
fn installed_version(name: &str, out: &mut [u8]) -> usize {
    let mut p = [0u8; 128];
    let n = installed_path(&mut p, name);
    let mut rec = [0u8; 512];
    let rlen = read_path(&p[..n], &mut rec);
    // 记录格式：name=..\nversion=..\nfiles=..
    let mut ls = 0usize;
    while ls < rlen {
        let le = next_line(&rec, ls);
        let line = line_str(&rec, ls, le);
        if line.len() >= 8 && &line[..8] == b"version=" {
            let v = &line[8..];
            let c = v.len().min(out.len());
            out[..c].copy_from_slice(&v[..c]);
            return c;
        }
        ls = le;
    }
    0
}

// ==================== 软件源 ====================

/// 从 sources.list 读取第一个本地源，构造 `<源目录>/<fname>`，返回路径长度。
unsafe fn resolve_repo_file(fname: &[u8], out: &mut [u8]) -> usize {
    let sn = read_path(SOURCES, &mut SRC_BUF);
    let mut ls = 0usize;
    while ls < sn {
        let le = next_line(&SRC_BUF, ls);
        let line = line_str(&SRC_BUF, ls, le);
        let s = core::str::from_utf8(line).unwrap_or("");
        if let Some(rest) = s.strip_prefix("file://") {
            return path_join3(out, rest.as_bytes(), b"/", fname);
        }
        ls = le;
    }
    0
}

/// 读取可用索引到 AVAIL_BUF，返回长度。
unsafe fn load_available() -> usize {
    read_path(AVAILABLE, &mut AVAIL_BUF)
}

// ==================== 命令实现 ====================

unsafe fn cmd_update() {
    let sn = read_path(SOURCES, &mut SRC_BUF);
    if sn == 0 {
        print("[pkg] cannot read sources.list\n");
        return;
    }
    mkdir_path(PKG_DIR);

    let mut total = 0usize;
    let mut count = 0usize;
    let mut ls = 0usize;
    while ls < sn {
        let le = next_line(&SRC_BUF, ls);
        let line = line_str(&SRC_BUF, ls, le);
        let s = core::str::from_utf8(line).unwrap_or("");
        if let Some(rest) = s.strip_prefix("file://") {
            // <源目录>/Packages
            let mut path = [0u8; 128];
            let plen = path_join3(&mut path, rest.as_bytes(), b"/", b"Packages");
            let inlen = read_path(&path[..plen], &mut IDX_BUF);
            if inlen > 0 {
                if total > 0 && total < AVAIL_BUF.len() - 1 {
                    AVAIL_BUF[total] = b'\n';
                    total += 1;
                }
                let c = inlen.min(AVAIL_BUF.len() - total);
                AVAIL_BUF[total..total + c].copy_from_slice(&IDX_BUF[..c]);
                total += c;
                count += count_packages(&IDX_BUF[..inlen]);
            }
        }
        ls = le;
    }

    if total == 0 {
        print("[pkg] no packages in sources\n");
        return;
    }
    write_path(AVAILABLE, &AVAIL_BUF[..total]);
    print("[pkg] updated ");
    print_dec(count);
    print(" packages\n");
}

/// 解析 Depends 字段（逗号分隔），依次检查依赖是否已安装。
fn check_deps(idx: &[u8], start: usize, end: usize) -> bool {
    let mut deps = [0u8; 128];
    let dlen = field_copy(idx, start, end, "Depends", &mut deps);
    if dlen == 0 {
        return true;
    }
    let mut i = 0usize;
    while i < dlen {
        while i < dlen && (deps[i] == b' ' || deps[i] == b'\t') {
            i += 1;
        }
        let mut j = i;
        while j < dlen && deps[j] != b',' {
            j += 1;
        }
        let mut e = j;
        while e > i && (deps[e - 1] == b' ' || deps[e - 1] == b'\t') {
            e -= 1;
        }
        if e > i {
            let dep = core::str::from_utf8(&deps[i..e]).unwrap_or("");
            if !is_installed(dep) {
                print("[pkg] missing dependency: ");
                print(dep);
                print("\n");
                return false;
            }
        }
        i = j + 1;
    }
    true
}

fn build_files(pkg: &Pkg, data: &[u8], out: &mut [u8]) -> usize {
    let name = core::str::from_utf8(&pkg.name[..pkg.name_len]).unwrap_or("");
    let nb = name.as_bytes();
    let mut n = 0usize;

    for &c in b"/bin/" {
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
    }
    for &c in nb {
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
    }
    for &c in b",/pkg/" {
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
    }
    for &c in nb {
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
    }
    for &c in b".mf" {
        if n < out.len() {
            out[n] = c;
            n += 1;
        }
    }

    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        let rnl = get_u32(data, off) as usize;
        off += 4;
        let rdl = get_u32(data, off) as usize;
        off += 4;
        for &c in b",/res/" {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
        }
        for &c in nb {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
        }
        if n < out.len() {
            out[n] = b'/';
            n += 1;
        }
        let rname = &data[off..off + rnl];
        for &c in rname {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
        }
        off += rnl + rdl;
    }
    n
}

/// 把 `data` 追加到 `dst` 的 `off` 处（越界部分截断，绝不越界写）。
fn append_bounded(dst: &mut [u8], off: &mut usize, data: &[u8]) {
    let n = data.len().min(dst.len().saturating_sub(*off));
    dst[*off..*off + n].copy_from_slice(&data[..n]);
    *off += n;
}

fn write_installed_record(name: &str, version: &[u8], files: &[u8]) {
    mkdir_path(INSTALLED_DIR);
    let mut p = [0u8; 128];
    let n = installed_path(&mut p, name);

    let mut rec = [0u8; 512];
    let mut off = 0usize;
    append_bounded(&mut rec, &mut off, b"name=");
    append_bounded(&mut rec, &mut off, name.as_bytes());
    append_bounded(&mut rec, &mut off, b"\n");
    append_bounded(&mut rec, &mut off, b"version=");
    append_bounded(&mut rec, &mut off, version);
    append_bounded(&mut rec, &mut off, b"\n");
    append_bounded(&mut rec, &mut off, b"files=");
    append_bounded(&mut rec, &mut off, files);
    append_bounded(&mut rec, &mut off, b"\n");

    write_path(&p[..n], &rec[..off]);
}

unsafe fn cmd_install(name: &str) {
    let alen = load_available();
    if alen == 0 {
        print("[pkg] no package index, run 'pkg update' first\n");
        return;
    }
    let Some((rs, re)) = find_record(&AVAIL_BUF[..alen], name) else {
        print("[pkg] package not found: ");
        print(name);
        print("\n");
        return;
    };

    if !check_deps(&AVAIL_BUF[..alen], rs, re) {
        return;
    }

    let mut filename = [0u8; 64];
    let flen = field_copy(&AVAIL_BUF[..alen], rs, re, "Filename", &mut filename);
    if flen == 0 {
        print("[pkg] package has no filename\n");
        return;
    }
    let mut version = [0u8; 32];
    let vlen = field_copy(&AVAIL_BUF[..alen], rs, re, "Version", &mut version);

    // 定位 .pkg 文件（本地源）
    let mut repo = [0u8; 128];
    let rlen = resolve_repo_file(&filename[..flen], &mut repo);
    if rlen == 0 {
        print("[pkg] cannot locate ");
        print_bytes(&filename[..flen]);
        print("\n");
        return;
    }

    // 下载
    print("[pkg] downloading ");
    print_bytes(&filename[..flen]);
    print("...\n");
    let pn = read_path(&repo[..rlen], &mut PKG_BUF);
    if pn == 0 {
        print("[pkg] download failed\n");
        return;
    }

    // 缓存到 /var/lib/pkg/cache/<name>.pkg
    mkdir_path(PKG_DIR);
    mkdir_path(CACHE_DIR);
    let mut cache = [0u8; 128];
    let cn = path_join3(&mut cache, CACHE_DIR, b"/", name.as_bytes());
    let mut cfull = cn;
    for &c in b".pkg" {
        if cfull < cache.len() - 1 {
            cache[cfull] = c;
            cfull += 1;
        }
    }
    cache[cfull] = 0;
    write_path(&cache[..cfull], &PKG_BUF[..pn]);

    // 解析 + 校验 + 安装
    let pkg = match parse(&PKG_BUF[..pn]) {
        Ok(p) => p,
        Err(_) => {
            print("[pkg] invalid package\n");
            return;
        }
    };
    if !verify(&PKG_BUF[..pn], &pkg) {
        print("[pkg] verifying signature... FAILED\n");
        return;
    }
    print("[pkg] verifying signature... OK\n");
    print("[pkg] installing...\n");
    if install(&pkg, &PKG_BUF[..pn]) != 0 {
        print("[pkg] install failed\n");
        return;
    }

    // 记录
    let mut files = [0u8; 512];
    let fl = build_files(&pkg, &PKG_BUF[..pn], &mut files);
    write_installed_record(name, &version[..vlen], &files[..fl]);

    print("[pkg] installed ");
    print(name);
    print("\n");
}

fn cmd_remove(name: &str) {
    if !is_installed(name) {
        print("[pkg] not installed: ");
        print(name);
        print("\n");
        return;
    }
    uninstall(name);

    // 删除已安装记录
    let mut p = [0u8; 128];
    let n = installed_path(&mut p, name);
    unlink_path(&p[..n]);

    print("[pkg] removed ");
    print(name);
    print("\n");
}

fn cmd_list() {
    let mut names = [0u8; 1024];
    let total = list_dir(INSTALLED_DIR, &mut names);
    let mut i = 0usize;
    let mut any = false;
    while i < total {
        let mut j = i;
        while j < total && names[j] != 0 {
            j += 1;
        }
        if j > i {
            let name = core::str::from_utf8(&names[i..j]).unwrap_or("");
            let mut ver = [0u8; 32];
            let vlen = installed_version(name, &mut ver);
            print("- ");
            print(name);
            print(" ");
            print_bytes(&ver[..vlen]);
            print("\n");
            any = true;
        }
        i = j + 1;
    }
    if !any {
        print("[pkg] no packages installed\n");
    }
}

unsafe fn cmd_search(kw: &str) {
    let alen = load_available();
    let kwb = kw.as_bytes();
    let mut ls = 0usize;
    let mut found = 0usize;
    while ls < alen {
        let le = next_line(&AVAIL_BUF, ls);
        let line = line_str(&AVAIL_BUF, ls, le);
        if line.is_empty() {
            ls = le;
            continue;
        }
        if line.len() >= 8 && &line[..8] == b"Package:" {
            // 定位记录结尾
            let rs = ls;
            let mut rs2 = le;
            let mut re = le;
            while rs2 < alen {
                let re2 = next_line(&AVAIL_BUF, rs2);
                let rl = line_str(&AVAIL_BUF, rs2, re2);
                if rl.is_empty() || (rl.len() >= 8 && &rl[..8] == b"Package:") {
                    re = rs2;
                    break;
                }
                re = re2;
                rs2 = re2;
            }
            let mut name = [0u8; 32];
            let nlen = field_copy(&AVAIL_BUF[..alen], rs, re, "Package", &mut name);
            let mut desc = [0u8; 128];
            let dlen = field_copy(&AVAIL_BUF[..alen], rs, re, "Description", &mut desc);
            let mut ver = [0u8; 32];
            let vlen = field_copy(&AVAIL_BUF[..alen], rs, re, "Version", &mut ver);
            if contains(&name[..nlen], kwb) || contains(&desc[..dlen], kwb) {
                print_bytes(&name[..nlen]);
                print(" ");
                print_bytes(&ver[..vlen]);
                print(" ");
                print_bytes(&desc[..dlen]);
                print("\n");
                found += 1;
            }
            ls = re;
        } else {
            ls = le;
        }
    }
    if found == 0 {
        print("[pkg] no matches for '");
        print(kw);
        print("'\n");
    }
}

unsafe fn cmd_info(name: &str) {
    let alen = load_available();
    if alen == 0 {
        print("[pkg] no package index, run 'pkg update' first\n");
        return;
    }
    let Some((rs, re)) = find_record(&AVAIL_BUF[..alen], name) else {
        print("[pkg] package not found: ");
        print(name);
        print("\n");
        return;
    };
    // 打印记录的全部字段
    let mut ls = rs;
    while ls < re {
        let le = next_line(&AVAIL_BUF, ls);
        if le > re {
            break;
        }
        let line = line_str(&AVAIL_BUF, ls, le);
        if !line.is_empty() {
            print_bytes(line);
            print("\n");
        }
        ls = le;
    }
}

/// 简单子串匹配（大小写不敏感）。
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let n = needle.len();
    if haystack.len() < n {
        return false;
    }
    let mut i = 0usize;
    while i + n <= haystack.len() {
        let mut eq = true;
        for k in 0..n {
            let a = haystack[i + k].to_ascii_lowercase();
            let b = needle[k].to_ascii_lowercase();
            if a != b {
                eq = false;
                break;
            }
        }
        if eq {
            return true;
        }
        i += 1;
    }
    false
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    if argc < 2 {
        print("usage: pkg <update|install|remove|list|search|info>\n");
        return 1;
    }
    let sub = arg(argv, 1);
    match sub {
        "update" => unsafe { cmd_update() },
        "install" => {
            if argc < 3 {
                print("usage: pkg install <name>\n");
                return 1;
            }
            unsafe { cmd_install(arg(argv, 2)) };
        }
        "remove" => {
            if argc < 3 {
                print("usage: pkg remove <name>\n");
                return 1;
            }
            cmd_remove(arg(argv, 2));
        }
        "list" => cmd_list(),
        "search" => {
            if argc < 3 {
                print("usage: pkg search <keyword>\n");
                return 1;
            }
            unsafe { cmd_search(arg(argv, 2)) };
        }
        "info" => {
            if argc < 3 {
                print("usage: pkg info <name>\n");
                return 1;
            }
            unsafe { cmd_info(arg(argv, 2)) };
        }
        _ => {
            print("pkg: unknown command '");
            print(sub);
            print("'\n");
        }
    }
    0
}

// ==================== 入口 ====================

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mov x2, sp",
    "    ldr x0, [x2], #8",
    "    mov x1, x2",
    "    bl k_main",
    "    mov x8, #93",
    "    svc #0",
    "    b .",
);

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe {
        write_fd(2, b"PANIC: kunxpkg\n");
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}
