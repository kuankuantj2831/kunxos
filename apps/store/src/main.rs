// KunxOS 应用商店（Store，完整版）：no_std 裸 syscall 用户态程序。
//
// 连接 Cloudflare R2 对象存储（S3 兼容 API，AWS SigV4 手写签名）作为软件源：
//   1. R2 连接：GET Object（Packages.json 索引 / .kunx 包 / 索引签名），
//      SigV4 (HMAC-SHA256) 签名请求，复用天气应用的 HTTP+TCP 内核网络栈。
//   2. 索引同步：下载 Packages.json 并强制索引签名验证（Packages.json.sig，
//      Ed25519 REPO_ANCHOR）：验签通过才可安装并写本地缓存
//      /data/store/cache/index.bin（KXST2 头 + 时间戳，只存验签索引）；
//      签名缺失/不可用 → 索引仅可浏览，安装一律拒绝（fail-closed）；
//      签名不匹配 → 索引整体拒绝。缓存 TTL 12h，过期重拉，离线回退
//      过期缓存（缓存索引必已验签）。
//   3. 商店 UI：分类浏览（chips）、搜索框（物理键盘）、应用详情页、安装按钮、
//      更新检查（索引版本 vs 已安装版本）。
//   4. 下载管理：队列式下载（逐一处理）、进度显示、网络失败重试（1s/2s/4s 退避，
//      3 次）、断点续传预留（s3_head / Range 预留）。
//   5. 安装流程：下载 .kunx → 复用 kunxsec/kunxpkg 的 Ed25519 验签 + 证书链验证
//      （review::review，提示词 #31）→ 调用 /bin/pkg install-file 安装 → 完成。
//
// 配置：/data/store/config（endpoint / bucket / region / access_key / secret_key，
// key=value 每行一条；设置页可编辑）。多软件源：配置按「单源」设计，文件格式
// 天然支持后续扩展多源（预留 sources 目录）。https:// 端点当前内核无 TLS 栈被
// 明确拒绝（与 kunxpkg 同约定），需本地 S3/R2 兼容网关或内核 TLS 落地后启用。
//
// 后台下载（设计决策）：本版本下载为前台同步（与天气/云应用一致），安装/下载
// 期间保持首屏渲染，进度经 DL_PROGRESS 写入状态行；Home 后台化由 lifd
// SIGSTOP/SIGCONT 处理，恢复后自动重绘（launcher 帧快照）。真正由守护进程
// 接管的后台下载属后续工作——lifd 后台任务白名单已预留 BGT_DOWNLOAD=3，
// 队列与断点续传（s3_head + Range）已在本模块预留接口。
//
// 复用模块：
//   - kunxpkg/src/{json,format,trust,perms,cert,review,semver,sha512,ed25519,hex}.rs
//   - kunxsec/src/sha256.rs（SigV4 payload/签名哈希）
//   - rootfs 模式：/bin/pkg 负责实际安装（install-file）

#![no_std]
#![no_main]
#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

// ==================== 复用的加密/包/索引模块 ====================

#[path = "../../../kunxpkg/src/sha512.rs"]
mod sha512;
#[path = "../../../shared/motion.rs"]
mod motion;
#[path = "../../../kunxpkg/src/ed25519.rs"]
mod ed25519;
#[path = "../../../kunxpkg/src/json.rs"]
mod json;
#[path = "../../../kunxpkg/src/hex.rs"]
mod hex;
#[path = "../../../kunxpkg/src/format.rs"]
mod format;
#[path = "../../../kunxpkg/src/trust.rs"]
mod trust;
#[path = "../../../kunxpkg/src/perms.rs"]
mod perms;
#[path = "../../../kunxpkg/src/cert.rs"]
mod cert;
#[path = "../../../kunxpkg/src/review.rs"]
mod review;
#[path = "../../../kunxpkg/src/semver.rs"]
mod semver;
#[path = "../../../kunxsec/src/sha256.rs"]
mod sha256;

mod r2;
mod index;

// ==================== 共享 UI 模块 ====================

#[path = "../../../shared/ttf_text.rs"]
mod ttf_text;

#[path = "../../../shared/nav.rs"]
mod sys_nav;

#[path = "../../../shared/error_ui.rs"]
mod err_ui;

#[path = "../../../shared/cursor_overlay.rs"]
mod cursor_overlay;

#[path = "../../../shared/cjk_font_store.rs"]
mod cjk_font;

// ==================== 系统调用号（AArch64 asm-generic） ====================

const SYS_READ: usize = 63;
const SYS_WRITE: usize = 64;
const SYS_OPENAT: usize = 56;
const SYS_CLOSE: usize = 57;
const SYS_LSEEK: usize = 62;
const SYS_FTRUNCATE: usize = 46;
const SYS_MKDIRAT: usize = 34;
const SYS_UNLINKAT: usize = 35;
const SYS_RENAMEAT: usize = 38;
const SYS_EXIT: usize = 93;
const SYS_NANOSLEEP: usize = 101;
const SYS_GETTIMEOFDAY: usize = 169;
const SYS_MMAP: usize = 222;
const SYS_IOCTL: usize = 29;
const SYS_CLONE: usize = 220;
const SYS_EXECVE: usize = 221;
const SYS_WAIT4: usize = 260;

const AT_FDCWD: usize = 0xFFFF_FFFF_FFFF_FF9C;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0x40;
const O_TRUNC: usize = 0x200;
const O_WRONLY: usize = 1;
const O_NONBLOCK: usize = 0x800;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;

/// FBIO_PRESENT 请求码：把 canvas 脏区拷入扫描缓冲并翻页（应用驱动显示节奏）。
const FBIO_PRESENT: usize = 0x4603;

const APP_NAME: &str = "store";

// ==================== 路径常量 ====================

const STORE_DIR: &str = "/data/store";
const CFG_FILE: &str = "/data/store/config";
const CACHE_DIR: &str = "/data/store/cache";
const IDX_CACHE: &str = "/data/store/cache/index.bin";
const AVAILABLE_PATH: &str = "/var/lib/pkg/available";
const INSTALLED_DIR: &str = "/var/lib/pkg/installed";
const DOWNLOAD_DIR: &str = "/data/store/cache";

/// 索引缓存 TTL（秒）：12 小时。
const INDEX_TTL: u64 = 12 * 3600;
/// 网络重试次数（索引/包下载）。
const RETRY_MAX: u32 = 3;
/// 索引缓冲（Packages.json ≤ 32KB）。
const IDX_MAX: usize = 32768;
/// 包缓冲（.kunx ≤ 168KB，与 kunxpkg PKG_BUF 同级）。
const PKG_MAX: usize = 168 * 1024;

// ==================== 全局缓冲 ====================

static mut PATH_BUF: [u8; 128] = [0u8; 128];
static mut MKDIR_BUF: [u8; 96] = [0u8; 96];
static mut FILE_BUF: [u8; 2048] = [0u8; 2048];
static mut IDX_BUF: [u8; IDX_MAX] = [0u8; IDX_MAX];
static mut SIG_BUF: [u8; 96] = [0u8; 96];
static mut PKG_BUF: [u8; PKG_MAX] = [0u8; PKG_MAX];

// ==================== 配置（/data/store/config） ====================

/// 设置字段索引（settings 编辑页使用）。
const FIELD_ENDPOINT: usize = 0;
const FIELD_BUCKET: usize = 1;
const FIELD_REGION: usize = 2;
const FIELD_ACCESS: usize = 3;
const FIELD_SECRET: usize = 4;
const FIELD_MAX: usize = 5;

static mut SET_VALS: [[u8; 96]; FIELD_MAX] = [[0; 96]; FIELD_MAX];
static mut SET_LENS: [usize; FIELD_MAX] = [0; FIELD_MAX];
static mut CFG_VALID: bool = false;
static mut CFG_TLS: bool = false;

/// 组装一个 r2::R2Config（从当前 SET_VALS 拷贝）。
fn build_r2_cfg() -> r2::R2Config {
    unsafe {
        let mut c = r2::R2Config::empty();
        let ep = &SET_VALS[FIELD_ENDPOINT];
        let el = SET_LENS[FIELD_ENDPOINT];
        // 端点：可选 http:// 或 https:// 前缀
        let mut h = &ep[..el];
        c.tls_only = false;
        if el >= 8 && &ep[..8] == b"https://" {
            c.tls_only = true;
            h = &ep[8..el];
        } else if el >= 7 && &ep[..7] == b"http://" {
            h = &ep[7..el];
        }
        // host[:port]
        let mut split = h;
        let mut port: u16 = 80;
        for i in 0..h.len() {
            if h[i] == b':' {
                split = &h[..i];
                let p = &h[i + 1..];
                let mut v: u32 = 0;
                let mut any = false;
                for &d in p {
                    if !d.is_ascii_digit() {
                        break;
                    }
                    v = v * 10 + (d - b'0') as u32;
                    any = true;
                }
                if any && v <= 65535 && v > 0 {
                    port = v as u16;
                }
                break;
            }
        }
        let hc = split.len().min(c.host.len());
        c.host[..hc].copy_from_slice(&split[..hc]);
        c.host_len = hc;
        c.port = port;
        let b = &SET_VALS[FIELD_BUCKET];
        let bl = SET_LENS[FIELD_BUCKET].min(c.bucket.len());
        c.bucket[..bl].copy_from_slice(&b[..bl]);
        c.bucket_len = bl;
        let r = &SET_VALS[FIELD_REGION];
        let rl = SET_LENS[FIELD_REGION].min(c.region.len());
        c.region[..rl].copy_from_slice(&r[..rl]);
        c.region_len = rl;
        let ak = &SET_VALS[FIELD_ACCESS];
        let akl = SET_LENS[FIELD_ACCESS].min(c.access_key.len());
        c.access_key[..akl].copy_from_slice(&ak[..akl]);
        c.access_key_len = akl;
        let sk = &SET_VALS[FIELD_SECRET];
        let skl = SET_LENS[FIELD_SECRET].min(c.secret_key.len());
        c.secret_key[..skl].copy_from_slice(&sk[..skl]);
        c.secret_key_len = skl;
        c
    }
}

/// 读取配置（解析 key=value 行）。
fn load_config() -> bool {
    unsafe {
        CFG_VALID = false;
        for i in 0..FIELD_MAX {
            SET_LENS[i] = 0;
        }
        let n = read_file(CFG_FILE, &mut FILE_BUF);
        if n <= 0 {
            return false;
        }
        let b = &FILE_BUF[..n as usize];
        let mut ls = 0usize;
        while ls < b.len() {
            let mut le = ls;
            while le < b.len() && b[le] != b'\n' {
                le += 1;
            }
            let line = &b[ls..le];
            ls = if le < b.len() { le + 1 } else { b.len() };
            if let Some(i) = line.iter().position(|&c| c == b'=') {
                let k = &line[..i];
                let v = trim(&line[i + 1..]);
                let f = match k {
                    b"endpoint" => FIELD_ENDPOINT,
                    b"bucket" => FIELD_BUCKET,
                    b"region" => FIELD_REGION,
                    b"access_key" => FIELD_ACCESS,
                    b"secret_key" => FIELD_SECRET,
                    _ => continue,
                };
                let c = v.len().min(SET_VALS[f].len());
                SET_VALS[f][..c].copy_from_slice(&v[..c]);
                SET_LENS[f] = c;
            }
        }
        CFG_VALID = true;
        CFG_TLS = SET_LENS[FIELD_ENDPOINT] >= 8
            && &SET_VALS[FIELD_ENDPOINT][..8] == b"https://";
        true
    }
}

