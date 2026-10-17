//! KOS 对象存储内核集成（Phase 1 双跑：SFS 之外的第二个卷）。
//!
//! 职责：
//! - [`KosBlkDev`]：把内核 [`crate::blkdev`] 的一个设备适配成
//!   `kos::block::BlockDevice`（4KB 块语义直通）；
//! - 全局挂载槽 + 复用 `fs_lock` 大锁串行化（KOS 引擎要求同一设备
//!   同时只有一个活跃实例）；
//! - 系统调用面（号段 460..=466，见 `linux_compat`）：
//!   mount / read / write / mkdir / unlink / list / stat。
//!
//! 与 SFS 的关系：本阶段双写并存（DESIGN.md Phase 1），KOS 卷默认
//! 挂在第二个磁盘（`-drive file=kos.img`），路径以 `/kos/` 前缀的
//! 私有命名空间存在于 KOS 卷内部，不与 SFS 路径混用。

use crate::blkdev;
use kos::block::BlockDevice;
use kos::error::KosError;
use kos::store::{KosStore, VerifyLevel};

// ==================== Manifest 验签（kunxsec Ed25519） ====================

// 信任公钥（32 字节；全零 = 未配置 → 不验签）。由 `make kos-disk`
// 生成（tools/.kos-seed 派生），写入 kunxcore/src/kos_trust_pk.rs。
include!("kos_trust_pk.rs");

/// 提交期签名回调：用开发种子（make kos-disk 生成，随仓库走）给
/// 每一代 manifest 签名——严格验签模式下系统可持续写入。
fn kos_commit_signer(payload: [u8; 80]) -> ([u8; 32], [u8; 64]) {
    let seed = KOS_SIGN_SEED;
    let pk = kunxsec::ed25519::public_key(&seed);
    let sig = kunxsec::ed25519::sign(&seed, &pk, &payload);
    (pk, sig)
}

/// kunxsec Ed25519 验签器：签名覆盖 manifest[8..120)（受信字段区）。
pub struct KunxsecVerifier {
    pk: [u8; 32],
}

impl kos::store::ManifestVerifier for KunxsecVerifier {
    fn verify_manifest(&self, m: &kos::format::manifest::KosRootManifest) -> bool {
        let payload = kos::store::manifest_sign_payload(m);
        kunxsec::ed25519::verify(&self.pk, &payload, &m.signature)
    }
    fn strict(&self) -> bool {
        // 配置了信任锚即严格：未签名槽一律拒绝。
        self.pk != [0u8; 32]
    }
}

// ==================== 系统调用号（私有号段） ====================

pub const SYS_KOS_MOUNT: usize = 460;
pub const SYS_KOS_READ: usize = 461;
pub const SYS_KOS_WRITE: usize = 462;
pub const SYS_KOS_MKDIR: usize = 463;
pub const SYS_KOS_UNLINK: usize = 464;
pub const SYS_KOS_LIST: usize = 465;
pub const SYS_KOS_STAT: usize = 466;
pub const SYS_KOS_APPEND: usize = 467;
pub const SYS_KOS_TRUNCATE: usize = 468;
pub const SYS_KOS_LIST2: usize = 469;
pub const SYS_KOS_READ_RANGE: usize = 470;
pub const SYS_KOS_RENAME: usize = 471;
pub const SYS_KOS_OPEN: usize = 472;
pub const SYS_KOS_READ_FD: usize = 473;
pub const SYS_KOS_CLOSE_FD: usize = 474;
pub const SYS_KOS_WRITE_RANGE: usize = 475;
pub const SYS_KOS_UP_BEGIN: usize = 476;
pub const SYS_KOS_UP_PUSH: usize = 477;
pub const SYS_KOS_UP_STATUS: usize = 478;
pub const SYS_KOS_UP_FINISH: usize = 479;

// ==================== I/O 故障注入与降级（E2） ====================
//
// 「报错了还能继续维持运行」的内核侧落地：
// - 故障注入（root 专用 syscall 480）：KosBlkDev 一切读/写/冲刷返回
//   Io 错误——所有 kos syscall 转为 EIO，内核不 panic 不死锁；
// - 降级上报：首个 I/O 失败经 health 报 CRITICAL（边沿触发一次），
//   恢复后报 INFO；系统全程继续运行，注入/故障解除即自动恢复服务。

/// 故障注入开关（true = 一切块设备 IO 返回 Io(5)）。
static KOS_FAULT_INJECT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// 降级状态（health 边沿触发用）。
static KOS_DEGRADED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

fn kos_io_fail() -> bool {
    KOS_FAULT_INJECT.load(core::sync::atomic::Ordering::Relaxed)
}

