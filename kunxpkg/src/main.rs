// KunxOS 包管理器 CLI（kunxpkg）：no_std 裸 syscall 用户态程序，静态分配、零依赖。
//
// 支持的包格式：
//   1. `.kunx`（原生）：KUNXPKG1 归档 = manifest.json + 载荷 + META-INF 证书链
//      + Ed25519 签名。安装流程：下载 → 验签（证书链 + 包签名）→ API 级别检查
//      → 依赖解析/冲突检测 → 权限声明与用户授权 → 沙箱创建（per-app UID +
//      /data/app/<name>/ 数据目录）→ 备份 → 解压 → 注册 →（失败回滚）。
//   2. `.pkg`（旧版 NOVAPKG1）：仅 FNV-1a 完整性校验（可被伪造），安装时
//      明确告警，仅作兼容。
//
// 命令：
//   pkg update                    更新软件源索引（验签 + 增量 + 缓存）
//   pkg install <name> [--yes]    从索引下载并安装
//   pkg install-file <file.kunx>  安装本地 .kunx 文件
//   pkg remove <name> [--keep-data]
//   pkg rollback <name>           从备份恢复上一个版本
//   pkg list | search <kw> | info <name>
//   pkg review <file|name> [--approve]   审核：验签 + 权限审查 + 批准
//   pkg perms <name> [grant|revoke <perm>]
//   pkg verify <file>             仅验签输出
//   pkg keys | sources            显示信任锚 / 软件源

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

mod sha512;
pub mod ed25519;
mod hex;
mod json;
mod format;
mod cert;
pub mod trust;
mod semver;
mod perms;
mod deps;
mod net;
mod fsio;
mod repo;
mod delta;
mod review;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_EXIT: usize = 93;

// ==================== 路径常量 ====================

const SOURCES: &[u8] = b"/var/lib/pkg/sources.list";
const AVAILABLE: &[u8] = b"/var/lib/pkg/available";
const INSTALLED_DIR: &[u8] = b"/var/lib/pkg/installed";
const CACHE_DIR: &[u8] = b"/var/lib/pkg/cache";
const PKG_DIR: &[u8] = b"/var/lib/pkg";
const PERMS_DIR: &[u8] = b"/var/lib/pkg/permissions";
const REVIEWED_DIR: &[u8] = b"/var/lib/pkg/reviewed";
const BACKUP_DIR: &[u8] = b"/var/lib/pkg/backup";
const INDEX_CACHE: &[u8] = b"/var/lib/pkg/cache/index";
const UID_COUNTER: &[u8] = b"/var/lib/pkg/uid_counter";
const DATA_APP: &[u8] = b"/data/app";
/// per-app UID 基址（Android 惯例）。
const APP_UID_BASE: u32 = 10000;

// ==================== 全局缓冲（静态分配） ====================

/// .kunx 包文件缓冲（ELF ≤ 111KB + manifest + 证书 + 签名，留足余量）。
static mut PKG_BUF: [u8; 160 * 1024] = [0; 160 * 1024];
/// 合并后的可用包索引。
static mut AVAIL_BUF: [u8; 16384] = [0; 16384];
/// 单个源索引缓冲。
static mut IDX_BUF: [u8; 16384] = [0; 16384];
/// 单个源缓存索引缓冲。
static mut CIDX_BUF: [u8; 16384] = [0; 16384];
/// 单个源缓存 meta 缓冲（与 META_BUF 分离，避免别名）。
static mut CMETA_BUF: [u8; 512] = [0; 512];
/// meta 缓冲。
static mut META_BUF: [u8; 512] = [0; 512];
/// sources.list 内容。
static mut SRC_BUF: [u8; 1024] = [0; 1024];
/// 已安装记录缓冲。
static mut REC_BUF: [u8; 1024] = [0; 1024];
/// 临时缓冲（权限提示等）。
static mut TMP_BUF: [u8; 4096] = [0; 4096];

// ==================== 基础 I/O ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("svc #0", in("x8") n, inlateout("x0") a1 => ret, options(nostack));
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

fn print_dec_u32(mut v: u32) {
    if v == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 12];
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

fn print_hex_buf(b: &[u8]) {
    let mut out = [0u8; 132];
    let n = hex::encode(b, &mut out);
    unsafe {
        write_fd(1, &out[..n]);
    }
}

fn fmt_u32(mut v: u32, out: &mut [u8]) -> usize {
    if v == 0 {
        if out.is_empty() {
            return 0;
        }
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 12];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    let c = n.min(out.len());
    out[..c].copy_from_slice(&tmp[i..i + c]);
    c
}