/// 保存配置（覆盖写）。
fn save_config() -> bool {
    unsafe {
        let mut out = FILE_BUF;
        let mut o = 0usize;
        let lines: [(&[u8], usize); FIELD_MAX] = [
            (b"endpoint", FIELD_ENDPOINT),
            (b"bucket", FIELD_BUCKET),
            (b"region", FIELD_REGION),
            (b"access_key", FIELD_ACCESS),
            (b"secret_key", FIELD_SECRET),
        ];
        for (k, f) in lines {
            if o < out.len() {
                out[o] = 0;
            }
            for &c in k {
                if o < out.len() {
                    out[o] = c;
                    o += 1;
                }
            }
            out[o] = b'=';
            o += 1;
            let v = &SET_VALS[..];
            let _ = v;
            for &c in &SET_VALS[f][..SET_LENS[f]] {
                if o < out.len() {
                    out[o] = c;
                    o += 1;
                }
            }
            out[o] = b'\n';
            o += 1;
        }
        CFG_VALID = true;
        write_file(CFG_FILE, &out[..o])
    }
}

// ==================== 基础工具 ====================

#[inline(always)]
unsafe fn syscall1(n: usize, a1: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall2(n: usize, a1: usize, a2: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret, in("a1") a2, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(n: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a1 => ret,
        in("a1") a2,
        in("a2") a3,
        options(nostack)
    );
    ret
}

#[inline(always)]
unsafe fn syscall4(n: usize, a1: usize, a2: usize, a3: usize, a4: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall5(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall6(n: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) -> usize {
    let ret: usize;
    asm!("ecall", in("a7") n, inlateout("a0") a1 => ret,
        in("a1") a2, in("a2") a3, in("a3") a4, in("a4") a5, in("a5") a6, options(nostack));
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

fn print_dec(mut v: u32) {
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

fn sleep_ms(ms: usize) {
    let mut req = [0u8; 16];
    let sec = (ms / 1000) as u64;
    let nsec = ((ms % 1000) * 1_000_000) as u64;
    req[..8].copy_from_slice(&sec.to_le_bytes());
    req[8..].copy_from_slice(&nsec.to_le_bytes());
    unsafe {
        syscall2(SYS_NANOSLEEP, req.as_ptr() as usize, 0);
    }
}

/// 当前时间（微秒，UTC epoch；不可用返回 0）。
fn now_us() -> u64 {
    let mut tv = [0u8; 16];
    unsafe {
        let r = syscall2(SYS_GETTIMEOFDAY, tv.as_mut_ptr() as usize, 0);
        if (r as isize) < 0 {
            return 0;
        }
        let sec = u64::from_le_bytes([tv[0], tv[1], tv[2], tv[3], tv[4], tv[5], tv[6], tv[7]]);
        let usec =
            u64::from_le_bytes([tv[8], tv[9], tv[10], tv[11], tv[12], tv[13], tv[14], tv[15]]);
        sec * 1_000_000 + usec
    }
}

unsafe fn open(path: &str, flags: usize) -> isize {
    let n = path.len().min(PATH_BUF.len() - 1);
    PATH_BUF[..n].copy_from_slice(&path.as_bytes()[..n]);
    PATH_BUF[n] = 0;
    syscall3(SYS_OPENAT, AT_FDCWD, PATH_BUF.as_ptr() as usize, flags) as isize
}

static mut PATH_BUF2: [u8; 128] = [0u8; 128];

fn mkdir(path: &str) -> isize {
    unsafe {
        let n = path.len().min(MKDIR_BUF.len() - 1);
        MKDIR_BUF[..n].copy_from_slice(path.as_bytes());
        MKDIR_BUF[n] = 0;
        syscall3(SYS_MKDIRAT, AT_FDCWD, MKDIR_BUF.as_ptr() as usize, 0o755) as isize
    }
}

/// 读取整个文件到 buf，返回长度；失败 ≤ 0。
fn read_file(path: &str, buf: &mut [u8]) -> isize {
    unsafe {
        let fd = open(path, O_RDONLY);
        if fd < 0 {
            return -1;
        }
        let mut total = 0usize;
        while total < buf.len() {
            let n = syscall3(
                SYS_READ,
                fd as usize,
                buf.as_mut_ptr().add(total) as usize,
                buf.len() - total,
            ) as isize;
            if n <= 0 {
                break;
            }
            total += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        total as isize
    }
}

/// 写整个文件（O_CREAT|O_TRUNC|O_WRONLY）。
fn write_file(path: &str, data: &[u8]) -> bool {
    unsafe {
        let fd = open(path, O_CREAT | O_TRUNC | O_WRONLY);
        if fd < 0 {
            return false;
        }
        let mut off = 0usize;
        while off < data.len() {
            let n = syscall3(
                SYS_WRITE,
                fd as usize,
                data.as_ptr().add(off) as usize,
                data.len() - off,
            ) as isize;
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
        off == data.len()
    }
}

fn unlink(path: &str) {
    unsafe {
        let n = path.len().min(PATH_BUF2.len() - 1);
        PATH_BUF2[..n].copy_from_slice(path.as_bytes());
        PATH_BUF2[n] = 0;
        syscall3(SYS_UNLINKAT, AT_FDCWD, PATH_BUF2.as_ptr() as usize, 0);
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && (b[s] == b' ' || b[s] == b'\t' || b[s] == b'\r' || b[s] == b'\n') {
        s += 1;
    }
    while e > s && (b[e - 1] == b' ' || b[e - 1] == b'\t' || b[e - 1] == b'\r' || b[e - 1] == b'\n') {
        e -= 1;
    }
    &b[s..e]
}

fn starts_with(a: &[u8], p: &[u8]) -> bool {
    a.len() >= p.len() && a[..p.len()] == *p
}

fn find_sub(data: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || data.len() < needle.len() {
        return None;
    }
    let mut i = 0;
    while i + needle.len() <= data.len() {
        if &data[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn fmt_dec(mut v: u64, buf: &mut [u8]) -> usize {
    if v == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut pos = buf.len();
    while v > 0 {
        pos -= 1;
        buf[pos] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    let n = buf.len() - pos;
    buf.copy_within(pos.., 0);
    n
}

// ==================== 已安装数据库（更新检查） ====================

static mut INST_VER: [u8; 32] = [0u8; 32];
static mut INST_VER_LEN: usize = 0;

/// 读取 /var/lib/pkg/installed/<name> 的 version= 字段。
fn installed_version(name: &[u8]) -> &'static [u8] {
    unsafe {
        INST_VER_LEN = 0;
        let mut p = [0u8; 128];
        let mut n = 0usize;
        for &c in INSTALLED_DIR.as_bytes() {
            if n < p.len() {
                p[n] = c;
                n += 1;
            }
        }
        p[n] = b'/';
        n += 1;
        for &c in name {
            if n < p.len() - 1 {
                p[n] = c;
                n += 1;
            }
        }
        // name 来自网络索引/目录数据，可能非 UTF-8：安全转换（UB 防护）
        let path = core::str::from_utf8(&p[..n]).unwrap_or("");
        let r = read_file(path, &mut FILE_BUF);
        if r <= 0 {
            return &[];
        }
        let b = &FILE_BUF[..r as usize];
        let mut ls = 0usize;
        while ls < b.len() {
            let mut le = ls;
            while le < b.len() && b[le] != b'\n' {
                le += 1;
            }
            let line = &b[ls..le];
            ls = if le < b.len() { le + 1 } else { b.len() };
            if line.len() > 8 && &line[..8] == b"version=" {
                let v = trim(&line[8..]);
                let c = v.len().min(INST_VER.len());
                INST_VER[..c].copy_from_slice(&v[..c]);
                INST_VER_LEN = c;
                return &INST_VER[..c];
            }
        }
        &[]
    }
}

/// 安装状态：0 未安装，1 已安装同版本，2 有新版可更新。
fn pkg_install_state(p: &index::StorePkg) -> u8 {
    let iv = installed_version(&p.name[..p.name_len]);
    if iv.is_empty() {
        return 0;
    }
    match semver::parse(iv).and_then(|a| semver::parse(&p.version[..p.version_len]).map(|b| (a, b))) {
        Some((a, b)) => {
            let c = semver::cmp(&b, &a);
            if c > 0 {
                2
            } else {
                1
            }
        }
        None => 1,
    }
}

/// 拷贝索引记录 [start,end) 内 `key` 字段的值到 `out`，返回长度；无此字段 0。
fn field_copy(buf: &[u8], start: usize, end: usize, key: &str, out: &mut [u8]) -> usize {
    let kb = key.as_bytes();
    let mut ls = start;
    while ls < end {
        let mut le = ls;
        while le < end && buf[le] != b'\n' {
            le += 1;
        }
        let line = &buf[ls..le];
        if line.len() > kb.len() && &line[..kb.len()] == kb && line[kb.len()] == b':' {
            let val = trim(&line[kb.len() + 1..]);
            let c = val.len().min(out.len());
            out[..c].copy_from_slice(&val[..c]);
            return c;
        }
        ls = if le < end { le + 1 } else { end };
    }
    0
}

/// 设置用户可见错误（自动消失）。
fn show_error(msg: &[u8]) {
    err_ui::err_set(msg, now_us());
}

// ==================== 本地可用索引回退（R2 未配置/失败时） ====================

/// 从 /var/lib/pkg/available（Debian 风格）填充 index 模型，标记本地来源。
fn load_local_catalog() -> bool {
    unsafe {
        index::reset();
        let n = read_file(AVAILABLE_PATH, &mut FILE_BUF);
        if n <= 0 {
            return false;
        }
        let b = &FILE_BUF[..n as usize];
        let mut ls = 0usize;
        while ls < b.len() && index::count() < index::MAX_PKGS {
            // 找 Package: 行
            while ls < b.len()
                && !(b[ls] == b'P' && b.len() - ls >= 8 && &b[ls..ls + 8] == b"Package:")
            {
                while ls < b.len() && b[ls] != b'\n' {
                    ls += 1;
                }
                if ls < b.len() {
                    ls += 1;
                }
            }
            if ls >= b.len() {
                break;
            }
            let rs = ls;
            let mut r = ls;
            while r < b.len() {
                if b[r] == b'\n'
                    && (r + 1 >= b.len()
                        || b[r + 1] == b'\n'
                        || (b.len() - (r + 1) >= 8 && &b[r + 1..r + 9] == b"Package:"))
                {
                    break;
                }
                while r < b.len() && b[r] != b'\n' {
                    r += 1;
                }
                if r < b.len() {
                    r += 1;
                }
            }
            let re = r;
            // 拷贝字段到 index 模型
            let mut p = index::StorePkg::new();
            let mut name = [0u8; 64];
            let nm = field_copy(b, rs, re, "Package", &mut name);
            p.name_len = name[..nm].len().min(p.name.len());
            p.name[..p.name_len].copy_from_slice(&name[..nm.min(32)]);
            let mut ver = [0u8; 40];
            let vm = field_copy(b, rs, re, "Version", &mut ver);
            p.version_len = vm.min(p.version.len());
            p.version[..p.version_len].copy_from_slice(&ver[..vm.min(16)]);
            let mut desc = [0u8; 128];
            let dm = field_copy(b, rs, re, "Description", &mut desc);
            p.desc_len = dm.min(p.desc.len());
            p.desc[..p.desc_len].copy_from_slice(&desc[..dm.min(96)]);
            let mut perm = [0u8; 96];
            let pm = field_copy(b, rs, re, "Permissions", &mut perm);
            p.perm_count = 0;
            let mut i = 0usize;
            while i < pm && p.perm_count < 6 {
                while i < pm && perm[i] == b',' {
                    i += 1;
                }
                let s = i;
                while i < pm && perm[i] != b',' {
                    i += 1;
                }
                let x = &perm[s..i];
                if !x.is_empty() {
                    let c = x.len().min(16);
                    p.perms[p.perm_count][..c].copy_from_slice(&x[..c]);
                    p.perm_lens[p.perm_count] = c;
                    p.perm_count += 1;
                }
            }
            let cat = b"\xE6\x9C\xAC\xE5\x9C\xB0".as_ref(); // 「本地」
            p.category_len = cat.len().min(16);
            p.category[..cat.len().min(16)].copy_from_slice(&cat[..cat.len().min(16)]);
            index::push_local(p);
            ls = if re > rs { re } else { re + 1 };
        }
        index::count() > 0
    }
}

// ==================== R2 索引同步 ====================

const IDX_FROM_R2: u8 = 1;
const IDX_FROM_LOCAL: u8 = 2;
static mut IDX_SRC: u8 = 0;
static mut IDX_TS: u64 = 0; // 缓存时间戳
/// 当前索引是否经 Ed25519 签名验证。只有本次在线拉取验签通过、或加载了
/// 「验签后才写入」的 KXST2 缓存才为 true；未签名索引仅可浏览，安装一律
/// 拒绝（install_app 入口强制检查）。
static mut IDX_SIG_OK: bool = false;

/// 保存索引缓存：`KXST2 <unix>` + JSON body。
///
/// 头版本从 KXST1 升到 KXST2：旧缓存可能来自「未签名照收」时代的索引，
/// 一律作废（读取端只认 KXST2），首次重新在线拉取。只有验签通过的索引
/// 才会写入缓存（调用点保证），由此维持「缓存内索引必已验签」的不变量。
fn index_cache_save(ts: u64, body: &[u8]) {
    unsafe {
        let mut hdr = [0u8; 32];
        let mut n = 0usize;
        hdr[n..n + 6].copy_from_slice(b"KXST2 ");
        n += 6;
        let mut num = [0u8; 20];
        let nl = fmt_dec(ts, &mut num);
        hdr[n..n + nl].copy_from_slice(&num[..nl]);
        n += nl;
        hdr[n] = b'\n';
        n += 1;
        // 写头（缓存文件 = 头 + body；分两步写，避免超大栈拼接）
        let fd = open(IDX_CACHE, O_CREAT | O_TRUNC | O_WRONLY);
        if fd < 0 {
            return;
        }
        let mut off = 0usize;
        while off < n {
            let w = syscall3(SYS_WRITE, fd as usize, hdr.as_ptr().add(off) as usize, n - off) as isize;
            if w <= 0 {
                break;
            }
            off += w as usize;
        }
        let bl = body.len().min(IDX_MAX);
        let mut boff = 0usize;
        while boff < bl {
            let w = syscall3(
                SYS_WRITE,
                fd as usize,
                body.as_ptr().add(boff) as usize,
                bl - boff,
            ) as isize;
            if w <= 0 {
                break;
            }
            boff += w as usize;
        }
        syscall1(SYS_CLOSE, fd as usize);
    }
}

/// 读取缓存，body 前移回 IDX_BUF。返回 (ok, ts, body_len)。
/// 只认 KXST2（验签后才写入的缓存）；KXST1 旧缓存视为不存在。
fn index_cache_load() -> (bool, u64, usize) {
    unsafe {
        let n = read_file(IDX_CACHE, &mut IDX_BUF);
        if n <= 0 {
            return (false, 0, 0);
        }
        let b = &IDX_BUF[..n as usize];
        if !starts_with(b, b"KXST2 ") {
            return (false, 0, 0);
        }
        let mut ts: u64 = 0;
        let mut j = 6usize;
        let mut any = false;
        while j < b.len() && b[j] >= b'0' && b[j] <= b'9' {
            ts = ts.saturating_mul(10).saturating_add((b[j] - b'0') as u64);
            j += 1;
            any = true;
        }
        if !any || j >= b.len() || b[j] != b'\n' {
            return (false, 0, 0);
        }
        let bl = b.len() - j - 1;
        IDX_BUF.copy_within(j + 1.., 0);
        (true, ts, bl)
    }
}

fn r2_err_hint(code: isize) -> &'static [u8] {
    match code {
        r2::R2_ERR_TLS => "R2 需要 HTTPS（内核暂无 TLS 栈）".as_bytes(),
        r2::R2_ERR_CONFIG => "R2 配置缺失，请到设置页配置".as_bytes(),
        r2::R2_ERR_TIME => "系统时间不可用，无法签名".as_bytes(),
        r2::R2_ERR_RESOLVE => "R2 端点域名解析失败".as_bytes(),
        r2::R2_ERR_CONNECT => "无法连接 R2 端点".as_bytes(),
        r2::R2_ERR_SEND => "请求发送失败".as_bytes(),
        r2::R2_ERR_RECV => "响应接收失败".as_bytes(),
        r2::R2_ERR_AUTH => "R2 凭证错误（AccessKey/SecretKey）".as_bytes(),
        r2::R2_ERR_NOTFOUND => "对象不存在（404）".as_bytes(),
        r2::R2_ERR_BIG => "响应超出缓冲".as_bytes(),
        r2::R2_ERR_BADRESP => "响应格式错误".as_bytes(),
        _ => "R2 请求失败".as_bytes(),
    }
}

/// 判断错误是否可重试（网络层错误，而非凭证/404/HTTP 语义错误）。
fn r2_retryable(code: isize) -> bool {
    matches!(
        code,
        r2::R2_ERR_RESOLVE | r2::R2_ERR_CONNECT | r2::R2_ERR_SEND | r2::R2_ERR_RECV
    )
}

/// 拉取 Packages.json 并做索引签名验证（fail-closed），成功返回 body 长度
/// （写入 IDX_BUF）。失败返回负错误码。
///
/// 签名策略：`Packages.json.sig` 存在且 Ed25519 验签通过 → 索引可信
/// （`IDX_SIG_OK = true`，可安装）。签名缺失（404）、拉取失败或长度异常
/// → 索引仅可浏览，`IDX_SIG_OK` 保持 false，安装一律拒绝。签名不匹配
/// → 整个索引拒绝（返回 -50），不进缓存。
unsafe fn index_fetch_r2() -> isize {
    let cfg = build_r2_cfg();
    let mut status = 0i32;
    // 带重试（网络层错误退避 1s/2s/4s）
    let mut attempt = 0u32;
    loop {
        let r = r2::s3_get(&cfg, b"Packages.json", &mut IDX_BUF, &mut status);
        if r > 0 {
            // 拉索引签名（必需：缺失/不可用即禁止安装，不能仅告警照收）
            let sig_r = r2::s3_get(&cfg, b"Packages.json.sig", &mut SIG_BUF, &mut status);
            if sig_r == 64 {
                let sig: [u8; 64] = SIG_BUF[..64].try_into().unwrap();
                if !ed25519::verify(&trust::REPO_ANCHOR, &IDX_BUF[..r as usize], &sig) {
                    print("[store] 索引签名不匹配\n");
                    return -50;
                }
                print("[store] 索引签名校验通过\n");
                IDX_SIG_OK = true;
            } else if sig_r == r2::R2_ERR_NOTFOUND {
                print("[store] 索引未签名（无 Packages.json.sig），仅可浏览，禁止安装\n");
            } else {
                // 网络错误拉签名失败 / 签名长度异常（≠64）：同样按未签名
                // 处理（禁止安装），不能默默照收。
                print("[store] 索引签名不可用，仅可浏览，禁止安装\n");
            }
            return r;
        }
        if !r2_retryable(r) || attempt + 1 >= RETRY_MAX {
            return r;
        }
        attempt += 1;
        let backoff: usize = if attempt == 1 { 1000 } else { 2000 };
        print("[store] 索引拉取重试 ");
        print_dec(attempt);
        print("，等待 ");
        print_dec(backoff as u32);
        print("ms 后重试\n");
        sleep_ms(backoff);
    }
}

/// 加载索引：新鲜缓存 → 用之；否则拉 R2；失败回退过期缓存/本地目录。
/// 返回 0 新数据，1 过期缓存，2 本地目录，3 无数据。
///
/// IDX_SIG_OK 不变量：缓存（KXST2）只存验签通过的索引，因此两条缓存路径
/// 均置 true；网络路径由 index_fetch_r2 按验签结果置位；本地目录不涉及
/// Packages.json.sig（安装走 pkg 自身的包级验签）。
fn index_load() -> u8 {
    unsafe {
        ensure_dirs();
        index::reset();
        IDX_SIG_OK = false;
        let now = now_us() / 1_000_000;
        // 1. 新鲜缓存
        let (ok, ts, bl) = index_cache_load();
        if ok && ts != 0 && now >= ts && now - ts < INDEX_TTL {
            if index::parse(&IDX_BUF[..bl]) {
                IDX_SRC = IDX_FROM_R2;
                IDX_TS = ts;
                IDX_SIG_OK = true;
                print("[store] 索引缓存命中 ts=");
                print_dec(ts as u32);
                print("\n");
                return 0;
            }
        }
        // 2. 网络拉取
        let cfg = build_r2_cfg();
        if cfg.ready() {
            let r = index_fetch_r2();
            if r > 0 {
                let m = r as usize;
                if index::parse(&IDX_BUF[..m]) {
                    IDX_SRC = IDX_FROM_R2;
                    IDX_TS = now;
                    // 只缓存验签通过的索引；未签名索引仅本会话内浏览。
                    if IDX_SIG_OK {
                        index_cache_save(now, &IDX_BUF[..m]);
                    }
                    print("[store] 索引已获取，包数=");
                    print_dec(index::count() as u32);
                    print("\n");
                    return 0;
                }
                print("[store] 索引解析失败\n");
            } else {
                print("[store] 索引拉取失败（");
                print_dec((-r) as u32);
print("）\n");
                show_error(r2_err_hint(r));
            }
        }
        // 3. 过期缓存回退
        let (ok, ts, bl) = index_cache_load();
        if ok && bl > 0 {
            index::reset();
            if index::parse(&IDX_BUF[..bl]) {
                IDX_SRC = IDX_FROM_R2;
                IDX_TS = ts;
                IDX_SIG_OK = true;
                print("[store] 索引过期，回退使用\n");
                return 1;
            }
        }
        // 4. 本地目录回退
        if load_local_catalog() {
            IDX_SRC = IDX_FROM_LOCAL;
            print("[store] 回退本地软件目录\n");
            return 2;
        }
        print("[store] 无可用索引\n");
        3
    }
}

fn ensure_dirs() {
    let _ = mkdir(STORE_DIR);
    let _ = mkdir(CACHE_DIR);
    let _ = mkdir(DOWNLOAD_DIR);
}

// ==================== 下载管理 ====================

static mut DL_PROGRESS: u32 = 0;
static mut DL_TOTAL: u32 = 0;
static mut DL_ACTIVE: bool = false;

/// 下载 .kunx 包到缓存目录，返回 (写到文件的字节数)。失败 ≤ 0。
/// 带重试（网络错误退避）；进度写入 DL_PROGRESS。
fn download_pkg_to_cache(cfg: &r2::R2Config, key: &[u8], out_path: &str) -> isize {
    unsafe {
        let fd = open(out_path, O_CREAT | O_TRUNC | O_WRONLY);
        if fd < 0 {
            return -2;
        }
        let mut status = 0i32;
        let mut attempt = 0u32;
        let r = loop {
            DL_PROGRESS = 0;
            let r = r2::s3_download(cfg, key, fd, &raw mut DL_PROGRESS, &mut status);
            if r > 0 || !r2_retryable(r) || attempt + 1 >= RETRY_MAX {
                break r;
            }
            attempt += 1;
            let backoff: usize = if attempt == 1 { 1000 } else { 2000 };
            print("[store] 下载重试 ");
            print_dec(attempt);
            print("\n");
            // 重试时从文件头重写（无断点续传；续传由 s3_head + Range 预留）。
            // 必须同时截断：上一次尝试的残留尾部（短包）会让整包验签失败。
            let _ = syscall3(SYS_LSEEK, fd as usize, 0, 0);
            syscall3(SYS_FTRUNCATE, fd as usize, 0, 0);
            sleep_ms(backoff);
        };
        syscall1(SYS_CLOSE, fd as usize);
        r
    }
}

// ==================== 安装流程 ====================

/// 运行 /bin/pkg install-file <path> --yes；成功返回 true。
fn run_pkg_file(path: &str) -> bool {
    unsafe {
        let mut cmd = [0u8; 40];
        let mut n = 0usize;
        for &c in b"/bin/pkg" {
            cmd[n] = c;
            n += 1;
        }
        cmd[n] = 0;
        let mut sub = [0u8; 40];
        let mut sn = 0usize;
        for &c in b"install-file" {
            sub[sn] = c;
            sn += 1;
        }
        sub[sn] = 0;
        let mut fl = [0u8; 16];
        fl[..5].copy_from_slice(b"--yes");
        fl[5] = 0;
        let mut p = [0u8; 128];
        let pn = path.len().min(p.len() - 1);
        p[..pn].copy_from_slice(&path.as_bytes()[..pn]);
        p[pn] = 0;
        let argv = [
            cmd.as_ptr() as usize,
            sub.as_ptr() as usize,
            fl.as_ptr() as usize,
            p.as_ptr() as usize,
            0,
        ];
        let pid = syscall5(SYS_CLONE, 17, 0, 0, 0, 0) as isize;
        if pid == 0 {
            syscall3(SYS_EXECVE, cmd.as_ptr() as usize, argv.as_ptr() as usize, 0);
            syscall1(SYS_EXIT, 127);
            loop {}
        }
        if pid > 0 {
            let mut st = 0i32;
            syscall4(SYS_WAIT4, pid as usize, &mut st as *mut i32 as usize, 0, 0);
            let code = ((st >> 8) & 0xff) as u8;
            code == 0
        } else {
            false
        }
    }
}

/// 运行 /bin/pkg install <name> --yes（本地目录回退路径）。
fn run_pkg_by_name(name: &[u8]) -> bool {
    unsafe {
        let mut cmd = [0u8; 40];
        let mut n = 0usize;
        for &c in b"/bin/pkg" {
            cmd[n] = c;
            n += 1;
        }
        cmd[n] = 0;
        let mut sub = [0u8; 24];
        let mut sn = 0usize;
        for &c in b"install" {
            sub[sn] = c;
            sn += 1;
        }
        sub[sn] = 0;
        let mut fl = [0u8; 16];
        fl[..5].copy_from_slice(b"--yes");
        fl[5] = 0;
        let mut nm = [0u8; 40];
        let nn = name.len().min(nm.len() - 1);
        nm[..nn].copy_from_slice(&name[..nn]);
        nm[nn] = 0;
        let argv = [
            cmd.as_ptr() as usize,
            sub.as_ptr() as usize,
            fl.as_ptr() as usize,
            nm.as_ptr() as usize,
            0,
        ];
        let pid = syscall5(SYS_CLONE, 17, 0, 0, 0, 0) as isize;
        if pid == 0 {
            syscall3(SYS_EXECVE, cmd.as_ptr() as usize, argv.as_ptr() as usize, 0);
            syscall1(SYS_EXIT, 127);
            loop {}
        }
        if pid > 0 {
            let mut st = 0i32;
            syscall4(SYS_WAIT4, pid as usize, &mut st as *mut i32 as usize, 0, 0);
            let code = ((st >> 8) & 0xff) as u8;
            code == 0
        } else {
            false
        }
    }
}

/// 完整安装流程：下载 → 验签（Ed25519 + 证书链）→ pkg install-file。
/// idx = index 中的包下标。
fn install_app(idx: usize) -> bool {
    unsafe {
        let p = index::pkg(idx);
        let cfg = build_r2_cfg();
        if IDX_SRC == IDX_FROM_LOCAL {
            print("[store] 本地软件目录按名称安装\n");
            let ok = run_pkg_by_name(&p.name[..p.name_len]);
            if ok {
                status("已安装".as_bytes());
            } else {
                status("安装失败".as_bytes());
                show_error("安装失败，请稍后重试".as_bytes());
            }
            return ok;
        }
        if !cfg.ready() {
            show_error("请先在设置页配置 R2 软件源".as_bytes());
            return false;
        }
        // 索引未验签（Packages.json.sig 缺失/不可用）→ 明确阻止安装：
        // 索引可浏览，但绝不从不可信索引起草下载与安装（fail-closed）。
        if !IDX_SIG_OK {
            status("索引未签名".as_bytes());
            show_error("软件源索引未签名（缺 Packages.json.sig），已阻止安装".as_bytes());
            print("[store] 拒绝安装：索引未签名\n");
            return false;
        }
        // 构造对象键：优先索引的 filename，否则 packages/<name>-<version>.kunx
        let mut key = [0u8; 128];
        let mut kn = 0usize;
        if p.filename_len > 0 {
            let fc = p.filename_len.min(key.len());
            key[..fc].copy_from_slice(&p.filename[..fc]);
            kn = fc;
        } else {
            for &c in b"packages/" {
                if kn < key.len() {
                    key[kn] = c;
                    kn += 1;
                }
            }
            for &c in &p.name[..p.name_len] {
                if kn < key.len() {
                    key[kn] = c;
                    kn += 1;
                }
            }
            if kn < key.len() {
                key[kn] = b'-';
                kn += 1;
            }
            for &c in &p.version[..p.version_len] {
                if kn < key.len() {
                    key[kn] = c;
                    kn += 1;
                }
            }
            for &c in b".kunx" {
                if kn < key.len() {
                    key[kn] = c;
                    kn += 1;
                }
            }
        }

        // 下载到缓存
        let mut path = [0u8; 160];
        let mut pth = 0usize;
        for &c in DOWNLOAD_DIR.as_bytes() {
            if pth < path.len() {
                path[pth] = c;
                pth += 1;
            }
        }
        path[pth] = b'/';
        pth += 1;
        for &c in &p.name[..p.name_len] {
            if pth < path.len() {
                path[pth] = c;
                pth += 1;
            }
        }
        path[pth] = b'-';
        pth += 1;
        for &c in &p.version[..p.version_len] {
            if pth < path.len() {
                path[pth] = c;
                pth += 1;
            }
        }
        for &c in b".kunx" {
            if pth < path.len() {
                path[pth] = c;
                pth += 1;
            }
        }
        // key 来自网络目录数据，可能非 UTF-8：安全转换（UB 防护）
        let out_path = core::str::from_utf8(&path[..pth]).unwrap_or("");

        DL_TOTAL = p.size;
        DL_ACTIVE = true;
        print("[store] 正在下载 packages/...\n");
        let dn = download_pkg_to_cache(&cfg, &key[..kn], out_path);
        DL_ACTIVE = false;
        if dn <= 0 {
            status("下载失败".as_bytes());
            show_error(r2_err_hint(dn));
            unlink(out_path);
            return false;
        }
        print("[store] 已下载 ");
        print_dec(dn as u32);
        print(" 字节\n");

        // 读回内存验签
        let pn = read_file(out_path, &mut PKG_BUF);
        if pn <= 0 {
            show_error("本地包读取失败".as_bytes());
            return false;
        }
        let now = now_us() / 1_000_000;
        let Some(rv) = review::review(&PKG_BUF[..pn as usize], now) else {
            status("无效包".as_bytes());
            show_error("包格式无效".as_bytes());
            unlink(out_path);
            return false;
        };
        if !rv.chain_valid {
            status("验签失败".as_bytes());
            show_error("证书链验证失败".as_bytes());
            unlink(out_path);
            return false;
        }
        if !rv.sig_valid {
            status("验签失败".as_bytes());
            show_error("包签名验证失败（Ed25519）".as_bytes());
            unlink(out_path);
            return false;
        }
        let mname = rv.manifest.name;
        if !mname.is_empty() && mname != &p.name[..p.name_len] {
            status("名称不符".as_bytes());
            show_error("包名与索引不一致".as_bytes());
            unlink(out_path);
            return false;
        }
        print("[store] 签名与证书链校验通过\n");

        // 调用 pkg install-file
        let ok = run_pkg_file(out_path);
        if ok {
            status("已安装".as_bytes());
            print("[store] 安装成功\n");
        } else {
            status("安装失败".as_bytes());
            show_error("安装失败，请稍后重试".as_bytes());
        }
        ok
    }
}

// ==================== UI 状态 ====================

const VIEW_LIST: u8 = 0;
const VIEW_DETAIL: u8 = 1;
const VIEW_SETTINGS: u8 = 2;

static mut VIEW: u8 = VIEW_LIST;

// ==================== 页面切换转场（P0-2，settings/sms/files 同款整页滑动） ====================
static mut SNAP_BUF: [u32; 540 * 960] = [0u32; 540 * 960];
static mut TRANS_T0: u64 = 0;
static mut TRANS_PUSH: bool = false;
const TRANS_DUR: u64 = 240_000;

fn shade_px(c: u32, a: u32) -> u32 {
    let r = ((c >> 16) & 0xFF) * a / 255;
    let g = ((c >> 8) & 0xFF) * a / 255;
    let b = (c & 0xFF) * a / 255;
    (r << 16) | (g << 8) | b
}

fn snapshot_page() {
    unsafe {
        core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), SNAP_BUF.as_mut_ptr(), 540 * 960);
    }
}

fn begin_transition(push: bool) {
    unsafe {
        TRANS_PUSH = push;
        TRANS_T0 = now_us();
    }
}

fn transition_active() -> bool {
    unsafe { TRANS_T0 != 0 && now_us() < TRANS_T0 + TRANS_DUR }
}

fn compose_transition(e: f32) {
    if e >= 1.0 {
        return;
    }
    unsafe {
        let w = 540usize;
        let off = ((1.0 - e) * w as f32) as usize;
        if off == 0 {
            return;
        }
        for y in 0..960usize {
            let row = y * w;
            if TRANS_PUSH {
                for x in (off..w).rev() {
                    BACK_BUF[row + x] = BACK_BUF[row + x - off];
                }
                for x in 0..off {
                    BACK_BUF[row + x] = SNAP_BUF[row + x];
                }
                for s in 0..10 {
                    if s < off {
                        let x = off - 1 - s;
                        BACK_BUF[row + x] = shade_px(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            } else {
                for x in 0..w - off {
                    BACK_BUF[row + x] = BACK_BUF[row + x + off];
                }
                for x in (w - off)..w {
                    BACK_BUF[row + x] = SNAP_BUF[row + x - (w - off)];
                }
                let c = w - off;
                for s in 0..10 {
                    if s < off && c > s {
                        let x = c - 1 - s;
                        BACK_BUF[row + x] = shade_px(BACK_BUF[row + x], (90 - 9 * s) as u32);
                    }
                }
            }
        }
    }
}
/// 首屏卡片入场错峰起点（0 = 不在入场期；P0-6，仅启动播放一次）。
static mut ENTER_T0: u64 = 0;
static mut VIEW_APP: usize = 0; // 详情页选中的 index 下标
static mut CATS: [[u8; 16]; 8] = [[0; 16]; 8];
static mut CAT_LENS: [usize; 8] = [0; 8];
static mut CAT_COUNT: usize = 0;
static mut CAT_SEL: usize = 0; // 0 = 全部
static mut QUERY: [u8; 40] = [0u8; 40];
static mut QUERY_LEN: usize = 0;
static mut QUERY_FOCUS: bool = false;
static mut SET_FOCUS: isize = -1; // 设置页当前编辑字段
static mut LIST: [usize; index::MAX_PKGS] = [0; index::MAX_PKGS];
static mut LIST_COUNT: usize = 0;
/// 滚动偏移（列表首行下标）。
static mut SCROLL: usize = 0;
static mut STATUS_BUF: [u8; 64] = [0; 64];
static mut STATUS_LEN: usize = 0;

fn status(s: &[u8]) {
    unsafe {
        let n = s.len().min(STATUS_BUF.len());
        STATUS_BUF[..n].copy_from_slice(&s[..n]);
        STATUS_LEN = n;
    }
}

/// EV_KEY 键码 → ASCII（QWERTY 物理布局，与 OOBE 一致）。
fn key_to_char(code: usize, shift: bool) -> Option<u8> {
    let c = match code {
        2 => b'1', 3 => b'2', 4 => b'3', 5 => b'4', 6 => b'5',
        7 => b'6', 8 => b'7', 9 => b'8', 10 => b'9', 11 => b'0',
        16 => b'q', 17 => b'w', 18 => b'e', 19 => b'r', 20 => b't',
        21 => b'y', 22 => b'u', 23 => b'i', 24 => b'o', 25 => b'p',
        30 => b'a', 31 => b's', 32 => b'd', 33 => b'f', 34 => b'g',
        35 => b'h', 36 => b'j', 37 => b'k', 38 => b'l',
        44 => b'z', 45 => b'x', 46 => b'c', 47 => b'v', 48 => b'b',
        49 => b'n', 50 => b'm', 57 => b' ',
        12 => b'-', 13 => b'=', 52 => b'.', 51 => b',', 53 => b'/',
        77 => b'@',
        _ => return None,
    };
    let c = if shift && c.is_ascii_lowercase() { c - 32 } else { c };
    if shift {
        Some(match c {
            b'1' => b'!', b'2' => b'@', b'3' => b'#', b'4' => b'$', b'5' => b'%',
            b'6' => b'^', b'7' => b'&', b'8' => b'*', b'9' => b'(', b'0' => b')',
            b'-' => b'_', b'=' => b'+', b'.' => b'>', b',' => b'<', b'/' => b'?',
            _ => c,
        })
    } else {
        Some(c)
    }
}

// ==================== 输入事件 ====================

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_TOUCH: u16 = 0x14a;
const BTN_LEFT: u16 = 0x110;
const KEY_ENTER: u16 = 28;
const KEY_BACKSPACE: u16 = 14;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_ESC: u16 = 1;

static mut CUR_X: i32 = 270;
static mut CUR_Y: i32 = 480;
static mut TOUCH_DOWN: bool = false;
static mut MOUSE_DOWN: bool = false;
static mut SHIFT: bool = false;
/// 键盘事件：本次轮询产生的一个字符 / 退格 / 回车 / ESC。
static mut KB_CH: Option<u8> = None;
static mut KB_BACKSPACE: bool = false;
static mut KB_ENTER: bool = false;
static mut KB_ESC: bool = false;

/// 应用编辑：向焦点输入追加字符 / 删除末尾。
fn edit_append(f: usize, c: u8) {
    unsafe {
        if SET_LENS[f] < SET_VALS[f].len() {
            SET_VALS[f][SET_LENS[f]] = c;
            SET_LENS[f] += 1;
        }
    }
}

fn edit_backspace(f: usize) {
    unsafe {
        if SET_LENS[f] > 0 {
            SET_LENS[f] -= 1;
        }
    }
}

fn search_append(c: u8) {
    unsafe {
        if QUERY_LEN < QUERY.len() {
            QUERY[QUERY_LEN] = c;
            QUERY_LEN += 1;
        }
    }
}

fn search_backspace() {
    unsafe {
        if QUERY_LEN > 0 {
            QUERY_LEN -= 1;
        }
    }
}

/// 重建过滤后的列表（分类 + 搜索）。
fn rebuild_list() {
    unsafe {
        // 全部包先过搜索
        let mut all = [0usize; index::MAX_PKGS];
        let n = index::search(&QUERY[..QUERY_LEN], &mut all, index::MAX_PKGS);
        LIST_COUNT = 0;
        if CAT_SEL == 0 {
            for i in 0..n {
                LIST[LIST_COUNT] = all[i];
                LIST_COUNT += 1;
            }
        } else {
            let cat = &CATS[CAT_SEL - 1];
            let cl = CAT_LENS[CAT_SEL - 1];
            for i in 0..n {
                let p = index::pkg(all[i]);
                if p.category_len == cl && &p.category[..cl] == &cat[..cl] {
                    LIST[LIST_COUNT] = all[i];
                    LIST_COUNT += 1;
                }
            }
        }
        if SCROLL > 0 && SCROLL >= LIST_COUNT {
            SCROLL = 0;
        }
    }
}

/// 刷新分类列表。
fn rebuild_categories() {
    unsafe {
        CAT_COUNT = index::categories(&mut CATS, &mut CAT_LENS);
        if CAT_SEL > CAT_COUNT {
            CAT_SEL = 0;
        }
    }
}

// ==================== 渲染（store BACK_BUF 风格） ====================

static mut STORE_FB: *mut u32 = core::ptr::null_mut();
/// 保留的 /dev/fb0 fd：FBIO_PRESENT 提交脏区需要（mmap 后页表已指向画布，fd 仅用于 ioctl）。
static mut STORE_FB_FD: isize = -1;
static mut BACK_BUF: [u32; 540 * 960] = [0u32; 540 * 960];

fn store_pixel(x: usize, y: usize, c: u32) {
    unsafe {
        if x < 540 && y < 960 {
            if y < BACK_BUF.len() / 540 {
                BACK_BUF[y * 540 + x] = c;
            }
        }
    }
}

fn store_fill(x: i32, y: i32, w: i32, h: i32, c: u32) {
    if x < 0 || y < 0 {
        return;
    }
    let x0 = x as usize;
    let y0 = y as usize;
    let w = w as usize;
    let h = h as usize;
    for yy in y0..(y0 + h).min(960) {
        for xx in x0..(x0 + w).min(540) {
            store_pixel(xx, yy, c);
        }
    }
}

use sys_nav::glass::{GlassStyle, GlassTarget, STYLE_DANGER, glass_rect, glass_rect_style};

struct StoreGlass;
impl GlassTarget for StoreGlass {
    fn backdrop(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && x < 540 && y < 960 {
            unsafe { BACK_BUF[y as usize * 540 + x as usize] }
        } else {
            0
        }
    }
    fn put(&mut self, x: i32, y: i32, color: u32) {
        store_pixel(x as usize, y as usize, color);
    }
}

/// 毛玻璃面板（浅色玻璃：白色磨砂 + 浅灰描边）。
fn store_glass(x: i32, y: i32, w: i32, h: i32, r: i32) {
    glass_rect(&mut StoreGlass, x, y, w, h, r, false, true);
}

fn store_glass_style(x: i32, y: i32, w: i32, h: i32, r: i32, st: &GlassStyle) {
    glass_rect_style(&mut StoreGlass, x, y, w, h, r, st, true);
}

fn store_flip() {
    unsafe {
        if STORE_FB.is_null() {
            return;
        }
        core::ptr::copy_nonoverlapping(BACK_BUF.as_ptr(), STORE_FB, 540 * 960);
        fb_present();
    }
}

/// 提交整屏脏区并请求呈现（FBIO_PRESENT，单矩形 (0,0,540,960)）。
///
/// 内核在首次 FBIO_PRESENT 后进入应用驱动模式：不再由定时器自动翻页，
/// 显示节奏完全由本调用驱动（与 terminal 的 present_frame 同一最小实现）。
fn fb_present() {
    unsafe {
        if STORE_FB_FD < 0 || STORE_FB.is_null() {
            return;
        }
        let mut dbuf = [0u8; 4 + 16];
        // 缓冲布局：[count:u32][{x:i32, y:i32, w:u32, h:u32}]，全部小端。
        dbuf[0..4].copy_from_slice(&1u32.to_le_bytes());
        dbuf[4..8].copy_from_slice(&0i32.to_le_bytes());
        dbuf[8..12].copy_from_slice(&0i32.to_le_bytes());
        dbuf[12..16].copy_from_slice(&540u32.to_le_bytes());
        dbuf[16..20].copy_from_slice(&960u32.to_le_bytes());
        // 忽略返回值：内核在应用接管模式下始终呈现（无脏区时为 no-op）。
        syscall3(SYS_IOCTL, STORE_FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

/// 提交任意小脏矩形并请求呈现（FBIO_PRESENT，光标移动快速路径）。
///
/// 光标移动只提交 ~26x26 矩形：内核脏区拷贝与翻页微秒级完成，替代整屏
/// 2MB 提交（QEMU 软件渲染下整屏提交是移动卡顿的直接原因）。
fn fb_present_rects(rects: &[(i32, i32, u32, u32)]) {
    unsafe {
        if STORE_FB_FD < 0 || STORE_FB.is_null() || rects.is_empty() {
            return;
        }
        let n = rects.len().min(8);
        let mut dbuf = [0u8; 4 + 8 * 16];
        dbuf[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let r = rects[i];
            let off = 4 + i * 16;
            dbuf[off..off + 4].copy_from_slice(&r.0.to_le_bytes());
            dbuf[off + 4..off + 8].copy_from_slice(&r.1.to_le_bytes());
            dbuf[off + 8..off + 12].copy_from_slice(&r.2.to_le_bytes());
            dbuf[off + 12..off + 16].copy_from_slice(&r.3.to_le_bytes());
        }
        syscall3(SYS_IOCTL, STORE_FB_FD as usize, FBIO_PRESENT, dbuf.as_ptr() as usize);
    }
}

fn store_ttf_glyph(x: i32, y: i32, cp: u32, c: u32) -> i32 {
    if !ttf_text::font_render::font_ready() {
        return if cp >= 0x80 { 16 } else { 8 };
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top(y, g.ymin, g.height, px);
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    if px_ >= 0 && py_ >= 0 && px_ < 540 && py_ < 960 {
                        let bg = unsafe { *BACK_BUF.as_ptr().add(py_ as usize * 540 + px_ as usize) };
                        store_pixel(px_ as usize, py_ as usize, ttf_text::font_render::blend(bg, c, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

fn store_text(x: i32, y: i32, s: &[u8], c: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += store_ttf_glyph(cx, y, cp, c);
        i += n;
    }
}

/// 标题字体（bold.ttf）下绘制单个码点；标题字体未就绪时退回常规字体。
fn store_ttf_glyph_bold(x: i32, y: i32, cp: u32, c: u32) -> i32 {
    if !ttf_text::font_render::has_glyph_bold(cp) {
        return store_ttf_glyph(x, y, cp, c);
    }
    let px = ttf_text::font_render::FONT_PX;
    let ch = core::char::from_u32(cp).unwrap_or(' ');
    if let Some(g) = ttf_text::font_render::rasterize_char_bold(ch, px) {
        let gx = x + g.xmin;
        let gy = ttf_text::font_render::glyph_draw_top_bold(y, g.ymin, g.height, px);
        for row in 0..g.height {
            for col in 0..g.width {
                let a = g.data[row * g.width + col];
                if a != 0 {
                    let px_ = gx + col as i32;
                    let py_ = gy + row as i32;
                    if px_ >= 0 && py_ >= 0 && px_ < 540 && py_ < 960 {
                        let bg = unsafe { *BACK_BUF.as_ptr().add(py_ as usize * 540 + px_ as usize) };
                        store_pixel(px_ as usize, py_ as usize, ttf_text::font_render::blend(bg, c, a));
                    }
                }
            }
        }
        return g.advance;
    }
    0
}

/// 以标题字体（bold.ttf）绘制文本（标题栏）。
fn store_text_bold(x: i32, y: i32, s: &[u8], c: u32) {
    let mut cx = x;
    let mut i = 0;
    while i < s.len() {
        let (cp, n) = ttf_text::font_render::utf8_next(s, i);
        cx += store_ttf_glyph_bold(cx, y, cp, c);
        i += n;
    }
}

/// 标题字体下的文本宽度。
fn store_text_width_bold(s: &[u8]) -> i32 {
    let w = ttf_text::font_render::text_width_bold(s, ttf_text::font_render::FONT_PX);
    if w < 0 {
        0
    } else {
        w
    }
}

fn store_text_width(s: &[u8]) -> i32 {
    let w = ttf_text::font_render::text_width(s, ttf_text::font_render::FONT_PX);
    if w < 0 {
        0
    } else {
        w
    }
}

/// 背景渐变（白色背景打底：顶部纯白 → 底部极淡冷灰的浅色渐变）。
fn store_bg() {
    for y in 0..960usize {
        let t = (y as u32 * 255 / 960).min(255);
        let inv = 255 - t;
        let r = (0xFFu32 * inv + 0xE9u32 * t) / 255;
        let g = (0xFFu32 * inv + 0xEDu32 * t) / 255;
        let b = (0xFFu32 * inv + 0xF3u32 * t) / 255;
        let row = (r << 16) | (g << 8) | b;
        for x in 0..540usize {
            store_pixel(x, y, row);
        }
    }
}

fn draw_title_bar() {
    store_glass(0, 0, 540, 44, 0);
    store_glass(4, 4, 36, 36, 10);
    store_text(14, 14, "<".as_bytes(), 0x00_1A_1C_22);
    let (title, tw) = match unsafe { VIEW } {
        VIEW_SETTINGS => ("应用商店 · 设置".as_bytes(), store_text_width_bold("应用商店 · 设置".as_bytes())),
        VIEW_DETAIL => ("应用详情".as_bytes(), store_text_width_bold("应用详情".as_bytes())),
        _ => ("应用商店".as_bytes(), store_text_width_bold("应用商店".as_bytes())),
    };
    store_text_bold((540 - tw) / 2, 24, title, 0x00_1A_1C_22);
    // 设置入口（列表页右上）：设置页此前不可达（VIEW 从未被赋值
    // VIEW_SETTINGS），而错误提示让用户「到设置页配置」。
    if unsafe { VIEW } == VIEW_LIST {
        store_glass(492, 4, 44, 36, 10);
        let sw = store_text_width("设置".as_bytes());
        store_text(492 + (44 - sw) / 2, 15, "设置".as_bytes(), 0x00_1A_1C_22);
    }
}

fn back_hit(x: i32, y: i32) -> bool {
    x >= 4 && x < 40 && y >= 4 && y < 40
}

fn draw_error_banner() {
    if !err_ui::err_active(now_us()) {
        return;
    }
    let msg = err_ui::err_msg();
    if msg.is_empty() {
        return;
    }
    store_glass_style(0, 48, 540, 44, 0, &STYLE_DANGER);
    let w = store_text_width(msg);
    let tx = ((540 - w) / 2).max(0);
    store_text(tx, 62, msg, 0x00_FF_FF_FF);
}

/// 系统导航栏。
fn draw_sys_nav() {
    struct StoreCanvas;
    impl sys_nav::NavCanvas for StoreCanvas {
        fn put(&mut self, x: i32, y: i32, color: u32) {
            store_pixel(x as usize, y as usize, color);
        }
        fn backdrop(&self, x: i32, y: i32) -> u32 {
            if x >= 0 && y >= 0 && x < 540 && y < 960 {
                unsafe { BACK_BUF[y as usize * 540 + x as usize] }
            } else {
                0
            }
        }
    }
    let mut cv = StoreCanvas;
    sys_nav::draw_nav_bar(&mut cv, sys_nav::nav_mode());
}

/// 状态行（列表视图底部）。
fn draw_status() {
    unsafe {
        let y = 874i32;
        store_text(24, y, "状态".as_bytes(), 0x00_6B_72_80);
        let sx = 24 + store_text_width("状态".as_bytes()) + 12;
        store_text(sx, y, &STATUS_BUF[..STATUS_LEN], 0x00_1A_1C_22);
    }
}

/// 安装/更新按钮颜色。
fn btn_color_for(state: u8) -> u32 {
    match state {
        0 => 0x00_00_B9_6B,
        2 => 0x00_E0_9E_3C,
        _ => 0x00_6B_72_80,
    }
}

fn btn_label_for(state: u8) -> &'static [u8] {
    match state {
        0 => "安装".as_bytes(),
        2 => "更新".as_bytes(),
        _ => "已装".as_bytes(),
    }
}

fn render_list() {
    unsafe {
        store_bg();
        draw_title_bar();
        // 搜索框
        store_glass(16, 52, 508, 46, 23);
        let hint = if QUERY_LEN == 0 {
            "搜索应用（输入关键字）".as_bytes()
        } else {
            &QUERY[..QUERY_LEN]
        };
        store_text(36, 66, hint, if QUERY_LEN == 0 { 0x00_6B_72_80 } else { 0x00_1A_1C_22 });
        if QUERY_FOCUS && QUERY_LEN < QUERY.len() {
            let caret_x = 36 + store_text_width(hint) + 2;
            store_fill(caret_x, 66, 2, 18, 0x00_00_B9_6B);
        }
        // 分类 chips
        let y = 104i32;
        let cx = 16i32;
        store_glass(cx, y, 74, 36, 12);
        store_text(cx + 22, y + 11, "全部".as_bytes(), if CAT_SEL == 0 { 0x00_00_B9_6B } else { 0x00_1A_1C_22 });
        let mut nx = cx + 74 + 8;
        for i in 0..CAT_COUNT.min(5) {
            let w = store_text_width(&CATS[i][..CAT_LENS[i]]) as i32 + 24;
            store_glass(nx, y, w, 36, 12);
            store_text(nx + 12, y + 11, &CATS[i][..CAT_LENS[i]],
                if CAT_SEL == i + 1 { 0x00_00_B9_6B } else { 0x00_1A_1C_22 });
            nx += w + 8;
        }
        // 列表
        let row_h = 84i32;
        let top = 156i32;
        let mut visible = 0;
        for i in SCROLL..LIST_COUNT {
            let mut ry = top + row_h * (i - SCROLL) as i32;
            if ry + row_h > 890 {
                break;
            }
            // 首屏卡片入场错峰（P0-6）：未到出场时间的卡不绘制，出场后自
            // 下方 20px 上滑入位（200ms EaseOut，卡间错峰 25ms；仅启动
            // 首次进入且未滚动时播放，滚动/搜索过滤不重播）。
            if ENTER_T0 != 0 && SCROLL == 0 {
                let p = motion::progress(ENTER_T0 + (i - SCROLL) as u64 * 25_000, 200_000, now_us());
                if p <= 0.0 {
                    continue;
                }
                if p < 1.0 {
                    ry += (20.0 * (1.0 - motion::Easing::ColorOSStandard.eval(p))) as i32;
                }
            }
            let p = index::pkg(LIST[i]);
            let st = pkg_install_state(p);
            store_glass(16, ry, 508, row_h - 8, 18);
            store_fill(20, ry + 20, 4, 32, 0x00_7D_3C_98); // 左侧色条
            store_text(36, ry + 18, &p.name[..p.name_len], 0x00_1A_1C_22);
            store_text(36, ry + 48, &p.version[..p.version_len], 0x00_6B_72_80);
            // 描述（截断）
            let maxw = 300isize;
            let desc = &p.desc[..p.desc_len];
            let dw = store_text_width(desc) as isize;
            if dw > maxw {
                let mut cut = p.desc_len;
                while cut > 4 && store_text_width(&desc[..cut]) as isize > maxw {
                    cut -= 2;
                }
                store_text(160, ry + 18, &desc[..cut], 0x00_6B_72_80);
                store_text(160 + store_text_width(&desc[..cut]), ry + 18, "...".as_bytes(), 0x00_6B_72_80);
            } else {
                store_text(160, ry + 18, desc, 0x00_6B_72_80);
            }
            // 权限提示
            if p.perm_count > 0 {
                let mut lab = [0u8; 48];
                let mut ln = 0usize;
                for &c in "权限 ".as_bytes() {
                    lab[ln] = c;
                    ln += 1;
                }
                ln += fmt_dec(p.perm_count as u64, &mut lab[ln..]);
                // 危险计数
                let d = perm_dangerous_count(p);
                if d > 0 {
                    lab[ln] = b' ';
                    ln += 1;
                    lab[ln] = b'(';
                    ln += 1;
                    for &c in "危险".as_bytes() {
                        lab[ln] = c;
                        ln += 1;
                    }
                    lab[ln] = b' ';
                    ln += 1;
                    ln += fmt_dec(d as u64, &mut lab[ln..]);
                    lab[ln] = b')';
                    ln += 1;
                }
                store_text(160, ry + 48, &lab[..ln], 0x00_6B_72_80);
            }
            // 右侧按钮
            let bw = 92i32;
            let bx = 540 - 16 - bw;
            let bcol = btn_color_for(st);
            store_glass_style(bx, ry + (row_h - 8 - 40) / 2, bw, 40, 14, &glass_style_acc(bcol));
            let label = btn_label_for(st);
            let lw = store_text_width(label) as i32;
            store_text(bx + (bw - lw) / 2, ry + (row_h - 8 - 40) / 2 + 11, label, 0x00_FF_FF_FF);
            visible += 1;
            if visible >= 8 {
                break;
            }
        }
        if SCROLL > 0 {
            // 顶部指示：点击回到列表首行
            store_text(430, 150, "↑ 顶部".as_bytes(), 0x00_6B_72_80);
        }
        if LIST_COUNT == 0 {
            store_text(150, 400, "没有找到应用".as_bytes(), 0x00_6B_72_80);
        } else if LIST_COUNT > SCROLL + visible {
            store_text(200, 892, "↓ 更多".as_bytes(), 0x00_6B_72_80);
        }
        draw_status();
        draw_error_banner();
        draw_sys_nav();
    }
}

fn glass_style_acc(c: u32) -> sys_nav::GlassStyle {
    sys_nav::GlassStyle {
        tint: c,
        alpha: 230,
        spec: 90,
        sheen: 60,
        border: 0x00_FF_FF_FF,
        border_a: 70,
    }
}

fn perm_dangerous_count(p: &index::StorePkg) -> u32 {
    let mut n = 0u32;
    for i in 0..p.perm_count {
        if perm_dangerous(&p.perms[i][..p.perm_lens[i]]) {
            n += 1;
        }
    }
    n
}

fn perm_dangerous(p: &[u8]) -> bool {
    const DANGEROUS: [&[u8]; 8] = [
        b"STORAGE", b"CAMERA", b"MICROPHONE", b"LOCATION", b"CONTACTS", b"SMS", b"PHONE",
        b"LOCATION",
    ];
    DANGEROUS.iter().any(|&d| d == p)
}

fn render_detail() {
    unsafe {
        store_bg();
        draw_title_bar();
        let p = index::pkg(VIEW_APP);
        let y0 = 64i32;
        // 头部卡
        store_glass(16, y0, 508, 96, 18);
        store_fill(20, y0 + 20, 4, 56, 0x00_7D_3C_98);
        store_text(36, y0 + 22, &p.name[..p.name_len], 0x00_1A_1C_22);
        store_text(36, y0 + 56, &p.version[..p.version_len], 0x00_6B_72_80);
        let st = pkg_install_state(p);
        let label = btn_label_for(st);
        let bcol = btn_color_for(st);
        store_glass_style(416, y0 + 24, 92, 44, 14, &glass_style_acc(bcol));
        let lw = store_text_width(label) as i32;
        store_text(416 + (92 - lw) / 2, y0 + 36, label, 0x00_FF_FF_FF);
        // 信息行
        let mut y = y0 + 108i32;
        info_row(y, "分类".as_bytes(), &p.category[..p.category_len]);
        y += 40;
        if p.author_len > 0 {
            info_row(y, "作者".as_bytes(), &p.author[..p.author_len]);
            y += 40;
        }
        let mut sz = [0u8; 16];
        let mut sn = 0usize;
        if p.size > 0 {
            sn += fmt_dec(p.size as u64, &mut sz[sn..]);
            for &c in b" \xE5\xAD\x97\xE8\x8A\x82" { // 「字节」
                if sn < sz.len() {
                    sz[sn] = c;
                    sn += 1;
                }
            }
            info_row(y, "大小".as_bytes(), &sz[..sn]);
            y += 40;
        }
        if p.min_api > 0 {
            let mut ap = [0u8; 8];
            let an = fmt_dec(p.min_api as u64, &mut ap);
            info_row(y, "API".as_bytes(), &ap[..an]);
            y += 40;
        }
        // 权限
        let py = y;
        store_text(24, py, "权限声明".as_bytes(), 0x00_6B_72_80);
        let mut yy = py + 32;
        if p.perm_count == 0 {
            store_text(36, yy, "该应用不请求任何权限".as_bytes(), 0x00_1A_1C_22);
            yy += 32;
        } else {
            for i in 0..p.perm_count {
                store_text(36, yy, &p.perms[i][..p.perm_lens[i]], 0x00_1A_1C_22);
                if perm_dangerous(&p.perms[i][..p.perm_lens[i]]) {
                    store_text(220, yy, "（危险）".as_bytes(), 0x00_C8_3B_2E);
                }
                yy += 32;
            }
        }
        // 说明
        let dy = yy + 8;
        store_text(24, dy, "说明".as_bytes(), 0x00_6B_72_80);
        if p.desc_len > 0 {
            store_text(24, dy + 28, &p.desc[..p.desc_len], 0x00_1A_1C_22);
        } else {
            store_text(24, dy + 28, "（无说明）".as_bytes(), 0x00_6B_72_80);
        }
        draw_error_banner();
        draw_sys_nav();
    }
}

fn info_row(y: i32, label: &[u8], value: &[u8]) {
    store_text(24, y, label, 0x00_6B_72_80);
    let vx = 24 + store_text_width(label) + 16;
    store_text(vx, y, value, 0x00_1A_1C_22);
}

const SETTINGS_KEYS: [&[u8]; FIELD_MAX] = [
    b"endpoint",
    b"bucket",
    b"region",
    b"access_key",
    b"secret_key",
];

const SETTINGS_LABELS: [&[u8]; FIELD_MAX] = [
    "R2 端点".as_bytes(),
    "存储桶".as_bytes(),
    "区域".as_bytes(),
    "AccessKey".as_bytes(),
    "SecretKey".as_bytes(),
];

fn render_settings() {
    unsafe {
        store_bg();
        draw_title_bar();
        store_text(24, 60, "R2 软件源配置".as_bytes(), 0x00_6B_72_80);
        store_text(24, 84, "端点形式：host[:port] 或 http(s)://host[:port]".as_bytes(), 0x00_6B_72_80);
        let mut y = 108i32;
        for f in 0..FIELD_MAX {
            store_glass(16, y, 508, 54, 14);
            store_text(32, y + 12, SETTINGS_LABELS[f], 0x00_6B_72_80);
            let v = &SET_VALS[f][..SET_LENS[f]];
            if f == FIELD_SECRET && SET_LENS[f] > 0 {
                let mut m = [0u8; 12];
                for (_i, x) in m.iter_mut().enumerate() {
                    *x = b'*';
                }
                let ml = SET_LENS[f].min(12);
                store_text(32, y + 30, &m[..ml], 0x00_1A_1C_22);
            } else {
                store_text(32, y + 30, v, 0x00_1A_1C_22);
            }
            if SET_FOCUS == f as isize {
                store_fill(34 + store_text_width(v), y + 30, 2, 18, 0x00_00_B9_6B);
            }
            y += 62;
        }
        // 按钮：保存 / 测试连接 / 同步索引
        store_glass_style(16, 460, 160, 52, 14, &glass_style_acc(0x00_00_B9_6B));
        store_text(56, 476, "保存".as_bytes(), 0x00_FF_FF_FF);
        store_glass_style(188, 460, 160, 52, 14, &glass_style_acc(0x00_3A_7C_C8));
        store_text(224, 476, "测试连接".as_bytes(), 0x00_FF_FF_FF);
        store_glass_style(360, 460, 164, 52, 14, &glass_style_acc(0x00_E0_9E_3C));
        store_text(396, 476, "同步索引".as_bytes(), 0x00_FF_FF_FF);
        // 加载状态
        let mut y2 = 530i32;
        store_text(24, y2, format_line().as_bytes(), 0x00_6B_72_80);
        y2 += 30;
        if CFG_TLS {
            store_text(24, y2, "注意：内核无 TLS，https:// 端点当前被拒绝".as_bytes(), 0x00_C8_3B_2E);
        }
        draw_error_banner();
        draw_sys_nav();
    }
}

fn format_line() -> &'static str {
    unsafe {
        if CFG_VALID {
            "配置文件 /data/store/config"
        } else {
            "未找到配置（首次使用请填写并保存）"
        }
    }
}

// ==================== 输入 & 主循环 ====================

/// 输入事件解析（含鼠标/触摸/键盘）。
fn store_input(fd: isize) -> (Option<(i32, i32)>, sys_nav::NavAction, bool) {
    let mut click = None;
    let mut nav = sys_nav::NavAction::None;
    let mut moved = false;
    unsafe {
        KB_CH = None;
        KB_BACKSPACE = false;
        KB_ENTER = false;
        KB_ESC = false;
        loop {
            let mut b = [0u8; 24];
            let n = syscall3(SYS_READ, fd as usize, b.as_mut_ptr() as usize, 24);
            if (n as isize) <= 0 || n < 24 {
                return (click, nav, moved);
            }
            let ty = u16::from_le_bytes([b[16], b[17]]);
            let code = u16::from_le_bytes([b[18], b[19]]);
            let val = i32::from_le_bytes([b[20], b[21], b[22], b[23]]);
            match ty {
                EV_REL => {
                    if code == REL_X {
                        CUR_X = (CUR_X + val).clamp(0, 539);
                        moved = true;
                    } else if code == REL_Y {
                        CUR_Y = (CUR_Y + val).clamp(0, 959);
                        moved = true;
                    }
                }
                EV_ABS => {
                    if code == ABS_X || code == ABS_MT_POSITION_X {
                        CUR_X = val.clamp(0, 539);
                        moved = true;
                    } else if code == ABS_Y || code == ABS_MT_POSITION_Y {
                        CUR_Y = val.clamp(0, 959);
                        moved = true;
                    }
                }
                EV_KEY => {
                    if code == BTN_TOUCH {
                        if val == 1 {
                            TOUCH_DOWN = true;
                        } else if val == 0 && TOUCH_DOWN {
                            TOUCH_DOWN = false;
                            let mode = sys_nav::nav_mode();
                            let a = sys_nav::nav_hit(CUR_X, CUR_Y, mode);
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                click = Some((CUR_X, CUR_Y));
                            }
                        }
                    } else if code == BTN_LEFT {
                        if val == 1 {
                            MOUSE_DOWN = true;
                        } else if val == 0 && MOUSE_DOWN {
                            MOUSE_DOWN = false;
                            let a = sys_nav::nav_hit(CUR_X, CUR_Y, sys_nav::nav_mode());
                            if a != sys_nav::NavAction::None {
                                nav = a;
                            } else {
                                click = Some((CUR_X, CUR_Y));
                            }
                        }
                    } else if code == KEY_ESC {
                        if val == 1 {
                            KB_ESC = true;
                        }
                    } else if code == KEY_ENTER {
                        if val == 1 {
                            KB_ENTER = true;
                        }
                    } else if code == KEY_BACKSPACE {
                        if val == 1 {
                            KB_BACKSPACE = true;
                        }
                    } else if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
                        SHIFT = val != 0;
                    } else if val == 1 {
                        if let Some(c) = key_to_char(code as usize, SHIFT) {
                            KB_CH = Some(c);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// 处理键盘事件（搜索框 / 设置字段编辑）。
fn handle_keyboard() {
    unsafe {
        if KB_ESC {
            if VIEW == VIEW_SETTINGS || VIEW == VIEW_DETAIL {
                snapshot_page();
                begin_transition(false);
                VIEW = VIEW_LIST;
                QUERY_FOCUS = false;
                SET_FOCUS = -1;
                rebuild_list();
            } else {
                // ESC -> 退出
                exit(0);
            }
        }
        if KB_ENTER {
            if QUERY_FOCUS {
                QUERY_FOCUS = false;
                rebuild_list();
            } else if SET_FOCUS >= 0 {
                SET_FOCUS = -1;
            }
        }
        if let Some(c) = KB_CH {
            if QUERY_FOCUS {
                search_append(c);
                rebuild_list();
            } else if SET_FOCUS >= 0 && SET_FOCUS < FIELD_MAX as isize {
                edit_append(SET_FOCUS as usize, c);
            }
        }
        if KB_BACKSPACE {
            if QUERY_FOCUS {
                search_backspace();
                rebuild_list();
            } else if SET_FOCUS >= 0 && SET_FOCUS < FIELD_MAX as isize {
                edit_backspace(SET_FOCUS as usize);
            }
        }
    }
}

// ==================== 交互 ====================

/// 列表行命中（返回行号或 None）。
fn list_row_hit(x: i32, y: i32, row_h: i32, top: i32) -> Option<usize> {
    if x < 16 || x >= 524 {
        return None;
    }
    if y >= top && y < top + row_h * 8 {
        let r = ((y - top) / row_h) as usize;
        let (sc, lc) = unsafe { (SCROLL, LIST_COUNT) };
        let idx = sc + r;
        if idx < lc {
            return Some(idx);
        }
    }
    None
}

fn is_install_button(x: i32, y: i32, ry: i32) -> bool {
    let bw = 92i32;
    let bx = 540 - 16 - bw;
    let bh = 40i32;
    let by = ry + (84 - 8 - 40) / 2;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

fn handle_tap(x: i32, y: i32) {
    unsafe {
        match VIEW {
            VIEW_LIST => {
                if back_hit(x, y) {
                    exit(0);
                }
                if x >= 16 && x < 524 && y >= 52 && y < 98 {
                    QUERY_FOCUS = !QUERY_FOCUS;
                    return;
                }
                // 分类 chip
                if y >= 104 && y < 140 {
                    if x >= 16 && x < 90 {
                        CAT_SEL = 0;
                        rebuild_list();
                        return;
                    }
                    let mut nx = 90i32;
                    for i in 0..CAT_COUNT.min(5) {
                        let w = store_text_width(&CATS[i][..CAT_LENS[i]]) as i32 + 24;
                        if x >= nx && x < nx + w {
                            if CAT_SEL == i + 1 {
                                CAT_SEL = 0;
                            } else {
                                CAT_SEL = i + 1;
                            }
                            rebuild_list();
                            return;
                        }
                        nx += w + 8;
                    }
                }
                // 滚动指示：顶部 / 更多
                if y >= 884 && y < 902 {
                    if x >= 160 && x < 360 {
                        if SCROLL + 8 < LIST_COUNT {
                            SCROLL += 8;
                        }
                        return;
                    }
                }
                if y >= 144 && y < 154 && x >= 400 {
                    if SCROLL > 0 {
                        SCROLL = 0;
                    }
                    return;
                }
                // 设置入口（标题栏右上）
                if x >= 492 && x < 536 && y >= 4 && y < 40 {
                    snapshot_page();
                    begin_transition(true);
                    VIEW = VIEW_SETTINGS;
                    return;
                }
                // 列表行
                let top = 156i32;
                let row_h = 84i32;
                if let Some(r) = list_row_hit(x, y, row_h, top) {
                    let ry = top + row_h * (r - SCROLL) as i32;
                    if is_install_button(x, y, ry) {
                        let p = index::pkg(LIST[r]);
                        let st = pkg_install_state(p);
                        if st != 1 {
                            status("下载中...".as_bytes());
                            store_redraw();
                            store_flip();
                            cursor_overlay_redraw();
                            install_app(LIST[r]);
                            store_redraw();
                            store_flip();
                            cursor_overlay_redraw();
                        }
                    } else if x >= 16 && x < 524 && y >= ry && y < ry + 76 {
                        snapshot_page();
                        begin_transition(true);
                        VIEW_APP = LIST[r];
                        VIEW = VIEW_DETAIL;
                    }
                }
            }
            VIEW_DETAIL => {
                if back_hit(x, y) {
                    snapshot_page();
                    begin_transition(false);
                    VIEW = VIEW_LIST;
                    rebuild_list();
                    return;
                }
                let p = index::pkg(VIEW_APP);
                let st = pkg_install_state(p);
                if x >= 416 && x < 508 && y >= 88 && y < 132 && st != 1 {
                    status("下载中...".as_bytes());
                    store_redraw();
                    store_flip();
                    cursor_overlay_redraw();
                    install_app(VIEW_APP);
                    store_redraw();
                    store_flip();
                    cursor_overlay_redraw();
                }
            }
            VIEW_SETTINGS => {
                if back_hit(x, y) {
                    snapshot_page();
                    begin_transition(false);
                    VIEW = VIEW_LIST;
                    rebuild_list();
                    return;
                }
                // 字段行
                if x >= 16 && x < 524 {
                    let mut fy = 108i32;
                    for f in 0..FIELD_MAX {
                        if y >= fy && y < fy + 54 {
                            if SET_FOCUS == f as isize {
                                SET_FOCUS = -1;
                            } else {
                                SET_FOCUS = f as isize;
                                QUERY_FOCUS = false;
                            }
                            return;
                        }
                        fy += 62;
                    }
                }
                // 按钮
                if x >= 16 && x < 176 && y >= 460 && y < 512 {
                    SET_FOCUS = -1;
                    if save_config() {
                        status("已保存配置".as_bytes());
                    } else {
                        show_error("配置保存失败".as_bytes());
                    }
                    return;
                }
                if x >= 188 && x < 348 && y >= 460 && y < 512 {
                    test_connection();
                    return;
                }
                if x >= 360 && x < 524 && y >= 460 && y < 512 {
                    SET_FOCUS = -1;
                    save_config();
                    status("正在同步索引...".as_bytes());
                    store_redraw();
                    store_flip();
                    cursor_overlay_redraw();
                    index_load();
                    rebuild_categories();
                    rebuild_list();
                    return;
                }
            }
            _ => {}
        }
    }
}

fn test_connection() {
    // HEAD 或读索引测试；用 s3_get Packages.json（小文件）
    let cfg = build_r2_cfg();
    let mut st = 0i32;
    let r = unsafe { r2::s3_get(&cfg, b"Packages.json", &mut IDX_BUF, &mut st) };
    if r > 0 {
        status("连接成功：Packages.json 可读".as_bytes());
        print("[store] 测试通过 ");
        print_dec(r as u32);
        print(" 字节\n");
    } else {
        status("连接失败".as_bytes());
        show_error(r2_err_hint(r));
        print("[store] 测试失败（");
        print_dec((-r) as u32);
        print(")\n");
    }
}

// ==================== 渲染入口 ====================

fn store_redraw() {
    match unsafe { VIEW } {
        VIEW_DETAIL => render_detail(),
        VIEW_SETTINGS => render_settings(),
        _ => render_list(),
    }
    // 页面转场合成（目标页已画进 BACK_BUF，叠加滑动 + 快照补底）。
    unsafe {
        if TRANS_T0 != 0 {
            let now = now_us();
            let el = now.saturating_sub(TRANS_T0);
            let p = if el >= TRANS_DUR { 1.0 } else { el as f32 / TRANS_DUR as f32 };
            let e = ease_fosl(p);
            compose_transition(e);
            if p >= 1.0 {
                TRANS_T0 = 0;
            }
        }
    }
}

/// FastOutSlowIn 近似（0.4,0,0.2,1）8 段查表（与 shared/glass 同款，避免
/// 引入 motion 模块的重依赖）。
fn ease_fosl(t: f32) -> f32 {
    const Y: [f32; 9] = [0.0, 0.33, 0.60, 0.78, 0.89, 0.95, 0.98, 0.995, 1.0];
    let t = t.clamp(0.0, 1.0);
    let i = ((t * 8.0) as usize).min(7);
    Y[i] + (Y[i + 1] - Y[i]) * (t * 8.0 - i as f32)
}

fn cursor_overlay_redraw() {
    cursor_overlay::overlay_redraw(
        unsafe { STORE_FB },
        540,
        540,
        960,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    // overlay 直接写画布，整帧提交不含光标像素；补光标小矩形呈现上屏
    // （不再整屏重复提交）。
    fb_present_rects(&[cursor_overlay::under_rect(
        unsafe { CUR_X },
        unsafe { CUR_Y },
    )]);
}

fn cursor_overlay_move() {
    // 移动光标并把新旧位置区以小脏矩形提交呈现（不再整屏提交）。
    let rects = cursor_overlay::overlay_move_rects(
        unsafe { STORE_FB },
        540,
        540,
        960,
        unsafe { CUR_X },
        unsafe { CUR_Y },
    );
    fb_present_rects(&rects);
}

fn store_poll_key() -> bool {
    // ★ 独立非阻塞 tty fd：fd 0 是阻塞语义（musl fgets），无数据时直读
    // 会把应用卡死在内核 yield 循环（tty_read 此前无视 O_NONBLOCK——
    // 内核已修，此处也换非阻塞 fd 双保险）。
    static mut TTY_POLL_FD: isize = -1;
    let fd = unsafe {
        if TTY_POLL_FD < 0 {
            let mut p = [0u8; 16];
            let s = b"/dev/tty\0";
            p[..s.len()].copy_from_slice(s);
            TTY_POLL_FD = syscall3(SYS_OPENAT, 0xFFFF_FFFF_FFFF_FF9Cusize, p.as_ptr() as usize, 0x800) as isize;
        }
        TTY_POLL_FD
    };
    if fd < 0 {
        return false;
    }
    let mut buf = [0u8; 8];
    let n = unsafe { syscall3(SYS_READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) };
    // 读失败 -1（usize 天文数字）按 isize 判负，否则越界 panic
    let n = n as isize;
    if n <= 0 {
        return false;
    }
    for i in 0..(n.min(buf.len() as isize) as usize) {
        if buf[i] == 0x1b {
            return true;
        }
    }
    false
}

fn exit(code: usize) -> ! {
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    loop {}
}

// ==================== 主流程 ====================

#[no_mangle]
pub extern "C" fn k_main(_argc: usize, _argv: *const usize) -> usize {
    print("[store] 正在启动\n");
    unsafe {
        let fd = open("/dev/fb0", O_RDWR);
        if fd < 0 {
            print("[store] 无法打开 /dev/fb0\n");
            exit(1);
        }
        let base = syscall6(SYS_MMAP, 0, 540 * 960 * 4, PROT_READ | PROT_WRITE, MAP_SHARED, fd as usize, 0);
        if (base as isize) < 0 {
            // mmap 失败：维持原有失败退出行为（提示后 exit(1)）。
            syscall1(SYS_CLOSE, fd as usize);
            STORE_FB = core::ptr::null_mut();
        } else {
            STORE_FB = base as *mut u32;
            // 保留 fd：FBIO_PRESENT / FBIO_PUT_DAMAGE 依赖它提交脏区（mmap 后
            // 页表已指向画布，fd 仅用于 ioctl）。
            STORE_FB_FD = fd as isize;
        }
    }
    if unsafe { STORE_FB.is_null() } {
        print("[store] 帧缓冲内存映射失败\n");
        exit(1);
    }

    ensure_dirs();
    load_config();
    let st = index_load();
    let _ = st;
    rebuild_categories();
    rebuild_list();

    let input = unsafe { open("/dev/input/event0", O_RDONLY | O_NONBLOCK) };

    store_status_init();
    store_redraw();
    store_flip();
    cursor_overlay_redraw();
    ttf_text::font_init();
    // 标题字体按需加载（font_init 不再自动带粗体）：商店标题栏走粗体，显式加载。
    ttf_text::font_init_bold();
    store_redraw();
    store_flip();
    cursor_overlay_redraw();
    print("[store] 就绪\n");
    // 首屏卡片入场错峰（P0-6）：仅启动播放一次（首屏可见 ~8 卡）。
    {
        let t0 = now_us();
        unsafe { ENTER_T0 = t0; }
        let last_end = t0 + 7 * 25_000 + 200_000;
        loop {
            store_redraw();
            store_flip();
            if now_us() >= last_end {
                break;
            }
            sleep_ms(8);
        }
        unsafe { ENTER_T0 = 0; }
        store_redraw();
        store_flip();
        cursor_overlay_redraw();
    }

    loop {
        let moved = if input >= 0 {
            let (click, nav, moved) = store_input(input);
            if moved {
                cursor_overlay_move();
            }
            if nav != sys_nav::NavAction::None {
                print("[store] 导航 → 退出\n");
                exit(sys_nav::nav_exit_code(nav));
            }
            if let Some((x, y)) = click {
                handle_tap(x, y);
                store_redraw();
                store_flip();
                cursor_overlay_redraw();
            }
            moved
        } else {
            false
        };
        let _ = moved;
        // 键盘处理
        unsafe {
            if KB_ESC || KB_ENTER || KB_BACKSPACE || KB_CH.is_some() {
                handle_keyboard();
                store_redraw();
                store_flip();
                cursor_overlay_redraw();
            }
        }
        if store_poll_key() {
            print("[store] ESC → 退出\n");
            break;
        }
        // 页面转场进行中：逐帧重绘推进（240ms）。
        if transition_active() {
            store_redraw();
            store_flip();
            cursor_overlay_redraw();
        }
        sleep_ms(16);
    }

    print("[store] 正在退出\n");
    exit(0);
}

fn store_status_init() {
    status("就绪（点击安装或右上角设置）".as_bytes());
}

global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    ".type _start, %function",
    "_start:",
    "    mv a1, sp",
    "    ld a0, 0(a1)",
    "    addi a1, a1, 8",
    "    call k_main",
    "    li a7, 93",
    "    ecall",
    "    j .",
);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // 诊断：把 panic 消息与位置（文件:行:列）写入 fd 2（无堆，固定缓冲）。
    struct BufWriter {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for BufWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let k = b.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + k].copy_from_slice(&b[..k]);
            self.len += k;
            Ok(())
        }
    }
    let mut w = BufWriter { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut w, core::format_args!("{}\n", info));
    unsafe {
        write_fd(2, b"PANIC: store\n");
        write_fd(2, &w.buf[..w.len]);
        syscall1(SYS_EXIT, 1);
    }
    loop {}
}