fn note_io_fail() {
    if !KOS_DEGRADED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::health::report(
            crate::health::SEV_CRITICAL,
            "storage",
            "KOS 对象存储 I/O 故障，卷已降级（读写返回 EIO，系统继续运行）",
            "检查存储设备连接；故障解除后自动恢复，无需重启",
        );
    }
}

fn note_io_ok() {
    if KOS_DEGRADED.swap(false, core::sync::atomic::Ordering::Relaxed) {
        crate::health::report(
            crate::health::SEV_INFO,
            "storage",
            "KOS 对象存储 I/O 恢复，卷重回正常服务",
            "",
        );
    }
}

/// syscall 480（root 专用）：x0=1 打开故障注入、x0=0 解除。返回旧状态。
pub fn sys_fault(on: usize) -> i64 {
    if crate::process::current_uid() != 0 {
        return -13; // EACCES
    }
    let prev = KOS_FAULT_INJECT.swap(on != 0, core::sync::atomic::Ordering::Relaxed);
    if on == 0 && prev {
        // 注入解除视为设备恢复：立即兑现恢复上报（下一轮成功 IO 亦会清）。
        note_io_ok();
    }
    prev as i64
}

// ==================== 块设备适配 ====================

/// 内核块设备适配器：`major` 设备按 4KB 块读写。
pub struct KosBlkDev {
    pub major: u32,
    pub blocks: u64,
}

impl BlockDevice for KosBlkDev {
    fn block_size(&self) -> usize {
        kos::format::KOS_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), KosError> {
        if kos_io_fail() {
            note_io_fail();
            return Err(KosError::Io(5));
        }
        // blk_read 成功返回拷贝字节数（正数），失败返回负数。
        if blkdev::blk_read(self.major, lba as usize, buf) >= 0 {
            note_io_ok();
            Ok(())
        } else {
            note_io_fail();
            Err(KosError::Io(5))
        }
    }

    fn write_block(&mut self, lba: u64, data: &[u8]) -> Result<(), KosError> {
        if kos_io_fail() {
            note_io_fail();
            return Err(KosError::Io(5));
        }
        if blkdev::blk_write(self.major, lba as usize, data) >= 0 {
            note_io_ok();
            Ok(())
        } else {
            note_io_fail();
            Err(KosError::Io(5))
        }
    }

    fn flush(&mut self) -> Result<(), KosError> {
        if kos_io_fail() {
            note_io_fail();
            return Err(KosError::Io(5));
        }
        if blkdev::blk_flush(self.major) >= 0 {
            note_io_ok();
            Ok(())
        } else {
            note_io_fail();
            Err(KosError::Io(5))
        }
    }
}

// ==================== 全局挂载槽 ====================

/// 已挂载的 KOS 卷。访问一律在 fs_lock 大锁内（与 SFS 共用一把锁，
/// 避免两套存储各自为政造成的锁序问题）。
static mut KOS_STORE: Option<KosStore<KosBlkDev>> = None;

fn with_store<T>(f: impl FnOnce(&mut KosStore<KosBlkDev>) -> T) -> Option<T> {
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 是任务身份可重入大锁，KOS_STORE 只在此临界区
    // 内访问；SMP 下与 SFS 路径互斥。
    let r = unsafe { KOS_STORE.as_mut().map(f) };
    // F4：提交后低空间水位检查（O(1)）。
    // 写流畅度：Batched 冲刷的定时兜底（>500ms 未落盘即补一次 FLUSH）。
    if let Some(s) = unsafe { KOS_STORE.as_mut() } {
        check_space_watermark(s.free_blocks(), s.pool_blocks());
        let now = crate::timer::now_us();
        if now - LAST_SYNC.load(core::sync::atomic::Ordering::Relaxed) > 500_000 {
            LAST_SYNC.store(now, core::sync::atomic::Ordering::Relaxed);
            let _ = s.sync_now();
        }
    }
    crate::fs_simple::fs_unlock();
    r
}

/// 低空间水位检查（F4 稳定性）：空闲 <10% 警告、<3% 严重，各报一次。
static LOW_SPACE_WARNED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static LOW_SPACE_CRITICAL: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static LAST_SYNC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn check_space_watermark(free: u64, pool: u64) {
    if pool == 0 {
        return;
    }
    let pct = free * 100 / pool;
    if pct < 3 && !LOW_SPACE_CRITICAL.load(core::sync::atomic::Ordering::Relaxed) {
        LOW_SPACE_CRITICAL.store(true, core::sync::atomic::Ordering::Relaxed);
        crate::health::report(
            crate::health::SEV_CRITICAL,
            "storage",
            "KOS 对象存储空间严重不足（<3%），写入即将失败",
            "请清理不需要的对象（shell: kos 命令）或运行 GC 后重试",
        );
    } else if pct < 10 && !LOW_SPACE_WARNED.load(core::sync::atomic::Ordering::Relaxed) {
        LOW_SPACE_WARNED.store(true, core::sync::atomic::Ordering::Relaxed);
        crate::health::report(
            crate::health::SEV_WARN,
            "storage",
            "KOS 对象存储空间不足（<10%）",
            "建议清理旧对象或运行 GC",
        );
    }
}