fn fmt_u64(mut v: u64, out: &mut [u8]) -> usize {
    if v == 0 {
        if out.is_empty() {
            return 0;
        }
        out[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = tmp.len() - i;
    let c = n.min(out.len());
    out[..c].copy_from_slice(&tmp[i..i + c]);
    c
}

// ==================== 程序参数（argv） ====================

fn arg(argv: *const usize, i: usize) -> &'static str {
    unsafe {
        let p = *argv.add(i);
        if p == 0 {
            return "";
        }
        let mut len = 0usize;
        while len < 512 && *((p as *const u8).add(len)) != 0 {
            len += 1;
        }
        core::str::from_utf8(core::slice::from_raw_parts(p as *const u8, len)).unwrap_or("")
    }
}

fn has_flag(argv: *const usize, argc: usize, flag: &str) -> bool {
    for i in 2..argc {
        if arg(argv, i) == flag {
            return true;
        }
    }
    false
}

// ==================== 索引 / 记录解析（Debian Packages 风格） ====================

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

/// 定位名为 `name` 的包记录，返回 (start, end)。
fn find_record(idx: &[u8], name: &str) -> Option<(usize, usize)> {
    let nb = name.as_bytes();
    let mut ls = 0usize;
    while ls < idx.len() {
        let le = next_line(idx, ls);
        let line = line_str(idx, ls, le);
        if line.len() >= 8 && &line[..8] == b"Package:" {
            if trim(&line[8..]) == nb {
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

/// 拷贝记录 [start,end) 内 `key` 字段的值到 `out`，返回长度；无此字段 0。
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
    fsio::path_join(out, INSTALLED_DIR, name.as_bytes())
}

fn is_installed(name: &str) -> bool {
    let mut p = [0u8; 160];
    let n = installed_path(&mut p, name);
    fsio::exists_path(&p[..n])
}

/// 读已安装记录到 REC_BUF，返回长度。
unsafe fn load_installed(name: &str) -> usize {
    let mut p = [0u8; 160];
    let n = installed_path(&mut p, name);
    fsio::read_path(&p[..n], &mut REC_BUF)
}

/// 从 REC_BUF 记录取字段（name=.. / version=.. 风格）。
unsafe fn rec_field(key: &[u8], rlen: usize) -> &[u8] {
    let mut ls = 0usize;
    while ls < rlen {
        let le = next_line(&REC_BUF, ls);
        let line = line_str(&REC_BUF, ls, le);
        if line.len() > key.len() && &line[..key.len()] == key && line[key.len()] == b'=' {
            return &line[key.len() + 1..];
        }
        ls = le;
    }
    &[]
}

fn installed_version_of(name: &str, out: &mut [u8]) -> usize {
    unsafe {
        let rlen = load_installed(name);
        let v = rec_field(b"version", rlen);
        let c = v.len().min(out.len());
        out[..c].copy_from_slice(&v[..c]);
        c
    }
}

/// 已安装版本是否满足约束（deps 引擎回调）。
fn installed_satisfies(name: &[u8], constraint: &[u8]) -> bool {
    if constraint.is_empty() {
        return is_installed(core::str::from_utf8(name).unwrap_or(""));
    }
    let name_str = core::str::from_utf8(name).unwrap_or("");
    if !is_installed(name_str) {
        return false;
    }
    unsafe {
        let rlen = load_installed(name_str);
        let v = rec_field(b"version", rlen);
        match semver::parse(v) {
            Some(ver) => match semver::parse_constraints(constraint) {
                Some((cs, cn)) => semver::match_all(&ver, &cs, cn),
                None => false,
            },
            None => false,
        }
    }
}

/// 已安装包是否提供虚拟名 / 冲突名（deps 引擎回调）。
fn installed_provides(name: &[u8]) -> bool {
    let name_str = core::str::from_utf8(name).unwrap_or("");
    if is_installed(name_str) {
        return true;
    }
    // 扫描所有已安装记录的 provides= 字段
    let mut names = [0u8; 1024];
    let total = fsio::list_dir(INSTALLED_DIR, &mut names);
    let mut i = 0usize;
    while i < total {
        let mut j = i;
        while j < total && names[j] != 0 {
            j += 1;
        }
        if j > i {
            let pkg = core::str::from_utf8(&names[i..j]).unwrap_or("");
            unsafe {
                let rlen = load_installed(pkg);
                let prov = rec_field(b"provides", rlen);
                for part in prov.split(|&c| c == b',') {
                    if trim(part) == name {
                        return true;
                    }
                }
            }
        }
        i = j + 1;
    }
    false
}

/// 可用索引中是否存在（deps 引擎回调）。
fn available_has(name: &[u8]) -> bool {
    unsafe {
        let alen = load_available();
        let s = core::str::from_utf8(name).unwrap_or("");
        find_record(&AVAIL_BUF[..alen], s).is_some()
    }
}

unsafe fn load_available() -> usize {
    fsio::read_path(AVAILABLE, &mut AVAIL_BUF)
}

// ==================== 软件源 / 下载 ====================

/// 下载索引中 `filename` 指向的包到 PKG_BUF，返回长度；失败 0。
unsafe fn download_pkg(filename: &[u8]) -> usize {
    let sn = fsio::read_path(SOURCES, &mut SRC_BUF);
    let mut sources = [repo::Source {
        kind: repo::SourceKind::File,
        host: [0; 96],
        host_len: 0,
        port: 0,
        path: [0; 192],
        path_len: 0,
    }; repo::MAX_SOURCES];
    let (n, _bad) = repo::parse_sources(&SRC_BUF[..sn], &mut sources);
    for i in 0..n {
        let src = sources[i];
        let r = repo::fetch_file(&src, filename, &mut PKG_BUF);
        if r > 0 {
            return r as usize;
        }
    }
    0
}

// ==================== 命令：update ====================

unsafe fn cmd_update() {
    let sn = fsio::read_path(SOURCES, &mut SRC_BUF);
    if sn == 0 {
        print("[pkg] 无法读取 sources.list\n");
        return;
    }
    fsio::mkdir_path(PKG_DIR);
    fsio::mkdir_path(CACHE_DIR);
    fsio::mkdir_path(INDEX_CACHE);

    let mut sources = [repo::Source {
        kind: repo::SourceKind::File,
        host: [0; 96],
        host_len: 0,
        port: 0,
        path: [0; 192],
        path_len: 0,
    }; repo::MAX_SOURCES];
    let (n, bad) = repo::parse_sources(&SRC_BUF[..sn], &mut sources);
    if n == 0 {
        print("[pkg] sources.list 中没有有效软件源\n");
        return;
    }
    if bad > 0 {
        print("[pkg] 警告: ");
        print_dec(bad);
        print(" 条无效软件源行已跳过\n");
    }

    let mut total = 0usize;
    let mut count = 0usize;
    for i in 0..n {
        let src = sources[i];
        // 源缓存目录 cache/index/<i>/
        let mut dir = [0u8; 160];
        let mut idx_s = [0u8; 8];
        let ilen = fmt_u32(i as u32, &mut idx_s);
        let dn = fsio::path_join3(&mut dir, INDEX_CACHE, b"/", &idx_s[..ilen]);
        fsio::mkdir_path(&dir[..dn]);

        // 读缓存
        let mut cp = [0u8; 200];
        let cn = fsio::path_join3(&mut cp, &dir[..dn], b"/", b"Packages");
        let clen = fsio::read_path(&cp[..cn], &mut CIDX_BUF);
        let mut mp = [0u8; 200];
        let mn = fsio::path_join3(&mut mp, &dir[..dn], b"/", b"Packages.meta");
        let mlen = fsio::read_path(&mp[..mn], &mut CMETA_BUF);

        print("[pkg] 正在获取软件源 ");
        print_dec(i);
        print("...\n");
        let (len, rev, delta, mlen_new) = repo::fetch_index(
            &src,
            &mut IDX_BUF,
            &mut META_BUF,
            &CIDX_BUF[..clen],
            &CMETA_BUF[..mlen],
        );
        if len < 0 {
            // 临时诊断
            {
                let mut dp = [0u8; 256];
                let mut d1 = [0u8; 256];
                let d1n = repo::join_url_path_pub(&src, b"Packages", &mut d1);
                let fd1 = fsio::open_path(&d1[..d1n], fsio::O_RDONLY);
                print("[dbg] open fd=");
                print_dec((fd1 as isize) as usize);
                let n1 = if fd1 >= 0 {
                    let mut total = 0usize;
                    loop {
                        let space = dp.len() - total;
                        if space == 0 { break; }
                        let chunk = space.min(4096);
                        let rn = syscall3(SYS_READ, fd1 as usize, dp.as_mut_ptr() as usize + total, chunk) as isize;
                        if rn <= 0 { print(" rd="); print_dec((rn as isize) as usize); break; }
                        total += rn as usize;
                        if rn < chunk as isize { break; }
                    }
                    if fd1 >= 0 { syscall1(57, fd1 as usize); }
                    total
                } else { 0 };
                print("[dbg] idxpath=");
                print(core::str::from_utf8(&d1[..d1n]).unwrap_or("?"));
                print(" n=");
                print_dec(n1);
                let mut d2 = [0u8; 256];
                let d2n = repo::join_url_path_pub(&src, b"Packages.sig", &mut d2);
                let mut sb2 = [0u8; 80];
                let n2 = fsio::read_path(&d2[..d2n], &mut sb2);
                print(" sign=");
                print_dec(n2);
                print("\n");
                if n1 > 0 && n2 == 64 {
                    let mut s64 = [0u8; 64];
                    s64.copy_from_slice(&sb2[..64]);
                    let ok = crate::ed25519::verify(&crate::trust::REPO_ANCHOR, &dp[..n1], &s64);
                    print("[dbg] verify=");
                    print_dec(ok as usize);
                    print("\n");
                }
            }
            print("[pkg]   软件源 ");
            print_dec(i);
            print(" 获取失败 (");
            print_dec((-len) as usize);
            print(")\n");
            continue;
        }
        let len = len as usize;
        print("[pkg]   revision=");
        print_dec_u32(rev as u32);
        if delta {
            print(" (增量已应用)");
        }
        print(", packages=");
        print_dec(count_packages(&IDX_BUF[..len]));
        print("\n");

        // 写缓存
        fsio::write_path(&cp[..cn], &IDX_BUF[..len]);
        fsio::write_path(&mp[..mn], &META_BUF[..mlen_new]);

        // 合并进 available
        if total > 0 && total < AVAIL_BUF.len() - 1 {
            AVAIL_BUF[total] = b'\n';
            total += 1;
        }
        let c = len.min(AVAIL_BUF.len() - total);
        AVAIL_BUF[total..total + c].copy_from_slice(&IDX_BUF[..c]);
        total += c;
        count += count_packages(&IDX_BUF[..len]);
    }

    if total == 0 {
        print("[pkg] 未获取到任何软件包\n");
        return;
    }
    fsio::write_path(AVAILABLE, &AVAIL_BUF[..total]);
    print("[pkg] 已更新 ");
    print_dec(count);
    print(" 个软件包\n");
}

// ==================== 命令：install ====================

unsafe fn cmd_install(name: &str, yes: bool) {
    let alen = load_available();
    if alen == 0 {
        print("[pkg] 没有软件包索引，请先运行 'pkg update'\n");
        return;
    }
    let Some((rs, re)) = find_record(&AVAIL_BUF[..alen], name) else {
        print("[pkg] 未找到软件包: ");
        print(name);
        print("\n");
        return;
    };

    let mut filename = [0u8; 64];
    let flen = field_copy(&AVAIL_BUF[..alen], rs, re, "Filename", &mut filename);
    if flen == 0 {
        print("[pkg] 软件包缺少文件名\n");
        return;
    }
    let mut version = [0u8; 32];
    let vlen = field_copy(&AVAIL_BUF[..alen], rs, re, "Version", &mut version);

    print("[pkg] 正在下载 ");
    print_bytes(&filename[..flen]);
    print("...\n");
    let pn = download_pkg(&filename[..flen]);
    if pn == 0 {
        print("[pkg] 下载失败\n");
        return;
    }

    // 缓存
    fsio::mkdir_path(PKG_DIR);
    fsio::mkdir_path(CACHE_DIR);
    let mut cache = [0u8; 200];
    let cn = fsio::path_join3(&mut cache, CACHE_DIR, b"/", name.as_bytes());
    let mut cfull = cn;
    for &c in b".kunx" {
        if cfull < cache.len() - 1 {
            cache[cfull] = c;
            cfull += 1;
        }
    }
    cache[cfull] = 0;
    fsio::write_path(&cache[..cfull], &PKG_BUF[..pn]);

    if PKG_BUF[..pn].starts_with(&format::MAGIC) {
        install_kunx(&PKG_BUF[..pn], name, yes);
    } else if PKG_BUF[..pn].starts_with(b"NOVAPKG1") {
        print("[pkg] 旧版 .pkg 格式：仅 FNV-1a 完整性校验，未经密码学验证\n");
        install_legacy(&PKG_BUF[..pn], name, &version[..vlen]);
    } else {
        print("[pkg] 未知的软件包格式\n");
    }
}

/// 安装本地 .kunx 文件。
unsafe fn cmd_install_file(path: &str, yes: bool) {
    let pn = fsio::read_path(path.as_bytes(), &mut PKG_BUF);
    if pn == 0 {
        print("[pkg] 无法读取 ");
        print(path);
        print("\n");
        return;
    }
    if PKG_BUF[..pn].starts_with(&format::MAGIC) {
        install_kunx(&PKG_BUF[..pn], "", yes);
    } else {
        print("[pkg] 不是 .kunx 软件包\n");
    }
}

/// 核心安装流程（.kunx）：验签 → API → 依赖 → 授权 → 沙箱 → 备份 → 解压 → 注册。
unsafe fn install_kunx(data: &[u8], index_name: &str, yes: bool) {
    let rv = match review::review(data, 0) {
        Some(r) => r,
        None => {
            print("[pkg] 无效的软件包\n");
            return;
        }
    };
    let mname = rv.manifest.name;
    if !index_name.is_empty() && mname != index_name.as_bytes() {
        print("[pkg] manifest 名称不匹配: ");
        print_bytes(mname);
        print(" != ");
        print(index_name);
        print("\n");
        return;
    }
    let name_str = core::str::from_utf8(mname).unwrap_or("");
    if name_str.is_empty() {
        print("[pkg] 软件包名称为空\n");
        return;
    }
    // 包名白名单（防路径穿越）
    if !format::valid_path(mname) || mname.contains(&b'/') {
        print("[pkg] 软件包名称无效\n");
        return;
    }

    print("[pkg] 正在验证证书链... ");
    if !rv.chain_valid {
        print("FAILED\n");
        return;
    }
    print("OK\n");
    print("[pkg] 正在验证软件包签名... ");
    if !rv.sig_valid {
        print("FAILED\n");
        return;
    }
    print("OK\n");

    if !rv.api_ok {
        print("[pkg] 需要 API 级别 ");
        print_dec_u32(rv.manifest.min_api);
        print("，系统为 ");
        print_dec_u32(trust::API_LEVEL);
        print("\n");
        return;
    }

    // 依赖 + 冲突
    match deps::check(
        &rv.manifest.deps,
        rv.manifest.dep_count,
        &rv.manifest.provides,
        rv.manifest.provide_count,
        &rv.manifest.conflicts,
        rv.manifest.conflict_count,
        installed_satisfies,
        available_has,
        installed_provides,
    ) {
        Ok(()) => {}
        Err(e) => {
            print("[pkg] 依赖检查失败: ");
            match e {
                deps::DepError::Missing(n) => {
                    print_bytes(n);
                    print(" 无法满足依赖");
                }
                deps::DepError::Conflict(n) => {
                    print_bytes(n);
                    print(" 与已安装的软件包冲突");
                }
                deps::DepError::ProvidesTaken(n) => {
                    print_bytes(n);
                    print(" 已被其他软件包提供");
                }
            }
            print("\n");
            return;
        }
    }

    // 权限声明与授权
    print("[pkg] 权限: ");
    if rv.manifest.perm_count == 0 {
        print("(无)");
    } else {
        for i in 0..rv.manifest.perm_count {
            if i > 0 {
                print(", ");
            }
            print_bytes(rv.manifest.permissions[i]);
        }
    }
    print("\n");
    if rv.dangerous > 0 {
        print("[pkg] 警告: ");
        print_dec(rv.dangerous);
        print(" 项危险权限需要用户授权\n");
        if !yes && !approved_before(name_str, &rv.manifest.version) && !prompt_yes() {
            print("[pkg] 安装已被用户取消\n");
            return;
        }
        if yes {
            print("[pkg] 权限已授权 (--yes)\n");
        }
    }

    // 沙箱：分配 per-app UID + 数据目录
    let uid = alloc_uid();
    print("[pkg] 沙箱 uid=");
    print_dec_u32(uid);
    print("\n");
    let mut appdir = [0u8; 160];
    let an = fsio::path_join3(&mut appdir, DATA_APP, b"/", name_str.as_bytes());
    fsio::mkdir_path(&appdir[..an]);
    let mut fdir = [0u8; 200];
    let fn0 = fsio::path_join3(&mut fdir, &appdir[..an], b"/", b"files");
    fsio::mkdir_path(&fdir[..fn0]);
    let mut cdir = [0u8; 200];
    let cn0 = fsio::path_join3(&mut cdir, &appdir[..an], b"/", b"cache");
    fsio::mkdir_path(&cdir[..cn0]);

    // 备份旧版本（升级回滚）
    let had_old = is_installed(name_str);
    if had_old {
        backup_pkg(name_str);
    }

    // 解压载荷
    let mut files = [0u8; 1024];
    let mut fn_len = 0usize;
    let mut ok = true;
    let mut main_extracted = false;
    let mut it = format::PayloadIter::new(rv.pkg.payload(data));
    while let Some(e) = it.next() {
        if !format::valid_path(e.path) {
            print("[pkg] 非法的载荷路径: ");
            print_bytes(e.path);
            print("\n");
            ok = false;
            break;
        }
        let path_str = core::str::from_utf8(e.path).unwrap_or("");
        if path_str.as_bytes() == rv.manifest.main {
            main_extracted = true;
        }
        match extract_entry(e, name_str) {
            Ok(()) => {
                fn_len = append_path(&mut files, fn_len, e.path);
            }
            Err(msg) => {
                print("[pkg] 解压失败: ");
                print_bytes(e.path);
                print(" (");
                print(msg);
                print(")\n");
                ok = false;
                break;
            }
        }
    }
    if ok && !rv.manifest.main.is_empty() && !main_extracted {
        print("[pkg] 缺少主入口: ");
        print_bytes(rv.manifest.main);
        print("\n");
        ok = false;
    }
    if !ok {
        rollback_files(name_str, &files[..fn_len]);
        // ★ 升级失败先恢复备份：rollback_files 会把新解压的 /bin/<name> 删掉，
        // 旧版本只存在于 backup/ —— 不恢复则系统处于"记录声称已安装 vX 而
        // 二进制不存在"的不一致状态（restore_backup 对全新安装是 no-op）。
        if !restore_backup(name_str) {
            print("[pkg] 警告：旧版本恢复失败，可用 pkg rollback 手动恢复\n");
        }
        print("[pkg] 安装失败，已回滚\n");
        return;
    }

    // manifest → /pkg/<name>.mf
    fsio::mkdir_path(b"/pkg");
    let mut mf = [0u8; 160];
    let mn = fsio::path_join3(&mut mf, b"/pkg/", name_str.as_bytes(), b".mf");
    if fsio::write_path(&mf[..mn], rv.pkg.manifest(data)) != 0 {
        rollback_files(name_str, &files[..fn_len]);
        if !restore_backup(name_str) {
            print("[pkg] 警告：旧版本恢复失败，可用 pkg rollback 手动恢复\n");
        }
        print("[pkg] 无法写入 manifest，已回滚\n");
        return;
    }
    fn_len = append_path(&mut files, fn_len, b"/pkg/<name>.mf");

    // 注册
    let mut rec = [0u8; 1024];
    let mut off = 0usize;
    append_bounded(&mut rec, &mut off, b"name=");
    append_bounded(&mut rec, &mut off, name_str.as_bytes());
    append_bounded(&mut rec, &mut off, b"\nversion=");
    append_bounded(&mut rec, &mut off, rv.manifest.version);
    append_bounded(&mut rec, &mut off, b"\nuid=");
    append_bounded(&mut rec, &mut off, &fmt_u32_buf(uid));
    append_bounded(&mut rec, &mut off, b"\napi=");
    append_bounded(&mut rec, &mut off, &fmt_u32_buf(rv.manifest.min_api));
    append_bounded(&mut rec, &mut off, b"\nperms=");
    for i in 0..rv.manifest.perm_count {
        if i > 0 {
            append_bounded(&mut rec, &mut off, b",");
        }
        append_bounded(&mut rec, &mut off, rv.manifest.permissions[i]);
    }
    append_bounded(&mut rec, &mut off, b"\nprovides=");
    for i in 0..rv.manifest.provide_count {
        if i > 0 {
            append_bounded(&mut rec, &mut off, b",");
        }
        append_bounded(&mut rec, &mut off, rv.manifest.provides[i]);
    }
    append_bounded(&mut rec, &mut off, b"\nsigner=");
    append_bounded(&mut rec, &mut off, &fp_buf(&rv.leaf_key));
    append_bounded(&mut rec, &mut off, b"\ncert=");
    append_bounded(&mut rec, &mut off, cert_subject(data));
    append_bounded(&mut rec, &mut off, b"\nfiles=");
    append_bounded(&mut rec, &mut off, &files[..fn_len]);
    append_bounded(&mut rec, &mut off, b"\n");

    fsio::mkdir_path(INSTALLED_DIR);
    let mut ip = [0u8; 160];
    let in0 = installed_path(&mut ip, name_str);
    if fsio::write_path(&ip[..in0], &rec[..off]) != 0 {
        rollback_files(name_str, &files[..fn_len]);
        print("[pkg] 无法注册，已回滚\n");
        return;
    }

    // 权限授权记录
    write_perms_record(name_str, &rv.manifest.permissions, rv.manifest.perm_count);

    // 清理备份（安装成功）
    clear_backup(name_str);

    print("[pkg] 已安装 ");
    print(name_str);
    print(" (uid=");
    print_dec_u32(uid);
    print(")\n");
}

/// 旧版 .pkg（NOVAPKG1）安装：FNV-1a 完整性校验 + 直接写入（无沙箱）。
unsafe fn install_legacy(data: &[u8], name: &str, version: &[u8]) {
    let pkg = match legacy_parse(data) {
        Ok(p) => p,
        Err(_) => {
            print("[pkg] 无效的软件包\n");
            return;
        }
    };
    if !legacy_verify(data, &pkg) {
        print("[pkg] 完整性校验 FAILED\n");
        return;
    }
    print("[pkg] 完整性 OK (旧版 FNV-1a，非密码学签名)\n");
    if legacy_install(&pkg, data) != 0 {
        print("[pkg] 安装失败\n");
        return;
    }
    let mut files = [0u8; 512];
    let fl = legacy_build_files(&pkg, data, &mut files);
    write_installed_record(name, version, &files[..fl]);
    print("[pkg] 已安装 (旧版) ");
    print(name);
    print("\n");
}

// ==================== 沙箱 / UID ====================

/// 分配 per-app UID：扫描已安装记录已用 UID，取 ≥ 10000 的最小空闲值；
/// 更新 uid_counter。
unsafe fn alloc_uid() -> u32 {
    // 收集已用 UID
    let mut used = [0u32; 64];
    let mut used_n = 0usize;
    let mut names = [0u8; 1024];
    let total = fsio::list_dir(INSTALLED_DIR, &mut names);
    let mut i = 0usize;
    while i < total {
        let mut j = i;
        while j < total && names[j] != 0 {
            j += 1;
        }
        if j > i {
            let pkg = core::str::from_utf8(&names[i..j]).unwrap_or("");
            let rlen = load_installed(pkg);
            let u = rec_field(b"uid", rlen);
            if let Some(v) = parse_u32_field(u) {
                if used_n < used.len() {
                    used[used_n] = v;
                    used_n += 1;
                }
            }
        }
        i = j + 1;
    }
    // 从 counter 继续（counter 文件存的是已分配数量）
    let mut counter = 0u32;
    let mut cb = [0u8; 16];
    let cn = fsio::read_path(UID_COUNTER, &mut cb);
    if cn > 0 {
        if let Some(v) = parse_u32_field(&cb[..cn]) {
            counter = v;
        }
    }
    let mut uid = APP_UID_BASE + counter;
    // 防御：若被占用则顺延
    while used_n > 0 && used.contains(&uid) {
        uid += 1;
    }
    let next = uid - APP_UID_BASE + 1;
    let mut nb = [0u8; 16];
    let nn = fmt_u32(next, &mut nb);
    fsio::write_path(UID_COUNTER, &nb[..nn]);
    uid
}

fn parse_u32_field(b: &[u8]) -> Option<u32> {
    let t = trim(b);
    if t.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &c in t {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u32)?;
    }
    Some(v)
}

/// 读取 per-app UID（供 launcher 等使用；未安装返回 None）。
pub fn uid_of_app(name: &str) -> Option<u32> {
    unsafe {
        let rlen = load_installed(name);
        if rlen == 0 {
            return None;
        }
        let u = rec_field(b"uid", rlen);
        parse_u32_field(u)
    }
}

// ==================== 权限 / 授权 ====================

/// 是否已批准安装该包（reviewed 记录存在且版本/指纹匹配）。
unsafe fn approved_before(name: &str, version: &[u8]) -> bool {
    let mut p = [0u8; 160];
    let n = fsio::path_join3(&mut p, REVIEWED_DIR, b"/", name.as_bytes());
    let rlen = fsio::read_path(&p[..n], &mut TMP_BUF);
    if rlen == 0 {
        return false;
    }
    // 记录：version=..

    let mut ls = 0usize;
    while ls < rlen {
        let le = next_line(&TMP_BUF, ls);
        let line = line_str(&TMP_BUF, ls, le);
        if line.len() >= 8 && &line[..8] == b"version=" && &line[8..] == version {
            return true;
        }
        ls = le;
    }
    false
}

/// 交互式授权：提示并读取一个字节（y = 同意）。
fn prompt_yes() -> bool {
    print("[pkg] 是否授权上述权限？[y/N] ");
    let mut b = [0u8; 1];
    let n = unsafe { syscall3(SYS_READ, 0, b.as_mut_ptr() as usize, 1) };
    if n != 1 {
        print("\n[pkg] 无法读取终端输入，按拒绝处理 (请用 --yes)\n");
        return false;
    }
    print("\n");
    b[0] == b'y' || b[0] == b'Y'
}

/// 写权限授权记录 /var/lib/pkg/permissions/<name>。
unsafe fn write_perms_record(name: &str, perms: &[&[u8]], n: usize) {
    fsio::mkdir_path(PERMS_DIR);
    let mut p = [0u8; 160];
    let plen = fsio::path_join3(&mut p, PERMS_DIR, b"/", name.as_bytes());
    let mut rec = [0u8; 512];
    let mut off = 0usize;
    append_bounded(&mut rec, &mut off, b"package=");
    append_bounded(&mut rec, &mut off, name.as_bytes());
    append_bounded(&mut rec, &mut off, b"\n");
    for i in 0..n {
        append_bounded(&mut rec, &mut off, perms[i]);
        append_bounded(&mut rec, &mut off, b": granted\n");
    }
    fsio::write_path(&p[..plen], &rec[..off]);
}

/// 列出某包已授权权限。
unsafe fn list_perms(name: &str) {
    let mut p = [0u8; 160];
    let plen = fsio::path_join3(&mut p, PERMS_DIR, b"/", name.as_bytes());
    let rlen = fsio::read_path(&p[..plen], &mut TMP_BUF);
    if rlen == 0 {
        print("[pkg] 无权限授权记录: ");
        print(name);
        print("\n");
        return;
    }
    print_bytes(&TMP_BUF[..rlen]);
}

/// 授予/撤销某权限（改写授权记录）。
unsafe fn cmd_perms(name: &str, action: &str, perm: &str) {
    if action.is_empty() {
        list_perms(name);
        return;
    }
    if !perms::valid_name(perm.as_bytes()) {
        print("[pkg] 无效的权限名\n");
        return;
    }
    let mut p = [0u8; 160];
    let plen = fsio::path_join3(&mut p, PERMS_DIR, b"/", name.as_bytes());
    let rlen = fsio::read_path(&p[..plen], &mut TMP_BUF);
    let mut rec = [0u8; 512];
    let mut off = 0usize;
    match action {
        "grant" => {
            // 若已存在则替换，否则追加
            let mut found = false;
            let mut ls = 0usize;
            while ls < rlen {
                let le = next_line(&TMP_BUF, ls);
                let line = line_str(&TMP_BUF, ls, le);
                if line.starts_with(perm.as_bytes()) {
                    append_bounded(&mut rec, &mut off, perm.as_bytes());
                    append_bounded(&mut rec, &mut off, b": granted\n");
                    found = true;
                } else {
                    append_bounded(&mut rec, &mut off, line);
                    append_bounded(&mut rec, &mut off, b"\n");
                }
                ls = le;
            }
            if !found {
                append_bounded(&mut rec, &mut off, perm.as_bytes());
                append_bounded(&mut rec, &mut off, b": granted\n");
            }
            fsio::write_path(&p[..plen], &rec[..off]);
            print("[pkg] 已授权 ");
            print(perm);
            print(" 给 ");
            print(name);
            print("\n");
        }
        "revoke" => {
            let mut ls = 0usize;
            while ls < rlen {
                let le = next_line(&TMP_BUF, ls);
                let line = line_str(&TMP_BUF, ls, le);
                if !line.starts_with(perm.as_bytes()) {
                    append_bounded(&mut rec, &mut off, line);
                    append_bounded(&mut rec, &mut off, b"\n");
                }
                ls = le;
            }
            fsio::write_path(&p[..plen], &rec[..off]);
            print("[pkg] 已撤销 ");
            print(perm);
            print(" 权限，来自 ");
            print(name);
            print("\n");
        }
        _ => {
            print("[pkg] 用法: pkg perms <name> [grant|revoke <perm>]\n");
        }
    }
}

// ==================== 备份 / 回滚 ====================

/// 备份现有 /bin/<name> 与 /pkg/<name>.mf 到 backup/<name>/。
unsafe fn backup_pkg(name: &str) {
    let mut bdir = [0u8; 160];
    let bn = fsio::path_join3(&mut bdir, BACKUP_DIR, b"/", name.as_bytes());
    fsio::mkdir_path(BACKUP_DIR);
    fsio::mkdir_path(&bdir[..bn]);

    let mut src = [0u8; 160];
    let mut dst = [0u8; 200];
    let sn = fsio::path_join(&mut src, b"/bin/", name.as_bytes());
    let dn = fsio::path_join3(&mut dst, &bdir[..bn], b"/", b"bin");
    let n = fsio::read_path(&src[..sn], &mut TMP_BUF);
    if n > 0 {
        fsio::write_path(&dst[..dn], &TMP_BUF[..n]);
    }
    let sn = fsio::path_join3(&mut src, b"/pkg/", name.as_bytes(), b".mf");
    let dn = fsio::path_join3(&mut dst, &bdir[..bn], b"/", b"manifest.mf");
    let n = fsio::read_path(&src[..sn], &mut TMP_BUF);
    if n > 0 {
        fsio::write_path(&dst[..dn], &TMP_BUF[..n]);
    }
}

/// 恢复备份（回滚命令 / 升级失败时）。
unsafe fn restore_backup(name: &str) -> bool {
    let mut bdir = [0u8; 160];
    let bn = fsio::path_join3(&mut bdir, BACKUP_DIR, b"/", name.as_bytes());
    let mut src = [0u8; 200];
    let mut dst = [0u8; 160];
    let sn = fsio::path_join3(&mut src, &bdir[..bn], b"/", b"bin");
    let n = fsio::read_path(&src[..sn], &mut TMP_BUF);
    if n > 0 {
        let dn = fsio::path_join(&mut dst, b"/bin/", name.as_bytes());
        if fsio::write_path(&dst[..dn], &TMP_BUF[..n]) != 0 {
            return false;
        }
    }
    let sn = fsio::path_join3(&mut src, &bdir[..bn], b"/", b"manifest.mf");
    let n = fsio::read_path(&src[..sn], &mut TMP_BUF);
    if n > 0 {
        let dn = fsio::path_join3(&mut dst, b"/pkg/", name.as_bytes(), b".mf");
        if fsio::write_path(&dst[..dn], &TMP_BUF[..n]) != 0 {
            return false;
        }
    }
    clear_backup(name);
    true
}

unsafe fn clear_backup(name: &str) {
    let mut bdir = [0u8; 160];
    let bn = fsio::path_join3(&mut bdir, BACKUP_DIR, b"/", name.as_bytes());
    let mut f = [0u8; 200];
    let fn0 = fsio::path_join3(&mut f, &bdir[..bn], b"/", b"bin");
    fsio::unlink_path(&f[..fn0]);
    let fn0 = fsio::path_join3(&mut f, &bdir[..bn], b"/", b"manifest.mf");
    fsio::unlink_path(&f[..fn0]);
}

/// 卸载时删除 files 列表中记录的文件（回滚/卸载共用）。
unsafe fn rollback_files(name: &str, files: &[u8]) {
    let mut i = 0usize;
    while i < files.len() {
        let mut j = i;
        while j < files.len() && files[j] != 0 {
            j += 1;
        }
        if j > i {
            let f = &files[i..j];
            // 映射回真实路径
            let mut real = [0u8; 256];
            let rn = map_record_path(name, f, &mut real);
            if rn > 0 {
                fsio::unlink_path(&real[..rn]);
            }
        }
        i = j + 1;
    }
    // 资源目录
    let mut rdir = [0u8; 160];
    let rn = fsio::path_join(&mut rdir, b"/res/", name.as_bytes());
    remove_dir_contents(&rdir[..rn]);
}

/// 把 files 记录中的路径映射回真实路径。
fn map_record_path(name: &str, rec: &[u8], out: &mut [u8]) -> usize {
    if rec == b"/pkg/<name>.mf" {
        return fsio::path_join3(out, b"/pkg/", name.as_bytes(), b".mf");
    }
    if rec.starts_with(b"bin/") {
        return fsio::path_join(out, b"/bin/", &rec[4..]);
    }
    if rec.starts_with(b"res/") {
        let mut t = [0u8; 256];
        let tn = fsio::path_join3(&mut t, b"/res/", name.as_bytes(), b"/");
        let base = &t[..tn];
        return fsio::path_join(out, base, &rec[4..]);
    }
    if rec.starts_with(b"lib/") {
        return fsio::path_join(out, b"/lib/", &rec[4..]);
    }
    if rec.starts_with(b"data/") {
        let mut t = [0u8; 256];
        let tn = fsio::path_join3(&mut t, DATA_APP, b"/", name.as_bytes());
        let base = &t[..tn];
        return fsio::path_join(out, base, &rec[5..]);
    }
    0
}

/// 删除目录下全部条目。
unsafe fn remove_dir_contents(dir: &[u8]) {
    let mut names = [0u8; 1024];
    let total = fsio::list_dir(dir, &mut names);
    let mut i = 0usize;
    while i < total {
        let mut j = i;
        while j < total && names[j] != 0 {
            j += 1;
        }
        if j > i {
            let mut full = [0u8; 256];
            let flen = fsio::path_join3(&mut full, dir, b"/", &names[i..j]);
            fsio::unlink_path(&full[..flen]);
        }
        i = j + 1;
    }
}

// ==================== 命令：remove / rollback ====================

unsafe fn cmd_remove(name: &str, keep_data: bool) {
    if !is_installed(name) {
        print("[pkg] 未安装: ");
        print(name);
        print("\n");
        return;
    }
    let rlen = load_installed(name);
    let files = rec_field(b"files", rlen);
    // 从 files 拷贝出来（rec_field 引用 REC_BUF，后续调用会覆盖）
    let mut fl = [0u8; 1024];
    let fn_len = files.len().min(fl.len());
    fl[..fn_len].copy_from_slice(&files[..fn_len]);
    rollback_files(name, &fl[..fn_len]);

    // 沙箱数据：默认清除；--keep-data 保留
    let mut appdir = [0u8; 160];
    let an = fsio::path_join3(&mut appdir, DATA_APP, b"/", name.as_bytes());
    if !keep_data {
        remove_dir_contents(&appdir[..an]);
    } else {
        print("[pkg] 用户数据保留在 ");
        print_bytes(&appdir[..an]);
        print("\n");
    }

    // 已安装记录 / 权限记录 / 缓存
    let mut p = [0u8; 160];
    let n = installed_path(&mut p, name);
    fsio::unlink_path(&p[..n]);
    let n = fsio::path_join3(&mut p, PERMS_DIR, b"/", name.as_bytes());
    fsio::unlink_path(&p[..n]);
    let n = fsio::path_join3(&mut p, CACHE_DIR, b"/", name.as_bytes());
    let mut cn = n;
    for &c in b".kunx" {
        if cn < p.len() - 1 {
            p[cn] = c;
            cn += 1;
        }
    }
    p[cn] = 0;
    fsio::unlink_path(&p[..cn]);

    print("[pkg] 已卸载 ");
    print(name);
    print("\n");
}

unsafe fn cmd_rollback(name: &str) {
    if !is_installed(name) {
        print("[pkg] 未安装: ");
        print(name);
        print("\n");
        return;
    }
    if restore_backup(name) {
        print("[pkg] 已回滚 ");
        print(name);
        print(" (从备份恢复)\n");
    } else {
        print("[pkg] 无可用备份: ");
        print(name);
        print("\n");
    }
}

// ==================== 命令：list / search / info ====================

fn cmd_list() {
    let mut names = [0u8; 1024];
    let total = fsio::list_dir(INSTALLED_DIR, &mut names);
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
            let vlen = installed_version_of(name, &mut ver);
            let uid = uid_of_app(name);
            print("- ");
            print(name);
            print(" ");
            print_bytes(&ver[..vlen]);
            if let Some(u) = uid {
                print(" uid=");
                print_dec_u32(u);
            }
            print("\n");
            any = true;
        }
        i = j + 1;
    }
    if !any {
        print("[pkg] 未安装任何软件包\n");
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
        print("[pkg] 未找到匹配 '");
        print(kw);
        print("'\n");
    }
}

unsafe fn cmd_info(name: &str) {
    let alen = load_available();
    if alen == 0 {
        print("[pkg] 没有软件包索引，请先运行 'pkg update'\n");
        return;
    }
    let Some((rs, re)) = find_record(&AVAIL_BUF[..alen], name) else {
        print("[pkg] 未找到软件包: ");
        print(name);
        print("\n");
        return;
    };
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

// ==================== 命令：review / verify / keys / sources ====================

unsafe fn load_pkg_arg(arg0: &str) -> usize {
    // 若是文件路径（含 '/' 或 '.kunx' 后缀）则直接读；否则查缓存
    if arg0.contains('/') || arg0.ends_with(".kunx") || arg0.ends_with(".pkg") {
        return fsio::read_path(arg0.as_bytes(), &mut PKG_BUF);
    }
    let mut p = [0u8; 200];
    let n = fsio::path_join3(&mut p, CACHE_DIR, b"/", arg0.as_bytes());
    let mut cn = n;
    for &c in b".kunx" {
        if cn < p.len() - 1 {
            p[cn] = c;
            cn += 1;
        }
    }
    p[cn] = 0;
    fsio::read_path(&p[..cn], &mut PKG_BUF)
}

unsafe fn cmd_review(arg0: &str, approve: bool) {
    let pn = load_pkg_arg(arg0);
    if pn == 0 {
        print("[pkg] 无法读取 ");
        print(arg0);
        print(" (需要文件路径或已缓存的软件包名)\n");
        return;
    }
    if !PKG_BUF[..pn].starts_with(&format::MAGIC) {
        print("[pkg] 不是 .kunx 软件包\n");
        return;
    }
    let rv = match review::review(&PKG_BUF[..pn], 0) {
        Some(r) => r,
        None => {
            print("[pkg] 无效的软件包\n");
            return;
        }
    };
    print("========== 审核报告 ==========\n");
    print("包名: ");
    print_bytes(rv.manifest.name);
    print("\n版本: ");
    print_bytes(rv.manifest.version);
    print("\nAPI 级别: ");
    print_dec_u32(rv.manifest.min_api);
    print(" (系统 ");
    print_dec_u32(trust::API_LEVEL);
    print(")");
    if rv.api_ok {
        print(" [兼容]");
    } else {
        print(" [不兼容]");
    }
    print("\n描述: ");
    print_bytes(rv.manifest.description);
    print("\n作者: ");
    print_bytes(rv.manifest.author);
    print("\n入口: ");
    print_bytes(rv.manifest.main);
    print("\n证书链: ");
    print_dec(rv.cert_count);
    if rv.chain_valid {
        print(" 级 [有效]");
    } else {
        print(" 级 [无效]");
    }
    print("\n签名: ");
    if rv.sig_valid {
        print("[有效]");
    } else {
        print("[无效]");
    }
    print("\n签名者指纹: ");
    print_hex_buf(&fp16(&rv.leaf_key));
    print("\n证书主体: ");
    print_bytes(cert_subject(&PKG_BUF[..pn]));
    print("\n权限: ");
    if rv.manifest.perm_count == 0 {
        print("(无)");
    }
    print("\n");
    for i in 0..rv.manifest.perm_count {
        let p = rv.manifest.permissions[i];
        print("  - ");
        print_bytes(p);
        let risk = if perms::is_dangerous(p) { "危险" } else { "普通" };
        print(" [");
        print(risk);
        print("]");
        if let Some(d) = perms::lookup(p) {
            print(" ");
            print_bytes(d.label_zh);
        }
        print("\n");
    }
    print("依赖: ");
    if rv.manifest.dep_count == 0 {
        print("(无)");
    }
    print("\n");
    for i in 0..rv.manifest.dep_count {
        print("  - ");
        print_bytes(rv.manifest.deps[i].0 .0);
        print(" ");
        print_bytes(rv.manifest.deps[i].1);
        print("\n");
    }
    print("========== 结论 ==========\n");
    if rv.chain_valid && rv.sig_valid && rv.api_ok {
        print("通过：签名有效，证书链有效，API 兼容");
        if rv.dangerous > 0 {
            print("，含 ");
            print_dec(rv.dangerous);
            print(" 项危险权限需用户授权");
        }
        print("\n");
    } else {
        print("不通过：");
        if !rv.chain_valid {
            print("证书链无效 ");
        }
        if !rv.sig_valid {
            print("签名无效 ");
        }
        if !rv.api_ok {
            print("API 不兼容 ");
        }
        print("\n");
        return;
    }

    if approve {
        // ★ 包名白名单（与 install 路径同级防御）：manifest.name 来自包内容
        // （含 `..` 或 `/` 时可写出 REVIEWED_DIR 之外的任意路径）。签名链
        // 只验身份不验名字合法性。
        if !format::valid_path(rv.manifest.name) || rv.manifest.name.contains(&b'/') {
            print("[pkg] 软件包名称无效\n");
            return;
        }
        fsio::mkdir_path(REVIEWED_DIR);
        let mut p = [0u8; 160];
        let n = fsio::path_join3(&mut p, REVIEWED_DIR, b"/", rv.manifest.name);
        let mut rec = [0u8; 256];
        let mut off = 0usize;
        append_bounded(&mut rec, &mut off, b"version=");
        append_bounded(&mut rec, &mut off, rv.manifest.version);
        append_bounded(&mut rec, &mut off, b"\nsigner=");
        append_bounded(&mut rec, &mut off, &fp16(&rv.leaf_key));
        append_bounded(&mut rec, &mut off, b"\napproved=1\n");
        fsio::write_path(&p[..n], &rec[..off]);
        print("[pkg] 已批准 ");
        print_bytes(rv.manifest.name);
        print(" v");
        print_bytes(rv.manifest.version);
        print("\n");
    }
}

unsafe fn cmd_verify(arg0: &str) {
    let pn = load_pkg_arg(arg0);
    if pn == 0 {
        print("[pkg] 无法读取 ");
        print(arg0);
        print("\n");
        return;
    }
    if !PKG_BUF[..pn].starts_with(&format::MAGIC) {
        print("[pkg] 不是 .kunx 软件包\n");
        return;
    }
    match review::review(&PKG_BUF[..pn], 0) {
        Some(rv) => {
            print("[pkg] 签名: ");
            print(if rv.sig_valid { "OK" } else { "FAILED" });
            print(", 证书链: ");
            print(if rv.chain_valid { "OK" } else { "FAILED" });
            print(", API: ");
            print(if rv.api_ok { "OK" } else { "FAILED" });
            print("\n");
        }
        None => print("[pkg] 无效的软件包\n"),
    }
}

fn cmd_keys() {
    print("[pkg] 信任锚 (Ed25519)\n");
    print("  root:  ");
    print_hex_buf(&trust::ROOT_ANCHOR);
    print("\n  repo:  ");
    print_hex_buf(&trust::REPO_ANCHOR);
    print("\n  api_level: ");
    print_dec_u32(trust::API_LEVEL);
    print("\n");
}

unsafe fn cmd_sources() {
    let sn = fsio::read_path(SOURCES, &mut SRC_BUF);
    if sn == 0 {
        print("[pkg] 缺少 sources.list\n");
        return;
    }
    print_bytes(&SRC_BUF[..sn]);
}

// ==================== 安装辅助 ====================

/// 解压单个载荷条目到安装位置。返回 Ok(()) / Err(原因)。
unsafe fn extract_entry(e: format::Entry, name: &str) -> Result<(), &'static str> {
    let p = e.path;
    let mut out = [0u8; 256];
    let n = if let Some(rest) = p.strip_prefix(b"bin/") {
        fsio::path_join(&mut out, b"/bin/", rest)
    } else if let Some(rest) = p.strip_prefix(b"res/") {
        let mut t = [0u8; 256];
        let tn = fsio::path_join3(&mut t, b"/res/", name.as_bytes(), b"/");
        fsio::mkdir_path(&t[..tn]);
        let base = &t[..tn];
        fsio::path_join(&mut out, base, rest)
    } else if let Some(rest) = p.strip_prefix(b"lib/") {
        fsio::mkdir_path(b"/lib");
        fsio::path_join(&mut out, b"/lib/", rest)
    } else if let Some(rest) = p.strip_prefix(b"data/") {
        let mut t = [0u8; 256];
        let tn = fsio::path_join3(&mut t, DATA_APP, b"/", name.as_bytes());
        fsio::mkdir_path(&t[..tn]);
        let base = &t[..tn];
        fsio::path_join(&mut out, base, rest)
    } else {
        return Err("未知路径前缀 (预期 bin/ res/ lib/ data/)");
    };
    if n == 0 || n >= out.len() {
        return Err("路径过长");
    }
    if fsio::write_path(&out[..n], e.data) != 0 {
        return Err("写入失败");
    }
    Ok(())
}

/// 把 payload 相对路径加入 files 记录（保留原相对路径）。
fn append_path(files: &mut [u8], mut len: usize, p: &[u8]) -> usize {
    if len > 0 && len < files.len() {
        files[len] = b',';
        len += 1;
    }
    let c = p.len().min(files.len().saturating_sub(len));
    files[len..len + c].copy_from_slice(&p[..c]);
    len + c
}

fn append_bounded(dst: &mut [u8], off: &mut usize, data: &[u8]) {
    let n = data.len().min(dst.len().saturating_sub(*off));
    dst[*off..*off + n].copy_from_slice(&data[..n]);
    *off += n;
}

/// 写入已安装记录（旧版 .pkg 路径用）。
fn write_installed_record(name: &str, version: &[u8], files: &[u8]) {
    fsio::mkdir_path(INSTALLED_DIR);
    let mut p = [0u8; 160];
    let n = installed_path(&mut p, name);
    let mut rec = [0u8; 512];
    let mut off = 0usize;
    append_bounded(&mut rec, &mut off, b"name=");
    append_bounded(&mut rec, &mut off, name.as_bytes());
    append_bounded(&mut rec, &mut off, b"\nversion=");
    append_bounded(&mut rec, &mut off, version);
    append_bounded(&mut rec, &mut off, b"\nuid=0\nperms=\nfiles=");
    append_bounded(&mut rec, &mut off, files);
    append_bounded(&mut rec, &mut off, b"\n");
    fsio::write_path(&p[..n], &rec[..off]);
}

fn fmt_u32_buf(v: u32) -> [u8; 12] {
    let mut b = [0u8; 12];
    let n = fmt_u32(v, &mut b);
    let mut out = [0u8; 12];
    out[..n].copy_from_slice(&b[..n]);
    out
}

/// leaf 公钥指纹（hex 前 16 字节）。
fn fp16(pk: &[u8; 32]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = trust::fingerprint(pk, &mut out);
    let mut r = [0u8; 16];
    r[..n].copy_from_slice(&out[..n]);
    r
}

fn fp_buf(pk: &[u8; 32]) -> [u8; 16] {
    fp16(pk)
}

/// 从 META-INF 取 leaf 证书主体（字符串）。
unsafe fn cert_subject(data: &[u8]) -> &'static [u8] {
    static mut SUBJ: [u8; 64] = [0; 64];
    static mut SUBJ_LEN: usize = 0;
    let pkg = match format::parse(data) {
        Some(p) => p,
        None => return b"",
    };
    let meta = pkg.meta(data);
    if let Some(c) = cert::parse(meta) {
        let n = c.subject.len().min(SUBJ.len());
        SUBJ[..n].copy_from_slice(&c.subject[..n]);
        SUBJ_LEN = n;
        &SUBJ[..SUBJ_LEN]
    } else {
        b""
    }
}

// ==================== 旧版 .pkg（NOVAPKG1）兼容 ====================

const LEGACY_MAGIC: [u8; 8] = *b"NOVAPKG1";
const LEGACY_SIG_LEN: usize = 32;
const LEGACY_NAME_MAX: usize = 28;

struct LegacyPkg {
    name: [u8; LEGACY_NAME_MAX],
    name_len: usize,
    elf_off: usize,
    elf_len: usize,
    man_off: usize,
    man_len: usize,
    res_off: usize,
    res_count: usize,
}

fn legacy_get_u32(b: &[u8], off: usize) -> u32 {
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

fn legacy_sign(data: &[u8]) -> [u8; LEGACY_SIG_LEN] {
    let mut out = [0u8; LEGACY_SIG_LEN];
    for i in 0..4 {
        let seed = 0xCBF2_9CE4_8422_2325u64.wrapping_add(i as u64 * 0x9E37_79B9_7F4A_7C15);
        let h = fnv1a(seed, data);
        out[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes());
    }
    out
}

fn legacy_valid_name(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > LEGACY_NAME_MAX {
        return false;
    }
    for &c in b {
        let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.';
        if !ok {
            return false;
        }
    }
    b != b"..".as_slice() && b != b".".as_slice()
}

fn legacy_parse(data: &[u8]) -> Result<LegacyPkg, i32> {
    if data.len() < 32 + LEGACY_SIG_LEN {
        return Err(-1);
    }
    if &data[0..8] != &LEGACY_MAGIC {
        return Err(-1);
    }
    let name_len = legacy_get_u32(data, 12) as usize;
    let elf_len = legacy_get_u32(data, 16) as usize;
    let man_len = legacy_get_u32(data, 20) as usize;
    let res_count = legacy_get_u32(data, 24) as usize;
    if name_len == 0 || name_len > LEGACY_NAME_MAX || res_count > 8 {
        return Err(-1);
    }
    let payload_end = data.len() - LEGACY_SIG_LEN;
    let mut off = 32usize;
    if off + name_len + elf_len + man_len > payload_end {
        return Err(-1);
    }
    let mut name = [0u8; LEGACY_NAME_MAX];
    name[..name_len].copy_from_slice(&data[off..off + name_len]);
    if !legacy_valid_name(&name[..name_len]) {
        return Err(-1);
    }
    off += name_len;
    let elf_off = off;
    off += elf_len;
    let man_off = off;
    off += man_len;
    let res_off = off;
    let mut ro = res_off;
    for _ in 0..res_count {
        if ro + 8 > payload_end {
            return Err(-1);
        }
        let rnl = legacy_get_u32(data, ro) as usize;
        let rdl = legacy_get_u32(data, ro + 4) as usize;
        ro += 8;
        if ro + rnl + rdl > payload_end {
            return Err(-1);
        }
        if rnl == 0 || rnl > 64 || !legacy_valid_name(&data[ro..ro + rnl]) {
            return Err(-1);
        }
        ro += rnl + rdl;
    }
    Ok(LegacyPkg { name, name_len, elf_off, elf_len, man_off, man_len, res_off, res_count })
}

fn legacy_verify(data: &[u8], _pkg: &LegacyPkg) -> bool {
    if data.len() < 32 + LEGACY_SIG_LEN {
        return false;
    }
    let sig_off = data.len() - LEGACY_SIG_LEN;
    let expected = &data[sig_off..sig_off + LEGACY_SIG_LEN];
    let computed = legacy_sign(&data[32..sig_off]);
    expected == &computed[..]
}

fn legacy_install(pkg: &LegacyPkg, data: &[u8]) -> isize {
    let name = core::str::from_utf8(&pkg.name[..pkg.name_len]).unwrap_or("");
    if !legacy_valid_name(name.as_bytes()) {
        return -1;
    }
    fsio::mkdir_path(b"/bin");
    fsio::mkdir_path(b"/pkg");
    fsio::mkdir_path(b"/res");
    let mut path = [0u8; 128];
    let n = fsio::path_join(&mut path, b"/bin/", name.as_bytes());
    if fsio::write_path(&path[..n], &data[pkg.elf_off..pkg.elf_off + pkg.elf_len]) != 0 {
        return -1;
    }
    let n = fsio::path_join3(&mut path, b"/pkg/", name.as_bytes(), b".mf");
    if fsio::write_path(&path[..n], &data[pkg.man_off..pkg.man_off + pkg.man_len]) != 0 {
        return -1;
    }
    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        let rnl = legacy_get_u32(data, off) as usize;
        off += 4;
        let rdl = legacy_get_u32(data, off) as usize;
        off += 4;
        let rname = &data[off..off + rnl];
        off += rnl;
        let rdata = &data[off..off + rdl];
        off += rdl;
        let dn = fsio::path_join(&mut path, b"/res/", name.as_bytes());
        fsio::mkdir_path(&path[..dn]);
        let fn0 = fsio::path_join3(&mut path, b"/res/", name.as_bytes(), b"/");
        let mut full = [0u8; 128];
        let flen = fsio::path_join(&mut full, &path[..fn0], rname);
        if fsio::write_path(&full[..flen], rdata) != 0 {
            return -1;
        }
    }
    0
}