/// 启动期挂载策略（P1 持久化）：
/// 1. 优先扫描 virtio-blk 磁盘（sdb..，major 9..=11；sda 留给 SFS），
///    块 0 为 KOS 超级块者挂载为**持久卷**；
/// 2. 否则回退内嵌 kosrd（RAM 卷，写入不跨重启）。
/// 返回挂载描述（打印用）。
pub fn boot_mount() -> bool {
    // 全槽扫描：SFS 磁盘不会有 KOS 超级块（magic 不同），无冲突。
    for slot in 0..crate::virtio_blk::dev_count() {
        let major = crate::virtio_blk::MAJOR + slot as u32;
        if kos_superblock_at(major) && mount(major) {
            crate::uart_send_str("[kos] 持久对象存储卷已挂载（磁盘 major=");
            let mut tmp = [0u8; 4];
            let mut v = major;
            for i in (0..4).rev() {
                tmp[i] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            crate::uart_send_str(core::str::from_utf8(&tmp).unwrap_or("?"));
            crate::uart_send_str("）\n");
            return true;
        }
    }
    // 回退：内嵌 kosrd（main.rs 早期已注册为 major 254）。
    if kos_superblock_at(254) && mount(254) {
        crate::uart_send_str("[kos] 对象存储卷已挂载（内嵌 RAM 卷，写入不跨重启）\n");
        return true;
    }
    crate::uart_send_str("[kos] 未找到可挂载的 KOS 卷\n");
    false
}

/// `major` 块 0 是否为合法 KOS 超级块（magic + version + 4096 块大小）。
fn kos_superblock_at(major: u32) -> bool {
    let mut buf = [0u8; 512];
    if blkdev::blk_read_sectors(major, 0, 1, &mut buf) < 0 {
        return false;
    }
    buf[0..8] == *b"KOSV0001"
}

/// 内核内部挂载（启动期用，无 uid 检查）。成功返回 true。
pub fn mount(major: u32) -> bool {
    let mut blocks = 0u64;
    blkdev::for_each_blkdev(|m, _name, b| {
        if m == major {
            blocks = b as u64;
        }
    });
    if blocks == 0 {
        return false;
    }
    let dev = KosBlkDev { major, blocks };
    let verifier = KunxsecVerifier { pk: KOS_TRUST_PK };
    match KosStore::open_with_verifier(dev, VerifyLevel::HashOnly, &verifier) {
        Ok(mut s) => {
            // 信任锚已配置（种子非零）时，后续提交全部自动签名。
            if KOS_SIGN_SEED != [0u8; 32] {
                s.set_signer(kos_commit_signer);
            }
            // 写流畅度：批量冲刷（64 次提交一次设备 FLUSH），定时器
            // 500ms 兜底（见 with_store）；持久性窗口有界。
            s.set_flush_policy(kos::store::FlushPolicy::Batched(64));
            crate::fs_simple::fs_lock();
            // SAFETY：fs_lock 临界区内（见 with_store）。
            unsafe {
                KOS_STORE = Some(s);
            }
            crate::fs_simple::fs_unlock();
            true
        }
        Err(e) => {
            crate::uart_send_str("[kos-dbg] open err code=");
            crate::uart_send_str(match e {
                KosError::InvalidMagic { .. } => "magic",
                KosError::CrcMismatch { .. } => "crc",
                KosError::UnsupportedVersion { .. } => "ver",
                KosError::InvalidBlockSize { .. } => "bsz",
                KosError::NoValidManifest => "nomanifest",
                KosError::BlockOutOfRange { .. } => "oor",
                KosError::InvalidBlockLen { .. } => "blen",
                KosError::Io(_) => "io",
                _ => "other",
            });
            crate::uart_send_str(" blocks=");
            let mut tmp = [0u8; 16];
            let mut v = blocks;
            for i in (0..16).rev() {
                tmp[i] = b'0' + (v & 0xF) as u8;
                v >>= 4;
            }
            crate::uart_send_str(core::str::from_utf8(&tmp).unwrap_or(""));
            crate::uart_send_str("\n");
            false
        }
    }
}

/// 挂载（root 专用）：`major` 为内核块设备主号。
pub fn sys_mount(major: u32) -> i64 {
    if crate::process::current_uid() != 0 {
        return -1;
    }
    let mut blocks = 0u64;
    blkdev::for_each_blkdev(|m, _name, b| {
        if m == major {
            blocks = b as u64;
        }
    });
    if blocks == 0 {
        return -1;
    }
    let dev = KosBlkDev { major, blocks };
    let verifier = KunxsecVerifier { pk: KOS_TRUST_PK };
    match KosStore::open_with_verifier(dev, VerifyLevel::HashOnly, &verifier) {
        Ok(mut s) => {
            // 信任锚已配置（种子非零）时，后续提交全部自动签名。
            if KOS_SIGN_SEED != [0u8; 32] {
                s.set_signer(kos_commit_signer);
            }
            // 写流畅度：批量冲刷（64 次提交一次设备 FLUSH），定时器
            // 500ms 兜底（见 with_store）；持久性窗口有界。
            s.set_flush_policy(kos::store::FlushPolicy::Batched(64));
            crate::fs_simple::fs_lock();
            // SAFETY：同上，fs_lock 临界区内。
            unsafe {
                KOS_STORE = Some(s);
            }
            crate::fs_simple::fs_unlock();
            0
        }
        Err(_) => -1,
    }
}

/// 是否已挂载。
pub fn mounted() -> bool {
    with_store(|_| true).unwrap_or(false)
}

// ==================== 数据面系统调用 ====================

/// KosError → errno（负值）。系统调用统一走这里，替代裸 -1。
pub fn kos_errno(e: KosError) -> i64 {
    const ENOENT: i64 = -2;
    const EIO: i64 = -5;
    const EACCES: i64 = -13;
    const EEXIST: i64 = -17;
    const ENOSPC: i64 = -28;
    match e {
        KosError::ObjectNotFound | KosError::PathNotFound | KosError::NoValidManifest => ENOENT,
        KosError::PermissionDenied => EACCES,
        KosError::AlreadyExists => EEXIST,
        KosError::NoSpace => ENOSPC,
        KosError::Io(_) => EIO,
        KosError::NotMutable => EACCES,
        KosError::UploadIncomplete { received, total } => {
            // 精确进度编进高 32 位：用户态可直接换算续传点。
            -((0x1_0000_0000u64 | ((received as u64) << 20) | total as u64) as i64)
        }
        _ => EIO,
    }
}

/// 读对象：成功返回字节数，失败 -1。路径由调用方以 C 字符串给出。
pub fn sys_read(path: &str, buf: &mut [u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.open_path(path, uid)) {
        Some(Ok(data)) => {
            let n = core::cmp::min(data.len(), buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            n as i64
        }
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 写对象（新建或整体替换）：caps 为权限位（0 则用缺省）；
/// `mutable` 为真时建可变记录（支持 append）。
pub fn sys_write(path: &str, data: &[u8], caps: u32, mutable: bool) -> i64 {
    let uid = crate::process::current_uid();
    let caps = if caps == 0 { kos::store::perm::DEFAULT } else { caps };
    let r = with_store(|s| {
        if mutable {
            s.write_mutable(path, data, uid, caps).map(|_| ())
        } else {
            s.write(path, data, uid, caps)
        }
    });
    match r {
        Some(Ok(())) => data.len() as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

pub fn sys_mkdir(path: &str, caps: u32) -> i64 {
    let uid = crate::process::current_uid();
    let caps = if caps == 0 { kos::store::perm::DIR_DEFAULT } else { caps };
    match with_store(|s| s.mkdir(path, uid, caps)) {
        Some(Ok(())) => 0,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

pub fn sys_unlink(path: &str) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.unlink(path, uid)) {
        Some(Ok(())) => 0,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 列目录：文本行 `type caps uid size name\n` 写入 buf，返回字节数。
pub fn sys_list(path: &str, buf: &mut [u8]) -> i64 {
    let lines = match with_store(|s| s.list(path)) {
        Some(Ok(entries)) => {
            let mut out = alloc::string::String::new();
            use core::fmt::Write;
            for e in entries {
                let t = match e.entry_type {
                    kos::format::object::ObjectType::Dir => 'd',
                    kos::format::object::ObjectType::Symlink => 'l',
                    _ => '-',
                };
                let _ = write!(
                    &mut out,
                    "{t} {:o} {} {}\n",
                    e.capability, e.owner_uid, e.name
                );
            }
            out
        }
        _ => return -1,
    };
    let b = lines.as_bytes();
    let n = core::cmp::min(b.len(), buf.len());
    buf[..n].copy_from_slice(&b[..n]);
    n as i64
}

/// 截断/扩展（仅可变记录）。
pub fn sys_truncate(path: &str, len: u64) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.truncate(path, len, uid)) {
        Some(Ok(n)) => n as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 分页列目录：跳过前 `skip` 条。
pub fn sys_list_paged(path: &str, skip: usize, buf: &mut [u8]) -> i64 {
    let lines = match with_store(|s| s.list_skip(path, skip)) {
        Some(Ok(entries)) => {
            let mut out = alloc::string::String::new();
            use core::fmt::Write;
            for e in entries {
                let t = match e.entry_type {
                    kos::format::object::ObjectType::Dir => 'd',
                    kos::format::object::ObjectType::Symlink => 'l',
                    _ => '-',
                };
                let _ = write!(&mut out, "{t} {:o} {} {}\n", e.capability, e.owner_uid, e.name);
            }
            out
        }
        _ => return -1,
    };
    let b = lines.as_bytes();
    let n = core::cmp::min(b.len(), buf.len());
    buf[..n].copy_from_slice(&b[..n]);
    n as i64
}

/// 追加写（仅可变记录）：返回新长度。
pub fn sys_append(path: &str, data: &[u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.append(path, data, uid)) {
        Some(Ok(len)) => len as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 随机读（pread）：`offset..offset+len`。返回读取字节数。
pub fn sys_read_range(path: &str, offset: u64, buf: &mut [u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.read_range(path, offset, buf, uid)) {
        Some(Ok(n)) => n as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// rename / move。
pub fn sys_rename(from: &str, to: &str) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.rename(from, to, uid)) {
        Some(Ok(())) => 0,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

// ==================== POSIX 桥接：/kos/ 路径路由 ====================
//
// 应用用普通 open/read/write/close/lseek 访问 `/kos/<对象路径>`，
// 内核在此路由到 KOS 引擎——零改造接入。KOS fd 号从 1000 起，
// 避开进程 fd 表（FD_MAX 远小于 1000）。
//
// 稳定性纪律（P 稳定性轮加固）：
// - 表项按 (pid, fd) 归属：进程退出统一回收（close_pid_fds），不再
//   泄漏，也不会被 pid 复用后的新进程捡到他人 fd；
// - 全部表操作持 fs_lock（与 KOS_FDS/引擎同一锁序，SMP 安全）；
// - 每进程 fd 上限 POSIX_FD_MAX，防单进程耗尽全局表。

/// KOS POSIX fd 起点。
pub const KOS_FD_BASE: u32 = 1000;

/// `/kos/` 前缀长度。
pub const KOS_PREFIX: &str = "/kos/";

/// 每进程 POSIX 桥接 fd 上限。
const POSIX_FD_MAX: usize = 64;

struct PosixFd {
    /// 归属进程。
    pid: u32,
    /// 命名空间内路径（去掉 /kos/ 前缀）。
    path: alloc::string::String,
    key: [u8; 32],
    pos: u64,
    append_mode: bool,
}

static mut POSIX_FDS: Option<alloc::collections::BTreeMap<u32, PosixFd>> = None;
static mut POSIX_FD_NEXT: u32 = KOS_FD_BASE;

/// fs_lock 临界区内访问 POSIX fd 表。
fn posix_table_locked() -> &'static mut alloc::collections::BTreeMap<u32, PosixFd> {
    // SAFETY：仅在各入口先 fs_lock 的路径调用（与 with_store 同一锁序）。
    unsafe { POSIX_FDS.get_or_insert_with(alloc::collections::BTreeMap::new) }
}

/// 路径是否路由到 KOS。
pub fn is_kos_path(p: &str) -> bool {
    p.starts_with(KOS_PREFIX)
}

/// 进程退出清理：回收其全部 POSIX 桥接 fd 与 KOS fd 表项。
/// 在进程仍处普通任务上下文时调用（exit 顶部 / 强杀自杀分支）。
pub fn close_pid_fds(pid: u32) {
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    unsafe {
        if let Some(t) = POSIX_FDS.as_mut() {
            t.retain(|_, e| e.pid != pid);
        }
        if let Some(t) = KOS_FDS.as_mut() {
            t.retain(|(p, _), _| *p != pid);
        }
    }
    crate::fs_simple::fs_unlock();
}

/// POSIX open：flags 支持 O_CREAT/O_TRUNC/O_APPEND/O_RDONLY/O_WRONLY。
pub fn posix_open(rel: &str, flags: u32, uid: u32) -> i64 {
    const O_CREAT: u32 = 0x40;
    const O_TRUNC: u32 = 0x200;
    const O_APPEND: u32 = 0x400;
    let pid = crate::process::current_pid();
    let path = if rel.starts_with('/') { rel } else { &alloc::format!("/{rel}") };
    // 已存在 → 直接打开；带 O_CREAT 且不存在 → 建可变记录。
    let exists = matches!(
        with_store(|s| s.resolve_pub(path)),
        Some(Ok(Some(_)))
    );
    if !exists {
        if flags & O_CREAT == 0 {
            return kos_errno(KosError::PathNotFound);
        }
        if let Some(Err(e)) = with_store(|s| s.write_mutable(path, &[], uid, kos::store::perm::DEFAULT)) {
            return kos_errno(e);
        }
    } else if flags & O_TRUNC != 0 {
        if let Some(Err(e)) = with_store(|s| s.truncate(path, 0, uid)) {
            return kos_errno(e);
        }
    }
    let key = match with_store(|s| s.resolve_pub(path)) {
        Some(Ok(Some(e))) => e.content_hash,
        _ => return kos_errno(KosError::PathNotFound),
    };
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let r = unsafe {
        let table = posix_table_locked();
        let used = table.values().filter(|e| e.pid == pid).count();
        if used >= POSIX_FD_MAX {
            crate::fs_simple::fs_unlock();
            return kos_errno(KosError::PermissionDenied); // fd 耗尽：EACCES 语义（无 EMFILE 映射）
        }
        // 环绕安全：4G 次 open 后回卷到基点，跳过仍存活的号。
        let mut fd = POSIX_FD_NEXT;
        loop {
            if !table.contains_key(&fd) {
                break;
            }
            fd = fd.wrapping_add(1);
            if fd < KOS_FD_BASE {
                fd = KOS_FD_BASE;
            }
        }
        POSIX_FD_NEXT = fd.wrapping_add(1).max(KOS_FD_BASE);
        table.insert(
            fd,
            PosixFd {
                pid,
                path: path.into(),
                key,
                pos: 0,
                append_mode: flags & O_APPEND != 0,
            },
        );
        fd
    };
    crate::fs_simple::fs_unlock();
    r as i64
}

/// POSIX read（fd >= 1000）。
pub fn posix_read(fd: u32, buf: &mut [u8]) -> i64 {
    let pid = crate::process::current_pid();
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let entry = unsafe { posix_table_locked().get(&fd).filter(|e| e.pid == pid).map(|e| (e.key, e.pos)) };
    crate::fs_simple::fs_unlock();
    let Some((key, pos)) = entry else { return kos_errno(KosError::ObjectNotFound) };
    let n = match with_store(|s| s.read_range_hash(&key, pos, buf)) {
        Some(Ok(n)) => n,
        Some(Err(e)) => return kos_errno(e),
        None => return -1,
    };
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    unsafe {
        if let Some(e) = posix_table_locked().get_mut(&fd) {
            if e.pid == pid {
                e.pos += n as u64;
            }
        }
    }
    crate::fs_simple::fs_unlock();
    n as i64
}

/// POSIX write：追加模式走 append（顺序高效），否则 write_range @ pos。
pub fn posix_write(fd: u32, data: &[u8], uid: u32) -> i64 {
    let pid = crate::process::current_pid();
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let owned = unsafe {
        posix_table_locked().get(&fd).filter(|e| e.pid == pid).map(|e| {
            (e.path.clone(), e.key, e.pos, e.append_mode)
        })
    };
    crate::fs_simple::fs_unlock();
    let Some((path, key, pos, append)) = owned else { return kos_errno(KosError::ObjectNotFound) };
    let r = if append {
        with_store(|s| s.append(&path, data, uid)).map(|v| v.map(|len| len as usize))
    } else {
        with_store(|s| s.write_range(&path, pos, data, uid))
            .map(|v| v.map(|len| len as usize))
    };
    match r {
        Some(Ok(new_len)) => {
            // 新键（写入换代）：刷新 fd 引用并推进位置；POSIX 语义
            // 返回**本次写入字节数**（而非文件新长度）。
            let new_key = match with_store(|s| s.resolve_pub(&path)) {
                Some(Ok(Some(e))) => e.content_hash,
                _ => key,
            };
            crate::fs_simple::fs_lock();
            // SAFETY：fs_lock 临界区内。
            unsafe {
                if let Some(e) = posix_table_locked().get_mut(&fd) {
                    if e.pid == pid {
                        e.key = new_key;
                        e.pos = new_len as u64;
                    }
                }
            }
            crate::fs_simple::fs_unlock();
            data.len() as i64
        }
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// POSIX lseek。
pub fn posix_lseek(fd: u32, off: i64, whence: u32) -> i64 {
    let pid = crate::process::current_pid();
    // whence: 0=SEEK_SET 1=SEEK_CUR 2=SEEK_END
    let (key, pos) = {
        crate::fs_simple::fs_lock();
        // SAFETY：fs_lock 临界区内。
        let e = unsafe { posix_table_locked().get(&fd).filter(|e| e.pid == pid) };
        let r = match e {
            Some(e) => Some((e.key, e.pos)),
            None => None,
        };
        crate::fs_simple::fs_unlock();
        match r {
            Some(v) => v,
            None => return kos_errno(KosError::ObjectNotFound),
        }
    };
    let end = match with_store(|s| s.stat(&key)) {
        Some(Ok(h)) => h.content_len as i64,
        _ => -1,
    };
    let base = match whence {
        0 => 0,
        1 => pos as i64,
        2 => end,
        _ => return -22, // EINVAL
    };
    let np = base + off;
    if np < 0 {
        return -22;
    }
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    unsafe {
        if let Some(e) = posix_table_locked().get_mut(&fd) {
            if e.pid == pid {
                e.pos = np as u64;
            }
        }
    }
    crate::fs_simple::fs_unlock();
    np
}

/// POSIX close（仅本进程的 fd 可关）。
pub fn posix_close(fd: u32) -> i64 {
    let pid = crate::process::current_pid();
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let r = unsafe {
        let table = posix_table_locked();
        match table.get(&fd) {
            Some(e) if e.pid == pid => {
                table.remove(&fd);
                0
            }
            _ => kos_errno(KosError::ObjectNotFound),
        }
    };
    crate::fs_simple::fs_unlock();
    r
}

/// fd 是否属于 POSIX 桥接（供 read/write/close/lseek 分发）。
pub fn is_posix_fd(fd: i32) -> bool {
    fd >= KOS_FD_BASE as i32
}

// ==================== KOS fd 层（470..=474 的 472..474） ====================

/// 每进程 KOS fd 表：fd → (对象键, 读位置)。上限 64/进程。
const KOS_FD_MAX: usize = 64;

struct KosFd {
    key: [u8; 32],
    pos: u64,
}

static mut KOS_FDS: Option<alloc::collections::BTreeMap<(u32, u32), KosFd>> = None;
static mut KOS_FD_NEXT: u32 = 3;

/// kos_open(path) → fd（权限检查同 read；pid 绑定）。
pub fn sys_kos_open(path: &str) -> i64 {
    let uid = crate::process::current_uid();
    let pid = crate::process::current_pid();
    let key = match with_store(|s| {
        s.resolve_pub(path).map(|e| {
            e.filter(|e| {
                kos::store::check_access(
                    uid,
                    e.owner_uid,
                    e.capability,
                    kos::store::perm::OWNER_R | kos::store::perm::OTHER_R,
                )
            })
            .map(|e| e.content_hash)
        })
    }) {
        Some(Ok(Some(k))) => k,
        _ => return -1,
    };
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内（与 with_store 同一锁序）。
    unsafe {
        let table = KOS_FDS.get_or_insert_with(alloc::collections::BTreeMap::new);
        let used = table.keys().filter(|(p, _)| *p == pid).count();
        if used >= KOS_FD_MAX {
            crate::fs_simple::fs_unlock();
            return -1;
        }
        let fd = KOS_FD_NEXT;
        KOS_FD_NEXT += 1;
        table.insert((pid, fd), KosFd { key, pos: 0 });
        crate::fs_simple::fs_unlock();
        fd as i64
    }
}

/// kos_read(fd, buf)：顺序读（位置记忆），返回本读取到的字节数，EOF=0。
pub fn sys_kos_read_fd(fd: u32, buf: &mut [u8]) -> i64 {
    let pid = crate::process::current_pid();
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let entry = unsafe {
        KOS_FDS.as_ref().and_then(|t| t.get(&(pid, fd)).map(|f| (f.key, f.pos)))
    };
    crate::fs_simple::fs_unlock();
    let Some((key, pos)) = entry else { return -1 };
    let n = match with_store(|s| s.read_range_hash(&key, pos, buf)) {
        Some(Ok(n)) => n,
        _ => return -1,
    };
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    unsafe {
        if let Some(f) = KOS_FDS.as_mut().and_then(|t| t.get_mut(&(pid, fd))) {
            f.pos += n as u64;
        }
    }
    crate::fs_simple::fs_unlock();
    n as i64
}

/// kos_close(fd)。
pub fn sys_kos_close_fd(fd: u32) -> i64 {
    let pid = crate::process::current_pid();
    crate::fs_simple::fs_lock();
    // SAFETY：fs_lock 临界区内。
    let r = unsafe {
        KOS_FDS.as_mut().map(|t| t.remove(&(pid, fd)).is_some()).unwrap_or(false) as i64
    };
    crate::fs_simple::fs_unlock();
    r
}

pub fn sys_write_range(path: &str, offset: u64, data: &[u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.write_range(path, offset, data, uid)) {
        Some(Ok(len)) => len as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

pub fn sys_upload_begin(path: &str, total: u64) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.upload_begin(path, total, uid, kos::store::perm::DEFAULT)) {
        Some(Ok(())) => 0,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

pub fn sys_upload_push(path: &str, offset: u64, data: &[u8]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.upload_push(path, offset, data, uid)) {
        Some(Ok((recv, total))) => ((recv as u64) << 32 | total as u64) as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 状态 → 24 字节小端 [total_len, received, contiguous]。
pub fn sys_upload_status(path: &str, out: &mut [u8; 24]) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.upload_status(path, uid)) {
        Some(Ok((total, recv, cont))) => {
            out[..8].copy_from_slice(&total.to_le_bytes());
            out[8..16].copy_from_slice(&(recv as u64).to_le_bytes());
            out[16..24].copy_from_slice(&(cont as u64).to_le_bytes());
            0
        }
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

pub fn sys_upload_finish(path: &str) -> i64 {
    let uid = crate::process::current_uid();
    match with_store(|s| s.upload_finish(path, uid)) {
        Some(Ok(())) => 0,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// 卷状态：返回 (generation, objects, free_blocks, pool_blocks)。
pub fn sys_stat() -> (u64, usize, u64, u64) {
    with_store(|s| (s.generation(), s.object_count(), s.free_blocks(), s.pool_blocks()))
        .unwrap_or((0, 0, 0, 0))
}

/// 引擎级复制（C2 无缝复制系列）：单次 fs_lock 内完成「读源 → 写目标」，
/// 不经用户态两跳。类型保留：
/// - 可变记录 → 分块流式（write_mutable 建槽/截断 + append 链，内存
///   峰值 16KB 缓冲，任意大小可复制）；
/// - 不可变对象 → 整读整写（put 按内容哈希寻址：**同内容天然去重**，
///   复制系统镜像/包内共享块只存一份）。
/// 返回复制字节数；失败为 errno（源读权限 + 目标写权限都在引擎侧检查）。
pub fn sys_copy(src: &str, dst: &str) -> i64 {
    let uid = crate::process::current_uid();
    let r = with_store(|s| {
        let Some(entry) = s.resolve_pub(src)? else {
            return Err(KosError::PathNotFound);
        };
        if !kos::store::check_access(
            uid,
            entry.owner_uid,
            entry.capability,
            kos::store::perm::OWNER_R | kos::store::perm::OTHER_R,
        ) {
            return Err(KosError::PermissionDenied);
        }
        let hdr = s.stat(&entry.content_hash)?;
        if s.is_mutable(&entry.content_hash) {
            // 记录 → 流式：建空记录（已有则截断），append 链逐块搬运。
            s.write_mutable(dst, &[], uid, kos::store::perm::DEFAULT)?;
            let len = hdr.content_len;
            let mut off = 0u64;
            let mut buf = [0u8; 16384];
            while off < len {
                let n = s.read_range(src, off, &mut buf, uid)?;
                if n == 0 {
                    break;
                }
                s.append(dst, &buf[..n], uid)?;
                off += n as u64;
            }
            Ok(off)
        } else {
            // 不可变 → 整读整写（put 内容寻址去重）。
            let data = s.open_path(src, uid)?;
            let n = data.len() as u64;
            s.write(dst, &data, uid, entry.capability)?;
            Ok(n)
        }
    });
    match r {
        Some(Ok(n)) => n as i64,
        Some(Err(e)) => kos_errno(e),
        None => -1,
    }
}

/// `/kos/` 路径的 (类型码, 大小)：映射到 fs_simple 的 T_*，供
/// vfs::stat_path / lstat_path 路由（cp 复制进目录的 basename 拼接等）。
pub fn sys_stat_path(path: &str) -> Option<(u16, u64)> {
    // 尾斜杠归一 + 前导斜杠补齐（调用方可能传剥掉 /kos/ 前缀的 "data"
    // 或带尾斜杠的 "/data/"；resolve 只认 "/data" 形式）。根路径特判
    // 为目录（引擎 resolve("/") 不认零分量路径）。
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return Some((crate::fs_simple::T_DIR, 0));
    }
    let owned;
    let p: &str = if p.starts_with('/') {
        p
    } else {
        owned = alloc::format!("/{p}");
        &owned
    };
    // with_store 返回 Option<Result<...>>（未挂载 / 引擎错），两层都摊平。
    let entry = match with_store(|s| s.resolve_pub(p)) {
        Some(Ok(Some(e))) => e,
        _ => return None,
    };
    let hdr = match with_store(|s| s.stat(&entry.content_hash)) {
        Some(Ok(h)) => h,
        _ => return None,
    };
    let t = match entry.entry_type {
        kos::format::object::ObjectType::Dir => crate::fs_simple::T_DIR,
        kos::format::object::ObjectType::Symlink => crate::fs_simple::T_SYM,
        _ => crate::fs_simple::T_REG,
    };
    Some((t, hdr.content_len))
}