fn legacy_build_files(pkg: &LegacyPkg, data: &[u8], out: &mut [u8]) -> usize {
    let name = core::str::from_utf8(&pkg.name[..pkg.name_len]).unwrap_or("");
    let nb = name.as_bytes();
    let mut n = 0usize;
    for &c in b"bin/" {
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
    let mut off = pkg.res_off;
    for _ in 0..pkg.res_count {
        let rnl = legacy_get_u32(data, off) as usize;
        off += 4;
        let rdl = legacy_get_u32(data, off) as usize;
        off += 4;
        if n < out.len() {
            out[n] = b',';
            n += 1;
        }
        for &c in b"res/" {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
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

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(argc: usize, argv: *const usize) -> usize {
    if argc < 2 {
        print("用法: pkg <update|install|install-file|remove|rollback|list|search|info|review|verify|perms|keys|sources>\n");
        return 1;
    }
    let sub = arg(argv, 1);
    unsafe {
        match sub {
            "update" => cmd_update(),
            "install" => {
                if argc < 3 {
                    print("用法: pkg install <name> [--yes]\n");
                    return 1;
                }
                let yes = has_flag(argv, argc, "--yes");
                cmd_install(arg(argv, 2), yes);
            }
            "install-file" => {
                if argc < 3 {
                    print("用法: pkg install-file <file.kunx> [--yes]\n");
                    return 1;
                }
                let yes = has_flag(argv, argc, "--yes");
                cmd_install_file(arg(argv, 2), yes);
            }
            "remove" => {
                if argc < 3 {
                    print("用法: pkg remove <name> [--keep-data]\n");
                    return 1;
                }
                let keep = has_flag(argv, argc, "--keep-data");
                cmd_remove(arg(argv, 2), keep);
            }
            "rollback" => {
                if argc < 3 {
                    print("用法: pkg rollback <name>\n");
                    return 1;
                }
                cmd_rollback(arg(argv, 2));
            }
            "list" => cmd_list(),
            "search" => {
                if argc < 3 {
                    print("用法: pkg search <keyword>\n");
                    return 1;
                }
                cmd_search(arg(argv, 2));
            }
            "info" => {
                if argc < 3 {
                    print("用法: pkg info <name>\n");
                    return 1;
                }
                cmd_info(arg(argv, 2));
            }
            "review" => {
                if argc < 3 {
                    print("用法: pkg review <file.kunx|name> [--approve]\n");
                    return 1;
                }
                let approve = has_flag(argv, argc, "--approve");
                cmd_review(arg(argv, 2), approve);
            }
            "verify" => {
                if argc < 3 {
                    print("用法: pkg verify <file.kunx|name>\n");
                    return 1;
                }
                cmd_verify(arg(argv, 2));
            }
            "perms" => {
                if argc < 3 {
                    print("用法: pkg perms <name> [grant|revoke <perm>]\n");
                    return 1;
                }
                let action = if argc >= 4 { arg(argv, 3) } else { "" };
                let perm = if argc >= 5 { arg(argv, 4) } else { "" };
                cmd_perms(arg(argv, 2), action, perm);
            }
            "keys" => cmd_keys(),
            "sources" => cmd_sources(),
            _ => {
                print("pkg: 未知命令 '");
                print(sub);
                print("'\n");
            }
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
